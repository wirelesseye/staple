//! `Bool` is an alias of the exact sum `True | False`.
//!
//! These tests cover the observable consequences of that declaration: the
//! singletons convert into a `Bool` representation in one step, the alias
//! flattens inside wider sums, and the standard traits and runtime `i1`
//! contract keep working for the sum rather than for a `from` wrapper.

mod common;

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
        "staple-bool-alias-{}-{nonce}",
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

#[cfg(unix)]
#[test]
fn bool_alias_introduces_from_bool_and_keeps_the_bool_runtime_contract() {
    let output = run(concat!(
        "use std.io.println\n",
        "use std.fmt.Formatter\n",
        "use std.convert.Convert\n",
        "type Flag from Bool\n",
        "let enabled: Flag = True\n",
        "let disabled: Flag = False\n",
        "def describe = (value: Bool | I32) => String :: match value {\n",
        "    True => \"true\",\n",
        "    False => \"false\",\n",
        "    number: I32 => \"number\",\n",
        "}\n",
        "def main = () => {\n",
        "    let mut count = 0\n",
        "    let mut index = 0\n",
        "    while (index < 3) {\n",
        "        if (index == 1 && ! (disabled.*)) { count = count + 1 }\n",
        "        index = index + 1\n",
        "    }\n",
        "    let equal = (Bool :: enabled.*) == (Bool :: enabled.*)\n",
        "    let different = (Bool :: enabled.*) == (Bool :: disabled.*)\n",
        "    let number: I32 = Convert.convert (Bool :: enabled.*)\n",
        "    let label = when { disabled.* => \"disabled\", else => \"enabled\" }\n",
        "    println \"${count} ${equal} ${different} ${number} ${label}\"\n",
        "    println \"${Formatter.display (Bool :: enabled.*)} ${Formatter.debug (Bool :: disabled.*)}\"\n",
        "    println \"${Formatter.debug ((Bool :: enabled.*, Bool :: disabled.*))}\"\n",
        "    println \"${describe True} ${describe False} ${describe 7}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(
        output,
        "1 True False 1 enabled\nTrue False\n(True, False)\ntrue false number\n"
    );
}

#[cfg(unix)]
#[test]
fn bool_alias_flows_through_option_and_result() {
    let output = run(concat!(
        "use std.io.println\n",
        "def option = (flag: Bool) => Option<Bool> :: Some flag\n",
        "def preserve = (value: Option<Bool>) => Option<Bool> :: {\n",
        "    let ?Some(flag) = value\n",
        "    Some flag\n",
        "}\n",
        "def show_option = (value: Option<Bool>) => String :: match value {\n",
        "    Some True => \"true\",\n",
        "    Some False => \"false\",\n",
        "    None => \"none\",\n",
        "}\n",
        "def result = (flag: Bool) => Result<Bool, String> :: if flag { Ok True } else { \"absent\" }\n",
        "def preserve_result = (value: Result<Bool, String>) => Result<Bool, String> :: {\n",
        "    let ?Ok(flag) = value\n",
        "    Ok flag\n",
        "}\n",
        "def show_result = (value: Result<Bool, String>) => String :: match value {\n",
        "    Ok True => \"ok true\",\n",
        "    Ok False => \"ok false\",\n",
        "    error: String => \"error ${error}\",\n",
        "}\n",
        "def main = () => {\n",
        "    println \"${show_option (preserve (option True))} ${show_option (preserve (option False))} ${show_option (preserve None)}\"\n",
        "    println \"${show_result (preserve_result (result True))} ${show_result (preserve_result (result False))}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "true false none\nok true error absent\n");
}

#[test]
fn bool_alias_generic_sum_row_inference() {
    // Accepting `Bool | None` proves `T` is the whole `True | False` row: a
    // single singleton would not cover the argument.
    check(concat!(
        "def present = <T> move value: T | None => T :: match value {\n",
        "    inner: T => inner,\n",
        "    None => panic \"absent\",\n",
        "}\n",
        "let maybe: Bool | None = True\n",
        "let inferred: Bool = present (maybe)\n",
    ));
}

#[test]
fn bool_alias_match_exhaustiveness_requires_both_singletons() {
    let messages = check_errors(concat!(
        "def incomplete = (value: Bool | I32) => I32 :: match value {\n",
        "    True => 1,\n",
        "    number: I32 => number,\n",
        "}\n",
    ));
    assert!(
        messages
            .iter()
            .any(|message| message.contains("non-exhaustive match")),
        "{messages:?}"
    );

    check(concat!(
        "def complete = (value: Bool | I32) => I32 :: match value {\n",
        "    True => 1,\n",
        "    False => 0,\n",
        "    number: I32 => number,\n",
        "}\n",
        "def wildcard = (value: Bool | I32) => I32 :: match value {\n",
        "    True => 1,\n",
        "    _ => 0,\n",
        "}\n",
    ));

    // A typed binding selects one alternative, not the aliased sub-sum.
    let messages = check_errors(concat!(
        "def split = (value: Bool | I32) => I32 :: match value {\n",
        "    flag: Bool => 1,\n",
        "    number: I32 => number,\n",
        "}\n",
    ));
    assert!(
        messages
            .iter()
            .any(|message| message.contains("does not select an alternative")),
        "{messages:?}"
    );
}

#[test]
fn bool_alias_is_the_only_bool_shape_recognized_by_the_coroutine_runner() {
    // The `until` predicate must be exactly `True | False`; a wider sum is
    // rejected by ordinary checking before it reaches the runner.
    let messages = check_errors(concat!(
        "use std.coroutine.*\n",
        "let signal flag = 0\n",
        "def wider: Bool -> (True | I32) = value => match value {\n",
        "    True => True,\n",
        "    False => 7,\n",
        "}\n",
        "def waiting: () -> Coroutine{Reactive}<()> = () => coro {\n",
        "    let _ = await (until { wider flag })\n",
        "    ()\n",
        "}\n",
        "let task = with Reactive = reactive_scope () { waiting () }\n",
    ));
    assert!(
        messages
            .iter()
            .any(|message| message.contains("found `True | I32`")),
        "a `True | I32` predicate should not be accepted as `Bool`: {messages:?}"
    );
}

#[cfg(unix)]
#[test]
fn bool_alias_singletons_fill_generic_bool_parameters() {
    // `False` alone is the singleton `False`; the expected result widens it
    // to the `Bool` that `Some` and `Ok` need, and a generic `Bool` result
    // widens into a sum containing `Bool`.
    let output = run(concat!(
        "use std.io.println\n",
        "def identity = <T> move value: T => T :: value\n",
        "def show = (value: Bool | I32) => String :: match value {\n",
        "    True => \"true\",\n",
        "    False => \"false\",\n",
        "    number: I32 => \"${number}\",\n",
        "}\n",
        "def main = () => {\n",
        "    let absent: Option<Bool> = Some False\n",
        "    let present: Result<Bool, String> = Ok True\n",
        "    let flag: Bool = True\n",
        "    let wide: Bool | I32 = identity flag\n",
        "    let literal: Bool | I32 = identity False\n",
        "    let first = match absent {\n",
        "        Some True => \"some true\",\n",
        "        Some False => \"some false\",\n",
        "        None => \"none\",\n",
        "    }\n",
        "    let second = match present {\n",
        "        Ok value => \"ok ${value}\",\n",
        "        _: String => \"error\",\n",
        "    }\n",
        "    println \"${first} ${second} ${show wide} ${show literal}\"\n",
        "}\n",
        "main ()\n",
    ));
    assert_eq!(output, "some false ok True true false\n");
}

#[test]
fn bool_alias_from_wrappers_are_not_bool() {
    // `Flag` is its own type: it is not a condition, even though `Bool`
    // converts into it.
    let messages = check_errors(concat!(
        "type Flag from Bool\n",
        "let flag: Flag = True\n",
        "let chosen = if flag { 1 } else { 0 }\n",
    ));
    assert!(
        messages
            .iter()
            .any(|message| message.contains("found `Flag`")),
        "{messages:?}"
    );
}
