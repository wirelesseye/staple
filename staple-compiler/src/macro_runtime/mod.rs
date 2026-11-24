//! The syntax runtime that compiled macro code calls.
//!
//! Macro code reaches the compiler through the `std.syntax` intrinsics. They
//! are lowered as ordinary external calls; the host supplies their addresses
//! from [`intrinsics::TABLE`]. In tests, [`harness`] runs the checked code
//! in-process through LLVM's JIT.

pub(crate) mod arena;
pub mod args;
pub mod canonical;
pub mod client;
pub mod intrinsics;
pub mod manifest;
pub mod quote;
pub mod server;
pub mod wire;

#[cfg(test)]
pub(crate) mod compare;
#[cfg(test)]
pub(crate) mod harness;

/// Finds the concrete instance symbol a source function's name lowered to.
pub fn instance_symbol(lowered: &crate::LoweredModule, name: &str) -> Option<String> {
    let mangled = crate::resolve::mangle_function_name(name);
    lowered.planned_instance_names(&mangled).into_iter().next()
}

/// Recorded call metadata returned by [`prepare_macro_library_object`] for
/// integration tests.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct TestMacroCall {
    pub name: String,
    pub entry_shim: String,
    pub definition_module: crate::ModuleId,
    pub call_site: staple_syntax::Span,
    pub arguments: Vec<args::ArgTree>,
}

/// Expands, lowers, and emits a shared-library object for integration tests.
#[doc(hidden)]
pub fn prepare_macro_library_object(
    source: &str,
    stdlib: &std::path::Path,
    workspace: &std::path::Path,
    macro_names: &[&str],
    object_path: &std::path::Path,
) -> Result<(Vec<(usize, staple_syntax::Syntax)>, Vec<TestMacroCall>), String> {
    use crate::codegen::{CodeGenerator, EmissionUnit, LibrarySpec};
    use crate::macro_expand::check_program::{CheckMode, check_programs, entry_function_name};
    use crate::macro_expand::compiled_call::record_calls;
    use crate::{Lowerer, NameResolver, ProgramLoader, TypeChecker};

    let program = ProgramLoader::new()
        .with_standard_library_root(stdlib.to_path_buf())
        .load_source(source, workspace)
        .map_err(|errors| format!("load failed: {errors:?}"))?;
    let (expanded_result, calls) = record_calls(|| {
        crate::macro_expand::expand_program_checked(program, true, Some(CheckMode::All), false)
    });
    let (expanded, analysis, _) =
        expanded_result.map_err(|errors| format!("expand failed: {errors:?}"))?;
    let snapshot = analysis
        .check
        .ok_or_else(|| "missing check snapshot".to_owned())?;
    let mut next = analysis.next_syntax_id;
    let check_program_list = check_programs(&expanded, &snapshot, &mut next);
    let (_, _, check_program, templates) = check_program_list
        .into_iter()
        .find(|(package, _, _, _)| package.is_none())
        .ok_or_else(|| "missing entry check program".to_owned())?;

    let next_syntax_id = check_program.next_syntax_id();
    let (resolved, diagnostics) =
        NameResolver::pre_expanded(next_syntax_id).resolve_program_recovering(check_program);
    if !diagnostics.is_empty() {
        return Err(format!("resolve failed: {diagnostics:?}"));
    }
    let resolved = resolved.ok_or_else(|| "the check program could not be resolved".to_owned())?;
    let typed = TypeChecker::new()
        .check(resolved)
        .map_err(|diagnostics| format!("typecheck failed: {diagnostics:?}"))?;
    let lowered = Lowerer::new()
        .lower(&typed)
        .map_err(|diagnostics| format!("lower failed: {diagnostics:?}"))?;

    let entry_module = expanded.entry();
    let module = expanded.module(entry_module);
    let mut entry_symbols = Vec::with_capacity(macro_names.len());
    let mut prepared_calls = Vec::with_capacity(macro_names.len());
    let fallback_call = calls
        .iter()
        .find(|call| call.definition_module == entry_module)
        .cloned();

    for (index, macro_name) in macro_names.iter().enumerate() {
        let decl = module
            .syntax
            .items
            .iter()
            .find_map(|item| match item {
                staple_syntax::Item::MacroDeclaration(decl) if decl.name == *macro_name => {
                    Some(decl)
                }
                _ => None,
            })
            .ok_or_else(|| format!("macro `{macro_name}` not found"))?;
        let fn_name = entry_function_name(decl);
        let mangled = crate::resolve::mangle_function_name(&fn_name);
        let symbol = lowered
            .planned_instance_names(&mangled)
            .into_iter()
            .next()
            .ok_or_else(|| format!("entry instance `{fn_name}` not lowered"))?;
        entry_symbols.push(symbol);

        let recorded = calls
            .iter()
            .find(|call| call.definition_module == entry_module && call.declaration.name == *macro_name)
            .or(fallback_call.as_ref())
            .ok_or_else(|| format!("no recorded call for `{macro_name}`"))?;
        prepared_calls.push(TestMacroCall {
            name: (*macro_name).to_owned(),
            entry_shim: format!("__staple_entry.{index}"),
            definition_module: entry_module,
            call_site: recorded.call_site.clone(),
            arguments: recorded.arguments.clone(),
        });
    }

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
            object_path,
            None,
        )
        .map_err(|diagnostics| format!("codegen failed: {diagnostics:?}"))?;

    let wire_templates = templates
        .templates
        .into_iter()
        .map(|template| (template.module.0, template.contents))
        .collect();
    Ok((wire_templates, prepared_calls))
}
