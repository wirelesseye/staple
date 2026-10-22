// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};
use staple_syntax::Diagnostic;
use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn check(source: &str) -> Result<staple_compiler::TypedModule, Vec<Diagnostic>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let resolved = NameResolver::new().resolve_program(program)?;
    TypeChecker::new().check(resolved)
}

fn lower(source: &str) -> LoweredModule {
    let typed = check(source).unwrap_or_else(|errors| panic!("check: {errors:?}"));
    Lowerer::new()
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

#[cfg(unix)]
fn run(lowered: &LoweredModule) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-iterator-operations-{}-{nonce}",
        std::process::id()
    ));
    let object = base.with_extension("o");
    let context = Context::create();
    CodeGenerator::new(&context)
        .emit_object(lowered, &object, None)
        .unwrap_or_else(|errors| panic!("emit: {errors:?}"));
    let link = Command::new("cc")
        .arg(&object)
        .arg("-o")
        .arg(&base)
        .output()
        .unwrap();
    assert!(
        link.status.success(),
        "link: {}",
        String::from_utf8_lossy(&link.stderr)
    );
    let output = Command::new(&base).output().unwrap();
    let _ = std::fs::remove_file(object);
    let _ = std::fs::remove_file(base);
    assert!(
        output.status.success(),
        "execution: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(unix)]
#[test]
fn iterator_operations_produce_their_documented_output() {
    let output = run(&lower(include_str!("fixtures/iterator_operations.sta")));
    assert_eq!(
        output,
        concat!(
            "map: 10 20 30\n",
            "filter: 4 5 6\n",
            "filter_map: 4 16 36\n",
            "take: 0 1 2\n",
            "take past end: 0 1\n",
            "skip: 4 5\n",
            "skip past end:\n",
            "take_while: 1 2 3\n",
            "enumerate: 5 600 7\n",
            "zip: 11 13 15\n",
            "chain: 1 2 7 8\n",
            "chain mapped: 2 4 4\n",
            "fold: 10\n",
            "reduce: 24\n",
            "reduce empty: none\n",
            "count: 3\n",
            "any: yes\n",
            "all: no\n",
            "find: 5\n",
            "find none: none\n",
            "last: 3\n",
            "sum: 10\n",
            "min: 2\n",
            "max: 6\n",
            "min empty: none\n",
            "for_each: 1\n",
            "for_each: 2\n",
            "for_each: 3\n",
            "for: 0 10\n",
            "for: 1 11\n",
            "for: 2 12\n",
            "list sum: 12\n",
            "nested: 3 6\n",
        )
    );
}

#[test]
fn lazy_adapters_reject_effectful_functions() {
    // `Iterator.next` is pure, so an adapter cannot run an effect when it is
    // advanced. The effect belongs in a consumer such as `for_each`.
    let errors = check(concat!(
        "use std.io.println\n",
        "def run = () => {\n",
        "    let noisy = (x: I32) => I32 :: { println \"${x}\"; x }\n",
        "    let mapped = (1 .. 4) |> map noisy\n",
        "    ()\n",
        "}\n",
    ))
    .err()
    .expect("an effectful `map` function should be rejected");
    assert!(
        errors.iter().any(|error| error.message.contains("IO")),
        "{errors:?}"
    );
}

#[test]
#[ignore = "the checker accepts any effectful inline lambda for a pure function parameter"]
fn lazy_adapters_reject_effectful_inline_lambdas() {
    let errors = check(concat!(
        "use std.io.println\n",
        "def run = () => {\n",
        "    let noisy = (1 .. 4) |> map (x => { println \"${x}\"; x })\n",
        "    ()\n",
        "}\n",
    ))
    .err()
    .expect("an effectful inline `map` function should be rejected");
    assert!(
        errors.iter().any(|error| error.message.contains("IO")),
        "{errors:?}"
    );
}

#[test]
fn consumers_propagate_their_function_effects() {
    let errors = check(concat!(
        "use std.io.println\n",
        "def quiet: () -> () = () => (1 .. 4) |> for_each (x => println \"${x}\")\n",
    ))
    .err()
    .expect("a pure function cannot call `for_each` with an effectful function");
    assert!(
        errors.iter().any(|error| error.message.contains("IO")),
        "{errors:?}"
    );
}

#[cfg(unix)]
#[test]
fn operations_are_iterator_members_rather_than_prelude_names() {
    // The operations are blanket companion members of every `Iterator`, so a
    // program keeps names such as `count` for itself and still pipes into the
    // operation, and `Type.member` reaches them through an iterator type.
    let output = run(&lower(concat!(
        "use std.io.println\n",
        "def count = (label: String; n: I32) => \"${label}: ${n}\"\n",
        "def run = () => {\n",
        "    println (count \"own count\" 3)\n",
        "    println \"pipe count: ${(0 .. 4) |> count}\"\n",
        "    println \"named sum: ${Range.sum (1 .. 5)}\"\n",
        "    println \"named map: ${Range.map (1 .. 4) (x => x * 3) |> sum}\"\n",
        "}\n",
        "run ()\n",
    )));
    assert_eq!(
        output,
        "own count: 3\npipe count: 4\nnamed sum: 10\nnamed map: 18\n"
    );
    let errors = check("let total = map (0 .. 3) (x => x)\n")
        .err()
        .expect("`map` is no longer a free function");
    assert!(
        errors.iter().any(|error| error.message.contains("map")),
        "{errors:?}"
    );
}
