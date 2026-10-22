//! Runs matches whose patterns inspect a sum without taking its payload:
//! typed wildcards select one alternative, and a match that binds nothing
//! non-`Copy` borrows a named subject instead of consuming it.

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

fn check(source: &str) -> Result<staple_compiler::TypedModule, Vec<String>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let resolved = NameResolver::new()
        .resolve_program(program)
        .unwrap_or_else(|errors| panic!("resolve: {errors:?}"));
    TypeChecker::new().check(resolved).map_err(|errors| {
        errors
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect()
    })
}

fn lower(source: &str) -> LoweredModule {
    let typed = check(source).unwrap_or_else(|errors| panic!("check: {errors:?}"));
    Lowerer::new()
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"))
}

/// Builds and runs a program, returning its standard output.
#[cfg(unix)]
fn run(source: &str) -> String {
    let lowered = lower(source);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-match-patterns-{}-{nonce}",
        std::process::id()
    ));
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

/// Declares a `Tag` whose drop prints its identifier.
const TAG: &str = concat!(
    "use std.cinterop.*\n",
    "use std.ptr.*\n",
    "extern \"c\" {\n",
    "    printf: (RawPtr<CChar>, ...) -> I32\n",
    "}\n",
    "type Tag wrap I32\n",
    "impl ! Copy Tag {}\n",
    "impl Drop Tag {\n",
    "    cleanup = Tag id => {\n",
    "        printf (c_string \"dropped %d\\n\", id)\n",
    "        ()\n",
    "    }\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn typed_wildcards_select_their_alternative() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "def kind = move value: Tag | I32 => I32 :: match value {\n",
            "    _: Tag => 1,\n",
            "    _: I32 => 2,\n",
            "}\n",
            "def scoped = () => () :: {\n",
            "    printf (c_string \"tag %d\\n\", kind (Tag 3))\n",
            "    printf (c_string \"number %d\\n\", kind 7)\n",
            "    ()\n",
            "}\n",
            "scoped ()\n",
            "printf (c_string \"done\\n\")\n",
        ),
    );
    // The selected `Tag` payload is discarded, and so dropped, by the wildcard.
    assert_eq!(run(&source), "dropped 3\ntag 1\nnumber 2\ndone\n");
}

#[cfg(unix)]
#[test]
fn inspecting_matches_borrow_their_subject() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "def inspect = <T, E> value: Result<T, E> => Bool :: match value {\n",
            "    Ok _ => True,\n",
            "    _ => False,\n",
            "}\n",
            "def code = flag: Bool => I32 :: match flag { True => 1, False => 0 }\n",
            "def scoped = () => () :: {\n",
            "    let held: Result<Tag, I32> = Ok (Tag 1)\n",
            "    let failed: Result<Tag, I32> = 5\n",
            "    printf (c_string \"inspect %d %d\\n\", code (inspect held), code (inspect failed))\n",
            "    let local: Result<Tag, I32> = Ok (Tag 2)\n",
            "    let first = match local { Ok _ => 10, _ => 20 }\n",
            "    let again = match local { Ok _ => 30, _ => 40 }\n",
            "    printf (c_string \"local %d %d\\n\", first, again)\n",
            "    let moved: Result<Tag, I32> = Ok (Tag 4)\n",
            "    let taken = match moved { Ok tag => tag, _ => Tag 5 }\n",
            "    printf (c_string \"end\\n\")\n",
            "    ()\n",
            "}\n",
            "scoped ()\n",
            "printf (c_string \"done\\n\")\n",
        ),
    );
    // Borrowed subjects drop once, with their binding, at the end of scope.
    assert_eq!(
        run(&source),
        "inspect 1 0\nlocal 10 30\nend\ndropped 4\ndropped 2\ndropped 1\ndone\n"
    );
}

#[test]
fn matches_binding_a_non_copy_payload_still_require_ownership() {
    let errors = check(concat!(
        "def take = <T, E> value: Result<T, E> => T :: match value {\n",
        "    Ok inner => inner,\n",
        "    _ => take value,\n",
        "}\n",
    ))
    .err()
    .expect("binding a borrowed payload must be rejected");
    assert!(
        errors
            .iter()
            .any(|message| message.contains("cannot move out of a borrowed value")),
        "{errors:?}"
    );
}
