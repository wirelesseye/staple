//! Tests for `std.dict`: `Dict` lookups, growth, removal and trait impls.

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
    let base = std::env::temp_dir().join(format!("staple-dict-{}-{nonce}", std::process::id()));
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
fn dict_inserts_finds_replaces_and_grows_under_any_key() {
    let output = run(r#"
use std.hash.HashKey
use std.rand.Rng
use std.io.(IO, println)

def show = value: Bool => println (if value { "True" } else { "False" })

def exercise = key: HashKey =>{IO} () :: {
    let mut ages: Dict<String, I32> = Dict.with_key key
    show (Dict.is_empty ages)
    show (Dict.insert ages "ada" 36 == None)
    show (Dict.insert ages "alan" 41 == None)
    show (Dict.insert ages "ada" 37 == Some 36)
    show (Dict.length ages == 2)
    show (Dict.get ages "ada" == Some 37)
    show (Dict.get ages "grace" == None)
    show (Dict.contains_key ages "alan")
    show (!(Dict.contains_key ages "grace"))

    // Growing through many resizes keeps every entry reachable.
    let mut numbers: Dict<I32, I32> = Dict.with_key key
    let mut found: Bool = True
    let mut index: I32 = 0
    while (index < 1000) {
        let _ = Dict.insert numbers index (index * 2)
        index = index + 1
    }
    show (Dict.length numbers == 1000)
    show (Dict.capacity numbers >= 1000)
    index = 0
    while (index < 1000) {
        if (Dict.get numbers index != Some (index * 2)) { found = False }
        index = index + 1
    }
    show found
}

def main = () =>{IO} () :: {
    exercise (HashKey.fixed ())
    let mut rng = Rng.seeded 7
    exercise (HashKey.from_rng rng)

    // Pre-sizing avoids growth.
    let sized: Dict<I32, I32> = Dict.with_capacity 20
    show (Dict.capacity sized >= 20)
    let empty: Dict<I32, I32> = Dict.new ()
    show (Dict.capacity empty == 0)
}
main ()
"#);
    assert_eq!(output, "True\n".repeat(26));
}

#[cfg(unix)]
#[test]
fn dict_removal_keeps_probe_runs_intact() {
    let output = run(r#"
use std.io.(IO, println)

def show = value: Bool => println (if value { "True" } else { "False" })

def main = () =>{IO} () :: {
    let count: I32 = 300
    let mut numbers: Dict<I32, I32> = Dict.new ()
    let mut index: I32 = 0
    while (index < count) {
        let _ = Dict.insert numbers index (index + 1000)
        index = index + 1
    }

    // Remove in a scrambled order, checking every key after each removal so
    // backward shifting and the swap-remove slot fixup are both exercised.
    let mut consistent: Bool = True
    let mut step: I32 = 0
    while (step < count) {
        let victim = (step * 7 + 3) % count
        if (Dict.remove numbers victim != Some (victim + 1000)) { consistent = False }
        if (Dict.remove numbers victim != None) { consistent = False }
        let mut probe: I32 = 0
        while (probe < count) {
            let removed = ((probe - 3 + count) * 43) % count < step + 1
            let expected = if removed { None } else { Some (probe + 1000) }
            if (Dict.get numbers probe != expected) { consistent = False }
            probe = probe + 1
        }
        step = step + 1
    }
    show consistent
    show (Dict.is_empty numbers)

    // The emptied table is reusable.
    let _ = Dict.insert numbers 5 6
    show (Dict.get numbers 5 == Some 6)
    Dict.clear numbers
    show (Dict.is_empty numbers)
    show (Dict.get numbers 5 == None)
    let _ = Dict.insert numbers 5 7
    show (Dict.get numbers 5 == Some 7)
}
main ()
"#);
    assert_eq!(output, "True\n".repeat(6));
}

#[cfg(unix)]
#[test]
fn dict_moves_non_copy_entries_and_implements_standard_traits() {
    let output = run(r#"
use std.io.(IO, println)
use std.iterator.IterStep.(Done, Yield)

def show = value: Bool => println (if value { "True" } else { "False" })

def main = () =>{IO} () :: {
    // Non-`Copy` keys and values move in and out.
    let mut lists: Dict<String, List<I32>> = Dict.new ()
    let _ = Dict.insert lists "a" (List.of (1, 2))
    let _ = Dict.insert lists "b" (List.of (3))
    let _ = Dict.insert lists "c" (List.of ())
    let a_list: List<I32> = List.of (1, 2)
    let b_list: List<I32> = List.of (3)
    let empty_list: List<I32> = List.of ()
    let a_entry: Option<List<I32>> = Some a_list
    let b_entry: Option<(String, List<I32>)> = Some ("b", b_list)
    let c_entry: Option<List<I32>> = Some empty_list
    show (Dict.get_cloned lists "a" == a_entry)
    show (Dict.remove_entry lists "b" == b_entry)
    show (Dict.remove lists "b" == None)
    show (Dict.insert lists "c" (List.of (4)) == c_entry)

    // Clones are independent.
    let copy = Clone.clone lists
    let _ = Dict.insert lists "d" (List.of (5))
    show (Dict.length lists == 3 && Dict.length copy == 2)
    show (!(Dict.contains_key copy "d"))

    // Consuming iteration yields every entry once.
    let mut remaining = IntoIterator.into_iterator lists
    let mut total: I32 = 0
    let mut entries: USize = 0
    loop {
        match Iterator.next remaining {
            Done _ => { break },
            Yield (entry, rest) => {
                remaining = rest
                let (_, list) = entry
                total = total + (IntoIterator.into_iterator list |> fold 0 ((sum, value) => sum + value))
                entries = entries + 1
            },
        }
    }
    show (entries == 3 && total == 12)

    // Collecting lets later duplicates win; equality ignores order.
    let squares: Dict<I32, I32> = (1 .. 6) |> map (x => (x, x * x)) |> collect
    let reversed: Dict<I32, I32> = (1 .. 6) |> map (x => (6 - x, (6 - x) * (6 - x))) |> collect
    show (squares == reversed)
    let pairs: List<(I32, I32)> = List.of ((1, 1), (1, 2))
    let duplicates: Dict<I32, I32> = IntoIterator.into_iterator pairs |> collect
    show (Dict.length duplicates == 1 && duplicates[1] == 2)

    // Indexing reads and writes.
    let mut changed = Clone.clone squares
    changed[3] = 10
    changed[9] = 81
    show (squares != changed)
    show (changed[3] == 10 && changed[9] == 81 && squares[3] == 9)

    let empty: Dict<I32, I32> = Default.default ()
    show (empty == Dict.new ())
}
main ()
"#);
    assert_eq!(output, "True\n".repeat(12));
}

#[test]
fn dict_rejects_unhashable_keys() {
    let messages = check_errors(
        "let mut values: Dict<F64, I32> = Dict.new ()\n\
         let previous = Dict.insert values 1.5 2\n",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("no trait implementation")
                || message.contains("trait bound")),
        "{messages:?}"
    );
}
