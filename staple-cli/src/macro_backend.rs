use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use staple_compiler::macro_runtime::client::{LibraryHandle, MacroClient};
use staple_compiler::macro_runtime::intrinsics;
use staple_compiler::macro_runtime::manifest::build_manifest;
use staple_compiler::macro_runtime::quote::CallContext;
use staple_compiler::macro_runtime::instance_symbol;
use staple_compiler::{
    ClosureLibraryRequest, CodeGenerator, CompiledCall, EmissionUnit, LibrarySpec, Lowerer,
    MacroBackend, MacroEffectEntry, NameResolver, TypeChecker,
};
use staple_syntax::{Diagnostic, Span, Syntax, SyntaxId};

use crate::compile;

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

/// Builds closure libraries and runs macro calls in a macro server.
///
/// Macro libraries run in the compiler's own process tree, so they are always
/// built for the host, with the host's C compiler, whatever `--target`,
/// `--linker`, `-L` and `-l` say about the program being built.
pub struct CliMacroBackend {
    client: MacroClient,
    entries: HashMap<SyntaxId, (LibraryHandle, String)>,
    temp_dir: PathBuf,
    next_library_id: usize,
    libraries_built: Vec<Vec<String>>,
    total_prepare_time: Duration,
}

impl CliMacroBackend {
    pub fn new() -> Result<Self, String> {
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
            libraries_built: Vec::new(),
            total_prepare_time: Duration::ZERO,
        })
    }
}

impl Drop for CliMacroBackend {
    fn drop(&mut self) {
        self.client.shutdown();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
        if std::env::var_os("STAPLE_MACRO_STATS").is_some() {
            eprintln!(
                "staple macro stats: {} closure libraries built in {:.3}s",
                self.libraries_built.len(),
                self.total_prepare_time.as_secs_f64()
            );
            for (index, names) in self.libraries_built.iter().enumerate() {
                eprintln!("  lib #{index}: {}", names.join(", "));
            }
        }
    }
}

impl MacroBackend for CliMacroBackend {
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

        let resolved =
            NameResolver::pre_expanded(request.next_syntax_id).resolve_program(request.check_program)?;
        let typed = TypeChecker::new().check(resolved)?;
        let lowered = Lowerer::new().lower(&typed)?;

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

        let lib_id = self.next_library_id;
        self.next_library_id += 1;
        let obj_path = self.temp_dir.join(format!("closure-{lib_id}.o"));
        let dylib_path = self.temp_dir.join(format!(
            "closure-{lib_id}.{}",
            compile::shared_library_extension()
        ));

        let spec = LibrarySpec {
            entries: entry_symbols,
            host_symbols: intrinsics::host_symbols(),
            trap_exit: true,
        };
        let context = inkwell::context::Context::create();
        CodeGenerator::new(&context)
            .emit_unit_object(
                &lowered,
                &EmissionUnit::whole_program().with_library(spec),
                &obj_path,
                None,
            )?;

        compile::link_host_shared_library(&[obj_path], &dylib_path).map_err(internal)?;

        let manifest = build_manifest(request.program, &manifest_entries, &request.templates)
            .map_err(internal)?;
        manifest.write_beside(&dylib_path).map_err(internal)?;
        let loaded = manifest
            .resolve(request.program, &dylib_path)
            .map_err(internal)?;

        let handle = self.client.load(&dylib_path, loaded.client_templates());
        for (decl_id, shim) in loaded.entries {
            self.entries.insert(decl_id, (handle, shim));
        }

        self.libraries_built.push(macro_names);
        self.total_prepare_time += started.elapsed();

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
        Some(self.client.call(
            handle,
            &shim,
            &call.declaration.name,
            context,
            call.arguments.clone(),
        ))
    }
}
