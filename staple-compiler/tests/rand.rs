//! Behavior of `std.rand`: reference vectors, sampling bounds, resources,
//! collections and entropy.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};
use staple_syntax::Diagnostic;
use std::{
    path::Path,
    process::{Command, Output},
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

fn execute(source: &str) -> Output {
    let lowered = lower(source);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("staple-rand-{}-{nonce}", std::process::id()));
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
    output
}

#[cfg(unix)]
fn run(source: &str) -> String {
    let output = execute(source);
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
fn reference_vectors_and_sampling_are_stable() {
    let output = run(r#"
use std.rand.*
use std.io.println

def require = condition: Bool => () :: if (!condition) { panic "rand assertion failed" }

def check_sequence = (seed: U64; expected: List<U64>) => () :: {
    let mut rng = Rng.seeded seed
    let mut index: USize = 0
    while (index < List.length expected) {
        require (Rng.next_u64 rng == List.get_unchecked expected index)
        index = index + 1
    }
}

check_sequence 1 (List.of (
    12966619160104079557u64,
    9600361134598540522u64,
    10590380919521690900u64,
    7218738570589545383u64,
    12860671823995680371u64,
    2648436617965840162u64,
    1310552918490157286u64,
    7031611932980406429u64,
))
check_sequence 42 (List.of (
    1546998764402558742u64,
    6990951692964543102u64,
    12544586762248559009u64,
    17057574109182124193u64,
    18295552978065317476u64,
    14199186830065750584u64,
    13267978908934200754u64,
    15679888225317814407u64,
))

let mut jump_one = Rng.seeded 1
Rng.jump jump_one
require (Rng.next_u64 jump_one == 3686199559692413392)
let mut jump_forty_two = Rng.seeded 42
Rng.jump jump_forty_two
require (Rng.next_u64 jump_forty_two == 5766981335298035530)

let mut golden = Rng.seeded 42
require (Rng.between golden (1..=6) == 1)
let unit: F64 = Rng.random golden
require (unit >= 0.0 && unit < 1.0)

let mut ranges = Rng.seeded 7
let mut index: USize = 0
while (index < 1000) {
    let small = Rng.between ranges (U8.Min..=U8.Max)
    require (small >= U8.Min && small <= U8.Max)
    let wide = Rng.between ranges (I64.Min..=I64.Max)
    require (wide >= I64.Min && wide <= I64.Max)
    require (Rng.between ranges (5..=5) == 5)
    require (Rng.between ranges (0.0..1.0) >= 0.0)
    index = index + 1
}

let mut distribution_rng = Rng.seeded 9
require (Distribution.sample (Uniform.new (1..=6), distribution_rng) >= 1)
require (!Distribution.sample (Bernoulli.new 0.0, distribution_rng))
require (Distribution.sample (Bernoulli.new 1.0, distribution_rng))
let weighted = match (WeightedIndex.new (List.of (1.0, 0.0, 2.0))) {
    Ok value => value,
    _ => panic "weighted constructor failed",
}
require (Distribution.sample (weighted, distribution_rng) != 1)
let normal = match (Normal.new 0.0 1.0) {
    Ok value => value,
    _ => panic "normal constructor failed",
}
require (F64.is_finite (Distribution.sample (normal, distribution_rng)))
let exponential = match (Exponential.new 2.0) {
    Ok value => value,
    _ => panic "exponential constructor failed",
}
let exponential_value = Distribution.sample (exponential, distribution_rng)
require (F64.is_finite exponential_value && exponential_value >= 0.0)
let samples = Rng.sample_n distribution_rng (Uniform.new (0..10)) 16
require (List.length samples == 16)

let mut values: List<I32> = List.of (1, 2, 3, 4, 5)
List.swap values 0 4
require (List.get_unchecked values 0 == 5)
Rng.shuffle distribution_rng values
require (List.length values == 5)
let chosen = Rng.choose_multiple distribution_rng values 5
require (List.length chosen == 5)

let mut bytes: List<U8> = List.of (0u8 ^ 33)
Rng.fill_bytes distribution_rng bytes
require (List.length bytes == 33)

println "ok"
"#);
    assert_eq!(output, "ok\n");
}

#[cfg(unix)]
#[test]
fn resources_fork_and_entropy_work() {
    let output = run(r#"
use std.rand.*
use std.io.(IO, println)

def require = condition: Bool => () :: if (!condition) { panic "rand assertion failed" }

def resource_check = () => {
    with mut Rng = Rng.seeded 1 {
        let _ = next_u64 ()
        with mut Rng = Rng.seeded 42 {
            require (next_u64 () == Rng.next_u64 (Rng.seeded 42))
        }
    }
}

def fork_check = () => {
    let mut first = Rng.seeded 5
    let mut second = Rng.seeded 5
    let mut child_one = Rng.fork first
    let mut child_two = Rng.fork second
    require (Rng.next_u64 child_one == Rng.next_u64 child_two)
    require (Rng.next_u64 child_one != Rng.next_u64 first)
}

def check_size = size: USize =>{IO} () :: {
    let mut bytes: List<U8> = List.with_capacity size
    let mut index: USize = 0
    while (index < size) {
        List.push bytes 0u8
        index = index + 1
    }
    match (fill_entropy bytes) {
        Ok _ => require (List.length bytes == size),
        _ => panic "entropy failed",
    }
}

def entropy_check = () =>{IO} () :: {
    check_size 0
    check_size 1
    check_size 256
    check_size 1000
    let first = match (Rng.from_entropy ()) { Ok value => value, _ => panic "entropy failed" }
    let second = match (Rng.from_entropy ()) { Ok value => value, _ => panic "entropy failed" }
    let mut first = first
    let mut second = second
    require (Rng.next_u64 first != Rng.next_u64 second)
}

resource_check ()
fork_check ()
entropy_check ()
println "ok"
"#);
    assert_eq!(output, "ok\n");
}

#[test]
fn immutable_providers_are_rejected() {
    let diagnostics = check(
        r#"
use std.rand.*
with Rng = Rng.seeded 42 {
    next_u64 ()
}
"#,
    )
    .expect_err("an immutable provider cannot satisfy `mut Rng`");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("use `with mut`")),
        "{diagnostics:?}"
    );
}

#[cfg(unix)]
#[test]
fn empty_ranges_panic_at_the_caller() {
    let output = execute(
        r#"
use std.rand.*
let mut rng = Rng.seeded 1
Rng.between rng (5..5)
"#,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("between: empty range"), "{stderr}");
    assert!(stderr.contains("<stdin>.sta:4:"), "{stderr}");
}

#[test]
fn widening_multiply_returns_both_halves() {
    let output = run(r#"
use std.io.println

def require = condition: Bool => () :: if (!condition) { panic "widening assertion failed" }

let (low, high) = U8.widening_multiply 200 200
require (low == 64 && high == 156)
let (low, high) = U16.widening_multiply 65535 65535
require (low == 1 && high == 65534)
let (low, high) = U32.widening_multiply U32.Max U32.Max
require (low == 1 && high == 4294967294)
let (low, high) = U64.widening_multiply U64.Max U64.Max
require (low == 1 && high == 18446744073709551614)
// Exercises the carries between the 32-bit partial products.
let (low, high) = U64.widening_multiply 18446744069414584321 8589934591
require (low == 12884901887 && high == 8589934589)
let (low, high) = U64.widening_multiply 12345678901234567890 9876543210987654321
require (low == 133124662968603442 && high == 6609981178781634653)
let (low, high) = USize.widening_multiply 18446744073709551615 2
require (low == 18446744073709551614 && high == 1)
println "ok"
"#);
    assert_eq!(output, "ok\n");
}

// Golden values come from a reference implementation of xoshiro256**,
// SplitMix64 seeding, Lemire's sampler and the documented collection and
// weighted-index algorithms. A change here breaks seed stability.
#[test]
fn seeded_streams_match_golden_values() {
    let output = run(r#"
use std.rand.*
use std.io.println

def require = condition: Bool => () :: if (!condition) { panic "golden assertion failed" }

def draws = (seed: U64; low: I32; high: I32; inclusive: Bool) => List<I32> :: {
    let mut rng = Rng.seeded seed
    let mut values: List<I32> = List.new ()
    while (List.length values < 10) {
        let value = if (inclusive) { Rng.between rng (low..=high) } else { Rng.between rng (low..high) }
        List.push values value
    }
    values
}

require (draws 42 1 6 True == List.of (1, 3, 5, 6, 6, 5, 5, 6, 5, 4))
require (draws 42 (0 - 5) 5 False == List.of (0 - 5, 0 - 2, 1, 4, 4, 2, 2, 3, 2, 0))

let mut shuffle_rng = Rng.seeded 42
let mut deck: List<I32> = List.of (0, 1, 2, 3, 4, 5, 6, 7, 8, 9)
Rng.shuffle shuffle_rng deck
require (deck == List.of (9, 1, 4, 2, 8, 7, 6, 5, 3, 0))

let mut choose_rng = Rng.seeded 42
let picked = Rng.choose_multiple choose_rng (List.of (0, 1, 2, 3, 4, 5, 6, 7, 8, 9)) 3
require (picked == List.of (0, 4, 7))

let weighted = match (WeightedIndex.new (List.of (1.0, 0.0, 2.0, 0.0))) {
    Ok value => value,
    _ => panic "weighted constructor failed",
}
let mut weighted_rng = Rng.seeded 42
let indices = Rng.sample_n weighted_rng weighted 10
let expected_indices = List.of (0usize, 2usize, 2usize, 2usize, 2usize, 2usize, 2usize, 2usize, 2usize, 2usize)
require (indices == expected_indices)
println "ok"
"#);
    assert_eq!(output, "ok\n");
}

#[test]
fn bounded_sampling_is_unbiased() {
    let output = run(r#"
use std.rand.*
use std.io.println

def require = condition: Bool => () :: if (!condition) { panic "bias assertion failed" }

let mut rng = Rng.seeded 2026
let mut counts: List<I32> = List.of (0, 0, 0)
let mut index: I32 = 0
while (index < 300000) {
    let bucket = Rng.between rng ((0usize)..3)
    let _ = List.set counts bucket (List.get_unchecked counts bucket + 1)
    index = index + 1
}
let mut bucket: USize = 0
while (bucket < 3) {
    let count = List.get_unchecked counts bucket
    require (count > 99000 && count < 101000)
    bucket = bucket + 1
}
println "ok"
"#);
    assert_eq!(output, "ok\n");
}

#[test]
fn distributions_validate_their_parameters() {
    let output = run(r#"
use std.rand.*
use std.io.println

def require = condition: Bool => () :: if (!condition) { panic "distribution assertion failed" }

// Finite weights whose sum overflows are rejected.
require (match (WeightedIndex.new (List.of (1.0e308, 1.0e308, 1.0))) {
    DistributionError.InvalidParameter => True,
    _ => False,
})

// Rounding can make the target equal the total; a trailing zero weight is
// still never chosen.
let tiny = match (WeightedIndex.new (List.of (F64.from_bits 1, 0.0))) {
    Ok value => value,
    _ => panic "tiny constructor failed",
}
let mut rng = Rng.seeded 3
let mut index: I32 = 0
while (index < 1000) {
    require (Distribution.sample (tiny, rng) == 0)
    index = index + 1
}

let normal = match (Normal.new 1.5 2.0) {
    Ok value => value,
    _ => panic "normal constructor failed",
}
require (Normal.mean normal == 1.5 && Normal.std_dev normal == 2.0)
require (match (Normal.new 0.0 (0.0 - 1.0)) { DistributionError.InvalidParameter => True, _ => False })
let exponential = match (Exponential.new 4.0) {
    Ok value => value,
    _ => panic "exponential constructor failed",
}
require (Exponential.rate exponential == 4.0)
require (match (Exponential.new 0.0) { DistributionError.InvalidParameter => True, _ => False })
println "ok"
"#);
    assert_eq!(output, "ok\n");
}

#[test]
fn distributions_cannot_be_built_without_validation() {
    for source in [
        "use std.rand.*\nlet normal = Normal (mean: 0.0, std_dev: 0.0 - 1.0)\n",
        "use std.rand.*\nlet exponential = Exponential (rate: 0.0)\n",
    ] {
        let diagnostics = check(source).expect_err("distribution fields are private");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("is not a value")),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn choose_multiple_clones_only_chosen_elements() {
    let output = run(r#"
use std.rand.*
use std.io.println

def require = condition: Bool => () :: if (!condition) { panic "choose assertion failed" }

let words: List<String> = List.of ("alpha", "beta", "gamma", "delta")
let mut rng = Rng.seeded 11
let chosen = Rng.choose_multiple rng words 2
require (List.length chosen == 2)
require (List.length words == 4)
let all = Rng.choose_multiple rng words 10
require (List.length all == 4)
println "ok"
"#);
    assert_eq!(output, "ok\n");
}

#[cfg(unix)]
#[test]
fn entropy_writes_bytes() {
    let output = run(r#"
use std.rand.*
use std.io.println

let mut bytes: List<U8> = List.of (0u8 ^ 64)
match (fill_entropy bytes) {
    Ok _ => (),
    _ => panic "entropy failed",
}
let mut nonzero: USize = 0
let mut index: USize = 0
while (index < 64) {
    if (List.get_unchecked bytes index != 0) { nonzero = nonzero + 1 }
    index = index + 1
}
// 64 zero bytes from the OS has probability 2^-512.
if (nonzero == 0) { panic "entropy left the buffer unwritten" }
println "ok"
"#);
    assert_eq!(output, "ok\n");
}

#[cfg(unix)]
#[test]
fn float_ranges_reject_infinite_spans() {
    for range in ["(0.0 - F64.Max)..F64.Max", "0.0..(F64.Max * 2.0)"] {
        let output = execute(&format!(
            "use std.rand.*\nlet mut rng = Rng.seeded 1\nlet value = Rng.between rng ({range})\n"
        ));
        assert!(!output.status.success(), "{range}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("between: float range must have a finite span"), "{stderr}");
    }
}

#[cfg(unix)]
#[test]
fn uniform_rejects_empty_ranges_at_construction() {
    let output = execute(
        r#"
use std.rand.*
let uniform = Uniform.new (5..5)
"#,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Uniform.new: range cannot be sampled"), "{stderr}");
    assert!(stderr.contains("<stdin>.sta:3:"), "{stderr}");
}
