use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use inkwell::context::Context;
use stapler::{CodeGenerator, Item, NameResolver, ProgramLoader, TypeChecker};

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
fn imports_standard_io_print_functions() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use std.io (print, println)\n",
            "print \"hello\"\n",
            "println \" world\"\n",
        ),
    );

    let llvm = fixture.compile().expect("std.io should compile");
    assert!(llvm.contains("__staple_m1_print"));
    assert!(llvm.contains("__staple_m1_println"));
    assert!(llvm.contains("@printf"));
    assert!(llvm.contains("c\"%s\\00\""));
    assert!(llvm.contains("c\"%s\\0A\\00\""));
}

#[test]
fn only_the_declaring_module_can_assign_a_public_mutable_global() {
    let fixture = Fixture::new();
    fixture.write(
        "state.sta",
        "pub let mut counter = 1\ncounter = 2\npub let point = Ref (x: 1, y: 2)\n",
    );
    fixture.write(
        "main.sta",
        "use state (counter, point)\nlet observed = counter\npoint.x = 4\n",
    );
    fixture
        .compile()
        .expect("the owner module may assign its mutable global");

    fixture.write("main.sta", "use state counter\ncounter = 3\n");
    let error = fixture
        .compile()
        .expect_err("importers cannot assign mutable globals");
    assert!(error.contains("writable place"));
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
            "use math (Number)\n",
            "use math add as plus\n",
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
fn reexports_public_items_through_selected_renamed_glob_and_chained_uses() {
    let fixture = Fixture::new();
    fixture.write(
        "origin.sta",
        concat!(
            "pub type alias Number = I32\n",
            "pub let value: Number = 42\n",
            "pub macro reveal = item => quote { $item }\n",
        ),
    );
    fixture.write(
        "facade.sta",
        concat!(
            "pub use origin (Number, reveal)\n",
            "pub use origin value as answer\n",
        ),
    );
    fixture.write("all.sta", "pub use origin *\n");
    fixture.write("chain.sta", "pub use facade answer as result\n");
    fixture.write(
        "main.sta",
        concat!(
            "use facade (Number, reveal)\n",
            "use all value\n",
            "use chain result\n",
            "let first: Number = reveal value\n",
            "let second: Number = result\n",
            "first + second\n",
        ),
    );

    fixture
        .compile()
        .expect("public uses should re-export every imported item kind");
}

#[test]
fn inline_submodules_import_ancestors_and_reexport_public_items() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        concat!(
            "let private_parent: I32 = 40\n",
            "mod child {\n",
            "    use super private_parent\n",
            "    pub def add_two: I32 -> I32 = value => value + private_parent\n",
            "}\n",
            "mod extras { pub let answer: I32 = 42 }\n",
            "pub use child add_two\n",
            "pub use extras *\n",
        ),
    );
    fixture.write(
        "main.sta",
        "use library (add_two, answer)\nlet result: I32 = add_two 2\nlet copied: I32 = answer\n",
    );

    let llvm = fixture
        .compile()
        .expect("children should import ancestors and support re-exports");
    assert!(llvm.contains("add_two"));
}

#[test]
fn recursively_nested_submodules_use_super_and_initialize_once() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        concat!(
            "let root: I32 = 40\n",
            "pub mod outer {\n",
            "    use super root\n",
            "    let offset: I32 = 1\n",
            "    pub mod inner {\n",
            "        use super (offset)\n",
            "        use super.super root as base\n",
            "        pub let answer: I32 = base + offset + 1\n",
            "    }\n",
            "}\n",
        ),
    );
    fixture.write(
        "main.sta",
        "use library.outer.inner answer\nlet result: I32 = answer\n",
    );

    let llvm = fixture
        .compile()
        .expect("recursive relative imports should compile");
    for initializer in ["m1", "m2", "m3"] {
        assert_eq!(
            llvm.matches(&format!("call void @__staple_init_{initializer}()"))
                .count(),
            1,
            "each file and inline module should initialize exactly once",
        );
    }
}

#[test]
fn external_imports_traverse_public_inline_submodules() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub mod api { pub let answer: I32 = 42 }\n");
    fixture.write(
        "main.sta",
        "use library.api answer\nlet result: I32 = answer\n",
    );
    fixture
        .compile()
        .expect("public inline paths should be externally importable");

    fixture.write("library.sta", "mod hidden { pub let answer: I32 = 42 }\n");
    fixture.write("main.sta", "use library.hidden answer\n");
    let error = fixture
        .compile()
        .expect_err("private inline paths must not be externally importable");
    assert!(error.contains("private"));
}

#[test]
fn submodules_do_not_inherit_parent_items_implicitly() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        "let parent_value: I32 = 42\nmod child { let copy: I32 = parent_value }\n",
    );
    let error = fixture
        .compile()
        .expect_err("parent items should require an explicit import");
    assert!(error.contains("parent_value"));
}

#[test]
fn unknown_names_report_private_glob_candidates() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "mod submodule {\n",
            "    def id: T => T -> T = x => x\n",
            "}\n",
            "use submodule *\n",
            "id 42\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("a private item should not be imported by a glob");
    assert!(
        error.contains("unknown name `id`; `id` exists in module `submodule`, but it is private")
    );
}

#[test]
fn inline_glob_reexports_types_traits_and_macros() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        concat!(
            "mod child {\n",
            "    pub type alias Number = I32\n",
            "    pub trait Identity = T => { identity: T -> T }\n",
            "    pub macro reveal = item => quote { $item }\n",
            "}\n",
            "pub use child *\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use library *\n",
            "impl Identity I32 { def identity = value => value }\n",
            "let answer: Number = identity (reveal 42)\n",
        ),
    );
    fixture
        .compile()
        .expect("glob re-exports should preserve every named item kind");
}

#[test]
fn rejects_reexporting_a_private_item_from_an_ancestor() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        "let hidden = 42\nmod child { pub use super hidden }\n",
    );
    let error = fixture
        .compile()
        .expect_err("a public use must not expose a private ancestor item");
    assert!(error.contains("no public item named `hidden`"));
}

#[test]
fn a_complete_file_path_precedes_an_inline_submodule_path() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        "pub mod api { pub let inline_answer: I32 = 1 }\n",
    );
    fixture.write("library/api.sta", "pub let file_answer: I32 = 2\n");
    fixture.write("main.sta", "use library.api file_answer\n");

    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .load_path(&fixture.root.join("main.sta"))
        .expect("the complete file path should load");
    let Item::UseDeclaration(use_) = &program.module(program.entry()).syntax.items[0] else {
        panic!("expected use declaration");
    };
    let imported = program
        .imported_module(use_.syntax.id)
        .expect("use should resolve");
    assert!(program.module(imported).path.ends_with("library/api.sta"));
}

#[test]
fn rejects_super_at_a_file_module_root() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use super value\n");
    let error = fixture
        .compile()
        .expect_err("a file module has no lexical parent");
    assert!(error.contains("`super` has no parent module"));
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
            "use traits (Increment)\n",
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
            "use traits Increment as Inc\n",
            "use implementations\n",
            "def apply: T => Inc T => T -> T = value => Inc.increment value\n",
            "let answer: I32 = apply 41\n",
        ),
        concat!(
            "use traits *\n",
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
            "use traits (Increment)\n",
            "let answer: I32 = increment 41\n",
        ),
    );
    let error = fixture
        .compile()
        .expect_err("implementations in unloaded modules must not be discovered");
    assert!(error.contains("no trait implementation or matching bound"));
}

#[test]
fn preserves_trait_prerequisites_across_modules() {
    let fixture = Fixture::new();
    fixture.write(
        "traits.sta",
        concat!(
            "pub trait Base = T => { base: T -> T }\n",
            "pub trait Derived = T => Base T => { derived: T -> T }\n",
        ),
    );
    fixture.write(
        "implementations.sta",
        concat!(
            "use traits\n",
            "impl traits.Derived I32 { def derived = value => value }\n",
            "impl traits.Base I32 { def base = value => value }\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use traits (Base, Derived)\n",
            "use implementations\n",
            "def apply: T => Derived T => T -> T = value => Base.base (Derived.derived value)\n",
            "let answer: I32 = apply 42\n",
        ),
    );

    fixture
        .compile()
        .expect("imported trait prerequisites should remain available");
}

#[test]
fn imports_and_specializes_default_trait_members() {
    let fixture = Fixture::new();
    fixture.write(
        "traits.sta",
        concat!(
            "pub trait Increment = T => {\n",
            "  increment: T -> T\n",
            "  twice: T -> T = value => increment (increment value)\n",
            "}\n",
        ),
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
            "use traits Increment\n",
            "use implementations\n",
            "let answer: I32 = Increment.twice 40\n",
        ),
    );

    fixture
        .compile()
        .expect("imported default trait members should specialize");
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
            "use values *\n",
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
        concat!("use values *\n", "let user: UserId = UserId 42\n"),
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
            "use boxes (Box)\n",
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
            "use boxes Box as Wrapped\n",
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
            "use boxes *\n",
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
fn exports_public_singleton_types_as_values_without_public_repr() {
    let fixture = Fixture::new();
    fixture.write("markers.sta", "pub type Ready\ntype Hidden\n");
    fixture.write(
        "main.sta",
        concat!(
            "use markers\n",
            "let ready: markers.Ready = markers.Ready\n",
            "let markers.Ready() = ready\n",
        ),
    );
    fixture
        .compile()
        .expect("public singleton should export its unique value and pattern");

    fixture.write(
        "main.sta",
        "use markers (Ready)\nlet ready: Ready = Ready\nlet Ready() = ready\n",
    );
    fixture
        .compile()
        .expect("selected singleton import should include its type and value");

    fixture.write("main.sta", "use markers *\nlet hidden = Hidden\n");
    let error = fixture
        .compile()
        .expect_err("private singleton value should not be exported");
    assert!(error.contains("unknown name `Hidden`"));
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
            "use errors *\n",
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
        concat!("use ids *\n", "let UserId value = make 42\n",),
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
    fixture.write("main.sta", "use values (hidden)\nhidden\n");
    fixture.write("values.sta", "let hidden = 1\n");

    let error = fixture
        .compile()
        .expect_err("private item must not be imported");
    assert!(error.contains("no public item named `hidden`"));
}

#[test]
fn emits_dependency_initializers_before_the_entry_initializer() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use dependency *\nvalue\n");
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
    fixture.write("main.sta", "use package.first *\nanswer\n");
    fixture.write(
        "package/first.sta",
        "use shared *\npub let answer = shared_answer\n",
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
        "use ffi (puts)\nuse std.cinterop (c_string)\nputs (c_string \"hello\")\n",
    );
    fixture.write(
        "ffi.sta",
        "use std.cinterop *\npub extern \"c\" { let puts: (CPointer CChar) -> I32 }\n",
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
            "use std.cinterop c_string as cs\n",
            "def values = () => {\n",
            "  let first: cinterop.CString = cinterop.c_string \"first\"\n",
            "  let second: cinterop.CString = cs \"second\"\n",
            "}\n",
        ),
    );

    let llvm = fixture.compile().expect("macro imports should compile");
    assert!(llvm.contains("c\"first\\00\""));
    assert!(llvm.contains("c\"second\\00\""));
}

#[test]
fn imports_user_macros_with_definition_site_hygiene() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use helpers\n",
            "def private_identity: String -> String = value => value\n",
            "let result: I32 = helpers.reveal 42\n",
        ),
    );
    fixture.write(
        "helpers.sta",
        concat!(
            "def private_identity: I32 -> I32 = value => value\n",
            "pub macro reveal = value => quote { private_identity $value }\n",
        ),
    );

    fixture
        .compile()
        .expect("public macro should retain its definition-site environment");
}

#[test]
fn generated_items_keep_definition_and_splice_hygiene() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use helpers define_generated\n",
            "let caller_value: I32 = 2\n",
            "define_generated caller_value\n",
            "let result: I32 = generated ()\n",
        ),
    );
    fixture.write(
        "helpers.sta",
        concat!(
            "let private_value: I32 = 40\n",
            "pub macro define_generated: Expr -> Item = body => quote {\n",
            "    pub def generated = () => private_value + $body\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("generated item names and splices should retain their hygiene contexts");
}

#[test]
fn generated_type_and_pattern_splices_keep_caller_hygiene() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use helpers (define_alias, destructure)\n",
            "destructure ((left, right)) (40, 2)\n",
            "type alias Local = I32;\n",
            "define_alias Local\n",
            "let alias_value: Generated = 1\n",
            "let result: I32 = alias_value + left + right\n",
        ),
    );
    fixture.write(
        "helpers.sta",
        concat!(
            "pub macro define_alias = ty: Type => quote { type alias Generated = $ty }\n",
            "pub macro destructure = pattern: Pattern => value: Expr => quote { let $pattern = $value }\n",
        ),
    );

    fixture
        .compile()
        .expect("type and pattern splices should retain the caller environment");
}

#[test]
fn imports_user_macros_through_selected_and_renamed_forms() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use helpers reveal as renamed\n",
            "use helpers (reveal)\n",
            "let first: I32 = renamed 1\n",
            "let second: I32 = reveal 2\n",
        ),
    );
    fixture.write(
        "helpers.sta",
        "pub macro reveal = value => quote { $value }\n",
    );

    fixture
        .compile()
        .expect("user macros should support selected and renamed imports");
}

#[test]
fn preserves_macro_overload_sets_through_imports_and_reexports() {
    let fixture = Fixture::new();
    fixture.write(
        "helpers.sta",
        concat!(
            "pub macro reveal = value: Expr => quote { $value }\n",
            "pub macro reveal = value: Expr => _: Ident \"with\" => replacement: Expr => quote { $replacement }\n",
        ),
    );
    fixture.write("bridge.sta", "pub use helpers reveal\n");
    fixture.write(
        "main.sta",
        concat!(
            "use helpers\n",
            "use helpers reveal as renamed\n",
            "use bridge (reveal)\n",
            "let namespace_short: I32 = helpers.reveal 1\n",
            "let namespace_long: I32 = helpers.reveal 1 with 2\n",
            "let renamed_long: I32 = renamed 1 with 3\n",
            "let reexported_long: I32 = reveal 1 with 4\n",
        ),
    );

    fixture
        .compile()
        .expect("all import forms should preserve complete overload sets");
}

#[test]
fn does_not_merge_macro_overloads_from_unrelated_imports() {
    let fixture = Fixture::new();
    fixture.write(
        "left.sta",
        "pub macro choose = value: Expr => quote { 1 }\n",
    );
    fixture.write(
        "right.sta",
        "pub macro choose = value: Expr => other: Expr => quote { 2 }\n",
    );
    fixture.write(
        "main.sta",
        "use left (choose)\nuse right (choose)\nchoose 1\n",
    );

    let error = fixture
        .compile()
        .expect_err("unrelated imported macro groups must conflict");
    assert!(error.contains("duplicate import of `choose`"));
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
            "use math + as combine\n",
            "use math ((**))\n",
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
            "use values *\n",
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
fn rejects_an_early_read_across_a_module_cycle() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use ma *\na\n");
    fixture.write("ma.sta", "use mb *\npub let a = b\n");
    fixture.write("mb.sta", "use ma\npub let b = 41\n");

    let error = fixture
        .compile()
        .expect_err("the earlier module must not observe a default value");
    assert!(error.contains("binding is read before it is initialized"));
}
