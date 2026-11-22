use std::ffi::OsString;
use std::process::ExitCode;

pub(crate) fn run(arguments: &[OsString]) -> ExitCode {
    if !arguments.is_empty() {
        eprintln!("staple: `__macro-server` takes no arguments");
        return ExitCode::FAILURE;
    }
    match staple_compiler::macro_runtime::server::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("staple: {message}");
            ExitCode::FAILURE
        }
    }
}
