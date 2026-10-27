//! Tests for `std.hash`: the `Hash` trait, `Hasher`, keys and `@derive_hash`.

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
    let base = std::env::temp_dir().join(format!("staple-hash-{}-{nonce}", std::process::id()));
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
fn hash_contract_covers_equality_tags_keys_derives_and_distribution() {
    let output = run(r#"
use std.hash.*
use std.list.List
use std.shared.Shared
use std.char.Char
use std.cmp.Ordering
use std.fmt.derive_debug
use std.rand.Rng
use std.convert.TryConvert
use std.io.(IO, println)

@derive_hash
@derive_eq
@derive_debug
type Point wrap (x: I32, y: I32)

@derive_eq
@derive_hash
type Pair from (I32, Bool)

@derive_eq
@derive_hash
type Enabled

@derive_eq
@derive_hash
type Box<T> wrap T

@derive_eq
@derive_hash
type Nested wrap (value: (I32, String), flag: Bool)

def show = value: Bool => println (if value { "True" } else { "False" })

def main = () =>{IO} () :: {
    // Equal values of every explicit implementation hash alike.
    let first_string = "same"
    let second_string = "sa" + "me"
    show (hash_of first_string == hash_of second_string)

    let mut first_list: List<I32> = List.with_capacity 1
    List.push first_list 1
    List.push first_list 2
    let mut second_list: List<I32> = List.with_capacity 8
    List.push second_list 1
    List.push second_list 2
    show (hash_of first_list == hash_of second_list)

    let first_shared = Shared.new 5
    let second_shared = Shared.new 5
    show (hash_of first_shared == hash_of second_shared)

    let first_some: Option<I32> = Some 3
    let second_some: Option<I32> = Some 3
    show (hash_of first_some == hash_of second_some)

    let first_char = (Char.from_u32 65) |> unwrap
    let second_char = (Char.from_u32 65) |> unwrap
    show (hash_of first_char == hash_of second_char)

    let first_ok: Result<I32, String> = Ok 1
    let second_ok: Result<I32, String> = Ok 1
    show (hash_of first_ok == hash_of second_ok)

    // Variable-length values are prefix-free.
    show (hash_of ("ab", "c") != hash_of ("a", "bc"))
    show (hash_of ("", "x") != hash_of ("x", ""))
    show (hash_of (List.of (97u8, 98u8, 99u8)) != hash_of (List.of (97u8, 98u8)))
    let left_bytes: List<U8> = List.of (97u8, 98u8)
    let right_bytes: List<U8> = List.of (99u8)
    let prefixed_bytes: List<U8> = List.of (97u8)
    let suffix_bytes: List<U8> = List.of (98u8, 99u8)
    show (hash_of (left_bytes, right_bytes) != hash_of (prefixed_bytes, suffix_bytes))

    // Sums write their tag before the payload.
    let left_sum: I32 | U32 = I32 :: 1
    let right_sum: I32 | U32 = U32 :: 1
    show (hash_of left_sum != hash_of right_sum)

    let yes: Bool = True
    let no: Bool = False
    show (hash_of yes != hash_of no)

    let less: Ordering = Ordering.Less
    let greater: Ordering = Ordering.Greater
    show (hash_of less != hash_of greater)

    let none: Option<I32> = None
    let some: Option<I32> = Some 0
    show (hash_of none != hash_of some)

    // Incremental writes match the structural concatenation.
    let mut incremental = Hasher.new ()
    Hash.hash ("a", incremental)
    Hash.hash ("b", incremental)
    let mut structural = Hasher.new ()
    let pair: (String, String) = ("a", "b")
    Hash.hash (pair, structural)
    show (Hasher.finish incremental == Hasher.finish structural)

    // The empty product writes nothing.
    let mut empty_hasher = Hasher.new ()
    let empty_hash = Hasher.finish empty_hasher
    show (hash_of () == empty_hash)

    // SipHash-1-3 over one zero block and an 8-byte length with the fixed key.
    let mut reference = Hasher.new ()
    Hasher.write_u64 reference 0
    show (Hasher.finish reference == 2360399714353584901)

    // Keys are reproducible or come from OS entropy.
    let mut first_rng = Rng.seeded 42
    let first_key = HashKey.from_rng first_rng
    let mut second_rng = Rng.seeded 42
    let second_key = HashKey.from_rng second_rng
    let mut third_rng = Rng.seeded 43
    let third_key = HashKey.from_rng third_rng
    show (hash_with first_key "same" == hash_with second_key "same")
    show (hash_with first_key "same" != hash_with third_key "same")

    let entropy_first = match (HashKey.from_entropy ()) {
        Ok key => key,
        _ => HashKey.fixed (),
    }
    let entropy_second = match (HashKey.from_entropy ()) {
        Ok key => key,
        _ => HashKey.fixed (),
    }
    show (hash_with entropy_first "same" != hash_with entropy_second "same")

    // Derived representations hash their fields; singletons write nothing.
    let point = Point (x: 1, y: 2)
    let same_point = Point (x: 1, y: 2)
    show (hash_of point == hash_of same_point)
    show (hash_of point != hash_of (Point (x: 1, y: 3)))
    show (hash_of (Pair (1, True)) == hash_of (Pair (1, True)))
    show (hash_of Enabled == hash_of Enabled)
    show (hash_of (Box 3) == hash_of (Box 3))
    show (hash_of (Box 3) != hash_of (Box 4))
    show (hash_of (Nested ((1, "a"), True)) == hash_of (Nested ((1, "a"), True)))

    // A distribution smoke test over the low ten bits.
    let mut buckets: List<U32> = List.with_capacity 1024
    let mut index: USize = 0
    while (index < 1024) {
        List.push buckets 0u32
        index = index + 1
    }
    let mut value: U64 = 0
    while (value < 4096) {
        let hash = hash_of value
        let bucket_value: U64 = hash % 1024u64
        let bucket = match (TryConvert.try_convert.<U64, USize> bucket_value) {
            Ok bucket => bucket,
            _ => 0usize,
        }
        List.set buckets bucket (List.get_unchecked buckets bucket + 1)
        value = value + 1
    }
    let mut maximum: U32 = 0
    let mut index: USize = 0
    while (index < 1024) {
        let count = List.get_unchecked buckets index
        if (count > maximum) { maximum = count }
        index = index + 1
    }
    show (maximum < 32)
}
main ()
"#);
    assert_eq!(output, "True\n".repeat(28));
}

#[test]
fn hash_rejections_cover_unhashable_values_derives_and_structural_impls() {
    // Floats are deliberately not `Hash`.
    for source in [
        "let value = 1.5\nlet hash = hash_of value\n",
        "let value = (1, 2.0)\nlet hash = hash_of value\n",
        "let value: Option<F64> = Some 2.0\nlet hash = hash_of value\n",
    ] {
        let messages = check_errors(source);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("no trait implementation")
                    || message.contains("trait bound")),
            "{messages:?}"
        );
    }

    // `Hash` requires `Eq`.
    let messages =
        check_errors("type Local wrap I32\nimpl Hash Local { hash = (value, mut hasher) => () }\n");
    assert!(
        messages
            .iter()
            .any(|message| message.contains("Eq") || message.contains("prerequisite")),
        "{messages:?}"
    );

    // Products and sums with structural `Hash` cannot be implemented explicitly.
    let messages = check_errors(
        "@derive_eq\n\
         @derive_hash\n\
         type Local wrap I32\n\
         impl Hash (Local, Local) { hash = (value, mut hasher) => () }\n",
    );
    assert!(
        messages.iter().any(|message| message
            .contains("`Hash` is derived structurally for this product or sum type")),
        "{messages:?}"
    );

    // A generic derive needs a hashable payload.
    let messages = check_errors(
        "type Local wrap I32\n\
         @derive_eq\n\
         @derive_hash\n\
         type Box<T> wrap T\n\
         let value = Box (Local 1)\n\
         let hash = hash_of value\n",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("no trait implementation")
                || message.contains("trait bound")),
        "{messages:?}"
    );

    // Unsupported derive items.
    for source in [
        "@derive_hash\ntype Alias = I32\n",
        "@derive_hash\ndef value = 1\n",
    ] {
        let messages = check_errors(source);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("derive_hash item is unsupported")),
            "{messages:?}"
        );
    }

    // Entropy needs `IO`; an explicitly pure function cannot read it.
    let messages = check_errors(
        "use std.hash.HashKey\n\
         def key: () -> HashKey = () => HashKey :: match (HashKey.from_entropy ()) {\n\
             Ok key => key,\n\
             _ => HashKey.fixed (),\n\
         }\n",
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("IO") || message.contains("effect")),
        "{messages:?}"
    );
}
