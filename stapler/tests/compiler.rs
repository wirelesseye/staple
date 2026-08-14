use inkwell::context::Context;
use stapler::{
    CheckedType, CodeGenerator, Item, NameResolver, ProgramLoader, RecursiveConstruction,
    Statement, TypeChecker, parse,
};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn resolve(source: &str) -> stapler::ResolvedModule {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = with_syntax_imports(source);
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(&source, root)
        .expect("source should load");
    NameResolver::new()
        .resolve_program(program)
        .expect("source should resolve")
}

fn with_syntax_imports(source: &str) -> String {
    if source.contains("use std.syntax") {
        return source.to_owned();
    }
    let mut names = Vec::new();
    for (name, used) in [
        ("quote", source.contains("quote")),
        (
            "Expr",
            source.contains(": Expr") || source.contains("-> Expr"),
        ),
        (
            "Type",
            source.contains(": Type =>")
                || source.contains(": Type ->")
                || source.contains("-> Type"),
        ),
        (
            "Pattern",
            source.contains(": Pattern =>")
                || source.contains(": Pattern ->")
                || source.contains("-> Pattern"),
        ),
        (
            "Item",
            source.contains(": Item")
                || source.contains("-> Item")
                || source.contains("Sequence Item"),
        ),
        (
            "Syntax",
            source.contains(": Syntax")
                || source.contains("-> Syntax")
                || source.contains("Braced Syntax"),
        ),
        ("SyntaxNode", source.contains("SyntaxNode")),
        ("Ident", source.contains("Ident")),
        ("CallExpr", source.contains("CallExpr")),
        ("Sequence", source.contains("Sequence")),
        ("Optional", source.contains("Optional")),
        ("Separated", source.contains("Separated")),
        ("Comma", source.contains("Comma")),
        ("Equals", source.contains("Equals")),
        ("FatArrow", source.contains("FatArrow")),
        ("Parenthesized", source.contains("Parenthesized")),
        ("Bracketed", source.contains("Bracketed")),
        ("Braced", source.contains("Braced")),
        (
            "MacroCallVisibility",
            source.contains("MacroCallVisibility"),
        ),
        (
            "Visibility",
            source.contains(": Visibility") || source.contains("-> Visibility"),
        ),
        ("StringType", source.contains("StringType")),
        ("QuoteResult", source.contains("QuoteResult")),
    ] {
        if used && !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        source.to_owned()
    } else {
        format!("{source}\nuse std.syntax ({})\n", names.join(", "))
    }
}

fn type_check(source: &str) -> stapler::TypedModule {
    TypeChecker::new()
        .check(resolve(source))
        .expect("source should type-check")
}

#[test]
fn infers_checks_and_lowers_typed_resources() {
    let module = type_check(concat!(
        "type Clock = (now: () -> I32)\n",
        "type Logger = (write: I32 -> ())\n",
        "def system_clock = Clock (now: () => 41)\n",
        "def logger = Logger (write: value => ())\n",
        "def read: () ->{Clock} I32 = () => (resource Clock).now ()\n",
        "def inferred = () => read ()\n",
        "def both: () ->{Logger, Clock} I32 = () => {\n",
        "  (resource Logger).write (inferred ())\n",
        "  inferred ()\n",
        "}\n",
        "with Clock = system_clock {\n",
        "  with Logger = logger { both () }\n",
        "}\n",
    ));

    let inferred = module
        .functions()
        .iter()
        .find(|function| function.name == "inferred")
        .expect("inferred function");
    let resources = &module
        .type_of_function(inferred.id)
        .expect("inferred function type")
        .resources;
    assert_eq!(resources.resources.len(), 1);
    assert_eq!(resources.resources[0].value_type.to_string(), "Clock");

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("typed resources should lower to hidden parameters");
    assert!(llvm.contains("define i32 @read"));
}

#[test]
fn rejects_invalid_resource_contracts_and_types() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop *\n",
            "type MoveOnly = CString\n",
            "def invalid: () ->{MoveOnly} () = () => ()\n",
        )))
        .expect_err("move-only resources must be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("concrete, sized, Copy nominal type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Clock = (now: () -> I32)\n",
            "def need = () => (resource Clock).now ()\n",
            "def invalid: () ->{} I32 = () => need ()\n",
        )))
        .expect_err("undeclared resources must be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("not contained in its declared resource set")
    }));
}

#[test]
fn standard_io_is_a_compiler_provided_resource_and_propagates_to_main() {
    let module = type_check(concat!(
        "use std.io (IO, print, println)\n",
        "def identity: T => T -> T = value => value\n",
        "def explicit: String ->{IO} () = value => print value\n",
        "def inferred = value: String => println value\n",
        "def main = () => { explicit \"one\"; inferred (identity \"two\") }\n",
    ));

    for name in ["explicit", "inferred", "main"] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name.ends_with(name))
            .expect("resource-bearing function");
        let resources = &module
            .type_of_function(function.id)
            .unwrap()
            .resources
            .resources;
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].value_type.to_string(), "IO");
    }
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic calls should remain specialized during resource inference");
}

#[test]
fn rejects_io_at_top_level_and_non_builtin_opaque_resources() {
    let top_level = TypeChecker::new()
        .check(resolve("use std.io println\nprintln \"invalid\"\n"))
        .expect_err("top-level output must require unavailable IO");
    assert!(top_level.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("top-level initialization requires resources {IO}")
    }));

    for source in [
        "type Token = opaque\ndef use_token: () ->{Token} () = () => ()\n",
        "type IO = I32\ndef main: () ->{IO} () = () => ()\n",
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("only std.io.IO may be an opaque or entry resource");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("concrete, sized, Copy nominal type")
                || diagnostic
                    .message
                    .contains("may require only the `std.io.IO` resource")
        }));
    }
}

#[test]
fn validates_source_main_signature_and_resource_boundary() {
    for (source, expected) in [
        ("def main = value => ()\n", "must accept `()`"),
        ("def main = () => 1\n", "must return `()`"),
        ("def main: T => () -> () = () => ()\n", "cannot be generic"),
        (
            "type Clock = I32\ndef main: () ->{Clock} () = () => ()\n",
            "may require only the `std.io.IO` resource",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("invalid source main must be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "expected {expected:?}, got {diagnostics:?}"
        );
    }
}

#[test]
fn resources_obey_alias_exactness_macro_trait_and_boundary_rules() {
    let module = type_check(concat!(
        "type Clock = I32\n",
        "type alias CurrentClock = Clock\n",
        "type Logger = I32\n",
        "type Box = T => (value: T)\n",
        "trait Observe = T => { observe: T ->{Clock} Clock }\n",
        "impl Observe I32 { def observe = value => resource CurrentClock }\n",
        "macro request = _: Ident \"clock\" => quote { resource CurrentClock }\n",
        "def generated = () => request clock\n",
        "def declared: () ->{Logger, Clock, Clock} () = () => ()\n",
        "def through_trait = () => observe 1\n",
        "def boxed: () ->{Box I32} I32 = () => (resource Box I32).value\n",
        "with Clock = Clock 7 {\n",
        "  with Logger = Logger 8 { declared (); generated (); through_trait () }\n",
        "}\n",
        "with Box I32 = Box (value: 9) { boxed () }\n",
    ));
    let declared = module
        .functions()
        .iter()
        .find(|function| function.name == "declared")
        .expect("declared function");
    assert_eq!(
        module
            .type_of_function(declared.id)
            .expect("declared function type")
            .resources
            .resources
            .len(),
        2
    );
    for name in ["generated", "through_trait"] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .expect("resource function");
        assert_eq!(
            module
                .type_of_function(function.id)
                .expect("resource function type")
                .resources
                .resources
                .len(),
            1
        );
    }
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("trait and specialized resource calls should lower");

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Clock = I32\n",
            "def need: () ->{Clock} Clock = () => resource Clock\n",
            "let incompatible: () -> Clock = need\n",
        )))
        .expect_err("resource-bearing and pure function types must compare exactly");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("expected `() -> Clock`")
            && diagnostic.message.contains("->{Clock}")
    }));

    for (source, expected) in [
        (
            "type Clock = I32\nresource Clock\n",
            "top-level initialization requires resources",
        ),
        (
            "type Clock = I32\nextern \"c\" { let read: () ->{Clock} Clock }\n",
            "external functions cannot require Staple resources",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("resource boundary must be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "expected {expected:?}, got {diagnostics:?}"
        );
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro bad: Expr -> Expr = value: Expr => resource Expr\nlet generated = bad 1\n",
            root,
        )
        .expect("macro resource source should parse");
    let diagnostics = match NameResolver::new().resolve_program(program) {
        Err(diagnostics) => diagnostics,
        Ok(_) => panic!("resources must not be evaluated by macros"),
    };
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("resources are not available during compile-time macro evaluation")
    }));
}

#[test]
fn marks_compiler_owned_recursive_constructors() {
    let resolved = resolve("");
    let construction_for = |name: &str| {
        resolved
            .type_declarations()
            .iter()
            .find_map(|(id, declaration)| {
                (declaration.name == name).then(|| resolved.recursive_construction(*id))
            })
            .flatten()
    };

    assert_eq!(
        construction_for("Ref"),
        Some(RecursiveConstruction::ManagedReference)
    );
    assert_eq!(
        construction_for("CallExpr"),
        Some(RecursiveConstruction::Syntax)
    );
    assert_eq!(construction_for("String"), None);
}

fn copy_directory(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("test standard library directory should be created");
    for entry in std::fs::read_dir(source).expect("standard library directory should be readable") {
        let entry = entry.expect("standard library entry should be readable");
        let source = entry.path();
        let target = target.join(entry.file_name());
        if source.is_dir() {
            copy_directory(&source, &target);
        } else {
            std::fs::copy(source, target).expect("standard library file should be copied");
        }
    }
}

fn string_contract_diagnostics(declaration: &str) -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after the Unix epoch")
        .as_nanos();
    let temporary = std::env::temp_dir().join(format!(
        "stapler-string-contract-{}-{nonce}",
        std::process::id()
    ));
    copy_directory(&root.join("stdlib"), &temporary);
    std::fs::write(
        temporary.join("std/core/string.sta"),
        format!(
            "{declaration}\nextern \"staple-intrinsic\" {{ let __string_add: (String, String) -> String }}\npub trait ToString = T => {{ to_string: T -> String }}\nimpl ToString String {{ def to_string = value => value }}\nimpl Add String {{ def add = left => right => __string_add (left, right) }}\n"
        ),
    )
        .expect("test String declaration should be written");

    let messages = match ProgramLoader::new()
        .with_standard_library_root(&temporary)
        .load_source("", root)
    {
        Err(error) => vec![error],
        Ok(program) => match NameResolver::new().resolve_program(program) {
            Err(diagnostics) => diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.message)
                .collect(),
            Ok(module) => TypeChecker::new()
                .check(module)
                .err()
                .unwrap_or_default()
                .into_iter()
                .map(|diagnostic| diagnostic.message)
                .collect(),
        },
    };
    std::fs::remove_dir_all(temporary).expect("test standard library should be removed");
    messages
}

#[test]
fn supports_repeated_spread_and_erased_product_references() {
    let source = concat!(
        "let explicit: I32[3] = (1, 2, 3)\n",
        "let spread: (String, ...I32[2]) = (\"x\", 4, 5)\n",
        "let fixed: Ref I32[3] = Ref explicit\n",
        "let erased: Ref I32[] = fixed\n",
        "let constructed: Ref I32[] = Ref (6, 7)\n",
        "let singleton: Ref I32[] = Ref 8\n",
        "let empty: Ref I32[] = Ref ()\n",
        "let count: USize = length erased\n",
        "let fixed_count: USize = length fixed\n",
        "let literal: I32 = erased.1\n",
        "let index: USize = 2\n",
        "let dynamic: I32 = erased[index]\n",
        "let fixed_dynamic: I32 = fixed[index]\n",
        "let direct: I32 = explicit[index]\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    let llvm = generator
        .compile_module(&module)
        .expect("product extensions should generate LLVM");
    assert!(llvm.contains("erased_ref.length"));
    assert!(llvm.contains("index.out_of_bounds"));
    assert!(llvm.contains("llvm.trap"));
}

#[test]
fn spreads_fixed_product_values_and_call_arguments() {
    let source = concat!(
        "def sum: I32[4] -> I32 = (a, b, c, d) => a + b + c + d\n",
        "let pair = (left: 2, right: 3)\n",
        "let expanded = (prefix: \"value\", ...pair, suffix: False)\n",
        "let selected: I32 = expanded.left + expanded.right\n",
        "let answer: I32 = sum (1, ...pair, 4)\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("product value spreads should generate LLVM");
    assert!(llvm.contains("product.spread.element"));
}

#[test]
fn type_checks_and_lowers_mutable_places_and_ref_replace() {
    let source = concat!(
        "let mut value = 1\n",
        "value = 2\n",
        "let mut pair = (x: 3, y: 4)\n",
        "pair.x = value\n",
        "let fixed: Ref I32[2] = Ref (5, 6)\n",
        "fixed.0 = pair.x\n",
        "let index: USize = 1\n",
        "fixed[index] = 7\n",
        "let scalar = Ref 8\n",
        "let old = replace (scalar, 9)\n",
        "def local = () => { let mut inside = 10; inside = old; inside }\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("mutable places should generate LLVM");
    assert!(llvm.contains("mutable.binding.cell"));
    assert!(llvm.contains("place.field"));
    assert!(llvm.contains("ref.replace.old"));
}

#[test]
fn rejects_invalid_assignment_targets_and_uninitialized_mutable_lets() {
    let diagnostics = NameResolver::new()
        .resolve(&parse("let mut value: I32\n").expect("syntax should parse"))
        .expect_err("mutable lets require an initializer");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("require an initializer"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("let value = 1\nvalue = 2\n"))
        .expect_err("immutable names cannot be assigned");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("writable place"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "def product = () => (x: 1, y: 2)\nproduct ().x = 3\n",
        ))
        .expect_err("by-value temporaries are not writable");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("writable place"))
    );
}

#[test]
fn lowers_move_only_mutation_reinitialization_and_captured_cells() {
    let module = type_check(concat!(
        "use std.cinterop *\n",
        "def local = (first: CString, second: CString) => {\n",
        "  let mut value = first\n",
        "  value = second\n",
        "  let moved = value\n",
        "  value = c_string \"replacement\"\n",
        "  drop moved\n",
        "  drop value\n",
        "}\n",
        "def captured = (initial: CString) => {\n",
        "  let mut value = initial\n",
        "  () => { let old = value; value = c_string \"next\"; drop old }\n",
        "}\n",
        "def managed = (initial: CString, next: CString) => {\n",
        "  let reference = Ref initial\n",
        "  let old = replace (reference, next)\n",
        "  drop old\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("move-only mutable cells should generate LLVM");
    assert!(llvm.contains("mutable.drop.is_live"));
    assert!(llvm.contains("__staple_gc_finalize_mutable_cell_"));
    assert!(llvm.contains("ref.replace.old"));
}

#[test]
fn supports_mutable_parameter_match_and_copy_ref_pattern_binders() {
    type_check(concat!(
        "type Box = I32\n",
        "type Empty\n",
        "def parameter = (mut value: I32) => { value = value + 1; value }\n",
        "def matched = (value: Box | Empty) => match value { Box (mut inner) => { inner = 3; inner }, Empty() => 0 }\n",
        "def borrowed = (value: Ref I32) => { let Ref (mut inner) = value; inner = 4; inner }\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Resource = I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
            "def invalid = (value: Ref Resource) => { let Ref (mut inner) = value; inner }\n",
        )))
        .expect_err("move-only Ref borrows cannot become mutable locals");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("borrowed through `Ref` cannot be bound as mutable")
    }));
}

#[test]
fn rejects_invalid_product_spreads_and_indices() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: (...I32)\n"))
        .expect_err("a scalar cannot be spread");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot spread non-product type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid = (...1, 2)\n"))
        .expect_err("a scalar value cannot be spread");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot spread non-product value")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let pair = (1, \"two\")\nlet index: USize = 0\nlet invalid = pair[index]\n",
        ))
        .expect_err("a heterogeneous product cannot be indexed dynamically");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("homogeneous product") })
    );
}

#[test]
fn rejects_erased_products_outside_refs_and_ref_destructuring() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: I32[]\n"))
        .expect_err("an erased product cannot be used by value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("unsized type") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let fixed: Ref I32[2] = Ref (1, 2)\nlet erased: Ref I32[] = fixed\nlet Ref values = erased\n",
        ))
        .expect_err("an erased reference cannot be destructured");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("cannot be destructured") })
    );
}

#[test]
fn handles_product_repetition_edges_and_limits() {
    type_check("let value: (...I32[0], ...I32[1], I32) = (1, 2)\n");

    let diagnostics = TypeChecker::new()
        .check(resolve("let too_large: I32[65536]\n"))
        .expect_err("oversized repeated products must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("limit of 65535") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "let pair: I32[2] = (1, 2)\nlet invalid = pair[2]\n",
        ))
        .expect_err("known out-of-bounds indices must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("out of bounds") })
    );
}

#[test]
fn aliases_complete_erased_references_and_unsized_types_but_rejects_ffi() {
    type_check(concat!(
        "type alias Ints = Ref I32[]\n",
        "let fixed: Ref I32[2] = Ref (1, 2)\n",
        "let values: Ints = fixed\n",
        "let count: USize = length values\n",
    ));

    type_check("type alias Slice = I32[]\n");
    type_check(concat!(
        "type alias Slice = I32[]\n",
        "let fixed: Ref I32[2] = Ref (1, 2)\n",
        "let values: Ref Slice = fixed\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve("type alias Slice = I32[]\nlet invalid: Slice\n"))
        .expect_err("unsized aliases cannot be used by value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("unsized type") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("extern \"c\" { let invalid: Ref I32[] -> I32 }\n"))
        .expect_err("erased references must not cross the FFI");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("external binding types cannot contain erased products")
    }));
}

#[test]
fn enforces_implicit_sized_and_supports_question_sized_parameters() {
    type_check(concat!(
        "type alias Slice = E => E[]\n",
        "def preserve: T => ?Sized T => Ref T -> Ref T = value => value\n",
        "def explicitly_sized: T => ?Sized T => Sized T => Ref T -> Ref T = value => value\n",
        "let fixed: Ref I32[2] = Ref (1, 2)\n",
        "let erased: Ref (Slice I32) = fixed\n",
        "let same: Ref (Slice I32) = preserve erased\n",
        "let same_fixed: Ref I32[2] = explicitly_sized fixed\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def sized_only: T => Ref T -> Ref T = value => value\n",
            "let fixed: Ref I32[2] = Ref (1, 2)\n",
            "let erased: Ref I32[] = fixed\n",
            "let invalid = sized_only erased\n",
        )))
        .expect_err("ordinary generic parameters have an implicit Sized bound");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("implicit `Sized` bound") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "def invalid: T => ?Sized T => T -> () = value => ()\n",
        ))
        .expect_err("a relaxed parameter cannot be passed by value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("must be sized") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("impl Sized I32 {}\n"))
        .expect_err("Sized is structural");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`Sized` is implemented structurally")
    }));
}

#[test]
fn derives_default_structurally_for_products() {
    let source = concat!(
        "type Seed = I32\n",
        "impl Default Seed { def default = () => Seed 7 }\n",
        "let integers: I32[3] = default ()\n",
        "let mixed: (I32, Bool, String) = default ()\n",
        "let nested: I32[2][2] = default ()\n",
        "let seeds: Seed[2] = default ()\n",
        "let answer: I32 = integers.0 + integers.1 + integers.2\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("derived product defaults should generate LLVM");
    assert!(llvm.contains("default.element"));
    assert!(llvm.contains("default.call"));
}

#[test]
fn rejects_default_for_products_with_non_default_elements() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let invalid: (Ref I32)[2] = default ()\n"))
        .expect_err("Ref has no Default implementation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("no trait implementation"))
    );
}

fn infix_call(
    expression: &stapler::Expression,
) -> (&str, &stapler::Expression, &stapler::Expression) {
    let stapler::Expression::Call(outer) = expression else {
        panic!("expected lowered outer call");
    };
    let stapler::Expression::Call(inner) = outer.callee.as_ref() else {
        panic!("expected lowered inner call");
    };
    let stapler::Expression::Name(operator) = inner.callee.as_ref() else {
        panic!("expected operator name");
    };
    (&operator.name, &inner.argument, &outer.argument)
}

#[test]
fn resolves_names_in_function_parameters() {
    let source = "def first: _ -> I32 = (a: I32, b: I32) => a\nfirst (1, 2)\n";
    let module = resolve(source);

    assert_eq!(module.syntax().text(), source);
    assert!(
        module
            .functions()
            .iter()
            .any(|function| function.name == "first")
    );
}

#[test]
fn compiler_diagnostics_report_line_and_column() {
    let syntax = parse("let value = 1\nmissing\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("unknown name should not resolve");

    assert_eq!(
        diagnostics[0].to_string(),
        "unknown name `missing` at line 2, column 1"
    );
}

#[test]
fn infers_and_checks_function_return_types() {
    let module = type_check(concat!(
        "let first = (a: I32, b: I32) => a\n",
        "let second = (a: I32, b: I32) => b satisfies I32\n",
        "first (1, second (3, 2))\n",
    ));

    for function in module
        .functions()
        .iter()
        .filter(|function| matches!(function.name.as_str(), "first" | "second"))
    {
        let function_type = module
            .type_of_function(function.id)
            .expect("function should have a checked type");
        assert_eq!(*function_type.result, CheckedType::I32);
    }
}

#[test]
fn checks_general_satisfies_expressions_and_contextually_types_functions() {
    type_check(concat!(
        "let small = 42 satisfies I8\n",
        "let identity = (value => value) satisfies I32 -> I32\n",
        "let result: I32 = identity 42\n",
    ));

    let module = resolve("let invalid = \"text\" satisfies I32\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("an expression must satisfy its asserted type");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `I32`, found `String`")
    }));
}

#[test]
fn infers_and_lowers_nominal_sums_with_propagation() {
    let module = type_check(concat!(
        "pub(repr) type Ok = T => T\n",
        "pub(repr) type IOError = String\n",
        "def read: String -> Ok String | IOError = path => Ok(path)\n",
        "def parse = (path: String) => { let Ok(file)? = read(path); Ok(file) }\n",
        "parse \"input\"\n",
    ));
    let parse = module
        .functions()
        .iter()
        .find(|function| function.name == "parse")
        .expect("parse function");
    let result = &module
        .type_of_function(parse.id)
        .expect("parse type")
        .result;
    let CheckedType::Sum(sum) = result.as_ref() else {
        panic!("expected inferred sum, found {result}");
    };
    assert_eq!(sum.alternatives.len(), 2);

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("sum propagation should generate LLVM");
    assert!(llvm.contains("propagate.ok"));
    assert!(llvm.contains("propagate.return"));
}

#[test]
fn exhaustively_matches_sum_values_and_destructures_payloads() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "def choose = result: Ok I32 | IOError => match result {\n",
        "  Ok value => value,\n",
        "  IOError _ => 0,\n",
        "}\n",
        "choose (Ok 7)\n",
    ));
    let choose = module
        .functions()
        .iter()
        .find(|function| function.name == "choose")
        .expect("choose function");
    assert_eq!(
        *module
            .type_of_function(choose.id)
            .expect("choose type")
            .result,
        CheckedType::I32
    );
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("match should generate LLVM");
    assert!(llvm.contains("match.tag"));
    assert!(llvm.contains("match.tag.matches"));
    assert!(llvm.contains("match.value"));
}

#[test]
fn matches_singletons_and_joins_nominal_arm_results() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "def choose = value: Bool => match value {\n",
        "  True => Ok(1),\n",
        "  False => IOError(\"no\"),\n",
        "}\n",
        "choose True\n",
    ));
    let choose = module
        .functions()
        .iter()
        .find(|function| function.name == "choose")
        .expect("choose function");
    let CheckedType::Sum(sum) = module
        .type_of_function(choose.id)
        .expect("choose type")
        .result
        .as_ref()
    else {
        panic!("expected inferred sum result");
    };
    assert_eq!(sum.alternatives.len(), 2);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("singleton match should generate LLVM");
}

#[test]
fn supports_match_catch_alls_wildcard_parameters_and_returning_arms() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "def preserve: (Ok I32 | IOError) -> Ok I32 | IOError = result => match result {\n",
        "  Ok value => Ok(value),\n",
        "  other => other,\n",
        "}\n",
        "def discard: I32 -> () = _ => { let _ = 1; () }\n",
        "def number = value: Bool => match value {\n",
        "  True() => { return 1 },\n",
        "  False() => { return 2 },\n",
        "}\n",
        "def subject_returns = () => match { return 3 } {\n",
        "  True() => 1,\n",
        "  False() => 2,\n",
        "}\n",
        "preserve (Ok 1)\n",
        "discard 1\n",
        "number False\n",
        "subject_returns ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("catch-all and returning matches should generate LLVM");
}

#[test]
fn matches_values_of_any_runtime_type() {
    let module = type_check(concat!(
        "pub(repr) type Wrapped = I32\n",
        "def integer = value: I32 => match value { number => number }\n",
        "def float = value: F64 => match value { _: F64 => value }\n",
        "def identity = value: I32 => value\n",
        "def function = value: (I32 -> I32) => match value { callable => callable 4 }\n",
        "def nominal = value: Wrapped => match value { Wrapped number => number }\n",
        "def generic: T => T -> T = value => match value { same: T => same }\n",
        "integer 1\n",
        "float 1.5\n",
        "function identity\n",
        "nominal (Wrapped 2)\n",
        "generic 3\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("matches over arbitrary runtime values should generate LLVM");
}

#[test]
fn rejects_invalid_match_patterns_and_coverage() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def invalid = value: I32 => match value {\n",
            "  text: String => 0,\n",
            "  _ => 1,\n",
            "}\n",
        )))
        .expect_err("an incompatible typed pattern should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `String`, found `I32`")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "match 1 {\n",
            "  first => first,\n",
            "  second => second,\n",
            "}\n",
        )))
        .expect_err("an arm after a scalar catch-all should be unreachable");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unreachable match arm"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "def invalid = result: Ok I32 | IOError => match result { Ok value => value, }\n",
            "invalid (Ok 1)\n",
        )))
        .expect_err("non-exhaustive matches should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("non-exhaustive match")
            && diagnostic.message.contains("IOError")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "def invalid = result: Ok I32 | IOError => match result {\n",
            "  Ok value => value,\n",
            "  Ok other => other,\n",
            "  _ => 0,\n",
            "  IOError _ => 1,\n",
            "}\n",
            "invalid (Ok 1)\n",
        )))
        .expect_err("unreachable match arms should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("unreachable match arm") })
    );
}

#[test]
fn exhaustively_matches_product_combinations() {
    let module = type_check(concat!(
        "def same = (left: Bool, right: Bool) => match (left, right) {\n",
        "  (True(), True()) => True,\n",
        "  (False(), False()) => True,\n",
        "  _ => False,\n",
        "}\n",
        "def table = (left: Bool, right: Bool) => match (left, right) {\n",
        "  (True, True) => True,\n",
        "  (True, False) => False,\n",
        "  (False, True) => False,\n",
        "  (False, False) => True,\n",
        "}\n",
        "same (True, False)\n",
        "table (False, False)\n",
    ));
    let same = module
        .functions()
        .iter()
        .find(|function| function.name == "same")
        .expect("same function");
    let CheckedType::Sum(result) = module
        .type_of_function(same.id)
        .expect("same type")
        .result
        .as_ref()
    else {
        panic!("expected Bool result");
    };
    assert_eq!(result.alternatives.len(), 2);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("product match should generate LLVM");
    assert!(llvm.matches("match.element").count() >= 2);
    assert!(llvm.contains("match.next"));
}

#[test]
fn checks_product_match_coverage_and_reachability() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def incomplete = (left: Bool, right: Bool) => match (left, right) {\n",
            "  (True(), True()) => True,\n",
            "  (True(), False()) => False,\n",
            "}\n",
            "incomplete (True, True)\n",
        )))
        .expect_err("incomplete product match should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("non-exhaustive match"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def duplicate = (left: Bool, right: Bool) => match (left, right) {\n",
            "  (True(), True()) => True,\n",
            "  (True(), True()) => False,\n",
            "  _ => False,\n",
            "}\n",
            "duplicate (True, True)\n",
        )))
        .expect_err("duplicate product arm should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unreachable match arm"))
    );
}

#[test]
fn injects_and_widens_sum_values() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "pub(repr) type ParseError = String\n",
        "type alias Result = T => Ok T | IOError\n",
        "def small: () -> Ok I32 | IOError = () => Ok(1)\n",
        "let reordered: IOError | Ok I32 = small()\n",
        "let aliased: Result I32 = reordered\n",
        "let duplicate: Ok I32 | Ok I32 = Ok(3)\n",
        "let pair: (Result I32, I32) = (aliased, 2)\n",
        "def captured = () => { let local: Result I32 = pair.0; let inner = () => local; inner() }\n",
        "def wide: () -> Ok I32 | IOError | ParseError = () => small()\n",
        "captured()\n",
        "wide()\n",
    ));
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    generator
        .compile_module(&module)
        .expect("sum injection and widening should generate valid LLVM");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow the Unix epoch")
        .as_nanos();
    let output = std::env::temp_dir().join(format!("stapler-sum-test-{nonce}.o"));
    generator
        .emit_object(&module, &output, None)
        .expect("sum values should emit a native object");
    std::fs::remove_file(output).expect("temporary sum object should be removable");
}

#[test]
fn propagates_each_residual_variant_and_joins_explicit_returns() {
    let module = type_check(concat!(
        "pub(repr) type IOError = String\n",
        "pub(repr) type ParseError = String\n",
        "def fail: () -> Ok I32 | IOError = () => IOError(\"io\")\n",
        "def parse = () => { let Ok(value)? = fail(); return ParseError(\"parse\"); }\n",
        "parse()\n",
    ));
    let parse = module
        .functions()
        .iter()
        .find(|function| function.name == "parse")
        .expect("parse function");
    let CheckedType::Sum(sum) = module
        .type_of_function(parse.id)
        .expect("parse type")
        .result
        .as_ref()
    else {
        panic!("expected inferred sum");
    };
    assert_eq!(sum.alternatives.len(), 2);
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("residual-only propagation widening should generate LLVM");
}

#[test]
fn supports_arbitrary_sized_sum_alternatives_and_typed_matches() {
    let module = type_check(concat!(
        "def select = value: I32 | String => match value {\n",
        "  number: I32 => number,\n",
        "  text: String => 0,\n",
        "}\n",
        "let integer: I32 | String = 41\n",
        "let text: I32 | String = \"hello\"\n",
        "let pair: (I32, I32) | F64 = (1, 2)\n",
        "def identity = value: I32 => value\n",
        "let function: (I32 -> I32) | String = identity\n",
        "let applied: Ok I32 | Ok String = Ok(3)\n",
        "def small: () -> I32 | String = () => 7\n",
        "let widened: I32 | String | F64 = small()\n",
        "def generic: T => T -> T | String = value => value\n",
        "generic 9\n",
        "select(integer)\n",
        "select(text)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("arbitrary sized sum alternatives should lower to LLVM");
}

#[test]
fn rejects_unsized_sum_alternatives_and_ambiguous_nominal_patterns() {
    for (source, expected) in [
        (
            "let value: I32[] | String\n",
            "must be a fully known sized type",
        ),
        (
            concat!(
                "def inspect = value: Ok I32 | Ok String => match value {\n",
                "  Ok(payload) => 0,\n",
                "  _ => 1,\n",
                "}\n",
            ),
            "selects more than one alternative",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("invalid sum should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn rejects_invalid_propagation_and_sum_ffi() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "def invalid = () => { let Ok(value)? = Ok(1); Ok(value) }\n",
        )))
        .expect_err("propagation requires a sum");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("propagating binding requires a sum value")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "def read: () -> Ok I32 | IOError = () => Ok(1)\n",
            "def invalid: () -> Ok I32 = () => { let Ok(value)? = read(); Ok(value) }\n",
        )))
        .expect_err("explicit result should contain propagated variants");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("propagated variant `IOError` is not contained")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "pub(repr) type IOError = String\n",
            "extern \"c\" { let invalid: () -> Ok I32 | IOError }\n",
        )))
        .expect_err("sum ABI should remain internal");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("external binding types cannot contain sums")
    }));
}

#[test]
fn rejects_propagation_outside_functions_and_non_nominal_roots() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "pub(repr) type IOError = String\n",
                "let Ok(value)? = Ok(1)\n",
                "def invalid = () => { let (Ok(value), other)? = (Ok(1), 2); Ok(value) }\n",
            ),
            root,
        )
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("invalid propagation contexts should not resolve");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("only allowed inside a function")
    }));
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("requires a nominal pattern") })
    );
}

#[test]
fn infers_return_types_through_forward_function_references() {
    let module = type_check(concat!(
        "def first = () => second ()\n",
        "def second = () => 42\n",
        "first ()\n",
    ));

    for function in module
        .functions()
        .iter()
        .filter(|function| matches!(function.name.as_str(), "first" | "second"))
    {
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("function should have a checked type")
                .result,
            CheckedType::I32,
        );
    }
}

#[test]
fn rejects_an_immediate_read_before_a_def_initializer() {
    let syntax = parse("value\ndef value = 10\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("an immediate forward read must fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("read before it is initialized"))
    );
}

#[test]
fn permits_deferred_mutual_recursion_with_selective_state() {
    let module = type_check(concat!(
        "def f: () -> I32 = () => g ()\n",
        "def g: () -> I32 = () => f ()\n",
        "f ()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("capturing a forward binding must remain legal");
    assert!(llvm.contains("_g_state"));
    assert!(llvm.contains("binding.uninitialized"));
    assert!(!llvm.contains("_f_state"));
}

#[test]
fn captures_potentially_unsafe_local_defs_by_binding_cell() {
    let module = type_check(concat!(
        "def outer: () -> I32 = () => {\n",
        "  def f: () -> I32 = () => g ()\n",
        "  def g: () -> I32 = () => f ()\n",
        "  f ()\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("unsafe local captures should share a binding cell");
    assert!(llvm.contains("binding.cell"));
    assert!(llvm.contains("binding.uninitialized"));
}

#[test]
fn applies_initialization_state_to_recursive_local_generics() {
    let module = type_check(concat!(
        "def outer: () -> I32 = () => {\n",
        "  def recur: T => T -> T = value => recur value\n",
        "  recur 1\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic functions use the same initialization state model");
    assert!(llvm.contains("binding.state.cell"));
    assert!(llvm.contains("binding.uninitialized"));
}

#[test]
fn does_not_add_state_metadata_to_safe_bindings() {
    let module = type_check("def answer = 42\nanswer\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("safe bindings should compile without state metadata");
    assert!(!llvm.contains("_answer_state"));
    assert!(!llvm.contains("binding.uninitialized"));
}

#[test]
fn rejects_an_incorrect_function_result_type() {
    let module = resolve("use std.cinterop *\nlet answer = () => 42 satisfies CString\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("incorrect return type should fail");

    assert!(
        diagnostics[0]
            .message
            .contains("expected `CString`, found `I32`")
    );
}

#[test]
fn rejects_incorrect_call_arguments() {
    let module = resolve("let identity = (value: I32) => value\nidentity (\"wrong\")\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("incorrect argument type should fail");

    assert!(
        diagnostics[0]
            .message
            .contains("expected `I32`, found `String`")
    );
}

#[test]
fn treats_singleton_products_as_their_element() {
    let module = type_check(concat!(
        "let answer: (I32) = 42\n",
        "let identity = (value: I32) => value\n",
        "identity (answer)\n",
    ));
    let function = module
        .functions()
        .iter()
        .find(|function| function.name == "identity")
        .expect("identity should resolve");
    let function_type = module
        .type_of_function(function.id)
        .expect("identity should have a type");

    assert_eq!(*function_type.parameter, CheckedType::I32);

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("singleton products should compile as their element");
    assert!(llvm.contains("define i32 @identity(ptr %0, i32 %value)"));
    assert!(!llvm.contains("define i32 @identity(ptr %0, { i32 }"));
}

#[test]
fn destructures_nested_product_patterns() {
    let module = type_check(concat!(
        "let add_nested = (x: I32, (y: I32, z: I32)) => x\n",
        "add_nested (1, (2, 3))\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nested product pattern should compile");

    assert!(llvm.contains("define i32 @add_nested(ptr %0, i32 %x, <{ i32, i32 }>"));
    assert!(llvm.contains("extractvalue <{ i32, i32 }>"));
}

#[test]
fn binds_a_product_without_destructuring_it() {
    let module = type_check(concat!(
        "let sum = pair: (I32, I32) => pair.0\n",
        "sum (1, 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a product binding pattern should compile");

    assert!(llvm.contains("define i32 @sum(ptr %0, i32 %1, i32 %2)"));
    assert!(llvm.contains("extractvalue <{ i32, i32 }>"));
}

#[test]
fn type_checks_transparent_aliases() {
    type_check("type alias number = I32\nlet answer: number = 42\n");
}

#[test]
fn uses_regular_prelude_functions_for_i32_arithmetic() {
    let module = type_check(concat!(
        "def add_one = (+) 1\n",
        "let sum = 1 + 2\n",
        "let difference = 4 - 3\n",
        "let product = 2 * 3\n",
        "let quotient = 8 / 2\n",
        "add_one 2\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("prelude arithmetic should compile");

    assert!(llvm.contains("add i32"));
    assert!(llvm.contains("sub i32"));
    assert!(llvm.contains("mul i32"));
    assert!(llvm.contains("sdiv i32"));
    assert!(llvm.contains("closure.call"));
    assert!(!llvm.contains("declare i32 @__i32_add"));
}

#[test]
fn compares_all_standard_library_integer_types() {
    let module = type_check(concat!(
        "let equal: Bool = 1 == 1\n",
        "let not_equal: Bool = 1 != 2\n",
        "let less: Bool = 1 < 2\n",
        "let less_equal: Bool = 1 <= 2\n",
        "let greater: Bool = 2 > 1\n",
        "let greater_equal: Bool = 2 >= 1\n",
        "def same: T => Copy T => Eq T => T -> T -> Bool = left => right => left == right\n",
        "def before: T => PartialOrd T => T -> T -> Bool = left => right => left < right\n",
        "let generic_equal: Bool = same 1 1\n",
        "let generic_order: Bool = before 1 2\n",
        "def i8 = (x: I8, y: I8) => x < y\n",
        "def i16 = (x: I16, y: I16) => x < y\n",
        "def i64 = (x: I64, y: I64) => x < y\n",
        "def u8 = (x: U8, y: U8) => x < y\n",
        "def u16 = (x: U16, y: U16) => x < y\n",
        "def u32 = (x: U32, y: U32) => x < y\n",
        "def u64 = (x: U64, y: U64) => x < y\n",
        "def isize = (x: ISize, y: ISize) => x < y\n",
        "def usize = (x: USize, y: USize) => x < y\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("integer comparisons should compile");

    for predicate in [
        "icmp eq", "icmp ne", "icmp slt", "icmp sgt", "icmp ult", "icmp ugt",
    ] {
        assert!(llvm.contains(predicate), "missing `{predicate}` in LLVM");
    }
    assert!(llvm.contains("bool.tag"));
}

#[test]
fn supports_contextual_float_literals_arithmetic_and_partial_ordering() {
    let module = type_check(concat!(
        "extern \"c\" { let scale_float: F64 -> F64 }\n",
        "def single = () => { let a: F32 = 1.5; let b: F32 = .5; (a + b) * b - a / b; } satisfies F32\n",
        "def double = () => { let a: F64 = 1e3; let b: F64 = 2.; (a + b) / b; } satisfies F64\n",
        "def defaulted = () => 1.25\n",
        "let less: Bool = 1.0 < 2.0\n",
        "let equal: Bool = 2.0 == 2.0\n",
        "let scaled: F64 = scale_float 2.0\n",
        "let unordered: Option Ordering = PartialOrd.partial_cmp (0.0 / 0.0) 1.0\n",
        "let ordered_less: Ordering = Ord.cmp 1 2\n",
        "let ordered_equal: Ordering = Ord.cmp 2 2\n",
        "let ordered_greater: Ordering = Ord.cmp 3 2\n",
        "single ()\n",
    ));
    let defaulted = module
        .functions()
        .iter()
        .find(|function| function.name == "defaulted")
        .unwrap();
    assert_eq!(
        *module.type_of_function(defaulted.id).unwrap().result,
        CheckedType::F64
    );

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("float operations should compile");
    for instruction in [
        "fadd float",
        "fmul float",
        "fsub float",
        "fdiv float",
        "fadd double",
        "fdiv double",
        "fcmp olt",
        "fcmp oeq",
        "fcmp une",
    ] {
        assert!(
            llvm.contains(instruction),
            "missing `{instruction}` in LLVM"
        );
    }
    assert!(llvm.contains("declare double @scale_float(double)"));
}

#[test]
fn rejects_invalid_float_contexts_and_float_ord() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let value: F32 = 1e100\n"))
        .expect_err("overflowing F32 literal should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not fit in `F32`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("let value: F32 = 1\n"))
        .expect_err("integer literals should not become floats");
    assert!(!diagnostics.is_empty());

    let diagnostics = TypeChecker::new()
        .check(resolve("let value = 1e+\n"))
        .expect_err("an incomplete exponent should fail type checking");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("float literal"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("def compare: T => Ord T => T -> T -> Ordering = left => right => Ord.cmp left right\nlet invalid = compare 1.0 2.0\n"))
        .expect_err("floats should not implement Ord");
    assert!(!diagnostics.is_empty());
}

#[test]
fn requires_only_the_core_ordering_methods() {
    let diagnostics = TypeChecker::new()
        .check(resolve("impl PartialOrd String {}\n"))
        .expect_err("partial_cmp is required");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing member `partial_cmp`"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("impl Ord String {}\n"))
        .expect_err("cmp is required");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing member `cmp`"))
    );
}

#[test]
fn compares_library_defined_bool_values() {
    let module = type_check(concat!(
        "let yes: Bool = True\n",
        "let no: Bool = False\n",
        "let true_equals_true: Bool = yes == yes\n",
        "let false_equals_false: Bool = no == no\n",
        "let true_differs_false: Bool = yes != no\n",
        "let false_differs_true: Bool = no != yes\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("Bool equality should dispatch through its standard-library implementation");
    assert!(llvm.contains("match.tag.matches"));
}

#[test]
fn lowercase_i32_is_an_ordinary_unresolved_type_name() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let value: i32 = 1\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("lowercase i32 should not name the builtin");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `i32`")
    );
}

#[test]
fn bool_is_an_auto_loaded_standard_library_type() {
    let module = type_check(concat!(
        "let yes: Bool = True\n",
        "let no: Bool = False\n",
        "def require_true = (value: Bool) => { let True()? = value; True }\n",
        "require_true(yes)\n",
    ));
    let require_true = module
        .functions()
        .iter()
        .find(|function| function.name == "require_true")
        .expect("require_true function");
    assert!(matches!(
        module
            .type_of_function(require_true.id)
            .expect("require_true type")
            .result
            .as_ref(),
        CheckedType::Sum(sum) if sum.alternatives.len() == 2
    ));

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("library-defined Bool should use ordinary sum lowering");
}

#[test]
fn lowercase_bool_is_an_ordinary_unresolved_type_name() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let predicate: bool\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("lowercase bool should not name the builtin");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `bool`")
    );
}

#[test]
fn cinterop_types_require_an_explicit_import() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let text: CString\n", root)
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("CString should not be in the prelude");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `CString`")
    );

    type_check(concat!(
        "use std.cinterop *\n",
        "let character: CChar\n",
        "let pointer: CPointer CChar\n",
        "let text: CString\n",
    ));
}

#[test]
fn syntax_types_and_quote_require_an_explicit_import() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("type alias Captured = SyntaxNode\n", root)
        .expect("source should load without importing syntax names");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("syntax types should not be in the core prelude");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown type `SyntaxNode`"),
        "{diagnostics:#?}",
    );

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro identity = value => quote { $value }\nidentity 1\n",
            root,
        )
        .expect("quote syntax should parse before import resolution");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("quote should require an explicit import");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "`quote` requires an explicit import from `std.syntax`"
    }));

    resolve(concat!(
        "use std.syntax (quote, Expr)\n",
        "macro identity: Expr -> Expr = value => quote { $value }\n",
        "let result: I32 = identity 42\n",
    ));
}

#[test]
fn c_pointer_preserves_its_pointee_type() {
    let module = resolve(concat!(
        "use std.cinterop *\n",
        "extern \"c\" { let consume: (CPointer I32) -> I32 }\n",
        "consume (c_string \"wrong pointee\")\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("CString should only coerce to CPointer CChar");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `CPointer I32`, found `CString`")
    }));
}

#[test]
fn generic_opaque_arguments_are_part_of_type_identity() {
    let module = resolve(concat!(
        "type Handle = T => opaque\n",
        "def invalid: Handle I32 -> Handle String = value => value\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("opaque applications with different arguments must differ");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `Handle String`, found `Handle I32`")
    }));
}

#[test]
fn string_literals_have_the_canonical_string_type() {
    let module = type_check("\"hello\"\n");
    let Item::Statement(statement) = &module.syntax().items[0] else {
        panic!("expected expression statement");
    };
    let Statement::Expression(expression) = statement.as_ref() else {
        panic!("expected expression statement");
    };

    assert_eq!(
        module.type_of_expression(expression.syntax().id),
        Some(&CheckedType::String)
    );
}

#[test]
fn validates_the_standard_library_string_representation() {
    assert!(
        string_contract_diagnostics(concat!(
            "pub type String = Ref U8[]\n",
            "def bytes: String -> Ref U8[] = String value => value\n",
            "def matched_bytes: String -> Ref U8[] = value => match value { String bytes => bytes, }\n",
        ))
        .is_empty()
    );

    for (declaration, expected) in [
        (
            "pub type String = opaque\n",
            "standard library type `String` must be a represented distinct type",
        ),
        (
            "pub(repr) type String = Ref U8[]\n",
            "standard library type `String` must keep its representation private",
        ),
        (
            "pub type String = T => Ref U8[]\n",
            "standard library type `String` must not accept compile-time arguments",
        ),
        (
            "pub type String = Ref I8[]\n",
            "standard library type `String` must be represented by `Ref U8[]`, found `Ref I8[]`",
        ),
    ] {
        let diagnostics = string_contract_diagnostics(declaration);
        assert!(
            diagnostics.iter().any(|message| message == expected),
            "missing `{expected}` in {diagnostics:?}",
        );
    }
}

#[test]
fn c_string_is_an_imported_primitive_macro() {
    let module = type_check(concat!(
        "use std.cinterop *\n",
        "def exercise = () => {\n",
        "  let text: String = \"hello\"\n",
        "  let c_text: CString = c_string \"hello\"\n",
        "  let copied: String = string_from_c_string c_text\n",
        "  let converted: CString = string_to_c_string text\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("String operations should compile");

    assert!(llvm.contains("string.pointer"));
    assert!(llvm.contains("string.length"));
    assert!(!llvm.contains("string.capacity"));
    assert!(!llvm.contains("string.allocate"));
    assert!(llvm.contains("@strlen"));
    assert!(llvm.contains("@memchr"));
    assert!(llvm.contains("@llvm.trap"));
    assert!(llvm.contains("c\"hello\\00\""));
}

#[test]
fn c_string_rejects_non_literal_arguments() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "use std.cinterop *\n",
                "let text = \"hello\"\n",
                "c_string text\n",
            ),
            root,
        )
        .expect("source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("c_string should require a literal during expansion");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "`c_string` requires a string literal")
    );
}

#[test]
fn c_string_rejects_interior_nul_bytes() {
    let module = type_check("use std.cinterop *\nc_string \"bad\\0value\"\n");
    let context = Context::create();
    let diagnostics = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect_err("interior NUL should fail code generation");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "C string literals cannot contain an interior NUL byte"
    }));
}

#[test]
fn expands_user_defined_macros_hygienically() {
    let module = type_check(concat!(
        "macro keep = value => quote { { let temporary = 1; $value } }\n",
        "let temporary = 2\n",
        "let result: I32 = keep temporary\n",
    ));
    assert!(
        module
            .resolved()
            .syntax()
            .items
            .iter()
            .any(|item| matches!(item, Item::MacroDeclaration(_)))
    );
}

#[test]
fn inspects_identifier_and_call_syntax() {
    let module = type_check(concat!(
        "macro call_argument = value: CallExpr => match value {\n",
        "    CallExpr (_, argument) => argument,\n",
        "}\n",
        "macro call_callee = value: CallExpr => value.callee\n",
        "macro classify_ident = value: Ident String => match value {\n",
        "    Ident \"target\" => quote { 1 },\n",
        "    Ident _ => quote { 2 },\n",
        "}\n",
        "def identity = (value: I32) => value\n",
        "let argument: I32 = call_argument (identity 41)\n",
        "let callee: I32 = call_callee (identity 42) 0\n",
        "let spelling: I32 = classify_ident target\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("structured syntax inspection should generate code");
}

#[test]
fn constructs_identifier_and_call_syntax() {
    let module = type_check(concat!(
        "macro generated_name = _: Expr => Ident \"answer\"\n",
        "macro generated_call = callee: Expr => argument: Expr =>\n",
        "    CallExpr (callee: callee, argument: argument)\n",
        "def identity = (value: I32) => value\n",
        "let answer: I32 = 40\n",
        "let name: I32 = generated_name ()\n",
        "let call: I32 = generated_call identity 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("constructed syntax should generate code");
}

#[test]
fn structured_syntax_overloads_use_leaf_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Expr => quote { 1 }\n",
        "macro classify = _: CallExpr => quote { 2 }\n",
        "macro classify = _: UnstructuredExpr => quote { 3 }\n",
        "macro classify_name = _: Expr => quote { 4 }\n",
        "macro classify_name = _: Ident String => quote { 5 }\n",
        "let call: I32 = classify (f 0)\n",
        "let other: I32 = classify 0\n",
        "let name: I32 = classify_name identifier\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("structured syntax overloads should select a unique leaf");
}

#[test]
fn constructed_identifiers_use_definition_hygiene_and_children_keep_caller_hygiene() {
    let module = type_check(concat!(
        "macro definition_name = _: Expr => Ident \"captured\"\n",
        "macro apply = callee: Expr => argument: Expr =>\n",
        "    CallExpr (callee: callee, argument: argument)\n",
        "let captured: I32 = 7\n",
        "def definition_site = (captured: String) => definition_name ()\n",
        "def caller_site = (local: (I32 -> I32)) => apply local 8\n",
        "let first: I32 = definition_site \"shadow\"\n",
        "let second: I32 = caller_site (value => value)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("constructed syntax should retain the appropriate hygiene context");
}

#[test]
fn mutates_call_syntax_with_value_semantics_and_shared_capture_cells() {
    let module = type_check(concat!(
        "macro replace_call = value: CallExpr => replacement: Expr => {\n",
        "    let original = value\n",
        "    let mut changed = value\n",
        "    changed.argument = replacement\n",
        "    quote { ($original, $changed) }\n",
        "}\n",
        "macro replace_from_closure = value: CallExpr => replacement: Expr => {\n",
        "    let mut changed = value\n",
        "    let update = () => { changed.argument = replacement; () }\n",
        "    update ()\n",
        "    changed\n",
        "}\n",
        "def identity = (value: I32) => value\n",
        "let pair: (I32, I32) = replace_call (identity 1) 2\n",
        "let captured: I32 = replace_from_closure (identity 3) 4\n",
    ));
    let pair = module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| {
            let Item::Statement(statement) = item else {
                return None;
            };
            let Statement::Binding(binding) = statement.as_ref() else {
                return None;
            };
            (binding.name == "pair").then_some(binding.value.as_ref()?)
        })
        .expect("expanded pair binding");
    let stapler::Expression::Product(pair) = pair else {
        panic!("replacement macro should expand to a product");
    };
    let arguments = pair
        .elements
        .iter()
        .map(|element| {
            let stapler::Expression::Call(call) = &element.value else {
                panic!("pair element should remain a call");
            };
            let stapler::Expression::Integer(argument) = call.argument.as_ref() else {
                panic!("call argument should be an integer");
            };
            argument.literal.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(arguments, ["1", "2"]);

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("mutated syntax should generate code");
}

#[test]
fn diagnoses_invalid_structured_syntax_operations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "macro invalid = value: CallExpr => { value.argument = quote { 1 }; value }\ninvalid (f 0)\n",
            "cannot assign to immutable compile-time binding `value`",
        ),
        (
            "macro invalid = value: CallExpr => value.missing\ninvalid (f 0)\n",
            "call syntax has no field `missing`",
        ),
        (
            "macro invalid = value: Expr => CallExpr (callee: value, argument: 1)\ninvalid f\n",
            "`CallExpr.argument` must contain `Expr`",
        ),
        (
            "macro invalid = value: Expr => Ident \"not an identifier\"\ninvalid f\n",
            "`not an identifier` is not a valid identifier spelling",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid structured syntax operation should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn rejects_structured_syntax_values_at_runtime() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "let generated: CallExpr\n",
            "`Syntax` values are compile-time-only",
        ),
        (
            "let generated = Ident \"name\"\n",
            "`Ident` syntax values are compile-time-only",
        ),
        (
            "let generated = CallExpr (callee: 1, argument: 2)\n",
            "`CallExpr` syntax values are compile-time-only",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("runtime syntax values should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn expands_explicit_and_inferred_item_macros() {
    let module = type_check(concat!(
        "macro define_answer: Expr -> Item = value => quote {\n",
        "    def answer = () => $value\n",
        "}\n",
        "def item_identity: Item -> Item = item => item\n",
        "macro define_type = _: Expr => item_identity quote { type Generated }\n",
        "define_answer 42\n",
        "define_type ()\n",
        "let result: I32 = answer ()\n",
        "let generated: Generated = Generated\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, Item::Statement(statement)
            if matches!(statement.as_ref(), Statement::Binding(binding)
                if binding.name == "answer"))
    }));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, Item::TypeDeclaration(declaration) if declaration.name == "Generated")
    }));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generated bindings and types should generate code");
}

#[test]
fn expands_item_modifier_macros_with_nearest_modifier_first() {
    let module = type_check(concat!(
        "macro @identity = item => item\n",
        "macro @outer: Item -> Item = _ => quote { let selected: I32 = 42 }\n",
        "macro @inner: Item -> Item = _ => quote { let selected: String = \"inner\" }\n",
        "@identity\n",
        "@outer\n",
        "@inner\n",
        "let original = 0\n",
        "let result: I32 = selected\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, Item::Statement(statement)
            if matches!(statement.as_ref(), Statement::Binding(binding)
                if binding.name == "selected"))
    }));
}

#[test]
fn modifier_arguments_support_expression_type_and_pattern_syntax() {
    let module = type_check(concat!(
        "macro @value = value => _ => quote { let generated: I32 = $value }\n",
        "macro @typed: Type -> Item -> Item = ty => _ => quote { let typed: $ty = 1 }\n",
        "macro @bind: Pattern -> Item -> Item = pattern => _ => quote { let $pattern = (40, 2) }\n",
        "@value(42)\n",
        "let replaced = 0\n",
        "@typed(I32)\n",
        "let typed_original = 0\n",
        "@bind((left, right))\n",
        "let destructured = 0\n",
        "let result: I32 = typed + left + right\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("modifier arguments and generated items should compile");
}

#[test]
fn expands_modifier_lists_generated_by_item_macros() {
    let module = type_check(concat!(
        "macro @replace: Item -> Item = _ => quote { let generated: I32 = 42 }\n",
        "macro emit: Expr -> Item = _: Expr => quote { @replace let original = 0 }\n",
        "emit ()\n",
        "let result: I32 = generated\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generated modifier lists should expand before resolution");
}

#[test]
fn expands_visibility_aware_macros_and_contextual_visibility_splices() {
    let module = type_check(concat!(
        "def normalize_visibility: Visibility -> Visibility = value => value\n",
        "def visibility_number: Visibility -> Expr = value => match value {\n",
        "    Private => quote { 1 },\n",
        "    Public => quote { 2 },\n",
        "    PublicRepr => quote { 3 },\n",
        "}\n",
        "macro define_alias = vis: MacroCallVisibility => ty: Type => {\n",
        "    let actual = normalize_visibility vis\n",
        "    quote { $actual type alias Generated = $ty }\n",
        "}\n",
        "macro classify = before: Expr => vis: Visibility => after: Expr => {\n",
        "    let number = visibility_number vis\n",
        "    number\n",
        "}\n",
        "macro call_visibility = vis: MacroCallVisibility => visibility_number vis\n",
        "macro first_visibility = vis: Visibility => value: Expr => visibility_number vis\n",
        "macro final_visibility = value: Expr => vis: Visibility => visibility_number vis\n",
        "pub define_alias I32\n",
        "let implicit: I32 = classify 10 20\n",
        "let public: I32 = classify 10 pub 20\n",
        "let represented: I32 = classify 10 pub(repr) 20\n",
        "let private_call: I32 = call_visibility\n",
        "let first_private: I32 = first_visibility 0\n",
        "let first_public: I32 = first_visibility pub 0\n",
        "let final_private: I32 = final_visibility 0\n",
        "let final_public: I32 = final_visibility 0 pub\n",
        "let generated: Generated = 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("visibility-aware macro output should compile");
}

#[test]
fn expands_standard_typegroup_into_module_variants_and_alias() {
    let module = type_check(concat!(
        "pub(repr) typegroup Pattern = {\n",
        "    Literal String,\n",
        "    Wildcard,\n",
        "}\n",
        "pub use Pattern *\n",
        "let literal: Pattern = Pattern.Literal \"value\"\n",
        "let wildcard: Pattern = Pattern.Wildcard\n",
        "let reexported_literal: Pattern = Literal \"value\"\n",
        "let reexported_wildcard: Pattern = Wildcard\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generated typegroup variants and alias should compile");
}

#[test]
fn typegroup_supports_generic_groups_and_reexports_their_variants() {
    let module = type_check(concat!(
        "pub(repr) typegroup Maybe = T => {\n",
        "    Missing,\n",
        "    Present T,\n",
        "}\n",
        "pub use Maybe *\n",
        "let missing: Maybe I32 = Missing\n",
        "let present: Maybe I32 = Present 1\n",
        "let qualified: Maybe String = Maybe.Present \"value\"\n",
        "pub(repr) typegroup Either = (L, R,) => {\n",
        "    Left L,\n",
        "    Right R,\n",
        "}\n",
    ));
    let either = module
        .resolved()
        .syntax()
        .items
        .iter()
        .find_map(|item| match item {
            Item::TypeDeclaration(declaration) if declaration.name == "Either" => Some(declaration),
            _ => None,
        })
        .expect("generated Either alias");
    assert_eq!(
        either
            .type_parameters
            .iter()
            .flat_map(stapler::TypeParameterPattern::names)
            .collect::<Vec<_>>(),
        ["L", "R"],
    );
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic typegroup variants and their reexports should compile");
}

#[test]
fn equals_and_fat_arrow_are_structured_syntax_nodes() {
    let module = type_check(concat!(
        "macro punctuation = _: Ident String => equal: Equals => _: Ident String => arrow: FatArrow => _: Braced (Sequence SyntaxNode) =>\n",
        "    match (equal, arrow, Equals, FatArrow) {\n",
        "        (Equals, FatArrow, Equals, FatArrow) => quote { let punctuated: I32 = 42 },\n",
        "    }\n",
        "punctuation marker = T => {}\n",
        "let result: I32 = punctuated\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("equals and fat-arrow syntax should match and construct");
}

#[test]
fn rejects_legacy_typegroup_call_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for source in [
        "typegroup Legacy { Unit, }\n",
        "typegroup Legacy (T) { Wrapped T, }\n",
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("legacy syntax should still parse as an ordinary call");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("legacy typegroup syntax should not expand");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("macro `typegroup` requires")
                    || diagnostic
                        .message
                        .contains("no overload of macro `typegroup`")
            }),
            "expected a typegroup overload diagnostic, found {diagnostics:#?}",
        );
    }
}

#[test]
fn typegroup_variants_require_an_explicit_reexport() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "pub typegroup Status = { Ready, }\n",
                "let qualified: Status = Status.Ready\n",
                "let unqualified: Status = Ready\n",
            ),
            root,
        )
        .expect("typegroup source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("unqualified variants should require an explicit use");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown name `Ready`")),
        "expected an unknown variant diagnostic, found {diagnostics:#?}",
    );
}

#[test]
fn typegroup_supports_private_construction_compound_types_and_trailing_commas() {
    let module = type_check(concat!(
        "typegroup Local = {\n",
        "    Pair (I32, String),\n",
        "    Empty,\n",
        "}\n",
        "pub(repr) typegroup Generic = {\n",
        "    Wrapped Option I32,\n",
        "}\n",
        "pub typegroup PublicGroup = {\n",
        "    Unit,\n",
        "}\n",
        "let pair: Local = Local.Pair (1, \"value\")\n",
        "let empty: Local = Local.Empty\n",
        "let wrapped: Generic = Generic.Wrapped (Some 1)\n",
        "let unit: PublicGroup = PublicGroup.Unit\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("private and compound typegroups should compile");
}

#[test]
fn public_repr_is_allowed_on_singleton_types() {
    type_check("pub(repr) type Unit\nlet unit: Unit = Unit\n");
}

#[test]
fn rejects_empty_typegroups_and_top_level_optional_macro_parameters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "typegroup Empty = {}\n",
            "compile-time match was not exhaustive",
        ),
        (
            "macro invalid: Optional Type -> Expr = value => quote { 0 }\ninvalid I32\n",
            "`Optional` and product syntax shapes may only appear inside delimited contents",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid typegroup infrastructure use should fail");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn public_repr_macro_call_can_generate_a_public_representation() {
    let module = type_check(concat!(
        "macro define_box = vis: MacroCallVisibility => quote { $vis type Box = I32 }\n",
        "pub(repr) define_box\n",
        "let boxed: Box = Box 42\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, Item::TypeDeclaration(declaration)
            if declaration.name == "Box"
                && declaration.visibility == stapler::Visibility::Public
                && declaration.representation_visibility == stapler::Visibility::Public)
    }));
}

#[test]
fn modifiers_compose_after_visibility_aware_item_calls() {
    let module = type_check(concat!(
        "macro @identity: Item -> Item = item => item\n",
        "macro define = vis: MacroCallVisibility => quote { $vis let generated: I32 = 42 }\n",
        "@identity\n",
        "pub define\n",
        "let result: I32 = generated\n",
    ));
    assert!(module.resolved().syntax().items.iter().any(|item| {
        matches!(item, Item::Statement(statement)
            if matches!(statement.as_ref(), Statement::Binding(binding)
                if binding.name == "generated"
                    && binding.visibility == stapler::Visibility::Public))
    }));
}

#[test]
fn diagnoses_invalid_visibility_macro_uses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "macro invalid: Expr -> MacroCallVisibility -> Expr = left => vis => left\n",
            "macro `invalid` may use `MacroCallVisibility` only as its first parameter",
        ),
        (
            "def invalid: MacroCallVisibility -> Visibility = value => value\n",
            "`MacroCallVisibility` may only be the first parameter of a function-style macro",
        ),
        (
            "macro ordinary: Expr -> Expr = value => quote { $value }\npub ordinary 1\n",
            "macro `ordinary` has no overload whose first parameter is `MacroCallVisibility`",
        ),
        (
            "macro invalid = vis: MacroCallVisibility => quote { $vis type alias Generated = I32 }\npub(repr) invalid\n",
            "`PublicRepr` visibility requires a represented distinct type",
        ),
        (
            "macro @identity: Item -> Item = item => item\nmacro expression = vis: MacroCallVisibility => quote { 1 }\n@identity\npub expression\n",
            "modifier macros may only be applied to `let`, `def`, `type`, `extern`, `trait`, or `impl` items",
        ),
        (
            "let visibility = Private\n",
            "`Private` syntax values are compile-time-only",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid visibility syntax should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn implicit_macro_call_visibility_can_make_overloads_ambiguous() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = concat!(
        "macro choose: Expr -> Expr = value => quote { $value }\n",
        "macro choose: MacroCallVisibility -> Expr -> Expr = vis => value => quote { $value }\n",
        "let result = choose 1\n",
    );
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(&with_syntax_imports(source), root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("equal-consumption overloads should be ambiguous");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "ambiguous invocation of macro `choose`")
    );
}

#[test]
fn diagnoses_invalid_modifier_definitions_and_applications() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "macro @invalid: SyntaxNode -> Item -> Item = value => item => item\n",
            "modifier macro `@invalid` must have signature `Item -> Item` or `(Expr | Type | Pattern) -> Item -> Item`",
        ),
        (
            "macro @required: Expr -> Item -> Item = value => item => item\n@required\nlet value = 1\n",
            "modifier macro `@required` requires a parenthesized argument",
        ),
        (
            "macro @plain: Item -> Item = item => item\n@plain(1)\nlet value = 1\n",
            "modifier macro `@plain` does not accept an argument",
        ),
        (
            "macro ordinary = value => quote { $value }\n@ordinary\nlet value = 1\n",
            "macro `ordinary` is function-style and cannot be used as modifier `@ordinary`",
        ),
        (
            "macro @recursive_constructor: Item -> Item = item => item\n",
            "modifier name `@recursive_constructor` is reserved by the compiler",
        ),
        (
            "@recursive_constructor\ntype Box = I32\n",
            "`@recursive_constructor` may only mark a compiler-owned recursive constructor",
        ),
        (
            "@recursive_constructor(1)\ntype Box = I32\n",
            "`@recursive_constructor` does not accept an argument",
        ),
        (
            "@recursive_constructor\nlet value = 1\n",
            "`@recursive_constructor` may only modify a type declaration",
        ),
        (
            "macro @identity: Item -> Item = item => item\n@identity\nuse std.core *\n",
            "modifier macros may only be applied to `let`, `def`, `type`, `extern`, `trait`, or `impl` items",
        ),
        (
            "macro @recurse: Item -> Item = _ => quote { @recurse let value = 1 }\n@recurse\nlet original = 0\n",
            "recursive modifier macro expansion of `@recurse`",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid modifier use should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn accepts_opaque_type_and_pattern_macro_inputs_and_contextual_splices() {
    let module = type_check(concat!(
        "def type_identity: Type -> Type = value => value\n",
        "def pattern_identity: Pattern -> Pattern = value => value\n",
        "macro define_value = ty: Type => value: Expr => {\n",
        "    let actual = type_identity ty\n",
        "    quote { let generated: $actual = $value }\n",
        "}\n",
        "macro destructure = pattern: Pattern => value: Expr => {\n",
        "    let actual = pattern_identity pattern\n",
        "    quote { let $actual = $value }\n",
        "}\n",
        "define_value (I32 -> I32) (value => value)\n",
        "destructure ((left, right)) (40, 2)\n",
        "let answer: I32 = generated (left + right)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("type and pattern splices should generate valid code");
}

#[test]
fn splices_types_and_patterns_through_expression_quotation_contexts() {
    let module = type_check(concat!(
        "macro typed_identity = ty: Type => quote { (value => value) satisfies ($ty -> $ty) }\n",
        "macro parameter_function = pattern: Pattern => quote { $pattern => 42 }\n",
        "macro matching = pattern: Pattern => quote { match (True satisfies Bool) { $pattern => 1, _ => 0 } }\n",
        "let identity: I32 -> I32 = typed_identity I32\n",
        "let constant: I32 -> I32 = parameter_function (_)\n",
        "let matched: I32 = matching True\n",
        "let result: I32 = identity (constant matched)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("contextual type and pattern splices should compile in expressions");
}

#[test]
fn quote_uses_contextual_results_and_reinterprets_opaque_fragments() {
    let module = type_check(concat!(
        "macro delayed: Expr -> Expr = value => {\n",
        "    let fragment: Syntax = quote { $value + 1 }\n",
        "    quote { $fragment } satisfies Expr\n",
        "}\n",
        "macro raw: Expr -> Syntax = value => quote { $value }\n",
        "macro pattern_result: Expr -> Expr = value => {\n",
        "    let pattern: Pattern = quote { Some inner }\n",
        "    quote { match Some $value { $pattern => inner } }\n",
        "}\n",
        "let direct: I32 = raw 1\n",
        "let result: I32 = delayed (pattern_result 40) + direct\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("opaque syntax should reparse according to its later quote context");
}

#[test]
fn opaque_syntax_captures_whole_delimiter_contents_and_is_the_broadest_overload() {
    let module = type_check(concat!(
        "macro capture: Braced Syntax -> Expr = body => match body {\n",
        "    Braced fragment => quote { $fragment } satisfies Expr,\n",
        "}\n",
        "macro choose: Syntax -> Expr = _: Syntax => quote { 1 }\n",
        "macro choose: SyntaxNode -> Expr = _: SyntaxNode => quote { 2 }\n",
        "macro accepts: Braced Syntax -> Expr = _: Braced Syntax => quote { 3 }\n",
        "let captured: I32 = capture { 40 + 2 }\n",
        "let selected: I32 = choose name\n",
        "let arbitrary: I32 = accepts { left, => right }\n",
        "let result: I32 = captured + selected + arbitrary\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("whole-fragment capture and SyntaxNode overload precedence should compile");
}

#[test]
fn syntax_node_quotation_requires_one_shortest_structural_node() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(concat!(
                "macro invalid: Expr -> Expr = _: Expr => {\n",
                "    let node: SyntaxNode = quote { left right }\n",
                "    quote { 0 }\n",
                "}\n",
                "let result = invalid ()\n",
            )),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("two shortest nodes must not satisfy SyntaxNode");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "quotation does not contain exactly one structural syntax node"
    }));
}

#[test]
fn contextual_item_sequence_quotes_flatten_empty_single_and_multiple_results() {
    let module = type_check(concat!(
        "macro empty: Expr -> Sequence Item = _: Expr => quote {}\n",
        "macro single: Expr -> Sequence Item = _: Expr => quote { let one: I32 = 1 }\n",
        "macro multiple: Expr -> Sequence Item = _: Expr => quote {\n",
        "    let two: I32 = 2\n",
        "    let three: I32 = 3\n",
        "}\n",
        "empty ()\n",
        "single ()\n",
        "multiple ()\n",
        "let result: I32 = one + two + three\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("contextual item sequences should flatten in source order");
}

#[test]
fn quote_result_is_sealed_and_generic_user_macros_are_rejected() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let module = resolve("impl QuoteResult I32 {}\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("QuoteResult must remain sealed");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message
            == "`QuoteResult` is compiler-defined and cannot be implemented explicitly"
    }));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("macro generic: T => Expr -> T = value => value\n", root)
        .expect("generic macro syntax should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("generic user macros remain unsupported");
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message == "generic user-defined macros are not supported"));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro invalid: Expr -> Expr = _: Expr => {\n",
                "    let ident: Ident String = quote { name }\n",
                "    quote { 0 }\n",
                "}\n",
                "let result = invalid ()\n",
            ),
            root,
        )
        .expect("unsupported contextual quote source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("narrow syntax nodes are not direct QuoteResult targets");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message == "Ident String does not satisfy `QuoteResult`"
        })
    );
}

#[test]
fn type_and_pattern_macro_inputs_support_atomic_and_compound_forms() {
    let module = type_check(concat!(
        "macro atomic_type = _: Type => quote { let atomic: I32 = 1 }\n",
        "macro applied_type = _: Type => quote { let applied: I32 = 2 }\n",
        "macro nominal_pattern = _: Pattern => quote { let nominal: I32 = 3 }\n",
        "atomic_type I32\n",
        "applied_type (Ref I32)\n",
        "nominal_pattern (Some value)\n",
        "let result: I32 = atomic + applied + nominal\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("atomic and grouped category arguments should compile");
}

#[test]
fn type_and_pattern_overlaps_with_expression_overloads_are_ambiguous() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for source in [
        concat!(
            "macro choose = _: Expr => quote { 1 }\n",
            "macro choose = _: Type => quote { 2 }\n",
            "let result = choose I32\n",
        ),
        concat!(
            "macro choose = _: Type => quote { 1 }\n",
            "macro choose = _: Pattern => quote { 2 }\n",
            "let result = choose (I32)\n",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("overlapping syntax categories should be ambiguous");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == "ambiguous invocation of macro `choose`")
        );
    }
}

#[test]
fn diagnoses_invalid_type_and_pattern_macro_inputs_and_splices() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "macro consume = _: Type => quote { 1 }\nlet result = consume (I32 ->)\n",
            "argument 1 of macro `consume` must be a type",
        ),
        (
            "macro invalid = value: Pattern => quote { let generated: $value = 1 }\ninvalid name\n",
            "type splice `$value` contains pattern syntax",
        ),
        (
            "macro invalid = value: Type => quote { let $value = 1 }\ninvalid I32\n",
            "pattern splice `$value` contains type syntax",
        ),
        (
            "def runtime = value => value\nlet result = runtime (I32 -> I32)\n",
            "grouped type or pattern syntax requires a matching macro parameter",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse losslessly");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid category syntax should fail expansion or resolution");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn type_and_pattern_overloads_are_more_specific_than_syntax() {
    let module = type_check(concat!(
        "macro choose_type = _: SyntaxNode => quote { let type_choice: String = \"syntax\" }\n",
        "macro choose_type = _: Type => quote { let type_choice: I32 = 1 }\n",
        "macro choose_pattern = _: SyntaxNode => quote { let pattern_choice: String = \"syntax\" }\n",
        "macro choose_pattern = _: Pattern => quote { let pattern_choice: I32 = 2 }\n",
        "choose_type I32\n",
        "choose_pattern (Some value)\n",
        "let result: I32 = type_choice + pattern_choice\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("category overloads should beat Syntax");
}

#[test]
fn expands_macros_and_splices_inside_generated_items() {
    let module = type_check(concat!(
        "macro expression_identity = value: Expr => quote { $value }\n",
        "macro define_value = value: Expr => quote {\n",
        "    let generated: I32 = expression_identity $value\n",
        "}\n",
        "define_value 42\n",
        "let result: I32 = generated\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nested macros in generated items should expand");
}

#[test]
fn generates_extern_trait_and_implementation_items() {
    let module = type_check(concat!(
        "macro define_extern = _: Expr => quote {\n",
        "    extern \"c\" { let generated_external: I32 -> I32 }\n",
        "}\n",
        "macro define_trait = _: Expr => quote {\n",
        "    trait GeneratedTrait = T => { transform: T -> T }\n",
        "}\n",
        "macro define_impl = replacement: Expr => quote {\n",
        "    impl GeneratedTrait I32 { def transform = value => $replacement }\n",
        "}\n",
        "define_extern ()\n",
        "define_trait ()\n",
        "define_impl 41\n",
        "let transformed: I32 = GeneratedTrait.transform 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generated resolver-safe declaration items should generate code");
}

#[test]
fn generates_traits_with_functional_dependencies() {
    let module = type_check(concat!(
        "macro define_trait = _: Expr => quote {\n",
        "    trait Generated = Input => Output => Input ~> Output => { generate: Input -> Output }\n",
        "}\n",
        "define_trait ()\n",
        "impl Generated I32 String { def generate = value => \"generated\" }\n",
        "let generated = Generated.generate 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generated functional dependencies should resolve and specialize");
}

#[test]
fn diagnoses_invalid_item_macro_outputs_and_placements() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "macro invalid: Expr -> Item = value => quote { $value }\ninvalid 1\n",
            "quotation cannot be interpreted as Item",
        ),
        (
            "macro emit = value: Expr => quote { let generated = $value }\nlet result = emit 1\n",
            "macro `emit` produces item syntax and may only be invoked as a standalone top-level item",
        ),
        (
            "macro emit: Expr -> Item = value => quote { let generated = $value }\ndef enclosing = () => { emit 1; () }\n",
            "macro `emit` produces item syntax and may only be invoked as a standalone top-level item",
        ),
        (
            "macro emit = value: Expr => quote { let generated = $value }\nemit 1 2\n",
            "item-producing macro `emit` cannot have excess arguments",
        ),
        (
            "macro emit = _: Expr => quote { macro generated = value => quote { $value } }\nemit ()\n",
            "item quotations cannot generate `macro` declarations yet",
        ),
        (
            "macro consume = value: Item => quote { 1 }\nconsume candidate\n",
            "function-style macro `consume` cannot accept `Item`; define a modifier macro with `macro @consume` instead",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&with_syntax_imports(source), root)
            .expect("source should parse");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid item macro use should fail expansion");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "expected `{expected}`, found {diagnostics:#?}",
        );
    }
}

#[test]
fn evaluates_pure_syntax_helpers_and_conditional_macros() {
    let module = type_check(concat!(
        "def syntax_identity: SyntaxNode -> SyntaxNode = value => value\n",
        "macro choose = condition => then => else => quote {\n",
        "    match $condition { True => $then, False => $else, }\n",
        "}\n",
        "macro passthrough = value => syntax_identity value\n",
        "let condition: Bool = True\n",
        "let result: I32 = passthrough (choose condition 1 2)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("expanded macros should generate code");
}

#[test]
fn expands_typed_macros_with_literal_identifier_parameters() {
    let module = type_check(concat!(
        "macro conditional =\n",
        "    condition: Expr =>\n",
        "    then_branch: Expr =>\n",
        "    _: Ident \"else\" =>\n",
        "    else_branch: Expr => quote {\n",
        "        match $condition {\n",
        "            True() => $then_branch,\n",
        "            False() => $else_branch,\n",
        "        }\n",
        "    }\n",
        "let condition: Bool = True\n",
        "let result: I32 = conditional condition 1 else 2\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("typed macro should expand to an ordinary expression");
}

#[test]
fn expands_macros_with_delimited_syntax_parameters() {
    let module = type_check(concat!(
        "macro pair = _: Parenthesized (Ident String, Ident String) => quote { 11 }\n",
        "macro names = _: Bracketed (Sequence Ident String) => quote { 22 }\n",
        "macro body = _: Braced (Sequence SyntaxNode) => quote { 33 }\n",
        "let pair_result: I32 = pair (left right)\n",
        "let empty_names: I32 = names []\n",
        "let names_result: I32 = names [one two three]\n",
        "let body_result: I32 = body { left (right nested) }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("delimited macro parameters should expand");
}

#[test]
fn delimited_macro_overloads_use_structural_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Expr => quote { 1 }\n",
        "macro classify = _: Parenthesized (Sequence SyntaxNode) => quote { 2 }\n",
        "macro classify = _: Parenthesized (Sequence Ident String) => quote { 3 }\n",
        "macro classify = _: Parenthesized (Ident String, Ident String) => quote { 4 }\n",
        "let fixed: I32 = classify (left right)\n",
        "let sequence: I32 = classify (left)\n",
        "let empty: I32 = classify ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("delimiter overloads should be ordered by structural specificity");
}

#[test]
fn constructs_and_destructures_delimited_syntax_values() {
    let module = type_check(concat!(
        "macro inspect = value: Parenthesized (Sequence Ident String) => match value {\n",
        "    Parenthesized (Sequence (Ident \"only\")) => quote { 1 },\n",
        "    _ => quote { 0 },\n",
        "}\n",
        "macro construct = _: Expr => Parenthesized (quote { 7 })\n",
        "macro construct_empty = _: Expr => Parenthesized (Sequence ())\n",
        "macro construct_sequence = _: Expr => Parenthesized (Sequence (quote { increment }, quote { 41 }))\n",
        "macro construct_braced = _: Expr => Braced (Sequence (quote { 9 }))\n",
        "macro preserve = value: Parenthesized (Sequence SyntaxNode) => value\n",
        "def increment: I32 -> I32 = value => value + 1\n",
        "let inspected: I32 = inspect (only)\n",
        "let constructed: I32 = construct ()\n",
        "let empty = construct_empty ()\n",
        "let ordered: I32 = construct_sequence ()\n",
        "let braced: I32 = construct_braced ()\n",
        "let preserved: I32 = preserve ( 44 )\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("delimiter values should support constructors and nominal patterns");
}

#[test]
fn rejects_invalid_sequence_positions_and_source_punctuation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro invalid = value: Sequence Ident => quote { 0 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("bare Sequence should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message
            == "`Sequence` and `Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`"
    }));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro invalid = value: Separated (Ident String) Comma => quote { 0 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("bare Separated should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message
            == "`Sequence` and `Separated` may only be the entire contents of `Parenthesized`, `Bracketed`, or `Braced`"
    }));

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro invalid = value: Braced (Sequence Syntax) => quote { 0 }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("opaque Syntax has no canonical sequence partition");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .starts_with("opaque `Syntax` may be a top-level argument")
    }));
}

#[test]
fn matches_comma_and_separated_delimited_syntax() {
    let module = type_check(concat!(
        "macro fixed = _: Parenthesized (Ident String, Comma, Ident String) => quote { 1 }\n",
        "macro separated = _: Parenthesized (Separated (Ident String) Comma) => quote { 2 }\n",
        "macro bracketed = _: Bracketed (Separated (Ident String) Comma) => quote { 5 }\n",
        "macro braced = _: Braced (Separated (Ident String) Comma) => quote { 6 }\n",
        "macro syntax = value: Parenthesized (Sequence SyntaxNode) => match value {\n",
        "    Parenthesized (Sequence (Ident \"left\", Comma, Ident \"right\")) => quote { 3 },\n",
        "    _ => quote { 0 },\n",
        "}\n",
        "macro comma = value: Parenthesized (Comma) => match value {\n",
        "    Parenthesized Comma => quote { 4 },\n",
        "}\n",
        "let fixed_result: I32 = fixed (left, right)\n",
        "let empty: I32 = separated ()\n",
        "let single: I32 = separated (one)\n",
        "let multiple: I32 = separated (one, two, three)\n",
        "let trailing: I32 = separated (one, two,)\n",
        "let structural: I32 = syntax (left, right)\n",
        "let comma_result: I32 = comma (,)\n",
        "let bracketed_result: I32 = bracketed [left, right,]\n",
        "let braced_result: I32 = braced {left, right}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("comma and separated syntax should match");
}

#[test]
fn separated_overloads_use_structural_specificity() {
    let module = type_check(concat!(
        "macro classify = _: Parenthesized (Sequence SyntaxNode) => quote { 1 }\n",
        "macro classify = _: Parenthesized (Separated (Ident String) Comma) => quote { 2 }\n",
        "macro classify = _: Parenthesized (Ident String, Comma, Ident String) => quote { 3 }\n",
        "let fixed: I32 = classify (one, two)\n",
        "let separated: I32 = classify (one, two, three)\n",
        "let empty: I32 = classify ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("fixed, separated, and ordinary sequence overloads should be ordered");
}

#[test]
fn constructs_and_destructures_separated_syntax() {
    let module = type_check(concat!(
        "macro inspect = value: Parenthesized (Separated (Ident String) Comma) => match value {\n",
        "    Parenthesized (Separated (separator: Comma, elements: (Ident \"one\", Ident \"two\"), trailing: True())) => quote { 1 },\n",
        "    _ => quote { 0 },\n",
        "}\n",
        "macro construct = _: Expr => Parenthesized (Separated (\n",
        "    separator: Comma,\n",
        "    elements: (quote { 10 }, quote { 20 }),\n",
        "    trailing: True,\n",
        "))\n",
        "let inspected: I32 = inspect (one, two,)\n",
        "let constructed: (I32, I32) = construct ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("separated syntax should support construction and patterns");
}

#[test]
fn rejects_malformed_separated_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for argument in ["(,)", "(, one)", "(one,, two)", "(one two)", "(one, 2)"] {
        let source = format!(
            "macro names = _: Parenthesized (Separated (Ident String) Comma) => quote {{ 0 }}\nlet value: I32 = names {argument}\n"
        );
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(&source, root)
            .expect("source should parse");
        NameResolver::new()
            .resolve_program(program)
            .expect_err("malformed separated syntax should be rejected");
    }

    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro invalid = _: Expr => Parenthesized (Separated (\n",
                "    separator: Comma, elements: (), trailing: True,\n",
                "))\n",
                "let value = invalid ()\n",
            ),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("an empty separated value cannot have a trailing comma");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "an empty `Separated` value cannot have a trailing separator"
    }));
}

#[test]
fn expands_standard_braced_if_clauses() {
    let module = type_check(concat!(
        "let condition: Bool = True\n",
        "let clauses: I32 = if {\n",
        "  False => 7,\n",
        "  condition => 8,\n",
        "  else => 9,\n",
        "}\n",
        "let clauses_without_else = if { False => () }\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("standard braced if should expand and generate code");
}

#[test]
fn requires_else_to_be_the_last_braced_if_clause() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "let invalid = if { else => (), True => () }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("a non-final else clause should be rejected");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "compile-time match was not exhaustive"
    }));
}

#[test]
fn expands_standard_while_with_loop_control() {
    let module = type_check(concat!(
        "def run = () => {\n",
        "  let mut keep_going: Bool = True\n",
        "  while keep_going { keep_going = False; continue }\n",
        "}\n",
        "run ()\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("standard while should expand and generate valid LLVM");
    assert!(llvm.contains("loop.body"));
    assert!(llvm.contains("loop.exit"));
}

#[test]
fn macro_overloads_choose_longest_then_most_specific() {
    let module = type_check(concat!(
        "macro select = value: Expr => quote { 10 }\n",
        "macro select = value: Expr => _: Ident \"with\" => replacement: Expr => quote { $replacement }\n",
        "macro classify = value: SyntaxNode => quote { 1 }\n",
        "macro classify = value: Expr => quote { 2 }\n",
        "macro classify = value: Ident String => quote { 3 }\n",
        "macro classify = _: Ident \"else\" => quote { 4 }\n",
        "let longest: I32 = select 0 with 20\n",
        "let specific: I32 = classify else\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("selected overloads should generate code");
}

#[test]
fn rejects_legacy_standard_if_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "let condition: Bool = True\nlet invalid = if condition 1 else 2\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("legacy if syntax should be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("argument 1 of macro `if` must be `Braced"))
    );
}

#[test]
fn diagnoses_duplicate_and_ambiguous_macro_overloads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let duplicate = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro same = value: Expr => quote { 1 }\n",
                "macro same = other: Expr => quote { 2 }\n",
            ),
            root,
        )
        .expect("duplicate overload source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(duplicate)
        .expect_err("identical overload patterns should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate macro overload `same: Expr`")
    }));

    let ambiguous = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro crossed = left: Ident String => right: Expr => quote { 1 }\n",
                "macro crossed = left: Expr => right: Ident String => quote { 2 }\n",
                "crossed first second\n",
            ),
            root,
        )
        .expect("ambiguous overload source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(ambiguous)
        .expect_err("incomparable overloads should be ambiguous");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "ambiguous invocation of macro `crossed`" })
    );
}

#[test]
fn rejects_a_mismatched_literal_identifier_macro_argument() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "macro conditional = value: Expr => _: Ident \"else\" => quote { $value }\n",
                "conditional 1 otherwise\n",
            ),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("literal identifier mismatch should fail expansion");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("argument 2 of macro `conditional` must be identifier `else`")
    }));
}

#[test]
fn rejects_bare_literal_identifier_macro_parameters() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro conditional = value: Expr => Ident \"else\" => quote { $value }\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("bare literal identifier parameters should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("parameters must match atomic syntax values")
    }));
}

#[test]
fn string_type_is_satisfied_by_strings_and_literal_types() {
    type_check(concat!(
        "def preserve: T => StringType T => T -> T = value => value\n",
        "let literal = preserve \"hello\"\n",
        "let text: String = \"hello\"\n",
        "let copied: String = preserve text\n",
    ));
}

#[test]
fn ident_rejects_non_string_spelling_types() {
    let module = resolve(concat!(
        "type alias InvalidIdent = Ident I32\n",
        "let invalid: InvalidIdent\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("Ident's spelling argument must satisfy StringType");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message == "trait bound is not satisfied for `I32`" })
    );
}

#[test]
fn rejects_explicit_string_type_implementations() {
    let module = resolve("impl StringType I32 {}\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("StringType must remain sealed");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message
            == "`StringType` is compiler-defined and cannot be implemented explicitly"
    }));
}

#[test]
fn evaluates_pure_compile_time_control_flow_and_arithmetic() {
    let module = type_check(concat!(
        "macro computed = unused => match 1 + 1 == 2 {\n",
        "    True() => quote { 42 },\n",
        "    False() => quote { 0 },\n",
        "}\n",
        "let result: I32 = computed ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("pure compile-time arithmetic should select syntax");
}

#[test]
fn composes_nested_macros_and_applies_excess_arguments() {
    let module = type_check(concat!(
        "macro inner = value => quote { $value }\n",
        "macro outer = value => quote { inner $value }\n",
        "macro identity = value => quote { $value }\n",
        "def increment: I32 -> I32 = value => value + 1\n",
        "let nested: I32 = outer 41\n",
        "let applied: I32 = identity increment 41\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nested and excess-argument macro calls should compile");
}

#[test]
fn diagnoses_incomplete_and_non_syntax_macros() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let incomplete = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            "macro pair = left => right => quote { ($left, $right) }\npair 1\n",
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(incomplete)
        .expect_err("incomplete macro call should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("requires 2 arguments"))
    );

    let invalid = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("macro invalid = value => 42\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(invalid)
        .expect_err("non-Syntax macro result should fail without invocation");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "macro `invalid` must return `Syntax`")
    );
}

#[test]
fn rejects_runtime_syntax_values() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let generated: Syntax\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("Syntax should not reach runtime checking");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "`Syntax` values are compile-time-only")
    );
}

#[test]
fn diagnoses_recursive_macro_expansion() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports(
                "macro recursive = value => quote { recursive $value }\nrecursive 1\n",
            ),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("recursive expansion should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "recursive macro expansion of `recursive`")
    );
}

#[test]
fn rejects_repeated_splices_with_a_specific_diagnostic() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            &with_syntax_imports("macro many = values => quote { $values... }\nmany 1\n"),
            root,
        )
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("repeated splice should be reserved");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "repeated splices are not supported yet")
    );
}

#[test]
fn type_checks_function_declarations_without_values() {
    type_check("let add: (x: I32, y: I32) -> I32\n");
}

#[test]
fn reports_unknown_names_without_panicking() {
    let syntax = parse("missing\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("unknown name should fail resolution");

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "unknown name `missing`");
}

#[test]
fn reports_duplicate_definitions() {
    let syntax = parse("let value = 1\nlet value = 2\n").expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("duplicate name should fail resolution");

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "duplicate definition of `value`");
}

#[test]
fn generates_a_named_function_after_predeclaring_it() {
    let module = type_check("let first = (a: I32, b: I32) => a\nfirst (1, 2)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("module should compile");

    assert!(llvm.contains("define i32 @first(ptr %0, i32 %a, i32 %b)"));
    assert!(llvm.contains("closure.call"));
}

#[test]
fn predeclares_functions_for_recursion() {
    let module = type_check("def recurse: (n: I32) -> I32 = (n: I32) => recurse (n)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("recursive function should compile");

    assert!(llvm.contains("binding.uninitialized"));
    assert!(llvm.contains("closure.call"));
}

#[test]
fn preserves_the_environment_for_recursive_closures() {
    let module = type_check(concat!(
        "def outer = value: I32 => {\n",
        "  def recurse: I32 -> I32 = n => {\n",
        "    let captured = value\n",
        "    recurse n\n",
        "  }\n",
        "  recurse 1\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("recursive closure should compile");
    assert!(llvm.contains("binding.cell"));
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("load { i32 }"));
}

#[test]
fn lowers_captured_locals_into_closure_environments() {
    let module = type_check(concat!(
        "let outer = (value: I32) => {\n",
        "  let inner = () => value\n",
        "  inner ()\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("captured locals should use closure lowering");

    assert!(llvm.contains("@malloc"));
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("load { i32 }"));
}

#[test]
fn type_checks_and_lowers_managed_refs() {
    let module = type_check(concat!(
        "let point: Ref (x: I32, y: I32) = Ref (x: 1, y: 2)\n",
        "let copy = point\n",
        "let x: I32 = copy.x\n",
        "let Ref (captured_x, captured_y) = point\n",
        "def sum_ref: (Ref (x: I32, y: I32)) -> I32 = value => match value {\n",
        "  Ref (x, y) => x + y,\n",
        "}\n",
        "let nested: Ref (Ref I32) = Ref (Ref 9)\n",
        "let empty: Ref () = Ref ()\n",
        "let total: I32 = captured_x + captured_y\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("Ref values should generate valid LLVM");
    assert!(llvm.contains("call ptr @__staple_gc_alloc"));
    assert!(llvm.contains("ref.payload"));
    assert!(llvm.contains("@__staple_gc_collect"));
    assert!(llvm.contains("call void @__staple_gc_register_root"));
}

#[test]
fn preserves_literal_nominal_ref_container_semantics() {
    let module = type_check(concat!(
        "type RefPoint = Ref (x: I32, y: I32)\n",
        "let point: RefPoint = RefPoint (Ref (x: 3, y: 4))\n",
        "let x: I32 = point.x\n",
        "let RefPoint (Ref (captured_x, captured_y)) = point\n",
        "let y: I32 = captured_y\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nominal Ref containers should compile");
}

#[test]
fn rejects_incomplete_and_overapplied_ref_types() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let value: Ref\n"))
        .expect_err("Ref requires a payload type");
    assert!(!diagnostics.is_empty());

    let diagnostics = TypeChecker::new()
        .check(resolve("let value: Ref I32 I32\n"))
        .expect_err("Ref accepts exactly one payload type");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("does not accept compile-time arguments")
    }));
}

#[test]
fn type_checks_and_generates_curried_functions() {
    let module = type_check(concat!(
        "def inferred = a: I32 => b: I32 => a\n",
        "def annotated: I32 -> I32 -> I32 = a => b => a\n",
        "def add_one = annotated 1\n",
        "inferred 1 2\n",
        "add_one 2\n",
    ));

    for name in ["inferred", "annotated"] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .expect("curried function should resolve");
        assert_eq!(
            module.type_of_function(function.id).expect("checked type"),
            &stapler::CheckedFunctionType {
                parameter: Box::new(CheckedType::I32),
                resources: stapler::CheckedResourceSet::default(),
                result: Box::new(CheckedType::Function(stapler::CheckedFunctionType {
                    parameter: Box::new(CheckedType::I32),
                    resources: stapler::CheckedResourceSet::default(),
                    result: Box::new(CheckedType::I32),
                })),
            },
        );
    }

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("curried functions should compile");
    assert!(llvm.contains("closure.call"));
    assert!(llvm.contains("@malloc"));
}

#[test]
fn contextually_types_product_parameters() {
    type_check("def first: (I32, I32) -> I32 = (a, b) => a\nfirst (1, 2)\n");
}

#[test]
fn rejects_untyped_parameters_without_a_function_annotation() {
    let module = resolve("def identity = value => value\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("an untyped parameter needs context");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("could not fully infer"))
    );
}

#[test]
fn supports_typed_wildcard_parameters() {
    type_check("def demo = _: String => ()\ndemo \"unused\"\n");
}

#[test]
fn lowers_transitive_captures_across_curried_layers() {
    let module = type_check(concat!(
        "def sum = a: I32 => b: I32 => c: I32 => (a, b, c)\n",
        "def add_one = sum 1\n",
        "def add_one_two = add_one 2\n",
        "add_one_two 3\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("transitive captures should compile");
    assert!(llvm.matches("@malloc").count() >= 3);
    assert!(llvm.contains("load { i32 }"));
    assert!(llvm.contains("load { i32, i32 }"));
}

#[test]
fn adapts_non_variadic_externs_used_as_function_values() {
    let module = type_check(concat!(
        "use std.cinterop *\n",
        "extern \"c\" { let puts: (CPointer CChar) -> I32 }\n",
        "def apply: ((CPointer CChar) -> I32, CPointer CChar) -> I32 = (f, value) => f value\n",
        "apply (puts, c_string \"hello\")\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("external function adapter should compile");
    assert!(llvm.contains("define internal i32 @__staple_extern_puts"));
    assert!(llvm.contains("call i32 @puts"));
}

#[test]
fn calls_user_defined_functions_with_symbolic_and_backtick_infix_syntax() {
    let module = type_check(concat!(
        "def infixl 6 +: I32 -> I32 -> I32 = x => y => x\n",
        "def infixl 7 combine: I32 -> I32 -> I32 = x => y => y\n",
        "def plus = (+)\n",
        "1 + 2\n",
        "1 `combine` 2\n",
        "(+) 1 2\n",
        "plus 1 2\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("infix calls should compile as curried calls");
    assert!(llvm.contains("@operator.2b"));
    assert!(llvm.contains("closure.call"));
}

#[test]
fn rejects_incompatible_fixity_chains() {
    let syntax = parse(concat!(
        "def infix 4 ==: I32 -> I32 -> I32 = x => y => x\n",
        "1 == 2 == 3\n",
    ))
    .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("non-associative chaining should fail");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("incompatible associativity"))
    );
}

#[test]
fn associates_infix_chains_using_inline_fixity() {
    let source = concat!(
        "def infixl 6 +: I32 -> I32 -> I32 = x => y => x\n",
        "def infixr 7 **: I32 -> I32 -> I32 = x => y => y\n",
        "def choose: I32 -> I32 -> I32 = x => y => x\n",
        "1 + 2 + 3\n",
        "1 ** 2 ** 3\n",
        "1 + 2 ** 3\n",
        "1 `choose` 2 `choose` 3\n",
    );
    let module = resolve(source);
    let lowered = module
        .syntax()
        .items
        .iter()
        .skip(3)
        .map(|item| {
            let Item::Statement(statement) = item else {
                panic!("expected expression item");
            };
            let Statement::Expression(stapler::Expression::Infix(infix)) = statement.as_ref()
            else {
                panic!("expected infix source expression");
            };
            module
                .lowered_infix(infix.syntax.id)
                .expect("lowered infix")
        })
        .collect::<Vec<_>>();

    let (operator, left, _) = infix_call(lowered[0]);
    assert_eq!(operator, "+");
    assert_eq!(infix_call(left).0, "+");

    let (operator, _, right) = infix_call(lowered[1]);
    assert_eq!(operator, "**");
    assert_eq!(infix_call(right).0, "**");

    let (operator, _, right) = infix_call(lowered[2]);
    assert_eq!(operator, "+");
    assert_eq!(infix_call(right).0, "**");

    let (operator, left, _) = infix_call(lowered[3]);
    assert_eq!(operator, "choose");
    assert_eq!(infix_call(left).0, "choose");
}

#[test]
fn decodes_source_string_literals_before_llvm_generation() {
    let source = concat!(
        "use std.cinterop *\n",
        "extern \"c\" { let puts: (CPointer CChar) -> I32 }\n",
        "puts (c_string \"hello\\n\")\n",
    );
    let module = type_check(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("module should compile");

    assert!(llvm.contains("c\"hello\\0A\\00\""));
    assert!(!llvm.contains("\\22hello"));
}

#[test]
fn type_checks_generic_aliases_and_functions() {
    let module = type_check(concat!(
        "type alias Pair = (A, B) => (A, B)\n",
        "def identity: T => T -> T = x => x\n",
        "let pair: Pair (String, I32) = (\"answer\", 42)\n",
        "let answer: I32 = identity 42\n",
        "let text: String = identity \"hello\"\n",
    ));
    assert!(
        module
            .functions()
            .iter()
            .any(|function| function.name == "identity")
    );
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic functions should be monomorphized");
    assert!(llvm.matches("identity__").count() >= 2);
}

#[test]
fn type_checks_static_traits_and_bounded_generic_functions() {
    let module = type_check(concat!(
        "trait Increment = T => { increment: T -> T }\n",
        "trait Echo = T => { echo: T -> T }\n",
        "trait Swap = T => { swap: T -> T }\n",
        "impl Increment I32 { def increment = value => value + 1 }\n",
        "impl Echo I32 { def echo = value => value }\n",
        "impl Swap (I32, I32) { def swap = (left, right) => (right, left) }\n",
        "def increment_twice: T => Increment T => T -> T = value => increment (increment value)\n",
        "def increment_echo: T => Increment T => Echo T => T -> T = value => echo (increment value)\n",
        "let direct: I32 = Increment.increment 40\n",
        "let answer: I32 = increment_twice direct\n",
        "let bounded: I32 = increment_echo answer\n",
        "let first_class: I32 -> I32 = Increment.increment\n",
        "let other: I32 = first_class bounded\n",
        "let swapped: (I32, I32) = Swap.swap (1, 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("static trait calls should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn provides_to_string_for_prelude_scalar_types() {
    let module = type_check(concat!(
        "def render: T => ToString T => T -> String = value => to_string value\n",
        "let a = render (1 satisfies I8)\nlet b = render (1 satisfies I16)\n",
        "let c = render (1 satisfies I32)\nlet d = render (1 satisfies I64)\n",
        "let e = render (1 satisfies U8)\nlet f = render (1 satisfies U16)\n",
        "let g = render (1 satisfies U32)\nlet h = render (1 satisfies U64)\n",
        "let i = render (1 satisfies ISize)\nlet j = render (1 satisfies USize)\n",
        "let k = render (1.5 satisfies F32)\nlet l = render (1.5 satisfies F64)\n",
        "let boolean: Bool = True\nlet m = render boolean\n",
        "let string: String = \"text\"\nlet n = render string\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("ToString implementations should generate LLVM");
}

#[test]
fn uses_generic_default_trait_members_and_concrete_overrides() {
    let module = type_check(concat!(
        "trait Increment = T => {\n",
        "  increment: T -> T\n",
        "  twice: T -> T = value => increment (increment value)\n",
        "}\n",
        "impl Increment I32 { def increment = value => value + 1 }\n",
        "let direct: I32 = Increment.twice 40\n",
        "let first_class: I32 -> I32 = Increment.twice\n",
        "let answer: I32 = first_class direct\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("default trait members should specialize and compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn default_trait_members_use_prerequisites_multiple_arguments_and_macros() {
    let module = type_check(concat!(
        "trait Same = T => Eq T => { same: (T, T) -> Bool = (left, right) => Eq.equal left right }\n",
        "trait Select = Value => { select: (Bool, Value, Value) -> Value = (condition, left, right) => if { condition => left, else => right } }\n",
        "trait First = (Left, Right) => { first: (Left, Right) -> Left = (left, right) => left }\n",
        "impl Same I32 {}\n",
        "impl Select I32 {}\n",
        "impl First (I32, String) {}\n",
        "let condition: Bool = Same.same (42, 42)\n",
        "let selected: I32 = Select.select (condition, 1, 2)\n",
        "let first: I32 = First.first (selected, \"ignored\")\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("prerequisite and macro-using defaults should compile");
}

#[test]
fn explicit_trait_members_override_defaults() {
    let module = type_check(concat!(
        "trait Identity = T => { identity: T -> T = value => value }\n",
        "impl Identity I32 { def identity = value => value + 1 }\n",
        "let answer: I32 = Identity.identity 41\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("explicit overrides should compile");
}

#[test]
fn specializes_recursive_default_trait_members() {
    let module = type_check(concat!(
        "trait Recursive = T => { recurse: T -> T = value => Recursive.recurse value }\n",
        "impl Recursive I32 {}\n",
        "let result: I32 = Recursive.recurse 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("recursive defaults should reuse their specialization");
}

#[test]
fn rejects_invalid_default_trait_member_bodies() {
    let diagnostics = TypeChecker::new()
        .check(resolve(
            "trait Invalid = T => { identity: T -> T = value => \"wrong\" }\n",
        ))
        .expect_err("default bodies must match their member type");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("expected `T`, found `String`") })
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("trait Invalid = T => { identity: T -> T = 42 }\n", root)
        .expect("invalid default source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("default bodies must be function values");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("default trait member implementations must be function values")
    }));
}

#[test]
fn type_checks_product_and_curried_multi_parameter_traits() {
    let module = type_check(concat!(
        "trait Add = Left => Right => Output => { add: (Left, Right) -> Output }\n",
        "trait Convert = (From, To) => { convert: From -> To }\n",
        "impl Add I32 I32 I32 { def add = (left, right) => left + right }\n",
        "impl Convert (I32, String) { def convert = value => \"converted\" }\n",
        "def combine: (L, R, O) => Add L R O => (L, R) -> O = pair => Add.add pair\n",
        "let total: I32 = combine (20, 22)\n",
        "let converted: String = Convert.convert total\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("multi-parameter trait calls should compile");
    assert!(llvm.contains("trait.call"));
}

#[test]
fn infers_trait_functional_dependency_arguments() {
    let module = type_check(concat!(
        "trait Iterator = Iter => Item => Iter ~> Item => { next: Iter -> Item }\n",
        "impl Iterator I32 String { def next = value => \"next\" }\n",
        "trait AddTo = Left => Right => Output => {Left, Right} ~> Output => { add_to: Left -> Right -> Output }\n",
        "impl AddTo I32 I32 I32 { def add_to = left => right => left + right }\n",
        "trait Chain = A => B => C => A ~> B => B ~> C => { chained: A -> (B, C) }\n",
        "impl Chain I32 String U8 { def chained = value => (\"chain\", 7) }\n",
        "trait ConvertPair = (From, To) => From ~> To => { convert_pair: From -> To }\n",
        "impl ConvertPair (I32, String) { def convert_pair = value => \"pair\" }\n",
        "def requires_iterator: T => Iterator T => T -> () = value => ()\n",
        "def requires_iterator_explicit: T => Iterator T _ => T -> () = value => ()\n",
        "def requires_add: T => AddTo T T => T -> T = value => value\n",
        "def requires_pair: T => ConvertPair (T, _) => T -> () = value => ()\n",
        "trait UsesIterator = Iter => Iterator Iter => { use_iterator: Iter -> Iter }\n",
        "impl UsesIterator I32 { def use_iterator = value => value }\n",
        "let next_value = Iterator.next 1\n",
        "let next_string: String = next_value\n",
        "let sum: I32 = AddTo.add_to 20 22\n",
        "let chained = Chain.chained 1\n",
        "let _: () = requires_iterator 1\n",
        "let _: () = requires_iterator_explicit 1\n",
        "let _: I32 = requires_add 1\n",
        "let _: () = requires_pair 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("functional dependency dispatch should compile");
}

#[test]
fn rejects_invalid_functional_dependency_uses_and_conflicting_impls() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Convert = From => To => From ~> To => { convert: From -> To }\n",
            "impl Convert I32 String { def convert = value => \"one\" }\n",
            "impl Convert I32 I32 { def convert = value => value }\n",
        )))
        .expect_err("functional dependencies must make implementations coherent");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("violate a functional dependency")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait AddTo = Left => Right => Output => {Left, Right} ~> Output => { add_to: Left -> Right -> Output }\n",
            "def invalid: T => AddTo T _ T => T -> T = value => value\n",
        )))
        .expect_err("non-dependent arguments cannot be inferred");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot be inferred from functional dependencies")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Convert = From => To => From ~> To => { convert: From -> To }\n",
            "impl Convert I32 { def convert = value => value }\n",
        )))
        .expect_err("implementation headers remain exact-arity");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expects 2 compile-time arguments, found 1")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Iterator = Iter => Item => Iter ~> Item => { next: Iter -> Item }\n",
            "def invalid: (Iter, Item) => Iterator Iter Item => Iterator Iter String => Iter -> Iter = value => value\n",
        )))
        .expect_err("active bounds must respect functional dependencies");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("trait bounds conflict with a functional dependency")
    }));
}

#[test]
fn rejects_invalid_functional_dependency_declarations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, expected) in [
        (
            "trait Invalid = A => B => Missing ~> B => { convert: A -> B }\n",
            "unknown trait type parameter `Missing` in functional dependency",
        ),
        (
            "trait Invalid = A => B => {A, A} ~> B => { convert: A -> B }\n",
            "duplicate functional dependency determinant `A`",
        ),
        (
            "trait Invalid = A => A ~> A => { convert: A -> A }\n",
            "functional dependency cannot determine one of its determinants",
        ),
    ] {
        let program = ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .load_source(source, root)
            .expect("invalid dependency source should load");
        let diagnostics = NameResolver::new()
            .resolve_program(program)
            .expect_err("invalid functional dependency must not resolve");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected))
        );
    }
}

#[test]
fn preserves_applied_types_as_unary_trait_arguments() {
    let module = type_check(concat!(
        "type Box = T => (value: T)\n",
        "trait Echo = T => { echo: T -> T }\n",
        "impl Echo Box I32 { def echo = value => value }\n",
        "def echo_box: T => Echo Box T => (Box T) -> Box T = value => Echo.echo value\n",
        "let boxed: Box I32 = Box 42\n",
        "let echoed: Box I32 = echo_box boxed\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("legacy unary applied trait arguments should compile");
}

#[test]
fn enforces_and_propagates_transitive_trait_prerequisites() {
    let module = type_check(concat!(
        "trait Base = T => { base: T -> T }\n",
        "trait Middle = T => Base T => { middle: T -> T }\n",
        "trait Derived = T => Middle T => { derived: T -> T }\n",
        "impl Derived I32 { def derived = value => value }\n",
        "impl Middle I32 { def middle = value => value }\n",
        "impl Base I32 { def base = value => value }\n",
        "def apply: T => Derived T => T -> T = value => Base.base (Middle.middle (Derived.derived value))\n",
        "let answer: I32 = apply 42\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("transitive prerequisite dispatch should compile");
    assert!(llvm.contains("trait.call"));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Base = T => { base: T -> T }\n",
            "trait Derived = T => Base T => { derived: T -> T }\n",
            "impl Derived I32 { def derived = value => value }\n",
        )))
        .expect_err("implementations must satisfy trait prerequisites");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("trait prerequisite `Base I32` is not satisfied")
    }));
}

#[test]
fn prerequisite_copy_bounds_are_visible_to_ownership_checking() {
    type_check(concat!(
        "trait Duplicate = T => Copy T => { duplicate: T -> T }\n",
        "impl Duplicate I32 { def duplicate = value => value }\n",
        "def pair: T => Duplicate T => T -> (T, T) = value => (value, value)\n",
        "let values: (I32, I32) = pair 42\n",
    ));
}

#[test]
fn substitutes_product_parameters_into_multiple_prerequisites() {
    type_check(concat!(
        "trait BothEqual = (Left, Right) => Eq Left => Eq Right => { equal: (Left, Left, Right, Right) -> (Bool, Bool) }\n",
        "impl BothEqual (I32, I32) { def equal = (left_a, left_b, right_a, right_b) => (Eq.equal left_a left_b, Eq.equal right_a right_b) }\n",
        "def compare_both: (Left, Right) => BothEqual (Left, Right) => (Left, Left, Right, Right) -> (Bool, Bool) = (left_a, left_b, right_a, right_b) => (Eq.equal left_a left_b, Eq.equal right_a right_b)\n",
        "let result: (Bool, Bool) = compare_both (1, 1, 2, 2)\n",
    ));
}

#[test]
fn rejects_cyclic_trait_prerequisites() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(
            concat!(
                "trait First = T => Second T => { first: T -> T }\n",
                "trait Second = T => First T => { second: T -> T }\n",
            ),
            root,
        )
        .expect("cyclic prerequisite source should load");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("prerequisite cycles must be rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("cyclic trait prerequisite"))
    );
}

#[test]
fn rejects_invalid_multi_parameter_trait_uses_and_members() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Add = Left => Right => Output => { add: (Left, Right) -> Output }\n",
            "impl Add I32 I32 { def add = pair => 0 }\n",
        )))
        .expect_err("trait implementation arity must match the declaration");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expects 3 compile-time arguments, found 2")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(
            "trait Invalid = Left => Right => { keep_left: Left -> Left }\n",
        ))
        .expect_err("every member must mention every trait parameter");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("must mention trait parameter `Right`")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Convert = (From, To) => { convert: From -> To }\n",
            "impl Convert I32 { def convert = value => value }\n",
        )))
        .expect_err("product binders require product arguments");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("compile-time product parameter requires a product type argument")
    }));
}

#[test]
fn rejects_invalid_traits_implementations_and_unpropagated_bounds() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment = T => { increment: T -> T }\n",
            "impl Increment I32 { }\n",
        )))
        .expect_err("implementations must be complete");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("missing member `increment`") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment = T => { increment: T -> T }\n",
            "type alias Number = I32\n",
            "impl Increment I32 { def increment = value => value }\n",
            "impl Increment Number { def increment = value => value }\n",
        )))
        .expect_err("aliases may not create overlapping implementations");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("duplicate trait implementation")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment = T => { increment: T -> T }\n",
            "impl Increment I32 { def increment = value => \"wrong\" }\n",
        )))
        .expect_err("implementation bodies must match their trait member types");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected `I32`, found `String`")),
        "{diagnostics:?}"
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment = T => { increment: T -> T }\n",
            "impl Increment I32 { def increment = value => value }\n",
            "def invalid: T => T -> T = value => increment value\n",
        )))
        .expect_err("generic callers must propagate trait bounds");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("no trait implementation or matching bound")
    }));
}

#[test]
fn rejects_invalid_trait_member_signatures_and_non_concrete_targets() {
    let diagnostics = TypeChecker::new()
        .check(resolve("trait Invalid = T => { value: I32 }\n"))
        .expect_err("trait members must be functions that mention the parameter");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("function types"))
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("must mention trait parameter"))
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "trait Increment = T => { increment: T -> T }\n",
            "impl Increment _ { def increment = value => value }\n",
        )))
        .expect_err("implementation targets must be concrete");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("target must be fully concrete") })
    );
}

#[test]
fn requires_qualification_for_ambiguous_trait_methods() {
    let source = concat!(
        "trait Left = T => { convert: T -> T }\n",
        "trait Right = T => { convert: T -> T }\n",
        "impl Left I32 { def convert = value => value }\n",
        "impl Right I32 { def convert = value => value }\n",
        "let left: I32 = Left.convert 1\n",
        "let right: I32 = Right.convert 2\n",
        "let ambiguous: I32 = convert 3\n",
    );
    let diagnostics = TypeChecker::new()
        .check(resolve(source))
        .expect_err("unqualified overlapping method names must be ambiguous");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("ambiguous trait method; qualify the trait name")
    }));
}

#[test]
fn constructs_distinct_and_generic_distinct_values() {
    let module = type_check(concat!(
        "type UserId = I32\n",
        "type Box = T => (value: T)\n",
        "let user: UserId = UserId 42\n",
        "let boxed: Box I32 = Box (value: 42)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("constructors should compile as zero-cost conversions");
}

#[test]
fn constructs_and_propagates_singleton_nominal_values() {
    let module = type_check(concat!(
        "pub type Foo\n",
        "pub type Bar\n",
        "let foo: Foo = Foo\n",
        "let Foo() = foo\n",
        "def identity: Foo -> Foo = value => value\n",
        "let choice: Foo | Bar = Foo\n",
        "def select = (value: Foo | Bar) => { let Foo()? = value; Foo }\n",
        "identity(foo)\n",
        "select(choice)\n",
    ));
    let select = module
        .functions()
        .iter()
        .find(|function| function.name == "select")
        .expect("select function");
    assert!(matches!(
        module
            .type_of_function(select.id)
            .expect("select type")
            .result
            .as_ref(),
        CheckedType::Sum(sum) if sum.alternatives.len() == 2
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("singleton values should generate valid LLVM");
}

#[test]
fn opaque_types_do_not_introduce_values_and_singletons_are_not_callable() {
    let diagnostics = NameResolver::new()
        .resolve(&parse("pub type Secret = opaque\nSecret\n").expect("source should parse"))
        .expect_err("opaque type should not introduce a value");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "unknown name `Secret`")
    );

    let diagnostics = TypeChecker::new()
        .check(resolve("pub type Foo\nFoo()\n"))
        .expect_err("singleton value should not be callable");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("cannot call a value of type `Foo`")
    }));
}

#[test]
fn contextually_specializes_first_class_generic_functions() {
    let module = type_check(concat!(
        "def identity: T => T -> T = x => x\n",
        "def apply: (I32 -> I32) -> I32 = f => f 42\n",
        "let int_identity: I32 -> I32 = identity\n",
        "let answer: I32 = apply identity\n",
        "let other: I32 = int_identity 7\n",
        "def apply_pair: (I32 -> I32, I32) -> I32 = (f, x) => f x\n",
        "let paired: I32 = apply_pair (identity, 9)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("contextually specialized generic functions should compile");
}

#[test]
fn contextually_specializes_first_class_constructors() {
    let module = type_check(concat!(
        "type UserId = I32\n",
        "type Box = T => (value: T)\n",
        "let make_user: I32 -> UserId = UserId\n",
        "let make_box: (value: I32) -> Box I32 = Box\n",
        "let user: UserId = make_user 42\n",
        "let boxed: Box I32 = make_box (value: 42)\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("first-class constructors should compile");
}

#[test]
fn destructures_nominal_values_in_lets_and_functions() {
    let module = type_check(concat!(
        "type UserId = I32\n",
        "type PairIds = (UserId, UserId)\n",
        "let user: UserId = UserId 42\n",
        "let UserId raw = user\n",
        "let pair: PairIds = PairIds (UserId 1, UserId 2)\n",
        "let PairIds (UserId first, UserId second) = pair\n",
        "def unwrap: UserId -> I32 = UserId id => id\n",
        "def get_raw: () -> I32 = () => raw\n",
        "let answer: I32 = unwrap user\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nominal destructuring should be zero-cost");
}

#[test]
fn destructures_contextually_typed_generic_nominal_patterns() {
    let module = type_check(concat!(
        "type Box = T => (value: T)\n",
        "def unbox: T => Box T -> T = Box (value) => value\n",
        "let answer: I32 = unbox (Box (value: 42))\n",
        "let text: String = unbox (Box (value: \"hello\"))\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic nominal patterns should monomorphize");
}

#[test]
fn captures_values_bound_by_nominal_patterns() {
    let module = type_check(concat!(
        "type UserId = I32\n",
        "def capture: UserId -> (() -> I32) = user => {\n",
        "  let UserId id = user\n",
        "  () => id\n",
        "}\n",
        "let get_id: () -> I32 = capture (UserId 42)\n",
        "let answer: I32 = get_id ()\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("destructured leaves should be captured normally");
}

#[test]
fn rejects_non_nominal_and_mismatched_nominal_patterns() {
    let syntax = parse(concat!(
        "type alias Number = I32\n",
        "let Number value = 42\n",
    ))
    .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve(&syntax)
        .expect_err("aliases cannot be used as nominal patterns");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("not a represented nominal type")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Left = I32\n",
            "type Right = I32\n",
            "let right: Right = Right 42\n",
            "let Left value = right\n",
        )))
        .expect_err("a nominal pattern must match the same nominal type");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("cannot match"))
    );
}

#[test]
fn monomorphizes_nested_and_recursive_generic_calls() {
    let module = type_check(concat!(
        "def identity: T => T -> T = x => x\n",
        "def copy: U => U -> U = x => identity x\n",
        "def recur: V => V -> V = x => recur x\n",
        "let answer: I32 = copy 42\n",
        "let recurse: I32 = recur 1\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nested generic calls should discover concrete specializations");
}

#[test]
fn monomorphizes_generic_closures_with_captures() {
    let module = type_check(concat!(
        "def outer: I32 -> I32 = y => {\n",
        "  def inner: T => T -> I32 = x => y\n",
        "  inner \"ignored\"\n",
        "}\n",
        "let answer: I32 = outer 42\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("generic closures should retain their captured environment");
}

#[test]
fn infers_product_and_result_only_compile_time_parameters() {
    let module = type_check(concat!(
        "def first: (A, B) => (A, B) -> A = (a, b) => a\n",
        "type Phantom = T => I32\n",
        "def make: T => I32 -> Phantom T = x => Phantom x\n",
        "let answer: I32 = first (42, \"ignored\")\n",
        "let contextual: Phantom String = make 7\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("multiple compile-time parameters should specialize");
}

#[test]
fn monomorphizes_curried_generic_function_layers() {
    let module = type_check(concat!(
        "def keep_first: (A, B) => Copy A => A -> B -> A = a => b => a\n",
        "let answer: I32 = keep_first 42 \"ignored\"\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("curried generic layers should specialize together");
}

#[test]
fn rejects_unconstrained_generic_values_and_non_function_schemes() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def identity: T => T -> T = x => x\n",
            "def copied = identity\n",
            "def invalid: U => U = 42\n",
        )))
        .expect_err("generic values require a concrete use or function scheme");
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("requires a concrete expected type"))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("function-valued `def`"))
    );
}

#[test]
fn rejects_polymorphic_recursion() {
    let diagnostics = TypeChecker::new()
        .check(resolve("def grow: T => T -> T = x => grow (x, x)\n"))
        .expect_err("recursive calls may not change their specialization");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("polymorphic recursion")),
        "{diagnostics:?}"
    );
}

#[test]
fn infers_explicit_returns_and_keeps_trailing_expression_semicolons_value_preserving() {
    let module = type_check(concat!(
        "def explicit = () => { return 42; \"unreachable\"; };\n",
        "def implicit = () => { 42; };\n",
        "def binding_ended = () => { let value = 42; };\n",
        "def returned_unit = () => { return (); };\n",
        "explicit ();\n",
    ));

    for function in module
        .functions()
        .iter()
        .filter(|function| matches!(function.name.as_str(), "explicit" | "implicit"))
    {
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("function should have a checked type")
                .result,
            CheckedType::I32,
        );
    }
    for function in module
        .functions()
        .iter()
        .filter(|function| matches!(function.name.as_str(), "binding_ended" | "returned_unit"))
    {
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("function should have a checked type")
                .result,
            CheckedType::empty_product(),
        );
    }
}

#[test]
fn returns_from_nested_expression_blocks() {
    let module = type_check(concat!(
        "def identity = (value: I32) => value;\n",
        "def answer = () => { identity { return 42; }; 0; };\n",
        "def binding = () => { let unreachable: String = { return 7; }; 0; };\n",
        "answer ();\n",
        "binding ();\n",
    ));
    let answer = module
        .functions()
        .iter()
        .find(|function| function.name == "answer")
        .expect("answer should resolve");
    assert_eq!(
        *module
            .type_of_function(answer.id)
            .expect("answer should be checked")
            .result,
        CheckedType::I32,
    );
    let binding = module
        .functions()
        .iter()
        .find(|function| function.name == "binding")
        .expect("binding should resolve");
    assert_eq!(
        *module
            .type_of_function(binding.id)
            .expect("binding should be checked")
            .result,
        CheckedType::I32,
    );

    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nested return should generate valid LLVM");
    assert!(llvm.contains("ret i32 42"));
}

#[test]
fn rejects_returns_outside_functions_and_incompatible_return_values() {
    let outside = parse("return 1\n").expect("return statement should parse");
    let diagnostics = NameResolver::new()
        .resolve(&outside)
        .expect_err("top-level return should not resolve");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("only allowed inside a function")
    }));

    let module = resolve("def invalid = () => { return 42; } satisfies String\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("return value should match the function result");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("expected `String`, found `I32`")
    }));
}

#[test]
fn supports_contextual_literals_and_arithmetic_for_all_integer_types() {
    let source = concat!(
        "def i8_value = () => { let a: I8 = 8; let b: I8 = 4; (a + b) * b - a / b; } satisfies I8\n",
        "def i16_value = () => { let a: I16 = 8; let b: I16 = 4; (a + b) * b - a / b; } satisfies I16\n",
        "def i32_value = () => { let a: I32 = 8; let b: I32 = 4; (a + b) * b - a / b; } satisfies I32\n",
        "def i64_value = () => { let a: I64 = 8; let b: I64 = 4; (a + b) * b - a / b; } satisfies I64\n",
        "def u8_value = () => { let a: U8 = 8; let b: U8 = 4; (a + b) * b - a / b; } satisfies U8\n",
        "def u16_value = () => { let a: U16 = 8; let b: U16 = 4; (a + b) * b - a / b; } satisfies U16\n",
        "def u32_value = () => { let a: U32 = 8; let b: U32 = 4; (a + b) * b - a / b; } satisfies U32\n",
        "def u64_value = () => { let a: U64 = 8; let b: U64 = 4; (a + b) * b - a / b; } satisfies U64\n",
        "def isize_value = () => { let a: ISize = 8; let b: ISize = 4; (a + b) * b - a / b; } satisfies ISize\n",
        "def usize_value = () => { let a: USize = 8; let b: USize = 4; (a + b) * b - a / b; } satisfies USize\n",
        "i8_value ()\n",
    );
    let module = type_check(source);

    let expected = [
        ("i8_value", CheckedType::I8),
        ("i16_value", CheckedType::I16),
        ("i32_value", CheckedType::I32),
        ("i64_value", CheckedType::I64),
        ("u8_value", CheckedType::U8),
        ("u16_value", CheckedType::U16),
        ("u32_value", CheckedType::U32),
        ("u64_value", CheckedType::U64),
        ("isize_value", CheckedType::ISize),
        ("usize_value", CheckedType::USize),
    ];
    for (name, expected_type) in expected {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .unwrap();
        assert_eq!(
            *module.type_of_function(function.id).unwrap().result,
            expected_type
        );
    }

    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("all integer arithmetic should generate valid LLVM");
}

#[test]
fn rejects_out_of_range_and_mixed_integer_arithmetic() {
    let diagnostics = TypeChecker::new()
        .check(resolve("let too_large: U8 = 256\n"))
        .expect_err("an out-of-range literal should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("integer literal `256` does not fit in `U8`")
    }));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def mixed = (left: I8, right: I16) => left + right\n",
            "mixed (1, 2)\n",
        )))
        .expect_err("mixed integer arithmetic should require an explicit conversion");
    assert!(!diagnostics.is_empty());
}

#[test]
fn emits_a_native_object_file() {
    let module = type_check(include_str!("../examples/hello_world.sta"));
    let context = Context::create();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should follow the Unix epoch")
        .as_nanos();
    let output = std::env::temp_dir().join(format!("stapler-test-{nonce}.o"));

    CodeGenerator::new(&context)
        .emit_object(&module, &output, None)
        .expect("native object emission should succeed");
    let length = std::fs::metadata(&output)
        .expect("object should exist")
        .len();
    std::fs::remove_file(&output).expect("temporary object should be removable");

    assert!(length > 0);
}

#[test]
fn infers_copy_and_enforces_affine_moves() {
    type_check(concat!(
        "type Point = (I32, I32)\n",
        "def copied = () => { let point = Point (1, 2); let other = point; point; other }\n",
    ));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop *\n",
            "def invalid = (value: CString) => { let moved = value; value }\n",
        )))
        .expect_err("CString must be move-only");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("use of moved value"))
    );
}

#[test]
fn exposes_copy_but_rejects_explicit_implementations() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Value = I32\n",
            "impl Copy Value {}\n",
        )))
        .expect_err("Copy implementations must be inferred");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("`Copy` is implemented structurally")
    }));
}

#[test]
fn lowers_custom_drop_and_gc_finalizer_glue() {
    let module = type_check(concat!(
        "type Resource = I32\n",
        "impl Drop Resource { def drop = Resource value => () }\n",
        "def release = () => { let resource = Resource 7; drop resource }\n",
        "def managed = () => Ref (Resource 9)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("Drop glue should compile");

    assert!(llvm.contains("drop.call"));
    assert!(llvm.contains("__staple_gc_set_finalizer"));
    assert!(llvm.contains("__staple_gc_finalize_"));
}

#[test]
fn rejects_move_only_globals_partial_moves_and_cstring_returns_from_c() {
    let ffi_diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop *\n",
            "extern \"c\" { let invalid_return: () -> CString }\n",
        )))
        .expect_err("C must not manufacture owned CStrings");
    assert!(
        ffi_diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("cannot return owned `CString`") })
    );

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "use std.cinterop *\n",
            "let global: CString = c_string \"owned\"\n",
            "def partial = (pair: (CString, I32)) => pair.0\n",
        )))
        .expect_err("unsupported ownership operations should be rejected");
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("move-only values cannot be stored"))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("cannot move a field"))
    );
}

#[test]
fn moves_resources_into_managed_closures_and_borrows_ref_payloads() {
    let module = type_check(concat!(
        "use std.cinterop *\n",
        "extern \"c\" { let inspect: CString -> I32 }\n",
        "def make = (value: CString) => { let callback = () => inspect value; callback }\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a move-only capture should get managed finalizer glue");
    assert!(llvm.contains("__staple_gc_finalize_closure_"));

    for source in [
        concat!(
            "use std.cinterop *\n",
            "extern \"c\" { let inspect: CString -> I32 }\n",
            "def invalid = (value: CString) => { let callback = () => inspect value; value }\n",
        ),
        concat!(
            "use std.cinterop *\n",
            "def invalid = (value: Ref CString) => { let Ref inner = value; drop inner }\n",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("captured and Ref-owned resources cannot be moved again");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("moved value")
                || diagnostic.message.contains("cannot move this value")
        }));
    }
}

#[test]
fn lowers_path_sensitive_drop_flags() {
    let module = type_check(concat!(
        "type Resource = I32\n",
        "impl Drop Resource { def drop = Resource value => () }\n",
        "def conditional = (flag: Bool, resource: Resource) => match flag {\n",
        "  True() => { drop resource; () },\n",
        "  False() => (),\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("conditional ownership should lower through a runtime drop flag");
    assert!(llvm.contains("drop.is_live"));
    assert!(llvm.contains("drop.call"));
}

#[test]
fn supports_contextual_string_literal_types_and_widening() {
    let module = type_check(concat!(
        "type alias Answer = \"yes\" | \"no\"\n",
        "def inferred = () => \"yes\"\n",
        "def narrow: () -> Answer = () => \"yes\"\n",
        "let answer: Answer = narrow()\n",
        "let widened: String = answer\n",
        "let absorbed: \"yes\" | String = \"anything\"\n",
    ));
    let inferred = module
        .functions()
        .iter()
        .find(|function| function.name == "inferred")
        .expect("inferred function");
    assert_eq!(
        *module.type_of_function(inferred.id).unwrap().result,
        CheckedType::String
    );
    let narrow = module
        .functions()
        .iter()
        .find(|function| function.name == "narrow")
        .expect("narrow function");
    assert_eq!(
        *module.type_of_function(narrow.id).unwrap().result,
        CheckedType::StringLiteralSet(vec!["no".to_owned(), "yes".to_owned()])
    );
}

#[test]
fn rejects_invalid_string_literal_narrowing() {
    for (source, expected) in [
        (
            "def invalid: () -> \"yes\" | \"no\" = () => \"maybe\"\n",
            "expected `\"no\" | \"yes\"`, found `String`",
        ),
        (
            "let broad: String = \"yes\"\nlet narrow: \"yes\" | \"no\" = broad\n",
            "expected `\"no\" | \"yes\"`, found `String`",
        ),
    ] {
        let diagnostics = TypeChecker::new()
            .check(resolve(source))
            .expect_err("invalid literal refinement should be rejected");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "missing `{expected}` in {diagnostics:?}",
        );
    }
}

#[test]
fn matches_literal_sets_strings_and_mixed_nominal_unions() {
    let module = type_check(concat!(
        "type Some = String\n",
        "def pure: (\"yes\" | \"no\") -> String = value => match value {\n",
        "  \"yes\" => \"affirmative\",\n",
        "  \"no\" => \"negative\",\n",
        "}\n",
        "def broad: String -> String = value => match value {\n",
        "  \"yes\" => \"affirmative\",\n",
        "  _ => \"other\",\n",
        "}\n",
        "def mixed: Some | \"yes\" | \"no\" -> String = value => match value {\n",
        "  Some text => text,\n",
        "  \"yes\" => \"affirmative\",\n",
        "  \"no\" => \"negative\",\n",
        "}\n",
        "let small: \"yes\" = \"yes\"\n",
        "let larger: \"yes\" | \"no\" = small\n",
        "let injected: Some | \"yes\" | \"no\" = larger\n",
        "pure small\n",
        "broad \"other\"\n",
        "mixed injected\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("literal and mixed matches should generate LLVM");
    assert!(llvm.contains("match.string.length_matches"));
    assert!(llvm.contains("@memcmp"));
    assert!(llvm.contains("sum.target.tag"));
}

#[test]
fn checks_literal_match_exhaustiveness_and_reachability() {
    let missing = TypeChecker::new()
        .check(resolve(concat!(
            "def invalid: (\"yes\" | \"no\") -> String = value => match value {\n",
            "  \"yes\" => \"only\",\n",
            "}\n",
        )))
        .expect_err("a literal alternative is missing");
    assert!(
        missing
            .iter()
            .any(|diagnostic| diagnostic.message.contains("non-exhaustive match"))
    );

    let duplicate = TypeChecker::new()
        .check(resolve(concat!(
            "def invalid: String -> String = value => match value {\n",
            "  \"yes\" => \"first\",\n",
            "  \"yes\" => \"second\",\n",
            "  _ => \"other\",\n",
            "}\n",
        )))
        .expect_err("a duplicate literal arm is unreachable");
    assert!(
        duplicate
            .iter()
            .any(|diagnostic| diagnostic.message == "unreachable match arm")
    );
}

#[test]
fn rejects_refutable_string_literal_binding_patterns() {
    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "def invalid: String -> String = \"yes\" => \"matched\"\n",
        )))
        .expect_err("literal function patterns must be irrefutable");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("string literal binding pattern must have the same singleton literal type")
    }));

    type_check("def valid: \"yes\" -> String = \"yes\" => \"matched\"\n");
}

#[test]
fn infers_and_generates_loop_result_values() {
    let module = type_check(concat!(
        "def answer = () => loop { break 42 }\n",
        "def nested = () => loop { loop { break () }; break 7 }\n",
        "def unit = () => loop { break }\n",
        "answer ()\n",
        "nested ()\n",
        "unit ()\n",
    ));
    for (name, expected) in [
        ("answer", CheckedType::I32),
        ("nested", CheckedType::I32),
        ("unit", CheckedType::empty_product()),
    ] {
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == name)
            .expect("loop function should resolve");
        assert_eq!(
            *module
                .type_of_function(function.id)
                .expect("loop function should be typed")
                .result,
            expected,
        );
    }
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("loops should generate valid LLVM");
    assert!(llvm.contains("loop.body"));
    assert!(llvm.contains("loop.exit"));
    assert!(llvm.contains("loop.value"));
}

#[test]
fn supports_loop_control_through_matches_and_function_returns() {
    let module = type_check(concat!(
        "def select: Bool -> I32 = condition => loop {\n",
        "  match condition { True() => { break 9 }, False() => { continue } }\n",
        "}\n",
        "def early: Bool -> I32 = condition => loop {\n",
        "  match condition { True() => { return 5 }, False() => { break 6 } }\n",
        "}\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nested loop control should generate valid LLVM");
}

#[test]
fn rejects_invalid_loop_control_and_break_types() {
    for (source, expected) in [
        ("break 1\n", "`break` is only allowed inside a loop"),
        ("continue\n", "`continue` is only allowed inside a loop"),
        (
            "def invalid = () => loop { def nested = () => { break }; break () }\n",
            "`break` is only allowed inside a loop",
        ),
    ] {
        let diagnostics = NameResolver::new()
            .resolve(&parse(source).expect("invalid loop control should still parse"))
            .expect_err("loop control should fail resolution");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "missing `{expected}` in {diagnostics:?}",
        );
    }

    let diagnostics = TypeChecker::new()
        .check(resolve("def invalid: () -> I32 = () => loop { break }\n"))
        .expect_err("unit break should not satisfy I32");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("expected `I32`") && diagnostic.message.contains("found `()`")
    }));
}

#[test]
fn accepts_divergent_loops_in_typed_contexts() {
    let module = type_check("def forever: () -> I32 = () => loop { continue }\n");
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a divergent loop should satisfy its expected type");
}

#[test]
fn checks_ownership_across_loop_exits_and_back_edges() {
    let module = type_check(concat!(
        "type Resource = I32\n",
        "impl Drop Resource { def drop = Resource _ => () }\n",
        "def choose: Bool -> Resource = condition => loop {\n",
        "  let value = Resource 1\n",
        "  match condition {\n",
        "    True() => { break value },\n",
        "    False() => { continue },\n",
        "  }\n",
        "}\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("loop exits should preserve moved results and drop iteration locals");
    assert!(llvm.contains("loop.value"));
    assert!(llvm.contains("drop.call"));

    let diagnostics = TypeChecker::new()
        .check(resolve(concat!(
            "type Resource = I32\n",
            "impl Drop Resource { def drop = Resource _ => () }\n",
            "def invalid = (value: Resource) => loop {\n",
            "  let consumed = value\n",
            "  continue\n",
            "}\n",
        )))
        .expect_err("an outer move-only value cannot be consumed before a back-edge");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("may be moved before the next loop iteration")
    }));
}
