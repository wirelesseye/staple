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

/// The empty program emits strictly and verifies.
#[test]
fn strict_emission_empty_program_verifies() {
    let llvm = compile("", Emitter::Lowered).expect("the empty program emits strictly");
    assert!(llvm.contains("define i32 @main()"));
}

/// A mixed program (reactive bodies, a constructor adapter and a structural
/// `Debug` template) emits strictly, with no failed-body placeholder.
#[test]
fn strict_emission_of_a_mixed_program_has_no_placeholder_bodies() {
    let llvm = compile(
        concat!(
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
        ),
        Emitter::Lowered,
    )
    .expect("the mixed program emits strictly");
    assert!(!llvm.contains("stub.trap"), "no body is a trap placeholder");
}
