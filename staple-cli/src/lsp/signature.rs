use lsp_types::{Documentation, MarkupContent, MarkupKind, SignatureHelp, SignatureInformation};

use super::hover::HoverEntry;

/// Reuse checked hover schemes, including the trait-then-member parameter
/// order. The lexical call tail also works with last-successful hover data
/// while the user is typing an incomplete argument or type argument list.
pub(super) fn help(text: &str, entries: &[HoverEntry], offset: usize) -> Option<SignatureHelp> {
    let mut candidates = entries
        .iter()
        .filter(|entry| {
            entry.range.end <= offset
                && entry.signature.contains("->")
                && text
                    .get(entry.range.end..offset)
                    .is_some_and(active_call_tail)
        })
        .collect::<Vec<_>>();
    let end = candidates.iter().map(|entry| entry.range.end).max()?;
    candidates.retain(|entry| entry.range.end == end);
    candidates.sort_by(|a, b| a.signature.cmp(&b.signature));
    candidates.dedup_by(|a, b| a.signature == b.signature);
    Some(SignatureHelp {
        signatures: candidates
            .into_iter()
            .map(|entry| SignatureInformation {
                label: entry.signature.clone(),
                documentation: (!entry.documentation.is_empty()).then(|| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: entry.documentation.join("\n"),
                    })
                }),
                parameters: None,
                active_parameter: None,
            })
            .collect(),
        active_signature: Some(0),
        active_parameter: None,
    })
}

fn active_call_tail(tail: &str) -> bool {
    let Some(first) = tail.chars().next() else {
        return false;
    };
    let trimmed = tail.trim_start();
    if !(first.is_whitespace() || first == '(' || trimmed.starts_with(".<")) {
        return false;
    }
    if trimmed.starts_with([':', '=', ';']) {
        return false;
    }
    let mut delimiters = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut previous = '\0';
    for ch in tail.chars() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else {
            match ch {
                '"' => quoted = true,
                '(' => delimiters.push(')'),
                '[' => delimiters.push(']'),
                '{' => delimiters.push('}'),
                '<' if previous == '.' || delimiters.last() == Some(&'>') => delimiters.push('>'),
                '>' if previous == '-' => {}
                ')' | ']' | '}' | '>' if delimiters.last() == Some(&ch) => {
                    delimiters.pop();
                    if delimiters.is_empty() && ch != '>' {
                        return false;
                    }
                }
                ')' | ']' | '}' => return false,
                '\n' | ';' | '=' if delimiters.is_empty() => return false,
                '/' if previous == '/' => return false,
                _ => {}
            }
        }
        previous = ch;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use staple_compiler::{NameResolver, ProgramLoader, TypeChecker};
    use std::path::PathBuf;

    #[test]
    fn generic_trait_member_signature_help_keeps_explicit_argument_order() {
        let source = "trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = (_, move value) => value }\nlet answer = Visit.visit.<I32, String> (1, \"value\")\n";
        let path = std::env::temp_dir().join("staple-stage7-signature.sta");
        let program = ProgramLoader::new()
            .with_standard_library_root(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join("stdlib"),
            )
            .load_source_at(&path, source)
            .unwrap();
        let typed = TypeChecker::new()
            .check(NameResolver::new().resolve_program(program).unwrap())
            .unwrap();
        let surface = staple_syntax::parse(source).unwrap();
        let surface = super::super::rebased_surface(typed.resolved(), &path, &surface, source)
            .unwrap_or(surface);
        let entries = super::super::hover::entries_at_path(&path, &surface, &typed);
        for offset in [
            source.rfind("String>").unwrap() + 2,
            source.rfind("value").unwrap() + 2,
        ] {
            let result = help(source, &entries, offset).unwrap_or_else(|| {
                panic!("a member call has signature help at {offset}: {entries:?}")
            });
            assert!(
                result.signatures[0]
                    .label
                    .contains("Visit.visit: <T, A where Visit T>"),
                "{result:?}"
            );
        }
        let (connection, client) = lsp_server::Connection::memory();
        let uri = super::super::path_to_uri(&path).unwrap();
        let mut server = super::super::Server {
            connection,
            documents: std::collections::HashMap::from([(
                uri.clone(),
                super::super::Document {
                    text: source.to_owned(),
                    hover_entries: entries.clone(),
                    ..super::super::Document::default()
                },
            )]),
            published_by_root: Default::default(),
            stdlib: None,
        };
        let (line, character) =
            super::super::semantic::position(source, source.rfind("String>").unwrap() + 2);
        server.request(lsp_server::Request {
            id: 1.into(), method: "textDocument/signatureHelp".to_owned(),
            params: serde_json::json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } }),
        }).unwrap();
        let lsp_server::Message::Response(response) = client.receiver.recv().unwrap() else {
            panic!("expected a signature-help response");
        };
        let result: SignatureHelp =
            serde_json::from_value(response.response_result.unwrap()).unwrap();
        assert!(result.signatures[0].label.contains("<T, A where Visit T>"));
        let incomplete = &source[..source.rfind("String>").unwrap()];
        assert!(help(incomplete, &entries, incomplete.len()).is_some());
        assert!(help(source, &entries, source.len()).is_none());
    }

    #[test]
    fn signature_help_stops_after_calls_and_ignores_comments() {
        for tail in [
            " (1)",
            " ()\nnext",
            " : I32",
            " = value",
            " // comment",
            " (1, // comment",
        ] {
            assert!(!active_call_tail(tail), "{tail:?}");
        }
        for tail in [
            " ",
            " (",
            " (1, ",
            ".<I32, ",
            ".<(I32, String), _> (",
            " (\"argument",
        ] {
            assert!(active_call_tail(tail), "{tail:?}");
        }
    }
}
