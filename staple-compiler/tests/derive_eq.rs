//! Tests for the standard library `@derive_eq` modifier.

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
    let resolved = NameResolver::new().resolve_program(program)?;
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
    let base =
        std::env::temp_dir().join(format!("staple-derive-eq-{}-{nonce}", std::process::id()));
    let object = base.with_extension("o");
    let context = Context::create();
    CodeGenerator::new(&context)
        .emit_object(&lowered, &object, None)
        .unwrap_or_else(|errors| panic!("emit: {errors:?}"));
    let link = Command::new("cc")
        .arg(&object)
        .arg("-o")
        .arg(&base)
        .arg("-lm")
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
fn derived_equality_compares_representations() {
    let output = run(r#"
use std.io.println
def show = value: Bool => println (if value { "True" } else { "False" })
@derive_eq
type Point wrap (x: I32 = 9, y: (I32, String))
@derive_eq
type Pair from (I32, Bool)
@derive_eq
type Plain wrap I32
@derive_eq
type Enabled
@derive_eq
type Box<T> wrap T
@derive_eq
type Empty wrap ()
def main = () => {
    show (Point (1, (2, "a")) == Point (1, (2, "a")))
    show (Point (1, (2, "a")) == Point (1, (2, "b")))
    show (Pair (1, True) != Pair (1, False))
    show (Plain 3 == Plain 3)
    show (Enabled == Enabled)
    show (Box.<(I32, String)> (1, "a") == Box.<(I32, String)> (1, "a"))
    show (Box.<I32> 1 != Box.<I32> 2)
}
main ()
"#);
    assert_eq!(output, "True\nFalse\nTrue\nTrue\nTrue\nTrue\nTrue\n");
}

#[test]
fn derived_equality_requires_eq_representations() {
    for source in [
        "type Local wrap I32\n@derive_eq\ntype Holder wrap Local\n",
        "type Local wrap I32\n@derive_eq\ntype Box<T> wrap T\nlet invalid = Box (Local 1) == Box (Local 1)\n",
    ] {
        let messages = check_errors(source);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("no trait implementation")),
            "{messages:?}"
        );
    }
}

#[test]
fn derived_equality_rejects_unsupported_items() {
    for source in [
        "@derive_eq\ntype Alias = I32\n",
        "@derive_eq\ndef value = 1\n",
    ] {
        let messages = check_errors(source);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("derive_eq item is unsupported")),
            "{messages:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn derived_equality_uses_hygienic_trait_and_caller_field_types() {
    let output = run(r#"
use std.io.println
def show = value: Bool => println (if value { "True" } else { "False" })
use std.cmp.(derive_eq)
trait Eq T { eq: (T; T) -> Bool }
impl Eq I32 { eq = (_; _) => False }
type ParseError wrap I32
impl std.cmp.Eq ParseError { eq = (ParseError a; ParseError b) => a == b }
@derive_eq
type Holder<T> wrap (value: T, error: ParseError)
def main = () => {
    show (std.cmp.Eq.eq.<Holder<I32>> ...(Holder.<I32> (1, ParseError 2); Holder.<I32> (1, ParseError 2)))
    show (std.cmp.Eq.ne.<Holder<I32>> ...(Holder.<I32> (1, ParseError 2); Holder.<I32> (1, ParseError 3)))
}
main ()
"#);
    assert_eq!(output, "True\nTrue\n");
}
