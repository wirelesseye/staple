mod staple_lsp {
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
use lsp_types::request::{HoverRequest, Request as _, SemanticTokensFullRequest};
use lsp_types::*;
use staple_lsp::hover::{self, HoverEntry};
use staple_lsp::semantic;
use stapler::{Diagnostic as StapleDiagnostic, NameResolver, ProgramLoader, Span, TypeChecker};

#[derive(Default)]
struct Document {
    text: String,
    version: i32,
    semantic_tokens: Vec<SemanticToken>,
    hover_entries: Vec<HoverEntry>,
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
            let tokens = semantic::tokens(&text, stapler::parse(&text).ok().as_ref(), None);
            self.documents.get_mut(uri).unwrap().semantic_tokens = tokens;
            self.documents.get_mut(uri).unwrap().hover_entries.clear();
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
        let mut hover_entries = Vec::new();
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
                        resolved_for_tokens = Some(resolved.clone());
                        match TypeChecker::new().check(resolved) {
                            Ok(typed) => hover_entries = hover::entries(module, &typed),
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

        let tokens = semantic::tokens(&text, parsed.as_ref(), resolved_for_tokens.as_ref());
        self.documents.get_mut(uri).unwrap().semantic_tokens = tokens;
        self.documents.get_mut(uri).unwrap().hover_entries = hover_entries;

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
                        text: "let okay = 1\n".to_owned(),
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
