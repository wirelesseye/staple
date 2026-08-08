use inkwell::context::Context;
use stapler::{CodeGenerator, NameResolver, parse};

fn resolve(source: &str) -> stapler::ResolvedModule {
    let syntax = parse(source).expect("source should parse");
    NameResolver::new()
        .resolve(&syntax)
        .expect("source should resolve")
}

#[test]
fn resolves_names_in_function_parameters_and_binary_expressions() {
    let source = "def add: _ -> i32 = (a: i32, b: i32) -> a + b\nadd (1, 2)\n";
    let module = resolve(source);

    assert_eq!(module.syntax().text(), source);
    assert_eq!(module.functions().len(), 1);
    assert_eq!(module.functions()[0].name, "add");
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
    let module = resolve("def add: _ -> i32 = (a: i32, b: i32) -> a + b\nadd (1, 2)\n");
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
    let module = resolve("def recurse: i32 -> i32 = n: i32 -> recurse n\n");
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("recursive function should compile");

    assert!(llvm.contains("call i32 @recurse(i32 %n)"));
}

#[test]
fn decodes_source_string_literals_before_llvm_generation() {
    let source = concat!(
        "extern \"c\" { let puts: (*const c_char) -> i32 }\n",
        "puts \"hello\\n\"\n",
    );
    let module = resolve(source);
    let context = Context::create();
    let llvm = CodeGenerator::new(&context)
        .compile_module(&module)
        .expect("module should compile");

    assert!(llvm.contains("c\"hello\\0A\\00\""));
    assert!(!llvm.contains("\\22hello"));
}
