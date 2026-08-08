use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
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

#[derive(Debug, PartialEq, Eq)]
struct Options {
    input: OsString,
    output: Option<PathBuf>,
    emit: EmitKind,
    target: Option<String>,
    library_paths: Vec<PathBuf>,
    libraries: Vec<OsString>,
    linker: Option<OsString>,
}

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1)) {
        Ok(Some(llvm)) => {
            print!("{llvm}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("stapler: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Option<String>, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if matches!(arguments.as_slice(), [argument] if argument == "-h" || argument == "--help") {
        return Ok(Some(format!("{}\n", usage())));
    }
    let options = parse_options(arguments)?;
    let module = if options.input == "-" {
        let source = read_source(&options.input)?;
        let root = std::env::current_dir()
            .map_err(|error| format!("could not determine current directory: {error}"))?;
        let program = ProgramLoader::new().load_source(&source, &root)?;
        compile_program(program)?
    } else {
        let program = ProgramLoader::new().load_path(Path::new(&options.input))?;
        compile_program(program)?
    };
    let context = inkwell::context::Context::create();
    let generator = CodeGenerator::new(&context);

    match options.emit {
        EmitKind::Llvm => {
            let llvm = generator
                .compile_module_for_target(&module, options.target.as_deref())
                .map_err(format_diagnostics)?;
            if let Some(output) = options.output {
                std::fs::write(&output, llvm)
                    .map_err(|error| format!("could not write `{}`: {error}", output.display()))?;
                Ok(None)
            } else {
                Ok(Some(llvm))
            }
        }
        EmitKind::Object => {
            let output = artifact_output(&options, "o")?;
            generator
                .emit_object(&module, &output, options.target.as_deref())
                .map_err(format_diagnostics)?;
            Ok(None)
        }
        EmitKind::Executable => {
            let output = artifact_output(&options, executable_extension())?;
            let object = temporary_object_path();
            let emission = generator
                .emit_object(&module, &object, options.target.as_deref())
                .map_err(format_diagnostics);
            if let Err(error) = emission {
                let _ = std::fs::remove_file(&object);
                return Err(error);
            }
            let result = link_executable(&object, &output, &options);
            let _ = std::fs::remove_file(&object);
            result.map(|()| None)
        }
    }
}

fn parse_options(arguments: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut arguments = arguments.into_iter();
    let mut options = Options {
        input: OsString::new(),
        output: None,
        emit: EmitKind::Executable,
        target: None,
        library_paths: Vec::new(),
        libraries: Vec::new(),
        linker: None,
    };
    let mut positional_only = false;

    while let Some(argument) = arguments.next() {
        if !positional_only && argument == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && argument == "-o" {
            options.output = Some(PathBuf::from(next_value(&mut arguments, "-o")?));
            continue;
        }
        if !positional_only && argument == "--emit" {
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
            options.emit = parse_emit(OsStr::new(value))?;
        } else if !positional_only && let Some(value) = text.strip_prefix("--target=") {
            options.target = Some(value.to_owned());
        } else if !positional_only && let Some(value) = text.strip_prefix("--linker=") {
            options.linker = Some(value.into());
        } else if !positional_only && text.starts_with("-L") && text.len() > 2 {
            options.library_paths.push(PathBuf::from(&text[2..]));
        } else if !positional_only && text.starts_with("-l") && text.len() > 2 {
            options.libraries.push(text[2..].into());
        } else if !positional_only && text.starts_with('-') && argument != "-" {
            return Err(format!("unknown option `{text}`\n{}", usage()));
        } else if options.input.is_empty() {
            options.input = argument;
        } else {
            return Err(usage());
        }
    }

    if options.input.is_empty() {
        return Err(usage());
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
    let module = stapler::parse(source).map_err(|error| error.to_string())?;
    let module = NameResolver::new()
        .resolve(&module)
        .map_err(format_diagnostics)?;
    TypeChecker::new().check(module).map_err(format_diagnostics)
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

fn temporary_object_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("stapler-{}-{nonce}.o", std::process::id()))
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

fn usage() -> String {
    concat!(
        "usage: stapler [options] <input.sta>\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  --emit <llvm|object|exe>  output kind (default: exe)\n",
        "  -o <path>                 output file; LLVM uses stdout by default\n",
        "  --target <triple>         LLVM target triple\n",
        "  --linker <command>        linker driver (default: $CC or cc)\n",
        "  -L <path>                 add a library search path when linking\n",
        "  -l <name>                 link a library\n",
        "  --                         stop parsing options\n",
        "  -                          read source from standard input",
    )
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{EmitKind, compile, parse_options, run};

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
        let help = run(["--help".into()])
            .expect("help should succeed")
            .expect("help should be printed");

        assert!(help.starts_with("usage: stapler"));
        assert!(help.contains("--emit <llvm|object|exe>"));
    }

    #[test]
    fn parses_native_output_options() {
        let options = parse_options([
            "--emit=exe".into(),
            "-o".into(),
            "hello".into(),
            "--target".into(),
            "aarch64-apple-darwin".into(),
            "-L".into(),
            "vendor/lib".into(),
            "-lm".into(),
            "examples/hello_world.sta".into(),
        ])
        .expect("options should parse");

        assert_eq!(options.emit, EmitKind::Executable);
        assert_eq!(options.output.unwrap().to_string_lossy(), "hello");
        assert_eq!(options.target.as_deref(), Some("aarch64-apple-darwin"));
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
}
