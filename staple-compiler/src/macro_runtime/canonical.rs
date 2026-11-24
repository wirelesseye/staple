//! A text form of expanded syntax that names nothing particular to one
//! compilation, for comparing and fingerprinting expansions.
//!
//! `Syntax` holds ids, spans, tokens, expansion marks and module numbers,
//! which differ between compilations of the same code, and a hygienic name
//! carries its mark as `name__macro_<n>`. The forms here drop the first and
//! renumber the second, so the same code expanded in another compilation, or
//! after an unrelated edit, gives the same text.

use std::collections::HashMap;

use staple_syntax::Item;

use crate::Program;

/// `items` as their debug form, with each `Syntax` reduced to its
/// `pipe_stage` flag and the name of the module its names resolve in, and
/// every hygiene number replaced by its order of first appearance.
///
/// A closure library's fingerprint hashes this for each module of its check
/// program, so it includes what earlier compiled macros produced but not
/// where in the file the code sits or how it is formatted.
pub fn canonical_items(program: &Program, items: &[Item]) -> String {
    let dump = format!("{items:#?}");
    let dump = drop_syntax_with(&dump, &|body| {
        let module = definition_module(body)
            .and_then(|index| program.modules().iter().find(|module| module.id.0 == index))
            .map_or("None", |module| module.qualified_name.as_str());
        format!(
            "Syntax {{ pipe_stage: {}, module: {module} }}",
            field(body, "pipe_stage")
        )
    });
    renumber_hygiene(&dump)
}

/// Replaces each whole-word `Syntax { … }` value in a debug dump with what
/// `keep` writes for its body.
pub(crate) fn drop_syntax_with(dump: &str, keep: &dyn Fn(&str) -> String) -> String {
    let marker = "Syntax {";
    let mut output = String::with_capacity(dump.len() / 2);
    let mut rest = dump;
    while let Some(position) = rest.find(marker) {
        // `CallExpr` and the like end in `Syntax` too, so only a whole word
        // starts a `Syntax` value.
        let whole = rest[..position]
            .chars()
            .next_back()
            .is_none_or(|character| !character.is_alphanumeric() && character != '_');
        if !whole {
            output.push_str(&rest[..position + marker.len()]);
            rest = &rest[position + marker.len()..];
            continue;
        }
        output.push_str(&rest[..position]);
        let body_start = position + marker.len();
        let end = closing_brace(&rest[body_start..]).map(|end| body_start + end);
        let Some(end) = end else {
            output.push_str(&rest[position..]);
            return output;
        };
        output.push_str(&keep(&rest[body_start..end]));
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

/// The value of a one-line field in a `Syntax` body, or `?`.
pub(crate) fn field(body: &str, name: &str) -> String {
    body.split_once(&format!("{name}: "))
        .and_then(|(_, value)| value.split([',', '\n']).next())
        .unwrap_or("?")
        .trim()
        .to_owned()
}

/// The module number in a `Syntax` body's `definition_module: Some(n)`, which
/// the pretty debug form may spread over several lines.
fn definition_module(body: &str) -> Option<usize> {
    let (_, rest) = body.split_once("definition_module: Some(")?;
    rest.trim_start()
        .split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// The offset of the `}` that closes a brace already opened, skipping string
/// literals.
fn closing_brace(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in text.char_indices() {
        if in_string {
            match character {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' if depth == 0 => return Some(offset),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Replaces every `__macro_<n>` number with its order of first appearance, as
/// the expansion comparison script does.
pub(crate) fn renumber_hygiene(text: &str) -> String {
    let marker = "__macro_";
    let mut numbers: HashMap<String, usize> = HashMap::new();
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find(marker) {
        let (before, after) = rest.split_at(position + marker.len());
        output.push_str(before);
        let digits = after
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        if digits.is_empty() {
            rest = after;
            continue;
        }
        let next = numbers.len();
        let number = *numbers.entry(digits.clone()).or_insert(next);
        output.push_str(&number.to_string());
        rest = &after[digits.len()..];
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use staple_syntax::{Item, SyntaxId};

    use super::canonical_items;
    use crate::{Program, ProgramLoader};

    fn load(source: &str) -> Program {
        ProgramLoader::new()
            .load_source(source, Path::new("."))
            .expect("program loads")
    }

    fn canonical(source: &str) -> String {
        let program = load(source);
        canonical_items(&program, &program.module(program.entry()).syntax.items)
    }

    #[test]
    fn position_and_formatting_do_not_change_the_text() {
        assert_eq!(
            canonical("pub def x = 1\npub def y = x\n"),
            canonical("\n\n// a comment\npub def x =   1\n\npub def y = x\n"),
        );
        assert_ne!(canonical("pub def x = 1\n"), canonical("pub def x = 2\n"));
    }

    #[test]
    fn ids_and_expansion_marks_are_left_out() {
        let program = load("pub def x = 1\n");
        let items = program.module(program.entry()).syntax.items.clone();
        let mut renumbered = items.clone();
        if let Item::Binding(binding) = &mut renumbered[0] {
            binding.syntax.id = SyntaxId(9000);
            binding.syntax.expansion_mark = Some(77);
        }
        assert_eq!(
            canonical_items(&program, &items),
            canonical_items(&program, &renumbered)
        );
    }

    #[test]
    fn the_module_names_resolve_in_is_kept_by_name() {
        let program = load("pub def x = 1\n");
        let items = program.module(program.entry()).syntax.items.clone();
        let mut generated = items.clone();
        if let Item::Binding(binding) = &mut generated[0] {
            binding.syntax.definition_module = Some(program.entry().0);
        }
        let text = canonical_items(&program, &generated);
        assert_ne!(canonical_items(&program, &items), text);
        let name = &program.module(program.entry()).qualified_name;
        assert!(text.contains(&format!("module: {name}")), "{text}");
    }

    #[test]
    fn hygiene_numbers_are_renumbered_by_first_appearance() {
        assert_eq!(
            canonical("pub def f = (x__macro_5: I32) => x__macro_5\n"),
            canonical("pub def f = (x__macro_9: I32) => x__macro_9\n"),
        );
        assert_ne!(
            canonical("pub def f = (x__macro_5: I32) => x__macro_5\n"),
            canonical("pub def f = (x__macro_5: I32) => x__macro_6\n"),
        );
    }
}
