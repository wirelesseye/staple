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
        "let second = (a: I32, b: I32) -> I32 => b\n",
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
fn rejects_an_incorrect_function_result_type() {
    let module = resolve("let answer = () -> string => 42\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("incorrect return type should fail");

    assert!(
        diagnostics[0]
            .message
            .contains("expected `string`, found `I32`")
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
            .contains("expected `I32`, found `string`")
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
    assert!(llvm.contains("call i32 @first(ptr null, i32 1, i32 2)"));
}

#[test]
fn predeclares_functions_for_recursion() {
    let module = type_check("def recurse: (n: I32) -> I32 = (n: I32) => recurse (n)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("recursive function should compile");

    assert!(llvm.contains("call i32 @recurse(ptr %0, i32 %n)"));
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
    assert!(llvm.contains("call i32 @recurse(ptr %0, i32 %n)"));
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
        "extern \"c\" { let puts: (*const c_char) -> I32 }\n",
        "def apply: ((*const c_char) -> I32, *const c_char) -> I32 = (f, value) => f value\n",
        "apply (puts, \"hello\")\n",
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
        "extern \"c\" { let puts: (*const c_char) -> I32 }\n",
        "puts (\"hello\\n\")\n",
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
