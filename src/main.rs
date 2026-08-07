fn main() {
    let source = include_str!("../examples/hello_world.sta");
    match stapler::parse(source) {
        Ok(ast) => println!("{ast:#?}"),
        Err(error) => {
            eprintln!("parse error: {error}");
            std::process::exit(1);
        }
    }
}
