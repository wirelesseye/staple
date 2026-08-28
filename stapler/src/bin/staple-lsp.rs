mod staple_lsp {
    pub mod completion;
    pub mod definition;
    pub mod hover;
    pub mod semantic;
    pub mod source_projection;
}

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{
    Completion, GotoDefinition, HoverRequest, Request as _, SemanticTokensFullRequest,
};
use lsp_types::*;
use staple_lsp::completion::CompletionIndex;
use staple_lsp::definition::{self, DefinitionEntry};
use staple_lsp::hover::{self, HoverEntry};
use staple_lsp::semantic::{self, SemanticEntry};
use stapler::{Diagnostic as StapleDiagnostic, NameResolver, ProgramLoader, Span, TypeChecker};

#[derive(Default)]
struct Document {
    text: String,
    version: i32,
    semantic_tokens: Vec<SemanticToken>,
    hover_entries: Vec<HoverEntry>,
    definition_entries: Vec<DefinitionEntry>,
    last_successful: Option<SuccessfulAnalysis>,
    last_resolved: Option<ResolvedDefinitions>,
}

#[derive(Clone)]
struct SuccessfulAnalysis {
    source: String,
    completion_index: CompletionIndex,
    semantic_entries: Vec<SemanticEntry>,
    hover_entries: Vec<HoverEntry>,
}

#[derive(Clone)]
struct ResolvedDefinitions {
    source: String,
    path: PathBuf,
    entries: Vec<DefinitionEntry>,
}

struct Server {
    connection: Connection,
    documents: HashMap<Uri, Document>,
    published_by_root: HashMap<Uri, HashSet<Uri>>,
    stdlib: Option<PathBuf>,
}

fn package_config(path: &Path) -> Option<binder::PackageGraph> {
    let mut directory = path.parent()?;
    loop {
        let manifest = directory.join("binder.kdl");
        if manifest.is_file() {
            return binder::load_package_graph(&manifest).ok();
        }
        directory = directory.parent()?;
    }
}

fn normalized_source_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        let Some(parent) = path.parent() else {
            return path.to_owned();
        };
        let Some(file_name) = path.file_name() else {
            return path.to_owned();
        };
        std::fs::canonicalize(parent)
            .map(|parent| parent.join(file_name))
            .unwrap_or_else(|_| path.to_owned())
    })
}

/// Re-parses the editor buffer so its `SyntaxId`s match the ones the loaded
/// program assigned to this file. The program parses its modules from a shared
/// counter, so unless this file happened to be parsed first its ids are offset
/// from a plain `parse` by a constant; recovering that constant from the
/// program's own module for `path` and re-parsing from it realigns the two.
/// Returns `None` when the file isn't in the program or is already aligned, in
/// which case the caller keeps the original `parse` output.
fn rebased_surface(
    resolved: &stapler::ResolvedModule,
    path: &Path,
    editor_module: &stapler::Module,
    text: &str,
) -> Option<stapler::Module> {
    let program = resolved.program();
    let program_last_id = program
        .modules()
        .iter()
        .find(|source| {
            !source.companion
                && source.path == path
                && source
                    .parent
                    .is_none_or(|parent| program.module(parent).path != source.path)
        })?
        .syntax
        .syntax
        .id
        .0;
    let base = program_last_id.checked_sub(editor_module.syntax.id.0)?;
    if base == 0 {
        return None;
    }
    stapler::parse_at(text, base).ok()
}

fn is_standard_library_source(config: &binder::PackageGraph, path: &Path) -> bool {
    let package = config.root_package();
    package.name == "std"
        && normalized_source_path(path).starts_with(normalized_source_path(package.source_root()))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("staple-lsp: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let stdlib = parse_options(std::env::args_os().skip(1))?;
    let (connection, io_threads) = Connection::stdio();
    initialize(&connection)?;

    let mut server = Server {
        connection,
        documents: HashMap::new(),
        published_by_root: HashMap::new(),
        stdlib,
    };
    let result = server.event_loop();
    drop(server);
    result?;
    io_threads.join().map_err(|error| error.to_string())
}

fn initialize(connection: &Connection) -> Result<(), String> {
    let (initialize_id, initialize_params) = connection
        .initialize_start()
        .map_err(|error| error.to_string())?;
    let _: InitializeParams = serde_json::from_value(initialize_params)
        .map_err(|error| format!("invalid initialize parameters: {error}"))?;
    let capabilities = ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic::legend(),
                range: Some(false),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                work_done_progress_options: WorkDoneProgressOptions::default(),
            },
        )),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(false),
            trigger_characters: Some(vec!["^".to_owned(), ".".to_owned()]),
            all_commit_characters: None,
            work_done_progress_options: WorkDoneProgressOptions::default(),
            completion_item: None,
        }),
        ..ServerCapabilities::default()
    };
    let result = InitializeResult {
        capabilities,
        server_info: Some(ServerInfo {
            name: "staple-lsp".to_owned(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    };
    connection
        .initialize_finish(initialize_id, serde_json::to_value(result).unwrap())
        .map_err(|error| error.to_string())
}

fn parse_options(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Option<PathBuf>, String> {
    let mut arguments = arguments.into_iter();
    let mut stdlib = None;
    while let Some(argument) = arguments.next() {
        if argument == "--stdio" {
            // `vscode-languageclient` appends this conventional transport flag.
            // Staple LSP currently supports stdio exclusively, so it is a no-op.
            continue;
        } else if argument == "--stdlib" {
            stdlib = Some(PathBuf::from(
                arguments.next().ok_or("expected a path after `--stdlib`")?,
            ));
        } else if argument == "-h" || argument == "--help" {
            return Err("usage: staple-lsp [--stdlib <path>]".to_owned());
        } else {
            return Err(format!(
                "unknown option `{}`\nusage: staple-lsp [--stdlib <path>]",
                argument.to_string_lossy()
            ));
        }
    }
    Ok(stdlib)
}

impl Server {
    fn event_loop(&mut self) -> Result<(), String> {
        while let Ok(message) = self.connection.receiver.recv() {
            match message {
                Message::Request(request) => {
                    if self
                        .connection
                        .handle_shutdown(&request)
                        .map_err(|error| error.to_string())?
                    {
                        return Ok(());
                    }
                    self.request(request)?;
                }
                Message::Notification(notification) => self.notification(notification)?,
                Message::Response(_) => {}
            }
        }
        Ok(())
    }

    fn request(&mut self, request: Request) -> Result<(), String> {
        if request.method == SemanticTokensFullRequest::METHOD {
            let params: SemanticTokensParams =
                serde_json::from_value(request.params).map_err(|error| error.to_string())?;
            let data = self
                .documents
                .get(&params.text_document.uri)
                .map(|document| document.semantic_tokens.clone())
                .unwrap_or_default();
            let result = SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data,
            });
            self.connection
                .sender
                .send(Message::Response(Response::new_ok(
                    request.id,
                    serde_json::to_value(result).unwrap(),
                )))
                .map_err(|error| error.to_string())?;
        } else if request.method == Completion::METHOD {
            let params: CompletionParams =
                serde_json::from_value(request.params).map_err(|error| error.to_string())?;
            let uri = &params.text_document_position.text_document.uri;
            let requested_position = params.text_document_position.position;
            let items = self
                .documents
                .get(uri)
                .and_then(|document| {
                    let offset = semantic::offset(&document.text, requested_position)?;
                    let successful = document.last_successful.as_ref()?;
                    let previous_offset =
                        TextChange::between(&successful.source, &document.text).old_offset(offset);
                    let completion_receiver = completion_receiver(&document.text, offset).map(
                        |(receiver, qualified, name)| {
                            (
                                TextChange::between(&successful.source, &document.text)
                                    .old_offset(receiver),
                                qualified,
                                name,
                            )
                        },
                    );
                    Some(match completion_receiver {
                        Some((receiver, true, name)) => {
                            let indexed = successful.completion_index.qualifier_items(receiver);
                            if indexed.is_empty() {
                                successful
                                    .completion_index
                                    .named_qualifier_items(name, previous_offset)
                            } else {
                                indexed
                            }
                        }
                        Some((receiver, false, name)) => {
                            let indexed = successful.completion_index.method_items(receiver);
                            if indexed.is_empty() {
                                successful
                                    .completion_index
                                    .named_method_items(name, previous_offset)
                            } else {
                                indexed
                            }
                        }
                        None => successful.completion_index.items(previous_offset),
                    })
                })
                .unwrap_or_else(staple_lsp::completion::keywords);
            let result = CompletionResponse::Array(items);
            self.connection
                .sender
                .send(Message::Response(Response::new_ok(
                    request.id,
                    serde_json::to_value(result).unwrap(),
                )))
                .map_err(|error| error.to_string())?;
        } else if request.method == GotoDefinition::METHOD {
            let params: GotoDefinitionParams =
                serde_json::from_value(request.params).map_err(|error| error.to_string())?;
            let uri = &params.text_document_position_params.text_document.uri;
            let requested_position = params.text_document_position_params.position;
            let result = self.documents.get(uri).and_then(|document| {
                let offset = semantic::offset(&document.text, requested_position)?;
                let entry = document
                    .definition_entries
                    .iter()
                    .filter(|entry| entry.range.start <= offset && offset < entry.range.end)
                    .min_by_key(|entry| entry.range.end - entry.range.start)?;
                let origin_selection_range = lsp_range(&document.text, entry.range.clone());
                let links = entry
                    .targets
                    .iter()
                    .filter_map(|target| {
                        let is_entry = document
                            .last_resolved
                            .as_ref()
                            .is_some_and(|resolved| resolved.path == target.path);
                        let target_uri = if is_entry {
                            uri.clone()
                        } else {
                            path_to_uri(&target.path)?
                        };
                        let target_text = if is_entry || &target_uri == uri {
                            document.text.clone()
                        } else {
                            std::fs::read_to_string(&target.path).ok()?
                        };
                        Some(LocationLink {
                            origin_selection_range: Some(origin_selection_range),
                            target_uri,
                            target_range: lsp_range(&target_text, target.range.clone()),
                            target_selection_range: lsp_range(
                                &target_text,
                                target.selection_range.clone(),
                            ),
                        })
                    })
                    .collect::<Vec<_>>();
                (!links.is_empty()).then_some(GotoDefinitionResponse::Link(links))
            });
            self.connection
                .sender
                .send(Message::Response(Response::new_ok(
                    request.id,
                    serde_json::to_value(result).unwrap(),
                )))
                .map_err(|error| error.to_string())?;
        } else if request.method == HoverRequest::METHOD {
            let params: HoverParams =
                serde_json::from_value(request.params).map_err(|error| error.to_string())?;
            let uri = &params.text_document_position_params.text_document.uri;
            let requested_position = params.text_document_position_params.position;
            let result = self.documents.get(uri).and_then(|document| {
                let offset = semantic::offset(&document.text, requested_position)?;
                let width = document
                    .hover_entries
                    .iter()
                    .filter(|entry| entry.range.start <= offset && offset < entry.range.end)
                    .map(|entry| entry.range.end - entry.range.start)
                    .min()?;
                let mut entries = document
                    .hover_entries
                    .iter()
                    .filter(|entry| {
                        entry.range.start <= offset
                            && offset < entry.range.end
                            && entry.range.end - entry.range.start == width
                    })
                    .collect::<Vec<_>>();
                entries.sort_by(|left, right| {
                    left.signature
                        .cmp(&right.signature)
                        .then(left.documentation.cmp(&right.documentation))
                });
                entries.dedup();
                let entry = entries.first()?;
                let (start_line, start_character) =
                    semantic::position(&document.text, entry.range.start);
                let (end_line, end_character) = semantic::position(&document.text, entry.range.end);
                let value = entries
                    .into_iter()
                    .map(|entry| {
                        if entry.documentation.is_empty() {
                            format!("```staple\n{}\n```", entry.signature)
                        } else {
                            format!(
                                "```staple\n{}\n```\n\n{}",
                                entry.signature,
                                entry.documentation.join("\n")
                            )
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");
                Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    }),
                    range: Some(Range::new(
                        Position::new(start_line, start_character),
                        Position::new(end_line, end_character),
                    )),
                })
            });
            self.connection
                .sender
                .send(Message::Response(Response::new_ok(
                    request.id,
                    serde_json::to_value(result).unwrap(),
                )))
                .map_err(|error| error.to_string())?;
        } else {
            self.connection
                .sender
                .send(Message::Response(Response::new_err(
                    request.id,
                    lsp_server::ErrorCode::MethodNotFound as i32,
                    format!("unsupported request `{}`", request.method),
                )))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn notification(&mut self, notification: Notification) -> Result<(), String> {
        match notification.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let params: DidOpenTextDocumentParams = serde_json::from_value(notification.params)
                    .map_err(|error| error.to_string())?;
                let uri = params.text_document.uri;
                self.documents.insert(
                    uri.clone(),
                    Document {
                        text: params.text_document.text,
                        version: params.text_document.version,
                        semantic_tokens: Vec::new(),
                        hover_entries: Vec::new(),
                        definition_entries: Vec::new(),
                        last_successful: None,
                        last_resolved: None,
                    },
                );
                self.analyze(&uri)?;
            }
            DidChangeTextDocument::METHOD => {
                let params: DidChangeTextDocumentParams =
                    serde_json::from_value(notification.params)
                        .map_err(|error| error.to_string())?;
                if let (Some(document), Some(change)) = (
                    self.documents.get_mut(&params.text_document.uri),
                    params.content_changes.into_iter().last(),
                ) {
                    document.text = change.text;
                    document.version = params.text_document.version;
                    self.analyze(&params.text_document.uri)?;
                }
            }
            DidCloseTextDocument::METHOD => {
                let params: DidCloseTextDocumentParams =
                    serde_json::from_value(notification.params)
                        .map_err(|error| error.to_string())?;
                self.documents.remove(&params.text_document.uri);
                self.clear_diagnostics(&params.text_document.uri)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn analyze(&mut self, uri: &Uri) -> Result<(), String> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(());
        };
        let text = document.text.clone();
        let version = document.version;
        let Some(path) = uri_to_path(uri) else {
            let tokens = semantic::tokens(&text, stapler::parse(&text).ok().as_ref(), None, None);
            self.documents.get_mut(uri).unwrap().semantic_tokens = tokens;
            self.documents.get_mut(uri).unwrap().hover_entries.clear();
            self.documents
                .get_mut(uri)
                .unwrap()
                .definition_entries
                .clear();
            return self.publish(
                uri.clone(),
                vec![lsp_diagnostic(
                    &text,
                    0..0,
                    "Staple language analysis requires a file URI".to_owned(),
                )],
                Some(version),
            );
        };

        let parsed = stapler::parse(&text).ok();
        let mut grouped: HashMap<Uri, Vec<lsp_types::Diagnostic>> = HashMap::new();
        let mut resolved_for_tokens = None;
        let mut typed_for_tokens = None;
        let mut hover_entries = Vec::new();
        let mut document_completion = None;
        let mut definition_entries = None;
        let mut definition_path = path.clone();
        // The editor's own `parse` numbers `SyntaxId`s from zero, which only
        // lines up with the loaded program when this file was parsed first.
        // Once the program is loaded we re-parse the buffer starting from the
        // id the program assigned this module, so hover/definition/semantic
        // walks over the surface tree query the resolver with matching ids.
        let mut rebased_module: Option<stapler::Module> = None;
        let mut analysis_succeeded = false;
        if let Some(module) = &parsed {
            let mut loader = ProgramLoader::new();
            if let Some(stdlib) = &self.stdlib {
                loader = loader.with_standard_library_root(stdlib);
            }
            let config = package_config(&path);
            let use_package_graph = config
                .as_ref()
                .is_some_and(|config| !is_standard_library_source(config, &path));
            if use_package_graph && let Some(config) = &config {
                loader = loader.with_package_graph(config.clone());
            }
            let loaded = match use_package_graph {
                true => loader.load_package_graph_source_at(&path, &text),
                false => loader.load_source_at(&path, &text),
            };
            match loaded {
                Err(error) => {
                    let diagnostic_uri = error
                        .source
                        .as_deref()
                        .and_then(path_to_uri)
                        .unwrap_or_else(|| uri.clone());
                    let diagnostic_text = source_for(&diagnostic_uri, uri, &text);
                    grouped
                        .entry(diagnostic_uri)
                        .or_default()
                        .push(lsp_diagnostic(
                            &diagnostic_text,
                            error.range.unwrap_or(0..0),
                            error.message,
                        ));
                }
                Ok(program) => match NameResolver::new().resolve_program(program) {
                    Err(diagnostics) => {
                        add_compiler_diagnostics(&mut grouped, diagnostics, uri, &text)
                    }
                    Ok(resolved) => {
                        definition_path = normalized_source_path(&path);
                        rebased_module =
                            rebased_surface(&resolved, &definition_path, module, &text);
                        let surface = rebased_module.as_ref().unwrap_or(module);
                        definition_entries = Some(definition::entries_at_path(
                            &definition_path,
                            surface,
                            &resolved,
                            None,
                        ));
                        resolved_for_tokens = Some(resolved.clone());
                        match TypeChecker::new().check(resolved) {
                            Ok(typed) => {
                                let surface = rebased_module.as_ref().unwrap_or(module);
                                definition_entries = Some(definition::entries_at_path(
                                    &definition_path,
                                    surface,
                                    typed.resolved(),
                                    Some(&typed),
                                ));
                                hover_entries =
                                    hover::entries_at_path(&definition_path, surface, &typed);
                                let completion_index =
                                    staple_lsp::completion::index(surface, &typed);
                                typed_for_tokens = Some(typed);
                                document_completion = Some(completion_index);
                                analysis_succeeded = true;
                            }
                            Err(diagnostics) => {
                                add_compiler_diagnostics(&mut grouped, diagnostics, uri, &text)
                            }
                        }
                    }
                },
            }
        } else if let Err(error) = stapler::parse(&text) {
            grouped.entry(uri.clone()).or_default().push(lsp_diagnostic(
                &text,
                error.offset..error.offset,
                error.message,
            ));
        }

        let current_semantic = semantic::entries_at_path(
            &text,
            &definition_path,
            rebased_module.as_ref().or(parsed.as_ref()),
            resolved_for_tokens.as_ref(),
            typed_for_tokens.as_ref(),
        );
        let document = self.documents.get_mut(uri).unwrap();
        if let Some(entries) = definition_entries {
            document.definition_entries = entries.clone();
            document.last_resolved = Some(ResolvedDefinitions {
                source: text.clone(),
                path: definition_path,
                entries,
            });
        } else if let Some(resolved) = &document.last_resolved {
            document.definition_entries = remap_definition_entries(&text, resolved);
        } else {
            document.definition_entries.clear();
        }
        if analysis_succeeded {
            document.semantic_tokens = semantic::encode(&text, &current_semantic);
            document.hover_entries = hover_entries.clone();
            document.last_successful = Some(SuccessfulAnalysis {
                source: text.clone(),
                completion_index: document_completion
                    .expect("successful analysis builds completion data"),
                semantic_entries: current_semantic,
                hover_entries,
            });
        } else if let Some(successful) = &document.last_successful {
            let merged_semantic = merge_semantic_entries(&text, current_semantic, successful);
            document.semantic_tokens = semantic::encode(&text, &merged_semantic);
            document.hover_entries = remap_hover_entries(&text, successful);
        } else {
            document.semantic_tokens = semantic::encode(&text, &current_semantic);
            document.hover_entries.clear();
        }

        let old = self.published_by_root.remove(uri).unwrap_or_default();
        let new = grouped.keys().cloned().collect::<HashSet<_>>();
        for stale in old.difference(&new) {
            self.publish(stale.clone(), Vec::new(), None)?;
        }
        for (diagnostic_uri, diagnostics) in grouped {
            let diagnostic_version = (diagnostic_uri == *uri).then_some(version);
            self.publish(diagnostic_uri, diagnostics, diagnostic_version)?;
        }
        self.published_by_root.insert(uri.clone(), new);
        Ok(())
    }

    fn clear_diagnostics(&mut self, root: &Uri) -> Result<(), String> {
        if let Some(uris) = self.published_by_root.remove(root) {
            for uri in uris {
                self.publish(uri, Vec::new(), None)?;
            }
        }
        Ok(())
    }

    fn publish(
        &self,
        uri: Uri,
        diagnostics: Vec<lsp_types::Diagnostic>,
        version: Option<i32>,
    ) -> Result<(), String> {
        let params = PublishDiagnosticsParams::new(uri, diagnostics, version);
        self.connection
            .sender
            .send(Message::Notification(Notification::new(
                PublishDiagnostics::METHOD.to_owned(),
                params,
            )))
            .map_err(|error| error.to_string())
    }
}

fn merge_semantic_entries(
    source: &str,
    current: Vec<SemanticEntry>,
    successful: &SuccessfulAnalysis,
) -> Vec<SemanticEntry> {
    let change = TextChange::between(&successful.source, source);
    let mut merged = current
        .into_iter()
        .map(|entry| ((entry.start, entry.end), entry))
        .collect::<HashMap<_, _>>();
    for entry in &successful.semantic_entries {
        if entry.token_type > semantic::PROPERTY {
            continue;
        }
        let Some((start, end)) = change.remap(entry.start, entry.end) else {
            continue;
        };
        if successful.source.get(entry.start..entry.end) != source.get(start..end) {
            continue;
        }
        merged.insert(
            (start, end),
            SemanticEntry {
                start,
                end,
                token_type: entry.token_type,
                modifiers: entry.modifiers,
            },
        );
    }
    let mut entries = merged.into_values().collect::<Vec<_>>();
    entries.sort_by_key(|entry| (entry.start, entry.end));
    entries
}

fn remap_hover_entries(source: &str, successful: &SuccessfulAnalysis) -> Vec<HoverEntry> {
    let change = TextChange::between(&successful.source, source);
    successful
        .hover_entries
        .iter()
        .filter_map(|entry| {
            let (start, end) = change.remap(entry.range.start, entry.range.end)?;
            let range = start..end;
            (successful.source.get(entry.range.clone())? == source.get(range.clone())?).then(|| {
                HoverEntry {
                    range,
                    signature: entry.signature.clone(),
                    documentation: entry.documentation.clone(),
                }
            })
        })
        .collect()
}

fn remap_definition_entries(source: &str, resolved: &ResolvedDefinitions) -> Vec<DefinitionEntry> {
    let change = TextChange::between(&resolved.source, source);
    resolved
        .entries
        .iter()
        .filter_map(|entry| {
            let (start, end) = change.remap(entry.range.start, entry.range.end)?;
            let range = start..end;
            if resolved.source.get(entry.range.clone())? != source.get(range.clone())? {
                return None;
            }
            let targets = entry
                .targets
                .iter()
                .filter_map(|target| {
                    if target.path != resolved.path {
                        return Some(target.clone());
                    }
                    let (range_start, range_end) =
                        change.remap(target.range.start, target.range.end)?;
                    let (selection_start, selection_end) =
                        change.remap(target.selection_range.start, target.selection_range.end)?;
                    Some(definition::DefinitionTarget {
                        path: target.path.clone(),
                        range: range_start..range_end,
                        selection_range: selection_start..selection_end,
                    })
                })
                .collect::<Vec<_>>();
            (!targets.is_empty()).then_some(DefinitionEntry { range, targets })
        })
        .collect()
}

fn lsp_range(source: &str, range: std::ops::Range<usize>) -> Range {
    let (start_line, start_character) = semantic::position(source, range.start.min(source.len()));
    let (end_line, end_character) = semantic::position(source, range.end.min(source.len()));
    Range::new(
        Position::new(start_line, start_character),
        Position::new(end_line, end_character),
    )
}

#[derive(Debug, Clone, Copy)]
struct TextChange {
    unchanged_prefix: usize,
    old_suffix_start: usize,
    new_suffix_start: usize,
}

fn completion_receiver(text: &str, offset: usize) -> Option<(usize, bool, &str)> {
    let before_cursor = text.get(..offset)?;
    let line_start = before_cursor.rfind('\n').map_or(0, |index| index + 1);
    let line = &before_cursor[line_start..];
    let separator = line.rfind(['^', '.'])?;
    let prefix = &line[separator + 1..];
    if prefix
        .chars()
        .all(|character| character == '_' || character.is_alphanumeric())
        && separator > 0
    {
        let receiver = line[..separator]
            .trim_end()
            .rsplit_once(|character: char| {
                !(character == '_' || character == '.' || character.is_alphanumeric())
            })
            .map_or(line[..separator].trim_end(), |(_, receiver)| receiver);
        Some((
            line_start + separator,
            line.as_bytes()[separator] == b'.',
            receiver,
        ))
    } else {
        None
    }
}

impl TextChange {
    fn between(old: &str, new: &str) -> Self {
        let mut prefix = old
            .as_bytes()
            .iter()
            .zip(new.as_bytes())
            .take_while(|(left, right)| left == right)
            .count();
        while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
            prefix -= 1;
        }

        let maximum_suffix = old.len().saturating_sub(prefix).min(new.len() - prefix);
        let mut suffix = old
            .as_bytes()
            .iter()
            .rev()
            .zip(new.as_bytes().iter().rev())
            .take(maximum_suffix)
            .take_while(|(left, right)| left == right)
            .count();
        while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix)
        {
            suffix -= 1;
        }

        Self {
            unchanged_prefix: prefix,
            old_suffix_start: old.len() - suffix,
            new_suffix_start: new.len() - suffix,
        }
    }

    fn old_offset(self, new_offset: usize) -> usize {
        if new_offset <= self.unchanged_prefix {
            new_offset
        } else if new_offset >= self.new_suffix_start {
            self.old_suffix_start + (new_offset - self.new_suffix_start)
        } else {
            self.unchanged_prefix
        }
    }

    fn remap(self, start: usize, end: usize) -> Option<(usize, usize)> {
        if end <= self.unchanged_prefix {
            Some((start, end))
        } else if start >= self.old_suffix_start {
            Some((
                self.new_suffix_start + start - self.old_suffix_start,
                self.new_suffix_start + end - self.old_suffix_start,
            ))
        } else {
            None
        }
    }
}

fn add_compiler_diagnostics(
    grouped: &mut HashMap<Uri, Vec<lsp_types::Diagnostic>>,
    diagnostics: Vec<StapleDiagnostic>,
    entry_uri: &Uri,
    entry_text: &str,
) {
    for diagnostic in diagnostics {
        let (uri, range) = match diagnostic.span {
            Span::User { source, range, .. } => (
                source
                    .as_deref()
                    .and_then(|source| path_to_uri(Path::new(source)))
                    .unwrap_or_else(|| entry_uri.clone()),
                range,
            ),
            Span::Compiler => (entry_uri.clone(), 0..0),
        };
        let source = source_for(&uri, entry_uri, entry_text);
        grouped
            .entry(uri)
            .or_default()
            .push(lsp_diagnostic(&source, range, diagnostic.message));
    }
}

fn lsp_diagnostic(
    source: &str,
    range: std::ops::Range<usize>,
    message: String,
) -> lsp_types::Diagnostic {
    let start = range.start.min(source.len());
    let end = range.end.min(source.len()).max(start);
    let range = trim_diagnostic_range(source, start..end);
    let start = range.start;
    let end = range.end;
    let (start_line, start_character) = semantic::position(source, start);
    let (end_line, end_character) = semantic::position(source, end);
    lsp_types::Diagnostic {
        range: Range::new(
            Position::new(start_line, start_character),
            Position::new(end_line, end_character),
        ),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("staple".to_owned()),
        message,
        ..lsp_types::Diagnostic::default()
    }
}

fn trim_diagnostic_range(source: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let Some(text) = source.get(range.clone()) else {
        return range;
    };
    let leading = text.len() - text.trim_start_matches(char::is_whitespace).len();
    let trailing = text.len() - text.trim_end_matches(char::is_whitespace).len();
    if leading + trailing >= text.len() {
        range
    } else {
        range.start + leading..range.end - trailing
    }
}

fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    if uri.scheme().map(|scheme| scheme.as_str()) != Some("file") {
        return None;
    }
    let bytes = percent_decode(uri.path().as_str())?;
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

fn path_to_uri(path: &Path) -> Option<Uri> {
    if !path.is_absolute() {
        return None;
    }
    format!("file://{}", percent_encode(&path.to_string_lossy()))
        .parse()
        .ok()
}

fn percent_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(decoded)
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn source_for(uri: &Uri, entry_uri: &Uri, entry_text: &str) -> String {
    if uri == entry_uri {
        return entry_text.to_owned();
    }
    uri_to_path(uri)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::notification::{Exit, Initialized};
    use lsp_types::request::{Initialize, Shutdown};
    use std::time::Duration;

    #[test]
    fn projects_expanded_macro_argument_features_to_the_call_site() {
        let source = concat!(
            "use std.syntax.(parse_quote, Expr, Ident)\n",
            "def target = () => 1\n",
            "macro invoke: Ident -> Expr = name: Ident => parse_quote { $name () }\n",
            "macro discard: Expr -> Expr = _: Expr => parse_quote { 1 }\n",
            "let result = invoke target\n",
            "let ignored = discard target\n",
        );
        let path = std::env::temp_dir().join("staple-lsp-macro-argument-features.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let surface = stapler::parse(source).unwrap();
        let analysis_path = typed
            .resolved()
            .program()
            .module(typed.resolved().program().entry())
            .path
            .clone();
        let argument_start = source.find("invoke target").unwrap() + "invoke ".len();
        let argument_range = argument_start..argument_start + "target".len();
        let discarded_start = source.rfind("target").unwrap();
        let discarded_range = discarded_start..discarded_start + "target".len();
        let declaration_start = source.find("target").unwrap();

        let semantics = semantic::entries_at_path(
            source,
            &analysis_path,
            Some(&surface),
            Some(typed.resolved()),
            Some(&typed),
        );
        assert!(
            semantics.iter().any(|entry| {
                entry.start == argument_range.start
                    && entry.end == argument_range.end
                    && entry.token_type == semantic::FUNCTION
            }),
            "semantic entries: {semantics:?}"
        );

        let hovers = hover::entries_at_path(&analysis_path, &surface, &typed);
        assert!(
            hovers.iter().any(|entry| {
                entry.range == argument_range && entry.signature.contains("target")
            }),
            "hover entries: {hovers:?}"
        );

        let definitions =
            definition::entries_at_path(&analysis_path, &surface, typed.resolved(), Some(&typed));
        assert!(
            definitions.iter().any(|entry| {
                entry.range == argument_range
                    && entry.targets.iter().any(|target| {
                        target.selection_range.start == declaration_start
                            && &source[target.selection_range.clone()] == "target"
                    })
            }),
            "definition entries: {definitions:?}"
        );
        assert!(!semantics.iter().any(|entry| {
            entry.start == discarded_range.start
                && entry.end == discarded_range.end
                && entry.token_type == semantic::FUNCTION
        }));
        assert!(
            !hovers.iter().any(|entry| {
                entry.range == discarded_range && entry.signature.contains("target")
            })
        );
        assert!(
            !definitions
                .iter()
                .any(|entry| entry.range == discarded_range)
        );
    }

    #[test]
    fn projects_type_and_pattern_arguments_through_raw_quotes() {
        let source = concat!(
            "use std.syntax.(quote, Expr, Type, Pattern)\n",
            "macro bind = pattern: Pattern => ty: Type => value: Expr => quote { let $pattern: $ty = $value }\n",
            "macro destructure = pattern: Pattern => value: Expr => quote { let $pattern = $value }\n",
            "bind pair ((I32, I32)) (20, 22)\n",
            "destructure ((left, right)) pair\n",
        );
        let path = std::env::temp_dir().join("staple-lsp-raw-macro-argument-features.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let typed = TypeChecker::new().check(resolved).unwrap();
        let surface = stapler::parse(source).unwrap();
        let analysis_path = typed
            .resolved()
            .program()
            .module(typed.resolved().program().entry())
            .path
            .clone();
        let pair_declaration = source.find("bind pair").unwrap() + "bind ".len();
        let pair_reference = source.rfind("pair").unwrap();
        let left = source.rfind("left").unwrap();
        let i32 = source.find("((I32").unwrap() + 2;

        let semantics = semantic::entries_at_path(
            source,
            &analysis_path,
            Some(&surface),
            Some(typed.resolved()),
            Some(&typed),
        );
        for (start, kind) in [
            (pair_declaration, semantic::VARIABLE),
            (pair_reference, semantic::VARIABLE),
            (left, semantic::VARIABLE),
            (i32, semantic::TYPE),
        ] {
            assert!(
                semantics
                    .iter()
                    .any(|entry| entry.start == start && entry.token_type == kind),
                "missing semantic entry at {start}: {semantics:?}"
            );
        }

        let hovers = hover::entries_at_path(&analysis_path, &surface, &typed);
        for start in [pair_declaration, pair_reference, left, i32] {
            assert!(
                hovers.iter().any(|entry| entry.range.start == start),
                "missing hover at {start}: {hovers:?}"
            );
        }

        let definitions =
            definition::entries_at_path(&analysis_path, &surface, typed.resolved(), Some(&typed));
        assert!(
            definitions.iter().any(|entry| {
                entry.range.start == pair_reference
                    && entry
                        .targets
                        .iter()
                        .any(|target| target.selection_range.start == pair_declaration)
            }),
            "definition entries: {definitions:?}"
        );
        for start in [pair_declaration, left, i32] {
            assert!(
                definitions.iter().any(|entry| entry.range.start == start),
                "missing definition at {start}: {definitions:?}"
            );
        }
    }

    #[test]
    fn parses_server_options() {
        assert_eq!(
            parse_options(Vec::<std::ffi::OsString>::new()).unwrap(),
            None
        );
        assert_eq!(
            parse_options(["--stdlib".into(), "/tmp/stdlib".into()]).unwrap(),
            Some(PathBuf::from("/tmp/stdlib")),
        );
        assert_eq!(parse_options(["--stdio".into()]).unwrap(), None);
        assert_eq!(
            parse_options(["--stdio".into(), "--stdlib".into(), "/tmp/stdlib".into(),]).unwrap(),
            Some(PathBuf::from("/tmp/stdlib")),
        );
    }

    #[test]
    fn preserves_unchanged_successful_semantics_and_hover_after_an_edit() {
        let old = "def good = () => 1\ngood\n";
        let new = "def good = () => 1\nbroken ???\ngood\n";
        let declaration = old.find("good").unwrap();
        let reference = old.rfind("good").unwrap();
        let successful = SuccessfulAnalysis {
            source: old.to_owned(),
            completion_index: CompletionIndex::default(),
            semantic_entries: vec![
                SemanticEntry {
                    start: declaration,
                    end: declaration + 4,
                    token_type: semantic::FUNCTION,
                    modifiers: 0,
                },
                SemanticEntry {
                    start: reference,
                    end: reference + 4,
                    token_type: semantic::FUNCTION,
                    modifiers: 0,
                },
            ],
            hover_entries: vec![HoverEntry {
                range: reference..reference + 4,
                signature: "def good: () -> I32".to_owned(),
                documentation: Vec::new(),
            }],
        };

        let semantics = merge_semantic_entries(new, Vec::new(), &successful);
        assert_eq!(semantics.len(), 2);
        assert!(
            semantics
                .iter()
                .all(|entry| entry.token_type == semantic::FUNCTION)
        );
        assert!(
            semantics
                .iter()
                .all(|entry| &new[entry.start..entry.end] == "good")
        );

        let hover = remap_hover_entries(new, &successful);
        assert_eq!(hover.len(), 1);
        assert_eq!(&new[hover[0].range.clone()], "good");
        assert_eq!(hover[0].signature, "def good: () -> I32");
    }

    #[test]
    fn definition_ranges_use_utf16_and_crlf_positions() {
        let source = "😀\r\nlet café = 1\r\ncafé\r\n";
        let declaration = source.find("café").unwrap();
        let reference = source.rfind("café").unwrap();
        assert_eq!(
            lsp_range(source, declaration..declaration + "café".len()),
            Range::new(Position::new(1, 4), Position::new(1, 8))
        );
        assert_eq!(
            lsp_range(source, reference..reference + "café".len()),
            Range::new(Position::new(2, 0), Position::new(2, 4))
        );
    }

    #[test]
    fn diagnostics_do_not_include_preceding_blank_lines() {
        let source = concat!(
            "macro what = x => parse_quote {$x}\n",
            "\n",
            "\n",
            "missing\n",
            "\n",
            "def main = () => ()\n",
        );
        let path = std::env::temp_dir().join("staple-diagnostic-leading-trivia.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_source_at(&path, source)
            .unwrap();
        let diagnostics = NameResolver::new().resolve_program(program).unwrap_err();
        let diagnostic = diagnostics
            .into_iter()
            .find(|diagnostic| diagnostic.message.contains("missing"))
            .expect("missing name diagnostic");
        let Span::User { range, .. } = diagnostic.span else {
            panic!("expected a source diagnostic")
        };
        assert!(range.start < source.find("missing").unwrap());
        let diagnostic = lsp_diagnostic(source, range, diagnostic.message);

        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(3, 0), Position::new(3, 7))
        );
    }

    #[test]
    fn analyzes_an_empty_open_package_root_against_its_own_source() {
        let root = std::env::temp_dir().join(format!(
            "staple-lsp-empty-package-root-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("binder.kdl"), "package \"example\"\n").unwrap();
        std::fs::write(root.join("src/main.sta"), "42\n").unwrap();
        std::fs::write(root.join("src/root.sta"), "").unwrap();
        let root_path = std::fs::canonicalize(root.join("src/root.sta")).unwrap();
        let uri = path_to_uri(&root_path).unwrap();
        let (connection, _client) = Connection::memory();
        let mut server = Server {
            connection,
            documents: HashMap::from([(
                uri.clone(),
                Document {
                    text: String::new(),
                    version: 1,
                    ..Document::default()
                },
            )]),
            published_by_root: HashMap::new(),
            stdlib: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib")),
        };

        server
            .analyze(&uri)
            .expect("an empty opened root module should not project entry ranges onto itself");
        assert!(server.documents[&uri].semantic_tokens.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn analyzes_an_open_standard_library_module_from_its_package() {
        let stdlib = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let path = std::fs::canonicalize(stdlib.join("std/core/flow.sta")).unwrap();
        let source = std::fs::read_to_string(&path).unwrap();
        let uri = path_to_uri(&path).unwrap();
        let (connection, _client) = Connection::memory();
        let mut server = Server {
            connection,
            documents: HashMap::from([(
                uri.clone(),
                Document {
                    text: source.clone(),
                    version: 1,
                    ..Document::default()
                },
            )]),
            published_by_root: HashMap::new(),
            stdlib: Some(stdlib),
        };

        server.analyze(&uri).unwrap();

        let document = &server.documents[&uri];
        let imported = source.find("Expr").unwrap();
        assert!(
            document
                .hover_entries
                .iter()
                .any(|entry| { entry.range.start <= imported && imported < entry.range.end })
        );
        assert!(
            document
                .definition_entries
                .iter()
                .any(|entry| { entry.range.start <= imported && imported < entry.range.end })
        );
    }

    #[test]
    fn hover_and_semantics_track_a_macro_call_in_a_non_first_package_module() {
        // `main.sta` is parsed after the package-root module, so the editor's
        // own `parse` numbers its `SyntaxId`s below the ones the program gave
        // it. Without realigning them, hover on the `when` macro call reported
        // a neighbouring node's type (`I32`) and the semantic classifier
        // mislabelled the token.
        let root = std::env::temp_dir().join(format!(
            "staple-lsp-macro-call-shift-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("binder.kdl"), "package \"hello_world\"\n").unwrap();
        std::fs::write(root.join("src/root.sta"), "/// Test docs\npub mod").unwrap();
        let source = concat!(
            "use std.io.println\n",
            "\n",
            "let x = 3\n",
            "let y: I32 = x + 30\n",
            "\n",
            "when {\n",
            "    True => println \"123\"\n",
            "}\n",
            "\n",
            "println \"Hello, world!\"\n",
        );
        std::fs::write(root.join("src/main.sta"), source).unwrap();
        let path = std::fs::canonicalize(root.join("src/main.sta")).unwrap();
        let uri = path_to_uri(&path).unwrap();
        let (connection, _client) = Connection::memory();
        let mut server = Server {
            connection,
            documents: HashMap::from([(
                uri.clone(),
                Document {
                    text: source.to_owned(),
                    version: 1,
                    ..Document::default()
                },
            )]),
            published_by_root: HashMap::new(),
            stdlib: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib")),
        };

        server.analyze(&uri).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        let document = &server.documents[&uri];
        let when = source.find("when {").unwrap();

        let hover = document
            .hover_entries
            .iter()
            .find(|entry| entry.range.start <= when && when < entry.range.end)
            .unwrap_or_else(|| panic!("no hover entry for `when`: {:?}", document.hover_entries));
        assert!(
            hover.signature.contains("macro") && hover.signature.contains("when"),
            "`when` hover should describe the macro, got {:?}",
            hover.signature
        );
        assert!(
            !hover.signature.contains("I32"),
            "`when` hover should not report a neighbouring node's type, got {:?}",
            hover.signature
        );

        let semantics = &document
            .last_successful
            .as_ref()
            .expect("analysis succeeded")
            .semantic_entries;
        let token = semantics
            .iter()
            .find(|entry| entry.start == when && entry.end == when + "when".len())
            .unwrap_or_else(|| panic!("no semantic token for `when`: {semantics:?}"));
        assert_eq!(
            token.token_type,
            semantic::MACRO,
            "`when` should be classified as a macro"
        );
    }

    #[test]
    fn serves_the_core_protocol_in_memory() {
        let (server_connection, client) = Connection::memory();
        let server = std::thread::spawn(|| {
            initialize(&server_connection).unwrap();
            Server {
                connection: server_connection,
                documents: HashMap::new(),
                published_by_root: HashMap::new(),
                stdlib: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib")),
            }
            .event_loop()
            .unwrap();
        });

        client
            .sender
            .send(Message::Request(Request {
                id: 1.into(),
                method: Initialize::METHOD.to_owned(),
                params: serde_json::to_value(InitializeParams::default()).unwrap(),
            }))
            .unwrap();
        let Message::Response(response) = recv(&client) else {
            panic!("expected initialize response")
        };
        assert_eq!(response.id, 1.into());
        let result: InitializeResult =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert_eq!(
            result.capabilities.position_encoding,
            Some(PositionEncodingKind::UTF16)
        );
        assert!(result.capabilities.semantic_tokens_provider.is_some());
        assert_eq!(
            result.capabilities.hover_provider,
            Some(HoverProviderCapability::Simple(true))
        );
        assert_eq!(
            result.capabilities.definition_provider,
            Some(OneOf::Left(true))
        );
        assert!(result.capabilities.completion_provider.is_some());
        client
            .sender
            .send(Message::Notification(Notification::new(
                Initialized::METHOD.to_owned(),
                InitializedParams {},
            )))
            .unwrap();

        let uri: Uri = "file:///tmp/staple-lsp-test.sta".parse().unwrap();
        client
            .sender
            .send(Message::Notification(Notification::new(
                DidOpenTextDocument::METHOD.to_owned(),
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem::new(
                        uri.clone(),
                        "staple".to_owned(),
                        1,
                        "def broken = \"unterminated".to_owned(),
                    ),
                },
            )))
            .unwrap();
        let Message::Notification(notification) = recv(&client) else {
            panic!("expected diagnostics")
        };
        let diagnostics: PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        assert!(!diagnostics.diagnostics.is_empty());

        client
            .sender
            .send(Message::Request(Request {
                id: 2.into(),
                method: SemanticTokensFullRequest::METHOD.to_owned(),
                params: serde_json::to_value(SemanticTokensParams {
                    text_document: TextDocumentIdentifier::new(uri.clone()),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .unwrap(),
            }))
            .unwrap();
        let Message::Response(response) = recv(&client) else {
            panic!("expected token response")
        };
        let result: SemanticTokensResult =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert!(matches!(result, SemanticTokensResult::Tokens(tokens) if !tokens.data.is_empty()));

        client
            .sender
            .send(Message::Notification(Notification::new(
                DidChangeTextDocument::METHOD.to_owned(),
                DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier::new(uri.clone(), 2),
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: "let okay = 1\nokay\n".to_owned(),
                    }],
                },
            )))
            .unwrap();
        let Message::Notification(notification) = recv(&client) else {
            panic!("expected cleared diagnostics")
        };
        let diagnostics: PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        assert!(diagnostics.diagnostics.is_empty());

        client
            .sender
            .send(Message::Request(Request {
                id: 4.into(),
                method: HoverRequest::METHOD.to_owned(),
                params: serde_json::to_value(HoverParams {
                    text_document_position_params: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(0, 5),
                    ),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .unwrap(),
            }))
            .unwrap();
        let Message::Response(response) = recv(&client) else {
            panic!("expected hover response")
        };
        assert_eq!(response.id, 4.into());
        let hover: Option<Hover> =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert!(
            matches!(hover, Some(Hover { contents: HoverContents::Markup(content), .. }) if content.value.contains("I32"))
        );
        client
            .sender
            .send(Message::Request(Request {
                id: 8.into(),
                method: Completion::METHOD.to_owned(),
                params: serde_json::to_value(CompletionParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(1, 1),
                    ),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: None,
                })
                .unwrap(),
            }))
            .unwrap();
        let Message::Response(response) = recv(&client) else {
            panic!("expected completion response")
        };
        let completion: CompletionResponse =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert!(matches!(
            completion,
            CompletionResponse::Array(items)
                if items.iter().any(|item| item.label == "okay")
                    && items.iter().any(|item| item.label == "def")
        ));

        client
            .sender
            .send(Message::Request(Request {
                id: 6.into(),
                method: GotoDefinition::METHOD.to_owned(),
                params: serde_json::to_value(GotoDefinitionParams {
                    text_document_position_params: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(1, 1),
                    ),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .unwrap(),
            }))
            .unwrap();
        let Message::Response(response) = recv(&client) else {
            panic!("expected definition response")
        };
        let definition: Option<GotoDefinitionResponse> =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert!(
            matches!(
                &definition,
                Some(GotoDefinitionResponse::Link(links))
                    if links.len() == 1
                        && links[0].target_selection_range.start == Position::new(0, 4)
            ),
            "definition: {definition:?}"
        );

        client
            .sender
            .send(Message::Notification(Notification::new(
                DidChangeTextDocument::METHOD.to_owned(),
                DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier::new(uri.clone(), 3),
                    content_changes: vec![TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: "let okay = 1\nbroken ???\nokay\n".to_owned(),
                    }],
                },
            )))
            .unwrap();
        let Message::Notification(notification) = recv(&client) else {
            panic!("expected current diagnostics")
        };
        let diagnostics: PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        assert!(!diagnostics.diagnostics.is_empty());

        client
            .sender
            .send(Message::Request(Request {
                id: 5.into(),
                method: HoverRequest::METHOD.to_owned(),
                params: serde_json::to_value(HoverParams {
                    text_document_position_params: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(2, 1),
                    ),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                })
                .unwrap(),
            }))
            .unwrap();
        let Message::Response(response) = recv(&client) else {
            panic!("expected preserved hover response")
        };
        let hover: Option<Hover> =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert!(
            matches!(hover, Some(Hover { contents: HoverContents::Markup(content), .. }) if content.value.contains("let okay: I32"))
        );

        client
            .sender
            .send(Message::Request(Request {
                id: 7.into(),
                method: GotoDefinition::METHOD.to_owned(),
                params: serde_json::to_value(GotoDefinitionParams {
                    text_document_position_params: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(2, 1),
                    ),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                })
                .unwrap(),
            }))
            .unwrap();
        let Message::Response(response) = recv(&client) else {
            panic!("expected preserved definition response")
        };
        let definition: Option<GotoDefinitionResponse> =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert!(
            matches!(
                &definition,
                Some(GotoDefinitionResponse::Link(links))
                    if links.len() == 1
                        && links[0].target_selection_range.start == Position::new(0, 4)
            ),
            "definition: {definition:?}"
        );

        client
            .sender
            .send(Message::Request(Request {
                id: 9.into(),
                method: Completion::METHOD.to_owned(),
                params: serde_json::to_value(CompletionParams {
                    text_document_position: TextDocumentPositionParams::new(
                        TextDocumentIdentifier::new(uri.clone()),
                        Position::new(2, 1),
                    ),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                    context: None,
                })
                .unwrap(),
            }))
            .unwrap();
        let Message::Response(response) = recv(&client) else {
            panic!("expected preserved completion response")
        };
        let completion: CompletionResponse =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert!(matches!(
            completion,
            CompletionResponse::Array(items) if items.iter().any(|item| item.label == "okay")
        ));

        client
            .sender
            .send(Message::Notification(Notification::new(
                DidCloseTextDocument::METHOD.to_owned(),
                DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier::new(uri),
                },
            )))
            .unwrap();
        client
            .sender
            .send(Message::Request(Request {
                id: 3.into(),
                method: Shutdown::METHOD.to_owned(),
                params: serde_json::Value::Null,
            }))
            .unwrap();
        loop {
            if matches!(recv(&client), Message::Response(response) if response.id == 3.into()) {
                break;
            }
        }
        client
            .sender
            .send(Message::Notification(Notification::new(
                Exit::METHOD.to_owned(),
                (),
            )))
            .unwrap();
        server.join().unwrap();
    }

    fn recv(connection: &Connection) -> Message {
        connection
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    }
}
