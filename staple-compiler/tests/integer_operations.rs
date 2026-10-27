//! Runtime, panic, IR, folding, and pipe tests for the integer companions.

mod common;

// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};
use staple_syntax::{Diagnostic, Item, parse};
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
    let base = std::env::temp_dir().join(format!("staple-integer-{}-{nonce}", std::process::id()));
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
fn run_lines(source: &str) -> Vec<String> {
    run(source).lines().map(str::to_owned).collect()
}

fn compile_ir(source: &str) -> String {
    let typed = check(source).unwrap_or_else(|errors| panic!("check: {errors:?}"));
    let lowered = Lowerer::new()
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&lowered)
        .unwrap_or_else(|errors| panic!("compile: {errors:?}"))
}

fn folded_value(module: &staple_compiler::TypedModule, name: &str) -> staple_syntax::Expression {
    module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding(binding) if binding.name == name => binding.value.clone(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("binding `{name}` should exist"))
}

#[cfg(unix)]
#[test]
fn i8_edges_cover_the_toolbox() {
    let source = r#"
use std.io.println
println "${I8.Min} ${I8.Max} ${I8.Bits}"
println "${I8.min 3 ((I8 :: 0) - 4)} ${I8.max 3 ((I8 :: 0) - 4)} ${I8.clamp 9 ((I8 :: 0) - 4) 5}"
println "${I8.abs_diff I8.Min 127} ${I8.abs_diff 5 3}"
match (I8.checked_add 127 1) {
    Some value => println "${value}",
    None => println "None",
}
match (I8.checked_subtract I8.Min 1) {
    Some value => println "${value}",
    None => println "None",
}
match (I8.checked_multiply 100 2) {
    Some value => println "${value}",
    None => println "None",
}
println "${I8.wrapping_add 127 1} ${I8.wrapping_subtract I8.Min 1} ${I8.wrapping_multiply 100 2}"
println "${I8.saturating_add 127 1} ${I8.saturating_subtract I8.Min 1} ${I8.saturating_multiply ((I8 :: 0) - 100) 2}"
let (sum, carried) = I8.overflowing_add 127 1
println "${sum} ${carried}"
println "${I8.divide_euclid ((I8 :: 0) - 7) 3} ${I8.remainder_euclid ((I8 :: 0) - 7) 3}"
println "${I8.divide_euclid 7 ((I8 :: 0) - 3)} ${I8.remainder_euclid 7 ((I8 :: 0) - 3)}"
let powered = I8.pow 2 7
match (I8.checked_pow 2 7) {
    Some value => println "${powered} ${value} ${I8.saturating_pow 2 7}",
    None => println "${powered} None ${I8.saturating_pow 2 7}",
}
println "${I8.ilog2 8} ${I8.ilog10 100} ${I8.isqrt 120}"
println "${I8.count_ones ((I8 :: 0) - 1)} ${I8.count_zeros 0} ${I8.leading_zeros 1} ${I8.trailing_zeros 8}"
println "${I8.leading_ones ((I8 :: 0) - 8)} ${I8.trailing_ones 7}"
println "${I8.rotate_left 1 1} ${I8.rotate_right 1 1} ${I8.swap_bytes 1} ${I8.reverse_bits 1}"
println "${I8.abs I8.Min} ${I8.unsigned_abs I8.Min}"
match (I8.checked_abs I8.Min) {
    Some value => println "${value}",
    None => println "None",
}
match (I8.checked_negate I8.Min) {
    Some value => println "${value}",
    None => println "None",
}
println "${I8.wrapping_negate I8.Min} ${I8.signum ((I8 :: 0) - 5)} ${I8.signum 0} ${I8.signum 5}"
println "${I8.is_negative ((I8 :: 0) - 1)} ${I8.is_positive 1}"
"#;
    assert_eq!(
        run_lines(source),
        [
            "-128 127 8",
            "-4 3 5",
            "255 2",
            "None",
            "None",
            "None",
            "-128 127 -56",
            "127 -128 -128",
            "-128 True",
            "-3 2",
            "-2 1",
            "-128 None 127",
            "3 2 10",
            "8 8 7 3",
            "5 3",
            "2 -128 1 -128",
            "-128 128",
            "None",
            "None",
            "-128 -1 0 1",
            "True True",
        ]
    );
}

#[cfg(unix)]
#[test]
fn u8_edges_cover_the_toolbox() {
    let source = r#"
use std.io.println
println "${U8.Min} ${U8.Max} ${U8.Bits}"
println "${U8.min 3 4} ${U8.max 3 4} ${U8.clamp 9 4 5}"
println "${U8.abs_diff 3 5} ${U8.abs_diff 9 2}"
match (U8.checked_add 255 1) {
    Some value => println "${value}",
    None => println "None",
}
match (U8.checked_subtract 0 1) {
    Some value => println "${value}",
    None => println "None",
}
match (U8.checked_multiply 100 3) {
    Some value => println "${value}",
    None => println "None",
}
println "${U8.wrapping_add 255 1} ${U8.wrapping_subtract 0 1} ${U8.wrapping_multiply 100 3}"
println "${U8.saturating_add 255 1} ${U8.saturating_subtract 0 1} ${U8.saturating_multiply 200 2}"
let (difference, borrowed) = U8.overflowing_subtract 0 1
println "${difference} ${borrowed}"
println "${U8.divide_euclid 7 3} ${U8.remainder_euclid 7 3} ${U8.divide_ceil 7 3}"
let powered = U8.pow 2 8
match (U8.checked_pow 2 8) {
    Some value => println "${powered} ${value} ${U8.saturating_pow 2 8}",
    None => println "${powered} None ${U8.saturating_pow 2 8}",
}
println "${U8.ilog2 255} ${U8.ilog10 255} ${U8.isqrt 255}"
println "${U8.count_ones 255} ${U8.count_zeros 0} ${U8.leading_zeros 1} ${U8.trailing_zeros 8}"
println "${U8.leading_ones 255} ${U8.trailing_ones 255}"
println "${U8.rotate_left 128 1} ${U8.rotate_right 1 1} ${U8.swap_bytes 1} ${U8.reverse_bits 1}"
println "${U8.is_power_of_two 128} ${U8.is_power_of_two 0} ${U8.next_power_of_two 129}"
match (U8.checked_next_power_of_two 129) {
    Some value => println "${value}",
    None => println "None",
}
match (U8.checked_add_signed 255 1) {
    Some value => println "${value}",
    None => println "None",
}
match (U8.checked_add_signed 5 ((I8 :: 0) - 3)) {
    Some value => println "${value}",
    None => println "None",
}
"#;
    assert_eq!(
        run_lines(source),
        [
            "0 255 8",
            "3 4 5",
            "2 7",
            "None",
            "None",
            "None",
            "0 255 44",
            "255 0 255",
            "255 True",
            "2 1 3",
            "0 None 255",
            "7 2 15",
            "8 8 7 3",
            "8 8",
            "1 128 1 128",
            "True False 0",
            "None",
            "None",
            "2",
        ]
    );
}

#[cfg(unix)]
#[test]
fn i64_and_u64_edges_cover_constants_and_wide_bits() {
    let source = r#"
use std.io.println
println "${I64.Min} ${I64.Max} ${I64.Bits}"
match (I64.checked_add 9223372036854775807 1) {
    Some value => println "${value}",
    None => println "None",
}
println "${I64.wrapping_add 9223372036854775807 1} ${I64.saturating_add I64.Min ((I64 :: 0) - 1)}"
println "${I64.count_ones ((I64 :: 0) - 1)} ${I64.rotate_left 1 63} ${I64.reverse_bits 1}"
println "${I64.ilog2 9223372036854775807} ${I64.isqrt 9223372036854775807}"
println "${U64.Min} ${U64.Max} ${U64.Bits}"
match (U64.checked_add 18446744073709551615 1) {
    Some value => println "${value}",
    None => println "None",
}
println "${U64.wrapping_add 18446744073709551615 1} ${U64.saturating_add 18446744073709551615 1}"
println "${U64.next_power_of_two 18446744073709551615}"
match (U64.checked_next_power_of_two 18446744073709551615) {
    Some value => println "${value}",
    None => println "None",
}
println "${U64.count_ones 18446744073709551615} ${U64.rotate_left 1 63} ${U64.reverse_bits 1}"
println "${U64.ilog2 18446744073709551615} ${U64.isqrt 18446744073709551615}"
"#;
    assert_eq!(
        run_lines(source),
        [
            "-9223372036854775808 9223372036854775807 64",
            "None",
            "-9223372036854775808 -9223372036854775808",
            "64 -9223372036854775808 -9223372036854775808",
            "62 3037000499",
            "0 18446744073709551615 64",
            "None",
            "0 18446744073709551615",
            "0",
            "None",
            "64 9223372036854775808 9223372036854775808",
            "63 4294967295",
        ]
    );
}

#[cfg(unix)]
#[test]
fn out_of_range_shifts_and_other_failures_report_the_caller() {
    for (program, message, line) in [
        (
            "use std.io.println\nuse std.ops.ShiftLeft\nlet value = ShiftLeft.shift_left ((I32 :: 1); 32)\n",
            "shift count out of range",
            3,
        ),
        (
            "use std.io.println\nuse std.ops.ShiftLeft\nlet value = ShiftLeft.shift_left ((I32 :: 1); (I32 :: 0) - 1)\n",
            "shift count out of range",
            3,
        ),
        (
            "use std.io.println\nlet value = I32.clamp 1 5 4\n",
            "clamp: invalid bounds",
            2,
        ),
        (
            "use std.io.println\nlet value = I32.divide_euclid 7 0\n",
            "division by zero",
            2,
        ),
        (
            "use std.io.println\nlet value = I32.ilog2 0\n",
            "ilog2: non-positive value",
            2,
        ),
        (
            "use std.io.println\nlet value = I32.isqrt ((I32 :: 0) - 1)\n",
            "isqrt: negative value",
            2,
        ),
    ] {
        let output = execute(program);
        assert!(!output.status.success(), "{program}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{program}: {stderr}");
        assert!(
            stderr.contains(&format!("<stdin>.sta:{line}:")),
            "{program}: {stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn shift_at_the_width_boundary_runs() {
    assert_eq!(
        run_lines(
            "use std.io.println\nuse std.ops.(ShiftLeft, ShiftRight)\nprintln \"${ShiftLeft.shift_left ((I32 :: 1); 31)}\"\nprintln \"${ShiftRight.shift_right (((I32 :: 0) - 1); 31)}\"\n"
        ),
        ["-2147483648", "-1"]
    );
}

#[cfg(unix)]
#[test]
fn pointer_width_companions_use_the_target_width_at_run_time() {
    let source = r#"
use std.io.println
println "${ISize.count_ones ((ISize :: 0) - 1)}"
println "${ISize.ilog2 255}"
println "${ISize.wrapping_shift_left (ISize :: 1) 63}"
println "${ISize.saturating_add 9223372036854775807 1}"
println "${USize.next_power_of_two 18446744073709551615}"
match (USize.checked_add_signed 5 ((ISize :: 0) - 3)) {
    Some value => println "${value}",
    None => println "None",
}
"#;
    assert_eq!(
        run_lines(source),
        [
            "64",
            "7",
            "-9223372036854775808",
            "9223372036854775807",
            "0",
            "2",
        ]
    );
}

#[cfg(unix)]
#[test]
fn integer_intrinsics_emit_llvm_bit_and_overflow_operations() {
    let ir = compile_ir(
        r#"
let a = I32.overflowing_add 1 2
let b = U32.overflowing_multiply 3 4
let c = U32.count_ones 255
let d = U32.leading_zeros 1
let e = U8.swap_bytes 1
let f = U32.swap_bytes 1
let g = U8.rotate_left 1 1
"#,
    );
    for intrinsic in [
        "llvm.sadd.with.overflow",
        "llvm.umul.with.overflow",
        "llvm.ctpop",
        "llvm.ctlz",
        "llvm.fshl",
        "llvm.bswap",
    ] {
        assert!(
            ir.contains(intrinsic),
            "expected `{intrinsic}` in emitted IR:\n{ir}"
        );
    }
    assert!(
        !ir.contains("llvm.bswap.i8"),
        "U8 byte swap must be an identity:\n{ir}"
    );
}

#[cfg(unix)]
#[test]
fn same_width_sign_casts_run_at_the_type_edges() {
    let source = r#"
use std.io.println

println "${U8.cast_signed 255}"
println "${U8.cast_signed 128}"
println "${U8.cast_signed 127}"
println "${I8.cast_unsigned I8.Min}"
println "${I8.cast_unsigned I8.Max}"
println "${U16.cast_signed U16.Max}"
println "${I16.cast_unsigned I16.Min}"
println "${U32.cast_signed U32.Max}"
println "${I32.cast_unsigned I32.Min}"
println "${U64.cast_signed U64.Max}"
println "${I64.cast_unsigned I64.Min}"

def round_trip = (value: I32) => I32 :: value |> cast_unsigned |> cast_signed
println "${round_trip I32.Min}"
println "${round_trip ((I32 :: 0) - 1)}"
println "${round_trip 0}"
println "${round_trip I32.Max}"

println "${USize.cast_signed 1}"
println "${ISize.cast_unsigned 1}"
"#;
    assert_eq!(
        run_lines(source),
        [
            "-1",
            "-128",
            "127",
            "128",
            "127",
            "-1",
            "32768",
            "-1",
            "2147483648",
            "-1",
            "9223372036854775808",
            "-2147483648",
            "-1",
            "0",
            "2147483647",
            "1",
            "1",
        ]
    );
}

#[test]
fn same_width_sign_casts_emit_no_conversion_instruction() {
    let ir = compile_ir(
        r#"
let value: U32 = 0xFFFF_FFFF
let signed = U32.cast_signed value
let unsigned = I32.cast_unsigned (I32 :: 0)
"#,
    );
    for name in [
        "__staple_mstd.number.types.U32.cast_signed",
        "__staple_mstd.number.types.I32.cast_unsigned",
    ] {
        let body = function_definition(&ir, name);
        for instruction in ["zext", "sext", "trunc", "call"] {
            assert!(
                !body.contains(instruction),
                "a sign cast must not emit `{instruction}`:\n{body}"
            );
        }
    }
    assert!(
        !ir.contains("__u32_cast_sign") && !ir.contains("__i32_cast_sign"),
        "the sign-cast intrinsics must not survive into IR"
    );
}

/// Extracts one `define` body, from its `define` to the matching brace.
fn function_definition<'a>(ir: &'a str, name: &str) -> &'a str {
    let marker = format!("@{name}(");
    let name_start = ir
        .find(&marker)
        .unwrap_or_else(|| panic!("missing function `{name}`"));
    let start = ir[..name_start]
        .rfind("define ")
        .unwrap_or_else(|| panic!("missing definition for `{name}`"));
    let open = ir[start..]
        .find('{')
        .map(|offset| start + offset)
        .unwrap_or_else(|| panic!("missing body for `{name}`"));
    let mut depth = 0_usize;
    for (offset, character) in ir[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &ir[start..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated body for `{name}`")
}

#[cfg(unix)]
#[test]
fn same_width_sign_cast_constants_fold_with_the_counterpart_type() {
    check(concat!(
        "const AllOnes = U32.cast_signed 0xFFFF_FFFF\n",
        "let typed: I32 = AllOnes\n",
    ))
    .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    assert_eq!(
        run_lines(concat!(
            "use std.io.println\n",
            "const AllOnes = U32.cast_signed 0xFFFF_FFFF\n",
            "println \"${AllOnes}\"\n",
        )),
        ["-1"]
    );
}

#[test]
fn same_width_sign_cast_checking_rejects_wrong_types() {
    let diagnostics = check("let value = I32.cast_signed 1\n")
        .expect_err("signed companions have no cast_signed");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("no accessible companion member named `cast_signed`")),
        "{diagnostics:?}"
    );

    let diagnostics = check("let value: I32 = 1\nlet cast = U32.cast_signed value\n")
        .expect_err("the source value must be a U32");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected `U32`, found `I32`")),
        "{diagnostics:?}"
    );
}

#[test]
fn integer_companion_constants_fold_at_their_declared_width() {
    let module = check(concat!(
        "const min: I8 = I8.Min\n",
        "const ones: U32 = U8.count_ones 255\n",
        "const sum: I8 = match (I8.checked_add 100 20) { Some value => value, None => 0 }\n",
        "const wrapped: I8 = match (I8.checked_add 100 100) { Some value => value, None => 0 }\n",
        "const rotated: U8 = U8.rotate_left 1 7\n",
        "const no_sum: Option<U8> = U8.checked_add 200 100\n",
        "const some_sum: Option<U8> = U8.checked_add 1 2\n",
        "const auto_none = U8.checked_add 200 100\n",
        "const auto_some = U8.checked_add 1 2\n",
    ))
    .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    for (name, expected) in [
        ("ones", "8u32"),
        ("sum", "120i8"),
        ("wrapped", "0i8"),
        ("rotated", "128u8"),
    ] {
        match folded_value(&module, name) {
            staple_syntax::Expression::Integer(integer) => {
                assert_eq!(integer.literal, expected, "{name}")
            }
            other => panic!("expected `{name}` to fold to an integer, found {other:?}"),
        }
    }
    #[cfg(unix)]
    assert_eq!(
        run_lines(concat!(
            "use std.io.println\n",
            "const min: I8 = I8.Min\n",
            "const wide: I64 = I64.Min\n",
            "const no_sum: Option<U8> = U8.checked_add 200 100\n",
            "const some_sum: Option<U8> = U8.checked_add 1 2\n",
            "println \"${min}\"\n",
            "println \"${wide}\"\n",
            "match no_sum { Some value => println \"${value}\", None => println \"None\", }\n",
            "match some_sum { Some value => println \"${value}\", None => println \"None\", }\n",
        )),
        ["-128", "-9223372036854775808", "None", "3"]
    );
}

#[cfg(unix)]
#[test]
fn folded_options_check_negative_payloads_at_the_annotated_type() {
    // A negative payload reifies as `0 - (n - 1) - 1`; without the
    // annotation's `T` it would default to `I32`, and `I64.Min`'s magnitude
    // would not fit at all.
    assert_eq!(
        run_lines(concat!(
            "use std.io.println\n",
            "const narrow: Option<I8> = I8.checked_subtract I8.Min 0\n",
            "const wide: Option<I64> = I64.checked_add I64.Min 0\n",
            "const negative: Option<I16> = I16.checked_add (0 - 5) 0\n",
            "match narrow { Some value => println \"${value}\", None => println \"None\", }\n",
            "match wide { Some value => println \"${value}\", None => println \"None\", }\n",
            "match negative { Some value => println \"${value}\", None => println \"None\", }\n",
        )),
        ["-128", "-9223372036854775808", "-5"]
    );
}

#[test]
fn pointer_width_constants_report_a_folding_diagnostic() {
    let diagnostics = check("const ones: U32 = USize.count_ones 255\n")
        .expect_err("a pointer-width constant should not fold");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("pointer-width")),
        "{diagnostics:?}"
    );
}

#[test]
fn integer_companions_are_reachable_from_pipe_stages_without_an_import() {
    let ir = compile_ir(concat!(
        "let x: I32 = 0 - 7\n",
        "let y: U32 = 3\n",
        "let a = x |> abs\n",
        "let b = x |> checked_add 1\n",
        "let c = y |> rotate_left 3\n",
    ));
    assert!(ir.contains("llvm.fshl"), "{ir}");
}

#[test]
fn source_parses_the_integer_companions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let path = root.join("stdlib/std/number/types.sta");
    let source = std::fs::read_to_string(&path).unwrap();
    parse(&source).unwrap_or_else(|errors| panic!("parse {}: {errors:?}", path.display()));
}
