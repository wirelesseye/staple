use std::ffi::OsString;

use crate::Outcome;
use crate::compile;

/// `staple expand <file>` prints one source file after macro expansion. It is a
/// thin front-end over the shared compiler engine: the arguments already begin
/// with the `expand` verb that `compile::run` dispatches on, and a missing input
/// file is reported there with the `expand` usage.
pub fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Outcome, String> {
    compile::run(arguments)
}
