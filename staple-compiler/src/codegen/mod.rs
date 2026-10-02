//! LLVM emission from a validated, closed [`crate::LoweredProgram`].
//!
//! Codegen consumes only lowered records through their read-only emission view.
//! It declares catalog functions using planned names and linkage, emits concrete
//! instance and initializer bodies, expands recorded drop glue inline, and emits
//! validated adapters, finalizers, coroutine pairs, and reactive runners. Recorded
//! runtime requirements select the runtime modules to link. No source AST or
//! checker query participates in emission; trait selection, coercion choices,
//! and cleanup decisions are already recorded. LLVM type layout and instructions
//! are target-specific work performed here.
//!
//! The concrete ABI represents a callable as a code pointer and an environment
//! pointer. Closure entry points receive the environment first, then effect-row
//! resources in recorded order, then flattened value parameters. Mutable and
//! borrowed non-`Copy` slots pass by pointer; moved slots pass by value unless
//! mutated. Whole-product mutation uses one pointer. Native extern signatures
//! omit the environment and effect prefix and retain C variadic parameters.
//! A native CString argument is its NUL-terminated data pointer.
//! Sum values carry an `i32` tag and an ABI-aligned payload; binding cells retain
//! value and initialization state, with reactive metadata when required.
//! Coroutine resume/cleanup and runtime layouts share fixed field conventions
//! with the linked runtime modules.
//!
//! Emission failures return diagnostics. A module with failed bodies is never
//! returned; successful emission verifies the completed LLVM module.

use crate::LoweredModule;
#[cfg(test)]
use inkwell::values::BasicValue;
use inkwell::{
    OptimizationLevel,
    module::Module as LlvmModule,
    targets::{
        CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetData, TargetMachine,
        TargetTriple,
    },
};
use staple_syntax::{Diagnostic, Span};
#[cfg(test)]
use std::collections::{HashMap, HashSet};
use std::path::Path;
mod abi;
#[doc(hidden)]
pub mod corpus;
mod ir;
mod layout;
mod lowered;
mod runtime;
#[doc(hidden)]
pub use corpus::{CorpusExpectation, CorpusProgram, CorpusSource, codegen_corpus};
use layout::LayoutContext;

pub(crate) struct Backend<'program, 'context> {
    context: &'context inkwell::context::Context,
    llvm_module: inkwell::module::Module<'context>,
    builder: inkwell::builder::Builder<'context>,
    size_type: inkwell::types::IntType<'context>,
    target_data: TargetData,
    layout: LayoutContext<'program>,
}

impl<'program, 'context> Backend<'program, 'context> {
    fn new(
        context: &'context inkwell::context::Context,
        target_machine: &TargetMachine,
        layout: LayoutContext<'program>,
    ) -> Self {
        Self {
            context,
            llvm_module: context.create_module("staple"),
            builder: context.create_builder(),
            size_type: context.ptr_sized_int_type(&target_machine.get_target_data(), None),
            target_data: target_machine.get_target_data(),
            layout,
        }
    }
}

pub struct CodeGenerator<'context> {
    context: &'context inkwell::context::Context,
}
type CodeGenerationResult<T> = Result<T, Diagnostic>;

impl<'context> CodeGenerator<'context> {
    pub fn new(context: &'context inkwell::context::Context) -> Self {
        Self { context }
    }

    pub fn compile_module(&self, module: &LoweredModule) -> Result<String, Vec<Diagnostic>> {
        self.compile_module_for_target(module, None)
    }

    pub fn compile_module_for_target(
        &self,
        module: &LoweredModule,
        target: Option<&str>,
    ) -> Result<String, Vec<Diagnostic>> {
        let target_machine =
            create_target_machine(target).map_err(|diagnostic| vec![diagnostic])?;
        self.compile_llvm_module(module, &target_machine)
            .map(|module| module.print_to_string().to_string())
    }

    pub fn emit_object(
        &self,
        module: &LoweredModule,
        path: &Path,
        target: Option<&str>,
    ) -> Result<(), Vec<Diagnostic>> {
        if path.to_str().is_none() {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "LLVM object output paths must be valid UTF-8",
            )]);
        }
        let target_machine =
            create_target_machine(target).map_err(|diagnostic| vec![diagnostic])?;
        let llvm_module = self.compile_llvm_module(module, &target_machine)?;
        target_machine
            .write_to_file(&llvm_module, FileType::Object, path)
            .map_err(|error| {
                vec![Diagnostic::new(
                    Span::Compiler,
                    format!("could not emit `{}`: {error}", path.display()),
                )]
            })
    }

    fn compile_llvm_module(
        &self,
        module: &LoweredModule,
        target_machine: &TargetMachine,
    ) -> Result<LlvmModule<'context>, Vec<Diagnostic>> {
        lowered::LoweredEmitter::new(self.context, module.program(), target_machine)
            .compile(target_machine)
    }
}

/// Declaration snapshot: the LLVM type and linkage the catalog signatures
/// compile to, taken before any body is emitted. Each entry is the LLVM type
/// and whether the declaration is internal, so the comparison covers linkage
/// as well as the ABI.
#[cfg(test)]
pub(crate) fn lowered_catalog_types(
    context: &inkwell::context::Context,
    module: &LoweredModule,
) -> Result<HashMap<String, (String, bool)>, Vec<Diagnostic>> {
    let target_machine = create_target_machine(None).map_err(|diagnostic| vec![diagnostic])?;
    lowered::LoweredEmitter::new(context, module.program(), &target_machine)
        .declared_catalog_types(&target_machine)
        .map_err(|diagnostic| vec![diagnostic])
}

/// Declaration census input: the lowered module's function types, linkage,
/// and defined-function set.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct LoweredEmissions {
    /// LLVM function types keyed by final planned name.
    pub(crate) function_types: HashMap<String, String>,
    /// LLVM linkage keyed by final planned name (`true` for `Internal`).
    pub(crate) function_linkages: HashMap<String, bool>,
    /// Every function the module defines.
    pub(crate) defined_functions: HashSet<String>,
    /// Ground truth references outside runtime internals and the entry harness.
    #[cfg(test)]
    pub(crate) runtime_references: Vec<(String, String)>,
}

#[cfg(test)]
pub(crate) fn lowered_emissions(
    context: &inkwell::context::Context,
    module: &LoweredModule,
) -> Result<LoweredEmissions, Vec<Diagnostic>> {
    let target_machine = create_target_machine(None).map_err(|diagnostic| vec![diagnostic])?;
    lowered::LoweredEmitter::new(context, module.program(), &target_machine)
        .compile(&target_machine)
        .map(|llvm_module| snapshot_lowered(&llvm_module, module))
}

#[cfg(test)]
fn snapshot_lowered(llvm_module: &LlvmModule<'_>, _module: &LoweredModule) -> LoweredEmissions {
    LoweredEmissions {
        function_types: llvm_module
            .get_functions()
            .filter_map(|function| {
                function.get_name().to_str().ok().map(|name| {
                    (
                        name.to_owned(),
                        function.get_type().print_to_string().to_string(),
                    )
                })
            })
            .collect(),
        function_linkages: llvm_module
            .get_functions()
            .filter_map(|function| {
                function.get_name().to_str().ok().map(|name| {
                    (
                        name.to_owned(),
                        function.get_linkage() == inkwell::module::Linkage::Internal,
                    )
                })
            })
            .collect(),
        defined_functions: llvm_module
            .get_functions()
            .filter(|function| function.count_basic_blocks() > 0)
            .filter_map(|function| function.get_name().to_str().ok().map(str::to_owned))
            .collect(),
        #[cfg(test)]
        runtime_references: {
            let view = _module.program();
            let mut catalog = view
                .instances()
                .filter_map(|(id, _)| view.planned_name(id).map(str::to_owned))
                .collect::<HashSet<_>>();
            for (_, artifact) in view.artifacts() {
                if let Some((resume, cleanup)) = view.planned_coroutine_pair_names(artifact.ordinal)
                {
                    catalog.extend([resume, cleanup]);
                } else if let Some(name) = view.planned_artifact_name(artifact.ordinal) {
                    catalog.insert(name.to_owned());
                }
            }
            catalog.extend(
                view.initializers()
                    .map(|(_, initializer)| initializer.name.clone()),
            );
            let excluded = llvm_module
                .get_functions()
                .filter_map(|function| function.get_name().to_str().ok().map(str::to_owned))
                .filter(|name| !catalog.contains(name))
                .collect();
            referenced_runtime_symbols(llvm_module, &excluded)
        },
    }
}

/// Test-only ground truth for the runtime surfaces: every `(runtime symbol,
/// referencing function)` pair for a use outside the runtime's own functions
/// and the `main` harness. A use that is not an instruction (a constant
/// expression or global initializer) counts, conservatively, under an empty
/// function name.
#[cfg(test)]
fn referenced_runtime_symbols(
    llvm_module: &LlvmModule<'_>,
    excluded_functions: &HashSet<String>,
) -> Vec<(String, String)> {
    use inkwell::values::{AnyValueEnum, InstructionValue};

    fn user_instruction(user: AnyValueEnum<'_>) -> Option<InstructionValue<'_>> {
        match user {
            AnyValueEnum::InstructionValue(instruction) => Some(instruction),
            AnyValueEnum::IntValue(value) => value.as_instruction(),
            AnyValueEnum::FloatValue(value) => value.as_instruction(),
            AnyValueEnum::PointerValue(value) => value.as_instruction(),
            AnyValueEnum::StructValue(value) => value.as_instruction(),
            AnyValueEnum::ArrayValue(value) => value.as_instruction(),
            AnyValueEnum::VectorValue(value) => value.as_instruction(),
            AnyValueEnum::ScalableVectorValue(value) => value.as_instruction(),
            AnyValueEnum::PhiValue(value) => Some(value.as_instruction()),
            AnyValueEnum::FunctionValue(_) | AnyValueEnum::MetadataValue(_) => None,
        }
    }

    let mut references = Vec::new();
    for function in llvm_module.get_functions() {
        let Ok(name) = function.get_name().to_str() else {
            continue;
        };
        if crate::RuntimeRequirement::for_runtime_symbol(name).is_none() {
            continue;
        }
        let mut next = function
            .as_global_value()
            .as_pointer_value()
            .get_first_use();
        while let Some(use_) = next {
            next = use_.get_next_use();
            let parent = user_instruction(use_.get_user())
                .and_then(|instruction| instruction.get_parent())
                .and_then(|block| block.get_parent())
                .and_then(|parent| parent.get_name().to_str().ok().map(str::to_string));
            let excluded = parent
                .as_deref()
                .is_some_and(|parent| parent == "main" || excluded_functions.contains(parent));
            if !excluded {
                let reference = (name.to_string(), parent.unwrap_or_default());
                if !references.contains(&reference) {
                    references.push(reference);
                }
            }
        }
    }
    references
}

fn create_target_machine(target: Option<&str>) -> CodeGenerationResult<TargetMachine> {
    Target::initialize_all(&InitializationConfig::default());
    let triple = target
        .map(TargetTriple::create)
        .unwrap_or_else(TargetMachine::get_default_triple);
    let target = Target::from_triple(&triple)
        .map_err(|error| Diagnostic::new(Span::Compiler, error.to_string()))?;
    target
        .create_target_machine(
            &triple,
            "generic",
            "",
            OptimizationLevel::Default,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| Diagnostic::new(Span::Compiler, "could not create LLVM target machine"))
}

/// Whether a resolved function `candidate` denotes the standard-library
/// function whose source name is `name`. Standard-library definitions are
/// emitted with their bare source name in single-module builds but are
/// mangled to `__staple_m{module-prefix}.{name}` once the program spans more
/// than one non-standard module (see the name mangling in `resolve`), where
/// `module-prefix` is the module's dotted path (`std.io`). A bare string
/// comparison misses the mangled form, so also accept a candidate whose final
/// `.`-separated component is `name`. Callers pass plain identifier names.
fn compiler_diagnostic(error: inkwell::builder::BuilderError) -> Diagnostic {
    Diagnostic::new(Span::Compiler, error.to_string())
}
