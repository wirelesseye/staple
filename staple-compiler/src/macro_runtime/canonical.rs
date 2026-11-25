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
    let dump = staple_syntax::with_compact_syntax_debug(|| format!("{items:#?}"));
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

/// The entry module's items after expansion, as their debug form without any
/// `Syntax` and with every hygiene number replaced by its order of first
/// appearance.
///
/// `staple expand`'s rendering reads tokens where a node wasn't regenerated,
/// and a quotation the interpreter substitutes as a tree keeps its template's
/// `$name` tokens, so the rendering of an interpreted expansion can show
/// `impl Debug $target`. The tree is what the interpreter produced. `Syntax`
/// holds tokens, spans, ids and marks, which differ between the engines by
/// construction; resolving and type-checking each expansion covers what they
/// mean. Its `pipe_stage` flag is kept.
///
/// Compiled results are reparsed from text, which loses three things the
/// interpreter's trees keep, none of which changes what the code means:
///
/// - `implied`, which marks a node the parser supplied, such as an omitted
///   type. A node a macro builds has it unset, and nothing reads it.
/// - A `resolution_name` equal to the name. The resolver uses the name when a
///   binding has none.
/// - Whether a `let` that binds one name was written with a pattern. A
///   quotation's `let $pattern = …` is a pattern binding in the interpreter's
///   tree and a plain `let` once its text is read again.
///
/// One more difference comes from the interpreter. It binds a modifier's
/// `Parenthesized<…>` argument to what is inside the parentheses, so splicing
/// it writes `42` where compiled code, which has the group its type declares,
/// writes `(42)`. A parenthesized expression of one unnamed element is that
/// element.
///
/// The dump writes each of these one way.
pub fn tree_dump(program: &Program) -> String {
    let items = &program.module(program.entry()).syntax.items;
    let dump = staple_syntax::with_compact_syntax_debug(|| format!("{items:#?}"));
    let dump = canonical_lets(&same_resolution_names(&drop_syntax(&dump)));
    renumber_hygiene(&single_elements(&dump))
}

/// Writes a parenthesized expression of one unnamed element as the element.
fn single_elements(dump: &str) -> String {
    let lines = dump.lines().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        match single_element(&lines, index) {
            Some((element, next)) => {
                output.extend(element);
                index = next;
            }
            None => {
                output.push(lines[index].to_owned());
                index += 1;
            }
        }
    }
    let dump = output.join("\n");
    // An element may itself be one.
    if dump.lines().count() == lines.len() {
        dump
    } else {
        single_elements(&dump)
    }
}

/// The element of the one-element `Product(` at `start`, and the line after
/// the product.
fn single_element(lines: &[&str], start: usize) -> Option<(Vec<String>, usize)> {
    let prefix = lines[start].strip_suffix("Product(")?;
    if !prefix.trim().is_empty() && !prefix.trim_end().ends_with(':') {
        return None;
    }
    let depth = indent(lines[start]);
    let end = (start + 1..lines.len()).find(|&index| indent(lines[index]) == depth)?;
    let product = fields(lines, start + 2, end - 1, depth + 8);
    let field = |name: &str| {
        product
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, lines)| lines)
    };
    if field("slotted")?.first()?.trim() != "slotted: false," {
        return None;
    }
    let elements = field("elements")?;
    if elements.len() < 3 {
        return None;
    }
    let element_lines = &elements[1..elements.len() - 1];
    if element_lines.first()?.trim() != "ProductElement {" || element_lines.last()?.trim() != "}," {
        return None;
    }
    let element = fields(element_lines, 1, element_lines.len() - 1, depth + 16);
    if element.len() != 6 {
        return None;
    }
    let text = |name: &str| {
        element
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, lines)| {
                lines
                    .iter()
                    .map(|line| line.trim())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
    };
    for (name, expected) in [
        ("name", "name: None,"),
        ("designated", "designated: false,"),
        ("spread", "spread: false,"),
        ("named_spread", "named_spread: false,"),
    ] {
        if text(name)? != expected {
            return None;
        }
    }
    let value = element
        .iter()
        .find(|(field, _)| *field == "value")?
        .1
        .clone();
    let first = value[0].trim().strip_prefix("value: ")?;
    let mut written = vec![format!("{prefix}{first}")];
    written.extend(
        value[1..]
            .iter()
            .map(|line| line[16.min(indent(line))..].to_owned()),
    );
    Some((written, end + 1))
}

/// Replaces each `Syntax { ... }` in a debug dump with its `pipe_stage` flag.
fn drop_syntax(dump: &str) -> String {
    drop_syntax_with(dump, &|body| {
        format!("Syntax {{ pipe_stage: {} }}", field(body, "pipe_stage"))
    })
}

/// Writes a `resolution_name` that repeats the binding's name as `None`.
fn same_resolution_names(dump: &str) -> String {
    let lines = dump.lines().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let name = output
            .last()
            .and_then(|previous: &String| previous.trim().strip_prefix("name: "))
            .map(str::to_owned);
        if line.trim() == "resolution_name: Some("
            && let Some(name) = name
            && lines.get(index + 1).map(|line| line.trim()) == Some(name.as_str())
            && lines.get(index + 2).map(|line| line.trim()) == Some("),")
        {
            output.push(line.replace("Some(", "None,"));
            index += 3;
            continue;
        }
        output.push(line.to_owned());
        index += 1;
    }
    output.join("\n")
}

/// Writes a `let` that binds one name, without a type or `mut`, as `Let`,
/// whether it is a plain binding or an irrefutable pattern binding.
fn canonical_lets(dump: &str) -> String {
    let lines = dump.lines().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        match canonical_let(&lines, index) {
            Some((canonical, next)) => {
                output.extend(canonical);
                index = next;
            }
            None => {
                output.push(lines[index].to_owned());
                index += 1;
            }
        }
    }
    output.join("\n")
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The fields of a debug-printed struct whose fields are at `depth`, from its
/// lines `start..end`, each with its lines.
fn fields<'a>(
    lines: &[&'a str],
    start: usize,
    end: usize,
    depth: usize,
) -> Vec<(&'a str, Vec<&'a str>)> {
    let mut fields: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in &lines[start..end] {
        if indent(line) == depth
            && let Some((name, _)) = line.trim().split_once(": ")
        {
            fields.push((name, vec![line]));
        } else if let Some((_, field)) = fields.last_mut() {
            field.push(line);
        }
    }
    fields
}

/// The `Let` form of the `Binding(` or `PatternBinding(` item at `start`, and
/// the line after it.
fn canonical_let(lines: &[&str], start: usize) -> Option<(Vec<String>, usize)> {
    let header = lines[start].trim();
    let pattern = match header {
        "PatternBinding(" => true,
        "Binding(" => false,
        _ => return None,
    };
    let depth = indent(lines[start]);
    let inner = lines.get(start + 1)?.trim();
    if inner
        != if pattern {
            "PatternBinding {"
        } else {
            "Binding {"
        }
    {
        return None;
    }
    let end = (start + 1..lines.len()).find(|&index| indent(lines[index]) == depth)?;
    let fields = fields(lines, start + 2, end - 1, depth + 8);
    let field = |name: &str| {
        fields
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, lines)| lines)
    };
    let text = |name: &str| {
        field(name).map(|lines| {
            lines
                .iter()
                .map(|line| line.trim())
                .collect::<Vec<_>>()
                .join(" ")
        })
    };
    let shift = |line: &str, by: usize| line[by.min(indent(line))..].to_owned();
    let (name, value) = if pattern {
        let binding = text("pattern")?;
        let simple = binding.starts_with("pattern: Binding( BindingPattern {")
            && binding.contains(" mutable: false,")
            && binding.contains(" moved: false,")
            && binding.contains(" resolution_name: None,")
            && binding.contains(" ty: Inferred(");
        if text("kind")? != "kind: Irrefutable," || !simple {
            return None;
        }
        let name = binding
            .split_once(" name: ")?
            .1
            .split(',')
            .next()?
            .to_owned();
        let value = field("value")?;
        let mut written = vec![format!(
            "{}value: {}",
            " ".repeat(depth + 4),
            value[0].trim().strip_prefix("value: ")?
        )];
        written.extend(value[1..].iter().map(|line| shift(line, 4)));
        (name, written)
    } else {
        for (name, expected) in [
            ("docs", "docs: [],"),
            ("member_modifiers", "member_modifiers: [],"),
            ("visibility", "visibility: Private,"),
            ("kind", "kind: Let,"),
            ("mutable", "mutable: false,"),
            ("signal", "signal: false,"),
            ("annotation", "annotation: None,"),
            ("companion_target", "companion_target: None,"),
            ("companion_generics", "companion_generics: None,"),
            ("track_caller", "track_caller: false,"),
        ] {
            if text(name)? != expected {
                return None;
            }
        }
        let name = text("name")?
            .strip_prefix("name: ")?
            .trim_end_matches(',')
            .to_owned();
        let value = field("value")?;
        if value[0].trim() != "value: Some(" || value.len() < 3 {
            return None;
        }
        let inner = &value[1..value.len() - 1];
        let mut written = vec![format!(
            "{}value: {}",
            " ".repeat(depth + 4),
            inner[0].trim()
        )];
        written.extend(inner[1..].iter().map(|line| shift(line, 8)));
        (name, written)
    };
    let mut canonical = vec![
        format!("{}Let(", " ".repeat(depth)),
        format!("{}name: {name},", " ".repeat(depth + 4)),
    ];
    canonical.extend(value);
    canonical.push(lines[end].to_owned());
    Some((canonical, end + 1))
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
