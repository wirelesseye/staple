use stapler::{CodeGen, Normaliser};

fn main() {
    let source = include_str!("../examples/hello_world.sta");
    let mut module = stapler::parse(source).unwrap();

    let mut normaliser = Normaliser::new();
    normaliser.normalise(&mut module);

    let context = inkwell::context::Context::create();
    let codegen = CodeGen::new(&context);
    let result = codegen.compile_module(&module);
    println!("{}", result);
}
