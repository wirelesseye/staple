//! Expanding one program through both engines.
//!
//! The program is expanded twice: once by the interpreter, and once with every
//! call of one package's macros made by the macros' compiled entry functions.
//! Each compiled call goes through the wire encoding both ways, as it would to
//! a separate process. The two expanded trees are compared, and each is
//! resolved and type-checked, so names a macro generated must resolve where
//! its hygiene says.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::macro_expand::check_program::{CheckMode, check_programs, entry_function_name};
use crate::macro_expand::compiled_call::{
    ClosureLibraryRequest, CompiledCall, MacroBackend, record_calls, with_macro_backend,
};
use crate::macro_runtime::harness::{
    CheckRun, install_templates, lower_run, standard_library_root, with_jit, workspace_root,
};
use crate::macro_runtime::quote::{CallContext, begin_call, next_mark};
use crate::macro_runtime::wire::{Message, decode, encode};
use crate::{NameResolver, Program, ProgramLoader, TypeChecker};
use staple_syntax::Item;

/// Whose macros run compiled.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Compiled {
    /// The standard library's.
    Library,
    /// The source's own.
    Own,
}

/// One program expanded both ways.
pub(crate) struct Comparison {
    /// The interpreter's expansion of the entry module, as [`tree_dump`]
    /// writes it.
    pub(crate) interpreted: String,
    /// The same with compiled calls.
    pub(crate) compiled: String,
    /// What resolving and type-checking each expansion reported.
    pub(crate) interpreted_diagnostics: Vec<String>,
    pub(crate) compiled_diagnostics: Vec<String>,
    /// Each call compiled code made: the macro and how long the call took,
    /// wire encoding included.
    pub(crate) compiled_calls: Vec<(String, Duration)>,
    /// How long the entry function of each of those calls ran, without the
    /// encoding, the arguments and the result.
    pub(crate) entry_calls: Vec<(String, Duration)>,
    /// Each call of a macro with a body the interpreter made, and how long it
    /// took.
    pub(crate) interpreted_calls: Vec<(String, Duration)>,
    /// Calls of the compiled package's macros that compiled code could not
    /// make, because the macro has no entry function.
    pub(crate) left_to_the_interpreter: Vec<String>,
}

impl Comparison {
    /// The names of the macros compiled code was called for, each once.
    pub(crate) fn compiled_macros(&self) -> Vec<&str> {
        let mut names = Vec::new();
        for (name, _) in &self.compiled_calls {
            if !names.contains(&name.as_str()) {
                names.push(name.as_str());
            }
        }
        names
    }

    /// Mean call time per macro: interpreted, compiled, and the compiled
    /// entry function alone.
    pub(crate) fn call_costs(&self) -> Vec<(String, Duration, Duration, Duration, usize)> {
        let mean = |calls: &[(String, Duration)], name: &str| {
            let times = calls
                .iter()
                .filter(|(called, _)| called == name)
                .map(|(_, time)| *time)
                .collect::<Vec<_>>();
            let count = times.len().max(1) as u32;
            (times.iter().sum::<Duration>() / count, times.len())
        };
        self.compiled_macros()
            .into_iter()
            .map(|name| {
                let (compiled, count) = mean(&self.compiled_calls, name);
                let (interpreted, _) = mean(&self.interpreted_calls, name);
                let (entry, _) = mean(&self.entry_calls, name);
                (name.to_owned(), interpreted, compiled, entry, count)
            })
            .collect()
    }
}

/// Expands `source` both ways, with `compiled`'s macros running compiled in
/// the second expansion.
pub(crate) fn expand_both_ways(source: &str, compiled: Compiled) -> Comparison {
    let source = source.to_owned();
    // Expansion is deeply recursive, and the compiled code, the arena and the
    // call context all live on the thread that expands.
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || compare_on_this_thread(&source, compiled))
        .expect("the comparison thread spawns")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

fn compare_on_this_thread(source: &str, compiled: Compiled) -> Comparison {
    let root = workspace_root();
    let program = ProgramLoader::new()
        .with_standard_library_root(standard_library_root())
        .load_source(source, &root)
        .expect("the source loads");
    let package = match compiled {
        Compiled::Library => program.package_of(
            program
                .standard_library_core()
                .expect("the library is loaded"),
        ),
        Compiled::Own => program.package_of(program.entry()),
    };

    let ((interpreted, analysis), recorded) = record_calls(|| {
        let (expanded, analysis, diagnostics) = crate::macro_expand::expand_program_checked(
            program.clone(),
            false,
            Some(CheckMode::All),
            false,
        )
        .unwrap_or_else(|diagnostics| {
            panic!("the interpreter expands the source: {diagnostics:?}")
        });
        assert!(
            diagnostics.is_empty(),
            "the interpreter expands the source: {diagnostics:?}"
        );
        (expanded, analysis)
    });
    let interpreted_calls = recorded
        .iter()
        .filter_map(|call| Some((call.declaration.name.clone(), call.elapsed?)))
        .collect::<Vec<_>>();
    let snapshot = analysis.check.as_ref().expect("the check snapshot is kept");
    let mut next = analysis.next_syntax_id;
    let run = check_programs(&interpreted, snapshot, &mut next)
        .into_iter()
        .find(|(checked, ..)| *checked == package)
        .map(|(package, _, program, templates)| CheckRun {
            package,
            program,
            templates,
            expanded: interpreted.clone(),
        })
        .expect("the compiled package has macros");
    let interpreted_diagnostics = resolve_and_check(&interpreted, analysis.next_syntax_id);
    let interpreted = tree_dump(&interpreted);

    let lowered =
        lower_run(&run).unwrap_or_else(|diagnostics| panic!("the macros lower: {diagnostics:?}"));
    install_templates(&run);
    let entries = run
        .program
        .modules()
        .iter()
        .flat_map(|module| module.syntax.items.iter())
        .filter_map(|item| match item {
            Item::Binding(binding) if binding.name.starts_with("entry#") => {
                Some(binding.name.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    with_jit(&lowered, |engine| {
        let addresses = entries
            .into_iter()
            .filter_map(|name| {
                let symbol = crate::macro_runtime::harness::instance_symbol(
                    &lowered,
                    &crate::resolve::mangle_function_name(&name),
                )?;
                let address = engine.get_function_address(&symbol).ok()?;
                Some((name, address))
            })
            .collect::<HashMap<_, _>>();
        let made = Arc::new(Mutex::new(Vec::new()));
        let entry_times = Arc::new(Mutex::new(Vec::new()));
        let left = Arc::new(Mutex::new(Vec::new()));
        struct HarnessBackend {
            modules: Vec<crate::ModuleId>,
            addresses: HashMap<String, usize>,
            made: Arc<Mutex<Vec<(String, Duration)>>>,
            entry_times: Arc<Mutex<Vec<(String, Duration)>>>,
            left: Arc<Mutex<Vec<String>>>,
        }
        impl MacroBackend for HarnessBackend {
            fn prepare(
                &mut self,
                _request: ClosureLibraryRequest<'_>,
            ) -> Result<Vec<staple_syntax::SyntaxId>, String> {
                Ok(Vec::new())
            }
            fn call(&mut self, call: &CompiledCall<'_>) -> Option<Result<(staple_syntax::Syntax, u64), String>> {
                if !self.modules.contains(&call.definition_module) {
                    return None;
                }
                let name = entry_function_name(call.declaration);
                let Some(address) = self.addresses.get(&name).copied() else {
                    self.left.lock().unwrap().push(call.declaration.name.clone());
                    return None;
                };
                let started = Instant::now();
                let (outcome, entry) = make_call(address, name, call);
                self.made
                    .lock()
                    .unwrap()
                    .push((call.declaration.name.clone(), started.elapsed()));
                self.entry_times
                    .lock()
                    .unwrap()
                    .push((call.declaration.name.clone(), entry));
                Some(outcome)
            }
        }
        let backend = Box::new(HarnessBackend {
            modules: (0..program.modules().len())
                .map(crate::ModuleId)
                .filter(|module| program.package_of(*module) == package)
                .collect(),
            addresses,
            made: made.clone(),
            entry_times: entry_times.clone(),
            left: left.clone(),
        });
        let (expanded, analysis, diagnostics) = with_macro_backend(backend, false, || {
            crate::macro_expand::expand_program_checked(program.clone(), false, None, false)
        })
        .unwrap_or_else(|diagnostics| panic!("compiled calls expand the source: {diagnostics:?}"));
        let compiled_diagnostics = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .chain(resolve_and_check(&expanded, analysis.next_syntax_id))
            .collect();
        Comparison {
            interpreted,
            compiled: tree_dump(&expanded),
            interpreted_diagnostics,
            compiled_diagnostics,
            compiled_calls: std::mem::take(&mut *made.lock().unwrap()),
            entry_calls: std::mem::take(&mut *entry_times.lock().unwrap()),
            interpreted_calls,
            left_to_the_interpreter: std::mem::take(&mut *left.lock().unwrap()),
        }
    })
}

/// Makes one call through the wire encoding, as a separate process would,
/// with how long the entry function ran.
fn make_call(
    address: usize,
    entry: String,
    call: &CompiledCall<'_>,
) -> (Result<(staple_syntax::Syntax, u64), String>, Duration) {
    let mut ran = Duration::ZERO;
    let outcome = make_timed_call(address, entry, call, &mut ran);
    (outcome, ran)
}

fn make_timed_call(
    address: usize,
    entry: String,
    call: &CompiledCall<'_>,
    ran: &mut Duration,
) -> Result<(staple_syntax::Syntax, u64), String> {
    // A function takes its closure environment first, which a top-level
    // function doesn't use.
    type Entry = unsafe extern "C" fn(*const usize, u64) -> u64;
    let request = encode(&Message::Call {
        library: 0,
        entry,
        context: CallContext {
            definition_module: call.definition_module,
            next_mark: call.next_mark,
            call_site: call.call_site.clone(),
            identifier_mark: None,
        },
        arguments: call.arguments.clone(),
    });
    let Message::Call {
        context, arguments, ..
    } = decode(&request).map_err(|error| error.to_string())?.0
    else {
        unreachable!("a call decodes as a call");
    };
    begin_call(context);
    let handle = crate::macro_runtime::args::insert_arguments(&arguments);
    let function: Entry = unsafe { std::mem::transmute::<usize, Entry>(address) };
    let started = Instant::now();
    let result = unsafe { function(std::ptr::null(), handle) };
    *ran = started.elapsed();
    let syntax =
        crate::macro_runtime::arena::with_arena(|arena| arena.syntax(result.into()).cloned())
            .ok_or("the entry function returned no syntax")?;
    crate::macro_runtime::arena::reset();
    let reply = encode(&Message::Result {
        syntax,
        next_mark: next_mark(),
        output: Vec::new(),
    });
    match decode(&reply).map_err(|error| error.to_string())?.0 {
        Message::Result {
            syntax, next_mark, ..
        } => Ok((syntax, next_mark)),
        _ => unreachable!("a result decodes as a result"),
    }
}

fn resolve_and_check(program: &Program, next_syntax_id: usize) -> Vec<String> {
    let (resolved, diagnostics) =
        NameResolver::pre_expanded(next_syntax_id).resolve_program_recovering(program.clone());
    if !diagnostics.is_empty() {
        return diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect();
    }
    let Some(resolved) = resolved else {
        return vec!["the expansion could not be resolved".to_owned()];
    };
    match TypeChecker::new().check(resolved) {
        Ok(_) => Vec::new(),
        Err(diagnostics) => diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect(),
    }
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
pub(crate) fn tree_dump(program: &Program) -> String {
    let dump = format!("{:#?}", program.module(program.entry()).syntax.items);
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
        let body = &rest[body_start..end];
        let flag = |name: &str| {
            body.split_once(&format!("{name}: "))
                .and_then(|(_, value)| value.split([',', '\n']).next())
                .unwrap_or("?")
                .trim()
                .to_owned()
        };
        output.push_str(&format!("Syntax {{ pipe_stage: {} }}", flag("pipe_stage")));
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
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
fn renumber_hygiene(text: &str) -> String {
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
    use super::*;

    fn report(name: &str, comparison: &Comparison) {
        for (macro_name, interpreted, compiled, entry, count) in comparison.call_costs() {
            eprintln!(
                "{name}: `{macro_name}` × {count}: interpreted {interpreted:?}, compiled {compiled:?} \
                 (entry function {entry:?}) per call"
            );
        }
    }

    fn assert_same(name: &str, comparison: &Comparison) {
        assert!(
            comparison.interpreted_diagnostics.is_empty(),
            "{name}: the interpreter's expansion checks: {:?}",
            comparison.interpreted_diagnostics
        );
        assert!(
            comparison.compiled_diagnostics.is_empty(),
            "{name}: the compiled expansion checks: {:?}",
            comparison.compiled_diagnostics
        );
        if comparison.interpreted != comparison.compiled {
            let first = comparison
                .interpreted
                .lines()
                .zip(comparison.compiled.lines())
                .position(|(left, right)| left != right)
                .unwrap_or(0);
            let window = |text: &str| {
                text.lines()
                    .skip(first.saturating_sub(15))
                    .take(40)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            panic!(
                "{name}: the expanded trees differ from line {}\n--- interpreted\n{}\n--- compiled\n{}",
                first + 1,
                window(&comparison.interpreted),
                window(&comparison.compiled)
            );
        }
    }

    const LIBRARY_MACROS: &str = r#"use std.cmp.(derive_eq)
use std.hash.(derive_hash)
use std.fmt.(derive_debug)
use std.default.(derive_default)

@derive_eq
@derive_hash
@derive_debug
@derive_default
type Point wrap (x: I32, y: I32)

typegroup Shape {
    Circle wrap pub I32,
    Square wrap pub I32,
    Empty,
}

def classify = (value: I32) => I32 :: {
    if (value < 0) { 0 - 1 } else if (value == 0) { 0 } else { 1 }
}

def describe = (value: I32) => I32 :: when {
    value < 10 => 1,
    value < 100 => 2,
    else => 3,
}

def total = (limit: I32) => I32 :: {
    let mut sum = 0
    let mut index = 0
    while (index < limit) {
        sum = sum + index
        index = index + 1
    }
    let mut items: List<I32> = List.new ()
    List.push items 3
    for item in (Clone.clone items) {
        sum = sum + item
    }
    discard (classify sum)
    sum
}

def main = () => total 4
"#;

    #[test]
    fn library_macros_expand_the_same_compiled() {
        let comparison = expand_both_ways(LIBRARY_MACROS, Compiled::Library);
        report("library", &comparison);
        eprintln!(
            "left to the interpreter: {:?}",
            comparison.left_to_the_interpreter
        );
        let made = comparison.compiled_macros();
        for expected in [
            "if",
            "when",
            "while",
            "for",
            "discard",
            "derive_eq",
            "derive_hash",
            "derive_debug",
            "derive_default",
            "typegroup",
        ] {
            assert!(
                made.contains(&expected),
                "`{expected}` ran compiled: {made:?}"
            );
        }
        assert_same("library", &comparison);
    }

    /// The macros of every shape the type checker is tested with, and a call
    /// of each. The interpreter can't evaluate the type checker's `keep`,
    /// which clones its argument, so this one moves it. It can't splice a
    /// sequence in expression position either, so `apply` is left out.
    const OWN_MACROS: &str = r#"use std.syntax.*
use std.clone.Clone

def keep = move value: Expr => value
macro one: Expr -> Item = value => parse_quote { let one = $value }
macro two: (Expr; Expr) -> Item = (a; b) => match a { _ => parse_quote { let two = 2 } }
macro pick = move value: Expr => keep value
macro pick = (move a: Expr; b: Expr) => keep a
macro inspect modifiers (@traced): (MacroCallMetadata; Expr) -> Item = ((modifiers, _); value) => match modifiers {
    Sequence () => parse_quote { let plain = $value },
    _ => parse_quote { let traced = $value },
}
macro @mark: move Item -> Sequence<Item> = move item => Sequence (first: item, rest: Sequence ())
macro words: (Expr; Sequence<Expr>) -> Expr = (first; rest) => match first {
    else_word: Ident<"else"> => {
        let elements = Clone.clone rest
        let value = Parenthesized (
            Separated (separator: Comma, elements: elements, trailing: False),
        )
        parse_quote { $value }
    },
    other => {
        let elements = Sequence (first: Clone.clone other, rest: Clone.clone rest)
        let value = Parenthesized (
            Separated (separator: Comma, elements: elements, trailing: False),
        )
        parse_quote { $value }
    },
}

one 1
two 2 3
inspect 4
@traced
inspect 5

@mark
def marked = () => 6

let picked: I32 = pick 7
let picked_first: I32 = pick 8 9
let listed: (I32, I32) = words else 12 13
let all_listed: (I32, I32, I32) = words 14 15 16

def main = () => plain + traced + marked () + picked + picked_first
"#;

    #[test]
    fn own_macros_expand_the_same_compiled() {
        let comparison = expand_both_ways(OWN_MACROS, Compiled::Own);
        report("own", &comparison);
        let made = comparison.compiled_macros();
        for expected in ["one", "two", "pick", "inspect", "mark", "words"] {
            assert!(
                made.contains(&expected),
                "`{expected}` ran compiled: {made:?}"
            );
        }
        assert!(
            comparison.left_to_the_interpreter.is_empty(),
            "{:?}",
            comparison.left_to_the_interpreter
        );
        assert_same("own", &comparison);
    }

    #[test]
    fn the_macro_example_expands_the_same_compiled() {
        let source =
            std::fs::read_to_string(workspace_root().join("staple-compiler/examples/macros.sta"))
                .expect("the macro example reads");
        let comparison = expand_both_ways(&source, Compiled::Own);
        report("example", &comparison);
        let made = comparison.compiled_macros();
        for expected in [
            "choose",
            "describe",
            "bind",
            "destructure",
            "replace_argument",
            "call_increment",
            "generate_pair",
            "identity",
            "replace",
            "duplicate",
            "define_alias",
            "hygienic_identity",
            "setting_value",
        ] {
            assert!(
                made.contains(&expected),
                "`{expected}` ran compiled: {made:?}"
            );
        }
        assert!(
            comparison.left_to_the_interpreter.is_empty(),
            "{:?}",
            comparison.left_to_the_interpreter
        );
        assert_same("example", &comparison);
    }
}
