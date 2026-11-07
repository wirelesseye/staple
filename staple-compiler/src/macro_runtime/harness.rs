//! Test-only support for running macro code in-process.
//!
//! A check program is the same program the closure check resolves and type
//! checks. Here it is lowered and emitted, then executed by LLVM's JIT with
//! the syntax runtime's symbols mapped to their host functions. A check program
//! holds the whole standard library, as a real build does, and costs seconds to
//! compile, so a test puts its probes in one program and runs them together.

use std::path::{Path, PathBuf};

use staple_project::PackageId;
use staple_syntax::{Diagnostic, Item};

use crate::macro_expand::check_program::{
    CheckMode, build_check_program_with, check_programs,
};
use crate::macro_expand::lower_quote::TemplateTable;
use crate::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, Program, ProgramLoader, TypeChecker,
    TypedModule,
};

pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the compiler crate is in the workspace")
        .to_path_buf()
}

pub(crate) fn standard_library_root() -> PathBuf {
    workspace_root().join("stdlib")
}

/// One package's check program, built from the closures of the macros that
/// were invoked (or of every macro).
pub(crate) struct CheckRun {
    pub(crate) package: Option<PackageId>,
    pub(crate) program: Program,
    pub(crate) templates: TemplateTable,
}

/// Expands `source` with the closure check on, and builds each package's
/// check program.
pub(crate) fn check_runs(source: &str, mode: CheckMode) -> Vec<CheckRun> {
    check_runs_with(source, mode, true)
}

/// Like [`check_runs`], but a dependency package contributes only the closure
/// items a checked macro reaches. That is enough for an executable test and
/// keeps unrelated library code out of the JIT.
pub(crate) fn check_runs_with(
    source: &str,
    mode: CheckMode,
    whole_dependencies: bool,
) -> Vec<CheckRun> {
    check_runs_from(source, mode, whole_dependencies, &standard_library_root())
}

fn check_runs_from(
    source: &str,
    mode: CheckMode,
    whole_dependencies: bool,
    library: &std::path::Path,
) -> Vec<CheckRun> {
    let root = workspace_root();
    let program = ProgramLoader::new()
        .with_standard_library_root(library.to_path_buf())
        .load_source(source, &root)
        .expect("the source should load");
    let (program, analysis, _) = crate::macro_expand::expand_program_checked(
        program, true, Some(mode), false,
    )
    .expect("the source should expand");
    let snapshot = analysis.check.expect("the check snapshot is kept");
    let mut next = analysis.next_syntax_id;
    if whole_dependencies {
        return check_programs(&program, &snapshot, &mut next)
            .into_iter()
            .map(|(package, _macros, program, templates)| CheckRun {
                package,
                program,
                templates,
            })
            .collect();
    }
    let mut packages = Vec::new();
    for checked in &snapshot.macros {
        let package = program.package_of(checked.module);
        if !packages.contains(&package) {
            packages.push(package);
        }
    }
    packages
        .into_iter()
        .map(|package| {
            let checked = snapshot
                .macros
                .iter()
                .filter(|checked| program.package_of(checked.module) == package)
                .collect::<Vec<_>>();
            let (program, templates) = build_check_program_with(
                &program,
                &snapshot,
                package,
                &checked,
                &mut next,
                false,
            );
            CheckRun {
                package,
                program,
                templates,
            }
        })
        .collect()
}

/// The check program of the package `source` itself defines, with every macro
/// checked. Falls back to the first program when no package owns the source.
pub(crate) fn check_run_for(source: &str) -> CheckRun {
    check_run_for_with(source, true)
}

/// Like [`check_run_for`], choosing whether dependencies are whole programs
/// or only the closure items reached.
pub(crate) fn check_run_for_with(source: &str, whole_dependencies: bool) -> CheckRun {
    let mode = if whole_dependencies {
        CheckMode::All
    } else {
        CheckMode::Invoked
    };
    let mut runs = check_runs_with(source, mode, whole_dependencies);
    assert!(!runs.is_empty(), "the source should produce a check program");
    if runs.len() == 1 {
        return runs.remove(0);
    }
    // The source's own package has the closure items; dependency packages can
    // have no entry items at all.
    let own = runs
        .iter()
        .rposition(|run| !entry_items(run).is_empty())
        .unwrap_or(runs.len() - 1);
    runs.remove(own)
}

/// Resolves and type-checks a check program.
pub(crate) fn check(run: &CheckRun) -> Result<TypedModule, Vec<Diagnostic>> {
    let next_syntax_id = run.program.next_syntax_id();
    let (resolved, diagnostics) =
        NameResolver::pre_expanded(next_syntax_id).resolve_program_recovering(run.program.clone());
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let resolved = resolved.ok_or_else(|| {
        vec![Diagnostic::new(
            staple_syntax::Span::Compiler,
            "the check program could not be resolved",
        )]
    })?;
    TypeChecker::new().check(resolved)
}

/// Lowers a checked check program.
pub(crate) fn lower(module: &TypedModule) -> Result<LoweredModule, Vec<Diagnostic>> {
    Lowerer::new().lower(module)
}

/// Resolves, checks and lowers a check program for execution. Dependency
/// packages are linked from their prebuilt objects, so the JIT only emits the
/// package under test and the generic templates it instantiates.
pub(crate) fn lower_run(run: &CheckRun) -> Result<LoweredModule, Vec<Diagnostic>> {
    lower(&check(run)?)
}

/// Finds the concrete instance symbol a source function's name lowered to.
pub(crate) fn instance_symbol(lowered: &LoweredModule, name: &str) -> Option<String> {
    lowered.planned_instance_names(name).into_iter().next()
}

/// [`check_run_for`] against a standard library at `library`, for a test that
/// needs internals macro code can't reach.
#[cfg(test)]
pub(crate) fn check_run_with_library(source: &str, library: &std::path::Path) -> CheckRun {
    let mut runs = check_runs_from(source, CheckMode::All, true, library);
    assert!(!runs.is_empty(), "the source should produce a check program");
    let own = runs
        .iter()
        .rposition(|run| !entry_items(run).is_empty())
        .unwrap_or(runs.len() - 1);
    runs.remove(own)
}

/// Runs `f` with a JIT engine holding `lowered`.
///
/// The syntax runtime's symbols are mapped from its table, so compiled macro
/// code calls the host's implementations. A test that provides its own extern
/// function maps it inside `f`.
pub(crate) fn with_jit<T>(lowered: &LoweredModule, f: impl FnOnce(&inkwell::execution_engine::ExecutionEngine<'_>) -> T) -> T {
    with_jit_threshold(lowered, None, f)
}

/// Like [`with_jit`], with the allocated bytes that start the first collection.
pub(crate) fn with_jit_threshold<T>(
    lowered: &LoweredModule,
    collection_threshold: Option<u64>,
    f: impl FnOnce(&inkwell::execution_engine::ExecutionEngine<'_>) -> T,
) -> T {
    let context = inkwell::context::Context::create();
    let text = CodeGenerator::new(&context)
        .compile_module(lowered)
        .expect("the check program should emit");
    let buffer = inkwell::memory_buffer::MemoryBuffer::create_from_memory_range_copy(
        text.as_bytes(),
        "macro-check",
    );
    let llvm = context.create_module_from_ir(buffer).expect("the IR should parse");
    llvm.verify().expect("the IR should verify");
    if let Some(threshold) = collection_threshold {
        llvm.get_global("__staple_gc_threshold")
            .expect("the collector is linked")
            .set_initializer(&context.i64_type().const_int(threshold, false));
    }
    let engine = llvm
        .create_jit_execution_engine(inkwell::OptimizationLevel::None)
        .expect("the JIT should start");
    for intrinsic in crate::macro_runtime::intrinsics::INTRINSICS {
        if let Some(function) = llvm.get_function(intrinsic.symbol) {
            engine.add_global_mapping(&function, intrinsic.address());
        }
    }
    // `main` sets the collector's stack bottom, registers the global roots and
    // runs every module initializer, so plain functions and their globals are
    // ready. Its frame is gone once it returns, so the bottom then moves to this
    // frame, which outlives every call `f` makes.
    type Main = unsafe extern "C" fn() -> i32;
    let main = unsafe { engine.get_function::<Main>("main") }.expect("a whole program has `main`");
    assert_eq!(unsafe { main.call() }, 0, "module initialization should succeed");
    type SetStackBottom = unsafe extern "C" fn(*mut u8);
    let set_stack_bottom = unsafe { engine.get_function::<SetStackBottom>("__staple_gc_set_stack_bottom") }
        .expect("the collector is linked");
    let mut marker = 0u8;
    unsafe { set_stack_bottom.call(&mut marker) };
    f(&engine)
}

/// Items of the check program's entry module.
pub(crate) fn entry_items(run: &CheckRun) -> &[Item] {
    &run.program.module(run.program.entry()).syntax.items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macro_expand::check_program::CheckMode;


    /// The checked standard-library program lowers. A program with no entry
    /// point leaves closure items unreachable, which is not a failure.
    #[test]
    fn the_standard_library_check_program_lowers() {
        let runs = check_runs("def main = () => 0\n", CheckMode::All);
        assert!(!runs.is_empty(), "the standard library has macros");
        for run in &runs {
            let module = check(run).expect("the check program should check");
            match lower(&module) {
                Ok(_) => {}
                Err(diagnostics) => {
                    let real = diagnostics
                        .iter()
                        .map(ToString::to_string)
                        .filter(|message| !message.contains("is not reachable from any runtime root"))
                        .collect::<Vec<_>>();
                    assert!(real.is_empty(), "{real:#?}");
                }
            }
        }
    }




    /// Compiles `source` once and runs each named zero-argument `I32` probe.
    fn run_probes(source: &str, probes: &[(&str, i32)]) {
        let source = format!("use std.syntax.*\n{source}\nmacro touch = () => {}\n",
            probes.iter().map(|(name, _)| format!("{name} ()")).collect::<Vec<_>>().join(" + "));
        let run = check_run_for(&source);
        let lowered = lower_run(&run).expect("the probes should lower");
        with_jit(&lowered, |engine| {
            type Probe = unsafe extern "C" fn(*const usize) -> i32;
            for (name, expected) in probes {
                let symbol = instance_symbol(&lowered, name)
                    .unwrap_or_else(|| panic!("`{name}` is instantiated"));
                let probe = unsafe { engine.get_function::<Probe>(&symbol).unwrap() };
                eprintln!("running {name}");
                assert_eq!(unsafe { probe.call(std::ptr::null()) }, *expected, "{name}");
            }
        });
    }

    #[test]
    fn refinement_patterns_compare_the_spelling() {
        run_probes(
            r#"
def spelling: (Expr) -> I32 = (value) => match value {
    word: Ident<"else"> => 1,
    _: Ident<"then"> => 2,
    both: Ident<"one" | "two"> => 5,
    other: Ident<String> => 3,
    _ => 4,
}
def matches_refined: () -> I32 = () => spelling (Ident "else")
def matches_wildcard: () -> I32 = () => spelling (Ident "then")
def falls_through: () -> I32 = () => spelling (Ident "other")
def first_of_set: () -> I32 = () => spelling (Ident "one")
def second_of_set: () -> I32 = () => spelling (Ident "two")
def needs_equal_length: () -> I32 = () => spelling (Ident "els")
def nested: (CallExpr) -> I32 = (call) => match call {
    CallExpr (word: Ident<"size">, _) => 1,
    _ => 2,
}
def nested_match: () -> I32 = () => nested (CallExpr (callee: Ident "size", argument: Ident "x"))
def nested_miss: () -> I32 = () => nested (CallExpr (callee: Ident "grow", argument: Ident "x"))
def other_alternative: () -> I32 = () => spelling (CallExpr (callee: Ident "else", argument: Ident "x"))
"#,
            &[
                ("matches_refined", 1),
                ("matches_wildcard", 2),
                ("falls_through", 3),
                ("first_of_set", 5),
                ("second_of_set", 5),
                ("needs_equal_length", 3),
                ("other_alternative", 4),
                ("nested_match", 1),
                ("nested_miss", 2),
            ],
        );
    }

    #[test]
    fn plain_functions_run_after_initialization() {
        run_probes(
            r#"
def add2: (I32, I32) -> I32 = (a, b) => a + b
def count: (I32, I32) -> I32 = (n, total) => match (n == 0) {
    True => total,
    False => count (n - 1, total + 1),
}
let answer: I32 = add2 (40, 2)
def calls_plain: () -> I32 = () => add2 (1, 2)
def recurses: () -> I32 = () => count (3, 0)
def reads_global: () -> I32 = () => answer
"#,
            &[("calls_plain", 3), ("recurses", 3), ("reads_global", 42)],
        );
    }

    #[test]
    fn nodes_are_built_matched_and_survive_collection() {
        run_probes(
            r#"
def grow: (I32, move Sequence<Ident<String>>) -> Sequence<Ident<String>> = (count, move list) => match (count == 0) {
    True => list,
    False => grow (count - 1, Sequence (first: Ident "a", rest: list)),
}
def size: (move Sequence<Ident<String>>, I32) -> I32 = (move list, total) => match list {
    Sequence () => total,
    Sequence (_, rest) => size (rest, total + 1),
}
def sequence_alternatives: () -> I32 = () => {
    let empty: Sequence<Ident<String>> = Sequence ()
    let from_empty = match empty { Sequence () => 0, Sequence (_, _) => 10 }
    let tail: Sequence<Ident<String>> = Sequence ()
    let one: Sequence<Ident<String>> = Sequence (first: Ident "a", rest: tail)
    let from_one = match one {
        Sequence () => 20,
        Sequence (first, rest) => match rest {
            Sequence () => match first { Ident name => 1 },
            _ => 40,
        },
    }
    from_empty + from_one
}
def call_fields: () -> I32 = () => {
    let call: CallExpr = CallExpr (callee: Ident "f", argument: Ident "a")
    let callee = match call { CallExpr (callee, _) => callee }
    let changed: CallExpr = CallExpr (callee: callee, argument: Ident "b")
    match changed { CallExpr (_, argument) => match argument { Ident _ => 1, _ => 60 } }
}
def long_list: () -> I32 = () => {
    let empty: Sequence<Ident<String>> = Sequence ()
    size (grow (2000, empty), 0)
}
"#,
            &[("sequence_alternatives", 1), ("call_fields", 1), ("long_list", 2000)],
        );
    }

    #[test]
    fn field_assignment_writes_into_the_box() {
        run_probes(
            r#"
def is_named: (Expr) -> I32 = (value) => match value {
    word: Ident<"new"> => 1,
    word: Ident<"old"> => 2,
    _ => 3,
}
def assign_field: () -> I32 = () => {
    let mut call: CallExpr = CallExpr (callee: Ident "old", argument: Ident "a")
    call.callee = Ident "new"
    is_named (call.callee)
}
def later_read_sees_the_write: () -> I32 = () => {
    let mut call: CallExpr = CallExpr (callee: Ident "old", argument: Ident "a")
    let before = is_named (call.callee)
    call.callee = Ident "new"
    before * 10 + is_named (call.callee)
}
def other_field_kept: () -> I32 = () => {
    let mut call: CallExpr = CallExpr (callee: Ident "new", argument: Ident "old")
    call.callee = Ident "old"
    is_named (call.argument) * 10 + is_named (call.callee)
}
def representation_read: () -> I32 = () => {
    let name: Ident<String> = Ident "old"
    is_named (Ident (name.*))
}
def nested_field: () -> I32 = () => {
    let inner: NominalPattern = NominalPattern (name: Ident "old", argument: BindingPattern (Ident "a"))
    let mut body: WrapperBody<I32, NominalPattern> = WrapperBody (3, inner)
    body.1.name = Ident "new"
    is_named (body.1.name) * 10 + body.0
}
def node_read_from_mutable_binding: () -> I32 = () => {
    let mut pattern: NominalPattern = NominalPattern (name: Ident "old", argument: BindingPattern (Ident "a"))
    is_named (pattern.name)
}
def pair_or_text: ((I32, I32) | String) -> I32 = (value) => match value {
    pair: (I32, I32) => pair.0 + pair.1,
    _ => 0,
}
def product_read_from_mutable_binding: () -> I32 = () => {
    let mut outer = (inner: (3, 4), other: 5)
    outer.other = 6
    pair_or_text (outer.inner)
}
def write_through: mut CallExpr -> () = mut call => {
    call.callee = Ident "new"
}
def through_mut_parameter: () -> I32 = () => {
    let mut call: CallExpr = CallExpr (callee: Ident "old", argument: Ident "a")
    write_through call
    is_named (call.callee)
}
"#,
            &[
                ("assign_field", 1),
                ("later_read_sees_the_write", 21),
                ("other_field_kept", 22),
                ("representation_read", 2),
                ("nested_field", 13),
                ("node_read_from_mutable_binding", 2),
                ("product_read_from_mutable_binding", 7),
                ("through_mut_parameter", 1),
            ],
        );
    }

    #[test]
    fn generic_constructors_build_tagged_nodes() {
        run_probes(
            r#"
def wrap_one: <T> move T -> Sequence<T> = <T> move item => {
    let empty: Sequence<T> = Sequence ()
    Sequence (first: item, rest: empty)
}
def length: <T> (move Sequence<T>, I32) -> I32 = <T> (move list, total) => match list {
    Sequence () => total,
    Sequence (_, rest) => length (rest, total + 1),
}
def one_element: () -> I32 = () => length (wrap_one (Ident "a"), 0)
"#,
            &[("one_element", 1)],
        );
    }

    /// Reads the hidden origin and sets it, which macro code cannot do itself.
    /// Tests add them to a copy of `std.syntax`.
    const ORIGIN_ACCESS: &str = r#"
pub def call_origin: CallExpr -> Syntax = value => __origin value
pub def with_call_origin: (CallExpr, Syntax) -> CallExpr = (value, origin) => __with_origin (value, origin)
pub def sequence_origin: Sequence<CallExpr> -> Syntax = value => __origin value
pub def with_sequence_origin: (Sequence<CallExpr>, Syntax) -> Sequence<CallExpr> = (value, origin) => __with_origin (value, origin)
pub def body_origin: WrapperBody<I32, CallExpr> -> Syntax = value => __origin value
pub def with_body_origin: (WrapperBody<I32, CallExpr>, Syntax) -> WrapperBody<I32, CallExpr> = (value, origin) => __with_origin (value, origin)
"#;

    /// A copy of the standard library with [`ORIGIN_ACCESS`] in `std.syntax`.
    fn library_with_origin_access() -> std::path::PathBuf {
        fn copy(from: &std::path::Path, to: &std::path::Path) {
            std::fs::create_dir_all(to).unwrap();
            for entry in std::fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let target = to.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy(&entry.path(), &target);
                } else {
                    std::fs::copy(entry.path(), target).unwrap();
                }
            }
        }
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let target = std::env::temp_dir().join(format!(
            "staple-harness-library-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let _ = std::fs::remove_dir_all(&target);
        copy(&standard_library_root(), &target);
        let syntax = target.join("std/syntax.sta");
        let mut text = std::fs::read_to_string(&syntax).unwrap();
        text.push_str(ORIGIN_ACCESS);
        std::fs::write(&syntax, text).unwrap();
        target
    }

    const NODE_PROBES: &str = r#"
def is_named: (Expr) -> I32 = (value) => match value {
    word: Ident<"new"> => 1,
    word: Ident<"old"> => 2,
    _ => 3,
}
def make_call: () -> CallExpr = () => CallExpr (callee: Ident "old", argument: Ident "old")
def clone_call: CallExpr -> CallExpr = value => Clone.clone value
def callee_is: CallExpr -> I32 = value => is_named (value.callee)
def set_callee: mut CallExpr -> () = mut value => {
    value.callee = Ident "new"
}
def make_body: () -> WrapperBody<I32, CallExpr> = () => WrapperBody (3, make_call ())
def clone_body: WrapperBody<I32, CallExpr> -> WrapperBody<I32, CallExpr> = value => Clone.clone value
def body_call: WrapperBody<I32, CallExpr> -> CallExpr = value => Clone.clone (value.1)
def body_callee_is: WrapperBody<I32, CallExpr> -> I32 = value => is_named (value.1.callee)
def set_body_callee: mut WrapperBody<I32, CallExpr> -> () = mut value => {
    value.1.callee = Ident "new"
}
def make_sequence: () -> Sequence<CallExpr> = () => {
    let empty: Sequence<CallExpr> = Sequence ()
    Sequence (first: make_call (), rest: Sequence (first: make_call (), rest: empty))
}
def clone_sequence: Sequence<CallExpr> -> Sequence<CallExpr> = value => Clone.clone value
def sequence_rest: Sequence<CallExpr> -> Sequence<CallExpr> = value => match value {
    Sequence (_, rest) => Clone.clone rest,
    _ => Clone.clone value,
}
def sequence_callees: (Sequence<CallExpr>, I32) -> I32 = (value, total) => match value {
    Sequence (first, rest) => sequence_callees (rest, total * 10 + is_named (first.callee)),
    _ => total,
}
def sequence_callees_of: Sequence<CallExpr> -> I32 = value => sequence_callees (value, 0)
def set_first_callee: move Sequence<CallExpr> -> Sequence<CallExpr> = move value => match value {
    Sequence (first, rest) => {
        let mut changed = first
        changed.callee = Ident "new"
        Sequence (first: changed, rest: rest)
    },
    Sequence () => Sequence (),
}
def make_long: I32 -> Sequence<Ident<String>> = count => {
    let mut list: Sequence<Ident<String>> = Sequence ()
    let mut remaining = count
    loop {
        if (remaining == 0) { break }
        list = Sequence (first: Ident "a", rest: list)
        remaining = remaining - 1
    }
    list
}
def long_size: (Sequence<Ident<String>>) -> I32 = (value) => {
    let mut cursor = Clone.clone value
    let mut total = 0
    loop {
        match cursor {
            Sequence () => { break },
            Sequence (_, rest) => {
                total = total + 1
                cursor = rest
            },
        }
    }
    total
}
def long_clone: I32 -> I32 = count => {
    let list = make_long count
    long_size (Clone.clone list)
}
def clone_through_collections: () -> I32 = () => {
    let original = make_call ()
    let mut copy = Clone.clone original
    copy.callee = Ident "new"
    let mut round = 0
    let mut allocated = 0
    loop {
        if (round == 60) { break }
        allocated = allocated + long_clone 2000
        round = round + 1
    }
    is_named (original.callee) * 10 + is_named (copy.callee)
}
def nested_sequence_clone: () -> I32 = () => {
    let mut original = make_sequence ()
    let copy = Clone.clone original
    original = set_first_callee original
    sequence_callees (original, 0) * 100 + sequence_callees (copy, 0)
}
"#;

    type Node = *const u8;
    type Handle = u64;

    /// Runs `body` against the node probes, with a way to look probes up.
    fn with_node_probes(body: impl FnOnce(&Probes<'_, '_>)) {
        with_node_probes_threshold(None, body)
    }

    fn with_node_probes_threshold(threshold: Option<u64>, body: impl FnOnce(&Probes<'_, '_>)) {
        let library = library_with_origin_access();
        let source = format!(
            "use std.syntax.*\nuse std.clone.Clone\n{NODE_PROBES}\nmacro touch = () => \
             (make_call (), clone_call (make_call ()), callee_is (make_call ()), \
             set_callee, make_body (), clone_body (make_body ()), body_call (make_body ()), \
             body_callee_is (make_body ()), set_body_callee, make_sequence (), \
             clone_sequence (make_sequence ()), sequence_rest (make_sequence ()), \
             sequence_callees_of (make_sequence ()), long_clone 1, nested_sequence_clone (), clone_through_collections (), \
             call_origin (make_call ()), sequence_origin (make_sequence ()), \
             body_origin (make_body ()), with_call_origin, with_sequence_origin, with_body_origin)\n"
        );
        let run = check_run_with_library(&source, &library);
        let lowered = lower_run(&run).expect("the node probes should lower");
        with_jit_threshold(&lowered, threshold, |engine| {
            body(&Probes {
                lowered: &lowered,
                engine,
            });
        });
        let _ = std::fs::remove_dir_all(library);
    }

    struct Probes<'a, 'context> {
        lowered: &'a LoweredModule,
        engine: &'a inkwell::execution_engine::ExecutionEngine<'context>,
    }

    impl Probes<'_, '_> {
        fn address(&self, name: &str) -> usize {
            let symbol = instance_symbol(self.lowered, name)
                .unwrap_or_else(|| panic!("`{name}` is instantiated"));
            self.engine.get_function_address(&symbol).unwrap()
        }

        fn make(&self, name: &str) -> Node {
            let code: extern "C" fn(*const usize) -> Node =
                unsafe { std::mem::transmute(self.address(name)) };
            code(std::ptr::null())
        }

        /// Calls a one-argument probe. A node is pointer-sized, so a borrowed
        /// one is passed as its box pointer.
        fn call<R>(&self, name: &str, argument: &Node) -> R {
            let code: extern "C" fn(*const usize, Node) -> R =
                unsafe { std::mem::transmute_copy(&self.address(name)) };
            code(std::ptr::null(), *argument)
        }

        fn origin(&self, name: &str, node: &Node) -> Handle {
            self.call(name, node)
        }

        /// A copy of `node` carrying `origin`.
        fn with_origin(&self, name: &str, node: &Node, origin: Handle) -> Node {
            let code: extern "C" fn(*const usize, Node, Handle) -> Node =
                unsafe { std::mem::transmute_copy(&self.address(name)) };
            code(std::ptr::null(), *node, origin)
        }
    }

    #[test]
    fn clones_have_their_own_boxes_and_the_same_origins() {
        with_node_probes(|probes| {
            // A node inside a node: the nested box must be new too.
            let original = probes.make("make_body");
            let original = probes.with_origin("with_body_origin", &original, 7);
            let copy: Node = probes.call("clone_body", &original);
            assert_ne!(original, copy, "the clone has a box of its own");
            assert_eq!(probes.origin("body_origin", &copy), 7);
            assert_eq!(probes.call::<i32>("body_callee_is", &copy), 2);

            let call = probes.make("make_call");
            let call = probes.with_origin("with_call_origin", &call, 9);
            let call_copy: Node = probes.call("clone_call", &call);
            assert_ne!(call, call_copy);
            assert_eq!(probes.origin("call_origin", &call_copy), 9);

            // Each cell of a sequence keeps its own origin.
            let sequence = probes.make("make_sequence");
            let tail: Node = probes.call("sequence_rest", &sequence);
            assert_ne!(sequence, tail);
            let sequence = probes.with_origin("with_sequence_origin", &sequence, 11);
            let copy: Node = probes.call("clone_sequence", &sequence);
            assert_ne!(sequence, copy);
            assert_eq!(probes.origin("sequence_origin", &copy), 11);
            let copy_tail: Node = probes.call("sequence_rest", &copy);
            assert_ne!(copy, copy_tail);
            assert_eq!(probes.call::<i32>("sequence_callees_of", &copy), 22);
        });
    }

    #[test]
    fn assignment_keeps_the_box_and_the_origin() {
        with_node_probes(|probes| {
            let node = probes.make("make_call");
            let mut node = probes.with_origin("with_call_origin", &node, 5);
            let before = node;
            let set: extern "C" fn(*const usize, *mut Node) =
                unsafe { std::mem::transmute(probes.address("set_callee")) };
            set(std::ptr::null(), &mut node);
            assert_eq!(node, before, "assignment writes into the same box");
            assert_eq!(probes.origin("call_origin", &node), 5);
            assert_eq!(probes.call::<i32>("callee_is", &node), 1);
        });
    }

    #[test]
    fn assignment_leaves_the_other_copy_alone() {
        with_node_probes(|probes| {
            let set_call: extern "C" fn(*const usize, *mut Node) =
                unsafe { std::mem::transmute(probes.address("set_callee")) };
            let set_body: extern "C" fn(*const usize, *mut Node) =
                unsafe { std::mem::transmute(probes.address("set_body_callee")) };

            // Assigning to the clone.
            let original = probes.make("make_call");
            let mut copy: Node = probes.call("clone_call", &original);
            set_call(std::ptr::null(), &mut copy);
            assert_eq!(probes.call::<i32>("callee_is", &original), 2);
            assert_eq!(probes.call::<i32>("callee_is", &copy), 1);

            // Assigning to the original, including a field of a nested node.
            let mut original = probes.make("make_body");
            let copy: Node = probes.call("clone_body", &original);
            set_body(std::ptr::null(), &mut original);
            assert_eq!(probes.call::<i32>("body_callee_is", &original), 1);
            assert_eq!(probes.call::<i32>("body_callee_is", &copy), 2);
        });
    }

    #[test]
    fn a_sequence_clone_is_separate() {
        with_node_probes(|probes| {
            let nested: extern "C" fn(*const usize) -> i32 =
                unsafe { std::mem::transmute(probes.address("nested_sequence_clone")) };
            assert_eq!(nested(std::ptr::null()), 12 * 100 + 22);
        });
    }

    /// Far more cells than the test thread's stack could recurse through. No
    /// collection runs, because the collector's mark phase recurses along a list.
    #[test]
    fn a_long_sequence_clones_without_deep_stack() {
        with_node_probes_threshold(Some(1 << 40), |probes| {
            let long: extern "C" fn(*const usize, i32) -> i32 =
                unsafe { std::mem::transmute(probes.address("long_clone")) };
            assert_eq!(long(std::ptr::null(), 500_000), 500_000);
        });
    }

    #[test]
    fn nodes_and_clones_survive_collections() {
        with_node_probes_threshold(Some(4096), |probes| {
            let churn: extern "C" fn(*const usize) -> i32 =
                unsafe { std::mem::transmute(probes.address("clone_through_collections")) };
            assert_eq!(churn(std::ptr::null()), 21);
        });
    }

    #[test]
    fn handle_copies_reject_structured_nodes() {
        let library = library_with_origin_access();
        let syntax = library.join("std/syntax.sta");
        let mut text = std::fs::read_to_string(&syntax).unwrap();
        text.push_str("pub def clone_call_box: CallExpr -> CallExpr = value => __clone_syntax value\n");
        std::fs::write(&syntax, text).unwrap();
        let run = check_run_with_library(
            "use std.syntax.*\nmacro touch = () => clone_call_box\n",
            &library,
        );
        let diagnostics = lower_run(&run).err().expect("aliasing a box should not lower");
        let _ = std::fs::remove_dir_all(library);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("only copies handle types")),
            "{diagnostics:?}"
        );
    }

    /// Builds the check programs of the standard library's macros and records
    /// where the first lowering step rejects them. The result is written to
    /// the plan; the test is kept as the inventory's reproduction.
    #[test]
    #[ignore = "inventory; run while working on the runtime"]
    fn standard_library_lowering_inventory() {
        let runs = check_runs("def main = () => 0\n", CheckMode::All);
        let mut lines = Vec::new();
        for run in &runs {
            let label = format!("{:?}", run.package);
            match check(run) {
                Ok(module) => match lower(&module) {
                    Ok(_) => lines.push(format!("{label}: lowered")),
                    Err(diagnostics) => {
                        for diagnostic in diagnostics {
                            let message = diagnostic.to_string();
                            // A check program has no entry point of its own, so
                            // items nothing calls are expected to be unreachable.
                            if message.contains("is not reachable from any runtime root")
                                || message.contains("has no lowered body block")
                            {
                                continue;
                            }
                            lines.push(format!("{label}: {message}"));
                        }
                    }
                },
                Err(diagnostics) => {
                    for diagnostic in diagnostics {
                        lines.push(format!("{label}: check: {diagnostic}"));
                    }
                }
            }
        }
        panic!("{}", lines.join("\n"));
    }
}
