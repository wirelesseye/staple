//! Separate compilation: the standard library, the runtime, and a program's
//! root package are emitted as independent objects that must link into the
//! same executable the single-module build produces.

// Test binaries compile the standard library many times over; mimalloc keeps
// the compiler's small allocations from dominating the suite.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use inkwell::{context::Context, memory_buffer::MemoryBuffer, module::Linkage, values::BasicValue};
use staple_compiler::{
    CodeGenerator, EmissionUnit, LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker,
    codegen_corpus,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// Lowers a program in full, or (`prebuilt`) as an incremental build does:
/// only what the root unit defines, with every other package linked from its
/// own object.
fn lower(program: staple_compiler::Program, prebuilt: bool) -> LoweredModule {
    let resolved = NameResolver::new().resolve_program(program).unwrap();
    let typed = TypeChecker::new().check(resolved).unwrap();
    Lowerer::new()
        .with_prebuilt_dependencies(prebuilt)
        .lower(&typed)
        .unwrap()
}

/// The standard library on its own: a library package graph with no entry.
fn lower_std() -> LoweredModule {
    let root = repo_root();
    let graph = staple_project::load_package_graph(&root.join("stdlib/staple.kdl")).unwrap();
    lower(
        ProgramLoader::new()
            .with_standard_library_root(root.join("stdlib"))
            .with_package_graph(graph)
            .load_package_graph()
            .unwrap(),
        false,
    )
}

fn lower_file(path: &Path, prebuilt: bool) -> LoweredModule {
    lower(
        ProgramLoader::new()
            .with_standard_library_root(repo_root().join("stdlib"))
            .load_path(path)
            .unwrap(),
        prebuilt,
    )
}

fn lower_source(source: &str, prebuilt: bool) -> LoweredModule {
    lower(
        ProgramLoader::new()
            .with_standard_library_root(repo_root().join("stdlib"))
            .load_source(source, repo_root())
            .unwrap(),
        prebuilt,
    )
}

fn std_unit(std: &LoweredModule) -> EmissionUnit {
    let package = std
        .packages()
        .iter()
        .find(|package| package.name == "std")
        .expect("the standard library is a package");
    EmissionUnit::packages([package.id])
}

fn scratch(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("separate-compilation");
    std::fs::create_dir_all(&directory).unwrap();
    directory.join(name)
}

/// The standard library unit's IR and the runtime object, built once.
struct SharedUnits {
    std_ir: String,
    std_object: PathBuf,
    runtime_ir: String,
    runtime_object: PathBuf,
}

fn shared() -> &'static SharedUnits {
    static SHARED: OnceLock<SharedUnits> = OnceLock::new();
    SHARED.get_or_init(|| {
        let std = lower_std();
        let unit = std_unit(&std);
        let context = Context::create();
        let generator = CodeGenerator::new(&context);
        let std_object = scratch("std.o");
        generator
            .emit_unit_object(&std, &unit, &std_object, None)
            .unwrap();
        let runtime_object = scratch("runtime.o");
        generator
            .emit_runtime_object(&runtime_object, None)
            .unwrap();
        SharedUnits {
            std_ir: generator
                .compile_unit_for_target(&std, &unit, None)
                .unwrap(),
            std_object,
            runtime_ir: generator.compile_runtime_for_target(None).unwrap(),
            runtime_object,
        }
    })
}

/// A type's printed form with LLVM's uniquing suffixes removed: loading several
/// modules into one context renames a repeated named struct `%Name` to
/// `%Name.N`, which does not change the type.
fn structural(ty: String) -> String {
    let mut result = String::with_capacity(ty.len());
    let mut rest = ty.as_str();
    while let Some(start) = rest.find('%') {
        let name_end = rest[start + 1..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(rest.len(), |offset| start + 1 + offset);
        result.push_str(&rest[..name_end]);
        rest = &rest[name_end..];
        if let Some(suffix) = rest.strip_prefix('.') {
            let digits = suffix.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 {
                rest = &suffix[digits..];
            }
        }
    }
    result.push_str(rest);
    result
}

/// A function or global as a module sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Symbol {
    ty: String,
    defined: bool,
    external: bool,
    used: bool,
}

fn symbols(ir: &str) -> HashMap<String, Symbol> {
    let context = Context::create();
    let buffer = MemoryBuffer::create_from_memory_range_copy(ir.as_bytes(), "unit");
    let module = context.create_module_from_ir(buffer).unwrap();
    let mut symbols = HashMap::new();
    for function in module.get_functions() {
        let name = function.get_name().to_str().unwrap().to_owned();
        symbols.insert(
            name,
            Symbol {
                ty: structural(function.get_type().print_to_string().to_string()),
                defined: function.count_basic_blocks() > 0,
                external: matches!(
                    function.get_linkage(),
                    Linkage::External | Linkage::LinkOnceODR | Linkage::ExternalWeak
                ),
                used: function
                    .as_global_value()
                    .as_pointer_value()
                    .get_first_use()
                    .is_some(),
            },
        );
    }
    for global in module.get_globals() {
        let name = global.get_name().to_str().unwrap().to_owned();
        symbols.insert(
            name,
            Symbol {
                ty: structural(global.get_value_type().print_to_string().to_string()),
                defined: global.get_initializer().is_some(),
                external: matches!(
                    global.get_linkage(),
                    Linkage::External | Linkage::LinkOnceODR | Linkage::ExternalWeak
                ),
                used: global.as_pointer_value().get_first_use().is_some(),
            },
        );
    }
    symbols
}

/// Every symbol the root unit uses from another object is defined, with the
/// same type, by the standard library object or the runtime object.
fn assert_root_symbols_resolve(label: &str, root: &LoweredModule) {
    let context = Context::create();
    let root_ir = CodeGenerator::new(&context)
        .compile_unit_for_target(root, &EmissionUnit::root(root), None)
        .unwrap();
    let shared = shared();
    let std = symbols(&shared.std_ir);
    let runtime = symbols(&shared.runtime_ir);
    let root = symbols(&root_ir);

    let mut from_std = 0;
    for (name, symbol) in &root {
        if symbol.defined || !symbol.used || name.starts_with("llvm.") {
            continue;
        }
        let provider =
            [("std", &std), ("runtime", &runtime)]
                .into_iter()
                .find_map(|(object, symbols)| {
                    symbols
                        .get(name)
                        .filter(|candidate| candidate.defined && candidate.external)
                        .map(|candidate| (object, candidate))
                });
        match provider {
            Some((object, candidate)) => {
                assert_eq!(
                    candidate.ty, symbol.ty,
                    "{label}: `{name}` is declared with a different type than {object} defines"
                );
                from_std += usize::from(object == "std");
            }
            // A C symbol (libc, or a program `extern`) resolves at link time.
            None => assert!(
                !name.starts_with("__staple"),
                "{label}: `{name}` is declared but neither the standard library nor the runtime defines it"
            ),
        }
    }
    assert!(from_std > 0, "{label}: expected references into std");

    // No external definition may exist twice, or the link fails.
    for (name, symbol) in &root {
        if !(symbol.defined && symbol.external) || name == "main" {
            continue;
        }
        for (object, symbols) in [("std", &std), ("runtime", &runtime)] {
            assert!(
                !symbols
                    .get(name)
                    .is_some_and(|other| other.defined && other.external),
                "{label}: `{name}` is defined by the root unit and by {object}"
            );
        }
    }
}

#[test]
fn the_standard_library_unit_exports_what_a_program_declares() {
    let examples = repo_root().join("staple-compiler/examples");
    let sources = [
        "hello_world.sta",
        "language_tour.sta",
        "traits_and_generics.sta",
        "signals_and_reactions.sta",
        "coroutines.sta",
        "list_and_collections.sta",
        "modules_and_imports.sta",
    ];
    for prebuilt in [false, true] {
        for source in sources {
            assert_root_symbols_resolve(source, &lower_file(&examples.join(source), prebuilt));
        }
        assert_root_symbols_resolve(
            "game_loop",
            &lower_file(&examples.join("game_loop/main.sta"), prebuilt),
        );
    }
}

#[test]
fn the_standard_library_unit_has_no_foreign_definitions() {
    let std = symbols(&shared().std_ir);
    assert!(std.get("main").is_none(), "a library unit has no entry");
    for name in std.keys().filter(|name| name.starts_with("__staple_gc_")) {
        assert!(!std[name].defined, "`{name}` belongs to the runtime object");
    }
    // Ordinal-numbered artifacts and instances never cross an object boundary.
    for (name, symbol) in &std {
        if symbol.defined && symbol.external {
            assert!(
                !name.starts_with("__staple_instance_") && !name.contains("_structural_"),
                "`{name}` is exported with a program-numbered name"
            );
        }
    }
}

fn link(objects: &[&Path], output: &Path) {
    let result = Command::new("cc")
        .args(objects)
        .arg("-o")
        .arg(output)
        .output()
        .expect("a C compiler is available");
    assert!(
        result.status.success(),
        "link failed:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    stdout: Vec<u8>,
    status: Option<i32>,
}

fn run(executable: &Path) -> Outcome {
    let output = Command::new(executable).output().unwrap();
    Outcome {
        stdout: output.stdout,
        status: output.status.code(),
    }
}

/// Builds `lowered` as one module, and `root` (the same program lowered with
/// prebuilt dependencies) as std + runtime + root objects, and requires the
/// two executables to behave the same.
fn assert_same_behavior(name: &str, lowered: &LoweredModule, root: &LoweredModule) {
    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    let shared = shared();

    let whole_object = scratch(&format!("{name}.whole.o"));
    generator.emit_object(lowered, &whole_object, None).unwrap();
    let whole = scratch(&format!("{name}.whole"));
    link(&[&whole_object], &whole);

    let root_object = scratch(&format!("{name}.root.o"));
    generator
        .emit_unit_object(root, &EmissionUnit::root(root), &root_object, None)
        .unwrap();
    let separate = scratch(&format!("{name}.separate"));
    link(
        &[&shared.std_object, &shared.runtime_object, &root_object],
        &separate,
    );

    assert_eq!(
        run(&separate),
        run(&whole),
        "{name}: separately compiled output differs"
    );
}

#[test]
fn separately_compiled_examples_match_the_single_module_build() {
    let examples = repo_root().join("staple-compiler/examples");
    let mut sources = std::fs::read_dir(&examples)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "sta"))
        .collect::<Vec<_>>();
    sources.sort();
    sources.push(examples.join("game_loop/main.sta"));
    assert!(sources.len() > 8);
    for source in sources {
        let name = if source.ends_with("game_loop/main.sta") {
            "game_loop".to_owned()
        } else {
            source.file_stem().unwrap().to_string_lossy().into_owned()
        };
        assert_same_behavior(
            &name,
            &lower_file(&source, false),
            &lower_file(&source, true),
        );
    }
}

/// Every runnable corpus program, which is slow: run with `--ignored`.
#[test]
#[ignore = "builds every corpus program twice"]
fn separately_compiled_corpus_matches_the_single_module_build() {
    use staple_compiler::{CorpusExpectation, CorpusSource};
    let mut failures = Vec::new();
    for program in codegen_corpus() {
        if program.expectation != CorpusExpectation::MustRun {
            continue;
        }
        let outcome = std::panic::catch_unwind(|| {
            let lower = |prebuilt| match program.source {
                CorpusSource::Inline(source) => lower_source(source, prebuilt),
                CorpusSource::File(path) => lower_file(&repo_root().join(path), prebuilt),
            };
            assert_same_behavior(program.name, &lower(false), &lower(true));
        });
        if outcome.is_err() {
            failures.push(program.name);
        }
    }
    assert!(failures.is_empty(), "differing programs: {failures:?}");
}

fn lower_manifest(manifest: &Path, prebuilt: bool) -> LoweredModule {
    let graph = staple_project::load_package_graph(manifest).unwrap();
    lower(
        ProgramLoader::new()
            .with_standard_library_root(repo_root().join("stdlib"))
            .with_package_graph(graph)
            .load_package_graph()
            .unwrap(),
        prebuilt,
    )
}

/// A dependency package is built from a program that has never seen its
/// consumer, and must still agree with what the consumer declares.
#[test]
fn a_dependency_package_object_links_with_its_consumer() {
    let directory = scratch("packages");
    let _ = std::fs::remove_dir_all(&directory);
    let library = directory.join("shapes");
    let application = directory.join("app");
    std::fs::create_dir_all(library.join("src")).unwrap();
    std::fs::create_dir_all(application.join("src")).unwrap();
    let write = |path: PathBuf, text: &str| std::fs::write(path, text).unwrap();
    write(
        library.join("staple.kdl"),
        "package \"shapes\" {\n    kind \"library\"\n}\n",
    );
    write(
        library.join("src/geometry.sta"),
        concat!(
            "pub mod\n",
            "pub type Point wrap pub (x: I32, y: I32)\n",
            "pub trait Label T {label: T -> String}\n",
            "impl Label Point {label = _ => \"point\"}\n",
            "pub let scale: I32 = 3\n",
            "pub let mut calls: I32 = 0\n",
            "pub def scaled: Point -> Point = Point (x, y) => Point (x: x * scale, y: y * scale)\n",
            "pub def get_x: Point -> I32 = Point point => point.0\n",
            "pub def get_y: Point -> I32 = Point point => point.1\n",
            "pub def describe = <T where Label T> value: T => String :: Label.label value\n",
            "pub def counted = () => { calls = calls + 1; calls }\n",
        ),
    );
    write(
        application.join("staple.kdl"),
        "package \"app\" {\n    dependencies {\n        shapes path=\"../shapes\"\n    }\n}\n",
    );
    write(
        application.join("src/main.sta"),
        concat!(
            "use shapes.geometry.*\n",
            "use std.io.println\n",
            "type Local wrap I32\n",
            "impl Label Local {label = _ => \"local\"}\n",
            "let p = scaled (Point (x: 1, y: 2))\n",
            "println (describe p)\n",
            "println (describe (Local 7))\n",
            "println \"${get_x p} ${get_y p}\"\n",
            "let first = counted ()\n",
            "let second = counted ()\n",
            "println \"${first} ${second}\"\n",
        ),
    );

    let context = Context::create();
    let generator = CodeGenerator::new(&context);
    let shared = shared();

    // The dependency, compiled without its consumer.
    let library_program = lower_manifest(&library.join("staple.kdl"), false);
    let package = library_program
        .packages()
        .iter()
        .find(|package| package.name == "shapes")
        .unwrap();
    let library_object = directory.join("shapes.o");
    generator
        .emit_unit_object(
            &library_program,
            &EmissionUnit::packages([package.id]),
            &library_object,
            None,
        )
        .unwrap();

    let program = lower_manifest(&application.join("staple.kdl"), false);
    let root_program = lower_manifest(&application.join("staple.kdl"), true);
    let whole_object = directory.join("whole.o");
    generator
        .emit_object(&program, &whole_object, None)
        .unwrap();
    let whole = directory.join("whole");
    link(&[&whole_object], &whole);

    let root_object = directory.join("root.o");
    generator
        .emit_unit_object(
            &root_program,
            &EmissionUnit::root(&root_program),
            &root_object,
            None,
        )
        .unwrap();
    let separate = directory.join("separate");
    link(
        &[
            &shared.std_object,
            &shared.runtime_object,
            &library_object,
            &root_object,
        ],
        &separate,
    );
    let expected = run(&whole);
    assert_eq!(expected.status, Some(0));
    assert!(String::from_utf8_lossy(&expected.stdout).contains("point\nlocal\n3 6\n1 2\n"));
    assert_eq!(run(&separate), expected);
}

/// A `@track_caller` function's constant site is emitted by the unit that
/// references it, while `Location.caller` itself lives in the prebuilt
/// standard library object.
#[test]
fn track_caller_locations_survive_separate_compilation() {
    let source = concat!(
        "use std.io.println\n",
        "use std.fmt.Display\n",
        "use std.source.(Location, track_caller)\n",
        "@track_caller\n",
        "def forwards = () => Location :: Location.caller ()\n",
        "@track_caller\n",
        "def tagged: <T where Copy T, Display T> T -> String = <T where Copy T, Display T> value =>\n",
        "    \"${value}@${Location.caller ()}\"\n",
        "println \"${Location.caller ()}\"\n",
        "println \"${forwards ()}\"\n",
        "let held = forwards\n",
        "println \"${held ()}\"\n",
        "println (tagged 3)\n",
    );
    let expected = [
        "<stdin>.sta:9:12",
        "<stdin>.sta:10:12",
        "<stdin>.sta:11:12",
        "3@<stdin>.sta:13:10",
    ];
    let whole = lower_source(source, false);
    assert_same_behavior("track_caller", &whole, &lower_source(source, true));
    let outcome = run(&scratch("track_caller.whole"));
    assert_eq!(outcome.status, Some(0));
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    assert_eq!(stdout.lines().collect::<Vec<_>>(), expected);
}
