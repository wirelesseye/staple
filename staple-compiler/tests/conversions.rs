// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::context::Context;
use staple_compiler::{
    CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
};
use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn lower(source: &str) -> LoweredModule {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let resolved = NameResolver::new()
        .resolve_program(program)
        .unwrap_or_else(|errors| panic!("resolve: {errors:?}"));
    let typed = TypeChecker::new()
        .check(resolved)
        .unwrap_or_else(|errors| panic!("check: {errors:?}"));
    Lowerer::new()
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

#[cfg(unix)]
fn run(lowered: &LoweredModule) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base =
        std::env::temp_dir().join(format!("staple-conversions-{}-{nonce}", std::process::id()));
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
}

#[test]
fn numeric_conversion_matrix_runs_and_emits_for_32_and_64_bit_targets() {
    let numbers = [
        "I8", "I16", "I32", "I64", "U8", "U16", "U32", "U64", "ISize", "USize", "F32", "F64",
    ];
    let mut source = String::new();
    for to in numbers {
        let one = if to.starts_with('F') { "1.0" } else { "1" };
        source.push_str(&format!("def check_{to}: (Ok {to} | ConversionError) -> () = result => match result {{\n  Ok value => if (value == ({to} :: {one})) {{ () }} else {{ panic \"incorrect {to} conversion\" }},\n  _ => panic \"unexpected conversion error\",\n}}\n"));
    }
    for from in numbers {
        let one = if from.starts_with('F') { "1.0" } else { "1" };
        for to in numbers {
            source.push_str(&format!("let result_{from}_{to}: Ok {to} | TryConvert.Error {from} {to} = TryConvert.try_convert ({from} :: {one})\ncheck_{to} result_{from}_{to}\n"));
        }
    }
    for to in numbers {
        source.push_str(&format!("let parsed_{to}: Ok {to} | ConversionError = TryConvert.try_convert \"1\"\ncheck_{to} parsed_{to}\n"));
        if !to.starts_with('F') {
            source.push_str(&format!("let boolean_{to}: {to} = Convert.convert (Bool :: True)\nlet false_{to}: {to} = Convert.convert (Bool :: False)\nif (boolean_{to} == ({to} :: 1)) {{ () }} else {{ panic \"boolean conversion failed\" }}\nif (false_{to} == ({to} :: 0)) {{ () }} else {{ panic \"false conversion failed\" }}\n"));
        }
        let one = if to.starts_with('F') { "1.0" } else { "1" };
        source.push_str(&format!("let formatted_{to}: String = Convert.convert ({to} :: {one})\nif (formatted_{to} == \"1\") {{ () }} else {{ panic \"number formatting failed\" }}\n"));
    }
    // Exercise Convert directly, including every category of platform-independent guarantee.
    for (from, to) in [
        ("I8", "I16"),
        ("I16", "I32"),
        ("I32", "I64"),
        ("U8", "U16"),
        ("U16", "U32"),
        ("U32", "U64"),
        ("U8", "I16"),
        ("U16", "I32"),
        ("U32", "I64"),
        ("I16", "F32"),
        ("U16", "F32"),
        ("I32", "F64"),
        ("U32", "F64"),
        ("F32", "F64"),
        ("I32", "ISize"),
        ("U32", "USize"),
        ("ISize", "I64"),
        ("USize", "U64"),
    ] {
        let one = if from.starts_with('F') { "1.0" } else { "1" };
        source.push_str(&format!(
            "let direct_{from}_{to}: {to} = Convert.convert ({from} :: {one})\n"
        ));
    }
    let lowered = lower(&source);
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    for target in ["i386-unknown-linux-gnu", "x86_64-unknown-linux-gnu"] {
        generator
            .compile_module_for_target(&lowered, Some(target))
            .unwrap_or_else(|errors| panic!("{target}: {errors:?}"));
    }
    #[cfg(unix)]
    run(&lowered);
}

#[test]
fn checked_numeric_boundaries_and_parsing_run() {
    let lowered = lower(include_str!("fixtures/conversions_numeric.sta"));
    #[cfg(unix)]
    run(&lowered);
}

#[test]
fn string_collection_and_pointer_conversions_run() {
    let lowered = lower(include_str!("fixtures/conversions_values.sta"));
    #[cfg(unix)]
    run(&lowered);
}

#[test]
fn as_syntax_runs_for_builtin_generic_custom_and_move_only_conversions() {
    let lowered = lower(include_str!("fixtures/conversions_as.sta"));
    #[cfg(unix)]
    run(&lowered);
}

#[test]
fn opaque_pointer_conversions_run_and_emit_for_32_and_64_bit_targets() {
    let lowered = lower(include_str!("fixtures/conversions_opaque_pointer.sta"));
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    for target in ["i386-unknown-linux-gnu", "x86_64-unknown-linux-gnu"] {
        generator
            .compile_module_for_target(&lowered, Some(target))
            .unwrap_or_else(|errors| panic!("{target}: {errors:?}"));
    }
    #[cfg(unix)]
    run(&lowered);
}

#[test]
fn opaque_pointer_requires_explicit_conversion() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for source in [
        "use std.cinterop.*\nuse std.ptr.*\ndef erase: RawPtr I32 -> RawOpaquePtr = value => value\n",
        "use std.cinterop.*\nuse std.ptr.*\ndef restore: RawOpaquePtr -> RawPtr I32 = value => value\n",
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, root)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let errors = TypeChecker::new()
            .check(resolved)
            .expect_err("implicit pointer conversion must fail");
        assert!(
            errors.iter().any(|error| error.message.contains("expected")
                && error.message.contains("RawOpaquePtr")
                && error.message.contains("RawPtr I32")),
            "{errors:?}"
        );
    }
}

#[test]
fn raw_pointer_byte_offsets_run_and_emit_for_32_and_64_bit_targets() {
    let lowered = lower(include_str!("fixtures/pointer_offset.sta"));
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    for target in ["i386-unknown-linux-gnu", "x86_64-unknown-linux-gnu"] {
        generator
            .compile_module_for_target(&lowered, Some(target))
            .unwrap_or_else(|errors| panic!("{target}: {errors:?}"));
    }
    #[cfg(unix)]
    run(&lowered);
}

#[test]
fn mutable_raw_pointers_run_and_emit_for_32_and_64_bit_targets() {
    let lowered = lower(include_str!("fixtures/pointer_mutable.sta"));
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    for target in ["i386-unknown-linux-gnu", "x86_64-unknown-linux-gnu"] {
        let llvm = generator
            .compile_module_for_target(&lowered, Some(target))
            .unwrap_or_else(|errors| panic!("{target}: {errors:?}"));
        assert!(llvm.contains("pointer.load"), "concrete load must emit");
        assert!(
            llvm.contains("raw.pointer"),
            "dynamic memory access must emit"
        );
    }
    #[cfg(unix)]
    run(&lowered);
}

#[test]
fn raw_pointer_operations_reject_invalid_access_and_implicit_conversions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let cases = [
        (
            "def bad: RawPtr I32 -> RawMutPtr I32 = p => p",
            "RawMutPtr I32",
        ),
        (
            "def bad: RawMutPtr I32 -> RawPtr I32 = p => p",
            "RawPtr I32",
        ),
        (
            "def bad: RawOpaquePtr -> RawOpaqueMutPtr = p => p",
            "RawOpaqueMutPtr",
        ),
        (
            "def bad: RawOpaqueMutPtr -> RawOpaquePtr = p => p",
            "RawOpaquePtr",
        ),
        (
            "def bad: RawMutPtr I32 -> RawMutPtr U8 = p => p as RawMutPtr U8",
            "bound",
        ),
        (
            "let p: RawPtr I32 = RawPtr.null ()\nRawMutPtr.store p 1",
            "RawMutPtr",
        ),
        (
            "let p = RawOpaqueMutPtr.null ()\nRawMutPtr.store p 1",
            "RawMutPtr",
        ),
        ("let p = RawOpaquePtr.null ()\nRawPtr.load p", "RawPtr"),
        (
            "let p: RawPtr CString = RawPtr.null ()\nRawPtr.load p",
            "bound",
        ),
        (
            "let p: RawMutPtr CString = RawMutPtr.null ()\nRawMutPtr.load p",
            "bound",
        ),
        (
            "let p: RawMutPtr CString = RawMutPtr.null ()\nlet v = c_string \"bad\"\nRawMutPtr.store p v",
            "bound",
        ),
        (
            "let store = RawMutPtr.store\nlet p: RawMutPtr CString = RawMutPtr.null ()\nlet v = c_string \"bad\"\nstore p v",
            "bound",
        ),
        (
            "let p: RawMutPtr CString = RawMutPtr.null ()\nlet v = c_string \"bad\"\n(if True { RawMutPtr.store } else { RawMutPtr.store }) p v",
            "bound",
        ),
        (
            "let p: RawPtr I32 = RawPtr.null ()\nlet q: RawMutPtr I32 = RawMutPtr.null ()\nlet bad = p == q",
            "Raw",
        ),
        ("def bad: CString -> RawMutPtr CChar = p => p", "RawMutPtr"),
        (
            "type Token = opaque\nlet p: RawMutPtr Token = RawMutPtr.null ()\nRawMutPtr.load p",
            "bound",
        ),
    ];
    for (body, expected) in cases {
        let source = format!("use std.ptr.*\nuse std.cinterop.*\n{body}\n");
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, root)
            .unwrap();
        let resolved = NameResolver::new().resolve_program(program).unwrap();
        let errors = TypeChecker::new()
            .check(resolved)
            .err()
            .unwrap_or_else(|| panic!("invalid pointer operation was accepted: {body}"));
        assert!(
            errors.iter().any(|error| error.message.contains(expected)),
            "{body}: {errors:?}"
        );
    }
}
