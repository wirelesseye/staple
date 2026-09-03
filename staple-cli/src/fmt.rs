use std::ffi::OsString;

use crate::Outcome;
use crate::compile;

/// `staple fmt [--check] <file|->` formats one source file. It delegates to the
/// shared compiler engine, whose `fmt` mode neither loads the standard library
/// nor resolves macros.
pub fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Outcome, String> {
    compile::run(arguments)
}
