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
            "collect list: 10 20 30\n",
            "collect string: w1w2w3\n",
            "collect explicit: 1 2 3\n",
            "collect custom: 3\n",
            "to_list: 2 4 6\n",
            "extend list: 0 1 2 3\n",
            "extend string: start<1><2>\n",
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
            "chain different: 0 1 2 4 5\n",
            "chain empty first: 6 8\n",
            "chain empty second: 2 4\n",
            "chain owned: 1 2 4\n",
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

#[cfg(unix)]
#[test]
fn tier_two_iterator_operations_produce_their_documented_output() {
    let output = run(&lower(include_str!("fixtures/iterator_tier_two.sta")));
    assert_eq!(
        output,
        concat!(
            "flat_map: 0 0 1 0 1 2\n",
            "scan: 1 3 6 10\n",
            "scan stop: 1 3 6\n",
            "skip_while: 4 5 6\n",
            "step_by: 0 3 6 9\n",
            "peek: 1\n",
            "peek rest: 1 2 3\n",
            "position: 3\n",
            "position none: none\n",
            "nth: 3\n",
            "nth past end: none\n",
            "min_by: 2\n",
            "max_by: 6\n",
            "try_fold ok: ok 10\n",
            "try_fold err: err reached five\n",
            "empty:\n",
            "once: 7\n",
            "repeat_n: 5 5 5\n",
            "repeat_n zero:\n",
            "successors: 1 2 3 4\n",
            "successors empty:\n",
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

fn errors_of(source: &str, reason: &str) -> Vec<String> {
    check(source)
        .err()
        .unwrap_or_else(|| panic!("{reason}"))
        .into_iter()
        .map(|error| error.message)
        .collect()
}

#[test]
fn collect_rejects_targets_that_cannot_hold_the_items() {
    // The bounds of an explicit `collect.<_, Target, _>` are checked at the
    // call that supplies the iterator, so an unsuitable target is reported.
    let errors = errors_of(
        "def run = () => {\n    let wrong = (1 .. 4) |> collect.<_, Bool, _>\n    ()\n}\n",
        "a target without a `FromIterator` implementation is rejected",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("(Bool, I32)")),
        "{errors:?}"
    );
    let errors = errors_of(
        concat!(
            "use std.list.List\n",
            "def run = () => {\n",
            "    let wrong = (1 .. 4) |> collect.<_, List<String>, _>\n",
            "    ()\n",
            "}\n",
        ),
        "a target for a different item type is rejected",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("(List<String>, I32)")),
        "{errors:?}"
    );
}

#[test]
fn collect_without_a_target_suggests_an_annotation_or_to_list() {
    let errors = errors_of(
        "def run = () => {\n    let items = (1 .. 4) |> collect\n    ()\n}\n",
        "nothing chooses the target",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("`collect`") && error.contains("to_list")),
        "{errors:?}"
    );
}

#[test]
fn generic_code_must_state_the_item_bounds_an_operation_needs() {
    // Regression test: a bound on the item type of a generic iterator, such as
    // `FromIterator (List I32) (Iterator.Item Iter)`, was never checked in the
    // generic definition, and the mistake surfaced inside `std.iterator` once
    // the definition was used.
    let errors = errors_of(
        concat!(
            "use std.list.List\n",
            "def gather = <Iter where Iterator Iter> move iter: Iter => List<I32> :: iter |> collect\n",
        ),
        "the item type is unknown, so `List I32` cannot be proven",
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("(List<I32>, Iterator.Item)")),
        "{errors:?}"
    );
    let errors = errors_of(
        "def total = <Iter where Iterator Iter> move iter: Iter => Iterator.Item Iter :: iter |> sum\n",
        "`sum` needs `Add` and `Default` on the item type",
    );
    assert!(
        errors.iter().any(|error| error.contains("`Iterator.Item`")),
        "{errors:?}"
    );
}

#[cfg(unix)]
#[test]
fn generic_and_multi_item_targets_collect() {
    // A target may accept several item types, and generic code that states its
    // bounds collects into whatever its caller expects.
    let output = run(&lower(concat!(
        "use std.io.println\n",
        "use std.list.List\n",
        "use std.ops.(Add)\n",
        "use std.default.(Default)\n",
        "type Word wrap String\n",
        "impl FromIterator String Word {\n",
        "    insert = (move text; move word) => {\n",
        "        let Word inner = word\n",
        "        \"${text}[${inner}]\"\n",
        "    }\n",
        "}\n",
        "def integers = <Iter where Iterator Iter, Iterator.Item Iter == I32>\n",
        "    move iter: Iter => List<I32> :: iter |> collect\n",
        "def total = <Iter where Iterator Iter, Add (Iterator.Item Iter), Default (Iterator.Item Iter)>\n",
        "    move iter: Iter => Iterator.Item Iter :: iter |> sum\n",
        "def gather = <Iter, C where Iterator Iter, Default C, FromIterator C (Iterator.Item Iter)>\n",
        "    move iter: Iter => C :: iter |> collect\n",
        "def run = () => {\n",
        "    let words: String = (1 .. 3) |> map (x => Word \"w${x}\") |> collect\n",
        "    let pieces: String = (1 .. 3) |> map (x => \"p${x}\") |> collect\n",
        "    let extended = (1 .. 3) |> map (x => Word \"e${x}\") |> extend \"start\"\n",
        "    println \"${words} ${pieces} ${extended}\"\n",
        "    let gathered: String = gather ((1 .. 3) |> map (x => \"g${x}\"))\n",
        "    println \"${List.length (integers (1 .. 4))} ${total (1 .. 4)} ${gathered}\"\n",
        "}\n",
        "run ()\n",
    )));
    assert_eq!(output, "[w1][w2] p1p2 start[e1][e2]\n3 6 g1g2\n");
}
