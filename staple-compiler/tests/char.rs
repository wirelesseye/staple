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
    let base = std::env::temp_dir().join(format!("staple-char-{}-{nonce}", std::process::id()));
    let object = base.with_extension("o");
    let context = Context::create();
    CodeGenerator::new(&context)
        .emit_object(lowered, &object, None)
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
fn char_runtime() {
    let lowered = lower(
        r#"
use std.io.println
use std.fmt.Formatter
let text = "aé☃😀"
let restored: String = String.chars text |> collect
println restored
println "${String.char_count text}"
println "${Char.to_u32 'é'}"
println "${'\u{2603}'}"
println "\u{2603}"
println (Formatter.debug '\n')
println (Formatter.debug '\u{301}')
println (Formatter.display '😀')
"#,
    );
    assert_eq!(
        run(&lowered),
        "aé☃😀\n4\n233\n☃\n☃\n'\\n'\n'\\u{301}'\n😀\n"
    );
}

#[test]
fn invalid_character_literals_report_scalar_errors() {
    for (literal, expected) in [
        ("''", "empty character literal"),
        ("'ab'", "2 scalar values"),
        ("'e\u{301}'", "2 scalar values"),
        ("'\\u{D800}'", "U+D800"),
        ("'\\u{110000}'", "U+110000"),
        ("'\\q'", "unknown character escape"),
    ] {
        let errors = check(&format!("let c = {literal}\n")).unwrap_err();
        assert!(
            errors.iter().any(|e| e.message.contains(expected)),
            "{literal}: {errors:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn char_macros_round_trip_literals() {
    let lowered = lower(
        r#"
use std.io.println
use std.syntax.*
macro echo = value: CharExpr => parse_quote { $value }
macro snowman = () => CharExpr '\u{2603}'
macro rebuild = value: CharExpr => match value { CharExpr c => CharExpr c, }
macro parsed = body: Braced<Syntax> => {
    let Braced contents = body
    match (Parse.parse_all.<CharExpr> contents) {
        Ok value => parse_quote { $value },
        ParseError (message) => panic message,
    }
}
println "${echo 'é'}"
println "${snowman ()}"
println "${rebuild '😀'}"
println "${parsed { '\u{301}' }}"
"#,
    );
    assert_eq!(run(&lowered), "é\n☃\n😀\n\u{301}\n");
}

#[cfg(unix)]
#[test]
fn char_scalar_boundaries_conversions_and_collection() {
    let lowered = lower(
        r#"
use std.io.println
use std.convert.(Convert, TryConvert)
use std.result.Ok
use std.cmp.Ord
def main = () => {
let mut values: List<U32> = List.new ()
List.push values 0
List.push values 127
List.push values 128
List.push values 2047
List.push values 2048
List.push values 55295
List.push values 57344
List.push values 65535
List.push values 65536
List.push values 1114111
for value in values {
    match Char.from_u32 value {
        Some c => {
            let text: String = String.chars ("${c}") |> collect
            for decoded in (String.chars text) {
                println "${Char.to_u32 decoded == value}"
            }
            println "${Char.len_utf8 c}"
        },
        None => println "unexpected None",
    }
}
let mut invalid: List<U32> = List.new ()
List.push invalid 55296
List.push invalid 57343
List.push invalid 1114112
for value in invalid {
    match Char.from_u32 value {
        None => println "rejected",
        Some _ => println "unexpected Some",
    }
}
let numeric: U32 = Convert.convert 'é'
println "${numeric}"
let converted: Ok<Char> | ConversionError = TryConvert.try_convert (U32 :: 233)
match converted { Ok c => println "${c}", _ => println "conversion failed", }
let empty: String = String.chars "" |> collect
println "${String.char_count empty}"
let filtered: String = String.chars "aé☃😀" |> filter (c => Char.is_ascii c) |> collect
println filtered
println "${'a' < 'é'}"
}
main ()
"#,
    );
    assert_eq!(
        run(&lowered),
        concat!(
            "True\n1\nTrue\n1\nTrue\n2\nTrue\n2\nTrue\n3\nTrue\n3\nTrue\n3\nTrue\n3\nTrue\n4\nTrue\n4\n",
            "rejected\nrejected\nrejected\n233\né\n0\na\nTrue\n"
        )
    );
}

#[test]
fn char_representation_cannot_be_constructed_or_narrowed_by_clients() {
    for source in [
        "let invalid = Char 55296\n",
        "let invalid = Char.from_u32_unchecked 55296\n",
        "let invalid: Char = Convert.convert (U32 :: 55296)\n",
    ] {
        assert!(check(source).is_err(), "unexpectedly accepted: {source}");
    }
}
