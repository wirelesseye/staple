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
        "staple-string-builder-{}-{nonce}",
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
fn builder_runtime() {
    let lowered = lower(
        r#"
use std.io.println
use std.fmt.Formatter
use std.iterator.IterStep.(Done, Yield)
type Nested wrap I32
impl Display Nested {
    fmt = (Nested value, mut formatter) => {
        Formatter.write formatter "nested:"
        Display.fmt (value, formatter)
    }
}
def main = () => {
    let mut builder = StringBuilder.with_capacity 1
    builder |> append "total: "
    builder |> append_display 42
    builder |> push '\n'
    builder |> append_display (Nested 7)
    println (StringBuilder.to_string builder)
    builder |> push '😀'
    println (StringBuilder.finish builder)
    let mut scalars = StringBuilder.from_string "aé☃😀"
    for _ in (0 .. 5) {
        match StringBuilder.pop scalars {
            Some c => println "${Char.to_u32 c}", None => println "empty",
        }
    }
    let capacity = StringBuilder.capacity scalars
    scalars |> append "retained"
    StringBuilder.clear scalars
    println "${StringBuilder.is_empty scalars} ${StringBuilder.capacity scalars >= capacity}"
    let mut many = StringBuilder.new ()
    for _ in (0 .. 10000) { many |> push 'é' }
    println "${StringBuilder.length many} ${String.char_count (StringBuilder.finish many)}"
    let collected: StringBuilder = String.chars "aé😀" |> collect
    let extended = String.chars "!" |> extend collected
    println (StringBuilder.finish extended)
    let pieces: StringBuilder = (List.of ("a", "é", "😀") |> into_iterator) |> collect
    println (StringBuilder.finish pieces)
    let mut original = StringBuilder.from_string "first"
    let snapshot = StringBuilder.to_string original
    original |> append " second"
    println snapshot
    println (StringBuilder.to_string original)
}
main ()
"#,
    );
    assert_eq!(
        run(&lowered),
        "total: 42\nnested:7\ntotal: 42\nnested:7😀\n128512\n9731\n233\n97\nempty\nTrue True\n20000 10000\naé😀!\naé😀\nfirst\nfirst second\n"
    );
}

#[test]
fn builder_is_not_display() {
    // Formatting moves its value, so interpolation would consume the builder.
    let errors = check("let builder = StringBuilder.new ()\nlet text = \"${builder}\"\n").unwrap_err();
    assert!(!errors.is_empty());
}

#[cfg(unix)]
#[test]
fn conditionally_moved_mut_parameter_drops_once() {
    let lowered = lower(
        r#"
use std.io.println
type Tag wrap I32
impl !Copy Tag {}
use std.cinterop.*
use std.ptr.RawPtr
extern "c" { printf: (RawPtr<CChar>, ...) -> I32 }
impl Drop Tag {
    cleanup = Tag n => { printf (c_string "drop %d\n", n); () }
}
def consume = <T> move value: T => () :: ()
def replace = (mut value: Tag; flag: Bool) => () :: {
    if (flag) { consume value }
    value = Tag 9
}
def replace_any = <T> (mut value: T; move fresh: T; flag: Bool) => () :: {
    if (flag) { consume value }
    value = fresh
}
def main = () => {
    let mut a = Tag 1
    replace a True
    println "a"
    let mut b = Tag 2
    replace b False
    println "b"
    let mut c = Tag 3
    replace_any c (Tag 4) True
    println "c"
    let mut d = Tag 5
    replace_any d (Tag 6) False
    println "d"
}
main ()
"#,
    );
    assert_eq!(
        run(&lowered),
        "drop 1\na\ndrop 2\nb\ndrop 3\nc\ndrop 5\nd\ndrop 6\ndrop 4\ndrop 9\ndrop 9\n"
    );
}

#[test]
fn finish_moves_builder() {
    let errors = check("let builder = StringBuilder.new ()\nlet text = StringBuilder.finish builder\nlet again = StringBuilder.length builder\n").unwrap_err();
    assert!(
        errors.iter().any(|error| error.message.contains("mov")),
        "{errors:?}"
    );
}

#[cfg(unix)]
#[test]
fn moved_mut_parameter_restoration_and_list_drops() {
    let lowered = lower(
        r#"
use std.io.println
type Tag wrap I32
impl !Copy Tag {}
use std.cinterop.*
use std.ptr.RawPtr
extern "c" { printf: (RawPtr<CChar>, ...) -> I32 }
impl Drop Tag {
    cleanup = Tag n => { printf (c_string "drop %d\n", n); () }
}
def identity = <T> move value: T => T :: value
def restore = <T> mut value: T => () :: {
    let owned = identity value
    value = owned
}
def main = () => {
    let mut value = Tag 1
    restore value
    println "restored"
    value = Tag 2
    restore value
    drop value
    let mut values: List<Tag> = List.new ()
    values |> push (Tag 3)
    values |> push (Tag 4)
    List.reserve values 10
    List.truncate values 1
    println "truncated"
    List.truncate values 10
    let capacity = List.capacity values
    List.clear values
    println "${List.length values} ${List.capacity values == capacity}"
}
main ()
"#,
    );
    assert_eq!(
        run(&lowered),
        "restored\ndrop 1\ndrop 2\ndrop 4\ntruncated\ndrop 3\n0 True\n"
    );
}

#[cfg(unix)]
#[test]
fn formatting_preserves_unicode_and_escaping() {
    let lowered = lower(
        r#"
use std.io.println
use std.fmt.*
@derive_debug
type Label wrap String
println (Formatter.debug (Label "é😀"))
println (Formatter.debug "é😀\0\t\n\r\"\\")
let positive: I64 = 42
let negative = -positive
println (Formatter.display negative)
let product: (Bool, Char, String) = (True, 'é', "日本")
println (Formatter.debug product)
println (StringBuilder.to_string (StringBuilder.from_string "é日本😀"))
"#,
    );
    assert_eq!(
        run(&lowered),
        "Label \"é😀\"\n\"é😀\\0\\t\\n\\r\\\"\\\\\"\n-42\n(True, '\\u{e9}', \"日本\")\né日本😀\n"
    );
}
