use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Build,
    Check,
    New,
    Run,
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    mode: Mode,
    package_name: Option<String>,
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
    entry: Option<PathBuf>,
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
    if matches!(arguments.as_slice(), [command, argument] if (argument == "-h" || argument == "--help") && matches!(command.to_str(), Some("build" | "check" | "new" | "run")))
    {
        println!("{}", command_usage(arguments[0].to_str().unwrap()));
        return Ok(Outcome::Completed);
    }

    let options = parse_options(arguments)?;
    let current_directory = std::env::current_dir()
        .map_err(|error| format!("could not determine current directory: {error}"))?;
    if options.mode == Mode::New {
        let name = options
            .package_name
            .as_deref()
            .expect("new command always has a package name");
        let destination = create_project(&current_directory, name)?;
        println!("Created package `{name}` at `{}`", destination.display());
        return Ok(Outcome::Completed);
    }
    let manifest = discover_manifest(options.manifest_path.as_deref(), &current_directory)?;
    let package = load_package(&manifest)?;
    validate_package_options(&package, &options)?;
    let stapler = locate_stapler()?;

    if package.entry.is_none() {
        let status = Command::new(&stapler)
            .args(stapler_arguments(&package, &options, None))
            .status()
            .map_err(|error| compiler_start_error(&stapler, error))?;
        return Ok(Outcome::Executed(status));
    }

    match options.mode {
        Mode::Check => {
            let status = Command::new(&stapler)
                .args(stapler_arguments(&package, &options, None))
                .status()
                .map_err(|error| compiler_start_error(&stapler, error))?;
            Ok(Outcome::Executed(status))
        }
        Mode::New => unreachable!("new command returns before compiler invocation"),
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
        Some("new") => Mode::New,
        Some("run") => Mode::Run,
        _ => return Err(usage()),
    };
    let mut options = Options {
        mode,
        package_name: None,
        manifest_path: None,
        output: None,
        target: None,
        linker: None,
        standard_library: None,
        library_paths: Vec::new(),
        libraries: Vec::new(),
        program_arguments: Vec::new(),
    };

    if mode == Mode::New {
        let name = arguments
            .next()
            .ok_or_else(|| command_usage("new"))?
            .into_string()
            .map_err(|_| "`binder new` requires a UTF-8 package name".to_owned())?;
        validate_package_name(&name)?;
        if let Some(argument) = arguments.next() {
            return Err(format!(
                "unexpected argument `{}`\n{}",
                argument.to_string_lossy(),
                command_usage("new")
            ));
        }
        options.package_name = Some(name);
        return Ok(options);
    }

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

fn validate_package_options(package: &Package, options: &Options) -> Result<(), String> {
    if package.entry.is_some() {
        return Ok(());
    }
    if options.mode == Mode::Run {
        return Err(format!(
            "library package `{}` has no entry module and cannot be run",
            package.name
        ));
    }
    if options.output.is_some()
        || options.target.is_some()
        || options.linker.is_some()
        || !options.library_paths.is_empty()
        || !options.libraries.is_empty()
    {
        return Err("artifact and linker options are not supported when building a library without an entry module".to_owned());
    }
    Ok(())
}

fn create_project(parent: &Path, name: &str) -> Result<PathBuf, String> {
    validate_package_name(name)?;
    let destination = parent.join(name);
    std::fs::create_dir(&destination).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!("destination `{}` already exists", destination.display())
        } else {
            format!(
                "could not create project directory `{}`: {error}",
                destination.display()
            )
        }
    })?;

    let creation = (|| {
        let source_directory = destination.join("src");
        std::fs::create_dir(&source_directory).map_err(|error| {
            format!(
                "could not create source directory `{}`: {error}",
                source_directory.display()
            )
        })?;
        write_project_file(
            &destination.join("binder.kdl"),
            &format!("package \"{name}\"\n"),
        )?;
        write_project_file(&destination.join(".gitignore"), "/build\n")?;
        write_project_file(
            &source_directory.join("main.sta"),
            "use std.io.println\n\nprintln \"Hello, world!\"\n",
        )?;
        Ok(())
    })();

    if let Err(error) = creation {
        if let Err(cleanup) = std::fs::remove_dir_all(&destination) {
            return Err(format!(
                "{error}; also could not remove incomplete project `{}`: {cleanup}",
                destination.display()
            ));
        }
        return Err(error);
    }

    Ok(destination)
}

fn write_project_file(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents)
        .map_err(|error| format!("could not write `{}`: {error}", path.display()))
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
    let graph = binder::load_package_graph(manifest)?;
    let package = graph.root_package();
    Ok(Package {
        name: package.name.clone(),
        manifest: package.manifest.clone(),
        directory: package.directory.clone(),
        root: package.root.clone(),
        entry: package.entry.clone(),
    })
}

fn validate_package_name(name: &str) -> Result<(), String> {
    binder::validate_package_name(name)
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
    if options.mode == Mode::Check || package.entry.is_none() {
        arguments.push("check".into());
    }
    arguments.push("--manifest-path".into());
    arguments.push(package.manifest.clone().into_os_string());
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
        "usage: binder <build|check|new|run> [options]\n",
        "\n",
        "commands:\n",
        "  build  compile the package to a native executable\n",
        "  check  load, resolve, and type-check the package\n",
        "  new    create a new package\n",
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
        "new" => concat!(
            "usage: binder new <name>\n\n",
            "Create a new package in a directory named <name>.",
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
        Mode::New => "new",
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
        assert_eq!(package.root, fixture.root.join("src/root.sta"));
        assert_eq!(package.entry, Some(fixture.root.join("src/main.sta")));
    }

    #[test]
    fn parses_explicit_root_and_entry() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.root.join("source/bin")).unwrap();
        std::fs::write(fixture.root.join("source/bin/app.sta"), "42\n").unwrap();
        let manifest = fixture.manifest(
            "package \"hello\" {\n  root \"source/root.sta\"\n  entry \"source/bin/app.sta\"\n}\n",
        );
        let package = load_package(&manifest).expect("manifest should parse");

        assert_eq!(package.root, fixture.root.join("source/root.sta"));
        assert_eq!(package.entry, Some(fixture.root.join("source/bin/app.sta")));
    }

    #[test]
    fn rejects_unknown_duplicate_and_unsafe_manifest_values() {
        for source in [
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
        let error =
            load_package(&fixture.manifest(
                "package \"hello\" { root \"src/root.sta\"; entry \"../../main.sta\" }\n",
            ))
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
    fn parses_new_with_one_safe_package_name() {
        let options = parse_options(["new".into(), "hello-world".into()]).unwrap();

        assert_eq!(options.mode, Mode::New);
        assert_eq!(options.package_name.as_deref(), Some("hello-world"));
        assert!(command_usage("new").starts_with("usage: binder new <name>"));
    }

    #[test]
    fn rejects_invalid_new_arguments() {
        for arguments in [
            vec!["new"],
            vec!["new", "../hello"],
            vec!["new", "123"],
            vec!["new", "hello", "extra"],
            vec!["new", "--manifest-path=somewhere"],
        ] {
            assert!(
                parse_options(arguments.into_iter().map(OsString::from)).is_err(),
                "arguments should be rejected"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn rejects_a_non_utf8_new_package_name() {
        use std::os::unix::ffi::OsStringExt;

        let error = parse_options([OsString::from("new"), OsString::from_vec(vec![b'h', 0xff])])
            .expect_err("non-UTF-8 name should be rejected");

        assert!(error.contains("UTF-8 package name"));
    }

    #[test]
    fn creates_a_loadable_project_with_exact_scaffold_contents() {
        let fixture = Fixture::new();
        let destination =
            create_project(&fixture.root, "hello-world").expect("new project should be created");

        assert_eq!(destination, fixture.root.join("hello-world"));
        assert_eq!(
            std::fs::read_to_string(destination.join("binder.kdl")).unwrap(),
            "package \"hello-world\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join(".gitignore")).unwrap(),
            "/build\n"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("src/main.sta")).unwrap(),
            "use std.io.println\n\nprintln \"Hello, world!\"\n"
        );

        let package =
            load_package(&destination.join("binder.kdl")).expect("generated manifest should load");
        assert_eq!(package.name, "hello-world");
        assert_eq!(package.entry, Some(destination.join("src/main.sta")));
    }

    #[test]
    fn refuses_every_existing_destination_without_modifying_it() {
        let fixture = Fixture::new();
        let existing_file = fixture.root.join("existing-file");
        std::fs::write(&existing_file, "keep me").unwrap();
        let empty_directory = fixture.root.join("empty-directory");
        std::fs::create_dir(&empty_directory).unwrap();
        let full_directory = fixture.root.join("full-directory");
        std::fs::create_dir(&full_directory).unwrap();
        std::fs::write(full_directory.join("keep.txt"), "keep me too").unwrap();

        for name in ["existing-file", "empty-directory", "full-directory"] {
            let error = create_project(&fixture.root, name)
                .expect_err("existing destination should be rejected");
            assert!(error.contains("already exists"));
        }
        assert_eq!(std::fs::read_to_string(existing_file).unwrap(), "keep me");
        assert!(empty_directory.read_dir().unwrap().next().is_none());
        assert_eq!(
            std::fs::read_to_string(full_directory.join("keep.txt")).unwrap(),
            "keep me too"
        );
    }

    #[test]
    fn constructs_exact_stapler_commands() {
        let fixture = Fixture::new();
        let package = load_package(&fixture.manifest("package \"hello\"\n")).unwrap();
        let options = parse_options(["build".into(), "--stdlib=stdlib".into()]).unwrap();
        let output = fixture.root.join("build/hello");
        let arguments = stapler_arguments(&package, &options, Some(&output));

        assert_eq!(arguments[0], "--manifest-path");
        assert_eq!(arguments[1], package.manifest);
        assert_eq!(
            arguments[2..6],
            ["--emit", "exe", "-o", output.to_str().unwrap()]
        );
        assert_eq!(arguments[6..], ["--stdlib", "stdlib"]);
    }

    #[test]
    fn rejects_options_that_do_not_apply_to_a_command() {
        assert!(parse_options(["run".into(), "--target=other".into()]).is_err());
        assert!(parse_options(["check".into(), "-lm".into()]).is_err());
        assert!(parse_options(["check".into(), "-o".into(), "out".into()]).is_err());
    }

    #[test]
    fn pure_library_commands_validate_without_an_artifact() {
        let fixture = Fixture::new();
        let package =
            load_package(&fixture.manifest("package \"library\" { kind \"library\" }\n")).unwrap();
        assert_eq!(package.entry, None);
        let check = parse_options(["check".into()]).unwrap();
        let build = parse_options(["build".into()]).unwrap();
        assert!(validate_package_options(&package, &check).is_ok());
        assert!(validate_package_options(&package, &build).is_ok());
        assert_eq!(stapler_arguments(&package, &build, None)[0], "check");
        let run = parse_options(["run".into()]).unwrap();
        assert!(
            validate_package_options(&package, &run)
                .unwrap_err()
                .contains("cannot be run")
        );
        let output = parse_options(["build".into(), "-o".into(), "library.o".into()]).unwrap();
        assert!(validate_package_options(&package, &output).is_err());
    }
}
