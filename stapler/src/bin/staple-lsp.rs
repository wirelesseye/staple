mod staple_lsp {
    pub mod definition;
    pub mod hover;
    pub mod semantic;
}

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lsp_server::{Connection, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{GotoDefinition, HoverRequest, Request as _, SemanticTokensFullRequest};
use lsp_types::*;
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
    server.event_loop()?;
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
                let entry = document
                    .hover_entries
                    .iter()
                    .filter(|entry| entry.range.start <= offset && offset < entry.range.end)
                    .min_by_key(|entry| entry.range.end - entry.range.start)?;
                let (start_line, start_character) =
                    semantic::position(&document.text, entry.range.start);
                let (end_line, end_character) = semantic::position(&document.text, entry.range.end);
                Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("```staple\n{}\n```", entry.signature),
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
        let mut definition_entries = None;
        let mut definition_path = path.clone();
        let mut analysis_succeeded = false;
        if let Some(module) = &parsed {
            let mut loader = ProgramLoader::new();
            if let Some(stdlib) = &self.stdlib {
                loader = loader.with_standard_library_root(stdlib);
            }
            match loader.load_source_at(&path, &text) {
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
                        definition_path = resolved
                            .program()
                            .module(resolved.program().entry())
                            .path
                            .clone();
                        definition_entries = Some(definition::entries(module, &resolved, None));
                        resolved_for_tokens = Some(resolved.clone());
                        match TypeChecker::new().check(resolved) {
                            Ok(typed) => {
                                definition_entries = Some(definition::entries(
                                    module,
                                    typed.resolved(),
                                    Some(&typed),
                                ));
                                hover_entries = hover::entries(module, &typed);
                                typed_for_tokens = Some(typed);
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

        let current_semantic = semantic::entries(
            &text,
            parsed.as_ref(),
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
        let result: InitializeResult = serde_json::from_value(response.result.unwrap()).unwrap();
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
            serde_json::from_value(response.result.unwrap()).unwrap();
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
        let hover: Option<Hover> = serde_json::from_value(response.result.unwrap()).unwrap();
        assert!(
            matches!(hover, Some(Hover { contents: HoverContents::Markup(content), .. }) if content.value.contains("I32"))
        );

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
            serde_json::from_value(response.result.unwrap()).unwrap();
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
        let hover: Option<Hover> = serde_json::from_value(response.result.unwrap()).unwrap();
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
            serde_json::from_value(response.result.unwrap()).unwrap();
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
