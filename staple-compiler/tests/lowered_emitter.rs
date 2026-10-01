use std::path::Path;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, Emitter, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};

fn prepare(source: &str) -> LoweredModule {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap();
    let resolved = NameResolver::new().resolve_program(program).unwrap();
    let typed = TypeChecker::new().check(resolved).unwrap();
    Lowerer::new().lower(&typed).unwrap()
}

fn compile(source: &str, emitter: Emitter) -> Result<String, Vec<staple_syntax::Diagnostic>> {
    let lowered = prepare(source);
    let context = Context::create();
    CodeGenerator::with_emitter(&context, emitter).compile_module(&lowered)
}

#[test]
fn selector_preserves_legacy_and_reports_unported_lowered_body() {
    let llvm = compile("", Emitter::Legacy).unwrap();
    assert!(llvm.contains("define i32 @main()"));

    let diagnostics = compile("", Emitter::Lowered).unwrap_err();
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.starts_with("lowered emitter: ")),
        "{diagnostics:?}"
    );
}

/// F8: a failed body no longer stops the compile, so one strict lowered
/// compile reports every unsupported body it reached.
#[test]
fn strict_emission_reports_every_failed_body() {
    let diagnostics = compile("", Emitter::Lowered).unwrap_err();
    assert!(
        diagnostics.len() > 1,
        "expected one diagnostic per failed body, got {diagnostics:?}"
    );
}

/// Stage 5.3 Step 2 invariant: the empty program's partial module verifies and
/// the report is the progress measure for the standard library's eager bodies.
#[test]
fn partial_emission_empty_program_verifies() {
    let lowered = prepare("");
    let context = Context::create();
    let (_, report) = CodeGenerator::with_emitter(&context, Emitter::Lowered)
        .compile_lowered_partial(&lowered)
        .expect("the empty program should emit a verified partial module");
    assert!(
        !report.stubbed().is_empty(),
        "the standard library's eager bodies still stub in 5.3"
    );
    eprintln!(
        "empty program: {} stubs, {} families",
        report.stubbed().len(),
        report.family_histogram().len()
    );
    for (family, count) in report.family_histogram() {
        eprintln!("{count:5}  {family}");
    }
}

/// Stage 5.3 Step 2: partial mode stubs unsupported bodies, stubs the
/// artifact families without body emitters, and returns a verified module
/// with the stub/histogram report.
#[test]
fn partial_emission_stubs_unsupported_sites_and_verifies() {
    // Stage 5.6 Step 3 emits drop glue, so the stub fixture uses the
    // still-unported `coro` expressions and a 5.7 structural Debug artifact.
    let lowered = prepare(concat!(
        "use std.coroutine.*\n",
        "def first: () -> Coroutine{} I32 = () => coro { 1 }\n",
        "def second: () -> Coroutine{} I32 = () => coro { 2 }\n",
        "type Point = ctor (I32, I32)\n",
        "let point: Point = Point (1, 2)\n",
        "let make: () -> ((I32, I32) -> Point) = () => Point\n",
        "let pair = (1, 2)\n",
        "let shown = \"pair: ${pair:?}\"\n",
        "let one = first ()\n",
        "let two = second ()\n",
    ));
    let context = Context::create();
    let (ir, report) = CodeGenerator::with_emitter(&context, Emitter::Lowered)
        .compile_lowered_partial(&lowered)
        .expect("partial emission should verify");
    assert!(ir.contains("@llvm.trap"), "stub bodies call llvm.trap");
    assert!(
        report.stubbed().iter().any(|stub| stub.name() == "first"
            && stub.diagnostic().message.contains("coro is not implemented")),
        "the unsupported `first` body is stubbed: {:?}",
        report.stubbed()
    );
    assert!(
        report.stubbed().iter().any(|stub| stub.name() == "second"),
        "the unsupported `second` body is stubbed"
    );
    assert!(
        report.stubbed().iter().any(|stub| matches!(
            stub.entry(),
            staple_compiler::LoweredCatalogEntry::Artifact(_)
        )),
        "an artifact whose family has no body emitter is stubbed: {:?}",
        report.stubbed()
    );
    assert!(
        report
            .family_histogram()
            .iter()
            .any(|(family, count)| family == "coro" && *count >= 2),
        "the histogram counts the coro family: {:?}",
        report.family_histogram()
    );
    assert!(
        report.family_histogram().windows(2).all(|pair| {
            pair[0].1 > pair[1].1 || (pair[0].1 == pair[1].1 && pair[0].0 < pair[1].0)
        }),
        "the histogram is ordered by count then family: {:?}",
        report.family_histogram()
    );
    eprintln!(
        "fixture: {} stubs, {} families",
        report.stubbed().len(),
        report.family_histogram().len()
    );
    for (family, count) in report.family_histogram() {
        eprintln!("{count:5}  {family}");
    }
}
