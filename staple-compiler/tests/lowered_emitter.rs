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

/// Stage 5.8 Step 7 closed every construct family, so the selector now
/// compiles the same reactive body under both emitters; the lowered module is
/// named from the catalog rather than legacy's syntax keys.
#[test]
fn selector_preserves_legacy_and_emits_reactive_body() {
    let source = concat!(
        "use std.coroutine.*\n",
        "def first: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
        "let a = first ()\n",
    );
    let llvm = compile("", Emitter::Legacy).unwrap();
    assert!(llvm.contains("define i32 @main()"));
    compile("", Emitter::Lowered).expect("the empty program compiles strictly");

    let legacy = compile(source, Emitter::Legacy).expect("legacy reactive body");
    let lowered = compile(source, Emitter::Lowered).expect("lowered reactive body");
    assert!(
        legacy.contains("__staple_reaction_create") && lowered.contains("__staple_reaction_create"),
        "both emitters emit the reaction runtime call"
    );
    assert_ne!(
        legacy, lowered,
        "the selector routes through the catalog-named lowered emitter"
    );
}

/// F8 held while bodies could still fail. Every construct family is now
/// emitted, so strict lowered compilation of the bodies that used to fail
/// collects no diagnostic at all.
#[test]
fn strict_emission_compiles_every_reactive_body() {
    let llvm = compile(
        concat!(
            "use std.coroutine.*\n",
            "def first: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
            "def second: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
        ),
        Emitter::Lowered,
    )
    .expect("every reactive body emits strictly");
    assert!(
        llvm.matches("call ptr @__staple_reaction_create").count() >= 2,
        "both reaction bodies emitted"
    );
}

/// Stage 5.3 Step 2 invariant, ratcheted by Stage 5.6 Step 8: the empty
/// program's partial module verifies with no stubbed body at all.
#[test]
fn partial_emission_empty_program_verifies() {
    let lowered = prepare("");
    let context = Context::create();
    let (_, report) = CodeGenerator::with_emitter(&context, Emitter::Lowered)
        .compile_lowered_partial(&lowered)
        .expect("the empty program should emit a verified partial module");
    assert!(
        report.stubbed().is_empty(),
        "the empty program has no stubbed body: {:?}",
        report.family_histogram()
    );
    eprintln!(
        "empty program: {} stubs, {} families",
        report.stubbed().len(),
        report.family_histogram().len()
    );
}

/// Stage 5.3 Step 2's partial mode with every construct family now emitted:
/// the same program verifies with no stubbed body and no trap.
#[test]
fn partial_emission_with_reactive_bodies_verifies() {
    let lowered = prepare(concat!(
        "use std.coroutine.*\n",
        "def first: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
        "def second: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
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
    let _ = ir;
    assert!(
        report.stubbed().is_empty(),
        "every body emits: {:?}",
        report.stubbed()
    );
    assert!(
        report.family_histogram().is_empty(),
        "no stub families remain: {:?}",
        report.family_histogram()
    );
    eprintln!("fixture: {} stubs", report.stubbed().len());
}
