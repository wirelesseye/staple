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

const AMOUNTS: &str = concat!(
    "trait Measure T { measure: T -> USize }\n",
    "trait Weigh T { weigh: T -> USize }\n",
    "type Meters = wrap I32\n",
    "type Grams = wrap I32\n",
    "impl Measure Meters { measure = value => 4 }\n",
    "impl Weigh Grams { weigh = value => 9 }\n",
    "companion<T where Measure T> T {\n",
    "    pub def amount = value: T => USize :: Measure.measure value\n",
    "}\n",
    "companion<T where Weigh T> T {\n",
    "    pub def amount = value: T => USize :: Weigh.weigh value * 10\n",
    "}\n",
);

#[cfg(unix)]
#[test]
fn blanket_members_of_one_name_may_cover_disjoint_types() {
    let source = format!(
        "{AMOUNTS}{}",
        concat!(
            "use std.io.println\n",
            "println \"${Meters 1 |> amount} ${Grams 1 |> amount}\"\n",
            "println \"${Meters.amount (Meters 1)} ${Grams.amount (Grams 1)}\"\n",
        )
    );
    assert_eq!(run(&source), "4 90\n4 90\n");
}

#[test]
fn blanket_members_of_one_name_may_not_cover_a_common_type() {
    let errors = rejected(&format!(
        "{AMOUNTS}impl Weigh Meters {{ weigh = value => 1 }}\n"
    ));
    assert!(
        errors.contains(
            "blanket companion member `amount` overlaps another blanket companion member `amount`"
        ),
        "{errors}"
    );
    // The same bound always covers a common type.
    let errors = rejected(&format!(
        "{MEASURE}companion<U where Measure U> U {{ pub def double_size = value: U => USize :: 1 }}\n"
    ));
    assert!(
        errors.contains("overlaps another blanket companion member `double_size`"),
        "{errors}"
    );
}

#[test]
fn a_receiver_satisfying_two_blanket_members_is_ambiguous() {
    let errors = rejected(&format!(
        "{AMOUNTS}def both = <G where Measure G, Weigh G> value: G => USize :: value |> amount\n"
    ));
    assert!(errors.contains("ambiguous pipe stage `amount`"), "{errors}");
    let errors = rejected(&format!(
        "{AMOUNTS}type Plain = wrap I32\nlet plain: USize = Plain.amount (Plain 1)\n"
    ));
    assert!(
        errors.contains("no blanket companion member `amount` applies to `"),
        "{errors}"
    );
}

#[test]
fn try_helpers_are_blanket_members_another_package_trait_can_share() {
    // `unwrap` is no longer a `Try` member an implementation could replace.
    let errors = rejected(concat!(
        "type Flag = wrap Bool\n",
        "impl Try Flag {\n",
        "    type Output = Bool\n",
        "    type Residual = None\n",
        "    branch = move flag => Ok true\n",
        "    unwrap = move flag => true\n",
        "}\n",
    ));
    assert!(errors.contains("unwrap"), "{errors}");
    // A trait of its own may offer `unwrap` for types that are not `Try`.
    check(concat!(
        "trait Lazy T { force: move T -> I32 }\n",
        "type Thunk = wrap I32\n",
        "impl Lazy Thunk { force = move Thunk value => value }\n",
        "companion<T where Lazy T> T {\n",
        "    pub def unwrap = move value: T => I32 :: Lazy.force value\n",
        "}\n",
        "let forced: I32 = Thunk 1 |> unwrap\n",
        "let some: Option I32 = Some 2\n",
        "let tried: I32 = some |> unwrap\n",
    ))
    .unwrap_or_else(|errors| panic!("{errors}"));
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

const BAG: &str = concat!(
    "trait Bag T { type Item; first: T -> Item }\n",
    "type Pair = wrap (left: I32, right: I32)\n",
    "impl Bag Pair { type Item = I32; first = Pair (left, right) => left }\n",
    "type Box T = wrap T\n",
);

#[test]
fn companion_members_may_declare_their_own_bounded_parameters() {
    // Regression test: a member value declaring `<R where Bag R>` stored only
    // that bound as the function's declared bounds, dropping the companion's
    // `Bag T` that its scheme carries, and the checker's consistency check
    // panicked. This held for blanket and ordinary generic companions alike.
    let source = format!(
        "{BAG}{}",
        concat!(
            "use std.io.println\n",
            "companion<T where Bag T> T {\n",
            "    pub def with_other = <R where Bag R> (value: T; other: R) => (Bag.Item T, Bag.Item R) ::\n",
            "        (Bag.first value, Bag.first other)\n",
            "}\n",
            "companion<T where Bag T> Box T {\n",
            "    pub def boxed_with = <R where Bag R> (move value: Box T; other: R) => Bag.Item R ::\n",
            "        Bag.first other\n",
            "}\n",
            "def run = () => {\n",
            "    let (a, b) = Pair (left: 1, right: 2) |> with_other (Pair (left: 3, right: 4))\n",
            "    let c = Box (Pair (left: 5, right: 6)) |> boxed_with (Pair (left: 7, right: 8))\n",
            "    println \"${a} ${b} ${c}\"\n",
            "}\n",
            "run ()\n",
        )
    );
    assert_eq!(run(&source), "1 3 7\n");
}

#[test]
fn blanket_bounds_on_an_associated_type_hold_at_the_receiver() {
    // Regression test: pinning a blanket member to its receiver substituted
    // the blanket parameter into `Add (Bag.Item T)` but left `Bag.Item T` a
    // projection, so the bound was reported unsatisfied for `Pair`.
    let source = format!(
        "{BAG}{}",
        concat!(
            "use std.io.println\n",
            "use std.ops.(Add)\n",
            "use std.default.(Default)\n",
            "companion<T where Bag T, Add (Bag.Item T), Default (Bag.Item T)> T {\n",
            "    pub def plus_default = value: T => Bag.Item T ::\n",
            "        Bag.first value + (Bag.Item T :: Default.default ())\n",
            "}\n",
            "def run = () => {\n",
            "    println \"pipe: ${Pair (left: 3, right: 4) |> plus_default}\"\n",
            "    println \"named: ${Pair.plus_default (Pair (left: 5, right: 6))}\"\n",
            "}\n",
            "run ()\n",
        )
    );
    assert_eq!(run(&source), "pipe: 3\nnamed: 5\n");
}
