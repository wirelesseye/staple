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
        let source = with_syntax_imports(source);
        let source = if source.trim_start().starts_with("mod\n")
            || source.trim_start().starts_with("pub mod\n")
            || source.trim_start().starts_with("pub(package) mod\n")
        {
            source
        } else {
            format!("pub mod\n{source}")
        };
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

    fn check_at(&self, entry: &str, root: &str) -> Result<(), String> {
        let program = ProgramLoader::new()
            .with_module_root(self.root.join(root))
            .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
            .load_path(&self.root.join(entry))?;
        let resolved = NameResolver::new()
            .resolve_program(program)
            .map_err(format_diagnostics)?;
        TypeChecker::new()
            .check(resolved)
            .map(|_| ())
            .map_err(format_diagnostics)
    }
}

fn with_syntax_imports(source: &str) -> String {
    if source.contains("use std.syntax") {
        return source.to_owned();
    }
    let mut names = Vec::new();
    for name in [
        "quote",
        "parse_quote",
        "Expr",
        "Type",
        "Pattern",
        "Item",
        "Syntax",
        "SyntaxNode",
        "Ident",
        "CallExpr",
        "MacroCallMetadata",
    ] {
        if source.contains(name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        source.to_owned()
    } else {
        format!("{source}\nuse std.syntax.({})\n", names.join(", "))
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
            "use std.io.(print, println)\n",
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
fn uses_root_qualified_standard_library_items_without_imports() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let printer: String ->{std.io.IO} () = std.io.println\n",
            "def message: () -> std.cinterop.CString = () => std.cinterop.c_string \"hello\"\n",
            "message ()\n",
            "printer \"hello\"\n",
        ),
    );

    let llvm = fixture
        .compile()
        .expect("root-qualified standard-library values and types should compile");
    assert!(llvm.contains("__staple_m1_println"));
    assert!(llvm.contains("c\"hello\\00\""));
}

#[test]
fn only_the_declaring_module_can_assign_a_public_mutable_global() {
    let fixture = Fixture::new();
    fixture.write(
        "state.sta",
        "pub let mut counter = 1\ncounter = 2\npub let mut point = Ref (x: 1, y: 2)\npoint.x = 3\n",
    );
    fixture.write(
        "main.sta",
        "use state.(counter, point)\nlet observed = counter\nlet also_observed = point.x\n",
    );
    fixture
        .compile()
        .expect("the owner module may assign its mutable globals");

    fixture.write("main.sta", "use state.counter\ncounter = 3\n");
    let error = fixture
        .compile()
        .expect_err("importers cannot reassign a `mut` global");
    assert!(error.contains("writable place"));

    fixture.write("main.sta", "use state.point\npoint.x = 4\n");
    let error = fixture
        .compile()
        .expect_err("importers cannot write through a `mut` global either");
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
fn reexports_public_items_through_selected_renamed_glob_and_chained_uses() {
    let fixture = Fixture::new();
    fixture.write(
        "origin.sta",
        concat!(
            "pub type alias Number = I32\n",
            "pub let value: Number = 42\n",
            "pub macro reveal = item => parse_quote { $item }\n",
        ),
    );
    fixture.write(
        "facade.sta",
        concat!(
            "pub use origin.(Number, reveal)\n",
            "pub use origin.value as answer\n",
        ),
    );
    fixture.write("all.sta", "pub use origin.*\n");
    fixture.write("chain.sta", "pub use facade.answer as result\n");
    fixture.write(
        "main.sta",
        concat!(
            "use facade.(Number, reveal)\n",
            "use all.value\n",
            "use chain.result\n",
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
            "    use super.private_parent\n",
            "    pub def add_two: I32 -> I32 = value => value + private_parent\n",
            "}\n",
            "mod extras { pub let answer: I32 = 42 }\n",
            "pub use child.add_two\n",
            "pub use extras.*\n",
        ),
    );
    fixture.write(
        "main.sta",
        "use library.(add_two, answer)\nlet result: I32 = add_two 2\nlet copied: I32 = answer\n",
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
            "    use super.root\n",
            "    let offset: I32 = 1\n",
            "    pub mod inner {\n",
            "        use super.(offset)\n",
            "        use super.super.root as base\n",
            "        pub let answer: I32 = base + offset + 1\n",
            "    }\n",
            "}\n",
        ),
    );
    fixture.write(
        "main.sta",
        "use library.outer.inner.answer\nlet result: I32 = answer\n",
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
        "use library.api.answer\nlet result: I32 = answer\n",
    );
    fixture
        .compile()
        .expect("public inline paths should be externally importable");

    fixture.write("library.sta", "mod hidden { pub let answer: I32 = 42 }\n");
    fixture.write("main.sta", "use library.hidden.answer\n");
    let error = fixture
        .compile()
        .expect_err("private inline paths must not be externally importable");
    assert!(error.contains("private"));
}

#[test]
fn file_modules_are_private_unless_declared_public() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "mod\npub let answer: I32 = 42\n");
    fixture.write("main.sta", "use library.answer\n");
    let error = fixture
        .compile()
        .expect_err("a private file module must not be imported by another module");
    assert!(error.contains("module `library` is private"));

    fixture.write("library.sta", "pub mod\npub let answer: I32 = 42\n");
    fixture.write("main.sta", "use library.answer\nlet value: I32 = answer\n");
    fixture
        .compile()
        .expect("a public file module should import");
}

#[test]
fn rejects_a_dotted_import_that_names_both_an_item_and_a_module() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        "pub let user: I32 = 1\npub mod user { pub let id: I32 = 2 }\n",
    );
    fixture.write("main.sta", "use library.user\n");

    let error = fixture
        .compile()
        .expect_err("a dotted item/module collision should be ambiguous");
    assert!(error.contains("ambiguous dotted import"));
}

#[test]
fn dotted_import_combines_a_type_with_its_companion() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        "pub type alias User = I32\ncompanion User { pub let id: I32 = 42 }\n",
    );
    fixture.write(
        "main.sta",
        "use library.User\nlet user: User = 1\nlet id: I32 = User.id\n",
    );

    fixture
        .compile()
        .expect("a type and its companion should import together");
}

#[test]
fn discovers_companions_in_otherwise_unreachable_files() {
    let fixture = Fixture::new();
    fixture.write("animals.sta", "pub type alias Animal = I32\n");
    fixture.write(
        "animal_extensions.sta",
        "companion Animal { pub def move_to: Animal -> Animal = animal => animal }\n",
    );
    fixture.write(
        "main.sta",
        "use animals.Animal\nlet moved: Animal = Animal.move_to 1\n",
    );

    fixture
        .compile()
        .expect("an unreachable file's companion should be discovered");
}

#[test]
fn dotted_import_combines_a_type_with_a_same_named_file_module() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub type alias User = I32\n");
    fixture.write("library/User.sta", "pub let id: I32 = 42\n");
    fixture.write(
        "main.sta",
        "use library.User\nlet user: User = 1\nlet id: I32 = User.id\n",
    );

    fixture
        .compile()
        .expect("a type and same-named file module should import together");
}

#[test]
fn dotted_import_preserves_a_reexported_type_and_companion_pair() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        "pub type alias User = I32\ncompanion User { pub let id: I32 = 42 }\n",
    );
    fixture.write("facade.sta", "pub use library.User\n");
    fixture.write(
        "main.sta",
        "use facade.User\nlet user: User = 1\nlet id: I32 = User.id\n",
    );

    fixture
        .compile()
        .expect("reexports should preserve a type and companion pair");
}

#[test]
fn imports_a_dotted_module_path_as_a_namespace() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub mod api { pub let answer: I32 = 42 }\n");
    fixture.write(
        "main.sta",
        "use library.api\nlet result: I32 = api.answer\n",
    );

    fixture
        .compile()
        .expect("an unambiguous dotted module path should import its namespace");
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
            "    def id: <T> T -> T = x => x\n",
            "}\n",
            "use submodule.*\n",
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
fn block_scoped_submodules_are_reachable_within_their_block() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    mod inner { pub let answer: I32 = 42 }\n",
            "    inner.answer\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("a block-scoped submodule's public items should be reachable via qualified access");
}

#[test]
fn block_scoped_submodule_macros_are_reachable_within_their_block() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    mod inner {\n",
            "        pub macro reveal = value => value\n",
            "    }\n",
            "    inner.reveal 42\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("a macro in a block-scoped submodule should be callable within that block");
}

#[test]
fn block_scoped_submodules_see_the_enclosing_modules_top_level_items_via_super() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let root_value: I32 = 41\n",
            "{\n",
            "    mod inner {\n",
            "        use super.root_value\n",
            "        pub let answer: I32 = root_value + 1\n",
            "    }\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("`use super` from a block-scoped submodule should see the enclosing module's top-level items");
}

#[test]
fn block_scoped_submodule_names_are_not_visible_outside_their_block() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "{\n",
            "    mod inner { pub let answer: I32 = 42 }\n",
            "    let x: I32 = inner.answer\n",
            "}\n",
            "let leaked: I32 = inner.answer\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("a block-scoped submodule's name should not escape its block");
    assert!(error.contains("inner"));
}

#[test]
fn block_scoped_submodules_do_not_see_the_enclosing_blocks_locals() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "{\n",
            "    let local_value: I32 = 1\n",
            "    mod inner {\n",
            "        use super.local_value\n",
            "    }\n",
            "}\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("`super` from a block-scoped submodule should not see local bindings");
    assert!(error.contains("local_value"));
}

#[test]
fn sibling_blocks_may_reuse_the_same_submodule_name() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let a: I32 = {\n",
            "    mod foo { pub let value: I32 = 1 }\n",
            "    foo.value\n",
            "}\n",
            "let b: I32 = {\n",
            "    mod foo { pub let value: I32 = 2 }\n",
            "    foo.value\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("sibling blocks should not conflict over reusing a submodule name");
}

#[test]
fn duplicate_submodule_names_in_the_same_block_are_rejected() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let x: I32 = {\n",
            "    mod foo { pub let value: I32 = 1 }\n",
            "    mod foo { pub let value: I32 = 2 }\n",
            "    foo.value\n",
            "}\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("redeclaring a submodule name in the same block should be rejected");
    assert!(error.contains("duplicate definition of `foo`"));
}

#[test]
fn block_scoped_submodules_initialize_exactly_once() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    mod inner { pub let answer: I32 = 42 }\n",
            "    inner.answer\n",
            "}\n",
        ),
    );

    let llvm = fixture
        .compile()
        .expect("block-scoped submodule should compile");
    let init_symbols = llvm
        .lines()
        .filter(|line| line.trim_start().starts_with("define"))
        .filter_map(|line| {
            line.split("@__staple_init_")
                .nth(1)
                .and_then(|rest| rest.split('(').next())
        })
        .collect::<Vec<_>>();
    assert!(!init_symbols.is_empty());
    for symbol in init_symbols {
        let needle = format!("call void @__staple_init_{symbol}()");
        assert_eq!(
            llvm.matches(&needle).count(),
            1,
            "module `{symbol}` should initialize exactly once",
        );
    }
}

#[test]
fn block_scoped_type_declarations_are_usable_within_their_block() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    type Wrapped = I32\n",
            "    let value: Wrapped = Wrapped 42\n",
            "    match value { Wrapped inner => inner }\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("a block-scoped type's name and constructor should be usable within its block");
}

#[test]
fn block_scoped_type_names_are_not_visible_outside_their_block() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "{\n",
            "    type Wrapped = I32\n",
            "    let value: Wrapped = Wrapped 42\n",
            "}\n",
            "let leaked: Wrapped = Wrapped 1\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("a block-scoped type's name should not escape its block");
    assert!(error.contains("Wrapped"));
}

#[test]
fn block_scoped_types_shadow_module_level_types_of_the_same_name() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "type Wrapped = I32\n",
            "let result: Bool = {\n",
            "    type Wrapped = Bool\n",
            "    let value: Wrapped = Wrapped True\n",
            "    match value { Wrapped inner => inner }\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("a block-scoped type should shadow a module-level type of the same name");
}

#[test]
fn block_scoped_type_constructors_do_not_enter_macro_definition_scope() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "type Wrapped = I32\n",
            "macro make = _: Expr => parse_quote { Wrapped 42 }\n",
            "{ type Wrapped = Bool; () }\n",
            "let result: Wrapped = make ()\n",
        ),
    );

    fixture.compile().expect(
        "a block-local constructor should not replace a module constructor in macro definition scope",
    );
}

#[test]
fn sibling_blocks_may_reuse_the_same_type_name() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let a: I32 = {\n",
            "    type Wrapped = I32\n",
            "    let value: Wrapped = Wrapped 1\n",
            "    match value { Wrapped inner => inner }\n",
            "}\n",
            "let b: Bool = {\n",
            "    type Wrapped = Bool\n",
            "    let value: Wrapped = Wrapped True\n",
            "    match value { Wrapped inner => inner }\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("sibling blocks should not conflict over reusing a type name");
}

#[test]
fn duplicate_type_names_in_the_same_block_are_rejected() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let x: I32 = {\n",
            "    type Foo = I32\n",
            "    type Foo = Bool\n",
            "    0\n",
            "}\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("redeclaring a type name in the same block should be rejected");
    assert!(error.contains("duplicate type definition of `Foo`"));
}

#[test]
fn block_scoped_types_in_generic_functions_monomorphize_per_call_site() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "def wrap: <T> T -> T = value => {\n",
            "    type Boxed = T\n",
            "    match Boxed value { Boxed inner => inner }\n",
            "}\n",
            "wrap 1\n",
            "wrap True\n",
        ),
    );

    fixture.compile().expect(
        "a block-scoped type referencing its enclosing function's type parameter should monomorphize per call site",
    );
}

#[test]
fn block_scoped_use_namespace_is_usable_within_its_block() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub let value: I32 = 42\n");
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    use library\n",
            "    library.value\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("a block-scoped namespace use should be usable within its block");
}

#[test]
fn block_scoped_use_glob_is_usable_within_its_block() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub let value: I32 = 42\n");
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    use library.*\n",
            "    value\n",
            "}\n"
        ),
    );

    fixture
        .compile()
        .expect("a block-scoped glob use should be usable within its block");
}

#[test]
fn block_scoped_use_selected_is_usable_within_its_block() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub let a: I32 = 1\npub let b: I32 = 2\n");
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    use library.(a, b)\n",
            "    a + b\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("a block-scoped selected use should be usable within its block");
}

#[test]
fn block_scoped_use_renamed_is_usable_within_its_block() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub let value: I32 = 42\n");
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    use library.value as x\n",
            "    x\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("a block-scoped renamed use should be usable within its block");
}

#[test]
fn block_scoped_use_of_a_macro_is_usable_within_its_block() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        "pub macro reveal = item => parse_quote { $item }\n",
    );
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    use library.(reveal)\n",
            "    reveal 42\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("a macro imported in a block should expand within that block");
}

#[test]
fn block_scoped_use_names_are_not_visible_outside_their_block() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub let value: I32 = 42\n");
    fixture.write(
        "main.sta",
        concat!(
            "{\n",
            "    use library\n",
            "    let x: I32 = library.value\n",
            "}\n",
            "let leaked: I32 = library.value\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("a block-scoped use's name should not escape its block");
    assert!(error.contains("library"));
}

#[test]
fn sibling_blocks_may_reuse_the_same_imported_name() {
    let fixture = Fixture::new();
    fixture.write("library_a.sta", "pub let value: I32 = 1\n");
    fixture.write("library_b.sta", "pub let value: I32 = 2\n");
    fixture.write(
        "main.sta",
        concat!(
            "let a: I32 = {\n",
            "    use library_a.(value)\n",
            "    value\n",
            "}\n",
            "let b: I32 = {\n",
            "    use library_b.(value)\n",
            "    value\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("sibling blocks should not conflict over reusing an imported name");
}

#[test]
fn duplicate_imports_in_the_same_block_are_rejected() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub let a: I32 = 1\npub let b: I32 = 2\n");
    fixture.write(
        "main.sta",
        concat!(
            "let x: I32 = {\n",
            "    use library.(a)\n",
            "    use library.b as a\n",
            "    a\n",
            "}\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("redeclaring an imported name in the same block should be rejected");
    assert!(error.contains("duplicate import of `a`"));
}

#[test]
fn block_scoped_type_and_use_of_the_same_name_are_rejected_type_first() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub type alias Foo = Bool\n");
    fixture.write(
        "main.sta",
        concat!(
            "let x: I32 = {\n",
            "    type Foo = I32\n",
            "    use library.(Foo)\n",
            "    0\n",
            "}\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("a type and a later use of the same name in one block should be rejected");
    assert!(error.contains("duplicate import of `Foo`"));
}

#[test]
fn block_scoped_type_and_use_of_the_same_name_are_rejected_use_first() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub type alias Foo = Bool\n");
    fixture.write(
        "main.sta",
        concat!(
            "let x: I32 = {\n",
            "    use library.(Foo)\n",
            "    type Foo = I32\n",
            "    0\n",
            "}\n",
        ),
    );

    let error = fixture
        .compile()
        .expect_err("a use and a later type of the same name in one block should be rejected");
    assert!(error.contains("duplicate type definition of `Foo`"));
}

#[test]
fn block_scoped_use_super_resolves_relative_to_the_enclosing_module() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "let root_value: I32 = 41\n",
            "mod container {\n",
            "    pub let result: I32 = {\n",
            "        use super.root_value\n",
            "        root_value + 1\n",
            "    }\n",
            "}\n",
        ),
    );

    fixture.compile().expect(
        "`use super` from a bare block should resolve relative to the enclosing module, not the block",
    );
}

#[test]
fn block_scoped_use_initializes_its_target_exactly_once() {
    let fixture = Fixture::new();
    fixture.write("library.sta", "pub let value: I32 = 42\n");
    fixture.write(
        "main.sta",
        concat!(
            "let result: I32 = {\n",
            "    use library\n",
            "    library.value\n",
            "}\n",
        ),
    );

    let llvm = fixture.compile().expect("block-scoped use should compile");
    let init_symbols = llvm
        .lines()
        .filter(|line| line.trim_start().starts_with("define"))
        .filter_map(|line| {
            line.split("@__staple_init_")
                .nth(1)
                .and_then(|rest| rest.split('(').next())
        })
        .collect::<Vec<_>>();
    assert!(!init_symbols.is_empty());
    for symbol in init_symbols {
        let needle = format!("call void @__staple_init_{symbol}()");
        assert_eq!(
            llvm.matches(&needle).count(),
            1,
            "module `{symbol}` should initialize exactly once",
        );
    }
}

#[test]
fn inline_glob_reexports_types_traits_and_macros() {
    let fixture = Fixture::new();
    fixture.write(
        "library.sta",
        concat!(
            "mod child {\n",
            "    use std.syntax.(parse_quote)\n",
            "    pub type alias Number = I32\n",
            "    pub trait Identity T { identity: T -> T }\n",
            "    pub macro reveal = item => parse_quote { $item }\n",
            "    pub mod Variant { pub type Ready }\n",
            "}\n",
            "pub use child.*\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use library.*\n",
            "impl Identity I32 { def identity = value => value }\n",
            "let answer: Number = identity (reveal 42)\n",
            "let ready: Variant.Ready = Variant.Ready\n",
        ),
    );
    fixture
        .compile()
        .expect("glob re-exports should preserve every named item kind and namespace");
}

#[test]
fn rejects_reexporting_a_private_item_from_an_ancestor() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        "let hidden = 42\nmod child { pub use super.hidden }\n",
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
    fixture.write("main.sta", "use library.api.file_answer\n");

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
    fixture.write("main.sta", "use super.value\n");
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
        "pub trait Increment T { increment: T -> T }\n",
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
            "def twice: <T where Increment T> T -> T = value => increment (increment value)\n",
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
            "def apply: <T where traits.Increment T> T -> T = value => traits.Increment.increment value\n",
            "let answer: I32 = apply 41\n",
        ),
        concat!(
            "use traits.Increment as Inc\n",
            "use implementations\n",
            "def apply: <T where Inc T> T -> T = value => Inc.increment value\n",
            "let answer: I32 = apply 41\n",
        ),
        concat!(
            "use traits.*\n",
            "use implementations\n",
            "def apply: <T where Increment T> T -> T = value => increment value\n",
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
fn preserves_trait_prerequisites_across_modules() {
    let fixture = Fixture::new();
    fixture.write(
        "traits.sta",
        concat!(
            "pub trait Base T { base: T -> T }\n",
            "pub trait Derived T where Base T { derived: T -> T }\n",
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
            "use traits.(Base, Derived)\n",
            "use implementations\n",
            "def apply: <T where Derived T> T -> T = value => Base.base (Derived.derived value)\n",
            "let answer: I32 = apply 42\n",
        ),
    );

    fixture
        .compile()
        .expect("imported trait prerequisites should remain available");
}

#[test]
fn preserves_trait_functional_dependencies_across_modules() {
    let fixture = Fixture::new();
    fixture.write(
        "traits.sta",
        "pub trait Iterator Iter Item where Iter ~> Item { next: Iter -> Item }\n",
    );
    fixture.write(
        "implementations.sta",
        concat!(
            "use traits\n",
            "impl traits.Iterator I32 String { def next = value => \"next\" }\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use traits.Iterator\n",
            "use implementations\n",
            "let result = Iterator.next 1\n",
        ),
    );

    fixture
        .compile()
        .expect("imported trait functional dependencies should remain available");
}

#[test]
fn imports_and_specializes_default_trait_members() {
    let fixture = Fixture::new();
    fixture.write(
        "traits.sta",
        concat!(
            "pub trait Increment T {\n",
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
            "use traits.Increment\n",
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
            "pub def identity: <T> move T -> T = move x => x\n",
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
    fixture.write("boxes.sta", "pub(repr) type Box T = (value: T)\n");
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
        "use markers.(Ready)\nlet ready: Ready = Ready\nlet Ready() = ready\n",
    );
    fixture
        .compile()
        .expect("selected singleton import should include its type and value");

    fixture.write("main.sta", "use markers.*\nlet hidden = Hidden\n");
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
fn enforces_representation_visibility_for_explicit_and_shortcut_access() {
    let private = Fixture::new();
    private.write(
        "users.sta",
        concat!(
            "pub type User = (name: String)\n",
            "pub def make: String -> User = name => User (name)\n",
        ),
    );
    private.write(
        "main.sta",
        concat!(
            "use users.*\n",
            "let user = make \"Ada\"\n",
            "let explicit = user.*\n",
            "let shortcut = user.name\n",
        ),
    );
    let error = private
        .compile()
        .expect_err("private representations must not be projected by either spelling");
    assert!(
        error.contains("the representation of `m1.User` is private"),
        "{error}"
    );

    let public = Fixture::new();
    public.write("users.sta", "pub(repr) type User = (name: String)\n");
    public.write(
        "main.sta",
        concat!(
            "use users.*\n",
            "let user = User (name: \"Ada\")\n",
            "let inner: (name: String) = user.*\n",
            "let name: String = user.name\n",
        ),
    );
    public
        .compile()
        .expect("public representations should support both access spellings");
}

#[test]
fn destructures_a_private_representation_from_the_defining_module_including_its_companion() {
    // Regression test: `Staple.md`'s `type` section says an ordinary
    // `pub type`'s representation and constructor are "private to its
    // defining module" — i.e. visible within that module, not just to
    // external importers being excluded. The check used to compare the
    // type's defining module against the *exact* current module only, so a
    // companion body (a separate `ModuleId`, even though `Staple.md`'s
    // "Type companions" section says a companion "sees the parent's
    // declarations without spelling `use super.*`") was treated as if it
    // were a different module entirely, and destructuring the very type the
    // companion belongs to failed with "the representation of `X` is
    // private" from inside `X`'s own companion.
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "pub type Wrapped = I32\n",
            "companion Wrapped {\n",
            "    pub def unwrap: Wrapped -> I32 = value => {\n",
            "        let Wrapped inner = value\n",
            "        inner\n",
            "    }\n",
            "}\n",
            "let wrapped = Wrapped 42\n",
            "Wrapped.unwrap wrapped\n",
        ),
    );
    fixture.compile().expect(
        "a companion should be able to destructure its own type's representation, \
         the same as the rest of the defining module can",
    );
}

#[test]
fn companion_accesses_a_private_submodule_namespace_of_its_defining_module() {
    // Regression test: a companion's body can already refer directly to
    // private *values* and *types* declared in its defining module (see
    // `destructures_a_private_representation_from_the_defining_module_including_its_companion`
    // above), but the definition context built for it only carried the
    // parent's values and types, not its submodule namespaces. So a private
    // `mod` sibling to the type declaration was reported as an unknown name
    // from within that type's own companion, even though ordinary code in
    // the same module can reach it directly.
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "pub type Wrapped = I32\n",
            "mod helpers {\n",
            "    pub def helper: I32 -> I32 = x => x + 1\n",
            "}\n",
            "companion Wrapped {\n",
            "    pub def bump: Wrapped -> I32 = value => {\n",
            "        let Wrapped inner = value\n",
            "        helpers.helper inner\n",
            "    }\n",
            "}\n",
            "let wrapped = Wrapped 42\n",
            "Wrapped.bump wrapped\n",
        ),
    );
    fixture.compile().expect(
        "a companion should be able to reach a private submodule of its defining module, \
         the same as the rest of the defining module can",
    );
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
    fixture.write("main.sta", "use folder.first.*\nanswer\n");
    fixture.write(
        "folder/first.sta",
        "use shared.*\npub let answer = shared_answer\n",
    );
    fixture.write("shared.sta", "pub let shared_answer = 42\n");

    fixture
        .compile()
        .expect("nested imports should remain entry-relative");
}

#[test]
fn resolves_package_paths_from_an_explicit_module_root() {
    let fixture = Fixture::new();
    fixture.write(
        "src/bin/main.sta",
        "use package.models.answer\nlet result: I32 = answer\n",
    );
    fixture.write("src/models.sta", "pub let answer: I32 = 42\n");

    fixture
        .check_at("src/bin/main.sta", "src")
        .expect("package-qualified import should use the configured module root");
}

#[test]
fn uses_root_qualified_package_items_without_imports() {
    let fixture = Fixture::new();
    fixture.write(
        "src/bin/main.sta",
        concat!(
            "let result: package.models.Answer = package.models.answer\n",
            "result\n",
        ),
    );
    fixture.write(
        "src/models.sta",
        "pub type alias Answer = I32\npub let answer: Answer = 42\n",
    );

    fixture
        .check_at("src/bin/main.sta", "src")
        .expect("root-qualified package values and types should use the module root");
}

#[test]
fn package_root_owns_items_and_entry_has_its_relative_module_name() {
    let fixture = Fixture::new();
    fixture.write("src/root.sta", "pub def root_value: () -> I32 = () => 40\n");
    fixture.write(
        "src/main.sta",
        "pub def entry_value: () -> I32 = () => 2\nuse package.helper.total\ntotal ()\n",
    );
    fixture.write(
        "src/helper.sta",
        "pub def total: () -> I32 = () => package.root_value () + package.main.entry_value ()\n",
    );

    let program = ProgramLoader::new()
        .with_module_root(fixture.root.join("src"))
        .with_package_root(fixture.root.join("src/root.sta"))
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .load_path(&fixture.root.join("src/main.sta"))
        .expect("configured package root and entry should load");
    let resolved = NameResolver::new().resolve_program(program).unwrap();
    TypeChecker::new()
        .check(resolved)
        .expect("root and entry items should resolve through distinct package paths");
}

#[test]
fn missing_package_root_still_anchors_entry_and_sibling_modules() {
    let fixture = Fixture::new();
    fixture.write(
        "src/main.sta",
        "pub def entry_value: () -> I32 = () => 2\nuse package.helper.total\ntotal ()\n",
    );
    fixture.write(
        "src/helper.sta",
        "pub def total: () -> I32 = () => package.main.entry_value ()\n",
    );

    let program = ProgramLoader::new()
        .with_package_root(fixture.root.join("src/root.sta"))
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .load_path(&fixture.root.join("src/main.sta"))
        .expect("an absent root module should remain a valid package anchor");
    assert!(program.package_root().is_none());
    let resolved = NameResolver::new().resolve_program(program).unwrap();
    TypeChecker::new().check(resolved).unwrap();
}

#[test]
fn root_qualified_items_establish_initialization_dependencies() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "dependency.value\n");
    fixture.write("dependency.sta", "pub let value = 1\n");

    let error = fixture
        .compile()
        .expect_err("ordinary names must not become implicit package roots");
    assert!(error.contains("unknown name `dependency`"));

    fixture.write("main.sta", "package.dependency.value\n");
    let llvm = fixture
        .compile()
        .expect("root-qualified access should load and initialize its module");
    let main = llvm.split("define i32 @main()").nth(1).unwrap();
    assert!(main.find("@__staple_init_m1").unwrap() < main.find("@__staple_init_m0").unwrap());
}

#[test]
fn bare_package_refers_to_the_entry_module_during_a_cycle() {
    let fixture = Fixture::new();
    fixture.write(
        "src/bin/main.sta",
        "pub def answer: () -> I32 = () => 42\nuse package.helper.result\ndef observed: () -> I32 = () => result ()\n",
    );
    fixture.write(
        "src/helper.sta",
        "use package.answer\npub def result: () -> I32 = () => answer ()\n",
    );

    fixture
        .check_at("src/bin/main.sta", "src")
        .expect("bare package import should resolve to the entry before recursive loading");
}

#[test]
fn package_qualification_bypasses_inline_child_shadowing() {
    let fixture = Fixture::new();
    fixture.write(
        "src/main.sta",
        concat!(
            "mod models { pub let answer: Bool = True }\n",
            "use package.models.answer\n",
            "let result: I32 = answer\n",
        ),
    );
    fixture.write("src/models.sta", "pub let answer: I32 = 42\n");

    fixture
        .check_at("src/main.sta", "src")
        .expect("package qualifier should bypass an inline child with the same name");
}

#[test]
fn package_named_file_uses_a_repeated_package_component() {
    let fixture = Fixture::new();
    fixture.write(
        "src/main.sta",
        "use package.package.answer\nlet result: I32 = answer\n",
    );
    fixture.write("src/package.sta", "pub let answer: I32 = 42\n");

    fixture
        .check_at("src/main.sta", "src")
        .expect("package.package should address package.sta");
}

#[test]
fn provides_io_implicitly_only_to_the_entry_modules_top_level() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use std.io.println\nprintln \"entry output\"\n");

    fixture
        .check_at("main.sta", ".")
        .expect("IO should be implicitly available at the entry module's top level");

    let fixture = Fixture::new();
    fixture.write("main.sta", "use dependency.*\n");
    fixture.write(
        "dependency.sta",
        "use std.io.println\nprintln \"dependency output\"\n",
    );

    let error = fixture
        .check_at("main.sta", ".")
        .expect_err("IO must still be rejected at a non-entry module's top level");
    assert!(error.contains("top-level initialization requires resources {IO}"));
}

#[test]
fn does_not_select_imported_dependency_or_let_bindings_as_source_main() {
    let fixture = Fixture::new();
    fixture.write("dependency.sta", "pub def main = () => 1\n");
    fixture.write(
        "main.sta",
        "use dependency.main as dependency_main\nlet main = dependency_main\nmain ()\n",
    );

    let llvm = fixture
        .compile()
        .expect("ordinary main bindings should compile");
    let native_main = llvm.split("define i32 @main()").nth(1).unwrap();
    assert!(!native_main.contains("source.main"));
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
        "use std.cinterop.*\npub extern \"c\" { puts: (CPointer CChar) -> I32 }\n",
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
            "pub macro reveal = value => parse_quote { private_identity $value }\n",
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
            "use helpers.define_generated\n",
            "let caller_value: I32 = 2\n",
            "define_generated caller_value\n",
            "let result: I32 = generated ()\n",
        ),
    );
    fixture.write(
        "helpers.sta",
        concat!(
            "let private_value: I32 = 40\n",
            "pub macro define_generated: Expr -> Item = body => parse_quote {\n",
            "    pub def generated = () => private_value + $body\n",
            "}\n",
        ),
    );

    fixture
        .compile()
        .expect("generated item names and splices should retain their hygiene contexts");
}

#[test]
fn imports_function_and_modifier_macros_with_the_same_name() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use helpers.identity\n",
            "let expression: I32 = identity 41\n",
            "@identity\n",
            "let item: I32 = expression + 1\n",
            "let result: I32 = item\n",
        ),
    );
    fixture.write(
        "helpers.sta",
        concat!(
            "pub macro identity: Expr -> Expr = value => parse_quote { $value }\n",
            "pub macro @identity: Item -> Item = item => item\n",
        ),
    );

    fixture
        .compile()
        .expect("ordinary and modifier macro namespaces should import together");
}

#[test]
fn imports_and_reexports_metadata_aware_macros() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use facade.define\n",
            "pub define\n",
            "let result: I32 = generated\n",
        ),
    );
    fixture.write("facade.sta", "pub use helpers.define\n");
    fixture.write(
        "helpers.sta",
        concat!(
            "pub macro define = metadata: MacroCallMetadata => { let visibility = metadata.visibility; parse_quote {\n",
            "    $visibility let generated: I32 = 42\n",
            "} }\n",
        ),
    );

    fixture
        .compile()
        .expect("metadata-aware macros should survive imports and re-exports");
}

#[test]
fn generated_type_and_pattern_splices_keep_caller_hygiene() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        concat!(
            "use helpers.(define_alias, destructure)\n",
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
            "pub macro define_alias = ty: Type => parse_quote { type alias Generated = $ty }\n",
            "pub macro destructure = pattern: Pattern => value: Expr => parse_quote { let $pattern = $value }\n",
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
            "use helpers.reveal as renamed\n",
            "use helpers.(reveal)\n",
            "let first: I32 = renamed 1\n",
            "let second: I32 = reveal 2\n",
        ),
    );
    fixture.write(
        "helpers.sta",
        "pub macro reveal = value => parse_quote { $value }\n",
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
            "pub macro reveal = value: Expr => parse_quote { $value }\n",
            "pub macro reveal = value: Expr => _: Ident \"with\" => replacement: Expr => parse_quote { $replacement }\n",
        ),
    );
    fixture.write("bridge.sta", "pub use helpers.reveal\n");
    fixture.write(
        "main.sta",
        concat!(
            "use helpers\n",
            "use helpers.reveal as renamed\n",
            "use bridge.(reveal)\n",
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
        "pub macro choose = value: Expr => parse_quote { 1 }\n",
    );
    fixture.write(
        "right.sta",
        "pub macro choose = value: Expr => other: Expr => parse_quote { 2 }\n",
    );
    fixture.write(
        "main.sta",
        "use left.(choose)\nuse right.(choose)\nchoose 1\n",
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
        "extern \"staple-intrinsic\" { fake: (I32, I32) -> I32 }\n",
    );

    let error = fixture
        .compile()
        .expect_err("the intrinsic ABI must be reserved");
    assert!(error.contains("reserved for the standard library"));
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
fn rejects_an_early_read_across_a_module_cycle() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "use ma.*\na\n");
    fixture.write("ma.sta", "use mb.*\npub let a = b\n");
    fixture.write("mb.sta", "use ma\npub let b = 41\n");

    let error = fixture
        .compile()
        .expect_err("the earlier module must not observe a default value");
    assert!(error.contains("binding is read before it is initialized"));
}

#[test]
fn imports_resource_types_and_resource_bearing_functions() {
    let fixture = Fixture::new();
    fixture.write(
        "clocks.sta",
        concat!(
            "pub type Clock = I32\n",
            "pub def system_clock = () => Clock 42\n",
            "pub def read: () ->{Clock} Clock = () => resource Clock\n",
        ),
    );
    fixture.write(
        "main.sta",
        concat!(
            "use clocks.*\n",
            "with Clock = system_clock () { read () }\n",
        ),
    );

    let llvm = fixture
        .compile()
        .expect("imported resource contracts should compile and lower");
    assert!(llvm.contains("__staple_m1_read"));
}

#[test]
fn resolves_recursive_binder_dependencies_with_per_package_aliases() {
    let fixture = Fixture::new();
    fixture.write("app/src/main.sta", "let result: I32 = middle.answer\n");
    fixture.write("middle/src/root.sta", "pub use package.helper.answer\n");
    fixture.write(
        "middle/src/helper.sta",
        "use renamed_leaf.value.value\npub let answer: I32 = value\n",
    );
    fixture.write("leaf/src/value.sta", "pub let value: I32 = 42\n");
    fs::write(
        fixture.root.join("app/binder.kdl"),
        "package \"app\" { dependencies { middle path=\"../middle\" } }\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("middle/binder.kdl"),
        "package \"middle-lib\" { kind \"library\"; dependencies { renamed_leaf path=\"../leaf\" } }\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("leaf/binder.kdl"),
        "package \"leaf-lib\" { kind \"library\" }\n",
    )
    .unwrap();

    let graph = binder::load_package_graph(&fixture.root.join("app/binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .expect("package graph should load");
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(format_diagnostics)
        .expect("dependency aliases should resolve");
    TypeChecker::new()
        .check(resolved)
        .map_err(format_diagnostics)
        .expect("transitive dependency should type-check");
}

#[test]
fn resolves_package_visible_items_within_a_binder_package() {
    let fixture = Fixture::new();
    fixture.write(
        "src/main.sta",
        "use package.bridge.answer\nlet result: I32 = answer\n",
    );
    fixture.write(
        "src/bridge.sta",
        "pub(package) mod\npub(package) use package.internal.answer\n",
    );
    fixture.write(
        "src/internal.sta",
        "pub(package) mod\npub(package) let answer: I32 = 42\n",
    );
    fs::write(
        fixture.root.join("binder.kdl"),
        "package \"app\" { root \"src/main.sta\" }\n",
    )
    .unwrap();

    let graph = binder::load_package_graph(&fixture.root.join("binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .expect("package-visible modules should load within their package");
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(format_diagnostics)
        .expect("package re-export should resolve within its package");
    TypeChecker::new()
        .check(resolved)
        .map_err(format_diagnostics)
        .expect("package-visible value should type-check");
}

#[test]
fn rejects_package_visibility_without_a_binder_manifest() {
    let fixture = Fixture::new();
    fixture.write("main.sta", "pub(package) let secret = 42\nsecret\n");
    let error = fixture
        .compile()
        .expect_err("standalone package visibility must fail");
    assert!(error.contains("package visibility requires a Binder manifest"));
}

#[test]
fn rejects_package_visible_dependency_modules() {
    let fixture = Fixture::new();
    fixture.write("app/src/main.sta", "use lib.internal.secret\nsecret\n");
    fixture.write(
        "lib/src/internal.sta",
        "pub(package) mod\npub(package) let secret = 42\n",
    );
    fs::write(
        fixture.root.join("app/binder.kdl"),
        "package \"app\" { dependencies { lib path=\"../lib\" } }\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("lib/binder.kdl"),
        "package \"lib\" { kind \"library\" }\n",
    )
    .unwrap();

    let graph = binder::load_package_graph(&fixture.root.join("app/binder.kdl")).unwrap();
    let error = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .expect_err("dependency package modules must not expose package visibility");
    assert!(error.to_string().contains("private"));
}

#[test]
fn rejects_promoting_a_package_visible_reexport() {
    let fixture = Fixture::new();
    fixture.write(
        "src/main.sta",
        "pub use package.internal.secret\nlet local = package.internal.secret\n",
    );
    fixture.write(
        "src/internal.sta",
        "pub(package) mod\npub(package) let secret = 42\n",
    );
    fs::write(
        fixture.root.join("binder.kdl"),
        "package \"app\" { root \"src/main.sta\" }\n",
    )
    .unwrap();
    let graph = binder::load_package_graph(&fixture.root.join("binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .unwrap();
    let error = NameResolver::new()
        .resolve_program(program)
        .expect_err("public use must not promote package-visible items");
    assert!(format_diagnostics(error).contains("secret"));
}

#[test]
fn package_representation_is_usable_locally_but_not_by_a_dependency() {
    let fixture = Fixture::new();
    fixture.write(
        "app/src/main.sta",
        "let shared: lib.Shared = lib.Shared 42\n",
    );
    fixture.write(
        "lib/src/root.sta",
        "pub(repr(package)) type Shared = I32\nlet local: Shared = Shared 1\n",
    );
    fs::write(
        fixture.root.join("app/binder.kdl"),
        "package \"app\" { dependencies { lib path=\"../lib\" } }\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("lib/binder.kdl"),
        "package \"lib\" { kind \"library\"; root \"src/root.sta\" }\n",
    )
    .unwrap();
    let graph = binder::load_package_graph(&fixture.root.join("app/binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .unwrap();
    let error = NameResolver::new()
        .resolve_program(program)
        .expect_err("dependency must not construct a package-represented type");
    assert!(format_diagnostics(error).contains("Shared"));
}

#[test]
fn checks_a_rootless_library_public_surface_without_entry_resources() {
    let fixture = Fixture::new();
    fixture.write("src/api.sta", "pub let answer: I32 = 42\n");
    fs::write(
        fixture.root.join("src/unused.sta"),
        "mod\nmissing_private_name\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("binder.kdl"),
        "package \"library\" { kind \"library\" }\n",
    )
    .unwrap();
    let graph = binder::load_package_graph(&fixture.root.join("binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .expect("rootless library should load");
    assert_eq!(program.executable_entry(), None);
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(format_diagnostics)
        .expect("unused private files should not enter the public closure");
    TypeChecker::new()
        .check(resolved)
        .map_err(format_diagnostics)
        .expect("public surface should type-check");

    fixture.write(
        "src/io_api.sta",
        "use std.io.println\nprintln \"not an entry\"\n",
    );
    let graph = binder::load_package_graph(&fixture.root.join("binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .unwrap();
    let resolved = NameResolver::new().resolve_program(program).unwrap();
    let error = TypeChecker::new()
        .check(resolved)
        .expect_err("library modules must not receive implicit IO");
    assert!(format_diagnostics(error).contains("IO"));
}

#[test]
fn standard_library_imports_resolve_inside_a_binder_package() {
    let fixture = Fixture::new();
    fixture.write("src/main.sta", "use std.io.println\nprintln \"hi\"\n");
    fs::write(
        fixture.root.join("binder.kdl"),
        "package \"app\" { root \"src/main.sta\" }\n",
    )
    .unwrap();

    let graph = binder::load_package_graph(&fixture.root.join("binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .expect("`std` is an implicit dependency of every package");
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(format_diagnostics)
        .expect("`use std.io` should resolve through the dependency alias path");
    TypeChecker::new()
        .check(resolved)
        .map_err(format_diagnostics)
        .expect("standard-library import should type-check");
}

#[test]
fn standard_library_imports_resolve_from_in_memory_source() {
    let root = std::env::temp_dir();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .load_source("use std.io.println\nprintln \"hi\"\n", &root)
        .expect("in-memory builds still carry an implicit `std`");
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(format_diagnostics)
        .expect("`use std.io` should resolve without a manifest");
    TypeChecker::new()
        .check(resolved)
        .map_err(format_diagnostics)
        .expect("standard-library import should type-check");
}

#[test]
fn rejects_an_explicit_std_dependency_alias() {
    // The compiler injects `std` as a dependency alias on every package, so a
    // user manifest must never be allowed to bind that name itself.
    assert!(binder::validate_dependency_alias("std").is_err());
}

#[test]
fn binder_features_filter_items_before_resolution() {
    let fixture = Fixture::new();
    fixture.write(
        "src/main.sta",
        "@feature(\"broken\")\ndef bad = () => missing_name\ndef good = () => 1\n",
    );
    fs::write(
        fixture.root.join("binder.kdl"),
        "package \"app\" {\n  features {\n    default \"broken\"\n    broken\n  }\n}\n",
    )
    .unwrap();
    let graph = binder::load_package_graph(&fixture.root.join("binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .with_feature_selection(binder::FeatureSelection {
            no_default_features: true,
            ..Default::default()
        })
        .load_package_graph()
        .unwrap();
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(format_diagnostics)
        .expect("disabled item should not resolve");
    TypeChecker::new()
        .check(resolved)
        .map_err(format_diagnostics)
        .expect("remaining items should type-check");
}

#[test]
fn package_can_disable_and_explicitly_reimport_the_prelude() {
    let fixture = Fixture::new();
    fixture.write(
        "src/main.sta",
        "use std.prelude.*\nlet values: List I32 = List.singleton 1\nlet answer = 1 + 2\n",
    );
    fs::write(
        fixture.root.join("binder.kdl"),
        "package \"minimal\" {\n  prelude #false\n}\n",
    )
    .unwrap();
    let graph = binder::load_package_graph(&fixture.root.join("binder.kdl")).unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .with_package_graph(graph)
        .load_package_graph()
        .unwrap();
    let resolved = NameResolver::new()
        .resolve_program(program)
        .map_err(format_diagnostics)
        .expect("explicit prelude import should restore convenience names");
    TypeChecker::new()
        .check(resolved)
        .map_err(format_diagnostics)
        .expect("core operators and explicit prelude should type-check");
}

#[test]
fn no_prelude_modifier_removes_convenience_names_but_keeps_core() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("main.sta"),
        "@no_prelude\npub mod\nlet answer = 1 + 2\nlet values: List I32 = List.singleton 1\n",
    )
    .unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("stdlib"))
        .load_path(&fixture.root.join("main.sta"))
        .unwrap();
    let error = NameResolver::new()
        .resolve_program(program)
        .expect_err("List should not be implicit after @no_prelude");
    let diagnostics = format_diagnostics(error);
    assert!(diagnostics.contains("unknown type `List`"));
    assert!(!diagnostics.contains("unknown type `I32`"));
}

#[test]
fn moved_std_core_paths_are_private_but_canonical_paths_work() {
    let fixture = Fixture::new();
    fixture.write(
        "main.sta",
        "use std.list.List\nlet values: List I32 = List.singleton 1\n",
    );
    fixture
        .check_at("main.sta", ".")
        .expect("canonical std.list path should work");
    fixture.write(
        "main.sta",
        "use std.core.list.List\nlet values: List I32 = List.singleton 1\n",
    );
    assert!(
        fixture
            .check_at("main.sta", ".")
            .unwrap_err()
            .contains("private")
    );
}
