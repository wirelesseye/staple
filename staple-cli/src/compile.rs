use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

use staple_compiler::{
    CodeGenerator, NameResolver, Program, ProgramLoader, TypeChecker, TypedModule, expand_macros,
    render_expanded_module,
};
use staple_syntax::{Diagnostic, format_source};

use crate::Outcome;

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
    Format,
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
    format_check: bool,
}

pub fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<Outcome, String> {
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
        if command == "fmt" {
            return Ok(Outcome::Completed(Some(format!("{}\n", fmt_usage()))));
        }
    }
    let options = parse_options(arguments)?;
    if options.mode == Mode::Format {
        return format_input(&options);
    }
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
    loader = loader.with_feature_selection(staple_project::FeatureSelection {
        features: options.features.clone(),
        all_features: options.all_features,
        no_default_features: options.no_default_features,
    });
    let program = if let Some(manifest) = &options.manifest_path {
        let graph = staple_project::load_package_graph(manifest)?;
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
        let rendered = render_expanded_module(&expanded).map_err(|error| error.to_string())?;
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
        Some("fmt") => {
            arguments.next();
            Mode::Format
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
        format_check: false,
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
        if !positional_only && argument == "--check" && options.mode == Mode::Format {
            options.format_check = true;
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
                Mode::Format => fmt_usage(),
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
                Mode::Format => fmt_usage(),
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
            Mode::Format => fmt_usage(),
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
            return Err("`-o` is not supported by `staple run`".to_owned());
        }
        if emit_specified {
            return Err("`--emit` is not supported by `staple run`".to_owned());
        }
        if options.target.is_some() {
            return Err(
                "`--target` is not supported by `staple run`; programs run on the host target"
                    .to_owned(),
            );
        }
    }
    if options.mode == Mode::Check {
        if options.output.is_some() {
            return Err("`-o` is not supported by `staple check`".to_owned());
        }
        if emit_specified {
            return Err("`--emit` is not supported by `staple check`".to_owned());
        }
        if options.target.is_some() {
            return Err("`--target` is not supported by `staple check`".to_owned());
        }
        if options.linker.is_some()
            || !options.library_paths.is_empty()
            || !options.libraries.is_empty()
        {
            return Err("linker options are not supported by `staple check`".to_owned());
        }
    }
    if options.mode == Mode::Expand {
        if emit_specified {
            return Err("`--emit` is not supported by `staple expand`".to_owned());
        }
        if options.target.is_some() {
            return Err("`--target` is not supported by `staple expand`".to_owned());
        }
        if options.linker.is_some()
            || !options.library_paths.is_empty()
            || !options.libraries.is_empty()
        {
            return Err("linker options are not supported by `staple expand`".to_owned());
        }
    }
    if options.mode == Mode::Format {
        if options.output.is_some()
            || emit_specified
            || options.target.is_some()
            || options.linker.is_some()
            || options.standard_library.is_some()
            || options.module_root.is_some()
            || options.package_root.is_some()
            || options.package_name.is_some()
            || options.manifest_path.is_some()
            || !options.features.is_empty()
            || options.all_features
            || options.no_default_features
            || !options.library_paths.is_empty()
            || !options.libraries.is_empty()
        {
            return Err(
                "`staple fmt` supports only `--check` and one input file (or `-`)".to_owned(),
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

fn format_input(options: &Options) -> Result<Outcome, String> {
    let source = read_source(&options.input)?;
    let formatted = format_source(&source).map_err(|error| error.to_string())?;
    if options.format_check {
        return if formatted == source {
            Ok(Outcome::Completed(None))
        } else {
            Ok(Outcome::FormatMismatch(vec![
                options.input.to_string_lossy().into_owned(),
            ]))
        };
    }
    if options.input == "-" {
        return Ok(Outcome::Completed(Some(formatted)));
    }
    if formatted != source {
        atomic_write(Path::new(&options.input), formatted.as_bytes())?;
    }
    Ok(Outcome::Completed(None))
}

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("could not inspect `{}`: {error}", path.display()))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().unwrap_or_else(|| OsStr::new("source.sta"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".{}.staple-fmt-{}-{nonce}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("could not create `{}`: {error}", temporary.display()))?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("could not write `{}`: {error}", temporary.display()))?;
        std::fs::set_permissions(&temporary, metadata.permissions()).map_err(|error| {
            format!(
                "could not preserve permissions for `{}`: {error}",
                path.display()
            )
        })?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("could not replace `{}`: {error}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn add_features(destination: &mut Vec<String>, value: &str) -> Result<(), String> {
    for feature in value.split(',') {
        staple_project::validate_feature_name(feature)?;
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
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let program = ProgramLoader::new()
        .with_standard_library_root(workspace_root.join("stdlib"))
        .load_source(source, workspace_root)?;
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
                "staple-{}-{nonce}-{kind}{extension}",
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

pub(crate) fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map(ExitCode::from)
        .unwrap_or(ExitCode::FAILURE)
}

pub(crate) fn usage() -> String {
    concat!(
        "usage: staple compile [options] <input.sta>\n",
        "       staple check [options] <input.sta>\n",
        "       staple run [options] <input.sta> [-- <arguments>...]\n",
        "       staple expand [options] <input.sta>\n",
        "       staple fmt [--check] [--manifest-path <path>] [input.sta|-]\n",
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
        "  --manifest-path <path>    load a package manifest graph instead of an input\n",
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
        "usage: staple run [options] <input.sta> [-- <arguments>...]\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  --linker <command>        linker driver (default: $CC or cc)\n",
        "  --stdlib <path>           Staple standard-library root\n",
        "  --module-root <path>      package module directory (default: entry directory)\n",
        "  --package-root <path>     optional package root module\n",
        "  --package-name <name>     package name used by tooling\n",
        "  --manifest-path <path>    load a package manifest graph instead of an input\n",
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
        "usage: staple check [options] <input.sta>\n",
        "\n",
        "options:\n",
        "  -h, --help                print this help\n",
        "  --stdlib <path>           Staple standard-library root\n",
        "  --module-root <path>      package module directory (default: entry directory)\n",
        "  --package-root <path>     optional package root module\n",
        "  --package-name <name>     package name used by tooling\n",
        "  --manifest-path <path>    load a package manifest graph instead of an input\n",
        "  --features <names>        enable package features\n",
        "  --all-features            enable every package feature\n",
        "  --no-default-features     disable default package features\n",
        "  --                         stop parsing options",
    )
    .to_owned()
}

fn expand_usage() -> String {
    concat!(
        "usage: staple expand [options] <input.sta>\n",
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

pub(crate) fn fmt_usage() -> String {
    concat!(
        "usage: staple fmt [--check] [--manifest-path <path>] [input.sta|-]\n",
        "\n",
        "With no input, formats every `.sta` file in the current package.\n",
        "Given a file, formats just that file; given `-`, reads standard input\n",
        "and writes the formatted source to standard output. Macros are neither\n",
        "expanded nor resolved. Files are rewritten atomically.\n",
        "\n",
        "options:\n",
        "  -h, --help              print this help\n",
        "  --check                 fail without writing when an input is not formatted\n",
        "  --manifest-path <path>  format the package described by a specific staple.kdl",
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
            "staple-expand-{name}-{}.sta",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&path, source).expect("temporary expand source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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

    fn temporary_source(name: &str, source: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "staple-fmt-{name}-{}-{}.sta",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&path, source).unwrap();
        path
    }

    #[test]
    fn parses_fmt_subcommand_and_check() {
        let options = parse_options(["fmt".into(), "--check".into(), "input.sta".into()])
            .expect("fmt options should parse");
        assert_eq!(options.mode, Mode::Format);
        assert!(options.format_check);
    }

    #[test]
    fn fmt_help_describes_rewriting_and_checking() {
        let Outcome::Completed(Some(help)) =
            run(["fmt".into(), "--help".into()]).expect("fmt help should succeed")
        else {
            panic!("fmt help should be printed");
        };
        assert!(help.starts_with("usage: staple fmt"));
        assert!(help.contains("rewritten atomically"));
        assert!(help.contains("--check"));
    }

    #[test]
    fn fmt_rewrites_a_file_and_then_passes_check() {
        let path = temporary_source("rewrite", "let   answer=42");
        let outcome = run(["fmt".into(), path.clone().into_os_string()]).unwrap();
        assert!(matches!(outcome, Outcome::Completed(None)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "let answer = 42\n");
        let outcome = run([
            "fmt".into(),
            "--check".into(),
            path.clone().into_os_string(),
        ])
        .unwrap();
        assert!(matches!(outcome, Outcome::Completed(None)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fmt_check_does_not_rewrite_unformatted_input() {
        let source = "let answer=42\n";
        let path = temporary_source("check", source);
        let outcome = run([
            "fmt".into(),
            "--check".into(),
            path.clone().into_os_string(),
        ])
        .unwrap();
        assert!(matches!(outcome, Outcome::FormatMismatch(_)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fmt_parse_failure_leaves_the_file_unchanged() {
        let source = "let =\n";
        let path = temporary_source("invalid", source);
        assert!(run(["fmt".into(), path.clone().into_os_string()]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn fmt_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let path = temporary_source("permissions", "let answer=42\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        run(["fmt".into(), path.clone().into_os_string()]).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let _ = std::fs::remove_file(path);
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
        assert!(help.starts_with("usage: staple expand"));
        assert!(help.contains("macro expansion"));
    }

    #[test]
    fn expand_rejects_emit_option() {
        let error = parse_options(["expand".into(), "--emit=llvm".into(), "input.sta".into()])
            .expect_err("expand should reject --emit");
        assert!(error.contains("`--emit` is not supported by `staple expand`"));
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
    fn expand_materializes_declaration_name_and_type_splices() {
        let expanded = expand_source(
            "declaration-splices",
            concat!(
                "use std.syntax.(parse_quote, Ident, Type, Item)\n",
                "macro make_alias: Ident String * Type -> Item = name * ty => parse_quote { type alias $name = $ty }\n",
                "make_alias Generated I32\n",
            ),
        );
        assert!(expanded.contains("type alias Generated = I32"));
        assert_eq!(expanded.matches("type alias $name = $ty").count(), 1);
        staple_syntax::parse(&expanded).expect("expanded declaration should parse");
        assert_eq!(staple_syntax::format_source(&expanded).unwrap(), expanded);
    }

    #[test]
    fn expand_uses_the_expanded_flattened_inline_module() {
        let expanded = expand_source(
            "inline-module",
            concat!(
                "mod inner {\n",
                "    use std.syntax.(quote, Expr)\n",
                "    macro double = value: Expr => quote { ($value) + ($value) }\n",
                "    let value: I32 = double 2\n",
                "}\n",
            ),
        );
        assert!(expanded.contains("let value: I32 = (2) + (2)"));
        assert!(!expanded.contains("let value: I32 = double 2"));
        staple_syntax::parse(&expanded).expect("expanded inline module should parse");
    }

    #[test]
    fn expand_output_uses_the_shared_formatter_canonically() {
        let expanded = expand_source(
            "formatted",
            concat!(
                "// keep this comment\r\n",
                "let condition:Bool=True\r\n",
                "let selected:I32=if condition {1}else {2}\r\n",
                "let clauses:I32=when {condition=>selected,else=>0}\r\n",
            ),
        );
        assert!(expanded.contains("// keep this comment\n"));
        assert!(!expanded.contains('\r'));
        staple_syntax::parse(&expanded).expect("valid expanded output should still parse");
        assert_eq!(
            staple_syntax::format_source(&expanded).expect("expanded output should format"),
            expanded,
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

        assert!(help.starts_with("usage: staple compile"));
        assert!(help.contains("--emit <llvm|object|exe>"));
        assert!(help.contains("staple run"));
    }

    #[test]
    fn prints_run_help_without_an_input() {
        let Outcome::Completed(Some(help)) =
            run(["run".into(), "--help".into()]).expect("run help should succeed")
        else {
            panic!("run help should be printed");
        };

        assert!(help.starts_with("usage: staple run"));
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
            "staple.kdl".into(),
        ])
        .expect("manifest mode should derive its input from Binder");
        assert_eq!(options.manifest_path, Some(PathBuf::from("staple.kdl")));
        assert!(options.input.is_empty());
        assert!(
            parse_options([
                "check".into(),
                "--manifest-path=staple.kdl".into(),
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
            assert!(error.contains("not supported by `staple check`"));
        }
    }

    #[test]
    fn checks_a_nested_entry_without_creating_an_artifact() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("staple-compiler-check-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");

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
            assert!(error.contains("not supported by `staple run`"));
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
        let source = std::env::temp_dir().join(format!("staple-compiler-run-{nonce}.sta"));
        std::fs::write(&source, "extern \"c\" { exit: I32 -> () }\nexit 7\n")
            .expect("temporary run source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-run-const-{nonce}.sta"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-run-error-{nonce}.sta"));
        std::fs::write(&source, "missing\n").expect("temporary invalid source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-run-linker-{nonce}.sta"));
        std::fs::write(&source, "()\n").expect("temporary source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
        let error = run([
            "run".into(),
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--linker".into(),
            format!("staple-compiler-missing-linker-{nonce}").into(),
            source.clone().into_os_string(),
        ])
        .expect_err("a missing linker should be reported");
        let _ = std::fs::remove_file(source);

        assert!(error.contains("could not run linker"));
    }

    #[test]
    fn compiles_source_to_llvm() {
        let module =
            compile(include_str!("../../staple-compiler/examples/hello_world.sta")).expect("example should compile");
        let context = inkwell::context::Context::create();
        let llvm = staple_compiler::CodeGenerator::new(&context)
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
            compile(include_str!("../../staple-compiler/examples/hello_world.sta")).expect("example should compile");
        let context = inkwell::context::Context::create();
        let llvm = staple_compiler::CodeGenerator::new(&context)
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
        let llvm = staple_compiler::CodeGenerator::new(&context)
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
        let source = std::env::temp_dir().join(format!("staple-compiler-main-io-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-main-io-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
    fn exit_terminates_the_process_with_the_given_code() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-process-exit-{nonce}.sta"));
        std::fs::write(&source, "use std.process.exit\nexit 7\n")
            .expect("temporary exit source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
        let outcome = run([
            "run".into(),
            "--stdlib".into(),
            standard_library.into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("exit source should run");
        let _ = std::fs::remove_file(source);

        let Outcome::Executed(status) = outcome else {
            panic!("run mode should return a process status");
        };
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    #[cfg(unix)]
    fn panic_prints_its_message_and_terminates_the_process() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-panic-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-panic-{nonce}"));
        std::fs::write(&source, "use std.process.panic\npanic \"boom\"\n")
            .expect("temporary panic source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("panic executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("panic executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(!result.status.success(), "panic should exit unsuccessfully");
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("boom"),
            "expected panic message in stdout: {}",
            String::from_utf8_lossy(&result.stdout),
        );
    }

    #[test]
    #[cfg(unix)]
    fn runs_coroutines_with_nested_await_and_effect_ordering() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-coro-run-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-coro-run-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def worker: I32 -> Coroutine{IO} I32 = n => coro {\n",
                "    println \"worker\"\n",
                "    n * 10\n",
                "}\n",
                "def driver: () -> Coroutine{IO} I32 = () => coro {\n",
                "    let a = await (worker 4)\n",
                "    println \"driver resumed\"\n",
                "    a + 2\n",
                "}\n",
                "def sum_to: I32 -> Coroutine{} I32 = n => coro {\n",
                "    match n <= 0 {\n",
                "        True() => 0,\n",
                "        False() => {\n",
                "            let rest = await (sum_to (n - 1))\n",
                "            rest + n\n",
                "        },\n",
                "    }\n",
                "}\n",
                "println \"driver: ${block_on (driver ()):?}\"\n",
                "println \"sum_to 50: ${block_on (sum_to 50):?}\"\n",
            ),
        )
        .expect("temporary coroutine source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("coroutine executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("coroutine executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "coroutine program exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            "worker\ndriver resumed\ndriver: 42\nsum_to 50: 1275\n",
        );
    }

    #[test]
    #[cfg(unix)]
    fn scheduler_pumps_spawned_tasks_with_fifo_carryover_and_a_budget() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-sched-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-sched-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def job: I32 -> Coroutine{Tasks, IO} () = id => coro {\n",
                "    println \"job ${id:?} start\"\n",
                "    let _ = await (yield_now ())\n",
                "    println \"job ${id:?} done\"\n",
                "}\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                "    let a = spawn (job 1)\n",
                "    let _ = spawn (job 2)\n",
                "    let _ = spawn (job 3)\n",
                "    println \"a finished before pump: ${Task.is_finished a:?}\"\n",
                "    let r1 = pump (sched, 2)\n",
                "    println \"pump 1: e=${r1.executed:?} ready=${r1.ready:?}\"\n",
                "    let r2 = pump (sched, 10)\n",
                "    println \"pump 2: e=${r2.executed:?} ready=${r2.ready:?}\"\n",
                "    let r3 = pump (sched, 10)\n",
                "    println \"pump 3: e=${r3.executed:?} ready=${r3.ready:?}\"\n",
                "    println \"a finished after: ${Task.is_finished a:?}\"\n",
                "}\n",
            ),
        )
        .expect("temporary scheduler source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("scheduler executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("scheduler executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "scheduler program exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "a finished before pump: False\n",
                // pump 1, budget 2: jobs 1 and 2 start and yield; job 3 is not
                // reached, so `ready` = job 3 + the two yielded continuations.
                "job 1 start\n",
                "job 2 start\n",
                "pump 1: e=2 ready=3\n",
                // pump 2: the un-run job 3 (most senior) runs first and yields,
                // then the carried-over continuations of jobs 1 and 2 finish.
                "job 3 start\n",
                "job 1 done\n",
                "job 2 done\n",
                "pump 2: e=3 ready=1\n",
                // pump 3: job 3's continuation finishes.
                "job 3 done\n",
                "pump 3: e=1 ready=0\n",
                "a finished after: True\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn awaiting_a_spawned_task_delivers_its_completed_value() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-await-task-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-await-task-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def answer: I32 -> Coroutine{} I32 = base => coro { base + 2 }\n",
                "def parent: () -> Coroutine{Tasks, IO} () = () => coro {\n",
                "    let w = spawn (answer 40)\n",
                "    println \"parent awaiting\"\n",
                "    let r = await w\n",
                "    match r {\n",
                "        Completed v => println \"parent got ${v:?}\",\n",
                "        Cancelled() => println \"parent saw cancellation\",\n",
                "    }\n",
                "}\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                "    let p = spawn (parent ())\n",
                "    println \"p finished at start: ${Task.is_finished p:?}\"\n",
                "    let r1 = pump (sched, 10)\n",
                "    println \"pump 1: e=${r1.executed:?} ready=${r1.ready:?}\"\n",
                "    let r2 = pump (sched, 10)\n",
                "    println \"pump 2: e=${r2.executed:?} ready=${r2.ready:?}\"\n",
                "    let r3 = pump (sched, 10)\n",
                "    println \"pump 3: e=${r3.executed:?} ready=${r3.ready:?}\"\n",
                "    println \"p finished after: ${Task.is_finished p:?}\"\n",
                "}\n",
            ),
        )
        .expect("temporary await-task source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("await-task executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("await-task executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "await-task program exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "p finished at start: False\n",
                // pump 1 runs `parent`, which spawns `answer 40` and parks on
                // `await w`; the spawned task is eligible next pump.
                "parent awaiting\n",
                "pump 1: e=1 ready=1\n",
                // pump 2 runs `answer 40` to completion; finishing it re-queues
                // the waiting `parent`.
                "pump 2: e=1 ready=1\n",
                // pump 3 resumes `parent` past the await with `Completed 42`.
                "parent got 42\n",
                "pump 3: e=1 ready=0\n",
                "p finished after: True\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn cancelling_tasks_unwinds_them_at_the_next_boundary() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-cancel-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-cancel-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def quiet: () -> Coroutine{IO} () = () => coro { println \"quiet ran\" }\n",
                "def stepper: I32 -> Coroutine{Tasks, IO} () = id => coro {\n",
                "    println \"step ${id:?} start\"\n",
                "    let _ = await (yield_now ())\n",
                "    println \"step ${id:?} resumed\"\n",
                "}\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                "    let a = spawn (quiet ())\n",
                "    Task.cancel a\n",
                "    println \"a finished before pump: ${Task.is_finished a:?}\"\n",
                "    let _ = pump (sched, 10)\n",
                "    println \"a finished after pump: ${Task.is_finished a:?}\"\n",
                "    let b = spawn (stepper 1)\n",
                "    let _ = pump (sched, 10)\n",
                "    println \"b finished at yield: ${Task.is_finished b:?}\"\n",
                "    Task.cancel b\n",
                "    let _ = pump (sched, 10)\n",
                "    println \"b finished after cancel: ${Task.is_finished b:?}\"\n",
                "    let _ = spawn (quiet ())\n",
                "}\n",
                // The scope closed with a task still queued: teardown cancelled
                // it, so pumping the surviving scheduler never runs its body.
                "let after = pump (sched, 10)\n",
                "println \"post-close pump executed: ${after.executed:?}\"\n",
            ),
        )
        .expect("temporary cancellation source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("cancellation executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("cancellation executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "cancellation program exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                // `a` is cancelled while still queued: `is_finished` only turns
                // true once the pump has actually driven it into its unwind, and
                // its body never runs.
                "a finished before pump: False\n",
                "a finished after pump: True\n",
                // `b` runs to its `yield_now`, is cancelled there, and unwinds on
                // the next pump without printing its "resumed" line.
                "step 1 start\n",
                "b finished at yield: False\n",
                "b finished after cancel: True\n",
                // The last `spawn` is still queued at scope close; teardown
                // cancels it, so the post-close pump drives only the spent frame
                // and its body never prints.
                "post-close pump executed: 1\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn pump_traps_inside_a_reaction_or_an_open_batch() {
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        for (label, program) in [
            (
                "reaction",
                concat!(
                    "use std.coroutine.*\n",
                    "let signal tick = 0\n",
                    "let sched = scheduler ()\n",
                    "with Reactive = reactive_scope () {\n",
                    "    reaction {\n",
                    "        let _ = tick\n",
                    "        let _ = pump (sched, 1)\n",
                    "        ()\n",
                    "    }\n",
                    "    tick = 1\n",
                    "}\n",
                ),
            ),
            (
                "batch",
                concat!(
                    "use std.coroutine.*\n",
                    "let sched = scheduler ()\n",
                    "batch {\n",
                    "    let _ = pump (sched, 1)\n",
                    "    ()\n",
                    "}\n",
                ),
            ),
        ] {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let source =
                std::env::temp_dir().join(format!("staple-compiler-pumpguard-{label}-{nonce}.sta"));
            let output =
                std::env::temp_dir().join(format!("staple-compiler-pumpguard-{label}-{nonce}"));
            std::fs::write(&source, program).expect("temporary pump-guard source should be writable");
            run([
                "--stdlib".into(),
                standard_library.clone().into_os_string(),
                "--emit".into(),
                "exe".into(),
                "-o".into(),
                output.clone().into_os_string(),
                source.clone().into_os_string(),
            ])
            .unwrap_or_else(|_| panic!("pump-guard ({label}) executable should compile"));
            let result = Command::new(&output)
                .output()
                .unwrap_or_else(|_| panic!("pump-guard ({label}) executable should run"));
            let _ = std::fs::remove_file(&source);
            let _ = std::fs::remove_file(&output);
            assert!(
                !result.status.success(),
                "`pump` inside a {label} should trap, but the program exited cleanly"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn the_scheduler_drains_to_empty_after_many_task_and_cancel_cycles() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-drain-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-drain-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def child: () -> Coroutine{} I32 = () => coro { 7 }\n",
                "def waiter: () -> Coroutine{Tasks, IO} I32 = () => coro {\n",
                "    let c = spawn (child ())\n",
                "    let r = await c\n",
                "    match r { Completed v => v, Cancelled() => 0 }\n",
                "}\n",
                "def stepper: () -> Coroutine{Tasks} () = () => coro {\n",
                "    let _ = await (yield_now ())\n",
                "    ()\n",
                "}\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                "    let mut i = 0\n",
                "    while (i < 300) {\n",
                "        let _ = spawn (waiter ())\n",
                "        i = i + 1\n",
                "    }\n",
                "    let mut rounds = 0\n",
                "    while (rounds < 40) {\n",
                "        let _ = pump (sched, 4096)\n",
                "        rounds = rounds + 1\n",
                "    }\n",
                "    let settled = pump (sched, 4096)\n",
                "    println \"waiters settled: executed=${settled.executed:?} ready=${settled.ready:?}\"\n",
                "    let mut c = 0\n",
                "    while (c < 200) {\n",
                "        let t = spawn (stepper ())\n",
                "        let _ = pump (sched, 1)\n",
                "        Task.cancel t\n",
                "        let _ = pump (sched, 8)\n",
                "        c = c + 1\n",
                "    }\n",
                "    let mut d = 0\n",
                "    while (d < 40) {\n",
                "        let _ = pump (sched, 4096)\n",
                "        d = d + 1\n",
                "    }\n",
                "    let after = pump (sched, 4096)\n",
                "    println \"cancels settled: executed=${after.executed:?} ready=${after.ready:?}\"\n",
                "}\n",
            ),
        )
        .expect("temporary drain source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("drain executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("drain executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "drain program exited with {}",
            result.status
        );
        // After 300 spawn/await/complete tasks and 200 spawn/yield/cancel cycles,
        // the scheduler's ready queue is empty again — no leaked or re-queued work.
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "waiters settled: executed=0 ready=0\n",
                "cancels settled: executed=0 ready=0\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_completion_delivers_its_value_across_resolve_orderings() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-completion-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-completion-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def observe: (I32, move Wait I32) ->{IO} Coroutine{IO} () = (id, move w) => coro {\n",
                "    println \"task ${id:?} parked\"\n",
                "    let outcome = await w\n",
                "    match outcome {\n",
                "        Completed v => println \"task ${id:?} got ${v:?}\",\n",
                "        Cancelled() => println \"task ${id:?} cancelled\",\n",
                "    }\n",
                "}\n",
                "def make: Scheduler -> (wait: Wait I32, resolver: Resolver I32) = s => completion s\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                // (1) resolve after the task parks
                "    let (w1, r1) = make sched\n",
                "    let _ = spawn (observe (1, w1))\n",
                "    let _ = pump (sched, 8)\n",
                "    Resolver.complete (r1, 10)\n",
                "    let _ = pump (sched, 8)\n",
                // (2) resolve before the task ever runs — fast path, one resume
                "    let (w2, r2) = make sched\n",
                "    let _ = spawn (observe (2, w2))\n",
                "    Resolver.complete (r2, 20)\n",
                "    let p2 = pump (sched, 8)\n",
                "    println \"pump 2: executed=${p2.executed:?}\"\n",
                // (3) drop the resolver unresolved -> Cancelled
                "    let (w3, r3) = make sched\n",
                "    let _ = spawn (observe (3, w3))\n",
                "    let _ = pump (sched, 8)\n",
                "    let _ = r3\n",
                "    let _ = pump (sched, 8)\n",
                // (4) cancel a task parked on a wait -> unwinds, no value delivered
                "    let (w4, r4) = make sched\n",
                "    let h4 = spawn (observe (4, w4))\n",
                "    let _ = pump (sched, 8)\n",
                "    Task.cancel h4\n",
                "    let _ = pump (sched, 8)\n",
                "    Resolver.complete (r4, 40)\n",
                "    let _ = pump (sched, 8)\n",
                "    println \"task 4 finished: ${Task.is_finished h4:?}\"\n",
                "}\n",
            ),
        )
        .expect("temporary completion source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("completion executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("completion executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "completion program exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "task 1 parked\n",
                "task 1 got 10\n",
                // resolved before the task ran: the `await` returns in the same
                // resume, so the pump reports a single execution.
                "task 2 parked\n",
                "task 2 got 20\n",
                "pump 2: executed=1\n",
                // dropped resolver cancels the wait.
                "task 3 parked\n",
                "task 3 cancelled\n",
                // task 4 is cancelled while parked; the later resolve is a no-op.
                "task 4 parked\n",
                "task 4 finished: True\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn awaiting_a_completion_from_another_scheduler_traps() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-xsched-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-xsched-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "def observe: move Wait I32 -> Coroutine{} () = move w => coro { let _ = await w; () }\n",
                "def make: Scheduler -> (wait: Wait I32, resolver: Resolver I32) = s => completion s\n",
                "let a = scheduler ()\n",
                "let b = scheduler ()\n",
                "with Tasks = task_scope (a) {\n",
                "    let (w, r) = make b\n",
                "    let _ = spawn (observe w)\n",
                "    let _ = pump (a, 8)\n",
                "    let _ = r\n",
                "    ()\n",
                "}\n",
            ),
        )
        .expect("temporary cross-scheduler source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("cross-scheduler executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("cross-scheduler executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(
            !result.status.success(),
            "awaiting a completion from another scheduler should trap"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_completion_with_a_cancel_callback_handles_every_abandon_path() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-cancelcb-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-cancelcb-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def obs: (I32, move Wait I32) ->{IO} Coroutine{IO} () = (id, move w) => coro {\n",
                "    let r = await w\n",
                "    match r {\n",
                "        Completed v => println \"task ${id:?} Completed ${v:?}\",\n",
                "        Cancelled() => println \"task ${id:?} Cancelled\",\n",
                "    }\n",
                "}\n",
                "def noop: () -> () = () => ()\n",
                "def prim: (Scheduler, () -> ()) -> (wait: Wait I32, resolver: Resolver I32) =\n",
                "    (s, cb) => completion_with_cancel (s, cb)\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                // A: resolved — the callback is released without running.
                "    let (wa, ra) = prim (sched, noop)\n",
                "    let _ = spawn (obs (1, wa))\n",
                "    let _ = pump (sched, 8)\n",
                "    Resolver.complete (ra, 100)\n",
                "    let _ = pump (sched, 8)\n",
                // B: the `Wait` is dropped unconsumed — the callback runs, and a
                //    later `complete` reports the consumer is gone and drops 200.
                "    let (wb, rb) = prim (sched, noop)\n",
                "    let _ = wb\n",
                "    Resolver.complete (rb, 200)\n",
                "    println \"b: complete after abandon returned\"\n",
                // C: a task parked on the wait is cancelled — the callback runs
                //    at the unwind, and the later `complete` is a no-op.
                "    let (wc, rc) = prim (sched, noop)\n",
                "    let hc = spawn (obs (3, wc))\n",
                "    let _ = pump (sched, 8)\n",
                "    Task.cancel hc\n",
                "    let _ = pump (sched, 8)\n",
                "    Resolver.complete (rc, 300)\n",
                "    println \"c: task 3 finished ${Task.is_finished hc:?}\"\n",
                "}\n",
            ),
        )
        .expect("temporary cancel-callback source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("cancel-callback executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("cancel-callback executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "cancel-callback program exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "task 1 Completed 100\n",
                "b: complete after abandon returned\n",
                "c: task 3 finished True\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_completion_token_resolves_cancels_or_releases_a_unit_wait() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-token-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-token-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "def obs: (I32, move Wait ()) ->{IO} Coroutine{IO} () = (id, move w) => coro {\n",
                "    let r = await w\n",
                "    match r {\n",
                "        Completed uu => println \"task ${id:?} Completed\",\n",
                "        Cancelled() => println \"task ${id:?} Cancelled\",\n",
                "    }\n",
                "}\n",
                "def mk: Scheduler -> (wait: Wait (), token: CompletionToken) = s => completion_token s\n",
                "let sched = scheduler ()\n",
                "with Tasks = task_scope (sched) {\n",
                "    let (w1, t1) = mk sched\n",
                "    let _ = spawn (obs (1, w1))\n",
                "    let _ = pump (sched, 8)\n",
                "    CompletionToken.resolve t1\n",
                "    let _ = pump (sched, 8)\n",
                "    let (w2, t2) = mk sched\n",
                "    let _ = spawn (obs (2, w2))\n",
                "    let _ = pump (sched, 8)\n",
                "    CompletionToken.cancel t2\n",
                "    let _ = pump (sched, 8)\n",
                "    let (w3, t3) = mk sched\n",
                "    let _ = spawn (obs (3, w3))\n",
                "    let _ = pump (sched, 8)\n",
                "    let _ = t3\n",
                "    let _ = pump (sched, 8)\n",
                "}\n",
            ),
        )
        .expect("temporary token source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("token executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("token executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "token program exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "task 1 Completed\n",
                "task 2 Cancelled\n",
                // dropping the token unresolved releases it, which cancels.
                "task 3 Cancelled\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn until_resumes_when_its_predicate_first_holds() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-until-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-until-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "let signal n = 0\n",
                "let sched = scheduler ()\n",
                "with Reactive = reactive_scope () {\n",
                "  with Tasks = task_scope (sched) {\n",
                // (1) already-true: proceeds in the same pump, no suspension.
                "    n = 9\n",
                "    let _ = spawn (coro {\n",
                "      let _ = await (until { n >= 5 })\n",
                "      println \"a proceeded\"\n",
                "    })\n",
                "    let _ = pump (sched, 8)\n",
                // (2) parks, then resumes on the first committed `True`; a
                //     transient `True` inside a batch is not an event.
                "    n = 0\n",
                "    let _ = spawn (coro {\n",
                "      let _ = await (until { n >= 5 })\n",
                "      println \"b proceeded at n=${n:?}\"\n",
                "    })\n",
                "    let _ = pump (sched, 8)\n",
                "    n = 3\n",
                "    let _ = pump (sched, 8)\n",
                "    batch { n = 100\n n = 4 }\n",
                "    let _ = pump (sched, 8)\n",
                "    println \"b still parked\"\n",
                "    n = 6\n",
                "    let _ = pump (sched, 8)\n",
                "  }\n",
                "}\n",
            ),
        )
        .expect("temporary until source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("until executable should compile");
        let result = Command::new(&output).output().expect("until should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(result.status.success(), "until program exited with {}", result.status);
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "a proceeded\n",
                "b still parked\n",
                "b proceeded at n=6\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn cancelling_a_task_parked_in_until_tears_down_the_subscription() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-until-cancel-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-until-cancel-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.(IO, println)\n",
                "let signal n = 0\n",
                "let sched = scheduler ()\n",
                "with Reactive = reactive_scope () {\n",
                "  with Tasks = task_scope (sched) {\n",
                "    let h = spawn (coro {\n",
                "      let _ = await (until { n >= 100 })\n",
                "      println \"SHOULD NOT PRINT\"\n",
                "    })\n",
                "    let _ = pump (sched, 8)\n",
                "    Task.cancel h\n",
                "    let _ = pump (sched, 8)\n",
                "    println \"cancelled, finished=${Task.is_finished h:?}\"\n",
                // the subscription is gone: driving the signal past the
                // threshold must not wake anything or crash.
                "    n = 500\n",
                "    let r = pump (sched, 8)\n",
                "    println \"post-cancel pump executed=${r.executed:?}\"\n",
                "  }\n",
                "}\n",
            ),
        )
        .expect("temporary until-cancel source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("until-cancel executable should compile");
        let result = Command::new(&output).output().expect("until-cancel should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(
            result.status.success(),
            "until-cancel program exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "cancelled, finished=True\n",
                "post-cancel pump executed=0\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_coroutines_example_runs_with_expected_output() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-example-coroutines-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-example-coroutines-{nonce}"));
        std::fs::write(
            &source,
            include_str!("../../staple-compiler/examples/coroutines.sta"),
        )
        .expect("temporary example source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("coroutines example should compile");
        let result = Command::new(&output)
            .output()
            .expect("coroutines example should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "coroutines example exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                "-- pump 1 --\n",
                "worker 4: start\n",
                "worker 9: start\n",
                "pump 1: executed=4 ready=2\n",
                "-- pump 2 --\n",
                "worker 4: done\n",
                "greeter: host sent 200\n",
                "pump 2: executed=4 ready=1\n",
                "-- pump 3 --\n",
                "consumer: worker produced 40\n",
                "pump 3: executed=1 ready=0\n",
                "doomed finished: True\n",
                "scope closed\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_game_loop_adapter_demo_runs_every_scenario() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("staple-example-game-loop-{nonce}"));
        std::fs::create_dir_all(&dir).expect("temporary example dir should be creatable");
        std::fs::write(
            dir.join("game.sta"),
            include_str!("../../staple-compiler/examples/game_loop/game.sta"),
        )
        .expect("game.sta should be writable");
        std::fs::write(
            dir.join("main.sta"),
            include_str!("../../staple-compiler/examples/game_loop/main.sta"),
        )
        .expect("main.sta should be writable");
        let output = dir.join("demo");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            dir.join("main.sta").into_os_string(),
        ])
        .expect("game-loop demo should compile");
        let result = Command::new(&output)
            .output()
            .expect("game-loop demo should run");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            result.status.success(),
            "game-loop demo exited with {}",
            result.status
        );
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            concat!(
                // multiple fixed ticks per update: three ticks, three resumes.
                "fixed tick 1\n",
                "fixed tick 2\n",
                "fixed tick 3\n",
                // entity destruction: the mob's sleeper is cancelled.
                "entity destroyed\n",
                // unscaled progress: the wall sleeper wakes at its deadline.
                "wall sleeper woke: wall_ms=300\n",
                // paused scaled time: no "scaled sleeper woke" line — game_ms
                // froze at 100 once the scale went to 0.
                "final: game_ms=100 wall_ms=400 frame=4 fixed=3\n",
            ),
        );
    }

    #[test]
    #[cfg(unix)]
    fn deep_coroutine_nesting_does_not_grow_the_native_stack() {
        // 20 000 nested `await`s; the trampoline driver keeps the native stack
        // flat, so this completes rather than overflowing.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-coro-deep-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-coro-deep-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "use std.coroutine.*\n",
                "use std.io.println\n",
                "def sum_to: I32 -> Coroutine{} I32 = n => coro {\n",
                "    match n <= 0 {\n",
                "        True() => 0,\n",
                "        False() => {\n",
                "            let rest = await (sum_to (n - 1))\n",
                "            rest + n\n",
                "        },\n",
                "    }\n",
                "}\n",
                "println \"${block_on (sum_to 20000):?}\"\n",
            ),
        )
        .expect("temporary coroutine source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("coroutine executable should compile");
        let result = Command::new(&output)
            .output()
            .expect("coroutine executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);

        assert!(
            result.status.success(),
            "deep coroutine program exited with {}",
            result.status
        );
        // 20000 * 20001 / 2
        assert_eq!(String::from_utf8_lossy(&result.stdout), "200010000\n");
    }

    #[test]
    #[cfg(unix)]
    fn runs_typed_resources_with_lexical_shadowing() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-resources-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-resources-{nonce}"));
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
                "type Config = (x: I32)\n",
                "def read_config = () => (resource Config).x\n",
                "def update_config: () ->{mut Config} () = () => { (resource Config).x = 9 }\n",
                "let mut config = Config (x: 1)\n",
                "let copied = with Config = config { config.x = 2; read_config () }\n",
                "with mut Config = config { update_config () }\n",
                "exit ((derived - 42) + (later - 7) + (copied - 1) + (config.x - 9))\n",
            ),
        )
        .expect("temporary resource source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-ref-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-ref-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-buffer-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-buffer-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-transfer-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-transfer-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-list-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-list-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-list-index-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-list-index-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-list-of-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-list-of-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-erased-product-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-erased-product-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-mutable-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-mutable-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-thunk-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-thunk-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-reactive-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-reactive-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-batch-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-batch-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-self-reaction-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-self-reaction-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-queued-reaction-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-queued-reaction-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-nonconvergent-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-nonconvergent-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "let signal count = 0\n",
                "reaction { let current = count; count = current + 1; () }\n",
            ),
        )
        .expect("temporary nonconvergent source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-main-reactive-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-main-reactive-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-derived-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-derived-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
    fn runs_local_trait_implementations_for_products() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-default-product-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-default-product-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "trait ProductDefault T { product_default: () -> T }\n",
                "impl ProductDefault (I32, I32, I32) { def product_default = () => (2, 3, 5) }\n",
                "let values: (I32, I32, I32) = product_default ()\n",
                "exit (values.0 + values.1 + values.2 - 10)\n",
            ),
        )
        .expect("temporary default-product source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("local product-trait executable should compile");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-product-spread-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-product-spread-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-named-product-spread-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-named-product-spread-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-designated-product-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-designated-product-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
    fn runs_anonymous_product_field_defaults() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-product-defaults-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-product-defaults-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def text: (String, x: I32 = 0, y: I32 = 0) -> I32 = (value, x, y) => x + y\n",
                "let point: (x: I32 = 1, y: I32 = 2) = ()\n",
                "let result = text \"Hello\" + text (\"Hello\", .y: 10) + point.x + point.y\n",
                "exit (result - 13)\n",
            ),
        )
        .expect("temporary product-default source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("product-default executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("product-default executable should run");
        let _ = std::fs::remove_file(source);
        let _ = std::fs::remove_file(output);
        assert!(status.success());
    }

    #[test]
    #[cfg(unix)]
    fn runs_repeated_product_values() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("staple-compiler-repeated-product-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-repeated-product-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "const n = 2\n",
                "let cells: I32[4] = (3; n + 2)\n",
                "let total = cells.0 + cells.1 + cells.2 + cells.3\n",
                "exit (total - 12)\n",
            ),
        )
        .expect("temporary repeated-product source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
        run([
            "--stdlib".into(),
            standard_library.into_os_string(),
            "--emit".into(),
            "exe".into(),
            "-o".into(),
            output.clone().into_os_string(),
            source.clone().into_os_string(),
        ])
        .expect("repeated-product executable should compile");
        let status = Command::new(&output)
            .status()
            .expect("repeated-product executable should run");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-drop-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-drop-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-string-literals-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-string-literals-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-loop-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-loop-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-for-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-for-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-float-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-float-{nonce}"));
        std::fs::write(
            &source,
            concat!(
                "extern \"c\" { exit: I32 -> () }\n",
                "def score_bool: Bool -> I32 = value => match value { True() => 1, False() => 0, }\n",
                "let nan = 0.0 / 0.0\n",
                "let partial_score = match (PartialOrd.partial_cmp (nan, 1.0)) { None() => 0, Some _ => 1, }\n",
                "let less_score = match (Ord.cmp (1, 2)) { Ordering.Less() => 0, _ => 1, }\n",
                "let equal_score = match (Ord.cmp (2, 2)) { Ordering.Equal() => 0, _ => 1, }\n",
                "let greater_score = match (Ord.cmp (3, 2)) { Ordering.Greater() => 0, _ => 1, }\n",
                "let single: F32 = (1.5 satisfies F32) + .5\n",
                "exit (partial_score + less_score + equal_score + greater_score + score_bool (nan < 1.0) + score_bool (nan == nan) + (score_bool (nan != nan) - 1) + (score_bool (single == 2.0) - 1))\n",
            ),
        )
        .expect("temporary float source should be writable");
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-to-string-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-to-string-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-string-template-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-string-template-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
        let source = std::env::temp_dir().join(format!("staple-compiler-string-add-{nonce}.sta"));
        let output = std::env::temp_dir().join(format!("staple-compiler-string-add-{nonce}"));
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
        let standard_library = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("stdlib");
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
