use std::path::Path;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
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

fn compile(source: &str) -> Result<String, Vec<staple_syntax::Diagnostic>> {
    let lowered = prepare(source);
    let context = Context::create();
    CodeGenerator::new(&context).compile_module(&lowered)
}

/// The default emitter emits the empty entry harness and a reactive body.
#[test]
fn default_emitter_emits_the_entry_harness_and_reactive_body() {
    let context = Context::create();
    let empty = prepare("");
    let llvm = CodeGenerator::new(&context).compile_module(&empty).unwrap();
    assert!(llvm.contains("define i32 @main()"));
    let reactive = prepare(concat!(
        "use std.coroutine.*\n",
        "def first: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
        "let a = first ()\n",
    ));
    let llvm = CodeGenerator::new(&context)
        .compile_module(&reactive)
        .unwrap();
    assert!(llvm.contains("call ptr @__staple_reaction_create"));
}

/// Held while bodies could still fail. Every construct family is now
/// emitted, so strict lowered compilation of the bodies that used to fail
/// collects no diagnostic at all.
#[test]
fn strict_emission_compiles_every_reactive_body() {
    let llvm = compile(concat!(
        "use std.coroutine.*\n",
        "def first: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
        "def second: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
    ))
    .expect("every reactive body emits strictly");
    assert!(
        llvm.matches("call ptr @__staple_reaction_create").count() >= 2,
        "both reaction bodies emitted"
    );
}

/// The empty program emits strictly and verifies.
#[test]
fn strict_emission_empty_program_verifies() {
    let llvm = compile("").expect("the empty program emits strictly");
    assert!(llvm.contains("define i32 @main()"));
}

/// A mixed program (reactive bodies, a constructor adapter and a structural
/// `Debug` template) emits strictly, with no failed-body placeholder.
#[test]
fn strict_emission_of_a_mixed_program_has_no_placeholder_bodies() {
    let llvm = compile(concat!(
        "use std.coroutine.*\n",
        "def first: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
        "def second: () -> () = () => with Reactive = reactive_scope () { reaction { () } }\n",
        "type Point = wrap (I32, I32)\n",
        "let point: Point = Point (1, 2)\n",
        "let make: () -> ((I32, I32) -> Point) = () => Point\n",
        "let pair = (1, 2)\n",
        "let shown = \"pair: ${pair:?}\"\n",
        "let one = first ()\n",
        "let two = second ()\n",
    ))
    .expect("the mixed program emits strictly");
    assert!(!llvm.contains("stub.trap"), "no body is a trap placeholder");
}

/// Function-like intrinsics sit behind ordinary `def` wrappers, so their
/// public API remains usable as first-class values.
#[test]
fn wrapped_intrinsic_apis_are_first_class_values() {
    compile(concat!(
        "let release: move I32 -> () = drop\n",
        "let released = release 1\n",
        "let replace: [mut Ref I32, move I32] -> I32 = Ref.replace\n",
        "let mut reference = Ref 1\n",
        "let previous = replace reference 2\n",
    ))
    .unwrap();
}
