//! Constructor argument inference through a sum representation.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::Path;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};
use staple_syntax::Diagnostic;

const PAIR: &str = concat!(
    "pub type Empty wrap pub ()\n",
    "pub type Pair<Element> wrap pub\n",
    "    Empty |\n",
    "    (first: Element, rest: I32)\n",
    "\n",
    "def classify: Pair<I32> -> I32 = value => match value {\n",
    "    Pair inner => match inner {\n",
    "        _: Empty => 2,\n",
    "        _: (first: I32, rest: I32) => 1,\n",
    "    },\n",
    "}\n",
);

fn check(source: &str) -> Result<staple_compiler::TypedModule, Vec<Diagnostic>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .expect("source should load");
    let resolved = NameResolver::new().resolve_program(program)?;
    TypeChecker::new().check(resolved)
}

fn check_ok(source: &str) -> staple_compiler::TypedModule {
    check(source).unwrap_or_else(|diagnostics| {
        panic!("source should check: {diagnostics:#?}");
    })
}

fn diagnostics(source: &str) -> Vec<String> {
    match check(source) {
        Ok(_) => Vec::new(),
        Err(diagnostics) => diagnostics.iter().map(ToString::to_string).collect(),
    }
}

fn lower_whole(module: &staple_compiler::TypedModule) -> LoweredModule {
    Lowerer::new().lower(module).expect("module should lower")
}

fn run_i32(module: &staple_compiler::TypedModule, name: &str) -> i32 {
    let lowered = lower_whole(module);
    let symbol = lowered
        .planned_instance_names(name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("{name} should be instantiated"));
    let context = Context::create();
    let text = CodeGenerator::new(&context)
        .compile_module(&lowered)
        .expect("module should emit");
    let buffer = inkwell::memory_buffer::MemoryBuffer::create_from_memory_range_copy(
        text.as_bytes(),
        "constructor-inference",
    );
    let llvm = context.create_module_from_ir(buffer).unwrap();
    llvm.verify().unwrap();
    let engine = llvm
        .create_jit_execution_engine(inkwell::OptimizationLevel::None)
        .unwrap();
    type Main = unsafe extern "C" fn() -> i32;
    let main = unsafe { engine.get_function::<Main>("main").unwrap() };
    assert_eq!(unsafe { main.call() }, 0);
    type Run = unsafe extern "C" fn(*const usize) -> i32;
    let run = unsafe { engine.get_function::<Run>(&symbol).unwrap() };
    unsafe { run.call(std::ptr::null()) }
}

#[test]
fn a_constructor_inside_a_def_uses_the_annotated_sum_type() {
    let module = check_ok(&format!(
        "{PAIR}\
         def run: () -> I32 = () => {{\n\
             let empty: Pair<I32> = Pair (Empty ())\n\
             let one: Pair<I32> = Pair (first: 5, rest: 0)\n\
             classify one * 10 + classify empty\n\
         }}\n\
         run ()\n"
    ));
    assert_eq!(run_i32(&module, "run"), 12);
}

#[test]
fn a_constructor_as_a_def_result_uses_the_annotated_sum_type() {
    let module = check_ok(&format!(
        "{PAIR}\
         def make: () -> Pair<I32> = () => Pair (first: 5, rest: 0)\n\
         def run: () -> I32 = () => classify (make ())\n\
         run ()\n"
    ));
    assert_eq!(run_i32(&module, "run"), 1);
}

#[test]
fn a_top_level_constructor_uses_the_annotated_sum_type() {
    let module = check_ok(&format!(
        "{PAIR}\
         let one: Pair<I32> = Pair (first: 5, rest: 0)\n\
         def run: () -> I32 = () => classify one\n\
         run ()\n"
    ));
    assert_eq!(run_i32(&module, "run"), 1);
}

#[test]
fn an_unannotated_top_level_constructor_infers_from_its_argument() {
    check_ok(&format!(
        "{PAIR}\
         let inferred = Pair (first: 5, rest: 0)\n\
         def run: () -> I32 = () => classify inferred\n"
    ));
}

#[test]
fn an_ambiguous_argument_stays_an_inference_error() {
    let diagnostics = diagnostics(concat!(
        "pub type Either<A, B> wrap pub A | B\n",
        "def run: () -> I32 = () => {\n",
        "    let inferred = Either 5\n",
        "    0\n",
        "}\n",
    ));
    assert!(
        diagnostics
            .iter()
            .any(|message| message.contains("could not infer all compile-time parameters")),
        "{diagnostics:#?}"
    );
}

#[test]
fn a_wrong_result_annotation_is_rejected() {
    let diagnostics = diagnostics(&format!(
        "{PAIR}\
         def run: () -> I32 = () => {{\n\
             let wrong: Pair<String> = Pair (first: 5, rest: 0)\n\
             0\n\
         }}\n"
    ));
    assert!(!diagnostics.is_empty(), "the wrong annotation should fail");
}

#[test]
fn a_constructor_inside_a_generic_function_is_tagged() {
    let module = check_ok(&format!(
        "{PAIR}\
         def make: <T> move T -> Pair<T> = <T> move x => Pair (first: x, rest: 0)\n\
         def run: () -> I32 = () => classify (make 5)\n\
         run ()\n"
    ));
    assert_eq!(run_i32(&module, "run"), 1);
}

#[test]
fn an_annotation_settles_an_argument_that_matches_two_alternatives() {
    let module = check_ok(
        "pub type Choice<Left, Right> wrap pub Left | Right\n\
         def run: () -> I32 = () => {\n\
             let a: Choice<I32, String> = Choice 5\n\
             match a { Choice inner => match inner { number: I32 => number, _ => 0 } }\n\
         }\n\
         run ()\n",
    );
    assert_eq!(run_i32(&module, "run"), 5);
}
