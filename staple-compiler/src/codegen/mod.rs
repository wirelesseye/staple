//! LLVM emission from a validated, closed [`crate::LoweredProgram`].
//!
//! Codegen consumes only lowered records through their read-only emission view.
//! It declares catalog functions using planned names and linkage, emits concrete
//! instance and initializer bodies, emits descriptor ABI bodies, expands recorded
//! drop glue inline, and emits
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
//! Emission covers one [`EmissionUnit`] at a time. The whole-program unit is a
//! single module; per-package units and the runtime object
//! ([`CodeGenerator::emit_runtime_object`]) link into the same program.
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
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
mod abi;
mod aggregates;
mod conversions;
#[doc(hidden)]
pub mod corpus;
mod dynamic;
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
    /// The private constant for each `@track_caller` reference position,
    /// deduplicated per LLVM module.
    caller_sites: std::cell::RefCell<
        std::collections::HashMap<(String, u32, u32), inkwell::values::PointerValue<'context>>,
    >,
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
            caller_sites: std::cell::RefCell::default(),
        }
    }
}

/// The part of a lowered program one LLVM module (one object file) covers.
///
/// Lowering is whole-program, so every unit sees the full catalog. A unit
/// defines, with external linkage, the module-level functions, storage globals,
/// and initializers of the packages it owns and only declares those of other
/// packages. Everything else (generic instances, local and anonymous
/// functions, and every generated artifact) is an internal copy private to the
/// object, so ordinal-numbered names never cross an object boundary. A unit
/// can also define the program entry (`emit_main`) and carry the runtime
/// (`include_runtime`); a unit without the runtime only declares its entry
/// points, to be linked against [`CodeGenerator::emit_runtime_object`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmissionUnit {
    scope: UnitScope,
    emit_main: bool,
    include_runtime: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UnitScope {
    Everything,
    Packages {
        packages: HashSet<staple_project::PackageId>,
        unpackaged: bool,
    },
}

impl EmissionUnit {
    /// The single-module unit: every package, the entry, and the runtime.
    pub fn whole_program() -> Self {
        Self {
            scope: UnitScope::Everything,
            emit_main: true,
            include_runtime: true,
        }
    }

    /// The root unit of a program: its root package (if it has one) and any
    /// modules outside every package, with the entry but without the runtime.
    pub fn root(module: &LoweredModule) -> Self {
        Self::packages(
            module
                .packages()
                .iter()
                .filter(|package| package.is_root)
                .map(|package| package.id),
        )
        .with_unpackaged(true)
        .with_main(true)
    }

    /// A unit that owns the given packages, with neither entry nor runtime.
    pub fn packages(packages: impl IntoIterator<Item = staple_project::PackageId>) -> Self {
        Self {
            scope: UnitScope::Packages {
                packages: packages.into_iter().collect(),
                unpackaged: false,
            },
            emit_main: false,
            include_runtime: false,
        }
    }

    /// Also owns the modules that belong to no package (a single-file
    /// program). Meaningful for the root unit only.
    pub fn with_unpackaged(mut self, unpackaged_owned: bool) -> Self {
        if let UnitScope::Packages { unpackaged, .. } = &mut self.scope {
            *unpackaged = unpackaged_owned;
        }
        self
    }

    /// Whether the unit defines the program entry, which runs every
    /// initializer in initialization order.
    pub fn with_main(mut self, emit_main: bool) -> Self {
        self.emit_main = emit_main;
        self
    }

    /// Whether the unit carries the GC, coroutine, and reactive runtimes.
    pub fn with_runtime(mut self, include_runtime: bool) -> Self {
        self.include_runtime = include_runtime;
        self
    }

    pub fn is_whole_program(&self) -> bool {
        self.scope == UnitScope::Everything
    }

    pub(crate) fn emits_main(&self) -> bool {
        self.emit_main
    }

    pub(crate) fn includes_runtime(&self) -> bool {
        self.include_runtime
    }

    /// Whether the unit owns a module of the given package.
    pub(crate) fn owns(&self, package: Option<staple_project::PackageId>) -> bool {
        match &self.scope {
            UnitScope::Everything => true,
            UnitScope::Packages {
                packages,
                unpackaged,
            } => match package {
                Some(package) => packages.contains(&package),
                None => *unpackaged,
            },
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
        self.compile_unit_for_target(module, &EmissionUnit::whole_program(), target)
    }

    /// The LLVM IR of one emission unit.
    pub fn compile_unit_for_target(
        &self,
        module: &LoweredModule,
        unit: &EmissionUnit,
        target: Option<&str>,
    ) -> Result<String, Vec<Diagnostic>> {
        let target_machine =
            create_target_machine(target).map_err(|diagnostic| vec![diagnostic])?;
        self.compile_llvm_module(module, unit, &target_machine)
            .map(|module| module.print_to_string().to_string())
    }

    pub fn emit_object(
        &self,
        module: &LoweredModule,
        path: &Path,
        target: Option<&str>,
    ) -> Result<(), Vec<Diagnostic>> {
        self.emit_unit_object(module, &EmissionUnit::whole_program(), path, target)
    }

    /// Writes one emission unit as an object file.
    pub fn emit_unit_object(
        &self,
        module: &LoweredModule,
        unit: &EmissionUnit,
        path: &Path,
        target: Option<&str>,
    ) -> Result<(), Vec<Diagnostic>> {
        let target_machine = object_target_machine(path, target)?;
        let llvm_module = self.compile_llvm_module(module, unit, &target_machine)?;
        write_object(&target_machine, &llvm_module, path)
    }

    /// The LLVM IR of the runtime object.
    pub fn compile_runtime_for_target(
        &self,
        target: Option<&str>,
    ) -> Result<String, Vec<Diagnostic>> {
        let target_machine =
            create_target_machine(target).map_err(|diagnostic| vec![diagnostic])?;
        self.compile_runtime_module(&target_machine)
            .map(|module| module.print_to_string().to_string())
    }

    /// Writes the garbage collector, coroutine, and reactive runtimes as one
    /// object file, for units emitted without the runtime to link against.
    pub fn emit_runtime_object(
        &self,
        path: &Path,
        target: Option<&str>,
    ) -> Result<(), Vec<Diagnostic>> {
        let target_machine = object_target_machine(path, target)?;
        let llvm_module = self.compile_runtime_module(&target_machine)?;
        write_object(&target_machine, &llvm_module, path)
    }

    fn compile_llvm_module(
        &self,
        module: &LoweredModule,
        unit: &EmissionUnit,
        target_machine: &TargetMachine,
    ) -> Result<LlvmModule<'context>, Vec<Diagnostic>> {
        lowered::LoweredEmitter::new(self.context, module.program(), target_machine, unit)
            .compile(target_machine)
    }

    fn compile_runtime_module(
        &self,
        target_machine: &TargetMachine,
    ) -> Result<LlvmModule<'context>, Vec<Diagnostic>> {
        let module = self.context.create_module("staple-runtime");
        module.set_triple(&target_machine.get_triple());
        let target_data = target_machine.get_target_data();
        module.set_data_layout(&target_data.get_data_layout());
        for install in [
            runtime::link_gc_runtime,
            runtime::link_io_runtime,
            runtime::link_coroutine_runtime,
            runtime::link_reactive_runtime,
        ] {
            install(self.context, &target_data, &module).map_err(|diagnostic| vec![diagnostic])?;
        }
        runtime::export_runtime_functions(&module);
        module
            .verify()
            .map_err(|message| vec![runtime_module_diagnostic(message)])?;
        Ok(module)
    }
}

fn runtime_module_diagnostic(message: inkwell::support::LLVMString) -> Diagnostic {
    Diagnostic::new(
        Span::Compiler,
        format!("invalid runtime module: {}", message.to_string_lossy()),
    )
}

fn object_target_machine(
    path: &Path,
    target: Option<&str>,
) -> Result<TargetMachine, Vec<Diagnostic>> {
    if path.to_str().is_none() {
        return Err(vec![Diagnostic::new(
            Span::Compiler,
            "LLVM object output paths must be valid UTF-8",
        )]);
    }
    create_target_machine(target).map_err(|diagnostic| vec![diagnostic])
}

fn write_object(
    target_machine: &TargetMachine,
    llvm_module: &LlvmModule<'_>,
    path: &Path,
) -> Result<(), Vec<Diagnostic>> {
    target_machine
        .write_to_file(llvm_module, FileType::Object, path)
        .map_err(|error| {
            vec![Diagnostic::new(
                Span::Compiler,
                format!("could not emit `{}`: {error}", path.display()),
            )]
        })
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
    lowered::LoweredEmitter::new(
        context,
        module.program(),
        &target_machine,
        &EmissionUnit::whole_program(),
    )
    .declared_catalog_types(&target_machine)
    .map_err(|diagnostic| vec![diagnostic])
}

/// Emit and verify one production instance body independently of its callers.
#[cfg(test)]
pub(crate) fn lowered_instance_body_ir(
    context: &inkwell::context::Context,
    module: &LoweredModule,
    instance: crate::FunctionInstanceId,
) -> Result<String, Vec<Diagnostic>> {
    let target_machine = create_target_machine(None).map_err(|diagnostic| vec![diagnostic])?;
    lowered::LoweredEmitter::new(
        context,
        module.program(),
        &target_machine,
        &EmissionUnit::whole_program(),
    )
    .instance_body_ir(&target_machine, instance)
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
    /// `(function, instruction)` for every `alloca` outside its function's
    /// entry block, including the linked runtime modules.
    #[cfg(test)]
    pub(crate) misplaced_allocas: Vec<(String, String)>,
}

#[cfg(test)]
pub(crate) fn lowered_emissions(
    context: &inkwell::context::Context,
    module: &LoweredModule,
) -> Result<LoweredEmissions, Vec<Diagnostic>> {
    let target_machine = create_target_machine(None).map_err(|diagnostic| vec![diagnostic])?;
    lowered::LoweredEmitter::new(
        context,
        module.program(),
        &target_machine,
        &EmissionUnit::whole_program(),
    )
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
        #[cfg(test)]
        misplaced_allocas: {
            let mut misplaced = Vec::new();
            for function in llvm_module.get_functions() {
                let name = function.get_name().to_string_lossy().into_owned();
                for block in function.get_basic_blocks().into_iter().skip(1) {
                    for instruction in block.get_instructions() {
                        if instruction.get_opcode() == inkwell::values::InstructionOpcode::Alloca {
                            misplaced.push((
                                name.clone(),
                                inkwell::values::AnyValue::print_to_string(&instruction)
                                    .to_string(),
                            ));
                        }
                    }
                }
            }
            misplaced
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
