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
fn execute(lowered: &LoweredModule) -> std::process::Output {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("staple-string-{}-{nonce}", std::process::id()));
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
    output
}

#[cfg(unix)]
fn run(lowered: &LoweredModule) -> String {
    let output = execute(lowered);
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
fn string_operations_runtime() {
    let lowered = lower(
        r#"
use std.io.println
use std.fmt.Formatter
use std.range.Range
def expect = (condition: Bool; label: String) => () :: {
    if (condition == False) { panic label }
}
def offset = (actual: Option<USize>; expected: I32) => () :: match actual {
    Some n => expect (n == ((TryConvert.try_convert.<I32, USize> expected) |> unwrap)) "offset",
    None => expect (expected == -1) "missing offset",
}
def text = (actual: Option<String>; expected: String) => () :: match actual {
    Some value => expect (value == expected) "substring",
    None => panic "missing substring",
}
def absent = actual: Option<String> => () :: match actual {
    None => (), Some _ => panic "invalid slice accepted",
}
def main = () => {
    expect (String.length "héllo" == 6) "byte length"
    expect (String.char_count "héllo" == 5) "scalar count"
    expect (String.is_empty "") "empty"
    expect (String.is_empty "a" == False) "nonempty"
    for value in (List.of ("", "abc", "héllo", "日本", "😀")) {
        expect (String.starts_with value "") "empty prefix"
        expect (String.ends_with value "") "empty suffix"
        expect (String.contains value "") "empty contains"
        offset (String.find value "") 0
        offset (String.rfind value "") ((TryConvert.try_convert.<USize, I32> (String.length value)) |> unwrap)
        absent (String.slice value (Range (current: USize :: 1, end: USize :: 0)))
        absent (String.slice value (Range (current: USize :: 0, end: String.length value + 1)))
        text (String.slice value (Range (current: USize :: 0, end: String.length value))) value
        text (String.slice value (Range (current: USize :: 0, end: USize :: 0))) ""
        expect (String.trim value == value) "trim unchanged"
        expect (String.repeat value 0 == "") "repeat zero"
        expect (String.repeat value 2 == value + value) "repeat twice"
        expect (String.to_ascii_uppercase value == String.to_ascii_uppercase (String.to_ascii_uppercase value)) "upper idempotent"
        expect (String.eq_ignore_ascii_case value value) "case self"
        expect (String.replace value "missing" "replacement" == value) "replace absent"
        expect (String.join "," (List.of value) == value) "singleton join"
        expect (String.is_char_boundary value (String.length value)) "end boundary"
        expect (String.is_char_boundary value (String.length value + 1) == False) "past end"
        offset (String.find value "longer than any source") (-1)
        offset (String.rfind value "longer than any source") (-1)
        expect (String.starts_with value "longer than any source" == False) "long prefix"
        expect (String.ends_with value "longer than any source" == False) "long suffix"
    }
    offset (String.find "héllohéllo" "llo") 3
    offset (String.rfind "héllohéllo" "llo") 9
    offset (String.find "日本😀日本" "本") 3
    offset (String.rfind "日本😀日本" "日") 10
    offset (String.find_char "hé😀" '😀') 3
    offset (String.find "abc" "a") 0
    offset (String.find "abc" "b") 1
    offset (String.find "abc" "c") 2
    offset (String.find "abc" "x") (-1)
    offset (String.rfind "" "x") (-1)
    expect (String.starts_with "日本😀" "日本") "unicode prefix"
    expect (String.ends_with "日本😀" "😀") "unicode suffix"
    expect (String.contains "日本😀" "本") "unicode contains"
    expect (String.contains "日本😀" "x" == False) "absent contains"
    expect (String.is_char_boundary "hé😀" 2 == False) "continuation"
    absent (String.slice "hé😀" ((USize :: 1) .. (USize :: 2)))
    absent (String.slice "hé😀" ((USize :: 2) .. (USize :: 3)))
    text (String.slice "hé😀" ((USize :: 1) .. (USize :: 3))) "é"
    text (String.strip_prefix "hé😀" "hé") "😀"
    text (String.strip_suffix "hé😀" "😀") "hé"
    absent (String.strip_prefix "abc" "z")
    absent (String.strip_suffix "abc" "z")
    text (String.strip_prefix "abc" "") "abc"
    text (String.strip_suffix "abc" "") "abc"
    expect (String.trim " \t\u{2003}日本😀\u{3000}\r\n" == "日本😀") "unicode trim"
    expect (String.trim_start " é " == "é ") "trim start"
    expect (String.trim_end " é " == " é") "trim end"
    expect (String.trim "\u{85}\u{a0}\u{1680}\u{2028}\u{2029}\u{202f}\u{205f}" == "") "whitespace only"
    expect (Char.is_whitespace '\u{200b}' == False) "zero width space"
    expect (Char.is_whitespace '\u{180e}' == False) "mongolian separator"
    expect (Char.is_ascii_digit '9') "digit"
    expect (Char.is_ascii_digit 'é' == False) "unicode digit exclusion"
    expect (Char.is_ascii_alphabetic 'Z') "alphabetic"
    expect (Char.is_ascii_alphanumeric '8') "alphanumeric"
    expect (Char.is_ascii_uppercase 'Z') "uppercase"
    expect (Char.is_ascii_lowercase 'a') "lowercase"
    expect (Char.to_ascii_uppercase 'é' == 'é') "unicode case unchanged"
    expect (Char.to_ascii_lowercase 'A' == 'a') "char lower"
    expect (Char.to_ascii_uppercase 'z' == 'Z') "char upper"
    expect (Char.to_string '😀' == "😀") "char string"
    expect (String.to_ascii_uppercase "héLLo日本😀" == "HéLLO日本😀") "upper"
    expect (String.to_ascii_lowercase "HéLLo日本😀" == "héllo日本😀") "lower"
    expect (String.eq_ignore_ascii_case "Abé" "aBé") "ascii equal"
    expect (String.eq_ignore_ascii_case "é" "É" == False) "unicode unequal"
    expect (String.eq_ignore_ascii_case "a" "aa" == False) "unequal lengths"
    expect (String.join "|" ((List<String> :: List.new ())) == "") "empty join"
    expect (String.join ", " (List.of ("x", "y", "z")) == "x, y, z") "join"
    expect (String.join "|" (String.split "a,b,,c" ",") == "a|b||c") "split"
    expect (String.join "|" (String.split ",a,," ",") == "|a||") "empty fields"
    expect (String.join "|" (String.split "日本😀日本" "日本") == "|😀|") "unicode split"
    expect (String.join "|" (String.split "" ",") == "") "empty split"
    expect (String.join "|" (String.split "abc" "long") == "abc") "long split"
    match String.split_once "a=b=c" "=" {
        Some (a, b) => expect (a + "|" + b == "a|b=c") "split once", None => panic "split once missing",
    }
    match String.split_once "abc" "x" { None => (), Some _ => panic "unexpected split", }
    match String.split_once "abc" "" {
        Some (a, b) => expect (a + "|" + b == "|abc") "empty split once", None => panic "empty split once missing",
    }
    expect (String.join "|" (String.lines "a\r\nb\n") == "a|b") "CRLF lines"
    expect (String.join "|" (String.lines "\n\n") == "|") "empty lines"
    expect (String.join "|" (String.lines "single") == "single") "single line"
    expect (String.join "|" (String.lines "") == "") "no lines"
    expect (String.join "|" (String.split_whitespace "  hé\t日本\u{2003}😀 \n") == "hé|日本|😀") "split whitespace"
    expect (String.join "|" (String.split_whitespace " \t") == "") "all whitespace"
    expect (String.replace "aaaa" "aa" "b" == "bb") "nonoverlapping replace"
    expect (String.replace "日本😀日本" "日本" "é" == "é😀é") "unicode replace"
    expect (String.replace "ab" "" "-" == "-a-b-") "empty replace"
    expect (String.replace "" "" "-" == "-") "empty replacement source"
    expect ("a" < "b") "order ascii"
    expect ("b" < "é") "order unicode"
    expect ("ab" < "abc") "order prefix"
    expect ("abc" > "ab") "order longer"
    expect ("" <= "") "order empty"
    let mut builder = StringBuilder.from_string "héllo"
    builder |> append "😀"
    offset (String.find (StringBuilder.finish builder) "llo") 3
    let mut positions = StringBuilder.new ()
    for (at, c) in (String.char_indices "aé☃😀") {
        positions |> append "${at}:${Char.to_u32 c};"
    }
    expect (StringBuilder.finish positions == "0:97;1:233;3:9731;6:128512;") "indices"
    println "ok"
}
main ()
"#,
    );
    assert_eq!(run(&lowered), "ok\n");
}

#[cfg(unix)]
#[test]
fn empty_split_separator_panics() {
    let lowered = lower(r#"let parts = String.split "abc" """#);
    let output = execute(&lowered);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("String.split: empty separator"));
}

#[cfg(unix)]
#[test]
fn search_and_slice_match_utf8_reference_cases() {
    let mut source = String::from(
        r#"
use std.io.println
def expect = (actual: Bool; label: String) => () :: {
    if (actual == False) { panic label }
}
def offset = (actual: Option<USize>; expected: I32) => () :: match actual {
    Some n => expect (n == ((TryConvert.try_convert.<I32, USize> expected) |> unwrap)) "offset",
    None => expect (expected == -1) "missing offset",
}
def main = () => {
"#,
    );
    for value in ["", "abc", "héllo", "日本", "😀", "aé😀aé"] {
        for needle in [
            "", "a", "b", "c", "hé", "llo", "本", "😀", "absent", "abcabc",
        ] {
            let find = value.find(needle).map_or(-1, |n| n as i32);
            let rfind = value.rfind(needle).map_or(-1, |n| n as i32);
            source.push_str(&format!(
                "offset (String.find {value:?} {needle:?}) ({find})\n\
                 offset (String.rfind {value:?} {needle:?}) ({rfind})\n"
            ));
            for (operation, expected) in [
                ("starts_with", value.starts_with(needle)),
                ("ends_with", value.ends_with(needle)),
                ("contains", value.contains(needle)),
            ] {
                let expected = if expected { "True" } else { "False" };
                source.push_str(&format!(
                    "expect (String.{operation} {value:?} {needle:?} == {expected}) {operation:?}\n"
                ));
            }
        }
        for start in 0..=value.len() + 1 {
            for end in 0..=value.len() + 1 {
                let assertion = match value.get(start..end) {
                    Some(expected) => format!(
                        "Some actual => expect (actual == {expected:?}) \"slice\", None => panic \"missing slice\","
                    ),
                    None => "None => (), Some _ => panic \"invalid slice\",".to_owned(),
                };
                source.push_str(&format!(
                    "match String.slice {value:?} ((USize :: {start}) .. (USize :: {end})) {{ {assertion} }}\n"
                ));
            }
        }
    }
    source.push_str("println \"ok\"\n}\nmain ()\n");
    assert_eq!(run(&lower(&source)), "ok\n");
}
