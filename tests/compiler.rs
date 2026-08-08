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
fn rejects_an_incorrect_function_result_type() {
    let module = resolve("use std.cinterop.*\nlet answer = () -> CString => 42\n");
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
        "use std.cinterop.*\n",
        "let character: CChar\n",
        "let pointer: CPointer CChar\n",
        "let text: CString\n",
    ));
}

#[test]
fn c_pointer_preserves_its_pointee_type() {
    let module = resolve(concat!(
        "use std.cinterop.*\n",
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
        "let first: Handle I32\n",
        "let second: Handle String = first\n",
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
fn c_string_is_an_imported_primitive_macro() {
    let module = type_check(concat!(
        "use std.cinterop.*\n",
        "let text: String = \"hello\"\n",
        "let c_text: CString = c_string \"hello\"\n",
        "let copied: String = string_from_c_string c_text\n",
        "let converted: CString = string_to_c_string text\n",
    ));
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("String operations should compile");

    assert!(llvm.contains("string.capacity"));
    assert!(llvm.contains("@strlen"));
    assert!(llvm.contains("@memchr"));
    assert!(llvm.contains("@llvm.trap"));
    assert!(llvm.contains("c\"hello\\00\""));
}

#[test]
fn c_string_rejects_non_literal_arguments() {
    let module = resolve(concat!(
        "use std.cinterop.*\n",
        "let text = \"hello\"\n",
        "c_string text\n",
    ));
    let diagnostics = TypeChecker::new()
        .check(module)
        .expect_err("c_string should require a literal");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "`c_string` requires a string literal")
    );
}

#[test]
fn c_string_rejects_interior_nul_bytes() {
    let module = type_check("use std.cinterop.*\nc_string \"bad\\0value\"\n");
    let context = Context::create();
    let diagnostics = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect_err("interior NUL should fail code generation");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message == "C string literals cannot contain an interior NUL byte"
    }));
}

#[test]
fn user_defined_macros_are_reserved_for_future_support() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source("macro custom\n", root)
        .expect("macro declaration should parse");
    let diagnostics = NameResolver::new()
        .resolve_program(program)
        .expect_err("user macro should not resolve yet");
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message == "user-defined macros are not supported yet"
        })
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
        "use std.cinterop.*\n",
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
        "use std.cinterop.*\n",
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
        "def loop: V => V -> V = x => loop x\n",
        "let answer: I32 = copy 42\n",
        "let recurse: I32 = loop 1\n",
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
        "def keep_first: (A, B) => A -> B -> A = a => b => a\n",
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

    let module = resolve("def invalid = () -> String => { return 42; }\n");
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
        "def i8_value = () -> I8 => { let a: I8 = 8; let b: I8 = 4; (a + b) * b - a / b; }\n",
        "def i16_value = () -> I16 => { let a: I16 = 8; let b: I16 = 4; (a + b) * b - a / b; }\n",
        "def i32_value = () -> I32 => { let a: I32 = 8; let b: I32 = 4; (a + b) * b - a / b; }\n",
        "def i64_value = () -> I64 => { let a: I64 = 8; let b: I64 = 4; (a + b) * b - a / b; }\n",
        "def u8_value = () -> U8 => { let a: U8 = 8; let b: U8 = 4; (a + b) * b - a / b; }\n",
        "def u16_value = () -> U16 => { let a: U16 = 8; let b: U16 = 4; (a + b) * b - a / b; }\n",
        "def u32_value = () -> U32 => { let a: U32 = 8; let b: U32 = 4; (a + b) * b - a / b; }\n",
        "def u64_value = () -> U64 => { let a: U64 = 8; let b: U64 = 4; (a + b) * b - a / b; }\n",
        "def isize_value = () -> ISize => { let a: ISize = 8; let b: ISize = 4; (a + b) * b - a / b; }\n",
        "def usize_value = () -> USize => { let a: USize = 8; let b: USize = 4; (a + b) * b - a / b; }\n",
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
