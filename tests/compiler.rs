use inkwell::context::Context;
use stapler::{CheckedType, CodeGenerator, NameResolver, TypeChecker, parse};
use std::time::{SystemTime, UNIX_EPOCH};

fn resolve(source: &str) -> stapler::ResolvedModule {
    let syntax = parse(source).expect("source should parse");
    NameResolver::new()
        .resolve(&syntax)
        .expect("source should resolve")
}

fn type_check(source: &str) -> stapler::TypedModule {
    TypeChecker::new()
        .check(resolve(source))
        .expect("source should type-check")
}

#[test]
fn resolves_names_in_function_parameters_and_binary_expressions() {
    let source = "def add: _ -> i32 = (a: i32, b: i32) => a + b\nadd (1, 2)\n";
    let module = resolve(source);

    assert_eq!(module.syntax().text(), source);
    assert_eq!(module.functions().len(), 1);
    assert_eq!(module.functions()[0].name, "add");
}

#[test]
fn infers_and_checks_function_return_types() {
    let module = type_check(concat!(
        "let add = (a: i32, b: i32) => a + b\n",
        "let subtract = (a: i32, b: i32) -> i32 => a - b\n",
        "add (1, subtract (3, 2))\n",
    ));

    for function in module.functions() {
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

    for function in module.functions() {
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
            .contains("expected `string`, found `i32`")
    );
}

#[test]
fn rejects_incorrect_call_arguments() {
    let module = resolve("let identity = (value: i32) => value\nidentity (\"wrong\")\n");
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("incorrect argument type should fail");

    assert!(
        diagnostics[0]
            .message
            .contains("expected `i32`, found `string`")
    );
}

#[test]
fn treats_singleton_products_as_their_element() {
    let module = type_check(concat!(
        "let answer: (i32) = 42\n",
        "let identity = (value: i32) => value\n",
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
    assert!(llvm.contains("define i32 @identity(i32 %value)"));
    assert!(!llvm.contains("define i32 @identity({ i32 }"));
}

#[test]
fn destructures_nested_product_patterns() {
    let module = type_check(concat!(
        "let add_nested = (x: i32, (y: i32, z: i32)) => x + y + z\n",
        "add_nested (1, (2, 3))\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("nested product pattern should compile");

    assert!(llvm.contains("define i32 @add_nested(i32 %x, <{ i32, i32 }>"));
    assert!(llvm.contains("extractvalue <{ i32, i32 }>"));
}

#[test]
fn binds_a_product_without_destructuring_it() {
    let module = type_check(concat!(
        "let sum = pair: (i32, i32) => pair.0 + pair.1\n",
        "sum (1, 2)\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("a product binding pattern should compile");

    assert!(llvm.contains("define i32 @sum(i32 %0, i32 %1)"));
    assert!(llvm.contains("extractvalue <{ i32, i32 }>"));
}

#[test]
fn type_checks_transparent_aliases() {
    type_check("type alias number = i32\nlet answer: number = 42\n");
}

#[test]
fn type_checks_function_declarations_without_values() {
    type_check("let add: (x: i32, y: i32) -> i32\n");
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
    let module = type_check("let add = (a: i32, b: i32) => a + b\nadd (1, 2)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("module should compile");

    assert!(llvm.contains("define i32 @add(i32 %a, i32 %b)"));
    assert!(llvm.contains("add i32 %a, %b"));
    assert!(llvm.contains("call i32 @add(i32 1, i32 2)"));
}

#[test]
fn predeclares_functions_for_recursion() {
    let module = type_check("def recurse: (n: i32) -> i32 = (n: i32) => recurse (n)\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("recursive function should compile");

    assert!(llvm.contains("call i32 @recurse(i32 %n)"));
}

#[test]
fn does_not_leak_locals_between_generated_functions() {
    let module = type_check(concat!(
        "let outer = (value: i32) => {\n",
        "  let inner = () => value\n",
        "  inner ()\n",
        "}\n",
    ));
    let context = Context::create();
    let diagnostics = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect_err("captured locals require closure lowering");

    assert_eq!(
        diagnostics[0].message,
        "value `value` is not available here"
    );
}

#[test]
fn decodes_source_string_literals_before_llvm_generation() {
    let source = concat!(
        "extern \"c\" { let puts: (*const c_char) -> i32 }\n",
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
