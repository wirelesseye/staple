use inkwell::context::Context;
use stapler::{
    CheckedType, CodeGenerator, Item, NameResolver, ProgramLoader, Statement, TypeChecker, parse,
};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn resolve(source: &str) -> stapler::ResolvedModule {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)
        .expect("source should load");
    NameResolver::new()
        .resolve_program(program)
        .expect("source should resolve")
}

fn type_check(source: &str) -> stapler::TypedModule {
    TypeChecker::new()
        .check(resolve(source))
        .expect("source should type-check")
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
    std::fs::write(temporary.join("std/core/string.sta"), declaration)
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
        "  True() => Ok(1),\n",
        "  False() => IOError(\"no\"),\n",
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
fn rejects_invalid_match_subjects_and_coverage() {
    let diagnostics = TypeChecker::new()
        .check(resolve("match 1 { value => value, }\n"))
        .expect_err("non-sum subjects should fail");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("match subject must have a sum or product type")
    }));

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
        "  (True(), True()) => True,\n",
        "  (True(), False()) => False,\n",
        "  (False(), True()) => False,\n",
        "  (False(), False()) => True,\n",
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
fn rejects_invalid_sum_alternatives_and_ambiguous_nominal_heads() {
    for (source, expected) in [
        (
            "let value: I32 | Ok I32\n",
            "not a represented nominal type",
        ),
        (
            "let value: Ok I32 | Ok String\n",
            "multiple applications of the same nominal type",
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
        "def before: T => Copy T => Compare T => T -> T -> Bool = left => right => left < right\n",
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
        "icmp eq", "icmp ne", "icmp slt", "icmp sle", "icmp sgt", "icmp sge", "icmp ult",
    ] {
        assert!(llvm.contains(predicate), "missing `{predicate}` in LLVM");
    }
    assert!(llvm.contains("bool.tag"));
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
fn evaluates_pure_syntax_helpers_and_conditional_macros() {
    let module = type_check(concat!(
        "def syntax_identity: Syntax -> Syntax = value => value\n",
        "macro choose = condition => then => else => quote {\n",
        "    match $condition { True() => $then, False() => $else, }\n",
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
        "    Ident \"else\" =>\n",
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
fn expands_standard_if_overloads_and_nested_forms() {
    let module = type_check(concat!(
        "let condition: Bool = True\n",
        "let without_else = if condition ()\n",
        "let with_else: I32 = if condition 2 else 3\n",
        "let nested: I32 = if condition (if condition 4 else 5) else 6\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("standard if overloads should expand and generate code");
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
        "macro select = value: Expr => Ident \"with\" => replacement: Expr => quote { $replacement }\n",
        "macro classify = value: Syntax => quote { 1 }\n",
        "macro classify = value: Expr => quote { 2 }\n",
        "macro classify = value: Ident String => quote { 3 }\n",
        "macro classify = Ident \"else\" => quote { 4 }\n",
        "let longest: I32 = select 0 with 20\n",
        "let specific: I32 = classify else\n",
    ));
    let context = Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("selected overloads should generate code");
}

#[test]
fn incomplete_longer_macro_overloads_fall_back_to_shorter_forms() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("let condition: Bool = True\nif condition 1 else\n", root)
        .expect("source should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("leftover else should be resolved as ordinary syntax");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unknown name `else`"))
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("matching overloads require 4") })
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
                "macro conditional = value: Expr => Ident \"else\" => quote { $value }\n",
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
            "macro recursive = value => quote { recursive $value }\nrecursive 1\n",
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
            "macro many = values => quote { $values... }\nmany 1\n",
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
                result: Box::new(CheckedType::Function(stapler::CheckedFunctionType {
                    parameter: Box::new(CheckedType::I32),
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
fn rejects_invalid_string_literal_narrowing_and_unrestricted_mixed_sums() {
    for (source, expected) in [
        (
            "def invalid: () -> \"yes\" | \"no\" = () => \"maybe\"\n",
            "expected `\"no\" | \"yes\"`, found `String`",
        ),
        (
            "let broad: String = \"yes\"\nlet narrow: \"yes\" | \"no\" = broad\n",
            "expected `\"no\" | \"yes\"`, found `String`",
        ),
        (
            "type Some = String\nlet value: Some | String\n",
            "unrestricted `String` cannot be mixed",
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
