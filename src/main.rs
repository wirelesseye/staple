use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use stapler::{CodeGenerator, Diagnostic, NameResolver, TypeChecker};

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1)) {
        Ok(llvm) => {
            print!("{llvm}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("stapler: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<String, String> {
    let mut arguments = arguments.into_iter();
    let input = arguments.next().ok_or_else(usage)?;
    if arguments.next().is_some() {
        return Err(usage());
    }

    let source = read_source(input)?;
    compile(&source)
}

fn read_source(input: OsString) -> Result<String, String> {
    if input == "-" {
        let mut source = String::new();
        std::io::stdin()
            .read_to_string(&mut source)
            .map_err(|error| format!("could not read standard input: {error}"))?;
        return Ok(source);
    }

    let path = PathBuf::from(input);
    std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read `{}`: {error}", path.display()))
}

fn compile(source: &str) -> Result<String, String> {
    let module = stapler::parse(source).map_err(|error| error.to_string())?;
    let module = NameResolver::new()
        .resolve(&module)
        .map_err(format_diagnostics)?;
    let module = TypeChecker::new()
        .check(module)
        .map_err(format_diagnostics)?;

    let context = inkwell::context::Context::create();
    CodeGenerator::new(&context)
        .compile_module(&module)
        .map_err(format_diagnostics)
}

fn format_diagnostics(diagnostics: Vec<Diagnostic>) -> String {
    diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn usage() -> String {
    "usage: stapler <input.sta>\n       stapler -  # read from standard input".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{compile, run};

    #[test]
    fn requires_exactly_one_input() {
        assert!(run([]).unwrap_err().starts_with("usage:"));
        assert!(
            run(["one.sta".into(), "two.sta".into()])
                .unwrap_err()
                .starts_with("usage:")
        );
    }

    #[test]
    fn compiles_source_to_llvm() {
        let llvm =
            compile(include_str!("../examples/hello_world.sta")).expect("example should compile");

        assert!(llvm.contains("define i32 @main()"));
    }
}
