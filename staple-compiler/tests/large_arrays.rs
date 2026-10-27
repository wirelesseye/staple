mod common;

// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker, TypedModule,
};
use staple_syntax::Diagnostic;
use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn check(source: &str) -> Result<TypedModule, Vec<Diagnostic>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let resolved = NameResolver::new()
        .resolve_program(program)
        .unwrap_or_else(|errors| panic!("resolve: {errors:?}"));
    TypeChecker::new().check(resolved)
}

fn messages(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.clone())
        .collect()
}

fn lower(source: &str) -> LoweredModule {
    let typed = check(source).unwrap_or_else(|errors| panic!("check: {errors:?}"));
    Lowerer::new()
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

#[cfg(unix)]
fn run(source: &str) -> String {
    let lowered = lower(source);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-large-arrays-{}-{nonce}",
        std::process::id()
    ));
    let object = base.with_extension("o");
    let context = Context::create();
    CodeGenerator::new(&context)
        .emit_object(&lowered, &object, None)
        .unwrap_or_else(|errors| panic!("emit: {errors:?}"));
    let link = common::linker(&[&object], &base).output().unwrap();
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

/// Arrays far past the old 65,535-element limit move through bindings,
/// calls, returns, spreads, branches and loops with value semantics. Each
/// array here is larger than the size the backend keeps in registers.
#[cfg(unix)]
#[test]
fn large_arrays_keep_value_semantics_through_calls_and_branches() {
    let output = run(concat!(
        "use std.io.println\n",
        "def make = (x: I32) => (I32 ^ 100000) :: (x ^ 100000)\n",
        "def pick = (values: (I32 ^ 100000), index: USize) => I32 :: values[index]\n",
        "def pass = (values: (I32 ^ 100000), tag: I32) => (I32 ^ 100000) :: values\n",
        "def choose = (flag: Bool, left: (I32 ^ 300), right: (I32 ^ 300)) => (I32 ^ 300) ::\n",
        "    if flag { left } else { right }\n",
        "def last = values: (I32 ^ 1000) => I32 :: values[999]\n",
        "def main = () => {\n",
        "    let a = make 3\n",
        "    let mut b = pass (a, 0)\n",
        "    b.7 = 9\n",
        "    println \"${pick (a, 7)} ${b.7} ${a.99999}\"\n",
        "    let mixed: (String, ...(I32 ^ 300)) = (\"x\", ...(4 ^ 300))\n",
        "    println \"${mixed.0} ${mixed.300}\"\n",
        "    let left = (1 ^ 300)\n",
        "    let right = (2 ^ 300)\n",
        "    println \"${(choose (True, left, right)).0} ${(choose (False, left, right)).299}\"\n",
        "    let mut total = (0 ^ 300)\n",
        "    for step in (0 .. 3) {\n",
        "        total.5 = total.5 + step\n",
        "    }\n",
        "    println \"${total.5} ${last (5 ^ 1000)}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "3 9 3\nx 4\n1 2\n3 5\n");
}

/// Long spreads are copied a run at a time, splitting where the source's and
/// destination's runs end, so every position still lands in the right place.
#[cfg(unix)]
#[test]
fn long_spreads_copy_across_run_boundaries() {
    let output = run(concat!(
        "use std.io.println\n",
        "def main = () => {\n",
        "    let a: (String, ...(I32 ^ 20), F64) = (\"x\", ...(7 ^ 20), 2.5)\n",
        "    let b: (...(I32 ^ 5), String, ...(I32 ^ 20), F64, ...(I32 ^ 3)) =\n",
        "        (1, 2, 3, 4, 5, ...a, 8, 8, 8)\n",
        "    let c: (...(I32 ^ 40)) = (...(1 ^ 18), ...(2 ^ 22))\n",
        "    let d: (String, ...(I32 ^ 400000)) = (\"y\", ...(4 ^ 400000))\n",
        "    println \"${b.4} ${b.5} ${b.6} ${b.25} ${b.26} ${b.29} ${c.17} ${c.18} ${c.39}\"\n",
        "    println \"${d.0} ${d.1} ${d.400000}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "5 x 7 7 2.5 8 1 2 2\ny 4 4\n");
}

/// A million-element array type checks and lowers without visiting each
/// element: type checking an annotated binding once expanded every position.
#[test]
fn a_million_element_binding_lowers() {
    lower(concat!(
        "def main = () => {\n",
        "    let values: (U8 ^ 1000000) = (1 ^ 1000000)\n",
        "    let copy: (U8 ^ 1000000) = values\n",
        "}\n",
        "main ()\n",
    ));
}

#[test]
fn an_inferred_length_absorbs_the_positions_before_a_fixed_suffix() {
    check(concat!(
        "def first = <N where Natural N> values: (String, ...(I32 ^ N)) => String :: values.0\n",
        "def tail = <N where Natural N> values: (...(I32 ^ N), String) => () :: ()\n",
        "let name = first (\"name\", 1, 2, 3)\n",
        "let unit = tail (1, 2, 3, \"end\")\n",
    ))
    .unwrap_or_else(|errors| panic!("check: {errors:?}"));
}

#[test]
fn positions_after_a_symbolic_run_are_rejected() {
    let errors = check(
        "def tag = <N where Natural N> values: (...(I32 ^ N), String) => String :: values.1\n",
    )
    .expect_err("position 1 depends on N");
    assert!(
        messages(&errors)
            .iter()
            .any(|message| message
                .contains("depends on a repetition whose length is not known here")),
        "{errors:?}"
    );
}

#[test]
fn array_lengths_beyond_the_code_generator_limit_are_rejected() {
    let errors = check("let values: (U8 ^ 4294967296) = (1 ^ 4294967296)\n")
        .expect_err("the length does not fit an LLVM array");
    assert!(
        messages(&errors)
            .iter()
            .any(|message| message.contains("exceeds the limit of 4294967295")),
        "{errors:?}"
    );
}

#[test]
fn a_parameter_with_too_many_positions_is_rejected_when_lowered() {
    let typed = check("def ignore = (values: (U8 ^ 70000)) => () :: ()\n")
        .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    let errors = Lowerer::new()
        .lower(&typed)
        .expect_err("each position would be a separate argument");
    assert!(
        messages(&errors)
            .iter()
            .any(|message| message.contains("more than the limit of 65535")),
        "{errors:?}"
    );
}
