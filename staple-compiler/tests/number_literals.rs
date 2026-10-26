//! Radix prefixes, digit separators, and type suffixes on number literals.

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
    let base = std::env::temp_dir().join(format!("staple-numbers-{}-{nonce}", std::process::id()));
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

fn error_message(source: &str) -> String {
    check(source)
        .expect_err("source should be rejected")
        .into_iter()
        .map(|diagnostic| diagnostic.message)
        .collect::<Vec<_>>()
        .join("\n")
}

fn binding_value(module: &staple_compiler::TypedModule, name: &str) -> staple_syntax::Expression {
    module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            staple_syntax::Item::Binding(binding) if binding.name == name => binding.value.clone(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("binding `{name}` should exist"))
}

fn folded_integer(module: &staple_compiler::TypedModule, name: &str) -> String {
    match binding_value(module, name) {
        staple_syntax::Expression::Integer(integer) => integer.literal,
        other => panic!("expected `{name}` to fold to an integer, found {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn radix_separator_and_suffix_literals_run() {
    let lowered = lower(
        r#"
use std.io.println
println "${0xFF}"
println "${0b1010}"
println "${0o755}"
println "${1_000_000}"
println "${0xffffu16}"
println "${1_000.5}"
let maximum: U64 = 0xFFFF_FFFF_FFFF_FFFF
println "${maximum}"
let binary: U64 = 0b1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111_1111
println "${binary}"
let small: I8 = 0x7F_i8
println "${small}"
"#,
    );
    assert_eq!(
        run(&lowered),
        "255\n10\n493\n1000000\n65535\n1000.5\n18446744073709551615\n18446744073709551615\n127\n"
    );
}

#[test]
fn every_suffix_names_its_literal_type() {
    for (literal, ty) in [
        ("1i8", "I8"),
        ("1i16", "I16"),
        ("1i32", "I32"),
        ("1i64", "I64"),
        ("1isize", "ISize"),
        ("1u8", "U8"),
        ("1u16", "U16"),
        ("1u32", "U32"),
        ("1u64", "U64"),
        ("1usize", "USize"),
        ("1.5f32", "F32"),
        ("1.5f64", "F64"),
    ] {
        check(&format!("let value: {ty} = {literal}\n"))
            .unwrap_or_else(|errors| panic!("{literal}: {errors:?}"));
    }
}

#[cfg(unix)]
#[test]
fn suffixed_literals_print_at_their_type() {
    let lowered = lower(
        r#"
use std.io.println
println "${3i8} ${3i16} ${3i32} ${3i64} ${3isize}"
println "${3u8} ${3u16} ${3u32} ${3u64} ${3usize}"
println "${1.5f32} ${1.5f64}"
"#,
    );
    assert_eq!(run(&lowered), "3 3 3 3 3\n3 3 3 3 3\n1.5 1.5\n");
}

#[test]
fn malformed_or_out_of_range_literals_are_rejected() {
    for (source, expected) in [
        ("let value: U8 = 256u8\n", "does not fit in `U8`"),
        ("let value: I8 = 0x80i8\n", "does not fit in `I8`"),
        ("let value: I8 = 0b1000_0000i8\n", "does not fit in `I8`"),
        ("let value = -128i8\n", "does not fit in `I8`"),
        ("let value: I32 = 0xFFFFFFFF\n", "does not fit in `I32`"),
        ("let value: I32 = 3u8\n", "expected `I32`, found `U8`"),
        (
            "let value = 1.5u8\n",
            "integer suffix cannot be used on a float literal",
        ),
        ("let value = 0b1f32\n", "a binary literal cannot be a float"),
        ("let value = 0b102\n", "invalid digit `2` in binary literal"),
        ("let value = 1__000\n", "misplaced `_`"),
        (
            "let value = 42abc\n",
            "invalid suffix `abc` on number literal",
        ),
        ("let value = 0x\n", "hexadecimal literal is missing digits"),
        ("let value = 42U8\n", "u8"),
        ("let value = 0X1F\n", "0x"),
        ("let value = (1 ^ 3u8)\n", "type suffix"),
    ] {
        let message = error_message(source);
        assert!(
            message.contains(expected),
            "{source}: expected `{expected}`, found `{message}`"
        );
    }
}

#[test]
fn suffixed_literals_keep_their_type_in_inference() {
    check("let a = 3u8\nlet b: U8 = a\n").expect("a suffixed literal infers its suffix type");
    check("let values: (I32 ^ 0b11) = (1, 2, 3)\n")
        .expect("a radix literal works as a number literal type");

    let source = concat!(
        "def identity: <N where Natural N> N -> N = <N where Natural N> value => value\n",
        "let value = identity 3u8\n",
        "let typed: U8 = value\n",
    );
    let diagnostics = check(source).expect_err(
        "a suffixed literal is a U8 argument, not the literal 3, so Natural rejects it",
    );
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("trait bound is not satisfied for `U8`")),
        "{diagnostics:?}"
    );
}

#[test]
fn malformed_number_literal_types_are_rejected() {
    for (source, expected) in [
        ("let value: (I32 ^ 3u8) = (1, 2, 3)\n", "type suffix"),
        ("let value: (I32 ^ 0x) = (1, 2, 3)\n", "missing digits"),
        ("let value: (I32 ^ 1__0) = (1, 2, 3)\n", "misplaced `_`"),
    ] {
        let message = error_message(source);
        assert!(
            message.contains(expected),
            "{source}: expected `{expected}`, found `{message}`"
        );
    }
    check("let value: (I32 ^ 0x3) = (1, 2, 3)\n")
        .expect("a radix literal type is a valid repeat count");
    check("let value: (I32 ^ 1_0) = (1, 2, 3, 4, 5, 6, 7, 8, 9, 10)\n")
        .expect("separators are valid in number literal types");
}

#[test]
fn compile_time_integer_folds_carry_suffixes() {
    let module = check("const Small = 5u8\nconst Mode = 0o755\n")
        .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    assert_eq!(folded_integer(&module, "Small"), "5u8");
    assert_eq!(folded_integer(&module, "Mode"), "493");
    check("const Small = 5u8\nlet value: U8 = Small\n")
        .expect("a folded suffixed constant keeps its type");
}

#[cfg(unix)]
#[test]
fn compile_time_float_suffix_folds_with_its_type() {
    check("const Half = 0.5f32\nlet typed: F32 = Half\n")
        .expect("a suffixed float constant keeps its type");
    let lowered = lower(concat!(
        "use std.io.println\n",
        "const Half = 0.5f32\n",
        "println \"${Half}\"\n",
    ));
    assert_eq!(run(&lowered), "0.5\n");
}

#[test]
fn compile_time_arithmetic_folds_keep_the_suffix() {
    // The language's bitwise operators are trait methods, not expression
    // syntax, so this exercises the same fold path with radix literals and an
    // arithmetic operator.
    let module = check("const Mask = 0xF0u8 + 0b0000_1111u8\n")
        .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    assert_eq!(folded_integer(&module, "Mask"), "255u8");
}

#[test]
fn mixed_or_overflowing_folds_are_rejected() {
    let diagnostics = check("const Bad = 1u8 + 1u16\n").expect_err("mixed integer types");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected `U8`, found `U16`")),
        "{diagnostics:?}"
    );

    // A folded value out of its type's range is an error in either
    // direction, and the message names the value rather than a literal the
    // user never wrote.
    for (source, message) in [
        ("const Big = 255u8 + 1u8\n", "constant value `256` does not fit in `U8`"),
        ("const Under = 0u8 - 1u8\n", "constant value `-1` does not fit in `U8`"),
        ("const Annotated: U8 = 0 - 1\n", "constant value `-1` does not fit in `U8`"),
        ("const Wide = 0x7F_i8 + 1\n", "constant value `128` does not fit in `I8`"),
        ("const Low = I8.Min - 1\n", "constant value `-129` does not fit in `I8`"),
    ] {
        let diagnostics = check(source).expect_err("out-of-range constant");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(message)),
            "{source}: {diagnostics:?}"
        );
    }

    let diagnostics = check("const Mismatch: I32 = 1u8\n").expect_err("annotation mismatch");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected `I32`, found `U8`")),
        "{diagnostics:?}"
    );
}

#[test]
fn companion_folds_and_constant_references_keep_their_type() {
    let module = check(concat!(
        "const Reversed = U8.reverse_bits 1u8\n",
        "const Swapped = U16.swap_bytes 1\n",
        "const Rotated = U8.rotate_left 1 1\n",
        "const Ones = U8.count_ones 0xFFu8\n",
        "const Max = U8.Max\n",
        "const Min = I8.Min\n",
        "const Signed = U32.cast_signed 0xFFFF_FFFF\n",
        "let reversed: U8 = Reversed\n",
        "let swapped: U16 = Swapped\n",
        "let rotated: U8 = Rotated\n",
        "let ones: U32 = Ones\n",
        "let max: U8 = Max\n",
        "let min: I8 = Min\n",
        "let signed: I32 = Signed\n",
    ))
    .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    assert_eq!(folded_integer(&module, "Reversed"), "128u8");
    assert_eq!(folded_integer(&module, "Swapped"), "256u16");
    assert_eq!(folded_integer(&module, "Rotated"), "2u8");
    assert_eq!(folded_integer(&module, "Ones"), "8u32");
    assert_eq!(folded_integer(&module, "Max"), "255u8");
}

#[test]
fn quoted_number_literals_keep_their_text() {
    let module = check(concat!(
        "use std.syntax.(parse_quote, Expr)\n",
        "macro identity: Expr -> Expr = value => parse_quote { $value }\n",
        "let hexadecimal = identity 0xFF\n",
        "let separated = identity 1_000\n",
        "let suffixed = identity 3u8\n",
    ))
    .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    for (name, expected) in [
        ("hexadecimal", "0xFF"),
        ("separated", "1_000"),
        ("suffixed", "3u8"),
    ] {
        match binding_value(&module, name) {
            staple_syntax::Expression::Integer(integer) => {
                assert_eq!(integer.literal, expected, "{name}")
            }
            other => panic!("expected `{name}` to stay an integer, found {other:?}"),
        }
    }
}
