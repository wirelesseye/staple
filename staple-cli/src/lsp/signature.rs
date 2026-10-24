use lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureHelp,
    SignatureInformation,
};

use super::hover::HoverEntry;

/// Reuse checked hover schemes, including the trait-then-member parameter
/// order. The lexical call tail also works with last-successful hover data
/// while the user is typing an incomplete argument or type argument list.
pub(super) fn help(text: &str, entries: &[HoverEntry], offset: usize) -> Option<SignatureHelp> {
    let mut candidates = entries
        .iter()
        .filter(|entry| {
            entry.range.end <= offset
                && parameter_information(&entry.signature).is_some()
                && text.get(entry.range.end..offset).is_some_and(|tail| {
                    let slots = parameter_information(&entry.signature)
                        .map_or(1, |parameters| parameters.len());
                    active_call_tail_for_slots(tail, slots)
                        && checked_active_slot(text, entries, entry, offset).is_none_or(|slot| {
                            slot < slots && !(slot + 1 == slots && tail.ends_with([')', ']', '}']))
                        })
                })
        })
        .collect::<Vec<_>>();
    let end = candidates.iter().map(|entry| entry.range.end).max()?;
    candidates.retain(|entry| entry.range.end == end);
    candidates.sort_by(|a, b| a.signature.cmp(&b.signature));
    candidates.dedup_by(|a, b| a.signature == b.signature);
    let signatures = candidates
        .into_iter()
        .map(|entry| {
            let parameters = parameter_information(&entry.signature);
            let active_parameter = parameters.as_ref().and_then(|parameters| {
                checked_active_slot(text, entries, entry, offset)
                    .filter(|slot| *slot < parameters.len())
                    .map(|slot| slot as u32)
            });
            SignatureInformation {
                label: entry.signature.clone(),
                documentation: (!entry.documentation.is_empty()).then(|| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: entry.documentation.join("\n"),
                    })
                }),
                parameters,
                active_parameter,
            }
        })
        .collect::<Vec<_>>();
    Some(SignatureHelp {
        active_parameter: signatures
            .first()
            .and_then(|signature| signature.active_parameter),
        signatures,
        active_signature: Some(0),
    })
}

fn parameter_information(label: &str) -> Option<Vec<ParameterInformation>> {
    let start = label.find(": ")? + 2;
    let prefix = "type Signature = ";
    let module = staple_syntax::parse(&format!("{prefix}{}\n", &label[start..])).ok()?;
    let staple_syntax::Item::TypeDeclaration(declaration) = module.items.first()? else {
        return None;
    };
    let mut ty = declaration.body.as_ref()?.underlying.as_ref()?;
    while let staple_syntax::Type::Forall(forall) = ty {
        ty = &forall.body;
    }
    let staple_syntax::Type::Function(function) = ty else {
        return None;
    };
    let ranges = match function.parameter.as_ref() {
        staple_syntax::Type::Product(product) if product.slotted => product
            .elements
            .iter()
            .map(|element| element.syntax.span.to_range())
            .collect::<Vec<_>>(),
        parameter => vec![parameter.syntax().span.to_range()],
    };
    ranges
        .into_iter()
        .map(|range| {
            let left = start.checked_add(range.start.checked_sub(prefix.len())?)?;
            let right = start.checked_add(range.end.checked_sub(prefix.len())?)?;
            let field = label.get(left..right)?;
            let left = left + field.len() - field.trim_start().len();
            let right = right - field.len() + field.trim_end().len();
            Some(ParameterInformation {
                label: ParameterLabel::LabelOffsets([
                    label.get(..left)?.encode_utf16().count() as u32,
                    label.get(..right)?.encode_utf16().count() as u32,
                ]),
                documentation: None,
            })
        })
        .collect()
}

/// Count atomic arguments outside grouped expressions and explicit type arguments.
fn active_slot(tail: &str) -> Option<usize> {
    active_slot_with_shapes(tail, &|_, _| None)
}

fn checked_active_slot(
    text: &str,
    entries: &[HoverEntry],
    callee: &HoverEntry,
    offset: usize,
) -> Option<usize> {
    active_slot_with_shapes(&text[callee.range.end..offset], &|operand, at| {
        entries.iter().find_map(|entry| {
            let start = callee.range.end + at;
            if entry.range != (start..start + operand.len())
                || text.get(entry.range.clone()) != Some(operand)
            {
                return None;
            }
            let start = if entry.signature.starts_with('(') {
                0
            } else {
                entry.signature.find(": ")? + 2
            };
            let signature = &entry.signature[start..];
            if signature.contains(" ^ ")
                && let Some(slots) = normal_form_product_slots(signature)
            {
                return Some(slots);
            }
            let module = staple_syntax::parse(&format!("type Shape = {signature}")).ok()?;
            let staple_syntax::Item::TypeDeclaration(declaration) = module.items.first()? else {
                return None;
            };
            match declaration.body.as_ref()?.underlying.as_ref()? {
                staple_syntax::Type::Product(product)
                    if !product.variadic
                        && product.elements.iter().all(|element| !element.spread) =>
                {
                    Some(product.elements.len())
                }
                staple_syntax::Type::Array(array) => {
                    let staple_syntax::Type::NumberLiteral(count) = array.count.as_ref() else {
                        return None;
                    };
                    count.literal.parse::<usize>().ok()
                }
                _ => None,
            }
        })
    })
}

fn active_slot_with_shapes(
    tail: &str,
    shape: &impl Fn(&str, usize) -> Option<usize>,
) -> Option<usize> {
    let original_len = tail.len();
    let mut tail = tail.trim_start();
    if tail.starts_with(".<") {
        let mut depth = 0;
        let mut end = None;
        for token in staple_syntax::lex(tail) {
            match token.text.as_str() {
                "<" => depth += 1,
                ">" => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(token.span.end);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return Some(0);
        };
        tail = &tail[end..];
    }
    let base = original_len - tail.len();
    let mut depth = 0;
    let mut arguments: usize = 0;
    let mut start = None;
    for token in staple_syntax::lex(tail) {
        if token.kind.is_trivia() {
            if depth == 0 {
                if let Some(left) = start.take() {
                    arguments += argument_slots(&tail[left..token.span.start], base + left, shape)?;
                }
            }
            continue;
        }
        start.get_or_insert(token.span.start);
        match token.text.as_str() {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth -= 1,
            _ => {}
        }
    }
    if let Some(left) = start {
        let argument = &tail[left..];
        if argument.starts_with("...") && depth == 0 {
            return Some(
                arguments + argument_slots(argument, base + left, shape)?.saturating_sub(1),
            );
        }
    }
    Some(arguments)
}

fn argument_slots(
    argument: &str,
    at: usize,
    shape: &impl Fn(&str, usize) -> Option<usize>,
) -> Option<usize> {
    let Some(operand) = argument.strip_prefix("...") else {
        return Some(1);
    };
    if let Some(slots) = shape(operand, at + 3) {
        return Some(slots);
    }
    let module = staple_syntax::parse(operand).ok()?;
    let [staple_syntax::Item::Expression(expression)] = module.items.as_slice() else {
        return None;
    };
    literal_product_slots(expression)
}

/// Counts positions in a printed product normal form such as
/// `(I32 ^ 3; String, I32 ^ 2)`, including the repeated slotted spelling
/// `(I32 ^ 2;)` that is not valid source syntax.
fn normal_form_product_slots(signature: &str) -> Option<usize> {
    let start = signature.find('(')?;
    let mut depth = 0usize;
    let mut end = None;
    for (index, character) in signature[start..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(start + index);
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = &signature[start + 1..end?];
    let mut slots = 0usize;
    let mut segment_start = 0usize;
    let mut depth = 0usize;
    let mut segments = Vec::new();
    for (index, character) in inner.char_indices() {
        match character {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ';' | ',' if depth == 0 => {
                segments.push(&inner[segment_start..index]);
                segment_start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    segments.push(&inner[segment_start..]);
    for segment in segments {
        let segment = segment.trim().trim_end_matches(';').trim();
        if segment.is_empty() {
            continue;
        }
        if let Some((_, count)) = segment.rsplit_once(" ^ ") {
            let digits = count
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            slots += digits.parse::<usize>().ok()?;
        } else {
            slots += 1;
        }
    }
    Some(slots)
}

fn literal_product_slots(expression: &staple_syntax::Expression) -> Option<usize> {
    let staple_syntax::Expression::Product(product) = expression else {
        return None;
    };
    product.elements.iter().try_fold(0, |slots, element| {
        Some(
            slots
                + if element.spread {
                    literal_product_slots(&element.value)?
                } else {
                    1
                },
        )
    })
}

#[cfg(test)]
fn active_call_tail(tail: &str) -> bool {
    active_call_tail_for_slots(tail, 1)
}

fn active_call_tail_for_slots(tail: &str, slots: usize) -> bool {
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
                    if delimiters.is_empty() && ch != '>' && slots <= 1 {
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
    if slots > 1 && delimiters.is_empty() {
        if let Some(slot) = active_slot(tail) {
            if slot >= slots || (slot + 1 == slots && tail.ends_with([')', ']', '}'])) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use staple_compiler::{NameResolver, ProgramLoader, TypeChecker};
    use std::path::PathBuf;

    #[test]
    fn slotted_signatures_highlight_atomic_arguments_and_utf16_ranges() {
        let label = "def 𝒙: (left: (I32, I32); right: String) -> I32";
        let entry = HoverEntry {
            range: 0.."𝒙".len(),
            signature: label.to_owned(),
            documentation: vec![],
            module: None,
            instantiation: None,
        };
        for (tail, active) in [
            (" ", 0),
            (" (1, ", 0),
            (" (1, 2) ", 1),
            (" (1, 2) \"arg", 1),
        ] {
            let text = format!("𝒙{tail}");
            let help = help(&text, std::slice::from_ref(&entry), text.len()).unwrap();
            assert_eq!(help.active_parameter, Some(active), "{tail}");
            assert_eq!(help.signatures[0].active_parameter, Some(active));
            let parameters = help.signatures[0].parameters.as_ref().unwrap();
            assert_eq!(parameters.len(), 2);
            for (parameter, expected) in
                parameters.iter().zip(["left: (I32, I32)", "right: String"])
            {
                let ParameterLabel::LabelOffsets([start, end]) = parameter.label else {
                    panic!("offset label");
                };
                let utf16 = label.encode_utf16().collect::<Vec<_>>();
                assert_eq!(
                    String::from_utf16(&utf16[start as usize..end as usize]).unwrap(),
                    expected
                );
            }
        }
        assert_eq!(active_slot(".<(I32; I32), String> (1; 2) "), Some(1));
        for tail in [" (1, 2) (3, 4)", " 1 2 "] {
            let text = format!("𝒙{tail}");
            assert!(help(&text, std::slice::from_ref(&entry), text.len()).is_none());
        }
        assert_eq!(active_slot("...pair "), None);
        assert_eq!(active_slot("...(1, 2) "), Some(2));
        assert_eq!(active_slot("...(1; 2, 3) "), Some(2));
        assert_eq!(active_slot("1 ...(2, 3) "), Some(3));
        assert_eq!(active_slot("...((1, 2), 3) "), Some(2));
        assert_eq!(active_slot("...(...(1, 2), 3) "), Some(3));
        assert_eq!(active_slot("...(1, "), Some(0));
        assert!(!active_call_tail_for_slots(" ...(1, 2)", 2));
        assert_eq!(active_slot(".<I32 -> I32, String> (1; 2) "), Some(1));
        assert_eq!(active_slot(".<\"a>b\", String> 1 "), Some(1));
    }

    #[test]
    fn checked_spreads_advance_signature_slots() {
        for (ty, value) in [("(I32, I32)", "(1, 2)"), ("(I32; I32)", "(1; 2)")] {
            for operand in ["pair", "wrapped.pair", "(make ())"] {
                let source = format!(
                    "def f: (I32; I32; I32) -> I32 = (a; b; c) => a + b + c\nlet pair: {ty} = {value}\nlet wrapped = (pair: pair, other: 0)\ndef make: () -> {ty} = () => pair\nf ...{operand} 3\nf 1 ...(make ())\n"
                );
                let path = std::env::temp_dir().join("staple-named-spread-signature.sta");
                let program = ProgramLoader::new()
                    .with_standard_library_root(
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .parent()
                            .unwrap()
                            .join("stdlib"),
                    )
                    .load_source_at(&path, &source)
                    .unwrap();
                let typed = TypeChecker::new()
                    .check(NameResolver::new().resolve_program(program).unwrap())
                    .unwrap();
                let surface = staple_syntax::parse(&source).unwrap();
                let entries = super::super::hover::entries_at_path(&path, &surface, &typed);
                let offset = source.rfind(" 3\n").unwrap() + 1;
                let result = help(&source, &entries, offset).expect("known spread leaves one slot");
                assert_eq!(result.active_parameter, Some(2), "{entries:?}");
                let end = source.trim_end().len();
                assert!(help(&source, &entries, end).is_none());
                let completed = source.trim_end().to_owned() + " ";
                assert!(help(&completed, &entries, completed.len()).is_none());
            }
        }
    }

    #[test]
    fn function_value_generics_show_the_inferred_signature() {
        let source = "def identity = <T> move value: T => T :: value\nlet answer = identity 42\n";
        let path = std::env::temp_dir().join("staple-value-generic-signature.sta");
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
        let result = help(source, &entries, source.rfind("42").unwrap() + 1).unwrap();
        assert!(
            result.signatures[0]
                .label
                .contains("identity: <T> move T -> T"),
            "{result:?}"
        );
    }

    #[test]
    fn generic_trait_member_signature_help_keeps_explicit_argument_order() {
        let source = "trait Visit T { visit: <A> (T, move A) -> A }\nimpl Visit I32 { visit = <A> (_, move value) => value }\nlet answer = Visit.visit.<I32, String> (1, \"value\")\n";
        let path = std::env::temp_dir().join("staple-trait-member-signature.sta");
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
