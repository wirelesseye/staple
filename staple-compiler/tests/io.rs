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
fn run(source: &str, input: &[u8]) -> std::process::Output {
    use std::io::Write;
    use std::process::Stdio;
    let lowered = lower(source);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!("staple-io-{}-{nonce}", std::process::id()));
    let object = base.with_extension("o");
    let context = Context::create();
    CodeGenerator::new(&context)
        .emit_object(&lowered, &object, None)
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
    let mut child = Command::new(&base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    let _ = std::fs::remove_file(object);
    let _ = std::fs::remove_file(base);
    output
}

#[test]
#[cfg(unix)]
fn output_preserves_nul_utf8_and_separates_stderr() {
    let output = run(
        r#"
use std.io.*
use std.shared_buffer.SharedBuffer
print "a\0b"
println "é"
match "a\0b" { "a\0b" => (), _ => panic "NUL literal match failed", }
eprint "err\0or"
eprintln "!"
let _ = flush_stdout ()
let _ = flush_stderr ()
let mut out = stdout ()
let _ = write_string out "handle"
let _ = Write.flush out
let mut err = stderr ()
let fixed: (U8 ^ 5) = (98, 121, 116, 101, 115)
let bytes = SharedBuffer.from_array fixed
let _ = Write.write_all err bytes
"#,
        b"",
    );
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"a\0b\xc3\xa9\nhandle");
    assert_eq!(output.stderr, b"err\0or!\nbytes");
}

#[test]
#[cfg(unix)]
fn lines_distinguish_empty_eof_invalid_utf8_and_unterminated_input() {
    let output = run(
        r#"
use std.io.*
loop {
    match (read_line ()) {
        Ok (Some text) => println "[$text]",
        Ok None => { println "EOF"; break },
        InvalidUtf8 => println "INVALID",
        IoError => { println "ERROR"; break },
    }
}
"#,
        b"\nhello\r\n\xff\nA\0B\nlast",
    );
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        output.stdout,
        b"[]\n[hello]\nINVALID\n[A\0B]\n[last]\nEOF\n"
    );
}

#[test]
#[cfg(unix)]
fn panic_preserves_nul_and_uses_stderr() {
    let output = run("panic \"a\\0b\"\n", b"");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.starts_with(b"panic: a\0b at "),
        "{:?}",
        output
    );
}

#[test]
#[cfg(unix)]
fn stream_errors_are_returned() {
    let output = run(
        r#"
use std.io.*
extern "c" { close: I32 -> I32 }
let _ = close 0
match (read_line ()) {
    IoError => eprintln "read failed",
    _ => panic "expected read failure",
}
let _ = close 1
let _ = try_print "buffered"
match (flush_stdout ()) {
    IoError => eprintln "flush failed",
    Ok _ => panic "expected flush failure",
}
"#,
        b"",
    );
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stderr, b"read failed\nflush failed\n");
}

#[test]
#[cfg(unix)]
fn custom_readers_and_writers_use_the_same_helpers() {
    let output = run(
        r#"
use std.io.*
use std.shared_buffer.SharedBuffer
pub type Input wrap Bool
impl Read Input {
    read_byte = mut input => {
        if (input.*) { Ok (Option<U8> :: None) } else {
            input = Input True
            Ok (Option<U8> :: Some (U8 :: 65))
        }
    }
}
pub type Sink wrap USize
impl Write Sink {
    write_all = (mut sink; bytes) => {
        sink = Sink (sink.* + SharedBuffer.length bytes)
        Ok ()
    }
    flush = mut sink => Ok ()
}
let mut input = Input False
match (read_line_from input) {
    Ok (Some text) => println text,
    _ => panic "expected a line",
}
let mut sink = Sink 0
let _ = write_string sink "é\0"
let _ = Write.flush sink
println "${sink.*}"
"#,
        b"",
    );
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"A\n3\n");
}
