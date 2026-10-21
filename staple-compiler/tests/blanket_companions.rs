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
        "staple-blanket-companions-{}-{nonce}",
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

const MEASURE: &str = concat!(
    "trait Measure T { measure: T -> USize }\n",
    "type Meters = wrap I32\n",
    "type Box A = wrap A\n",
    "impl Measure Meters { measure = value => 4 }\n",
    "impl Measure I32 { measure = value => 2 }\n",
    "impl<A> Measure (Box A) { measure = value => 8 }\n",
    "companion<T where Measure T> T {\n",
    "    pub def double_size = value: T => USize :: Measure.measure value * 2\n",
    "    pub def plus = (value: T; extra: USize) => USize :: Measure.measure value + extra\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn blanket_companion_members_apply_to_every_satisfying_type() {
    let source = format!(
        "{MEASURE}{}",
        concat!(
            "use std.io.println\n",
            "def generic = <G where Measure G> value: G => USize :: value |> double_size\n",
            "def run = () => {\n",
            "    println \"pipe: ${Meters 1 |> double_size}\"\n",
            "    println \"builtin pipe: ${3 |> double_size}\"\n",
            "    println \"slotted pipe: ${Meters 1 |> plus 5}\"\n",
            "    println \"generic type pipe: ${Box \"text\" |> double_size}\"\n",
            "    println \"generic receiver: ${generic (Box 1)}\"\n",
            "    println \"named: ${Meters.double_size (Meters 2)}\"\n",
            "    println \"builtin named: ${I32.double_size 7}\"\n",
            "    println \"generic named: ${Box.double_size (Box 1)}\"\n",
            "    println \"slotted named: ${Meters.plus (Meters 2) 1}\"\n",
            "    let value = Meters.double_size\n",
            "    println \"value: ${value (Meters 3)}\"\n",
            "}\n",
            "run ()\n",
        )
    );
    assert_eq!(
        run(&source),
        concat!(
            "pipe: 8\n",
            "builtin pipe: 4\n",
            "slotted pipe: 9\n",
            "generic type pipe: 16\n",
            "generic receiver: 16\n",
            "named: 8\n",
            "builtin named: 4\n",
            "generic named: 16\n",
            "slotted named: 5\n",
            "value: 8\n",
        )
    );
}

#[test]
fn named_blanket_access_fixes_the_blanket_parameter_to_the_type() {
    let errors = rejected(&format!(
        "{MEASURE}let wrong: USize = Meters.double_size 3\n"
    ));
    assert!(errors.contains("expected `"), "{errors}");
    let errors = rejected(&format!("{MEASURE}let wrong: USize = Box.double_size 3\n"));
    assert!(errors.contains("expected `"), "{errors}");
}

#[test]
fn blanket_companion_members_require_their_bounds() {
    let errors = rejected(&format!(
        "{MEASURE}let unmeasured: USize = String.double_size \"s\"\n"
    ));
    assert!(errors.contains("trait bound is not satisfied"), "{errors}");
    let errors = rejected(&format!(
        "{MEASURE}let unmeasured: USize = \"s\" |> double_size\n"
    ));
    assert!(errors.contains("trait bound is not satisfied"), "{errors}");
}

#[test]
fn a_blanket_companion_is_not_a_module_named_after_its_parameter() {
    let errors = rejected(&format!(
        "{MEASURE}let size: USize = T.double_size (Meters 1)\n"
    ));
    assert!(errors.contains("unknown name `T`"), "{errors}");
}

#[test]
fn a_blanket_companion_needs_a_trait_bound_on_its_parameter() {
    let errors = rejected("companion<T> T { pub def anything = value: T => 1 }\n");
    assert!(
        errors.contains("a blanket companion needs a trait bound on `T`"),
        "{errors}"
    );
}

#[test]
fn a_blanket_companion_obeys_the_orphan_rule() {
    // `Iterator` belongs to the standard library, so only the standard
    // library may add blanket members to every iterator.
    let errors = rejected(concat!(
        "use std.iterator.Iterator\n",
        "companion<T where Iterator T> T {\n",
        "    pub def size = move iter: T => USize :: iter |> count\n",
        "}\n",
    ));
    assert!(
        errors.contains(
            "a blanket companion must bound `T` by a trait defined in the current package"
        ),
        "{errors}"
    );
}

#[test]
fn blanket_companion_member_names_are_unique() {
    let errors = rejected(&format!(
        "{MEASURE}{}",
        concat!(
            "trait Other T {}\n",
            "companion<U where Other U> U { pub def double_size = value: U => USize :: 1 }\n",
        )
    ));
    assert!(
        errors.contains(
            "blanket companion member `double_size` is already declared by another blanket companion"
        ),
        "{errors}"
    );
}

#[test]
fn a_companion_member_may_not_overlap_an_applicable_blanket_member() {
    let errors = rejected(&format!(
        "{MEASURE}companion Meters {{ pub def double_size = value: Meters => USize :: 1 }}\n"
    ));
    assert!(
        errors.contains("companion member `double_size` of `"),
        "{errors}"
    );
    // A generic type overlaps when an implementation could cover it.
    let errors = rejected(&format!(
        "{MEASURE}companion<A> Box A {{ pub def double_size = value: Box A => USize :: 1 }}\n"
    ));
    assert!(
        errors.contains("overlaps the blanket companion member"),
        "{errors}"
    );
    // Implementations declared after the companion count too.
    let errors = rejected(&format!(
        "{MEASURE}{}",
        concat!(
            "companion Later { pub def double_size = value: Later => USize :: 1 }\n",
            "type Later = wrap I32\n",
            "impl Measure Later { measure = value => 1 }\n",
        )
    ));
    assert!(
        errors.contains("overlaps the blanket companion member"),
        "{errors}"
    );
}

#[test]
fn a_companion_member_may_share_a_name_with_an_inapplicable_blanket_member() {
    check(&format!(
        "{MEASURE}{}",
        concat!(
            "type Feet = wrap I32\n",
            "type Bag A = wrap A\n",
            "companion Feet { pub def double_size = value: Feet => USize :: 1 }\n",
            "companion<A> Bag A { pub def double_size = value: Bag A => USize :: 1 }\n",
            "let feet: USize = Feet 1 |> double_size\n",
            "let bag: USize = Bag.double_size (Bag 1)\n",
        )
    ))
    .unwrap_or_else(|errors| panic!("{errors}"));
}

#[test]
fn a_private_blanket_member_is_reachable_only_from_its_module() {
    let source = concat!(
        "mod inner {\n",
        "    pub trait Measure T { measure: T -> USize }\n",
        "    impl Measure I32 { measure = value => 2 }\n",
        "    companion<T where Measure T> T {\n",
        "        def hidden = value: T => USize :: Measure.measure value\n",
        "    }\n",
        "    pub let inside: USize = 1 |> hidden\n",
        "}\n",
    );
    check(source).unwrap_or_else(|errors| panic!("{errors}"));
    let errors = rejected(&format!("{source}let outside: USize = 1 |> hidden\n"));
    assert!(
        errors.contains(
            "no compatible local function, accessible companion, or visible trait method"
        ),
        "{errors}"
    );
}
