use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use inkwell::context::Context;
use stapler::{CodeGenerator, NameResolver, ProgramLoader, TypeChecker};

struct Fixture {
    root: PathBuf,
}

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow epoch")
            .as_nanos();
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "stapler-modules-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("fixture directory should be created");
        Self { root }
    }

    fn write(&self, relative: &str, source: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("module directory should be created");
        }
        fs::write(path, source).expect("module should be written");
    }

    fn compile(&self) -> Result<String, String> {
        let program = ProgramLoader::new()
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_path(&self.root.join("main.sta"))?;
        let resolved = NameResolver::new()
            .resolve_program(program)
            .map_err(format_diagnostics)?;
        let typed = TypeChecker::new()
            .check(resolved)
            .map_err(format_diagnostics)?;
        let context = Context::create();
        CodeGenerator::new(&context)
            .compile_module(&typed)
            .map_err(format_diagnostics)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn format_diagnostics(diagnostics: Vec<stapler::Diagnostic>) -> String {
    diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn imports_public_values_and_types_through_all_use_forms() {
    let fixture = Fixture::new();
    fixture.write(
        "math.sta",
        concat!(
            "pub type alias Number = I32\n",
            "pub def add: (I32, I32) -> I32 = (a: I32, b: I32) => a\n",
            "pub let forty = 40\n",
            "let hidden = 2\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use math\n",
            "use math.(Number)\n",
            "use math.add as plus\n",
            "let first: math.Number = math.add (1, 2)\n",
            "let second: Number = plus (first, math.forty)\n",
            "second\n",
        ),
    );

    let llvm = fixture.compile().expect("imports should compile");
    assert!(llvm.contains("__staple_m1_add"));
    assert!(llvm.contains("define i32 @main()"));
}

#[test]
fn imports_public_traits_and_discovers_loaded_global_implementations() {
    let fixture = Fixture::new();
    fixture.write(
        "traits.sta",
        "pub trait Increment = T => { increment: T -> T }\n",
    );
    fixture.write(
        "implementations.sta",
        concat!(
            "use traits\n",
            "impl traits.Increment I32 { def increment = value => value + 1 }\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use traits.(Increment)\n",
            "use implementations\n",
            "def twice: T => Increment T => T -> T = value => increment (increment value)\n",
            "let answer: I32 = twice 40\n",
            "let qualified: I32 = Increment.increment answer\n",
        ),
    );

    fixture
        .compile()
        .expect("public traits and loaded global implementations should compile");

    for source in [
        concat!(
            "use traits\n",
            "use implementations\n",
            "def apply: T => traits.Increment T => T -> T = value => traits.Increment.increment value\n",
            "let answer: I32 = apply 41\n",
        ),
        concat!(
            "use traits.Increment as Inc\n",
            "use implementations\n",
            "def apply: T => Inc T => T -> T = value => Inc.increment value\n",
            "let answer: I32 = apply 41\n",
        ),
        concat!(
            "use traits.*\n",
            "use implementations\n",
            "def apply: T => Increment T => T -> T = value => increment value\n",
            "let answer: I32 = apply 41\n",
        ),
    ] {
        fixture.write("main.sta", source);
        fixture
            .compile()
            .expect("every public trait import form should compile");
    }

    fixture.write(
        "main.sta",
        concat!(
            "use traits.(Increment)\n",
            "let answer: I32 = increment 41\n",
        ),
    );
    let error = fixture
        .compile()
        .expect_err("implementations in unloaded modules must not be discovered");
    assert!(error.contains("no trait implementation or matching bound"));
}

#[test]
fn monomorphizes_imported_generic_functions_but_keeps_constructors_private() {
    let fixture = Fixture::new();
    fixture.write(
        "values.sta",
        concat!(
            "pub type UserId = I32\n",
            "pub def identity: T => T -> T = x => x\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use values.*\n",
            "let answer: I32 = identity 42\n",
            "let text: String = identity \"hello\"\n",
        ),
    );
    let llvm = fixture
        .compile()
        .expect("public generic functions should specialize");
    assert!(llvm.matches("identity__").count() >= 2);

    fixture.write(
        "main.sta",
        concat!("use values.*\n", "let user: UserId = UserId 42\n"),
    );
    let error = fixture
        .compile()
        .expect_err("a public type must not export its constructor");
    assert!(error.contains("unknown name `UserId`"));
}

#[test]
fn exports_constructors_and_destructors_for_public_representations() {
    let fixture = Fixture::new();
    fixture.write("boxes.sta", "pub(repr) type Box = T => (value: T)\n");
    fixture.write(
        "main.sta",
        concat!(
            "use boxes\n",
            "let boxed: boxes.Box I32 = boxes.Box (value: 42)\n",
            "let boxes.Box (value) = boxed\n",
            "value\n",
        ),
    );
    fixture
        .compile()
        .expect("public representations should construct and destructure by namespace");

    fixture.write(
        "main.sta",
        concat!(
            "use boxes.(Box)\n",
            "let boxed: Box I32 = Box (value: 42)\n",
            "let Box (value) = boxed\n",
            "value\n",
        ),
    );
    fixture
        .compile()
        .expect("public representations should import through selection");

    fixture.write(
        "main.sta",
        concat!(
            "use boxes.Box as Wrapped\n",
            "let boxed: Wrapped I32 = Wrapped (value: 42)\n",
            "let Wrapped (value) = boxed\n",
            "value\n",
        ),
    );
    fixture
        .compile()
        .expect("public representations should import through renaming");

    fixture.write(
        "main.sta",
        concat!(
            "use boxes.*\n",
            "let boxed: Box I32 = Box (value: 42)\n",
            "let Box (value) = boxed\n",
            "value\n",
        ),
    );
    fixture
        .compile()
        .expect("public representations should import through a glob");
}

#[test]
fn composes_sum_variants_across_modules() {
    let fixture = Fixture::new();
    fixture.write(
        "errors.sta",
        concat!(
            "pub(repr) type IOError = String\n",
            "pub(repr) type ParseError = String\n",
            "pub def read: String -> Ok String | IOError = path => Ok(path)\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use errors.*\n",
            "def parse = (path: String) => { let Ok(file)? = read(path); Ok(file) }\n",
            "let result: Ok String | IOError | ParseError = parse(\"input\")\n",
        ),
    );
    fixture
        .compile()
        .expect("sum alternatives should compose across module boundaries");
}

#[test]
fn rejects_private_components_of_public_representations() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!("type Hidden = I32\n", "pub(repr) type Exposed = Hidden\n",),
    );
    let error = fixture
        .compile()
        .expect_err("public representations must not expose private component types");
    assert!(error.contains("public representation references private type `Hidden`"));
}

#[test]
fn rejects_destructuring_an_imported_private_representation() {
    let fixture = Fixture::new();
    fixture.write(
        "ids.sta",
        concat!(
            "pub type UserId = I32\n",
            "pub def make: I32 -> UserId = UserId\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!("use ids.*\n", "let UserId value = make 42\n",),
    );
    let error = fixture
        .compile()
        .expect_err("ordinary public types keep their representations private");
    assert!(error.contains("the representation of `UserId` is private"));
}

#[test]
fn resolves_mutually_recursive_module_namespaces() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use ma\nma.a (1)\n");
    fixture.write(
        "ma.sta",
        concat!(
            "use mb\n",
            "pub def a: (I32) -> I32 = (n: I32) => mb.b (n)\n",
        ),
    );
    fixture.write(
        "mb.sta",
        concat!(
            "use ma\n",
            "pub def b: (I32) -> I32 = (n: I32) => ma.a (n)\n",
        ),
    );

    let llvm = fixture
        .compile()
        .expect("mutually recursive modules should compile");
    assert!(llvm.contains("__staple_m1_a"));
    assert!(llvm.contains("__staple_m2_b"));
}

#[test]
fn rejects_imports_of_private_items() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use values.(hidden)\nhidden\n");
    fixture.write("values.sta", "let hidden = 1\n");

    let error = fixture
        .compile()
        .expect_err("private item must not be imported");
    assert!(error.contains("no public item named `hidden`"));
}

#[test]
fn emits_dependency_initializers_before_the_entry_initializer() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use dependency.*\nvalue\n");
    fixture.write("dependency.sta", "pub let value = 1\n");

    let llvm = fixture.compile().expect("program should compile");
    let main = llvm
        .split("define i32 @main()")
        .nth(1)
        .expect("main should exist");
    let dependency = main
        .find("@__staple_init_m1")
        .expect("dependency init call");
    let entry = main.find("@__staple_init_m0").expect("entry init call");
    assert!(dependency < entry);
}

#[test]
fn resolves_every_module_path_from_the_entry_directory() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use package.first.*\nanswer\n");
    fixture.write(
        "package/first.sta",
        "use shared.*\npub let answer = shared_answer\n",
    );
    fixture.write("shared.sta", "pub let shared_answer = 42\n");

    fixture
        .compile()
        .expect("nested imports should remain entry-relative");
}

#[test]
fn mangles_a_source_binding_named_main_away_from_the_generated_entry_point() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "def main = () => 1\nmain ()\n");

    let llvm = fixture.compile().expect("source main should compile");
    assert!(llvm.contains("define i32 @__staple_m0_main(ptr %0)"));
    assert!(llvm.contains("define i32 @main()"));
}

#[test]
fn imports_public_extern_bindings() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        "use ffi.(puts)\nuse std.cinterop.(c_string)\nputs (c_string \"hello\")\n",
    );
    fixture.write(
        "ffi.sta",
        "use std.cinterop.*\npub extern \"c\" { let puts: (CPointer CChar) -> I32 }\n",
    );

    let llvm = fixture.compile().expect("public extern should import");
    assert!(llvm.contains("declare i32 @puts"));
}

#[test]
fn imports_primitive_macros_through_namespace_and_renaming() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use std.cinterop\n",
            "use std.cinterop.c_string as cs\n",
            "let first: cinterop.CString = cinterop.c_string \"first\"\n",
            "let second: cinterop.CString = cs \"second\"\n",
        ),
    );

    let llvm = fixture.compile().expect("macro imports should compile");
    assert!(llvm.contains("c\"first\\00\""));
    assert!(llvm.contains("c\"second\\00\""));
}

#[test]
fn rejects_user_declarations_with_the_intrinsic_abi() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        "extern \"staple-intrinsic\" { let fake: (I32, I32) -> I32 }\n",
    );

    let error = fixture
        .compile()
        .expect_err("the intrinsic ABI must be reserved");
    assert!(error.contains("reserved for the standard library"));
}

#[test]
fn imports_operator_values_and_their_fixities() {
    let fixture = Fixture::new();
    fixture.write(
        "math.sta",
        concat!(
            "pub def infixl 6 +: I32 -> I32 -> I32 = x => y => x\n",
            "pub def infixr 5 **: I32 -> I32 -> I32 = x => y => y\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use math\n",
            "use math.+ as combine\n",
            "use math.((**))\n",
            "1 `combine` 2\n",
            "1 ** 2 ** 3\n",
            "1 math.+ 2\n",
            "(math.+) 1 2\n",
        ),
    );

    let llvm = fixture
        .compile()
        .expect("imported operators and fixities should compile");
    assert!(llvm.contains("operator.2b"));
    assert!(llvm.contains("operator.2a2a"));
}

#[test]
fn loads_an_imported_top_level_global_from_a_function() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use values.*\n",
            "def get: () -> I32 = () => value\n",
            "get ()\n",
        ),
    );
    fixture.write("values.sta", "pub let value = 42\n");

    let llvm = fixture
        .compile()
        .expect("function should load imported global");
    assert!(llvm.contains("load i32, ptr @__staple_m1_value"));
}

#[test]
fn infers_imported_values_across_a_module_cycle() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use ma.*\na\n");
    fixture.write("ma.sta", "use mb.*\npub let a = b\n");
    fixture.write("mb.sta", "use ma\npub let b = 41\n");

    fixture
        .compile()
        .expect("cross-module value type should be inferred");
}
