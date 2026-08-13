use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

use kdl::{KdlDocument, KdlNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Build,
    Check,
    Run,
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    mode: Mode,
    manifest_path: Option<PathBuf>,
    output: Option<PathBuf>,
    target: Option<String>,
    linker: Option<OsString>,
    standard_library: Option<PathBuf>,
    library_paths: Vec<PathBuf>,
    libraries: Vec<OsString>,
    program_arguments: Vec<OsString>,
}

#[derive(Debug, PartialEq, Eq)]
struct Package {
    name: String,
    manifest: PathBuf,
    directory: PathBuf,
    root: PathBuf,
    entry: PathBuf,
}

enum Outcome {
    Completed,
    Executed(ExitStatus),
}

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1)) {
        Ok(Outcome::Completed) => ExitCode::SUCCESS,
        Ok(Outcome::Executed(status)) => exit_code(status),
        Err(error) => {
            eprintln!("binder: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Outcome, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if matches!(arguments.as_slice(), [argument] if argument == "-h" || argument == "--help") {
        println!("{}", usage());
        return Ok(Outcome::Completed);
    }
    if matches!(arguments.as_slice(), [command, argument] if (argument == "-h" || argument == "--help") && matches!(command.to_str(), Some("build" | "check" | "run")))
    {
        println!("{}", command_usage(arguments[0].to_str().unwrap()));
        return Ok(Outcome::Completed);
    }

    let options = parse_options(arguments)?;
    let current_directory = std::env::current_dir()
        .map_err(|error| format!("could not determine current directory: {error}"))?;
    let manifest = discover_manifest(options.manifest_path.as_deref(), &current_directory)?;
    let package = load_package(&manifest)?;
    let stapler = locate_stapler()?;

    match options.mode {
        Mode::Check => {
            let status = Command::new(&stapler)
                .args(stapler_arguments(&package, &options, None))
                .status()
                .map_err(|error| compiler_start_error(&stapler, error))?;
            Ok(Outcome::Executed(status))
        }
        Mode::Build | Mode::Run => {
            let output = output_path(&package, &options)?;
            if options.output.is_none() {
                std::fs::create_dir_all(
                    output
                        .parent()
                        .expect("default build output always has a parent"),
                )
                .map_err(|error| {
                    format!(
                        "could not create build directory `{}`: {error}",
                        output.parent().unwrap().display()
                    )
                })?;
            }
            let status = Command::new(&stapler)
                .args(stapler_arguments(&package, &options, Some(&output)))
                .status()
                .map_err(|error| compiler_start_error(&stapler, error))?;
            if !status.success() || options.mode == Mode::Build {
                return Ok(Outcome::Executed(status));
            }
            let status = Command::new(&output)
                .args(&options.program_arguments)
                .status()
                .map_err(|error| {
                    format!(
                        "could not run package executable `{}`: {error}",
                        output.display()
                    )
                })?;
            Ok(Outcome::Executed(status))
        }
    }
}

fn parse_options(arguments: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut arguments = arguments.into_iter();
    let mode = match arguments.next().as_deref().and_then(OsStr::to_str) {
        Some("build") => Mode::Build,
        Some("check") => Mode::Check,
        Some("run") => Mode::Run,
        _ => return Err(usage()),
    };
    let mut options = Options {
        mode,
        manifest_path: None,
        output: None,
        target: None,
        linker: None,
        standard_library: None,
        library_paths: Vec::new(),
        libraries: Vec::new(),
        program_arguments: Vec::new(),
    };

    while let Some(argument) = arguments.next() {
        if argument == "--" {
            if mode != Mode::Run {
                return Err(format!(
                    "`--` is only supported by `binder run`\n{}",
                    mode_usage(mode)
                ));
            }
            options.program_arguments.extend(arguments);
            break;
        }
        if argument == "--manifest-path" {
            set_once_path(
                &mut options.manifest_path,
                next_value(&mut arguments, "--manifest-path")?,
                "--manifest-path",
            )?;
            continue;
        }
        if argument == "-o" {
            set_once_path(&mut options.output, next_value(&mut arguments, "-o")?, "-o")?;
            continue;
        }
        if argument == "--target" {
            set_once(
                &mut options.target,
                utf8_value(next_value(&mut arguments, "--target")?, "--target")?,
                "--target",
            )?;
            continue;
        }
        if argument == "--linker" {
            set_once(
                &mut options.linker,
                next_value(&mut arguments, "--linker")?,
                "--linker",
            )?;
            continue;
        }
        if argument == "--stdlib" {
            set_once_path(
                &mut options.standard_library,
                next_value(&mut arguments, "--stdlib")?,
                "--stdlib",
            )?;
            continue;
        }
        if argument == "-L" {
            options
                .library_paths
                .push(PathBuf::from(next_value(&mut arguments, "-L")?));
            continue;
        }
        if argument == "-l" {
            options.libraries.push(next_value(&mut arguments, "-l")?);
            continue;
        }

        let text = argument.to_string_lossy();
        if let Some(value) = text.strip_prefix("--manifest-path=") {
            set_once_path(
                &mut options.manifest_path,
                OsString::from(value),
                "--manifest-path",
            )?;
        } else if let Some(value) = text.strip_prefix("--target=") {
            set_once(&mut options.target, value.to_owned(), "--target")?;
        } else if let Some(value) = text.strip_prefix("--linker=") {
            set_once(&mut options.linker, OsString::from(value), "--linker")?;
        } else if let Some(value) = text.strip_prefix("--stdlib=") {
            set_once_path(
                &mut options.standard_library,
                OsString::from(value),
                "--stdlib",
            )?;
        } else if text.starts_with("-L") && text.len() > 2 {
            options.library_paths.push(PathBuf::from(&text[2..]));
        } else if text.starts_with("-l") && text.len() > 2 {
            options.libraries.push(OsString::from(&text[2..]));
        } else {
            return Err(format!("unknown option `{text}`\n{}", mode_usage(mode)));
        }
    }

    validate_options(&options)?;
    Ok(options)
}

fn validate_options(options: &Options) -> Result<(), String> {
    if options.mode == Mode::Run {
        if options.output.is_some() {
            return Err("`-o` is not supported by `binder run`".to_owned());
        }
        if options.target.is_some() {
            return Err(
                "`--target` is not supported by `binder run`; programs run on the host target"
                    .to_owned(),
            );
        }
    }
    if options.mode == Mode::Check {
        if options.output.is_some() {
            return Err("`-o` is not supported by `binder check`".to_owned());
        }
        if options.target.is_some() {
            return Err("`--target` is not supported by `binder check`".to_owned());
        }
        if options.linker.is_some()
            || !options.library_paths.is_empty()
            || !options.libraries.is_empty()
        {
            return Err("linker options are not supported by `binder check`".to_owned());
        }
    }
    Ok(())
}

fn discover_manifest(explicit: Option<&Path>, current: &Path) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            current.join(path)
        };
        return canonical_file(&path, "manifest");
    }

    let mut directory = std::fs::canonicalize(current).map_err(|error| {
        format!(
            "could not resolve current directory `{}`: {error}",
            current.display()
        )
    })?;
    loop {
        let candidate = directory.join("binder.kdl");
        if candidate.is_file() {
            return canonical_file(&candidate, "manifest");
        }
        if !directory.pop() {
            return Err(
                "could not find `binder.kdl` in the current directory or any parent directory"
                    .to_owned(),
            );
        }
    }
}

fn load_package(manifest: &Path) -> Result<Package, String> {
    let manifest = canonical_file(manifest, "manifest")?;
    let manifest = manifest.as_path();
    let source = std::fs::read_to_string(manifest)
        .map_err(|error| format!("could not read `{}`: {error}", manifest.display()))?;
    let document = source
        .parse::<KdlDocument>()
        .map_err(|error| format!("could not parse `{}`: {error}", manifest.display()))?;
    let [node] = document.nodes() else {
        return Err(format!(
            "{} must contain exactly one `package` node",
            manifest.display()
        ));
    };
    if node.name().value() != "package" {
        return Err(format!(
            "{}: unknown top-level node `{}`; expected `package`",
            manifest.display(),
            node.name().value()
        ));
    }
    let name = package_name(node, manifest)?;
    let mut root = None;
    let mut entry = None;
    if let Some(children) = node.children() {
        for child in children.nodes() {
            match child.name().value() {
                "root" => parse_path_child(child, manifest, &mut root)?,
                "entry" => parse_path_child(child, manifest, &mut entry)?,
                unknown => {
                    let detail = if unknown == "dependencies" {
                        "dependencies are not supported in Binder v1".to_owned()
                    } else {
                        format!("unknown package node `{unknown}`")
                    };
                    return Err(format!("{}: {detail}", manifest.display()));
                }
            }
        }
    }

    let directory = manifest
        .parent()
        .expect("canonical manifest path always has a parent")
        .to_owned();
    let root = resolve_relative_directory(
        &directory,
        root.as_deref().unwrap_or_else(|| Path::new("src")),
        "root",
    )?;
    let entry_setting = entry.as_deref().unwrap_or_else(|| Path::new("main.sta"));
    if entry_setting.extension() != Some(OsStr::new("sta")) {
        return Err(format!(
            "{}: entry `{}` must use the `.sta` extension",
            manifest.display(),
            entry_setting.display()
        ));
    }
    let entry = resolve_relative_file(&root, entry_setting, "entry")?;

    Ok(Package {
        name,
        manifest: manifest.to_owned(),
        directory,
        root,
        entry,
    })
}

fn package_name(node: &KdlNode, manifest: &Path) -> Result<String, String> {
    let [entry] = node.entries() else {
        return Err(format!(
            "{}: `package` requires exactly one string name",
            manifest.display()
        ));
    };
    if entry.name().is_some() {
        return Err(format!(
            "{}: package properties are not supported",
            manifest.display()
        ));
    }
    let name = entry
        .value()
        .as_string()
        .ok_or_else(|| format!("{}: `package` name must be a string", manifest.display()))?;
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        || !characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(format!(
            "{}: package name `{name}` must start with an ASCII letter and contain only letters, numbers, `_`, or `-`",
            manifest.display()
        ));
    }
    Ok(name.to_owned())
}

fn parse_path_child(
    node: &KdlNode,
    manifest: &Path,
    destination: &mut Option<PathBuf>,
) -> Result<(), String> {
    let name = node.name().value();
    if destination.is_some() {
        return Err(format!("{}: duplicate `{name}` node", manifest.display()));
    }
    if node.children().is_some() {
        return Err(format!(
            "{}: `{name}` cannot contain child nodes",
            manifest.display()
        ));
    }
    let [entry] = node.entries() else {
        return Err(format!(
            "{}: `{name}` requires exactly one string path",
            manifest.display()
        ));
    };
    if entry.name().is_some() {
        return Err(format!(
            "{}: `{name}` properties are not supported",
            manifest.display()
        ));
    }
    let value = entry
        .value()
        .as_string()
        .ok_or_else(|| format!("{}: `{name}` path must be a string", manifest.display()))?;
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return Err(format!(
            "{}: `{name}` path must be relative",
            manifest.display()
        ));
    }
    *destination = Some(path);
    Ok(())
}

fn resolve_relative_directory(
    parent: &Path,
    relative: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    if relative.is_absolute() {
        return Err(format!("`{label}` path must be relative"));
    }
    let resolved = std::fs::canonicalize(parent.join(relative)).map_err(|error| {
        format!(
            "could not resolve {label} `{}`: {error}",
            parent.join(relative).display()
        )
    })?;
    if !resolved.starts_with(parent) {
        return Err(format!(
            "{label} `{}` escapes `{}`",
            relative.display(),
            parent.display()
        ));
    }
    if !resolved.is_dir() {
        return Err(format!(
            "{label} `{}` is not a directory",
            resolved.display()
        ));
    }
    Ok(resolved)
}

fn resolve_relative_file(parent: &Path, relative: &Path, label: &str) -> Result<PathBuf, String> {
    if relative.is_absolute() {
        return Err(format!("`{label}` path must be relative"));
    }
    let resolved = std::fs::canonicalize(parent.join(relative)).map_err(|error| {
        format!(
            "could not resolve {label} `{}`: {error}",
            parent.join(relative).display()
        )
    })?;
    if !resolved.starts_with(parent) {
        return Err(format!(
            "{label} `{}` escapes `{}`",
            relative.display(),
            parent.display()
        ));
    }
    if !resolved.is_file() {
        return Err(format!("{label} `{}` is not a file", resolved.display()));
    }
    Ok(resolved)
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(path)
        .map_err(|error| format!("could not resolve {label} `{}`: {error}", path.display()))?;
    if !path.is_file() {
        return Err(format!("{label} `{}` is not a file", path.display()));
    }
    Ok(path)
}

fn locate_stapler() -> Result<OsString, String> {
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let sibling = directory.join(format!("stpl{}", std::env::consts::EXE_SUFFIX));
        if sibling.is_file() {
            return Ok(sibling.into_os_string());
        }
    }
    Ok(OsString::from("stpl"))
}

fn stapler_arguments(package: &Package, options: &Options, output: Option<&Path>) -> Vec<OsString> {
    let mut arguments = Vec::new();
    if options.mode == Mode::Check {
        arguments.push("check".into());
    }
    arguments.push("--module-root".into());
    arguments.push(package.root.clone().into_os_string());
    if let Some(output) = output {
        arguments.extend([OsString::from("--emit"), OsString::from("exe")]);
        arguments.push("-o".into());
        arguments.push(output.as_os_str().to_owned());
    }
    if let Some(target) = &options.target {
        arguments.push("--target".into());
        arguments.push(target.into());
    }
    if let Some(linker) = &options.linker {
        arguments.push("--linker".into());
        arguments.push(linker.clone());
    }
    if let Some(root) = &options.standard_library {
        arguments.push("--stdlib".into());
        arguments.push(root.clone().into_os_string());
    }
    for path in &options.library_paths {
        arguments.push("-L".into());
        arguments.push(path.clone().into_os_string());
    }
    for library in &options.libraries {
        arguments.push("-l".into());
        arguments.push(library.clone());
    }
    arguments.push(package.entry.clone().into_os_string());
    arguments
}

fn output_path(package: &Package, options: &Options) -> Result<PathBuf, String> {
    if let Some(output) = &options.output {
        return Ok(output.clone());
    }
    let mut name = OsString::from(&package.name);
    name.push(std::env::consts::EXE_SUFFIX);
    Ok(package.directory.join("build").join(name))
}

fn compiler_start_error(compiler: &OsStr, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "could not find Stapler (`stpl`) beside Binder or on PATH; install the Staple toolchain ({error})"
        )
    } else {
        format!("could not start `{}`: {error}", compiler.to_string_lossy())
    }
}

fn next_value(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<OsString, String> {
    arguments
        .next()
        .ok_or_else(|| format!("expected a value after `{option}`"))
}

fn utf8_value(value: OsString, option: &str) -> Result<String, String> {
    value
        .into_string()
        .map_err(|_| format!("`{option}` requires UTF-8 text"))
}

fn set_once<T>(destination: &mut Option<T>, value: T, option: &str) -> Result<(), String> {
    if destination.replace(value).is_some() {
        return Err(format!("option `{option}` may only be specified once"));
    }
    Ok(())
}

fn set_once_path(
    destination: &mut Option<PathBuf>,
    value: OsString,
    option: &str,
) -> Result<(), String> {
    set_once(destination, PathBuf::from(value), option)
}

fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map(ExitCode::from)
        .unwrap_or(ExitCode::FAILURE)
}

fn usage() -> String {
    concat!(
        "usage: binder <build|check|run> [options]\n",
        "\n",
        "commands:\n",
        "  build  compile the package to a native executable\n",
        "  check  load, resolve, and type-check the package\n",
        "  run    build and run the package\n",
        "\n",
        "run `binder <command> --help` for command options",
    )
    .to_owned()
}

fn command_usage(command: &str) -> String {
    match command {
        "build" => concat!(
            "usage: binder build [options]\n\n",
            "  --manifest-path <path>  use a specific binder.kdl\n",
            "  -o <path>                output executable (default: build/<package>)\n",
            "  --target <triple>        LLVM target triple\n",
            "  --linker <command>       linker driver\n",
            "  --stdlib <path>          Staple standard-library root\n",
            "  -L <path>                add a linker search path\n",
            "  -l <name>                link a library",
        ),
        "check" => concat!(
            "usage: binder check [options]\n\n",
            "  --manifest-path <path>  use a specific binder.kdl\n",
            "  --stdlib <path>          Staple standard-library root",
        ),
        "run" => concat!(
            "usage: binder run [options] [-- <arguments>...]\n\n",
            "  --manifest-path <path>  use a specific binder.kdl\n",
            "  --linker <command>       linker driver\n",
            "  --stdlib <path>          Staple standard-library root\n",
            "  -L <path>                add a linker search path\n",
            "  -l <name>                link a library\n",
            "  --                        pass remaining arguments to the program",
        ),
        _ => unreachable!(),
    }
    .to_owned()
}

fn mode_usage(mode: Mode) -> String {
    command_usage(match mode {
        Mode::Build => "build",
        Mode::Check => "check",
        Mode::Run => "run",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
    }

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "binder-test-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/main.sta"), "42\n").unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            Self { root }
        }

        fn manifest(&self, source: &str) -> PathBuf {
            let path = self.root.join("binder.kdl");
            std::fs::write(&path, source).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn parses_minimal_manifest_defaults() {
        let fixture = Fixture::new();
        let manifest = fixture.manifest("package \"hello\"\n");
        let package = load_package(&manifest).expect("manifest should parse");

        assert_eq!(package.name, "hello");
        assert_eq!(package.root, fixture.root.join("src"));
        assert_eq!(package.entry, fixture.root.join("src/main.sta"));
    }

    #[test]
    fn parses_explicit_root_and_entry() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.root.join("source/bin")).unwrap();
        std::fs::write(fixture.root.join("source/bin/app.sta"), "42\n").unwrap();
        let manifest = fixture
            .manifest("package \"hello\" {\n  root \"source\"\n  entry \"bin/app.sta\"\n}\n");
        let package = load_package(&manifest).expect("manifest should parse");

        assert_eq!(package.root, fixture.root.join("source"));
        assert_eq!(package.entry, fixture.root.join("source/bin/app.sta"));
    }

    #[test]
    fn rejects_unknown_duplicate_and_unsafe_manifest_values() {
        for source in [
            "package \"hello\" { dependencies {} }\n",
            "package \"hello\" { root \"src\"; root \"src\" }\n",
            "package \"../hello\"\n",
            "project \"hello\"\n",
        ] {
            let fixture = Fixture::new();
            let error = load_package(&fixture.manifest(source))
                .expect_err("invalid manifest should be rejected");
            assert!(!error.is_empty());
        }
    }

    #[test]
    fn rejects_malformed_and_missing_project_inputs() {
        let malformed = Fixture::new();
        let error = load_package(&malformed.manifest("package \"hello\" {\n"))
            .expect_err("malformed KDL should be rejected");
        assert!(error.contains("could not parse"));

        let missing_root = Fixture::new();
        std::fs::remove_dir_all(missing_root.root.join("src")).unwrap();
        let error = load_package(&missing_root.manifest("package \"hello\"\n"))
            .expect_err("missing default root should be rejected");
        assert!(error.contains("could not resolve root"));

        let missing_entry = Fixture::new();
        std::fs::remove_file(missing_entry.root.join("src/main.sta")).unwrap();
        let error = load_package(&missing_entry.manifest("package \"hello\"\n"))
            .expect_err("missing default entry should be rejected");
        assert!(error.contains("could not resolve entry"));
    }

    #[test]
    fn rejects_paths_that_escape_the_package() {
        let fixture = Fixture::new();
        let outside = fixture.root.parent().unwrap().join("main.sta");
        std::fs::write(&outside, "42\n").unwrap();
        let error = load_package(
            &fixture.manifest("package \"hello\" { root \"src\"; entry \"../../main.sta\" }\n"),
        )
        .expect_err("escaping entry should fail");
        let _ = std::fs::remove_file(outside);

        assert!(error.contains("escapes") || error.contains("could not resolve"));
    }

    #[test]
    fn discovers_the_nearest_parent_manifest() {
        let fixture = Fixture::new();
        let manifest = fixture.manifest("package \"hello\"\n");
        let nested = fixture.root.join("src/nested/deeper");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(discover_manifest(None, &nested).unwrap(), manifest);
    }

    #[test]
    fn parses_build_check_and_run_options() {
        let build = parse_options([
            "build".into(),
            "--manifest-path=binder.kdl".into(),
            "--target".into(),
            "host".into(),
            "-Lnative".into(),
            "-lm".into(),
        ])
        .unwrap();
        assert_eq!(build.mode, Mode::Build);
        assert_eq!(build.target.as_deref(), Some("host"));

        let check = parse_options(["check".into(), "--stdlib=stdlib".into()]).unwrap();
        assert_eq!(check.mode, Mode::Check);

        let run =
            parse_options(["run".into(), "--".into(), "first".into(), "--second".into()]).unwrap();
        assert_eq!(run.program_arguments, ["first", "--second"]);
    }

    #[test]
    fn constructs_exact_stapler_commands() {
        let fixture = Fixture::new();
        let package = load_package(&fixture.manifest("package \"hello\"\n")).unwrap();
        let options = parse_options(["build".into(), "--stdlib=stdlib".into()]).unwrap();
        let output = fixture.root.join("build/hello");
        let arguments = stapler_arguments(&package, &options, Some(&output));

        assert_eq!(arguments[0], "--module-root");
        assert_eq!(arguments[1], package.root);
        assert_eq!(
            arguments[2..6],
            ["--emit", "exe", "-o", output.to_str().unwrap()]
        );
        assert_eq!(arguments.last().unwrap(), package.entry.as_os_str());
    }

    #[test]
    fn rejects_options_that_do_not_apply_to_a_command() {
        assert!(parse_options(["run".into(), "--target=other".into()]).is_err());
        assert!(parse_options(["check".into(), "-lm".into()]).is_err());
        assert!(parse_options(["check".into(), "-o".into(), "out".into()]).is_err());
    }
}
