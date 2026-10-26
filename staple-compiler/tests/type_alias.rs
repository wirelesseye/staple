//! Transparent type aliases keep their source name for display, while every
//! semantic question is answered by the aliased type.

// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use staple_compiler::{NameResolver, ProgramLoader, TypeChecker};
use staple_syntax::Diagnostic;
use std::path::Path;

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

fn check(source: &str) {
    check_result(source).unwrap_or_else(|errors| panic!("check: {errors:?}"));
}

fn check_errors(source: &str) -> Vec<String> {
    check_result(source)
        .expect_err("source should fail checking")
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect()
}

#[test]
fn aliases_are_interchangeable_with_their_targets() {
    check(concat!(
        "type Pair<T> = (T, T)\n",
        "type Flag = Bool\n",
        "def accept_bool = (value: Bool) => Bool :: value\n",
        "def accept_sum = (value: True | False) => Bool :: value\n",
        "def accept_flag = (value: Flag) => Bool :: value\n",
        "def accept_pair = (value: Pair<I32>) => (I32, I32) :: value\n",
        "let direct: Bool = True\n",
        "let sum: True | False = direct\n",
        "let flag: Flag = direct\n",
        "let pair: Pair<I32> = (1, 2)\n",
        "let accepted = accept_bool sum\n",
        "let accepted_flag = accept_sum flag\n",
        "let accepted_pair = accept_pair pair\n",
    ));
}

#[test]
fn a_spread_of_an_alias_product_works() {
    check(concat!(
        "type NamedPair<T> = (first: T, second: T)\n",
        "let named: NamedPair<I32> = (first: 1, second: 2)\n",
        "let first = named.first\n",
        "type Pair<T> = (T, T)\n",
        "let pair: Pair<I32> = (1, 2)\n",
        "let expanded: (I32, I32, I32, I32) = (0, ...pair, 3)\n",
    ));
}

#[test]
fn an_alias_of_a_function_type_is_callable() {
    check(concat!(
        "type Mutator<T> = mut T -> ()\n",
        "def bump: mut I32 -> () = mut value => { value = value + 1 }\n",
        "let mut target = 0\n",
        "let mutator: Mutator<I32> = bump\n",
        "let no_change: Mutator<I32> = mut value => ()\n",
        "mutator target\n",
        "no_change target\n",
    ));
}

#[test]
fn a_method_on_an_alias_of_a_wrapper_finds_the_wrapper_companion() {
    check(concat!(
        "type Meters wrap I32\n",
        "type Distance = Meters\n",
        "companion Meters {\n",
        "    pub def double = value: Meters => Meters :: Meters (value.* + value.*)\n",
        "}\n",
        "def double = (value: Distance) => Distance :: Meters.double value\n",
        "let distance: Distance = Meters 2\n",
        "let doubled = double distance\n",
    ));
}

#[test]
fn a_display_implementation_on_bool_still_resolves() {
    check(concat!(
        "let flag: Bool = True\n",
        "let text = \"${flag}\"\n",
    ));
}

#[test]
fn a_type_error_names_the_alias() {
    let messages = check_errors(concat!("type Flag = Bool\n", "let bad: Flag = 1\n"));
    assert!(
        messages
            .iter()
            .any(|message| message.contains("expected `Flag`, found `I32`")),
        "{messages:?}"
    );
}

#[test]
fn a_generic_alias_still_reports_a_failed_bound() {
    let messages = check_errors(concat!(
        "type Bad<T where Iterator T> = T\n",
        "let invalid: Bad<I32> = 1\n",
    ));
    assert!(
        messages.iter().any(|message| {
            message.contains("trait bound is not satisfied") && message.contains("I32")
        }),
        "{messages:?}"
    );
}

#[test]
fn a_cyclic_alias_still_reports_a_cyclic_definition() {
    let messages = check_errors("type A = B\ntype B = A\nlet value: A = 0\n");
    assert!(
        messages
            .iter()
            .any(|message| message.contains("cyclic type definition")),
        "{messages:?}"
    );
}
