use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use inkwell::context::Context;
use stapler::{CodeGenerator, NameResolver, ProgramLoader, TypeChecker};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("stapler-modules-{}-{nonce}", std::process::id()));
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
        let program = ProgramLoader::new().load_path(&self.root.join("main.sta"))?;
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
            "pub type alias Number = i32\n",
            "pub def add: (i32, i32) -> i32 = (a: i32, b: i32) => a\n",
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
fn resolves_mutually_recursive_module_namespaces() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use ma\nma.a (1)\n");
    fixture.write(
        "ma.sta",
        concat!(
            "use mb\n",
            "pub def a: (i32) -> i32 = (n: i32) => mb.b (n)\n",
        ),
    );
    fixture.write(
        "mb.sta",
        concat!(
            "use ma\n",
            "pub def b: (i32) -> i32 = (n: i32) => ma.a (n)\n",
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
    fixture.write("main.sta", "use ffi.(puts)\nputs (\"hello\")\n");
    fixture.write(
        "ffi.sta",
        "pub extern \"c\" { let puts: (*const c_char) -> i32 }\n",
    );

    let llvm = fixture.compile().expect("public extern should import");
    assert!(llvm.contains("declare i32 @puts"));
}

#[test]
fn imports_operator_values_and_their_fixities() {
    let fixture = Fixture::new();
    fixture.write(
        "math.sta",
        concat!(
            "pub def infixl 6 +: i32 -> i32 -> i32 = x => y => x\n",
            "pub def infixr 5 **: i32 -> i32 -> i32 = x => y => y\n",
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
            "def get: () -> i32 = () => value\n",
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
