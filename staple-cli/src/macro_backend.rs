use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use staple_compiler::macro_runtime::client::{LibraryHandle, MacroClient};
use staple_compiler::macro_runtime::intrinsics;
use staple_compiler::macro_runtime::canonical::canonical_items;
use staple_compiler::macro_runtime::manifest::{LibraryManifest, build_manifest, stable_entry_key};
use staple_compiler::macro_runtime::quote::CallContext;
use staple_compiler::macro_runtime::instance_symbol;
use staple_compiler::{
    ClosureLibraryRequest, CodeGenerator, CompiledCall, EmissionUnit, IrOptimization, LibrarySpec, Lowerer,
    MacroBackend, MacroEffectEntry, NameResolver, PackageLibraryBuild, Program, TypeChecker,
    prepare_package_library,
};
use staple_project::{ActiveFeatures, FeatureSelection, PackageGraph, PackageId};
use staple_syntax::{Diagnostic, Span, Syntax, SyntaxId};

use crate::cache::{self, Destination, Placement};
use crate::compile;

/// The IR pipeline for package libraries and their macro-mode objects, which
/// are built once per fingerprint. Task 11 measured no call-time difference
/// between `O0`, `O1` and `O2`, since a call's time goes to the wire and the
/// host's syntax intrinsics; `O1` makes the standard library's library 25%
/// smaller for about 1.5 s more on a cold build, and `O2` adds nothing to it.
const PACKAGE_OPTIMIZATION: IrOptimization = IrOptimization::O1;
/// The IR pipeline for closure libraries, which are rebuilt after an edit to
/// a macro's closure. `O1` made their codegen 25–35% slower and calls no
/// faster.
const CLOSURE_OPTIMIZATION: IrOptimization = IrOptimization::O0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MacroEngine {
    #[default]
    Interp,
    Compiled,
}

pub fn parse_macro_engine(value: &str) -> Result<MacroEngine, String> {
    match value {
        "interp" => Ok(MacroEngine::Interp),
        "compiled" => Ok(MacroEngine::Compiled),
        _ => Err(format!(
            "unknown macro engine `{value}`; expected `interp` or `compiled`"
        )),
    }
}

/// What every backend of one compiler invocation built, shared with the
/// backends that expand a dependency package.
#[derive(Default)]
struct MacroStats {
    closure_libraries: Vec<Vec<String>>,
    /// How long each step of each closure library build took, in the order
    /// of `closure_libraries`.
    closure_steps: Vec<Vec<(&'static str, Duration)>>,
    package_libraries: Vec<String>,
    closure_time: Duration,
    package_time: Duration,
    /// Calls per macro: how many, and their total time, including the wire
    /// encoding both ways.
    calls: BTreeMap<String, (u32, Duration)>,
}

/// The macro-mode objects and fingerprints of the packages prepared so far,
/// by package name, so a library can link its dependencies' objects.
#[derive(Default)]
struct PreparedPackages {
    objects: HashMap<String, PathBuf>,
    libraries: HashMap<String, PathBuf>,
    fingerprints: HashMap<String, cache::Fingerprint>,
}

type SharedStats = Arc<Mutex<MacroStats>>;
type SharedPackages = Arc<Mutex<PreparedPackages>>;

/// Builds macro libraries and runs macro calls in a macro server.
///
/// Macro libraries run in the compiler's own process tree, so they are always
/// built for the host, with the host's C compiler, whatever `--target`,
/// `--linker`, `-L` and `-l` say about the program being built.
pub struct CliMacroBackend {
    client: MacroClient,
    entries: HashMap<SyntaxId, (LibraryHandle, String)>,
    temp_dir: PathBuf,
    next_library_id: usize,
    stats: SharedStats,
    report_stats: bool,
    packages: SharedPackages,
    /// Package names whose package libraries this backend loaded or built.
    prepared_packages: HashSet<String>,
    /// Whether the pre-expansion phase ran, so closure libraries can rely on
    /// their dependencies' macro-mode objects.
    packages_prepared: bool,
    /// Whether this backend expands a dependency for its package library,
    /// which keeps its closure libraries out of the dependency's directory.
    dependency_build: bool,
}

impl CliMacroBackend {
    pub fn new() -> Result<Self, String> {
        Self::with_shared(Arc::default(), Arc::default(), true)
    }

    fn with_shared(
        stats: SharedStats,
        packages: SharedPackages,
        report_stats: bool,
    ) -> Result<Self, String> {
        let client = MacroClient::new()?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "staple-macro-libs-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).map_err(|error| {
            format!(
                "could not create temporary macro library directory `{}`: {error}",
                temp_dir.display()
            )
        })?;
        Ok(Self {
            client,
            entries: HashMap::new(),
            temp_dir,
            next_library_id: 0,
            stats,
            report_stats,
            packages,
            prepared_packages: HashSet::new(),
            packages_prepared: false,
            dependency_build: !report_stats,
        })
    }

    /// A backend for expanding one dependency package, sharing the invocation's
    /// stats and prepared objects with the backend that started it.
    fn for_dependency(stats: SharedStats, packages: SharedPackages) -> Result<Self, String> {
        Self::with_shared(stats, packages, false)
    }

    /// Loads the package library at `library_path` against `program` and
    /// registers its macros.
    fn load_manifest(
        &mut self,
        program: &Program,
        library_path: &Path,
        manifest: &LibraryManifest,
    ) -> Result<(), Vec<Diagnostic>> {
        let loaded = manifest
            .resolve(program, library_path)
            .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        let handle = self
            .client
            .load(library_path, loaded.client_templates());
        for (declaration, shim) in loaded.entries {
            self.entries.insert(declaration, (handle, shim));
        }
        Ok(())
    }

    /// The macro-mode object and package library of `package`, building and
    /// caching them if no read directory has both.
    fn prepare_package(
        &mut self,
        program: &Program,
        package: PackageId,
        write_own_directory: bool,
    ) -> Result<(), Vec<Diagnostic>> {
        let Some(graph) = program.package_graph() else {
            return Ok(());
        };
        let info = graph.package(package);
        let name = info.name.clone();
        if self.prepared_packages.contains(&name) {
            return Ok(());
        }
        let fingerprint = self.package_fingerprint(program, package)?;
        let triple = cache::target_triple(None);
        let placement = macro_placement(program, package, &triple, write_own_directory);
        let extension = compile::shared_library_extension();
        let mut bad_cache = false;
        if let Some((library_path, object_path)) =
            find_cached_library(&placement.read, &name, fingerprint, extension)
        {
            // `load_manifest` fails before it loads anything, so a manifest
            // that doesn't read or doesn't resolve leaves nothing behind.
            let loaded = LibraryManifest::read_beside(&library_path).is_ok_and(|manifest| {
                self.load_manifest(program, &library_path, &manifest).is_ok()
            });
            if loaded {
                let mut registry = self.packages.lock().expect("macro package registry");
                registry.objects.insert(name.clone(), object_path);
                registry.libraries.insert(name.clone(), library_path);
                drop(registry);
                self.prepared_packages.insert(name);
                return Ok(());
            }
            // A library whose manifest doesn't read or resolve is not
            // trusted; a writable copy is replaced by the build below.
            bad_cache = true;
            let _ = std::fs::remove_file(&library_path);
            let _ = std::fs::remove_file(library_path.with_extension("manifest"));
            let _ = std::fs::remove_file(&object_path);
        }

        let started = Instant::now();
        let build = self.build_package(program, graph, package).map_err(|mut diagnostics| {
            for diagnostic in &mut diagnostics {
                diagnostic.message = format!(
                    "while preparing package `{}`: {}",
                    name, diagnostic.message
                );
            }
            diagnostics
        })?;
        let build_placement = if bad_cache {
            Placement {
                read: Vec::new(),
                write: placement.write,
            }
        } else {
            placement
        };
        self.emit_package_library(
            program,
            package,
            &build,
            &build_placement,
            fingerprint,
            extension,
        )?;
        {
            let mut stats = self.stats.lock().expect("macro stats");
            stats.package_libraries.push(name.clone());
            stats.package_time += started.elapsed();
        }
        self.prepared_packages.insert(name);
        Ok(())
    }

    fn build_package(
        &mut self,
        program: &Program,
        graph: &PackageGraph,
        package: PackageId,
    ) -> Result<PackageLibraryBuild, Vec<Diagnostic>> {
        let info = graph.package(package);
        let features = program
            .active_features()
            .get(&package)
            .map(|features| features.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut loader = staple_compiler::ProgramLoader::new()
            .with_package_graph(
                staple_project::load_package_graph(&info.manifest)
                    .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?,
            )
            .with_feature_selection(FeatureSelection {
                features,
                all_features: false,
                no_default_features: true,
            });
        if let Some(root) = standard_library_root(program) {
            loader = loader.with_standard_library_root(root);
        }
        let dependency_program = loader
            .load_package_graph()
            .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        let dependency_package = dependency_program
            .package_of(dependency_program.entry())
            .ok_or_else(|| {
                vec![Diagnostic::new(
                    Span::Compiler,
                    format!(
                        "internal error: package `{}` has no root module",
                        info.name
                    ),
                )]
            })?;
        let nested = Self::for_dependency(self.stats.clone(), self.packages.clone()).map_err(
            |message| vec![Diagnostic::new(Span::Compiler, message)],
        )?;
        staple_compiler::with_macro_backend(Box::new(nested), true, || {
            prepare_package_library(dependency_program, dependency_package)
        })
    }

    fn emit_package_library(
        &mut self,
        program: &Program,
        package: PackageId,
        build: &PackageLibraryBuild,
        placement: &Placement,
        fingerprint: cache::Fingerprint,
        extension: &str,
    ) -> Result<(), Vec<Diagnostic>> {
        let info = program
            .package_graph()
            .map(|graph| graph.package(package))
            .ok_or_else(|| {
                vec![Diagnostic::new(
                    Span::Compiler,
                    "internal error: a package library needs a package graph",
                )]
            })?;
        let name = info.name.clone();
        // The build lowered its own package graph, whose ids need not match
        // the outer program's.
        let built_package = build
            .expanded
            .package_of(build.expanded.entry())
            .unwrap_or(package);

        let effects: Vec<MacroEffectEntry<'_>> = build
            .macros
            .iter()
            .map(|macros| MacroEffectEntry {
                macro_name: &macros.name,
                entry_symbol: &macros.entry_symbol,
                span: macros.span.clone(),
            })
            .collect();
        let effect_diagnostics = build.lowered.check_macro_effects(&effects);
        if !effect_diagnostics.is_empty() {
            return Err(effect_diagnostics);
        }

        let context = inkwell::context::Context::create();
        let generator = CodeGenerator::new(&context);
        let object_name = format!("{name}-macro");
        let object_path = cache::ensure_object(
            placement,
            &object_name,
            fingerprint,
            &mut |path| {
                generator
                    .emit_unit_object(
                        &build.lowered,
                        &EmissionUnit::packages([built_package])
                            .with_optimization(PACKAGE_OPTIMIZATION),
                        path,
                        None,
                    )
                    .map_err(compile::format_diagnostics)
            },
        )
        .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        self.packages
            .lock()
            .expect("macro package registry")
            .objects
            .insert(name.clone(), object_path.clone());

        let dependency_objects = self.dependency_objects(program, package)?;
        let runtime = self.runtime_object(&generator, placement)?;
        let manifest_entries = build
            .macros
            .iter()
            .enumerate()
            .map(|(index, macros)| {
                (
                    macros.module,
                    macros.declaration,
                    format!("__staple_entry.{index}"),
                )
            })
            .collect::<Vec<_>>();
        let manifest = build_manifest(&build.expanded, &manifest_entries, &build.templates)
            .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        let spec = LibrarySpec {
            entries: build
                .macros
                .iter()
                .map(|macros| macros.entry_symbol.clone())
                .collect(),
            host_symbols: intrinsics::host_symbols(),
            trap_exit: true,
        };
        let shim_path = self.temp_dir.join(format!(
            "package-{}-{}.o",
            self.next_library_id,
            name
        ));
        self.next_library_id += 1;
        generator
            .emit_unit_object(
                &build.lowered,
                &EmissionUnit::packages([]).with_library(spec),
                &shim_path,
                None,
            )?;

        let mut objects = vec![object_path];
        objects.extend(dependency_objects);
        objects.push(shim_path);
        objects.push(runtime);
        let library_path = cache::ensure_library(
            placement,
            &name,
            fingerprint,
            extension,
            &mut |path| {
                compile::link_host_shared_library(&objects, path)?;
                manifest.write_beside(path)
            },
        )
        .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        let manifest = LibraryManifest::read_beside(&library_path)
            .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        self.packages
            .lock()
            .expect("macro package registry")
            .libraries
            .insert(name.clone(), library_path.clone());
        self.load_manifest(program, &library_path, &manifest)?;
        Ok(())
    }

    /// The macro-mode objects of every package `package` depends on.
    fn dependency_objects(
        &self,
        program: &Program,
        package: PackageId,
    ) -> Result<Vec<PathBuf>, Vec<Diagnostic>> {
        let Some(graph) = program.package_graph() else {
            return Ok(Vec::new());
        };
        let registry = self.packages.lock().expect("macro package registry");
        let mut objects = Vec::new();
        for dependency in dependency_order(graph, Some(package)) {
            if dependency == package {
                continue;
            }
            let name = &graph.package(dependency).name;
            let object = registry.objects.get(name).cloned().ok_or_else(|| {
                vec![Diagnostic::new(
                    Span::Compiler,
                    format!(
                        "internal error: package `{}` needs `{name}`'s macro-mode object, which was not built",
                        graph.package(package).name
                    ),
                )]
            })?;
            objects.push(object);
        }
        Ok(objects)
    }

    fn package_fingerprint(
        &self,
        program: &Program,
        package: PackageId,
    ) -> Result<cache::Fingerprint, Vec<Diagnostic>> {
        let Some(graph) = program.package_graph() else {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "internal error: a macro-mode object needs a package graph",
            )]);
        };
        let name = graph.package(package).name.clone();
        if let Some(fingerprint) = self
            .packages
            .lock()
            .expect("macro package registry")
            .fingerprints
            .get(&name)
            .copied()
        {
            return Ok(fingerprint);
        }
        let fingerprints = package_macro_fingerprints(graph, program.active_features())
            .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        let mut registry = self.packages.lock().expect("macro package registry");
        for (id, fingerprint) in &fingerprints {
            registry
                .fingerprints
                .insert(graph.package(*id).name.clone(), *fingerprint);
        }
        registry.fingerprints.get(&name).copied().ok_or_else(|| {
            vec![Diagnostic::new(
                Span::Compiler,
                format!("internal error: package `{name}` has no macro-mode fingerprint"),
            )]
        })
    }

    fn runtime_object(
        &self,
        generator: &CodeGenerator<'_>,
        placement: &Placement,
    ) -> Result<PathBuf, Vec<Diagnostic>> {
        let identity = cache::compiler_identity()
            .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        let triple = cache::target_triple(None);
        let fingerprint = cache::runtime_fingerprint(identity, &triple);
        cache::ensure_object(placement, "runtime", fingerprint, &mut |path| {
            generator
                .emit_runtime_object(path, None)
                .map_err(compile::format_diagnostics)
        })
        .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])
    }

    /// Copies the package's macro-mode object and library into its own macro
    /// directory, where a later build looks for a prebuilt library. A build
    /// that came from the user cache still installs its result.
    fn install_package_library(
        &mut self,
        program: &Program,
        package: PackageId,
    ) -> Result<(), Vec<Diagnostic>> {
        let Some(graph) = program.package_graph() else {
            return Ok(());
        };
        let name = graph.package(package).name.clone();
        let fingerprint = self.package_fingerprint(program, package)?;
        let triple = cache::target_triple(None);
        let own = package_macro_directory(program, graph, package, &triple);
        let extension = compile::shared_library_extension();
        let (library, object) = {
            let registry = self.packages.lock().expect("macro package registry");
            let library = registry.libraries.get(&name).cloned();
            let object = registry.objects.get(&name).cloned();
            (library, object)
        };
        let Some(library) = library else {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                format!("internal error: package `{name}` has no macro library to install"),
            )]);
        };
        let Some(object) = object else {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                format!("internal error: package `{name}` has no macro-mode object to install"),
            )]);
        };
        if library.parent() == Some(own.as_path()) && object.parent() == Some(own.as_path()) {
            return Ok(());
        }
        let placement = Placement {
            read: vec![own.clone()],
            write: vec![Destination {
                directory: own.clone(),
                prune: true,
            }],
        };
        let object_name = format!("{name}-macro");
        let installed_object = cache::ensure_object(
            &placement,
            &object_name,
            fingerprint,
            &mut |path| {
                std::fs::copy(&object, path)
                    .map(|_| ())
                    .map_err(|error| format!("could not copy `{}`: {error}", object.display()))
            },
        )
        .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        let installed_library = cache::ensure_library(
            &placement,
            &name,
            fingerprint,
            extension,
            &mut |path| {
                std::fs::copy(&library, path).map_err(|error| {
                    format!("could not copy `{}`: {error}", library.display())
                })?;
                std::fs::copy(
                    library.with_extension("manifest"),
                    path.with_extension("manifest"),
                )
                .map(|_| ())
                .map_err(|error| {
                    format!(
                        "could not copy `{}`: {error}",
                        library.with_extension("manifest").display()
                    )
                })
            },
        )
        .map_err(|message| vec![Diagnostic::new(Span::Compiler, message)])?;
        let mut registry = self.packages.lock().expect("macro package registry");
        registry.objects.insert(name.clone(), installed_object);
        registry.libraries.insert(name, installed_library);
        Ok(())
    }

    fn prepare_dependencies(&mut self, program: &Program) -> Result<(), Vec<Diagnostic>> {
        let Some(graph) = program.package_graph() else {
            return Ok(());
        };
        let root = program.package_of(program.entry());
        for package in dependency_order(graph, root) {
            if Some(package) == root {
                continue;
            }
            self.prepare_package(program, package, false)?;
        }
        self.packages_prepared = true;
        Ok(())
    }
}

impl Drop for CliMacroBackend {
    fn drop(&mut self) {
        self.client.shutdown();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
        if self.report_stats && std::env::var_os("STAPLE_MACRO_STATS").is_some() {
            let stats = self.stats.lock().expect("macro stats");
            eprintln!(
                "staple macro stats: {} closure libraries built in {:.3}s, {} package libraries in {:.3}s",
                stats.closure_libraries.len(),
                stats.closure_time.as_secs_f64(),
                stats.package_libraries.len(),
                stats.package_time.as_secs_f64()
            );
            for (index, names) in stats.closure_libraries.iter().enumerate() {
                eprintln!("  lib #{index}: {}", names.join(", "));
                if let Some(steps) = stats.closure_steps.get(index) {
                    let steps = steps
                        .iter()
                        .map(|(step, time)| format!("{step} {:.3}s", time.as_secs_f64()))
                        .collect::<Vec<_>>();
                    eprintln!("    steps: {}", steps.join(", "));
                }
            }
            for (index, name) in stats.package_libraries.iter().enumerate() {
                eprintln!("  package #{index}: {name}");
            }
            for (name, (count, time)) in &stats.calls {
                eprintln!(
                    "  calls: {name} {count} in {:.3}ms, {:.1}us each",
                    time.as_secs_f64() * 1e3,
                    time.as_secs_f64() * 1e6 / f64::from(*count)
                );
            }
        }
    }
}

impl MacroBackend for CliMacroBackend {
    fn begin(&mut self, program: &Program) -> Result<(), Vec<Diagnostic>> {
        self.prepare_dependencies(program)
    }

    fn available(&self, _module: staple_compiler::ModuleId, declaration: SyntaxId) -> bool {
        self.entries.contains_key(&declaration)
    }

    fn prebuild(&mut self, program: &Program) -> Result<(), Vec<Diagnostic>> {
        let Some(graph) = program.package_graph() else {
            return Ok(());
        };
        let package = graph.root;
        self.prepare_dependencies(program)?;
        self.prepare_package(program, package, true)?;
        self.install_package_library(program, package)
    }

    fn prepare(
        &mut self,
        request: ClosureLibraryRequest<'_>,
    ) -> Result<Vec<SyntaxId>, Vec<Diagnostic>> {
        let started = Instant::now();
        let mut macro_names = Vec::with_capacity(request.entries.len());
        let mut entry_meta = Vec::with_capacity(request.entries.len());
        for (module, decl_id, entry_name) in &request.entries {
            let (name, span) = request
                .program
                .module(*module)
                .syntax
                .items
                .iter()
                .find_map(|item| match item {
                    staple_syntax::Item::MacroDeclaration(decl) if decl.syntax.id == *decl_id => {
                        Some((decl.name.clone(), decl.syntax.span.clone()))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| (entry_name.clone(), Span::Compiler));
            macro_names.push(name.clone());
            entry_meta.push((*module, *decl_id, entry_name.clone(), name, span));
        }

        // Errors that aren't about the source are reported at the call.
        let internal = |message: String| vec![Diagnostic::new(Span::Compiler, message)];

        let package = request.program.package_of(manifest_entries_module(&entry_meta));
        let triple = cache::target_triple(None);
        let extension = compile::shared_library_extension();
        let lib_name = closure_artifact_name(request.program, &entry_meta);
        let dep_fingerprints = if let (Some(pkg), Some(graph)) =
            (package, request.program.package_graph())
        {
            let mut deps = Vec::new();
            for dep in dependency_order(graph, Some(pkg)) {
                if dep == pkg {
                    continue;
                }
                deps.push((
                    graph.package(dep).name.clone(),
                    self.package_fingerprint(request.program, dep)?,
                ));
            }
            deps
        } else {
            Vec::new()
        };
        let fingerprint = closure_library_fingerprint(
            request.program,
            &request.check_program,
            &entry_meta,
            &dep_fingerprints,
            &triple,
            CLOSURE_OPTIMIZATION.name(),
        )
        .map_err(internal)?;
        let placement =
            closure_placement(request.program, package, &triple, self.dependency_build);

        let mut bad_cache = false;
        if let Some(cached_path) =
            find_cached_closure_library(&placement.read, &lib_name, fingerprint, extension)
        {
            let loaded = LibraryManifest::read_beside(&cached_path).is_ok_and(|manifest| {
                self.load_manifest(request.program, &cached_path, &manifest)
                    .is_ok()
            });
            if loaded {
                return Ok(request.entries.iter().map(|(_, id, _)| *id).collect());
            }
            bad_cache = true;
            let _ = std::fs::remove_file(&cached_path);
            let _ = std::fs::remove_file(cached_path.with_extension("manifest"));
        }

        let mut steps = vec![("program", request.program_time)];
        let mut step_started = started;
        let mut step = |name: &'static str| {
            steps.push((name, step_started.elapsed()));
            step_started = Instant::now();
        };
        step("fingerprint");
        let resolved =
            NameResolver::pre_expanded(request.next_syntax_id).resolve_program(request.check_program)?;
        step("resolve");
        let typed = TypeChecker::new().check(resolved)?;
        step("typecheck");
        let lowered = Lowerer::new().lower(&typed)?;
        step("lower");

        let mut entry_symbols = Vec::with_capacity(entry_meta.len());
        let mut manifest_entries = Vec::with_capacity(entry_meta.len());
        for (index, (module, decl_id, entry_name, macro_name, _)) in entry_meta.iter().enumerate() {
            let symbol = instance_symbol(&lowered, entry_name).ok_or_else(|| {
                internal(format!(
                    "internal error: missing lowered entry `{entry_name}` for macro `{macro_name}`"
                ))
            })?;
            entry_symbols.push(symbol);
            manifest_entries.push((*module, *decl_id, format!("__staple_entry.{index}")));
        }

        let effect_entries: Vec<MacroEffectEntry<'_>> = entry_meta
            .iter()
            .zip(&entry_symbols)
            .map(|((_, _, _, macro_name, span), symbol)| MacroEffectEntry {
                macro_name,
                entry_symbol: symbol,
                span: span.clone(),
            })
            .collect();
        let effect_diagnostics = lowered.check_macro_effects(&effect_entries);
        if !effect_diagnostics.is_empty() {
            return Err(effect_diagnostics);
        }
        step("effects");

        let lib_id = self.next_library_id;
        self.next_library_id += 1;
        let obj_path = self.temp_dir.join(format!("closure-{lib_id}.o"));

        let spec = LibrarySpec {
            entries: entry_symbols,
            host_symbols: intrinsics::host_symbols(),
            trap_exit: true,
        };
        let context = inkwell::context::Context::create();
        let generator = CodeGenerator::new(&context);
        let unit = if self.packages_prepared && package.is_some() {
            EmissionUnit::packages([package.expect("a package")]).with_library(spec)
        } else {
            EmissionUnit::whole_program().with_library(spec)
        }
        .with_optimization(CLOSURE_OPTIMIZATION);
        generator.emit_unit_object(&lowered, &unit, &obj_path, None)?;
        step("codegen");

        let mut objects = vec![obj_path];
        if self.packages_prepared {
            if let Some(pkg) = package {
                objects.extend(self.dependency_objects(request.program, pkg)?);
                let rt_placement = macro_placement(request.program, pkg, &triple, false);
                objects.push(self.runtime_object(&generator, &rt_placement)?);
            }
        }
        step("runtime");
        let manifest = build_manifest(request.program, &manifest_entries, &request.templates)
            .map_err(internal)?;
        let write_placement = if bad_cache {
            Placement {
                read: Vec::new(),
                write: placement.write,
            }
        } else {
            placement
        };
        let dylib_path = cache::ensure_library(
            &write_placement,
            &lib_name,
            fingerprint,
            extension,
            &mut |temporary_dylib| {
                compile::link_host_shared_library(&objects, temporary_dylib)?;
                manifest.write_beside(temporary_dylib)
            },
        )
        .map_err(internal)?;
        step("link");
        let loaded = manifest
            .resolve(request.program, &dylib_path)
            .map_err(internal)?;

        let handle = self.client.load(&dylib_path, loaded.client_templates());
        for (decl_id, shim) in loaded.entries {
            self.entries.insert(decl_id, (handle, shim));
        }
        drop(step);

        {
            let mut stats = self.stats.lock().expect("macro stats");
            stats.closure_libraries.push(macro_names);
            stats.closure_steps.push(steps);
            stats.closure_time += started.elapsed();
        }

        Ok(request.entries.iter().map(|(_, id, _)| *id).collect())
    }

    fn call(&mut self, call: &CompiledCall<'_>) -> Option<Result<(Syntax, u64), String>> {
        let (handle, shim) = self.entries.get(&call.declaration.syntax.id)?.clone();
        let context = CallContext {
            definition_module: call.definition_module,
            next_mark: call.next_mark,
            call_site: call.call_site.clone(),
            identifier_mark: None,
        };
        let started = Instant::now();
        let result = self.client.call(
            handle,
            &shim,
            &call.declaration.name,
            context,
            call.arguments.clone(),
        );
        let mut stats = self.stats.lock().expect("macro stats");
        let entry = stats
            .calls
            .entry(call.declaration.name.clone())
            .or_default();
        entry.0 += 1;
        entry.1 += started.elapsed();
        Some(result)
    }
}

/// `packages` and every package it depends on, dependencies first.
fn dependency_order(graph: &PackageGraph, root: Option<PackageId>) -> Vec<PackageId> {
    fn visit(
        graph: &PackageGraph,
        package: PackageId,
        seen: &mut HashSet<PackageId>,
        out: &mut Vec<PackageId>,
    ) {
        if !seen.insert(package) {
            return;
        }
        for dependency in &graph.package(package).dependencies {
            visit(graph, dependency.package, seen, out);
        }
        out.push(package);
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    match root {
        Some(root) => visit(graph, root, &mut seen, &mut out),
        None => {
            for package in 0..graph.packages.len() {
                visit(graph, PackageId(package), &mut seen, &mut out);
            }
        }
    }
    out
}

/// The macro-mode fingerprint of every package: the normal package fingerprint
/// inputs plus a macro-mode tag, the optimization level, and each dependency's
/// own macro-mode fingerprint.
fn package_macro_fingerprints(
    graph: &PackageGraph,
    active: &ActiveFeatures,
) -> Result<HashMap<PackageId, cache::Fingerprint>, String> {
    let order = dependency_order(graph, None);
    let packages = order
        .iter()
        .map(|id| staple_compiler::LoweredPackage {
            id: *id,
            name: graph.package(*id).name.clone(),
            is_root: *id == graph.root,
            dependencies: graph
                .package(*id)
                .dependencies
                .iter()
                .map(|dependency| dependency.package)
                .filter(|dependency| dependency != id)
                .collect(),
        })
        .collect::<Vec<_>>();
    let identity = cache::compiler_identity()?;
    let triple = cache::target_triple(None);
    cache::package_macro_fingerprints(
        graph,
        active,
        &packages,
        identity,
        &triple,
        PACKAGE_OPTIMIZATION.name(),
    )
}

/// Whether `package` is the standard library, whose prebuilt macros live in
/// its install directory.
fn is_standard_library(program: &Program, package: PackageId) -> bool {
    program
        .standard_library_syntax()
        .and_then(|module| program.package_of(module))
        == Some(package)
}

fn standard_library_root(program: &Program) -> Option<PathBuf> {
    let syntax = program.standard_library_syntax()?;
    program
        .module(syntax)
        .path
        .parent()?
        .parent()
        .map(Path::to_path_buf)
}

/// `<stdlib>/target/<triple>/macros` for the standard library, and
/// `<package>/target/staple/<triple>/macros` for other packages.
fn package_macro_directory(
    program: &Program,
    graph: &PackageGraph,
    package: PackageId,
    triple: &str,
) -> PathBuf {
    let info = graph.package(package);
    if is_standard_library(program, package) {
        info.source_root()
            .parent()
            .map_or_else(|| info.directory.clone(), Path::to_path_buf)
            .join("target")
            .join(triple)
            .join("macros")
    } else {
        info.directory
            .join("target/staple")
            .join(triple)
            .join("macros")
    }
}

/// Where to read and write a package's macro-mode object and library.
fn macro_placement(
    program: &Program,
    package: PackageId,
    triple: &str,
    write_own_directory: bool,
) -> Placement {
    let Some(graph) = program.package_graph() else {
        return Placement::default();
    };
    let own = package_macro_directory(program, graph, package, triple);
    let user = cache::user_cache_root().map(|root| root.join("macros"));
    let mut placement = Placement::default();
    placement.read.push(own.clone());
    if let Some(user) = &user {
        placement.read.push(user.clone());
    }
    if write_own_directory {
        placement.write.push(Destination {
            directory: own,
            prune: true,
        });
    }
    if let Some(user) = user {
        placement.write.push(Destination {
            directory: user,
            prune: false,
        });
    }
    placement
}

/// A cached package library and its macro-mode object, when both are present.
fn find_cached_library(
    read: &[PathBuf],
    name: &str,
    fingerprint: cache::Fingerprint,
    extension: &str,
) -> Option<(PathBuf, PathBuf)> {
    let library = format!("{name}-{fingerprint}.{extension}");
    let object = format!("{name}-macro-{fingerprint}.o");
    for directory in read {
        let library_path = directory.join(&library);
        let object_path = directory.join(&object);
        if library_path.is_file()
            && library_path.with_extension("manifest").is_file()
            && object_path.is_file()
        {
            return Some((library_path, object_path));
        }
    }
    None
}

fn manifest_entries_module(
    entry_meta: &[(staple_compiler::ModuleId, SyntaxId, String, String, Span)],
) -> staple_compiler::ModuleId {
    entry_meta[0].0
}

fn sanitize_artifact_component(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The cache name of a closure library: the macro's name for reading, then a
/// hash of its stable key (package, module path, macro name and overload
/// index). Every overload has its own name, so pruning one overload's stale
/// libraries never removes another's, and names that clean to the same text
/// stay distinct.
fn closure_artifact_name(
    program: &Program,
    entry_meta: &[(staple_compiler::ModuleId, SyntaxId, String, String, Span)],
) -> String {
    let (module, declaration, entry_name, macro_name, _) = &entry_meta[0];
    let key = stable_entry_key(program, *module, *declaration).unwrap_or_else(|| {
        let module_name = program
            .module_dotted_name(*module)
            .unwrap_or_else(|| "root".to_owned());
        format!("{module_name}#{entry_name}")
    });
    let mut hasher = cache::Hasher::new();
    hasher.write_str(&key);
    let hash = hasher.finish().to_string();
    format!(
        "closure-{}-{}",
        sanitize_artifact_component(macro_name),
        &hash[..16]
    )
}

/// Where a closure library of `package` is read and written. Only the
/// package a build was asked for writes its own macro directory; a dependency
/// expanded for its package library writes the user cache, as its package
/// library does.
fn closure_placement(
    program: &Program,
    package: Option<PackageId>,
    triple: &str,
    dependency_build: bool,
) -> Placement {
    let is_root = package.is_some() && program.package_of(program.entry()) == package;
    if let Some(package) = package {
        return macro_placement(program, package, triple, is_root && !dependency_build);
    }
    let user = cache::user_cache_root().map(|root| root.join("macros"));
    let mut placement = Placement::default();
    if let Some(user) = user {
        placement.read.push(user.clone());
        placement.write.push(Destination {
            directory: user,
            prune: false,
        });
    }
    placement
}

fn find_cached_closure_library(
    read: &[PathBuf],
    name: &str,
    fingerprint: cache::Fingerprint,
    extension: &str,
) -> Option<PathBuf> {
    let library = format!("{name}-{fingerprint}.{extension}");
    for directory in read {
        let library_path = directory.join(&library);
        if library_path.is_file() && library_path.with_extension("manifest").is_file() {
            return Some(library_path);
        }
    }
    None
}

fn closure_library_fingerprint(
    program: &Program,
    check_program: &Program,
    entry_meta: &[(staple_compiler::ModuleId, SyntaxId, String, String, Span)],
    dependency_fingerprints: &[(String, cache::Fingerprint)],
    triple: &str,
    opt_level: &str,
) -> Result<cache::Fingerprint, String> {
    let identity = cache::compiler_identity()?;
    let mut hasher = cache::Hasher::new();
    hasher.write_str("closure-library");
    hasher.write_str(opt_level);
    hasher.write_fingerprint(identity);
    hasher.write_str(triple);

    let package = program.package_of(manifest_entries_module(entry_meta));
    let mut features = package
        .and_then(|pkg| program.active_features().get(&pkg))
        .map(|features| features.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    features.sort();
    hasher.write(&(features.len() as u64).to_le_bytes());
    for feature in &features {
        hasher.write_str(feature);
    }

    hasher.write(&(dependency_fingerprints.len() as u64).to_le_bytes());
    for (dep_name, dep_fp) in dependency_fingerprints {
        hasher.write_str(dep_name);
        hasher.write_fingerprint(*dep_fp);
    }

    hasher.write(&(entry_meta.len() as u64).to_le_bytes());
    for (module, _, entry_name, macro_name, _) in entry_meta {
        let mod_name = program
            .module_dotted_name(*module)
            .unwrap_or_else(|| "root".to_owned());
        hasher.write_str(&mod_name);
        hasher.write_str(entry_name);
        hasher.write_str(macro_name);
    }

    for module in check_program.modules() {
        if module.syntax.items.is_empty() {
            continue;
        }
        let mod_name = check_program
            .module_dotted_name(module.id)
            .unwrap_or_else(|| "root".to_owned());
        hasher.write_str(&mod_name);
        hasher.write_str(&canonical_items(check_program, &module.syntax.items));
    }

    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_overload_has_its_own_closure_library_name() {
        let program = staple_compiler::ProgramLoader::new()
            .load_source(
                "macro m: Expr -> Expr = x => x\nmacro m: Type -> Type = x => x\n",
                Path::new("."),
            )
            .expect("program loads");
        let module = program.entry();
        let names = program
            .module(module)
            .syntax
            .items
            .iter()
            .filter_map(|item| match item {
                staple_syntax::Item::MacroDeclaration(declaration) => Some(closure_artifact_name(
                    &program,
                    &[(
                        module,
                        declaration.syntax.id,
                        "entry#m".to_owned(),
                        declaration.name.clone(),
                        Span::Compiler,
                    )],
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert!(names.iter().all(|name| name.starts_with("closure-m-")), "{names:?}");
    }
}
