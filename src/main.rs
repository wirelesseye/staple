use stapler::{CodeGenerator, NameResolver, TypeChecker};

fn main() {
    let source = include_str!("../examples/hello_world.sta");
    let module = stapler::parse(source).expect("example should parse");
    let module = NameResolver::new()
        .resolve(&module)
        .expect("example should resolve");
    let module = TypeChecker::new()
        .check(module)
        .expect("example should type-check");

    let context = inkwell::context::Context::create();
    let code_generator = CodeGenerator::new(&context);
    let result = code_generator
        .compile_module(&module)
        .expect("example should compile");
    println!("{}", result);
}
