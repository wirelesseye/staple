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
            .map(|diagnostic| format!("{diagnostic:?}"))
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

#[cfg(unix)]
#[test]
fn automatic_handle_patterns_borrow_or_consume_on_demand() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "use std.unique.(Unique)\n",
            "use std.shared.(Shared)\n",
            "def inspect = value: Tag => I32 :: match value { Tag id => id }\n",
            "def read = value: Unique<Option<Tag>> => I32 :: match value { Some tag => inspect tag, None() => 0 }\n",
            "def extract = move value: Unique<Option<Tag>> => Tag :: match value { Some tag => tag, None() => Tag 0 }\n",
            "def read_shared = value: Shared<Option<Tag>> => I32 :: match value { Some tag => inspect tag, None() => 0 }\n",
            "def scoped = () => () :: {\n",
            "  let initial: Option<Tag> = Some (Tag 1)\n",
            "  let owner = Unique initial\n",
            "  printf (c_string \"read %d %d\\n\", read owner, read owner)\n",
            "  let extracted = extract owner\n",
            "  let initial_shared: Option<Tag> = Some (Tag 2)\n",
            "  let shared = Shared initial_shared\n",
            "  printf (c_string \"shared %d\\n\", read_shared shared)\n",
            "  let mut scalar = Unique 3\n",
            "  let old = Unique.replace (scalar, 4)\n",
            "  let mut cell = Shared 5\n",
            "  let previous = Shared.replace (cell, 6)\n",
            "  printf (c_string \"replace %d %d %d %d\\n\", old, previous, Unique.get scalar, Shared.get cell)\n",
            "  ()\n",
            "}\n",
            "scoped ()\n",
        )
    );
    assert_eq!(
        run(&source),
        "read 1 1\nshared 2\nreplace 3 5 4 6\ndropped 1\n"
    );
}

#[test]
fn shared_payloads_cannot_be_extracted_by_automatic_patterns() {
    let errors = check(concat!(
        "use std.shared.(Shared)\n",
        "type Token wrap I32\n",
        "impl !Copy Token {}\n",
        "def extract = value: Shared<Option<Token>> => Token :: match value { Some token => token, None() => Token 0 }\n",
    )).err().expect("Shared payload extraction must fail");
    assert!(
        errors
            .iter()
            .any(|message| message.contains("cannot move out of a borrowed value")),
        "{errors:?}"
    );
}

#[cfg(unix)]
#[test]
fn consuming_handle_patterns_drop_unselected_fields_once() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "use std.unique.(Unique)\n",
            "def take: move Unique<(Tag, Tag)> -> Tag = move owner => match owner { (first, _) => first }\n",
            "def scoped = () => () :: {\n",
            "  let extracted = take (Unique.new (Tag 1, Tag 2))\n",
            "  printf (c_string \"extracted\\n\")\n",
            "  let mut owner = Unique.new (Tag 3)\n",
            "  let previous = Unique.replace (owner, Tag 4)\n",
            "  let current = Unique.into_inner owner\n",
            "  printf (c_string \"replaced\\n\")\n",
            "  ()\n",
            "}\n",
            "scoped ()\n",
        )
    );
    assert_eq!(
        run(&source),
        "dropped 2\nextracted\nreplaced\ndropped 4\ndropped 3\ndropped 1\n"
    );
}

#[test]
fn borrowed_matches_protect_their_owner() {
    for body in [
        "drop owner; inspect tag",
        "Unique.replace (owner, Some (Tag 2)); inspect tag",
    ] {
        let source = format!(
            "{TAG}\nuse std.unique.(Unique)\ndef inspect = value: Tag => () :: ()\ndef bad = () => {{ let initial: Option<Tag> = Some (Tag 1); let mut owner = Unique initial; match owner {{ Some tag => {{ {body} }}, None() => () }} }}\n"
        );
        let errors = check(&source).err().expect("owner must remain borrowed");
        assert!(
            errors
                .iter()
                .any(|message| message.contains("match borrows its payload")),
            "{errors:?}"
        );
    }
}

#[test]
fn replace_rejects_slotted_application() {
    for ty in ["Shared", "Unique"] {
        let module = ty.to_ascii_lowercase();
        for call in [
            format!("{ty}.replace value 2"),
            "value |> replace 2".to_owned(),
        ] {
            let source = format!(
                "use std.{module}.({ty})\ndef bad = () => {{ let mut value = {ty} 1; {call} }}\n"
            );
            assert!(
                check(&source).is_err(),
                "{ty}.replace must require a product argument"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn nested_handle_patterns_work_with_dynamic_payloads() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "use std.unique.(Unique)\n",
            "use std.shared.(Shared)\n",
            "def extract: <T where Sized T> move Unique<Unique<T>> -> T = <T where Sized T> move owner => match owner { value: T => value }\n",
            "def unwrap: <T where Sized T> move Unique<T> -> T = <T where Sized T> move owner => Unique.into_inner owner\n",
            "def swap: <T where Sized T> (mut Unique<T>, move T) -> T = <T where Sized T> (mut owner, move value) => Unique.replace (owner, value)\n",
            "def read = owner: Unique<Shared<Option<I32>>> => I32 :: match owner { Some value => value, None() => 0 }\n",
            "def scoped = () => () :: {\n",
            "  let initial: Option<I32> = Some 42\n",
            "  let nested = Unique (Shared initial)\n",
            "  printf (c_string \"nested %d %d\\n\", read nested, read nested)\n",
            "  let extracted = unwrap (Unique (extract (Unique (Unique (Tag 1)))))\n",
            "  let mut owner = Unique (Tag 2)\n",
            "  let previous = swap (owner, Tag 3)\n",
            "  let unit: () = unwrap (Unique ())\n",
            "  let number: I32 = unwrap (Unique 7)\n",
            "  ()\n",
            "}\n",
            "scoped ()\n",
        )
    );
    assert_eq!(
        run(&source),
        "nested 42 42\ndropped 2\ndropped 3\ndropped 1\n"
    );
}

#[cfg(unix)]
#[test]
fn handle_projection_composes_with_implicit_wrapper_projection() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "use std.unique.(Unique)\n",
            "type State from Tag | I32\n",
            "def extract: move Unique<State> -> Tag = move owner => match owner { tag: Tag => tag, _: I32 => Tag 0 }\n",
            "def scoped = () => () :: {\n",
            "  let state: State = Tag 1\n",
            "  let value = extract (Unique state)\n",
            "  ()\n",
            "}\n",
            "scoped ()\n",
        )
    );
    assert_eq!(run(&source), "dropped 1\n");
}

#[test]
fn shared_payload_borrows_reject_replacement_through_an_alias() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "use std.shared.(Shared)\n",
            "def inspect = tag: Tag => () :: ()\n",
            "def bad = () => {\n",
            "  let initial: Option<Tag> = Some (Tag 1)\n",
            "  let shared = Shared initial\n",
            "  let mut alias = shared\n",
            "  match shared { Some tag => { let replacement: Option<Tag> = None; Shared.replace (alias, replacement); inspect tag }, None() => () }\n",
            "}\n",
        )
    );
    let errors = check(&source)
        .err()
        .expect("alias replacement must be rejected during a payload borrow");
    assert!(
        errors
            .iter()
            .any(|message| message.contains("match borrows its payload")),
        "{errors:?}"
    );
}

#[cfg(unix)]
#[test]
fn mutable_non_copy_pattern_bindings_require_ownership() {
    let source = format!(
        "{TAG}{}",
        concat!(
            "use std.unique.(Unique)\n",
            "def scoped = () => () :: {\n",
            "  let initial: Option<Tag> = Some (Tag 1)\n",
            "  let owner = Unique initial\n",
            "  match owner { Some mut tag => { tag = Tag 2; () }, None() => () }\n",
            "  ()\n",
            "}\n",
            "scoped ()\n",
        )
    );
    assert_eq!(run(&source), "dropped 1\ndropped 2\n");
}
