use std::ffi::OsString;
use std::path::{Path, PathBuf};

use staple_syntax::format_source;

use crate::Outcome;
use crate::compile;
use crate::package;

/// `staple fmt [--check] [--manifest-path <path>] [<file|->]` formats Staple
/// source without expanding or resolving macros.
///
/// With a file argument (or `-`) it defers to the shared compiler engine, whose
/// `fmt` mode rewrites that single input. With no input it formats every `.sta`
/// file in the current package instead, discovering the package the same way
/// `staple check` does.
pub fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Outcome, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    match scan(&arguments[1..]) {
        Invocation::Delegate => compile::run(arguments),
        Invocation::Package { manifest, check } => format_package(manifest.as_deref(), check),
    }
}

/// Which formatting mode the arguments select. Anything the package branch does
/// not understand (a file, `-`, `--help`, an unknown option) is handed to
/// `compile::run` unchanged so it produces the canonical behaviour or error.
enum Invocation {
    Delegate,
    Package { manifest: Option<PathBuf>, check: bool },
}

fn scan(rest: &[OsString]) -> Invocation {
    let mut manifest = None;
    let mut check = false;
    let mut arguments = rest.iter();
    while let Some(argument) = arguments.next() {
        let text = argument.to_string_lossy();
        if argument == "-" || !text.starts_with('-') {
            return Invocation::Delegate;
        }
        if text == "--check" {
            check = true;
        } else if text == "--manifest-path" {
            match arguments.next() {
                Some(value) => manifest = Some(PathBuf::from(value)),
                None => return Invocation::Delegate,
            }
        } else if let Some(value) = text.strip_prefix("--manifest-path=") {
            manifest = Some(PathBuf::from(value));
        } else {
            return Invocation::Delegate;
        }
    }
    Invocation::Package { manifest, check }
}

fn format_package(manifest: Option<&Path>, check: bool) -> Result<Outcome, String> {
    let current_directory = std::env::current_dir()
        .map_err(|error| format!("could not determine current directory: {error}"))?;
    let manifest = package::discover_manifest(manifest, &current_directory)?;
    let graph = staple_project::load_package_graph(&manifest)?;
    let source_root = graph.root_package().source_root().to_owned();

    let mut files = Vec::new();
    collect_staple_files(&source_root, &mut files);
    files.sort();

    let mut unformatted = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file)
            .map_err(|error| format!("could not read `{}`: {error}", file.display()))?;
        let formatted =
            format_source(&source).map_err(|error| format!("{}: {error}", file.display()))?;
        if formatted == source {
            continue;
        }
        if check {
            unformatted.push(file.display().to_string());
        } else {
            compile::atomic_write(file, formatted.as_bytes())?;
        }
    }

    if !unformatted.is_empty() {
        return Ok(Outcome::FormatMismatch(unformatted));
    }
    Ok(Outcome::Completed(None))
}

fn collect_staple_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_staple_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "sta") {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::Outcome;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "staple-fmt-package-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(root.join("src/inner")).unwrap();
            std::fs::write(root.join("staple.kdl"), "package \"fixture\"\n").unwrap();
            std::fs::write(root.join("src/main.sta"), "let   answer=42\n").unwrap();
            std::fs::write(root.join("src/inner/helper.sta"), "pub mod\npub let   x=1\n").unwrap();
            Self { root }
        }

        fn manifest(&self) -> PathBuf {
            self.root.join("staple.kdl")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn formats_every_source_file_in_the_package() {
        let fixture = Fixture::new();
        let outcome = run([
            "fmt".into(),
            "--manifest-path".into(),
            fixture.manifest().into_os_string(),
        ])
        .expect("package fmt should succeed");
        assert!(matches!(outcome, Outcome::Completed(None)));
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("src/main.sta")).unwrap(),
            "let answer = 42\n"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("src/inner/helper.sta")).unwrap(),
            "pub mod\npub let x = 1\n"
        );
    }

    #[test]
    fn check_reports_every_unformatted_file_without_writing() {
        let fixture = Fixture::new();
        let outcome = run([
            "fmt".into(),
            "--check".into(),
            "--manifest-path".into(),
            fixture.manifest().into_os_string(),
        ])
        .expect("package fmt --check should succeed");
        let Outcome::FormatMismatch(unformatted) = outcome else {
            panic!("unformatted package should report a mismatch");
        };
        assert_eq!(unformatted.len(), 2);
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("src/main.sta")).unwrap(),
            "let   answer=42\n"
        );
    }

    #[test]
    fn check_passes_once_the_package_is_formatted() {
        let fixture = Fixture::new();
        run([
            "fmt".into(),
            "--manifest-path".into(),
            fixture.manifest().into_os_string(),
        ])
        .expect("package fmt should succeed");
        let outcome = run([
            "fmt".into(),
            "--check".into(),
            "--manifest-path".into(),
            fixture.manifest().into_os_string(),
        ])
        .expect("package fmt --check should succeed");
        assert!(matches!(outcome, Outcome::Completed(None)));
    }

    #[test]
    fn a_named_file_still_formats_just_that_file() {
        let fixture = Fixture::new();
        let outcome = run([
            "fmt".into(),
            fixture.root.join("src/main.sta").into_os_string(),
        ])
        .expect("single-file fmt should succeed");
        assert!(matches!(outcome, Outcome::Completed(None)));
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("src/main.sta")).unwrap(),
            "let answer = 42\n"
        );
        // The rest of the package was left untouched.
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("src/inner/helper.sta")).unwrap(),
            "pub mod\npub let   x=1\n"
        );
    }
}
