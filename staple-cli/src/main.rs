use std::ffi::OsString;
use std::process::{ExitCode, ExitStatus};

mod compile;
mod expand;
mod fmt;
mod lsp;
mod package;

/// The result of a single `staple` invocation, shared by every subcommand.
#[derive(Debug)]
pub enum Outcome {
    /// Finished successfully. `Some(text)` is written to standard output verbatim.
    Completed(Option<String>),
    /// A child process (the user's program) ran; propagate its status.
    Executed(ExitStatus),
    /// `staple fmt --check` found the input was not formatted.
    FormatMismatch(String),
}

fn main() -> ExitCode {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let command = arguments.first().and_then(|argument| argument.to_str());

    let result = match command {
        Some("lsp") => return lsp::run(arguments[1..].to_vec()),
        Some("-h") | Some("--help") | None => {
            Ok(Outcome::Completed(Some(format!("{}\n", usage()))))
        }
        Some("new") | Some("build") => package::run(arguments),
        Some("check") | Some("run") => {
            if uses_file_mode(&arguments) {
                compile::run(arguments)
            } else {
                package::run(arguments)
            }
        }
        Some("expand") => expand::run(arguments),
        Some("fmt") => fmt::run(arguments),
        Some("compile") => {
            if matches!(
                arguments.get(1).and_then(|argument| argument.to_str()),
                Some("run" | "check" | "expand" | "fmt" | "new" | "build")
            ) {
                Err(format!(
                    "`staple compile` does not take a subcommand\n{}",
                    compile::usage()
                ))
            } else {
                compile::run(arguments[1..].to_vec())
            }
        }
        Some(other) => Err(format!("unknown command `{other}`\n{}", usage())),
    };

    finish(result)
}

fn finish(result: Result<Outcome, String>) -> ExitCode {
    match result {
        Ok(Outcome::Completed(Some(output))) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Completed(None)) => ExitCode::SUCCESS,
        Ok(Outcome::Executed(status)) => compile::exit_code(status),
        Ok(Outcome::FormatMismatch(input)) => {
            eprintln!("staple: `{input}` is not formatted");
            ExitCode::FAILURE
        }
        Err(message) => {
            eprintln!("staple: {message}");
            ExitCode::FAILURE
        }
    }
}

/// `staple check` / `staple run` operate on the current package unless a source
/// file (or `-`) is named, or a manifest is pointed at explicitly. Options that
/// take a value are skipped so their arguments are not mistaken for an input.
fn uses_file_mode(arguments: &[OsString]) -> bool {
    let mut iterator = arguments.iter().skip(1);
    while let Some(argument) = iterator.next() {
        if argument == "--" {
            break;
        }
        let Some(text) = argument.to_str() else {
            return true;
        };
        if text == "--manifest-path" || text.starts_with("--manifest-path=") {
            return true;
        }
        if text == "-" {
            return true;
        }
        if text.starts_with('-') {
            if matches!(
                text,
                "-o" | "-L"
                    | "-l"
                    | "--emit"
                    | "--target"
                    | "--linker"
                    | "--stdlib"
                    | "--module-root"
                    | "--package-root"
                    | "--package-name"
                    | "--features"
            ) {
                iterator.next();
            }
            continue;
        }
        return true;
    }
    false
}

fn usage() -> String {
    concat!(
        "usage: staple <command> [options]\n",
        "\n",
        "commands:\n",
        "  new <name>     create a new package\n",
        "  build          compile the current package to a native executable\n",
        "  check [file]   type-check the current package, or a single source file\n",
        "  run [file]     build and run the current package, or a single source file\n",
        "  expand <file>  print a source file after macro expansion\n",
        "  compile <file> emit LLVM IR, an object file, or an executable\n",
        "  fmt [file|-]   format a source file in place (or standard input)\n",
        "  lsp            run the language server on stdin/stdout\n",
        "\n",
        "run `staple <command> --help` for command options",
    )
    .to_owned()
}
