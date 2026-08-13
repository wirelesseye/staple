use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

use stapler::{
    CodeGenerator, Diagnostic, NameResolver, Program, ProgramLoader, TypeChecker, TypedModule,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitKind {
    Llvm,
    Object,
    Executable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Compile,
    Run,
}

#[derive(Debug)]
enum Outcome {
    Completed(Option<String>),
    Executed(ExitStatus),
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    mode: Mode,
    input: OsString,
    output: Option<PathBuf>,
    emit: EmitKind,
    target: Option<String>,
    library_paths: Vec<PathBuf>,
    libraries: Vec<OsString>,
    linker: Option<OsString>,
    standard_library: Option<PathBuf>,
    program_arguments: Vec<OsString>,
}

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1)) {
        Ok(Outcome::Completed(Some(output))) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Completed(None)) => ExitCode::SUCCESS,
        Ok(Outcome::Executed(status)) => exit_code(status),
        Err(message) => {
            eprintln!("stapler: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Outcome, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if matches!(arguments.as_slice(), [argument] if argument == "-h" || argument == "--help") {
        return Ok(Outcome::Completed(Some(format!("{}\n", usage()))));
    }
    if matches!(arguments.as_slice(), [command, argument] if command == "run" && (argument == "-h" || argument == "--help"))
    {
        return Ok(Outcome::Completed(Some(format!("{}\n", run_usage()))));
    }
    let options = parse_options(arguments)?;
    let loader = match &options.standard_library {
        Some(root) => ProgramLoader::new().with_standard_library_root(root),
        None => ProgramLoader::new(),
    };
    let module = if options.input == "-" {
        let source = read_source(&options.input)?;
        let root = std::env::current_dir()
            .map_err(|error| format!("could not determine current directory: {error}"))?;
        let program = loader.load_source(&source, &root)?;
        compile_program(program)?
    } else {
        let program = loader.load_path(Path::new(&options.input))?;
        compile_program(program)?
    };
    let context = inkwell::context::Context::create();
    let generator = CodeGenerator::new(&context);

    if options.mode == Mode::Run {
        let object = TemporaryArtifact::new("object", "o");
        generator
            .emit_object(&module, object.path(), None)
            .map_err(format_diagnostics)?;
        let executable = TemporaryArtifact::new("executable", executable_extension());
        link_executable(object.path(), executable.path(), &options)?;
        let status = Command::new(executable.path())
            .args(&options.program_arguments)
            .status()
            .map_err(|error| {
                format!(
                    "could not run temporary executable `{}`: {error}",
                    executable.path().display()
                )
            })?;
        return Ok(Outcome::Executed(status));
    }

    match options.emit {
        EmitKind::Llvm => {
            let llvm = generator
                .compile_module_for_target(&module, options.target.as_deref())
                .map_err(format_diagnostics)?;
            if let Some(output) = options.output {
                std::fs::write(&output, llvm)
                    .map_err(|error| format!("could not write `{}`: {error}", output.display()))?;
                Ok(Outcome::Completed(None))
            } else {
                Ok(Outcome::Completed(Some(llvm)))
            }
        }
        EmitKind::Object => {
            let output = artifact_output(&options, "o")?;
            generator
                .emit_object(&module, &output, options.target.as_deref())
                .map_err(format_diagnostics)?;
            Ok(Outcome::Completed(None))
        }
        EmitKind::Executable => {
            let output = artifact_output(&options, executable_extension())?;
            let object = TemporaryArtifact::new("object", "o");
            generator
                .emit_object(&module, object.path(), options.target.as_deref())
                .map_err(format_diagnostics)?;
            link_executable(object.path(), &output, &options)?;
            Ok(Outcome::Completed(None))
        }
    }
}

fn parse_options(arguments: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut arguments = arguments.into_iter().peekable();
    let mode = if arguments.peek().is_some_and(|argument| argument == "run") {
        arguments.next();
        Mode::Run
    } else {
        Mode::Compile
    };
    let mut options = Options {
        mode,
        input: OsString::new(),
        output: None,
        emit: EmitKind::Executable,
        target: None,
        library_paths: Vec::new(),
        libraries: Vec::new(),
        linker: None,
        standard_library: None,
        program_arguments: Vec::new(),
    };
    let mut positional_only = false;
    let mut emit_specified = false;

    while let Some(argument) = arguments.next() {
        if !positional_only && argument == "--" {
            if options.mode == Mode::Run && !options.input.is_empty() {
                options.program_arguments.extend(arguments);
                break;
            }
            positional_only = true;
            continue;
        }
        if !positional_only && argument == "-o" {
            options.output = Some(PathBuf::from(next_value(&mut arguments, "-o")?));
            continue;
        }
        if !positional_only && argument == "--emit" {
            emit_specified = true;
            options.emit = parse_emit(&next_value(&mut arguments, "--emit")?)?;
            continue;
        }
        if !positional_only && argument == "--target" {
            options.target = Some(utf8_value(
                next_value(&mut arguments, "--target")?,
                "--target",
            )?);
            continue;
        }
        if !positional_only && argument == "--linker" {
            options.linker = Some(next_value(&mut arguments, "--linker")?);
            continue;
        }
        if !positional_only && argument == "--stdlib" {
            options.standard_library = Some(PathBuf::from(next_value(&mut arguments, "--stdlib")?));
            continue;
        }
        if !positional_only && argument == "-L" {
            options
                .library_paths
                .push(PathBuf::from(next_value(&mut arguments, "-L")?));
            continue;
        }
        if !positional_only && argument == "-l" {
            options.libraries.push(next_value(&mut arguments, "-l")?);
            continue;
        }

        let text = argument.to_string_lossy();
        if !positional_only && let Some(value) = text.strip_prefix("--emit=") {
            emit_specified = true;
            options.emit = parse_emit(OsStr::new(value))?;
        } else if !positional_only && let Some(value) = text.strip_prefix("--target=") {
            options.target = Some(value.to_owned());
        } else if !positional_only && let Some(value) = text.strip_prefix("--linker=") {
            options.linker = Some(value.into());
        } else if !positional_only && let Some(value) = text.strip_prefix("--stdlib=") {
            options.standard_library = Some(PathBuf::from(value));
        } else if !positional_only && text.starts_with("-L") && text.len() > 2 {
            options.library_paths.push(PathBuf::from(&text[2..]));
        } else if !positional_only && text.starts_with("-l") && text.len() > 2 {
            options.libraries.push(text[2..].into());
        } else if !positional_only && text.starts_with('-') && argument != "-" {
            let usage = if options.mode == Mode::Run {
                run_usage()
            } else {
                usage()
            };
            return Err(format!("unknown option `{text}`\n{usage}"));
        } else if options.input.is_empty() {
            options.input = argument;
        } else if options.mode == Mode::Run {
            return Err(format!(
                "program arguments require `--` after the input file\n{}",
                run_usage()
            ));
        } else {
            return Err(usage());
        }
    }

    if options.input.is_empty() {
        return Err(if options.mode == Mode::Run {
            run_usage()
        } else {
            usage()
        });
    }
    if options.mode == Mode::Run {
        if options.output.is_some() {
            return Err("`-o` is not supported by `stapler run`".to_owned());
        }
        if emit_specified {
            return Err("`--emit` is not supported by `stapler run`".to_owned());
        }
        if options.target.is_some() {
            return Err(
                "`--target` is not supported by `stapler run`; programs run on the host target"
                    .to_owned(),
            );
        }
    }
    if options.emit != EmitKind::Executable
        && (!options.library_paths.is_empty() || !options.libraries.is_empty())
    {
        return Err("`-L` and `-l` require `--emit=exe`".to_owned());
    }
    Ok(options)
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

fn parse_emit(value: &OsStr) -> Result<EmitKind, String> {
    match value.to_str() {
        Some("llvm" | "ir") => Ok(EmitKind::Llvm),
        Some("object" | "obj") => Ok(EmitKind::Object),
        Some("executable" | "exe") => Ok(EmitKind::Executable),
        _ => Err(format!(
            "unknown emission kind `{}`; expected `llvm`, `object`, or `exe`",
            value.to_string_lossy()
        )),
    }
}

fn read_source(input: &OsStr) -> Result<String, String> {
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

#[cfg(test)]
fn compile(source: &str) -> Result<TypedModule, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let program = ProgramLoader::new()
        .with_standard_library_root(root.join("stdlib"))
        .load_source(source, root)?;
    compile_program(program)
}

fn compile_program(program: Program) -> Result<TypedModule, String> {
    let module = NameResolver::new()
        .resolve_program(program)
        .map_err(format_diagnostics)?;
    TypeChecker::new().check(module).map_err(format_diagnostics)
}

fn artifact_output(options: &Options, extension: &str) -> Result<PathBuf, String> {
    if let Some(output) = &options.output {
        return Ok(output.clone());
    }
    if options.input == "-" {
        return Err("`-o` is required when emitting an artifact from standard input".to_owned());
    }
    let mut output = PathBuf::from(&options.input);
    output.set_extension(extension);
    Ok(output)
}

fn executable_extension() -> &'static str {
    if cfg!(windows) { "exe" } else { "" }
}

struct TemporaryArtifact {
    path: PathBuf,
}

impl TemporaryArtifact {
    fn new(kind: &str, extension: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let extension = if extension.is_empty() {
            String::new()
        } else {
            format!(".{extension}")
        };
        Self {
            path: std::env::temp_dir().join(format!(
                "stapler-{}-{nonce}-{kind}{extension}",
                std::process::id()
            )),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryArtifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn link_executable(object: &Path, output: &Path, options: &Options) -> Result<(), String> {
    let linker = options
        .linker
        .clone()
        .or_else(|| std::env::var_os("CC"))
        .unwrap_or_else(|| "cc".into());
    let mut command = Command::new(&linker);
    command.arg(object).arg("-o").arg(output);
    if let Some(target) = &options.target {
        command.arg(format!("--target={target}"));
    }
    for path in &options.library_paths {
        command.arg("-L").arg(path);
    }
    for library in &options.libraries {
        command.arg("-l").arg(library);
    }
    let result = command.output().map_err(|error| {
        format!(
            "could not run linker `{}`: {error}",
            linker.to_string_lossy()
        )
    })?;
    if result.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&result.stderr);
    Err(format!(
        "linker `{}` failed{}{}",
        linker.to_string_lossy(),
        if stderr.is_empty() { "" } else { ":\n" },
        stderr.trim_end()
    ))
}

fn format_diagnostics(diagnostics: Vec<Diagnostic>) -> String {
    diagnostics
        .into_iter()
        .map(|diagnostic| diagnostic.to_string())
        .collect::<Vec<_>>()
        .join("\n")
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
        "usage: stapler [options] <input.sta>\n",
        "       stapler run [options] <input.sta> [-- <arguments>...]\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  --emit <llvm|object|exe>  output kind (default: exe)\n",
        "  -o <path>                 output file; LLVM uses stdout by default\n",
        "  --target <triple>         LLVM target triple\n",
        "  --linker <command>        linker driver (default: $CC or cc)\n",
        "  --stdlib <path>           Staple standard-library root\n",
        "  -L <path>                 add a library search path when linking\n",
        "  -l <name>                 link a library\n",
        "  --                         stop parsing options\n",
        "  -                          read source from standard input",
    )
    .to_owned()
}

fn run_usage() -> String {
    concat!(
        "usage: stapler run [options] <input.sta> [-- <arguments>...]\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  --linker <command>        linker driver (default: $CC or cc)\n",
        "  --stdlib <path>           Staple standard-library root\n",
        "  -L <path>                 add a library search path when linking\n",
        "  -l <name>                 link a library\n",
        "  --                         pass remaining arguments to the program\n",
        "  -                          read source from standard input",
    )
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{EmitKind, Mode, Outcome, TemporaryArtifact, compile, parse_options, run};
    use std::ffi::{OsStr, OsString};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn requires_exactly_one_input() {
        assert!(parse_options([]).unwrap_err().starts_with("usage:"));
        assert!(
            parse_options(["one.sta".into(), "two.sta".into()])
                .unwrap_err()
                .starts_with("usage:")
        );
    }

    #[test]
    fn prints_help_without_an_input() {
        let Outcome::Completed(Some(help)) = run(["--help".into()]).expect("help should succeed")
        else {
            panic!("help should be printed");
        };

        assert!(help.starts_with("usage: stapler"));
        assert!(help.contains("--emit <llvm|object|exe>"));
        assert!(help.contains("stapler run"));
    }

    #[test]
    fn prints_run_help_without_an_input() {
        let Outcome::Completed(Some(help)) =
            run(["run".into(), "--help".into()]).expect("run help should succeed")
        else {
            panic!("run help should be printed");
        };

        assert!(help.starts_with("usage: stapler run"));
        assert!(help.contains("pass remaining arguments to the program"));
    }

    #[test]
    fn parses_run_options_and_program_arguments() {
        let options = parse_options([
            "run".into(),
            "--stdlib=vendor/stdlib".into(),
            "--linker".into(),
            "clang".into(),
            "-Lvendor/lib".into(),
            "-lm".into(),
            "examples/hello_world.sta".into(),
            "--".into(),
            "first".into(),
            "--second".into(),
        ])
        .expect("run options should parse");

        assert_eq!(options.mode, Mode::Run);
        assert_eq!(options.input, "examples/hello_world.sta");
        assert_eq!(options.linker.as_deref(), Some(OsStr::new("clang")));
        assert_eq!(options.library_paths[0].to_string_lossy(), "vendor/lib");
        assert_eq!(options.libraries[0].to_string_lossy(), "m");
        assert_eq!(
            options.program_arguments,
            [OsString::from("first"), OsString::from("--second")]
        );
    }

    #[test]
    fn rejects_build_only_run_options() {
        for arguments in [
            vec!["run", "-o", "output", "input.sta"],
            vec!["run", "--emit=exe", "input.sta"],
            vec!["run", "--target=native", "input.sta"],
        ] {
            let error = parse_options(arguments.into_iter().map(OsString::from))
                .expect_err("build-only option should be rejected in run mode");
            assert!(error.contains("not supported by `stapler run`"));
        }
    }

    #[test]
    fn requires_program_argument_separator() {
        let error = parse_options(["run".into(), "input.sta".into(), "argument".into()])
            .expect_err("program arguments without `--` should be rejected");

        assert!(error.starts_with("program arguments require `--`"));
    }

    #[test]
    fn removes_temporary_artifacts_when_dropped() {
        let path = {
            let artifact = TemporaryArtifact::new("cleanup-test", "tmp");
            std::fs::write(artifact.path(), "temporary")
                .expect("temporary artifact should be writable");
            artifact.path().to_owned()
        };

        assert!(!path.exists());
    }

    #[test]
    fn parses_native_output_options() {
        let options = parse_options([
            "--emit=exe".into(),
            "-o".into(),
            "hello".into(),
            "--target".into(),
            "aarch64-apple-darwin".into(),
            "--stdlib".into(),
            "vendor/stdlib".into(),
            "-L".into(),
            "vendor/lib".into(),
            "-lm".into(),
            "examples/hello_world.sta".into(),
        ])
        .expect("options should parse");

        assert_eq!(options.emit, EmitKind::Executable);
        assert_eq!(options.output.unwrap().to_string_lossy(), "hello");
        assert_eq!(options.target.as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(
            options.standard_library.unwrap().to_string_lossy(),
            "vendor/stdlib"
        );
        assert_eq!(options.library_paths[0].to_string_lossy(), "vendor/lib");
        assert_eq!(options.libraries[0].to_string_lossy(), "m");
    }

    #[test]
    fn emits_an_executable_by_default() {
        let options =
            parse_options(["examples/hello_world.sta".into()]).expect("the input should parse");

        assert_eq!(options.emit, EmitKind::Executable);
    }

    #[test]
    #[cfg(unix)]
    fn runs_source_directly_and_propagates_its_exit_status() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-run-{nonce}.sta"));
        std::fs::write(
            &source,
            "extern \"c\" { let exit: I32 -> () }\ndef main = () => exit 7\n",
        )
        .expect("temporary run source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let outcome = run([
            "run".into(),
            "--stdlib".into(),
            standard_library.into_os_string(),
            source.clone().into_os_string(),
            "--".into(),
            "ignored-for-now".into(),
        ])
        .expect("source should run directly");
        let _ = std::fs::remove_file(source);

        let Outcome::Executed(status) = outcome else {
            panic!("run mode should return a process status");
        };
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn run_reports_compiler_errors() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-run-error-{nonce}.sta"));
        std::fs::write(&source, "def main = () => missing\n")
            .expect("temporary invalid source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let error = run([
            "run".into(),
            "--stdlib".into(),
            standard_library.into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect_err("invalid source should not execute");
        let _ = std::fs::remove_file(source);

        assert!(error.contains("unknown name `missing`"));
    }

    #[test]
    fn run_reports_linker_launch_errors() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-run-linker-{nonce}.sta"));
        std::fs::write(&source, "def main = () => ()\n")
            .expect("temporary source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let error = run([
            "run".into(),
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--linker".into(),
            format!("stapler-missing-linker-{nonce}").into(),
            source.clone().into_os_string(),
        ])
        .expect_err("a missing linker should be reported");
        let _ = std::fs::remove_file(source);

        assert!(error.contains("could not run linker"));
    }

    #[test]
    fn compiles_source_to_llvm() {
        let module =
            compile(include_str!("../examples/hello_world.sta")).expect("example should compile");
        let context = inkwell::context::Context::create();
        let llvm = stapler::CodeGenerator::new(&context)
            .compile_module(&module)
            .expect("LLVM generation should succeed");

        assert!(llvm.contains("define i32 @main()"));
        assert!(llvm.contains("target triple"));
    }

    #[test]
    #[cfg(unix)]
    fn runs_source_main_with_io_after_top_level_initialization() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-main-io-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-main-io-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.io println\n",
                "extern \"c\" { let exit: I32 -> () }\n",
                "let mut initialized = 0\n",
                "initialized = 41\n",
                "def main = () => { println \"source main\"; exit (initialized - 41) }\n",
            ),
        )
        .expect("temporary source-main source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("source-main executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("source-main executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(
            result.status.success(),
            "source main exited with {}",
            result.status
        );
        assert_eq!(String::from_utf8_lossy(&result.stdout), "source main\n");
    }

    #[test]
    #[cfg(unix)]
    fn runs_typed_resources_with_lexical_shadowing() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-resources-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-resources-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "type Clock = (now: () -> I32)\n",
                "def clock = value: I32 => Clock (now: () => value)\n",
                "def read = () => (resource Clock).now ()\n",
                "def derive = () => clock (read () + 1)\n",
                "def make_reader = () => () => read ()\n",
                "let outer = clock 41\n",
                "let inner = clock 7\n",
                "let derived = with Clock = outer { with Clock = derive () { read () } }\n",
                "let reader = with Clock = outer { make_reader () }\n",
                "let later = with Clock = inner { reader () }\n",
                "exit ((derived - 42) + (later - 7))\n",
            ),
        )
        .expect("temporary resource source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("typed resource executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("typed resource executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "resource executable exited with {status}");
    }

    #[test]
    #[cfg(unix)]
    fn runs_refs_across_automatic_collection() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-ref-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-ref-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "def churn: I32 -> () = n => match n == 0 {\n",
                "  True() => (),\n",
                "  False() => { Ref n; churn (n - 1) },\n",
                "}\n",
                "def make_reader = () => {\n",
                "  def captured = Ref 42\n",
                "  () => captured\n",
                "}\n",
                "let keep = Ref (x: 42, y: 7)\n",
                "let read = make_reader ()\n",
                "churn 40000\n",
                "churn 40000\n",
                "let Ref captured = read ()\n",
                "exit ((keep.x - 42) + (captured - 42))\n",
            ),
        )
        .expect("temporary Ref source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("Ref executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("Ref executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_erased_product_length_and_indexing() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-erased-product-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-erased-product-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "let mut product: I32[3] = (10, 20, 30)\n",
                "let fixed: Ref I32[3] = Ref product\n",
                "let erased: Ref I32[] = fixed\n",
                "let index: USize = 1\n",
                "product[index] = 21\n",
                "erased[index] = 22\n",
                "let result = (erased[index] - 22) + (fixed[index] - 22) + (product[index] - 21)\n",
                "match length erased == 3 { True() => exit result, False() => exit 1 }\n",
            ),
        )
        .expect("temporary erased-product source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("erased-product executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("erased-product executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_mutable_bindings_captures_and_refs() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-mutable-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-mutable-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "let mut drops = 0\n",
                "type Resource = I32\n",
                "impl Drop Resource { def drop = Resource value => { drops = drops + value } }\n",
                "def release = () => { let mut resource = Resource 1; resource = Resource 2 }\n",
                "def abandon = () => { let mut resource = Resource 4; let update = () => { resource = Resource 5 }; update () }\n",
                "def churn: I32 -> () = n => match n == 0 { True() => (), False() => { Ref n; churn (n - 1) } }\n",
                "def make_counter = () => {\n",
                "  let mut value = 1\n",
                "  () => { value = value + 1; value }\n",
                "}\n",
                "let counter = make_counter ()\n",
                "let first = counter ()\n",
                "let second = counter ()\n",
                "let point = Ref (x: 4, y: 5)\n",
                "point.x = 6\n",
                "let scalar = Ref 7\n",
                "let old = replace (scalar, 8)\n",
                "let Ref current = scalar\n",
                "release ()\n",
                "abandon ()\n",
                "churn 40000\n",
                "churn 40000\n",
                "exit ((first - 2) + (second - 3) + (point.x - 6) + (old - 7) + (current - 8) + (drops - 12))\n",
            ),
        )
        .expect("temporary mutable source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("mutable executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("mutable executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_structurally_derived_product_defaults() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-default-product-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-default-product-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "type Seed = I32\n",
                "impl Default Seed { def default = () => Seed 7 }\n",
                "let integers: I32[3] = default ()\n",
                "let seeds: Seed[2] = default ()\n",
                "let Seed first = seeds.0\n",
                "let Seed second = seeds.1\n",
                "exit (integers.0 + integers.1 + integers.2 + first + second - 14)\n",
            ),
        )
        .expect("temporary default-product source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("default-product executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("default-product executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_product_value_spreads() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-product-spread-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-product-spread-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "def sum: I32[4] -> I32 = (a, b, c, d) => a + b + c + d\n",
                "let mut calls = 0\n",
                "def make_pair = () => { calls = calls + 1; (2, 3) }\n",
                "let expanded = (1, ...make_pair (), 4)\n",
                "exit ((sum expanded - 10) + (calls - 1))\n",
            ),
        )
        .expect("temporary product-spread source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("product-spread executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("product-spread executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn drops_owned_locals_in_reverse_scope_order() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-drop-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-drop-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "type Resource = I32\n",
                "impl Drop Resource {\n",
                "  def drop = Resource value => exit value\n",
                "}\n",
                "def exercise = () => {\n",
                "  let first = Resource 1\n",
                "  let second = Resource 0\n",
                "  ()\n",
                "}\n",
                "exercise ()\n",
                "exit 2\n",
            ),
        )
        .expect("temporary Drop source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("Drop executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("Drop executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_string_literal_and_mixed_union_matches() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-string-literals-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-string-literals-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "use std.cinterop (string_from_c_string, string_to_c_string)\n",
                "type Some = String\n",
                "def roundtrip: String -> String = value => string_from_c_string (string_to_c_string value)\n",
                "def return_unicode = () => roundtrip \"hé\"\n",
                "def capture = (value: String) => () => value\n",
                "def empty_score: String -> I32 = value => match value { \"\" => 1, _ => 100, }\n",
                "def unicode_score: String -> I32 = value => match value { \"hé\" => 2, _ => 100, }\n",
                "def pure: (\"yes\" | \"no\") -> I32 = value => match value {\n",
                "  \"yes\" => 1,\n",
                "  \"no\" => 2,\n",
                "}\n",
                "def mixed: Some | \"yes\" | \"no\" -> I32 = value => match value {\n",
                "  Some _ => 4,\n",
                "  \"yes\" => 5,\n",
                "  \"no\" => 3,\n",
                "}\n",
                "let literal: \"yes\" | \"no\" = \"no\"\n",
                "let injected: Some | \"yes\" | \"no\" = literal\n",
                "let empty = roundtrip \"\"\n",
                "let pair = (text: return_unicode (), count: 1)\n",
                "let copied = pair.text\n",
                "let read = capture copied\n",
                "exit (pure \"yes\" + pure literal + mixed injected + mixed (Some \"value\") + empty_score empty + unicode_score pair.text + unicode_score copied + unicode_score (read ()) - 17)\n",
            ),
        )
        .expect("temporary string-literal source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("string-literal executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("string-literal executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "String executable returned {status}");
    }

    #[test]
    #[cfg(unix)]
    fn runs_loop_break_and_continue_values() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-loop-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-loop-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "def exercise = () => {\n",
                "  let mut first: Bool = True\n",
                "  loop {\n",
                "    match first {\n",
                "      True() => { first = False; continue },\n",
                "      False() => { break 3 },\n",
                "    }\n",
                "  }\n",
                "}\n",
                "exit (exercise () - 3)\n",
            ),
        )
        .expect("temporary loop source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("loop executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("loop executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "loop executable returned {status}");
    }

    #[test]
    #[cfg(unix)]
    fn runs_float_arithmetic_and_partial_ordering() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-float-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-float-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { let exit: I32 -> () }\n",
                "def score_bool: Bool -> I32 = value => match value { True() => 1, False() => 0, }\n",
                "let nan = 0.0 / 0.0\n",
                "let partial_score = match (PartialOrd.partial_cmp nan 1.0) { None() => 0, Some _ => 1, }\n",
                "let less_score = match (Ord.cmp 1 2) { Less() => 0, _ => 1, }\n",
                "let equal_score = match (Ord.cmp 2 2) { Equal() => 0, _ => 1, }\n",
                "let greater_score = match (Ord.cmp 3 2) { Greater() => 0, _ => 1, }\n",
                "let single: F32 = (1.5 satisfies F32) + (.5 satisfies F32)\n",
                "exit (partial_score + less_score + equal_score + greater_score + score_bool (nan < 1.0) + score_bool (nan == nan) + (score_bool (nan != nan) - 1) + (score_bool (single == (2.0 satisfies F32)) - 1))\n",
            ),
        )
        .expect("temporary float source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("float executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("float executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "float executable returned {status}");
    }
}
