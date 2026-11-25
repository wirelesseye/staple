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

use crate::macro_runtime::canonical::tree_dump;
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
            ) -> Result<Vec<staple_syntax::SyntaxId>, Vec<staple_syntax::Diagnostic>> {
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
