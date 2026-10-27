mod common;

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

/// Resolves and checks `source`, returning every diagnostic as one string.
fn check(source: &str) -> Result<staple_compiler::TypedModule, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .unwrap_or_else(|error| panic!("load: {error}"));
    let messages = |diagnostics: Vec<staple_syntax::Diagnostic>| {
        diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(messages)?;
    TypeChecker::new().check(resolved).map_err(messages)
}

fn rejected(source: &str) -> String {
    match check(source) {
        Ok(_) => panic!("expected an error:\n{source}"),
        Err(messages) => messages,
    }
}

#[cfg(unix)]
fn run(source: &str) -> String {
    let typed = check(source).unwrap_or_else(|errors| panic!("check: {errors}"));
    let lowered: LoweredModule = Lowerer::new()
        .lower(&typed)
        .unwrap_or_else(|errors| panic!("lower: {errors:?}"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "staple-structural-companions-{}-{nonce}",
        std::process::id()
    ));
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
    assert!(
        output.status.success(),
        "execution: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

const TAGGED: &str = concat!(
    "type Tag wrap I32\n",
    "type Both = Tag | String\n",
    "type Tagged<T> = Tag | T\n",
    "companion<T> (Tag | T) {\n",
    "    pub def describe = value: Tag | T => String :: match value {\n",
    "        Tag _ => \"tag\",\n",
    "        _ => \"other\",\n",
    "    }\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn structural_companion_members_apply_to_matching_receivers() {
    let source = format!(
        "{TAGGED}{}",
        concat!(
            "use std.io.println\n",
            "let tag: Tag | String = Tag 1\n",
            "let text: Tag | String = \"s\"\n",
            "let wide: Tag | Bool | U8 = True\n",
            "println (tag |> describe)\n",
            "println (wide |> describe)\n",
            "println (Both.describe text)\n",
            "println (Tagged.describe tag)\n",
            "println (text |> Both.describe)\n",
        ),
    );
    assert_eq!(run(&source), "tag\nother\nother\ntag\nother\n");
}

#[test]
fn an_alias_target_is_the_same_companion_as_its_structural_type() {
    let errors = rejected(concat!(
        "type Tag wrap I32\n",
        "type Both = Tag | String\n",
        "companion Both { pub def size = () => I32 :: 1 }\n",
        "companion (String | Tag) { pub def size = () => I32 :: 2 }\n",
    ));
    assert!(
        errors
            .contains("overlaps another companion member `size`: a type could match both targets"),
        "{errors}"
    );
    check(concat!(
        "type Tag wrap I32\n",
        "type Both = Tag | String\n",
        "companion Both { pub def size = () => I32 :: 1 }\n",
        "companion (String | Tag) { pub def other = () => I32 :: 2 }\n",
        "let a: I32 = Both.size ()\n",
        "let b: I32 = Both.other ()\n",
    ))
    .unwrap_or_else(|errors| panic!("{errors}"));
}

#[test]
fn a_structural_companion_matches_its_target_exactly() {
    // A wider sum is a different type.
    let errors = rejected(concat!(
        "type Tag wrap I32\n",
        "companion (Tag | String) { pub def size = value: Tag | String => I32 :: 1 }\n",
        "let wide: Tag | String | Bool = True\n",
        "let size: I32 = wide |> size\n",
    ));
    assert!(
        errors.contains("no compatible local function, accessible companion"),
        "{errors}"
    );
}

#[test]
fn a_parameter_alternative_stands_for_at_least_one_alternative() {
    let errors = rejected(concat!(
        "type Tag wrap I32\n",
        "companion<T> (Tag | T) { pub def size = value: Tag | T => I32 :: 1 }\n",
        "let lone: Tag = Tag 1\n",
        "let size: I32 = lone |> size\n",
    ));
    assert!(
        errors.contains("no compatible local function, accessible companion"),
        "{errors}"
    );
}

#[test]
fn a_target_matching_a_receiver_two_ways_is_ambiguous() {
    let errors = rejected(concat!(
        "type Box<T> wrap T\n",
        "companion<T, E> (Box<T> | E) { pub def size = value: Box<T> | E => I32 :: 1 }\n",
        "let boxed: Box<I32> | Box<U8> = Box 1\n",
        "let size: I32 = boxed |> size\n",
    ));
    assert!(
        errors.contains("in more than one way, so its member `size` is ambiguous here"),
        "{errors}"
    );
}

#[test]
fn a_target_sum_has_at_most_one_parameter_alternative() {
    let errors = rejected(concat!(
        "type Tag wrap I32\n",
        "companion<A, B> (Tag | A | B) { pub def size = () => I32 :: 1 }\n",
    ));
    assert!(
        errors.contains("has more than one type parameter alternative"),
        "{errors}"
    );
}

#[test]
fn a_structural_member_may_not_overlap_a_blanket_member() {
    let errors = rejected(concat!(
        "trait Measure T { measure: T -> I32 }\n",
        "type Tag wrap I32\n",
        "impl Measure (Tag | String) { measure = value => 1 }\n",
        "companion<T where Measure T> T { pub def size = value: T => I32 :: 1 }\n",
        "companion (Tag | String) { pub def size = value: Tag | String => I32 :: 2 }\n",
    ));
    assert!(
        errors.contains("overlaps the blanket companion member `size`"),
        "{errors}"
    );
}

#[test]
fn a_structural_companion_needs_a_component_of_its_own_package() {
    // `Result<T, E>` is an alias of `Ok<T> | E`, so both spellings are the same
    // companion, and only the standard library declares its components.
    for target in [
        "<T, E> Result<T, E>",
        "<T, E> (Ok<T> | E)",
        " (Ok<I32> | String)",
    ] {
        let errors = rejected(&format!(
            "companion{target} {{ pub def size = () => I32 :: 1 }}\n"
        ));
        assert!(
            errors.contains("has no component declared in the current package"),
            "{target}: {errors}"
        );
    }
    check(concat!(
        "type Tag wrap I32\n",
        "companion (Ok<I32> | Tag) { pub def size = () => I32 :: 1 }\n",
    ))
    .unwrap_or_else(|errors| panic!("{errors}"));
}

#[test]
fn a_companion_of_another_package_s_nominal_type_is_rejected() {
    let errors = rejected(concat!(
        "use std.string.String\n",
        "type Text = String\n",
        "companion Text { pub def size = () => I32 :: 1 }\n",
    ));
    assert!(
        errors.contains("companion target `Text` is declared in another package"),
        "{errors}"
    );
}
