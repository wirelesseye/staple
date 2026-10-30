//! Recursive types whose back-edge passes through an out-of-line
//! indirection, including recursive sums and mutually recursive declarations.

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
    let resolved = NameResolver::new().resolve_program(program)?;
    TypeChecker::new().check(resolved)
}

fn check(source: &str) -> staple_compiler::TypedModule {
    check_result(source).unwrap_or_else(|errors| panic!("check: {errors:?}"))
}

fn check_errors(source: &str) -> Vec<String> {
    match check_result(source) {
        // Never format the `Ok` side: `TypedModule`'s debug tree is huge.
        Ok(_) => panic!("source should fail checking"),
        Err(errors) => errors
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect(),
    }
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
        "staple-recursive-types-{}-{nonce}",
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
fn recursive_typegroup_through_list_and_dict_runs() {
    let output = run(r#"
use std.io.(IO, println)

pub typegroup Value {
    Number wrap pub I32,
    Items wrap pub List<Value>,
    Fields wrap pub Dict<String, Value>,
}

def total = move value: Value => I32 :: match value {
    Value.Number n => n,
    Value.Items items => {
        let mut sum: I32 = 0
        for item in items { sum = sum + total item }
        sum
    },
    Value.Fields fields => {
        let mut sum: I32 = 0
        for (_, item) in fields { sum = sum + total item }
        sum
    },
}

def main = () =>{IO} () :: {
    let mut inner: List<Value> = List.new ()
    List.push inner (Value.Number 10)
    List.push inner (Value.Number 20)
    let mut fields: Dict<String, Value> = Dict.new ()
    let _ = Dict.insert fields "a" (Value.Items inner)
    let _ = Dict.insert fields "b" (Value.Number 5)
    let mut outer: List<Value> = List.new ()
    List.push outer (Value.Fields fields)
    List.push outer (Value.Number 1)
    println "${total (Value.Items outer)}"
}

main ()
"#);
    assert_eq!(output, "36\n");
}

#[test]
fn mutually_recursive_wrappers_through_an_indirection_check_in_any_order() {
    // Whichever of `Node` and `Tree` is resolved first, the reference back to
    // it passes through `List`, and the payload a pattern extracts is the real
    // type rather than a placeholder.
    for (declarations, leaf, node) in [
        (
            "type Leaf wrap I32\ntype Node wrap List<Tree>\ntype Tree from Leaf | Node\n",
            "Leaf n",
            "Node children",
        ),
        (
            "type Leaf wrap I32\ntype Tree from Leaf | Node\ntype Node wrap List<Tree>\n",
            "Leaf n",
            "Node children",
        ),
        (
            "type Leaf wrap I32\ntype Node wrap List<Tree>\ntype Tree wrap (Leaf | Node)\n",
            "Tree (Leaf n)",
            "Tree (Node children)",
        ),
    ] {
        check(&format!(
            r#"{declarations}
def size = move tree: Tree => I32 :: match tree {{
    {leaf} => n,
    {node} => {{
        let mut total: I32 = 0
        for child in children {{ total = total + size child }}
        total
    }},
}}
"#
        ));
    }
}

#[test]
fn self_recursive_sum_alternative_matches_its_written_type() {
    check(
        r#"
type Tree wrap (I32 | List<Tree>)

def size = move tree: Tree => I32 :: {
    let Tree value = tree
    match value {
        n: I32 => n,
        children: List<Tree> => {
            let mut total: I32 = 0
            for child in children { total = total + size child }
            total
        },
    }
}
"#,
    );
}

#[test]
fn mutually_recursive_types_without_an_indirection_are_still_cyclic() {
    let errors = check_errors("type A wrap B\ntype B from (I32 | A)\n");
    assert!(
        errors
            .iter()
            .any(|message| message.starts_with("cyclic type definition")),
        "unexpected diagnostics: {errors:#?}"
    );
    let errors = check_errors(
        "type Pair<T> wrap (first: T, second: I32)\ntype Loop wrap (pair: Pair<Loop>)\n",
    );
    assert!(
        errors
            .iter()
            .any(|message| message.starts_with("cyclic type definition")),
        "unexpected diagnostics: {errors:#?}"
    );
}
