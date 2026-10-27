use std::{ffi::OsStr, path::Path, process::Command};

/// Link native test objects with the standard library's platform dependencies.
/// Libraries follow objects so linkers can resolve their undefined symbols.
pub fn linker(objects: &[impl AsRef<OsStr>], output: &Path) -> Command {
    let mut command = Command::new("cc");
    command.args(objects).arg("-o").arg(output);
    if cfg!(target_os = "linux") {
        command.arg("-lm");
    }
    command
}
