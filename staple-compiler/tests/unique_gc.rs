//! Runs programs that force collections while `Unique` payloads are live,
//! raw, or part of a cycle through managed references.

use inkwell::context::Context;
use staple_compiler::{CodeGenerator, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker};
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

/// Builds and runs a program, returning its standard output.
#[cfg(unix)]
fn run(source: &str) -> String {
    let lowered = lower(source);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base =
        std::env::temp_dir().join(format!("staple-unique-gc-{}-{nonce}", std::process::id()));
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

/// Allocates well past the 1 MiB collection threshold.
const CHURN: &str = concat!(
    "def churn = () => () :: {\n",
    "    for index in (0 .. 200000) {\n",
    "        let garbage = Ref (index, index, index, index)\n",
    "        ()\n",
    "    }\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn collects_unique_payloads_in_cycles_through_managed_references() {
    // Ref Holder -> Unique Payload -> Ref (Option (Ref Holder)) -> Ref Holder.
    let source = format!(
        "{}{}",
        concat!(
            "use std.cinterop.*\n",
            "use std.ptr.*\n",
            "extern \"c\" {\n",
            "    printf: (RawPtr CChar, ...) -> I32\n",
            "}\n",
            "type Holder = wrap Unique Payload\n",
            "type Payload = wrap (tag: I32, back: Ref (Option (Ref Holder)))\n",
            "impl Drop Payload {\n",
            "    cleanup = Payload (tag, back) => {\n",
            "        printf (c_string \"dropped\\n\")\n",
            "        ()\n",
            "    }\n",
            "}\n",
            "def make_cycle = tag: I32 => () :: {\n",
            "    let empty: Option (Ref Holder) = None\n",
            "    let mut back: Ref (Option (Ref Holder)) = Ref empty\n",
            "    let holder = Ref (Holder (Unique (Payload (tag: tag, back: back))))\n",
            "    let previous = Ref.replace back (Some holder)\n",
            "    ()\n",
            "}\n",
            "def make_cycles = () => () :: {\n",
            "    for index in (0 .. 100) { make_cycle index }\n",
            "}\n",
        ),
        CHURN,
    ) + concat!("make_cycles ()\n", "churn ()\n", "churn ()\n");
    let output = run(&source);
    let dropped = output.lines().filter(|line| *line == "dropped").count();
    // A conservative stack scan may retain a stray cycle, but a payload that
    // is its own root retains every one of them.
    assert!(
        dropped >= 90,
        "only {dropped} of 100 cyclic Unique payloads were collected"
    );
}

#[cfg(unix)]
#[test]
fn unique_payloads_survive_collections_while_owned_or_raw() {
    let source = format!(
        "{}{}",
        concat!(
            "use std.cinterop.*\n",
            "use std.ptr.*\n",
            "extern \"c\" {\n",
            "    printf: (RawPtr CChar, ...) -> I32\n",
            "}\n",
        ),
        CHURN,
    ) + concat!(
        "def check = () => () :: {\n",
        "    let first_pair: (Ref I32, Ref I32) = (Ref 41, Ref 1)\n",
        "    let second_pair: (Ref I32, Ref I32) = (Ref 40, Ref 2)\n",
        "    let owned: Unique (Ref I32, Ref I32) = Unique first_pair\n",
        "    let lent: Unique (Ref I32, Ref I32) = Unique second_pair\n",
        "    let raw = Unique.unsafe_into_raw lent\n",
        "    churn ()\n",
        "    let restored: Unique (Ref I32, Ref I32) = Unique.unsafe_from_raw raw\n",
        "    churn ()\n",
        "    let (Ref first, Ref second) = owned.*\n",
        "    let (Ref third, Ref fourth) = restored.*\n",
        "    printf (c_string \"%d %d\\n\", first + second, third + fourth)\n",
        "    ()\n",
        "}\n",
        "check ()\n",
    );
    assert_eq!(run(&source), "42 42\n");
}

#[cfg(unix)]
#[test]
fn collects_list_storage_in_cycles_through_managed_references() {
    // Ref Holder -> List Payload storage -> Ref (Option closure) -> closure
    // environment -> Ref Holder. A closure closes the cycle because a type
    // cannot recurse through `List`.
    let source = format!(
        "{}{}",
        concat!(
            "use std.cinterop.*\n",
            "use std.ptr.*\n",
            "extern \"c\" {\n",
            "    printf: (RawPtr CChar, ...) -> I32\n",
            "}\n",
            "type Payload = wrap (tag: I32, back: Ref (Option (() -> I32)))\n",
            "type Holder = wrap (items: List Payload)\n",
            "impl Drop Payload {\n",
            "    cleanup = Payload (tag, back) => {\n",
            "        printf (c_string \"dropped\\n\")\n",
            "        ()\n",
            "    }\n",
            "}\n",
            "def make_cycle = tag: I32 => () :: {\n",
            "    let empty: Option (() -> I32) = None\n",
            "    let mut back: Ref (Option (() -> I32)) = Ref empty\n",
            "    let mut items: List Payload = List.new ()\n",
            "    List.push items (Payload (tag: tag, back: back))\n",
            "    let holder: Ref Holder = Ref (Holder (items: items))\n",
            "    let closure: () -> I32 = () => { let kept = holder; tag }\n",
            "    let previous = Ref.replace back (Some closure)\n",
            "    ()\n",
            "}\n",
            "def make_cycles = () => () :: {\n",
            "    for index in (0 .. 100) { make_cycle index }\n",
            "}\n",
        ),
        CHURN,
    ) + concat!("make_cycles ()\n", "churn ()\n", "churn ()\n");
    let output = run(&source);
    let dropped = output.lines().filter(|line| *line == "dropped").count();
    assert!(
        dropped >= 90,
        "only {dropped} of 100 cyclic List payloads were collected"
    );
}
