use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

use stapler::{
    CodeGenerator, Diagnostic, NameResolver, Program, ProgramLoader, TypeChecker, TypedModule,
    expand_macros,
};

mod expansion_render;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitKind {
    Llvm,
    Object,
    Executable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Compile,
    Check,
    Run,
    Expand,
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
    module_root: Option<PathBuf>,
    package_root: Option<PathBuf>,
    package_name: Option<String>,
    manifest_path: Option<PathBuf>,
    program_arguments: Vec<OsString>,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
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
            eprintln!("stpl: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Outcome, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if matches!(arguments.as_slice(), [argument] if argument == "-h" || argument == "--help") {
        return Ok(Outcome::Completed(Some(format!("{}\n", usage()))));
    }
    if let [command, argument] = arguments.as_slice()
        && (argument == "-h" || argument == "--help")
    {
        if command == "run" {
            return Ok(Outcome::Completed(Some(format!("{}\n", run_usage()))));
        }
        if command == "check" {
            return Ok(Outcome::Completed(Some(format!("{}\n", check_usage()))));
        }
        if command == "expand" {
            return Ok(Outcome::Completed(Some(format!("{}\n", expand_usage()))));
        }
    }
    let options = parse_options(arguments)?;
    let mut loader = match &options.standard_library {
        Some(root) => ProgramLoader::new().with_standard_library_root(root),
        None => ProgramLoader::new(),
    };
    if let Some(root) = &options.module_root {
        loader = loader.with_module_root(root);
    }
    if let Some(root) = &options.package_root {
        loader = loader.with_package_root(root);
    }
    if let Some(name) = &options.package_name {
        loader = loader.with_package_name(name);
    }
    loader = loader.with_feature_selection(binder::FeatureSelection {
        features: options.features.clone(),
        all_features: options.all_features,
        no_default_features: options.no_default_features,
    });
    let program = if let Some(manifest) = &options.manifest_path {
        let graph = binder::load_package_graph(manifest)?;
        if options.mode != Mode::Check && graph.root_package().entry.is_none() {
            return Err(format!(
                "library package `{}` has no entry module and cannot emit an executable",
                graph.root_package().name
            ));
        }
        loader.with_package_graph(graph).load_package_graph()?
    } else if options.input == "-" {
        let source = read_source(&options.input)?;
        let root = match &options.module_root {
            Some(root) => root.clone(),
            None => std::env::current_dir()
                .map_err(|error| format!("could not determine current directory: {error}"))?,
        };
        loader.load_source(&source, &root)?
    } else {
        loader.load_path(Path::new(&options.input))?
    };

    if options.mode == Mode::Expand {
        let expanded = expand_macros(program).map_err(format_diagnostics)?;
        let rendered = expansion_render::render_module(&expanded);
        return match &options.output {
            Some(output) => {
                std::fs::write(output, rendered)
                    .map_err(|error| format!("could not write `{}`: {error}", output.display()))?;
                Ok(Outcome::Completed(None))
            }
            None => Ok(Outcome::Completed(Some(rendered))),
        };
    }

    let module = compile_program(program)?;
    if options.mode == Mode::Check {
        return Ok(Outcome::Completed(None));
    }
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
    let mode = match arguments.peek().and_then(|argument| argument.to_str()) {
        Some("run") => {
            arguments.next();
            Mode::Run
        }
        Some("check") => {
            arguments.next();
            Mode::Check
        }
        Some("expand") => {
            arguments.next();
            Mode::Expand
        }
        _ => Mode::Compile,
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
        module_root: None,
        package_root: None,
        package_name: None,
        manifest_path: None,
        program_arguments: Vec::new(),
        features: Vec::new(),
        all_features: false,
        no_default_features: false,
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
        if !positional_only && argument == "--module-root" {
            options.module_root = Some(PathBuf::from(next_value(&mut arguments, "--module-root")?));
            continue;
        }
        if !positional_only && argument == "--package-root" {
            options.package_root =
                Some(PathBuf::from(next_value(&mut arguments, "--package-root")?));
            continue;
        }
        if !positional_only && argument == "--package-name" {
            options.package_name = Some(utf8_value(
                next_value(&mut arguments, "--package-name")?,
                "--package-name",
            )?);
            continue;
        }
        if !positional_only && argument == "--manifest-path" {
            options.manifest_path = Some(PathBuf::from(next_value(
                &mut arguments,
                "--manifest-path",
            )?));
            continue;
        }
        if !positional_only && argument == "--features" {
            add_features(
                &mut options.features,
                &utf8_value(next_value(&mut arguments, "--features")?, "--features")?,
            )?;
            continue;
        }
        if !positional_only && argument == "--all-features" {
            options.all_features = true;
            continue;
        }
        if !positional_only && argument == "--no-default-features" {
            options.no_default_features = true;
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
        } else if !positional_only && let Some(value) = text.strip_prefix("--module-root=") {
            options.module_root = Some(PathBuf::from(value));
        } else if !positional_only && let Some(value) = text.strip_prefix("--package-root=") {
            options.package_root = Some(PathBuf::from(value));
        } else if !positional_only && let Some(value) = text.strip_prefix("--package-name=") {
            options.package_name = Some(value.to_owned());
        } else if !positional_only && let Some(value) = text.strip_prefix("--manifest-path=") {
            options.manifest_path = Some(PathBuf::from(value));
        } else if !positional_only && let Some(value) = text.strip_prefix("--features=") {
            add_features(&mut options.features, value)?;
        } else if !positional_only && text.starts_with("-L") && text.len() > 2 {
            options.library_paths.push(PathBuf::from(&text[2..]));
        } else if !positional_only && text.starts_with("-l") && text.len() > 2 {
            options.libraries.push(text[2..].into());
        } else if !positional_only && text.starts_with('-') && argument != "-" {
            let usage = match options.mode {
                Mode::Run => run_usage(),
                Mode::Check => check_usage(),
                Mode::Expand => expand_usage(),
                Mode::Compile => usage(),
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
            return Err(match options.mode {
                Mode::Check => check_usage(),
                Mode::Expand => expand_usage(),
                _ => usage(),
            });
        }
    }
    if options.manifest_path.is_none()
        && (!options.features.is_empty() || options.all_features || options.no_default_features)
    {
        return Err("feature selection requires `--manifest-path`".to_owned());
    }

    if options.input.is_empty() && options.manifest_path.is_none() {
        return Err(match options.mode {
            Mode::Run => run_usage(),
            Mode::Check => check_usage(),
            Mode::Expand => expand_usage(),
            Mode::Compile => usage(),
        });
    }
    if options.manifest_path.is_some()
        && (!options.input.is_empty()
            || options.module_root.is_some()
            || options.package_root.is_some()
            || options.package_name.is_some())
    {
        return Err(
            "`--manifest-path` cannot be combined with an input file or low-level package options"
                .to_owned(),
        );
    }
    if options.mode == Mode::Run {
        if options.output.is_some() {
            return Err("`-o` is not supported by `stpl run`".to_owned());
        }
        if emit_specified {
            return Err("`--emit` is not supported by `stpl run`".to_owned());
        }
        if options.target.is_some() {
            return Err(
                "`--target` is not supported by `stpl run`; programs run on the host target"
                    .to_owned(),
            );
        }
    }
    if options.mode == Mode::Check {
        if options.output.is_some() {
            return Err("`-o` is not supported by `stpl check`".to_owned());
        }
        if emit_specified {
            return Err("`--emit` is not supported by `stpl check`".to_owned());
        }
        if options.target.is_some() {
            return Err("`--target` is not supported by `stpl check`".to_owned());
        }
        if options.linker.is_some()
            || !options.library_paths.is_empty()
            || !options.libraries.is_empty()
        {
            return Err("linker options are not supported by `stpl check`".to_owned());
        }
    }
    if options.mode == Mode::Expand {
        if emit_specified {
            return Err("`--emit` is not supported by `stpl expand`".to_owned());
        }
        if options.target.is_some() {
            return Err("`--target` is not supported by `stpl expand`".to_owned());
        }
        if options.linker.is_some()
            || !options.library_paths.is_empty()
            || !options.libraries.is_empty()
        {
            return Err("linker options are not supported by `stpl expand`".to_owned());
        }
    }
    if options.emit != EmitKind::Executable
        && (!options.library_paths.is_empty() || !options.libraries.is_empty())
    {
        return Err("`-L` and `-l` require `--emit=exe`".to_owned());
    }
    Ok(options)
}

fn add_features(destination: &mut Vec<String>, value: &str) -> Result<(), String> {
    for feature in value.split(',') {
        binder::validate_feature_name(feature)?;
        destination.push(feature.to_owned());
    }
    Ok(())
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
        "usage: stpl [options] <input.sta>\n",
        "       stpl check [options] <input.sta>\n",
        "       stpl run [options] <input.sta> [-- <arguments>...]\n",
        "       stpl expand [options] <input.sta>\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  --emit <llvm|object|exe>  output kind (default: exe)\n",
        "  -o <path>                 output file; LLVM uses stdout by default\n",
        "  --target <triple>         LLVM target triple\n",
        "  --linker <command>        linker driver (default: $CC or cc)\n",
        "  --stdlib <path>           Staple standard-library root\n",
        "  --module-root <path>      package module directory (default: entry directory)\n",
        "  --package-root <path>     optional package root module\n",
        "  --package-name <name>     package name used by tooling\n",
        "  --manifest-path <path>    load a Binder package graph instead of an input\n",
        "  --features <names>        enable package features\n",
        "  --all-features            enable every package feature\n",
        "  --no-default-features     disable default package features\n",
        "  -L <path>                 add a library search path when linking\n",
        "  -l <name>                 link a library\n",
        "  --                         stop parsing options\n",
        "  -                          read source from standard input",
    )
    .to_owned()
}

fn run_usage() -> String {
    concat!(
        "usage: stpl run [options] <input.sta> [-- <arguments>...]\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  --linker <command>        linker driver (default: $CC or cc)\n",
        "  --stdlib <path>           Staple standard-library root\n",
        "  --module-root <path>      package module directory (default: entry directory)\n",
        "  --package-root <path>     optional package root module\n",
        "  --package-name <name>     package name used by tooling\n",
        "  --manifest-path <path>    load a Binder package graph instead of an input\n",
        "  --features <names>        enable package features\n",
        "  --all-features            enable every package feature\n",
        "  --no-default-features     disable default package features\n",
        "  -L <path>                 add a library search path when linking\n",
        "  -l <name>                 link a library\n",
        "  --                         pass remaining arguments to the program\n",
        "  -                          read source from standard input",
    )
    .to_owned()
}

fn check_usage() -> String {
    concat!(
        "usage: stpl check [options] <input.sta>\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  --stdlib <path>           Staple standard-library root\n",
        "  --module-root <path>      package module directory (default: entry directory)\n",
        "  --package-root <path>     optional package root module\n",
        "  --package-name <name>     package name used by tooling\n",
        "  --manifest-path <path>    load a Binder package graph instead of an input\n",
        "  --features <names>        enable package features\n",
        "  --all-features            enable every package feature\n",
        "  --no-default-features     disable default package features\n",
        "  --                         stop parsing options",
    )
    .to_owned()
}

fn expand_usage() -> String {
    concat!(
        "usage: stpl expand [options] <input.sta>\n",
        "\n",
        "Prints the entry module's source after macro expansion, without\n",
        "type-checking it. Analogous to \"Expand macro recursively\".\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  -o <path>                 write to a file instead of standard output\n",
        "  --stdlib <path>           Staple standard-library root\n",
        "  --module-root <path>      package module directory (default: entry directory)\n",
        "  --package-root <path>     optional package root module\n",
        "  --package-name <name>     package name used by tooling\n",
        "  --                         stop parsing options\n",
        "  -                          read source from standard input",
    )
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{EmitKind, Mode, Outcome, TemporaryArtifact, compile, parse_options, run};
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn expand_source(name: &str, source: &str) -> String {
        let path = std::env::temp_dir().join(format!(
            "stpl-expand-{name}-{}.sta",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&path, source).expect("temporary expand source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let outcome = run([
            "expand".into(),
            "--stdlib".into(),
            standard_library.into_os_string(),
            path.clone().into_os_string(),
        ]);
        let _ = std::fs::remove_file(&path);
        match outcome.expect("expand should succeed") {
            Outcome::Completed(Some(text)) => text,
            other => panic!("expand should print to stdout, got {other:?}"),
        }
    }

    #[test]
    fn parses_expand_subcommand() {
        let options =
            parse_options(["expand".into(), "input.sta".into()]).expect("expand should parse");
        assert_eq!(options.mode, Mode::Expand);
        assert_eq!(options.input, OsString::from("input.sta"));
    }

    #[test]
    fn expand_help_describes_the_subcommand() {
        let Outcome::Completed(Some(help)) =
            run(["expand".into(), "--help".into()]).expect("expand help should succeed")
        else {
            panic!("expand help should be printed");
        };
        assert!(help.starts_with("usage: stpl expand"));
        assert!(help.contains("macro expansion"));
    }

    #[test]
    fn expand_rejects_emit_option() {
        let error = parse_options(["expand".into(), "--emit=llvm".into(), "input.sta".into()])
            .expect_err("expand should reject --emit");
        assert!(error.contains("`--emit` is not supported by `stpl expand`"));
    }

    #[test]
    fn expand_rewrites_a_macro_call_in_a_function_body() {
        let expanded = expand_source(
            "body",
            concat!(
                "use std.syntax.(quote, Expr)\n",
                "macro double = value: Expr => quote { ($value) + ($value) }\n",
                "def main: () -> () = () => {\n",
                "    let x: I32 = double 21\n",
                "    ()\n",
                "}\n",
            ),
        );
        assert!(
            expanded.contains("let x: I32 = (21) + (21)"),
            "expected the macro call to be expanded, got:\n{expanded}"
        );
        assert!(
            !expanded.contains("double 21"),
            "the original macro call should be gone, got:\n{expanded}"
        );
    }

    #[test]
    fn expand_rewrites_a_top_level_item_macro() {
        let expanded = expand_source(
            "item",
            concat!(
                "use std.syntax.(parse_quote, Expr, Item, Sequence)\n",
                "macro pair: Expr -> Sequence Item = value => parse_quote {\n",
                "    let first: I32 = $value\n",
                "    let second: I32 = first + 1\n",
                "}\n",
                "pair 41\n",
            ),
        );
        assert!(
            expanded.contains("let first: I32 = 41"),
            "expected the splice to be substituted, got:\n{expanded}"
        );
        assert!(
            expanded.contains("let second: I32 = first + 1"),
            "expected both generated items, got:\n{expanded}"
        );
        assert!(
            !expanded.contains("pair 41"),
            "the original macro invocation should be gone, got:\n{expanded}"
        );
    }

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

        assert!(help.starts_with("usage: stpl"));
        assert!(help.contains("--emit <llvm|object|exe>"));
        assert!(help.contains("stpl run"));
    }

    #[test]
    fn prints_run_help_without_an_input() {
        let Outcome::Completed(Some(help)) =
            run(["run".into(), "--help".into()]).expect("run help should succeed")
        else {
            panic!("run help should be printed");
        };

        assert!(help.starts_with("usage: stpl run"));
        assert!(help.contains("pass remaining arguments to the program"));
    }

    #[test]
    fn parses_check_with_an_explicit_module_root() {
        let options = parse_options([
            "check".into(),
            "--module-root=src".into(),
            "--stdlib".into(),
            "vendor/stdlib".into(),
            "src/bin/main.sta".into(),
        ])
        .expect("check options should parse");

        assert_eq!(options.mode, Mode::Check);
        assert_eq!(
            options.module_root.unwrap(),
            std::path::PathBuf::from("src")
        );
        assert_eq!(options.input, "src/bin/main.sta");
    }

    #[test]
    fn parses_manifest_mode_without_a_positional_input() {
        let options = parse_options([
            "check".into(),
            "--manifest-path".into(),
            "binder.kdl".into(),
        ])
        .expect("manifest mode should derive its input from Binder");
        assert_eq!(options.manifest_path, Some(PathBuf::from("binder.kdl")));
        assert!(options.input.is_empty());
        assert!(
            parse_options([
                "check".into(),
                "--manifest-path=binder.kdl".into(),
                "main.sta".into(),
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_codegen_and_linker_options_in_check_mode() {
        for arguments in [
            vec!["check", "-o", "output", "input.sta"],
            vec!["check", "--emit=llvm", "input.sta"],
            vec!["check", "--target=native", "input.sta"],
            vec!["check", "-lm", "input.sta"],
        ] {
            let error = parse_options(arguments.into_iter().map(OsString::from))
                .expect_err("code generation option should be rejected in check mode");
            assert!(error.contains("not supported by `stpl check`"));
        }
    }

    #[test]
    fn checks_a_nested_entry_without_creating_an_artifact() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("stapler-check-{nonce}"));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(
            root.join("bin/main.sta"),
            "use package.values.answer\nlet checked: I32 = answer\n",
        )
        .unwrap();
        std::fs::write(
            root.join("values.sta"),
            "pub mod\npub let answer: I32 = 42\n",
        )
        .unwrap();
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");

        let outcome = run([
            "check".into(),
            "--module-root".into(),
            root.clone().into_os_string(),
            "--stdlib".into(),
            standard_library.into_os_string(),
            root.join("bin/main.sta").into_os_string(),
        ])
        .expect("nested package should type-check");

        assert!(matches!(outcome, Outcome::Completed(None)));
        assert!(!root.join("bin/main").exists());
        let _ = std::fs::remove_dir_all(root);
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
            assert!(error.contains("not supported by `stpl run`"));
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
        std::fs::write(&source, "extern \"c\" { exit: I32 -> () }\nexit 7\n")
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
    fn runs_a_const_folded_recursive_computation_end_to_end() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-run-const-{nonce}.sta"));
        std::fs::write(
            &source,
            concat!(
                "const x: I32 = 1 + 3\n",
                "def fibonacci: I32 -> I32 = n =>\n",
                "    match n < 2 {\n",
                "        True() => n,\n",
                "        False() => fibonacci (n - 1) + fibonacci (n - 2),\n",
                "    }\n",
                "const y = fibonacci 10\n",
                "extern \"c\" { exit: I32 -> () }\n",
                "exit (x + y)\n",
            ),
        )
        .expect("temporary run source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("stdlib");
        let outcome = run([
            "run".into(),
            "--stdlib".into(),
            standard_library.into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("const-folded source should run");
        let _ = std::fs::remove_file(source);

        let Outcome::Executed(status) = outcome else {
            panic!("run mode should return a process status");
        };
        // x = 4, fibonacci(10) = 55, evaluated entirely at compile time.
        assert_eq!(status.code(), Some(59));
    }

    #[test]
    fn run_reports_compiler_errors() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-run-error-{nonce}.sta"));
        std::fs::write(&source, "missing\n").expect("temporary invalid source should be writable");
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
        std::fs::write(&source, "()\n").expect("temporary source should be writable");
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

    /// Module initializers (`__staple_init_m*`) are the only place the
    /// entry-module top level could call into the reactive runtime; the
    /// stdlib's own `reactive_scope`/`__reactive_scope` functions are always
    /// compiled in regardless of usage, so a whole-file substring check
    /// would false-positive. Isolate just the initializer bodies instead.
    fn extract_module_initializer_bodies(llvm: &str) -> String {
        let mut bodies = String::new();
        let mut inside = false;
        for line in llvm.lines() {
            if !inside && line.starts_with("define") && line.contains("@__staple_init_m") {
                inside = true;
            }
            if inside {
                bodies.push_str(line);
                bodies.push('\n');
                if line == "}" {
                    inside = false;
                }
            }
        }
        bodies
    }

    #[test]
    fn entry_module_without_reactive_omits_scope_creation() {
        let module =
            compile(include_str!("../examples/hello_world.sta")).expect("example should compile");
        let context = inkwell::context::Context::create();
        let llvm = stapler::CodeGenerator::new(&context)
            .compile_module(&module)
            .expect("LLVM generation should succeed");

        let initializers = extract_module_initializer_bodies(&llvm);
        assert!(!initializers.contains("__staple_reactive_scope_create"));
    }

    #[test]
    fn entry_module_with_top_level_reactive_creates_scope() {
        let module = compile(concat!(
            "let signal count = 0\n",
            "let mut observed = 0\n",
            "reaction { observed = count; () }\n",
        ))
        .expect("top-level reactive source should compile");
        let context = inkwell::context::Context::create();
        let llvm = stapler::CodeGenerator::new(&context)
            .compile_module(&module)
            .expect("LLVM generation should succeed");

        let initializers = extract_module_initializer_bodies(&llvm);
        assert!(initializers.contains("__staple_reactive_scope_create"));
        assert!(initializers.contains("__staple_reactive_scope_dispose"));
    }

    #[test]
    #[cfg(unix)]
    fn runs_top_level_io_in_the_entry_module() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-main-io-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-main-io-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.io.println\n",
                "extern \"c\" { exit: I32 -> () }\n",
                "let mut initialized = 0\n",
                "initialized = 41\n",
                "println \"source main\"\n",
                "exit (initialized - 41)\n",
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
                "extern \"c\" { exit: I32 -> () }\n",
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
                "extern \"c\" { exit: I32 -> () }\n",
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
    fn runs_buffers_and_keeps_interior_views_alive() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-buffer-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-buffer-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.buffer.Buffer\n",
                "use std.slice.Slice\n",
                "extern \"c\" { exit: I32 -> () }\n",
                "def churn: I32 -> () = n => match n == 0 {\n",
                "  True() => (),\n",
                "  False() => { Ref n; churn (n - 1) },\n",
                "}\n",
                "def make_ref = () => {\n",
                "  let mut values: Buffer I32 = Buffer.with_capacity 2\n",
                "  Buffer.push values 41\n",
                "  Buffer.push values 42\n",
                "  Buffer.get_ref (values, 1)\n",
                "}\n",
                "def make_slice = () => {\n",
                "  let mut values: Buffer I32 = Buffer.with_capacity 2\n",
                "  Buffer.push values 7\n",
                "  Buffer.push values 8\n",
                "  let popped = Buffer.pop values\n",
                "  Buffer.freeze values\n",
                "}\n",
                "let kept_ref = make_ref ()\n",
                "let kept_slice = make_slice ()\n",
                "churn 40000\n",
                "churn 40000\n",
                "let Ref answer = kept_ref\n",
                "let result = answer - 42\n",
                "match Slice.length kept_slice == 1 {\n",
                "  True() => exit result,\n",
                "  False() => exit 1,\n",
                "}\n",
            ),
        )
        .expect("temporary Buffer source should be writable");
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
        .expect("Buffer executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("Buffer executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "Buffer executable exited with {status}");
    }

    #[test]
    #[cfg(unix)]
    fn runs_buffer_transfer_and_moves_elements_in_order() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-transfer-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-transfer-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.buffer.Buffer\n",
                "extern \"c\" { exit: I32 -> () }\n",
                "def run = () => {\n",
                "let mut from: Buffer I32 = Buffer.with_capacity 3\n",
                "Buffer.push from 10\n",
                "Buffer.push from 20\n",
                "Buffer.push from 30\n",
                "let mut into: Buffer I32 = Buffer.with_capacity 5\n",
                "Buffer.push into 1\n",
                "Buffer.transfer (from, into)\n",
                "let into_length = Buffer.length into\n",
                "let from_length = Buffer.length from\n",
                "match into_length == 4 {\n",
                "  True() => match from_length == 0 {\n",
                "    True() => {\n",
                "      let Ref a = Buffer.get_ref (into, 0)\n",
                "      let Ref b = Buffer.get_ref (into, 1)\n",
                "      let Ref c = Buffer.get_ref (into, 2)\n",
                "      let Ref d = Buffer.get_ref (into, 3)\n",
                "      match a == 1 {\n",
                "        True() => match b == 10 {\n",
                "          True() => match c == 20 {\n",
                "            True() => match d == 30 {\n",
                "              True() => exit 0,\n",
                "              False() => exit 5,\n",
                "            },\n",
                "            False() => exit 4,\n",
                "          },\n",
                "          False() => exit 3,\n",
                "        },\n",
                "        False() => exit 2,\n",
                "      }\n",
                "    },\n",
                "    False() => exit 1,\n",
                "  },\n",
                "  False() => exit 1,\n",
                "}\n",
                "}\nrun ()\n",
            ),
        )
        .expect("temporary Buffer.transfer source should be writable");
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
        .expect("Buffer.transfer executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("Buffer.transfer executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(
            status.success(),
            "Buffer.transfer executable exited with {status}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn runs_list_growth_and_preserves_order() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-list-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-list-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def run = () => {\n",
                "let mut values: List I32 = List.new ()\n",
                "let mut i: I32 = 0\n",
                "while (i < 10) {\n",
                "  List.push values i\n",
                "  i = i + 1\n",
                "}\n",
                "let length = List.length values\n",
                "let capacity = List.capacity values\n",
                "let first = List.get_unchecked values 0\n",
                "let last = List.get_unchecked values 9\n",
                "let popped = List.pop values\n",
                "let length_after_pop = List.length values\n",
                "match length == 10 {\n",
                "  True() => match capacity >= 10 {\n",
                "    True() => match first == 0 {\n",
                "      True() => match last == 9 {\n",
                "        True() => match popped {\n",
                "          Some(value) => match value == 9 {\n",
                "            True() => match length_after_pop == 9 {\n",
                "              True() => exit 0,\n",
                "              False() => exit 6,\n",
                "            },\n",
                "            False() => exit 5,\n",
                "          },\n",
                "          None() => exit 4,\n",
                "        },\n",
                "        False() => exit 3,\n",
                "      },\n",
                "      False() => exit 2,\n",
                "    },\n",
                "    False() => exit 1,\n",
                "  },\n",
                "  False() => exit 1,\n",
                "}\n",
                "}\nrun ()\n",
            ),
        )
        .expect("temporary List source should be writable");
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
        .expect("List executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("List executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "List executable exited with {status}");
    }

    #[test]
    #[cfg(unix)]
    fn runs_list_bracket_indexing_mutation_and_iteration() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-list-index-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-list-index-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def run = () => {\n",
                "let mut values: List I32 = List.new ()\n",
                "List.push values 10\n",
                "List.push values 20\n",
                "List.push values 30\n",
                "let index: USize = 1\n",
                "let read = values[index]\n",
                "values[index] = 99\n",
                "let mut sum: I32 = 0\n",
                "for item in values {\n",
                "  sum = sum + item\n",
                "}\n",
                "match read == 20 {\n",
                "  True() => match values[index] == 99 {\n",
                "    True() => match sum == 139 {\n",
                "      True() => exit 0,\n",
                "      False() => exit 3,\n",
                "    },\n",
                "    False() => exit 2,\n",
                "  },\n",
                "  False() => exit 1,\n",
                "}\n",
                "}\nrun ()\n",
            ),
        )
        .expect("temporary List index source should be writable");
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
        .expect("List index executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("List index executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(
            status.success(),
            "List index executable exited with {status}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn runs_list_of_macro() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-list-of-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-list-of-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def run = () => {\n",
                "let empty: List I32 = List.of ()\n",
                "let one: List I32 = List.of (42)\n",
                "let five: List I32 = List.of (1, 2, 3, 4, 5)\n",
                "match List.length empty == 0 {\n",
                "  True() => match List.length one == 1 {\n",
                "    True() => match List.get_unchecked one 0 == 42 {\n",
                "      True() => match List.length five == 5 {\n",
                "        True() => match List.get_unchecked five 0 == 1 {\n",
                "          True() => match List.get_unchecked five 4 == 5 {\n",
                "            True() => exit 0,\n",
                "            False() => exit 6,\n",
                "          },\n",
                "          False() => exit 5,\n",
                "        },\n",
                "        False() => exit 4,\n",
                "      },\n",
                "      False() => exit 3,\n",
                "    },\n",
                "    False() => exit 2,\n",
                "  },\n",
                "  False() => exit 1,\n",
                "}\n",
                "}\nrun ()\n",
            ),
        )
        .expect("temporary List.of source should be writable");
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
        .expect("List.of executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("List.of executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "List.of executable exited with {status}");
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
                "use std.slice.Slice\n",
                "extern \"c\" { exit: I32 -> () }\n",
                "let index: USize = 1\n",
                "let mut product: I32[3] = (10, 20, 30)\n",
                "product[index] = 21\n",
                "let fixed: Ref I32[3] = Ref product\n",
                "let mut erased: Slice I32 = Slice.from_ref fixed\n",
                "erased[index] = 22\n",
                "def score: ((I32, String, Bool), USize) -> I32 = (values, position) => match values[position] {\n",
                "  value: I32 => value,\n",
                "  value: String => 0,\n",
                "  True() => 0,\n",
                "  False() => 1,\n",
                "}\n",
                "let mixed: (I32, String, Bool) = (7, \"text\", False)\n",
                "let mixed_result = (score (mixed, 0) - 7) + score (mixed, 1) + (score (mixed, 2) - 1)\n",
                "let result = mixed_result + (erased[index] - 22) + (fixed[index] - 22) + (product[index] - 21)\n",
                "match Slice.length erased == 3 { True() => exit result, False() => exit 1 }\n",
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
                "extern \"c\" { exit: I32 -> () }\n",
                "type Resource = I32\n",
                "def release = () => { let mut resource = Resource 1; resource = Resource 2 }\n",
                "def abandon = () => { let mut resource = Resource 4; let update = () => { resource = Resource 5 }; update () }\n",
                "def churn: I32 -> () = n => match n == 0 { True() => (), False() => { Ref n; churn (n - 1) } }\n",
                "def update_data = mut data: I32 => { data = 42 }\n",
                "def make_counter = () => {\n",
                "  let mut value = 1\n",
                "  () => { value = value + 1; value }\n",
                "}\n",
                "let counter = make_counter ()\n",
                "let first = counter ()\n",
                "let second = counter ()\n",
                "let mut point = Ref (x: 4, y: 5)\n",
                "point.x = 6\n",
                "let mut scalar = Ref 7\n",
                "let old = Ref.replace (scalar, 8)\n",
                "let Ref current = scalar\n",
                "let mut data = 1\n",
                "update_data data\n",
                "update_data (20 + 22)\n",
                "release ()\n",
                "abandon ()\n",
                "churn 40000\n",
                "churn 40000\n",
                "exit ((first - 2) + (second - 3) + (point.x - 6) + (old - 7) + (current - 8) + (data - 42))\n",
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
    fn runs_implicit_thunks_lazily_with_mutable_captures() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-thunk-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-thunk-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def ignore: <effect E> (() ->{E} I32) -> I32 = callback => 0\n",
                "def twice: (() ->{state} I32) ->{state} I32 = callback => { callback (); callback () }\n",
                "def test = () => {\n",
                "  let mut count = 0\n",
                "  ignore { count = count + 10; count }\n",
                "  let result = twice { count = count + 1; count }\n",
                "  (count - 2) + (result - 2)\n",
                "}\n",
                "exit (test ())\n",
            ),
        )
        .expect("temporary thunk source should be writable");
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
        .expect("implicit-thunk executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("implicit-thunk executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_signals_and_scoped_reactions_synchronously() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-reactive-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-reactive-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "let signal count = 0\n",
                "let mut observed = 0\n",
                "with Reactive = reactive_scope () {\n",
                "  reaction { observed = count; () }\n",
                "  count = 7\n",
                "}\n",
                "count = 8\n",
                "exit (observed - 7)\n",
            ),
        )
        .expect("temporary reactive source should be writable");
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
        .expect("reactive executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("reactive executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn batches_reactions_in_creation_order_and_coalesces_nested_writes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-batch-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-batch-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "let signal value = 0\n",
                "let mut order = 0\n",
                "let mut first_runs = 0\n",
                "let mut second_runs = 0\n",
                "let mut observed = 0\n",
                "let mut during = 0\n",
                "with Reactive = reactive_scope () {\n",
                "  reaction { let current = value; observed = current; first_runs = first_runs + 1; order = order * 10 + 1; () }\n",
                "  reaction { let current = value; observed = current; second_runs = second_runs + 1; order = order * 10 + 2; () }\n",
                "  order = 0\n",
                "  first_runs = 0\n",
                "  second_runs = 0\n",
                "  batch {\n",
                "    value = 1\n",
                "    batch { value = 2; value = 3; () }\n",
                "    during = order + value\n",
                "    ()\n",
                "  }\n",
                "}\n",
                "exit ((order - 12) + (first_runs - 1) * 10 + (second_runs - 1) * 100 + (observed - 3) * 1000 + (during - 3) * 10000)\n",
            ),
        )
        .expect("temporary batch source should be writable");
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
        .expect("batched reaction executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("batched reaction executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "batch executable exited with {status}");
    }

    #[test]
    #[cfg(unix)]
    fn reruns_self_triggering_reactions_iteratively() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-self-reaction-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-self-reaction-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "let signal count = 0\n",
                "let mut runs = 0\n",
                "def update = () => {\n",
                "  let current = count\n",
                "  runs = runs + 1\n",
                "  match current < 10 { True() => { count = current + 1; () }, False() => (), }\n",
                "}\n",
                "reaction update\n",
                "exit ((count - 10) + (runs - 11) * 100)\n",
            ),
        )
        .expect("temporary self-triggering source should be writable");
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
        .expect("self-triggering reaction executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("self-triggering reaction executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(
            status.success(),
            "self-triggering reaction executable exited with {status}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn keeps_queued_reaction_captures_rooted_and_skips_disposed_work() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-queued-reaction-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-queued-reaction-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def churn: I32 -> () = n => match n == 0 { True() => (), False() => { Ref n; churn (n - 1) } }\n",
                "let mut disposed_runs = 0\n",
                "let mut observed = 0\n",
                "let captured = Ref 42\n",
                "batch {\n",
                "  with Reactive = reactive_scope () { reaction { disposed_runs = disposed_runs + 1; () } }\n",
                "  ()\n",
                "}\n",
                "with Reactive = reactive_scope () {\n",
                "  batch {\n",
                "    reaction { let Ref value = captured; observed = value; () }\n",
                "    churn 40000\n",
                "    churn 40000\n",
                "    ()\n",
                "  }\n",
                "}\n",
                "exit (disposed_runs + (observed - 42) * 10)\n",
            ),
        )
        .expect("temporary queued reaction source should be writable");
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
        .expect("queued reaction executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("queued reaction executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(
            status.success(),
            "queued reaction executable exited with {status}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn reports_reactive_nonconvergence() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-nonconvergent-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-nonconvergent-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "let signal count = 0\n",
                "reaction { let current = count; count = current + 1; () }\n",
            ),
        )
        .expect("temporary nonconvergent source should be writable");
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
        .expect("nonconvergent reaction executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("nonconvergent reaction executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(!result.status.success());
        assert_eq!(
            String::from_utf8_lossy(&result.stderr),
            "reactive update did not stabilize after 100000 executions\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn runs_top_level_signals_and_reactions_in_the_entry_module() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-main-reactive-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-main-reactive-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "let signal count = 0\n",
                "let mut observed = 0\n",
                "reaction { observed = count; () }\n",
                "count = 7\n",
                "exit (observed - 7)\n",
            ),
        )
        .expect("temporary top-level reactive source should be writable");
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
        .expect("top-level reactive executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("top-level reactive executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_lazy_persistent_derived_bindings() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-derived-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-derived-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "let signal count = 2\n",
                "let mut observed = 0\n",
                "let mut runs = 0\n",
                "let mut frozen_runs = 0\n",
                "with Reactive = reactive_scope () {\n",
                "  let left = count + 1\n",
                "  let right = count + 2\n",
                "  let total = left + right\n",
                "  reaction { observed = total; runs = runs + 1; () }\n",
                "  reaction { let frozen = snapshot count; frozen_runs = frozen_runs + 1; () }\n",
                "  count = 7\n",
                "}\n",
                "exit ((observed - 17) + (runs - 2) + (frozen_runs - 1))\n",
            ),
        )
        .expect("temporary derived source should be writable");
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
        .expect("derived executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("derived executable should run");
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
                "extern \"c\" { exit: I32 -> () }\n",
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
                "extern \"c\" { exit: I32 -> () }\n",
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
    fn runs_named_product_value_spreads() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-named-product-spread-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-named-product-spread-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "let dimensions = (height: 600, width: 800)\n",
                "let config: (width: I32, height: I32, title: String) = (\n",
                "    ...=dimensions,\n",
                "    title: \"Staple\",\n",
                ")\n",
                "let overridden: (width: I32, height: I32) = (\n",
                "    ...=dimensions,\n",
                "    width: 900,\n",
                ")\n",
                "exit ((config.width - 800) + (config.height - 600) * 2 + (overridden.width - 900) * 4 + (overridden.height - 600) * 8)\n",
            ),
        )
        .expect("temporary named-product-spread source should be writable");
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
        .expect("named-product-spread executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("named-product-spread executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_contextual_named_product_initializers_in_source_order() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-designated-product-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-designated-product-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "let mut calls = 0\n",
                "def next_value = () => { calls = calls + 1; calls }\n",
                "let value: (I32, a: I32, b: I32) = (next_value (), .b: next_value (), .a: next_value ())\n",
                "exit ((value.0 - 1) + (value.a - 3) * 2 + (value.b - 2) * 4 + (calls - 3) * 8)\n",
            ),
        )
        .expect("temporary designated-product source should be writable");
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
        .expect("designated-product executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("designated-product executable should run");
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
                "extern \"c\" { exit: I32 -> () }\n",
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
                "extern \"c\" { exit: I32 -> () }\n",
                "use std.cinterop.(CString)\n",
                "type Some = String\n",
                "def roundtrip: String -> String = value => CString.to_string (CString.from_string value)\n",
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
                "extern \"c\" { exit: I32 -> () }\n",
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
    fn runs_for_loops_and_integer_ranges() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-for-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-for-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def run = () => {\n",
                "let mut total: I32 = 0\n",
                "for value in (0 ..= 4) {\n",
                "  match value == 2 { True() => { continue }, False() => () }\n",
                "  total = total + value\n",
                "}\n",
                "let mut maximum_count: I32 = 0\n",
                "for _ in (2147483647 ..= 2147483647) {\n",
                "  maximum_count = maximum_count + 1\n",
                "}\n",
                "(total - 8) + (maximum_count - 1)\n",
                "}\n",
                "exit (run ())\n",
            ),
        )
        .expect("temporary for-loop source should be writable");
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
        .expect("for-loop executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("for-loop executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "for-loop executable returned {status}");
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
                "extern \"c\" { exit: I32 -> () }\n",
                "def score_bool: Bool -> I32 = value => match value { True() => 1, False() => 0, }\n",
                "let nan = 0.0 / 0.0\n",
                "let partial_score = match (PartialOrd.partial_cmp nan 1.0) { None() => 0, Some _ => 1, }\n",
                "let less_score = match (Ord.cmp 1 2) { Less() => 0, _ => 1, }\n",
                "let equal_score = match (Ord.cmp 2 2) { Equal() => 0, _ => 1, }\n",
                "let greater_score = match (Ord.cmp 3 2) { Greater() => 0, _ => 1, }\n",
                "let single: F32 = (1.5 satisfies F32) + .5\n",
                "exit (partial_score + less_score + equal_score + greater_score + score_bool (nan < 1.0) + score_bool (nan == nan) + (score_bool (nan != nan) - 1) + (score_bool (single == 2.0) - 1))\n",
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

    #[test]
    #[cfg(unix)]
    fn runs_to_string_for_prelude_scalar_types() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-to-string-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-to-string-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def classify: String -> I32 = value => match value {\n",
                "  \"-42\" => 1, \"42\" => 2, \"1.5\" => 3,\n",
                "  \"True\" => 4, \"False\" => 5, \"hé\" => 6, _ => 100,\n",
                "}\n",
                "let integer = 42\n",
                "let integer_i8: I8 = 42\nlet integer_i16: I16 = 42\n",
                "let integer_i64: I64 = 42\nlet integer_isize: ISize = 42\n",
                "let boolean_true: Bool = True\nlet boolean_false: Bool = False\n",
                "let text: String = \"hé\"\n",
                "let score =\n",
                "  (classify (to_string integer_i8) - 2) +\n",
                "  (classify (to_string integer_i16) - 2) +\n",
                "  (classify (to_string integer) - 2) +\n",
                "  (classify (to_string integer_i64) - 2) +\n",
                "  (classify (to_string (42 satisfies U8)) - 2) +\n",
                "  (classify (to_string (42 satisfies U16)) - 2) +\n",
                "  (classify (to_string (42 satisfies U32)) - 2) +\n",
                "  (classify (to_string (42 satisfies U64)) - 2) +\n",
                "  (classify (to_string integer_isize) - 2) +\n",
                "  (classify (to_string 42) - 2) +\n",
                "  (classify (to_string (1.5 satisfies F32)) - 3) +\n",
                "  (classify (to_string (1.5 satisfies F64)) - 3) +\n",
                "  (classify (to_string boolean_true) - 4) +\n",
                "  (classify (to_string boolean_false) - 5) +\n",
                "  (classify (to_string text) - 6)\n",
                "exit score\n",
            ),
        )
        .expect("temporary ToString source should be writable");
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
        .expect("ToString executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("ToString executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "ToString executable returned {status}");
    }

    #[test]
    #[cfg(unix)]
    fn runs_string_templates_end_to_end() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-string-template-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-string-template-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "let name: String = \"wörld\"\n",
                "let answer: I32 = 42\n",
                "let pair = (answer, name)\n",
                "let rendered = \"hello $name: ${answer}; ${pair:?}; \\$5\"\n",
                "let status = match rendered {\n",
                "  \"hello wörld: 42; (42, \\\"wörld\\\"); \\$5\" => 0,\n",
                "  _ => 1,\n",
                "}\n",
                "exit status\n",
            ),
        )
        .expect("temporary string-template source should be writable");
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
        .expect("string-template executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("string-template executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(
            status.success(),
            "string-template executable returned {status}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn concatenates_strings_with_add() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("stapler-string-add-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("stapler-string-add-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def score: String -> I32 = value => match value {\n",
                "  \"hello world\" => 0, \"héllo 🌏\" => 0, \"\" => 0, _ => 1,\n",
                "}\n",
                "let greeting = \"hello \" + \"world\"\n",
                "let unicode = \"héllo \" + \"🌏\"\n",
                "let empty = \"\" + \"\"\n",
                "exit (score greeting + score unicode + score empty)\n",
            ),
        )
        .expect("temporary string-add source should be writable");
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
        .expect("string-add executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("string-add executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success(), "string-add executable returned {status}");
    }
}
