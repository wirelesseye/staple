//! Test-only support for running macro code in-process.
//!
//! A check program is the same program the closure check resolves and type
//! checks. Here it is lowered and emitted, then executed by LLVM's JIT with
//! the syntax runtime's symbols mapped to their host functions. One program
//! costs seconds to compile, so tests build the smallest program that holds
//! the macro they exercise.

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
    let root = workspace_root();
    let program = ProgramLoader::new()
        .with_standard_library_root(standard_library_root())
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

/// Runs `f` with a JIT engine holding `lowered`.
///
/// The syntax runtime's symbols are mapped from its table, so compiled macro
/// code calls the host's implementations. A test that provides its own extern
/// function maps it inside `f`.
pub(crate) fn with_jit<T>(lowered: &LoweredModule, f: impl FnOnce(&inkwell::execution_engine::ExecutionEngine) -> T) -> T {
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
    let engine = llvm
        .create_jit_execution_engine(inkwell::OptimizationLevel::None)
        .expect("the JIT should start");
    for intrinsic in crate::macro_runtime::intrinsics::INTRINSICS {
        if let Some(function) = llvm.get_function(intrinsic.symbol) {
            engine.add_global_mapping(&function, intrinsic.address());
        }
    }
    // Give the collector a stack bottom. Module initializers would also run
    // here in a whole-program executable; the harness calls only what a test
    // needs after this.
    if llvm.get_function("__staple_gc_set_stack_bottom").is_some() {
        type SetStackBottom = unsafe extern "C" fn(*mut u8);
        let set = unsafe {
            engine
                .get_function::<SetStackBottom>("__staple_gc_set_stack_bottom")
                .unwrap()
        };
        let mut marker = 0u8;
        unsafe { set.call(&mut marker) };
    }
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




    #[test]
    fn compiled_code_constructs_and_matches_a_sequence() {
        let run = check_run_for(concat!(
            "use std.syntax.*\n",
            "def probe: () -> I32 = () => {\n",
            "    let empty: Sequence<Ident<String>> = Sequence ()\n",
            "    let one: Sequence<Ident<String>> = Sequence (first: Ident \"a\", rest: empty)\n",
            "    let from_empty = match empty { Sequence () => 0, Sequence (_, _) => 10 }\n",
            "    let from_one = match one {\n",
            "        Sequence () => 20,\n",
            "        Sequence (first, rest) => match rest {\n",
            "            Sequence () => match first { Ident name => 1 },\n",
            "            _ => 40,\n",
            "        },\n",
            "    }\n",
            "    from_empty + from_one\n",
            "}\n",
            "macro touch = () => probe ()\n",
        ));
        let lowered = lower_run(&run).expect("the probe should lower");
        let names = lowered.planned_instance_names("probe");
        assert!(!names.is_empty(), "probe is instantiated; package={:?} items={:?}", run.package, entry_items(&run).iter().map(|item| item.syntax().id).collect::<Vec<_>>());
        let symbol = names[0].clone();
        with_jit(&lowered, |engine| {
            type Probe = unsafe extern "C" fn(*const usize) -> i32;
            let probe = unsafe { engine.get_function::<Probe>(&symbol).unwrap() };
            assert_eq!(unsafe { probe.call(std::ptr::null()) }, 1);
        });
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
