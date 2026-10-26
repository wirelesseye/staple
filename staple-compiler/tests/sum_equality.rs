//! Structural `Eq` for sum types.
//!
//! A sum value equals another value of the same sum when both hold the same
//! alternative and that alternative's `Eq` reports the payloads equal. `Bool`
//! is the sum `True | False`, so its equality is derived this way too.

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

fn check_result(source: &str) -> Result<staple_compiler::TypedModule, Vec<Diagnostic>> {
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

fn check(source: &str) -> staple_compiler::TypedModule {
    check_result(source).unwrap_or_else(|errors| panic!("check: {errors:?}"))
}

fn check_errors(source: &str) -> Vec<String> {
    check_result(source)
        .expect_err("source should fail checking")
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

fn lower(module: &staple_compiler::TypedModule) -> LoweredModule {
    Lowerer::new()
        .lower(module)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

/// Builds and runs a program, returning its standard output.
#[cfg(unix)]
fn run(source: &str) -> String {
    let module = check(source);
    let lowered = lower(&module);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-sum-equality-{}-{nonce}",
        std::process::id()
    ));
    let object = base.with_extension("o");
    let context = Context::create();
    CodeGenerator::new(&context)
        .emit_object(&lowered, &object, None)
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
fn bool_equality_is_derived_from_its_alternatives() {
    let output = run(concat!(
        "use std.io.println\n",
        "def main = () => {\n",
        "    let yes: Bool = True\n",
        "    let no: Bool = False\n",
        "    println \"${yes == yes} ${no == no} ${yes == no} ${no == yes}\"\n",
        "    println \"${yes != yes} ${yes != no}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "True True False False\nFalse True\n");
}

#[cfg(unix)]
#[test]
fn sum_equality_compares_tags_then_payloads() {
    let output = run(concat!(
        "use std.io.println\n",
        "use std.cmp.Eq\n",
        "def same: <T where Eq T> (T; T) -> Bool = <T where Eq T> (left; right) => left == right\n",
        "def main = () => {\n",
        "    let one: I32 | String = 1\n",
        "    let other: I32 | String = 2\n",
        "    let text: I32 | String = \"one\"\n",
        "    let same_text: I32 | String = \"one\"\n",
        "    println \"${one == one} ${one == other} ${one == text} ${text == same_text}\"\n",
        "    println \"${one != other} ${text != same_text}\"\n",
        "    println \"${same one one} ${same text one}\"\n",
        "    let pair: (Bool, I32 | String) = (True, text)\n",
        "    let different: (Bool, I32 | String) = (True, one)\n",
        "    println \"${pair == pair} ${pair == different}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(
        output,
        "True False False True\nTrue False\nTrue False\nTrue False\n"
    );
}

#[test]
fn sum_equality_requires_every_alternative_to_be_eq() {
    let messages = check_errors(concat!(
        "type Opaque\n",
        "let value: I32 | Opaque = 1\n",
        "let equal: Bool = value == value\n",
    ));
    assert!(!messages.is_empty());
}
