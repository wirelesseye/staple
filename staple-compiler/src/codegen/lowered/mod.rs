//! LLVM emitter over the read-only lowered program view.

mod coroutines;
mod reactive;
mod structural;

use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    basic_block::BasicBlock,
    module::{Linkage, Module as LlvmModule},
    targets::TargetMachine,
    types::BasicTypeEnum,
    values::{
        AnyValue, AnyValueEnum, BasicMetadataValueEnum, BasicValueEnum, FunctionValue, GlobalValue,
        PointerValue,
    },
};

use crate::specialization::ArtifactOrdinal;
use crate::{
    BlockId, CheckedMutation, CheckedResource, CheckedType, DropGlueBody, DropGluePlan,
    EmissionView, ExpressionId, FunctionInstanceId, GcFinalizerPlan, InitializerId,
    IntrinsicFunction, LoweredArgumentPassMode, LoweredArtifactPlan, LoweredBindingSite,
    LoweredBoundTarget, LoweredCallArgument, LoweredCallEnvironment, LoweredCallId,
    LoweredCallStep, LoweredCallableAdapter, LoweredCallableTarget, LoweredCallableValueId,
    LoweredClosureEnvironment, LoweredEntryResourceKind, LoweredExpressionKind,
    LoweredInstanceCapture, LoweredItemKind, LoweredPatternKind, LoweredProviderStorage,
    LoweredReactiveOperationKind, LoweredResourceProviderId, LoweredScopeExit, ModuleId,
    OwnedStorage, PatternId, PlannedArtifact, RuntimeRequirement, SymbolId,
};

use super::abi::flattened_parameter_types;
use super::{
    Backend, CodeGenerationResult, Diagnostic, LayoutContext, compiler_diagnostic,
    ir::value_as_basic,
};
use crate::lower::EmissionOwner;

#[derive(Default, Clone)]
struct FunctionEnvironment<'context> {
    locals: HashMap<SymbolId, AnyValueEnum<'context>>,
    binding_cells: HashMap<SymbolId, PointerValue<'context>>,
    parameter_pointers: HashMap<SymbolId, PointerValue<'context>>,
    closure_environment: Option<PointerValue<'context>>,
    /// The resource value each active provider supplies, keyed by the
    /// provider a `LoweredResourceUse` names. The emitter never selects a
    /// provider by type; lowering already recorded the selection.
    resources: HashMap<LoweredResourceProviderId, BoundResource<'context>>,
    reactive_scopes: Vec<PointerValue<'context>>,
    task_scopes: Vec<PointerValue<'context>>,
    loops: Vec<LoopContext<'context>>,
    /// The owned bindings currently in scope, keyed by
    /// symbol. `owned_order` preserves binding registration order;
    /// scope exits drop in reverse from a mark.
    owned: HashMap<SymbolId, OwnedValue<'context>>,
    owned_order: Vec<SymbolId>,
    returned: bool,
    coroutine: Option<coroutines::CoroutineContext<'context>>,
}

/// One registered owned binding. A `Value` owns its
/// SSA local with an `i1` live flag; a `Cell` owns its binding cell and is
/// dropped conditionally on the cell state. `glue` is the drop glue the
/// owner's `OwnedBinding` use record names.
#[derive(Clone)]
struct OwnedValue<'context> {
    storage: OwnedStorage,
    glue: ArtifactOrdinal,
    value: Option<AnyValueEnum<'context>>,
    live: Option<PointerValue<'context>>,
}

impl<'context> FunctionEnvironment<'context> {
    /// Match arms and
    /// logical operands restore the caller's local bindings, including the
    /// owned-binding registration order.
    fn restore_local_state(&mut self, snapshot: &Self) {
        self.locals = snapshot.locals.clone();
        self.binding_cells = snapshot.binding_cells.clone();
        self.parameter_pointers = snapshot.parameter_pointers.clone();
        self.owned = snapshot.owned.clone();
        self.owned_order = snapshot.owned_order.clone();
    }
}

/// How a drop position obtains the value it drops.
enum DropSource<'context> {
    /// The value is already evaluated.
    Value(BasicValueEnum<'context>),
    /// Load from the target place pointer.
    Place(PointerValue<'context>),
    /// Load from a mutation temporary.
    Temporary(PointerValue<'context>),
    /// Test the cell state, load the
    /// value, expand the glue, then clear the state.
    Cell(PointerValue<'context>),
}

/// One provider's bound resource value. emission reads these when it emits
/// `LoweredResourceUse` reads and call `resource_bindings`.
#[derive(Clone)]
struct BoundResource<'context> {
    resource: CheckedResource,
    value: AnyValueEnum<'context>,
    /// Reads pass through a pointer (`LoweredResourceProvider::indirect`).
    indirect: bool,
}

/// One active loop's context. The emitter
/// `LoopCodegenContext` carries the header, exit, cleanup marks, and the
/// break-value phi inputs.
#[derive(Clone)]
struct LoopContext<'context> {
    depth: usize,
    header: BasicBlock<'context>,
    exit: BasicBlock<'context>,
    /// The owned-binding registration mark at loop entry; `break` and
    /// `continue` drop back to it.
    owned_before: usize,
    /// The reactive-scope depth at loop entry; `break` and `continue`
    /// dispose every scope opened since, before the owned drops.
    reactive_before: usize,
    /// The task-scope depth at loop entry; `break` and `continue` close every
    /// scope opened since, right after the reactive disposal and before the
    /// owned drops .
    tasks_before: usize,
    incoming: Vec<(BasicValueEnum<'context>, BasicBlock<'context>)>,
}

pub(super) struct LoweredEmitter<'program, 'context> {
    view: EmissionView<'program>,
    backend: Backend<'program, 'context>,
    instances: HashMap<FunctionInstanceId, FunctionValue<'context>>,
    artifacts: HashMap<ArtifactOrdinal, Vec<FunctionValue<'context>>>,
    externs: HashMap<SymbolId, FunctionValue<'context>>,
    /// The declared adapter of each extern symbol used as a first-class value
    /// (closure codes). A capture or name read of an extern value
    /// builds this closure instead of looking up local storage.
    extern_adapters: HashMap<SymbolId, FunctionValue<'context>>,
    /// The declared binding symbol of each function template. A `Stored`
    /// callable value loads its closure from that symbol's storage, mirroring
    /// compile symbol value.
    function_symbols: HashMap<crate::FunctionId, SymbolId>,
    storage: HashMap<SymbolId, GlobalValue<'context>>,
    initialization_states: HashMap<SymbolId, GlobalValue<'context>>,
    signal_metadata: HashMap<SymbolId, GlobalValue<'context>>,
    derived_metadata: HashMap<SymbolId, GlobalValue<'context>>,
    initializers: HashMap<InitializerId, FunctionValue<'context>>,
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    pub(super) fn new(
        context: &'context inkwell::context::Context,
        view: EmissionView<'program>,
        target_machine: &TargetMachine,
    ) -> Self {
        Self {
            view,
            backend: Backend::new(context, target_machine, LayoutContext::new(view)),
            instances: HashMap::new(),
            artifacts: HashMap::new(),
            externs: HashMap::new(),
            extern_adapters: HashMap::new(),
            function_symbols: HashMap::new(),
            storage: HashMap::new(),
            initialization_states: HashMap::new(),
            signal_metadata: HashMap::new(),
            derived_metadata: HashMap::new(),
            initializers: HashMap::new(),
        }
    }

    /// Strict emission: every body is attempted even after an earlier one
    /// fails, so the returned diagnostic list covers every unsupported body
    ///The module is only verified when no body failed; a module with
    /// failed bodies is never returned.
    pub(super) fn compile(
        mut self,
        target_machine: &TargetMachine,
    ) -> Result<LlvmModule<'context>, Vec<Diagnostic>> {
        self.declare_program(target_machine)
            .map_err(|diagnostic| vec![diagnostic])?;
        let mut diagnostics = Vec::new();
        self.emit_instance_bodies(&mut diagnostics);
        self.emit_artifact_bodies(&mut diagnostics);
        self.emit_initializers(&mut diagnostics);
        if let Err(diagnostic) = self.emit_main() {
            diagnostics.push(diagnostic);
        }
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        self.backend
            .llvm_module
            .verify()
            .map_err(|message| vec![invalid_module_diagnostic(message)])?;
        Ok(self.backend.llvm_module)
    }

    fn declare_program(&mut self, target_machine: &TargetMachine) -> CodeGenerationResult<()> {
        self.backend
            .llvm_module
            .set_triple(&target_machine.get_triple());
        self.backend
            .llvm_module
            .set_data_layout(&target_machine.get_target_data().get_data_layout());

        // An executable always needs the collector for its entry harness.
        self.backend.install_gc_runtime()?;
        let requirements = self.view.runtime_requirements();
        if requirements.contains(RuntimeRequirement::ReactiveRuntime) {
            self.backend.install_reactive_runtime()?;
        }
        if requirements.contains(RuntimeRequirement::CoroutineRuntime) {
            self.backend.install_coroutine_runtime()?;
        }
        self.declare_required_runtime_symbols()?;

        self.declare_externs()?;
        for (id, function) in self.view.functions() {
            if let Some(symbol) = function.binding_symbol {
                self.function_symbols.entry(id).or_insert(symbol);
            }
        }
        self.declare_instances()?;
        self.declare_artifacts()?;
        self.declare_storage()?;
        self.declare_initializers()?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn declared_catalog_types(
        mut self,
        target_machine: &TargetMachine,
    ) -> CodeGenerationResult<HashMap<String, (String, bool)>> {
        self.declare_program(target_machine)?;
        let mut types = HashMap::new();
        for function in self
            .instances
            .values()
            .chain(self.artifacts.values().flatten())
        {
            let name = function.get_name().to_str().map_err(|_| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "internal invariant violated: planned names are UTF-8",
                )
            })?;
            types.insert(
                name.to_owned(),
                (
                    function.get_type().print_to_string().to_string(),
                    function.get_linkage() == Linkage::Internal,
                ),
            );
        }
        for function in self.initializers.values() {
            let name = function.get_name().to_str().map_err(|_| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "internal invariant violated: initializer names are UTF-8",
                )
            })?;
            types.insert(
                name.to_owned(),
                (
                    function.get_type().print_to_string().to_string(),
                    function.get_linkage() == Linkage::Internal,
                ),
            );
        }
        Ok(types)
    }

    fn declare_instances(&mut self) -> CodeGenerationResult<()> {
        // Linkage rule per family: an instance whose
        // template signature still has a type parameter is declared on demand
        // by ensure function specialization with `Internal` linkage
        // (the recorded lowering fact); an instance of a non-generic template
        // keeps the eager declaration's default (external) linkage. Names
        // always come from the catalog, never from the backend.
        for (id, instance) in self.view.instances() {
            let Some(signature) = self.view.instance_signature(id) else {
                continue;
            };
            if self
                .view
                .function(instance.template)
                .is_some_and(|template| template.class.coroutine_body)
            {
                continue;
            }
            let name = self.view.planned_name(id).ok_or_else(|| {
                Diagnostic::new(
                    instance.origin.span.clone(),
                    "missing planned instance name",
                )
            })?;
            let function_type = self.backend.compile_closure_function_type(signature)?;
            let linkage = instance
                .requires_internal_linkage
                .then_some(Linkage::Internal);
            let function = self
                .backend
                .llvm_module
                .add_function(name, function_type, linkage);
            self.instances.insert(id, function);
        }
        Ok(())
    }

    fn declare_required_runtime_symbols(&mut self) -> CodeGenerationResult<()> {
        let requirements = self.view.runtime_requirements();
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        if requirements.contains(RuntimeRequirement::Utf8Validator) {
            self.backend.build_utf8_validator()?;
        }
        if requirements.contains(RuntimeRequirement::CStringFree) {
            self.backend.declare_named_function(
                "free",
                self.backend
                    .context
                    .void_type()
                    .fn_type(&[pointer.into()], false),
            );
        }
        if requirements.contains(RuntimeRequirement::LiteralComparison) {
            self.backend.declare_named_function(
                "memcmp",
                self.backend.context.i32_type().fn_type(
                    &[
                        pointer.into(),
                        pointer.into(),
                        self.backend.size_type.into(),
                    ],
                    false,
                ),
            );
        }
        if requirements.contains(RuntimeRequirement::NumericToString) {
            self.backend.declare_named_function(
                "snprintf",
                self.backend.context.i32_type().fn_type(
                    &[
                        pointer.into(),
                        self.backend.size_type.into(),
                        pointer.into(),
                    ],
                    true,
                ),
            );
        }
        if requirements.contains(RuntimeRequirement::CStringLength) {
            self.backend.declare_named_function(
                "strlen",
                self.backend.size_type.fn_type(&[pointer.into()], false),
            );
        }
        if requirements.contains(RuntimeRequirement::InteriorNulCheck) {
            self.backend.declare_named_function(
                "memchr",
                pointer.fn_type(
                    &[
                        pointer.into(),
                        self.backend.context.i32_type().into(),
                        self.backend.size_type.into(),
                    ],
                    false,
                ),
            );
        }
        Ok(())
    }

    fn declare_externs(&mut self) -> CodeGenerationResult<()> {
        for (id, symbol) in self.view.symbols() {
            if !symbol.external || symbol.intrinsic.is_some() {
                continue;
            }
            let CheckedType::Function(signature) = &symbol.value_type else {
                return Err(Diagnostic::new(
                    symbol.origin.span.clone(),
                    "external bindings must have a function type",
                ));
            };
            let llvm_type = self.backend.compile_native_function_type(signature)?;
            let name = if symbol.overloaded {
                let arity = if signature.parameter_style
                    == staple_syntax::FunctionParameterStyle::Juxtaposed
                {
                    match signature.parameter.as_ref() {
                        CheckedType::Product(product) => product.elements.len(),
                        _ => 1,
                    }
                } else {
                    1
                };
                format!("{}.arity{arity}", symbol.name)
            } else {
                symbol.name.clone()
            };
            let function = match self.backend.llvm_module.get_function(&name) {
                Some(existing) if existing.get_type() == llvm_type => existing,
                Some(_) => {
                    return Err(Diagnostic::new(
                        symbol.origin.span.clone(),
                        format!(
                            "extern `{}` was already declared with a different type",
                            symbol.name
                        ),
                    ));
                }
                None => self
                    .backend
                    .llvm_module
                    .add_function(&name, llvm_type, None),
            };
            self.externs.insert(id, function);
        }
        Ok(())
    }

    fn declare_artifacts(&mut self) -> CodeGenerationResult<()> {
        // Linkage rule per family: constructor adapters,
        // extern adapters, runners, and the coroutine `resume`/`cleanup` pair
        // are `Internal`; structural methods and GC finalizers keep the
        // default (external) linkage; drop glue emits no function.
        // Coroutine pair names come from the catalog, where
        // `planned_names_with` collision-checks them with every other planned
        // name. The remaining names are the artifact's planned name.
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        for (_, artifact) in self.view.artifacts() {
            let plan = artifact.plan.as_ref().ok_or_else(|| {
                Diagnostic::new(artifact.origin.span.clone(), "missing artifact plan")
            })?;
            let name = self
                .view
                .planned_artifact_name(artifact.ordinal)
                .ok_or_else(|| {
                    Diagnostic::new(
                        artifact.origin.span.clone(),
                        "missing planned artifact name",
                    )
                })?;
            let functions = match plan {
                LoweredArtifactPlan::ConstructorAdapter(plan) => {
                    let ty = self
                        .backend
                        .compile_closure_function_type(&plan.callable_type)?;
                    vec![
                        self.backend
                            .llvm_module
                            .add_function(name, ty, Some(Linkage::Internal)),
                    ]
                }
                LoweredArtifactPlan::StructuralMethod(plan) => {
                    let ty = self
                        .backend
                        .compile_closure_function_type(&plan.callable_type)?;
                    vec![self.backend.llvm_module.add_function(name, ty, None)]
                }
                LoweredArtifactPlan::DropGlue(_) => Vec::new(),
                LoweredArtifactPlan::GcFinalizer(_) => {
                    let ty = self
                        .backend
                        .context
                        .void_type()
                        .fn_type(&[pointer.into()], false);
                    vec![self.backend.llvm_module.add_function(name, ty, None)]
                }
                LoweredArtifactPlan::CoroutineCodes(_) => {
                    let (resume_name, cleanup_name) = self
                        .view
                        .planned_coroutine_pair_names(artifact.ordinal)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                artifact.origin.span.clone(),
                                "missing planned coroutine pair names",
                            )
                        })?;
                    let status = self.backend.context.struct_type(
                        &[self.backend.context.i8_type().into(), pointer.into()],
                        false,
                    );
                    let resume = self.backend.llvm_module.add_function(
                        &resume_name,
                        status.fn_type(&[pointer.into()], false),
                        Some(Linkage::Internal),
                    );
                    let cleanup = self.backend.llvm_module.add_function(
                        &cleanup_name,
                        self.backend
                            .context
                            .void_type()
                            .fn_type(&[pointer.into()], false),
                        Some(Linkage::Internal),
                    );
                    vec![resume, cleanup]
                }
                LoweredArtifactPlan::ReactionRunner(_)
                | LoweredArtifactPlan::UntilRunner(_)
                | LoweredArtifactPlan::DerivedRunner(_) => {
                    let ty = self
                        .backend
                        .context
                        .void_type()
                        .fn_type(&[pointer.into()], false);
                    vec![
                        self.backend
                            .llvm_module
                            .add_function(name, ty, Some(Linkage::Internal)),
                    ]
                }
                LoweredArtifactPlan::ExternAdapter(plan) => {
                    let ty = self
                        .backend
                        .compile_closure_function_type(&plan.callable_type)?;
                    let function =
                        self.backend
                            .llvm_module
                            .add_function(name, ty, Some(Linkage::Internal));
                    self.extern_adapters.insert(plan.symbol, function);
                    vec![function]
                }
            };
            self.artifacts.insert(artifact.ordinal, functions);
        }
        Ok(())
    }

    fn module_prefix(&self, id: ModuleId) -> CodeGenerationResult<&str> {
        self.view
            .module(id)
            .map(|module| module.symbol_prefix.as_str())
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing module metadata")
            })
    }

    fn unique_global_name(&self, base: String, symbol: SymbolId) -> String {
        if self.backend.llvm_module.get_function(&base).is_some()
            || self.backend.llvm_module.get_global(&base).is_some()
        {
            format!("{base}.global.{}", symbol.0)
        } else {
            base
        }
    }

    fn declare_storage(&mut self) -> CodeGenerationResult<()> {
        for (id, symbol) in self.view.symbols() {
            if symbol.owner.is_some()
                || symbol.name.is_empty()
                || (!symbol.has_global && !symbol.requires_initialization_check)
            {
                continue;
            }
            let prefix = self.module_prefix(symbol.module)?;
            let binding_name = if symbol.overloaded {
                format!("{}.overload.{}", symbol.name, id.0)
            } else {
                symbol.name.clone()
            };
            let base = format!("__staple_m{prefix}.{binding_name}");
            if symbol.requires_initialization_check {
                let name = self.unique_global_name(format!("{base}_state"), id);
                let state = self.backend.llvm_module.add_global(
                    self.backend.context.i8_type(),
                    None,
                    &name,
                );
                state.set_initializer(&self.backend.context.i8_type().const_zero());
                state.set_linkage(Linkage::Internal);
                self.initialization_states.insert(id, state);
            }
            if !symbol.has_global {
                continue;
            }
            let llvm_type = self.backend.compile_type(&symbol.value_type)?;
            let name = self.unique_global_name(base, id);
            let global = self.backend.llvm_module.add_global(llvm_type, None, &name);
            global.set_initializer(&llvm_type.const_zero());
            global.set_linkage(Linkage::Internal);
            self.storage.insert(id, global);
            if symbol.signal || symbol.derived {
                let suffix = if symbol.signal { "signal" } else { "derived" };
                let name = self.unique_global_name(format!("{name}_{suffix}"), id);
                let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
                let metadata = self
                    .backend
                    .llvm_module
                    .add_global(pointer_type, None, &name);
                metadata.set_initializer(&pointer_type.const_null());
                metadata.set_linkage(Linkage::Internal);
                if symbol.signal {
                    self.signal_metadata.insert(id, metadata);
                } else {
                    self.derived_metadata.insert(id, metadata);
                }
            }
        }
        Ok(())
    }

    fn declare_initializers(&mut self) -> CodeGenerationResult<()> {
        let function_type = self.backend.context.void_type().fn_type(&[], false);
        for (id, initializer) in self.view.initializers() {
            let name = &initializer.name;
            if self.backend.llvm_module.get_function(name).is_some() {
                return Err(Diagnostic::new(
                    initializer.origin.span.clone(),
                    format!("initializer name `{name}` collides with a function"),
                ));
            }
            let function =
                self.backend
                    .llvm_module
                    .add_function(name, function_type, Some(Linkage::Internal));
            self.initializers.insert(id, function);
        }
        Ok(())
    }

    /// Looks up a drop position's exact artifact use and expands its glue inline.
    /// No use means no cleanup is required. Cleanup selection is recorded by
    /// lowering, including conditional cell cleanup and nested glue.
    fn emit_drop_site(
        &self,
        owner: EmissionOwner,
        site: crate::ArtifactUseSite,
        source: DropSource<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let Some(record) = self
            .view
            .artifact_uses(owner)
            .and_then(|uses| uses.iter().find(|use_| use_.site == site))
        else {
            return Ok(());
        };
        let plan = self.drop_glue_plan(record.artifact, span)?;
        let llvm_type = self.backend.compile_type(&plan.value_type)?;
        match source {
            DropSource::Value(value) => self.emit_drop_glue(value, plan, span),
            DropSource::Place(pointer) => {
                let old = self
                    .backend
                    .builder
                    .build_load(llvm_type, pointer, "assignment.old")
                    .map_err(compiler_diagnostic)?;
                self.emit_drop_glue(old, plan, span)
            }
            DropSource::Temporary(pointer) => {
                let value = self
                    .backend
                    .builder
                    .build_load(llvm_type, pointer, "mutation.temporary.final")
                    .map_err(compiler_diagnostic)?;
                self.emit_drop_glue(value, plan, span)
            }
            DropSource::Cell(cell) => {
                let blocks =
                    self.backend
                        .begin_conditional_cell_drop(cell, llvm_type, span.clone())?;
                self.emit_drop_glue(blocks.value, plan, span)?;
                self.backend.end_conditional_cell_drop(&blocks)
            }
        }
    }

    /// Expand one `DropGlueBody` inline at its site,
    /// recursing through each nested planned glue. No function is emitted.
    fn emit_drop_glue(
        &self,
        value: BasicValueEnum<'context>,
        plan: &DropGluePlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        match &plan.body {
            DropGlueBody::Unexpanded => Err(Diagnostic::new(
                span.clone(),
                "lowered emitter: drop glue plan was never expanded",
            )),
            DropGlueBody::UserDrop {
                method,
                representation,
            } => {
                let instance = method.instance.ok_or_else(|| {
                    Diagnostic::new(span.clone(), "selected Drop method has no instance")
                })?;
                let function = self.instances.get(&instance).copied().ok_or_else(|| {
                    Diagnostic::new(span.clone(), "Drop method instance is not declared")
                })?;
                let null_environment = self
                    .backend
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null();
                let pointer = self
                    .backend
                    .builder
                    .build_alloca(value.get_type(), "drop.borrow")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(pointer, value)
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_direct_call(
                        function,
                        &[null_environment.into(), pointer.into()],
                        "drop.call",
                    )
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                if let Some(representation) = representation {
                    let plan = self.planned_drop_glue(representation, span)?;
                    self.emit_drop_glue(value, plan, span)?;
                }
                Ok(())
            }
            DropGlueBody::CoroutineCleanup => {
                let BasicValueEnum::PointerValue(frame) = value else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "coroutine value is not a frame pointer",
                    ));
                };
                self.backend
                    .build_coroutine_frame_cleanup(frame, span.clone())
            }
            DropGlueBody::RuntimeRelease(release) => {
                let BasicValueEnum::PointerValue(record) = value else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "runtime value is not a pointer",
                    ));
                };
                self.backend
                    .build_runtime_release(*release, record, span.clone())
            }
            DropGlueBody::CStringFree => {
                let BasicValueEnum::PointerValue(pointer) = value else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "CString has an invalid representation",
                    ));
                };
                self.backend.build_free_c_string(pointer, span.clone())
            }
            DropGlueBody::Product { fields } => {
                let BasicValueEnum::StructValue(product) = value else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "product has an invalid representation",
                    ));
                };
                for field in fields {
                    let element = self
                        .backend
                        .builder
                        .build_extract_value(product, field.index as u32, "drop.field")
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                    let plan = self.planned_drop_glue(&field.glue, span)?;
                    self.emit_drop_glue(element, plan, span)?;
                }
                Ok(())
            }
            DropGlueBody::Sum { alternatives } => {
                let BasicValueEnum::StructValue(sum_value) = value else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "sum has an invalid representation",
                    ));
                };
                let CheckedType::Sum(sum) = &plan.value_type else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "drop glue sum plan does not name a sum type",
                    ));
                };
                let tag = self
                    .backend
                    .builder
                    .build_extract_value(sum_value, 0, "drop.tag")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
                    .into_int_value();
                let function = self
                    .backend
                    .builder
                    .get_insert_block()
                    .and_then(|block| block.get_parent())
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "drop glue is not in a function")
                    })?;
                let merge = self
                    .backend
                    .context
                    .append_basic_block(function, "drop.sum.done");
                let mut cases = Vec::with_capacity(sum.alternatives.len());
                for index in 0..sum.alternatives.len() {
                    cases.push((
                        self.backend
                            .context
                            .i32_type()
                            .const_int(index as u64, false),
                        self.backend
                            .context
                            .append_basic_block(function, "drop.sum.case"),
                    ));
                }
                self.backend
                    .builder
                    .build_switch(tag, merge, &cases)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                for index in 0..sum.alternatives.len() {
                    self.backend.builder.position_at_end(cases[index].1);
                    if let Some(dropped) = alternatives.iter().find(|entry| entry.index == index) {
                        let payload = self.backend.extract_sum_alternative(
                            sum_value,
                            sum,
                            index,
                            span.clone(),
                        )?;
                        let plan = self.planned_drop_glue(&dropped.glue, span)?;
                        self.emit_drop_glue(payload, plan, span)?;
                    }
                    self.backend
                        .builder
                        .build_unconditional_branch(merge)
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                }
                self.backend.builder.position_at_end(merge);
                Ok(())
            }
            DropGlueBody::Distinct { representation } => {
                let plan = self.planned_drop_glue(representation, span)?;
                self.emit_drop_glue(value, plan, span)
            }
        }
    }

    /// The expanded drop-glue plan of one planned artifact ordinal.
    fn drop_glue_plan(
        &self,
        ordinal: ArtifactOrdinal,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<&'program DropGluePlan> {
        let artifact = self
            .view
            .artifact(ordinal)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing drop glue artifact"))?;
        match &artifact.plan {
            Some(LoweredArtifactPlan::DropGlue(plan)) => Ok(plan),
            _ => Err(Diagnostic::new(
                span.clone(),
                "drop site artifact is not a drop glue plan",
            )),
        }
    }

    /// The expanded drop-glue plan of one planned artifact callee.
    fn planned_drop_glue(
        &self,
        planned: &PlannedArtifact,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<&'program DropGluePlan> {
        let ordinal = planned.artifact.ok_or_else(|| {
            Diagnostic::new(span.clone(), "nested drop glue has no artifact ordinal")
        })?;
        self.drop_glue_plan(ordinal, span)
    }

    /// One instance body, or `Ok(())` when the instance declares nothing
    /// emittable (no materialized body, no declaration, or no root block).
    fn emit_instance_body(&mut self, id: FunctionInstanceId) -> CodeGenerationResult<()> {
        let Some(body) = self
            .view
            .instance(id)
            .and_then(|record| record.body.as_ref())
        else {
            return Ok(());
        };
        let Some(function) = self.instances.get(&id).copied() else {
            return Ok(());
        };
        let Some(root) = body.root else {
            return Ok(());
        };
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let mut environment = FunctionEnvironment::default();
        self.bind_parameters(id, function, &mut environment)?;
        let owner = EmissionOwner::Instance(id);
        let value = self.emit_instance_root(owner, body, root, &mut environment)?;
        if !environment.returned {
            let result = value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    body.origin.span.clone(),
                    "function result is not a first-class value",
                )
            })?;
            // after the body expression's own
            // scope drops, drop every remaining owned binding (the
            // parameters) before returning.
            self.drop_all_owned(&environment, &body.origin.span)?;
            self.backend
                .builder
                .build_return(Some(&result))
                .map_err(compiler_diagnostic)?;
        }
        Ok(())
    }

    fn emit_instance_root(
        &mut self,
        owner: EmissionOwner,
        body: &crate::LoweredInstanceBody,
        root: BlockId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let mut value = self.emit_block(owner, root, environment)?;
        // Apply the body expression's header after its block. Moved symbols
        // release on every path, including divergent paths.
        if !environment.returned
            && let Some(coercion) = &body.body_coercion
        {
            let Some(plan) = &body.body_coercion_plan else {
                return Err(Diagnostic::new(
                    body.origin.span.clone(),
                    "lowered emitter: coercion",
                ));
            };
            value = self.emit_coercion(
                value,
                &coercion.source,
                &coercion.target,
                plan,
                &body.origin.span,
            )?;
        }
        for symbol in &body.body_moved_symbols {
            if let Some(record) = environment.owned.get(symbol)
                && let Some(live) = record.live
            {
                self.backend
                    .builder
                    .build_store(live, self.backend.context.bool_type().const_zero())
                    .map_err(compiler_diagnostic)?;
            }
            self.store_local_initialization_state(
                owner,
                environment,
                *symbol,
                0,
                &body.origin.span,
            )?;
        }
        Ok(value)
    }

    /// Strict: attempt every instance body, collecting one diagnostic per
    /// failure.
    fn emit_instance_bodies(&mut self, diagnostics: &mut Vec<Diagnostic>) {
        for (id, _) in self.view.instances() {
            if let Err(diagnostic) = self.emit_instance_body(id) {
                diagnostics.push(diagnostic);
            }
        }
    }

    fn bind_parameters(
        &mut self,
        instance: FunctionInstanceId,
        function: FunctionValue<'context>,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let body = self
            .view
            .instance(instance)
            .and_then(|record| record.body.as_ref())
            .ok_or_else(|| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "internal invariant violated: emitted instance has a body",
                )
            })?;
        let parameters = function.get_params();
        let environment_pointer = parameters
            .first()
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(body.origin.span.clone(), "missing closure environment")
            })?
            .into_pointer_value();
        environment.closure_environment = Some(environment_pointer);

        // bind the concrete effect-row resources from the body's
        // `function_providers`, in row order, keeping each provider's
        // indirect/borrowed fact. Emission resolves `LoweredResourceUse` reads and
        // call `resource_bindings` against these entries. The emitter binds the
        // same list from the checked effect row (bind function parameters).
        let resource_count = body.signature.effects.resources.len();
        if body.function_providers.len() != resource_count {
            return Err(Diagnostic::new(
                body.origin.span.clone(),
                "lowered emitter: function resource providers disagree with the effect row",
            ));
        }
        for (position, provider_id) in body.function_providers.iter().enumerate() {
            let provider = self
                .view
                .resource_provider(EmissionOwner::Instance(instance), *provider_id)
                .ok_or_else(|| {
                    Diagnostic::new(
                        body.origin.span.clone(),
                        "missing function resource provider",
                    )
                })?;
            let value = parameters.get(1 + position).ok_or_else(|| {
                Diagnostic::new(
                    body.origin.span.clone(),
                    "missing function resource parameter",
                )
            })?;
            environment.resources.insert(
                *provider_id,
                BoundResource {
                    resource: provider.resource.clone(),
                    value: value.as_any_value_enum(),
                    indirect: provider.indirect,
                },
            );
        }

        // Bind captures in the layout order shared with the planned
        // closure-environment finalizer.
        self.bind_instance_captures(body, environment_pointer, environment)?;
        let raw = parameters.get(1 + resource_count..).ok_or_else(|| {
            Diagnostic::new(body.origin.span.clone(), "missing function resources")
        })?;
        let logical_types = flattened_parameter_types(&body.signature.parameter);
        let indirect_mask = self.backend.indirect_parameter_mask(&body.signature);
        let whole = body.signature.mutations.contains(&CheckedMutation::Whole);
        // load every indirect parameter
        // through its pointer (or the single whole-mutation pointer) and keep
        // every one as a mutable pointer for compile place pointer.
        let mut values: Vec<BasicValueEnum<'context>> = Vec::new();
        let mut mutable_pointers: Vec<(usize, PointerValue<'context>)> = Vec::new();
        if whole {
            let pointer = raw
                .first()
                .ok_or_else(|| {
                    Diagnostic::new(body.origin.span.clone(), "missing function parameter")
                })?
                .into_pointer_value();
            let llvm_type = self.backend.compile_type(&body.signature.parameter)?;
            values.push(
                self.backend
                    .builder
                    .build_load(llvm_type, pointer, "parameter.value")
                    .map_err(compiler_diagnostic)?,
            );
            mutable_pointers.push((0, pointer));
        } else {
            for (index, parameter) in raw.iter().copied().enumerate() {
                if indirect_mask.get(index).copied().unwrap_or(false) {
                    let pointer = parameter.into_pointer_value();
                    let llvm_type = self.backend.compile_type(logical_types[index])?;
                    values.push(
                        self.backend
                            .builder
                            .build_load(llvm_type, pointer, "parameter.value")
                            .map_err(compiler_diagnostic)?,
                    );
                    mutable_pointers.push((index, pointer));
                } else {
                    values.push(parameter);
                }
            }
        }
        self.bind_mutable_parameter_pointers(
            EmissionOwner::Instance(instance),
            body.parameter_pattern,
            &body.signature.parameter,
            whole,
            &mutable_pointers,
            environment,
        )?;
        self.bind_top_level_pattern(
            EmissionOwner::Instance(instance),
            body.parameter_pattern,
            &values,
            environment,
        )
    }

    /// Every indirect parameter
    /// pointer replaces any capture cell for its top-level symbol, with a
    /// whole mutation projecting each product field.
    fn bind_mutable_parameter_pointers(
        &self,
        owner: EmissionOwner,
        pattern: PatternId,
        parameter_type: &CheckedType,
        whole: bool,
        pointers: &[(usize, PointerValue<'context>)],
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let symbols = self.top_level_pattern_symbols(owner, pattern);
        if whole {
            let Some((_, pointer)) = pointers.first().copied() else {
                return Ok(());
            };
            if symbols.len() == 1 {
                if let Some(Some(symbol)) = symbols.first() {
                    environment.binding_cells.remove(symbol);
                    environment.parameter_pointers.insert(*symbol, pointer);
                }
                return Ok(());
            }
            let CheckedType::Product(_) = parameter_type else {
                return Ok(());
            };
            let llvm_type = self
                .backend
                .compile_type(parameter_type)?
                .into_struct_type();
            for (index, symbol) in symbols.into_iter().enumerate() {
                if let Some(symbol) = symbol {
                    let field = self
                        .backend
                        .builder
                        .build_struct_gep(llvm_type, pointer, index as u32, "parameter.field")
                        .map_err(compiler_diagnostic)?;
                    environment.binding_cells.remove(&symbol);
                    environment.parameter_pointers.insert(symbol, field);
                }
            }
            return Ok(());
        }
        for (index, pointer) in pointers {
            if let Some(Some(symbol)) = symbols.get(*index) {
                environment.binding_cells.remove(symbol);
                environment.parameter_pointers.insert(*symbol, *pointer);
            }
        }
        Ok(())
    }

    /// A top-level non-product parameter
    /// pattern binds the flattened values rebuilt into one product value, a
    /// one-element product collapses, and a product element binds its own
    /// flattened slot directly.
    fn bind_top_level_pattern(
        &mut self,
        owner: EmissionOwner,
        pattern: PatternId,
        values: &[BasicValueEnum<'context>],
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let record = self
            .view
            .pattern(owner, pattern)
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered pattern")
            })?
            .clone();
        let span = record.origin.span.clone();
        match &record.kind {
            crate::LoweredPatternKind::Binding { .. }
            | crate::LoweredPatternKind::At { .. }
            | crate::LoweredPatternKind::Wildcard
            | crate::LoweredPatternKind::Nominal { .. } => {
                let value = self
                    .backend
                    .build_product_value(values, span)?
                    .as_any_value_enum();
                self.bind_pattern(owner, pattern, value, environment)
            }
            crate::LoweredPatternKind::Product { elements, .. } if elements.len() == 1 => {
                self.bind_top_level_pattern(owner, elements[0], values, environment)
            }
            crate::LoweredPatternKind::Product { elements, .. }
                if elements.len() == values.len() =>
            {
                for (element, value) in elements.iter().zip(values.iter().copied()) {
                    self.bind_pattern(owner, *element, value.as_any_value_enum(), environment)?;
                }
                Ok(())
            }
            // A whole `mut`/`move`-marked product pattern (`mut (a, b) => ...`)
            // is passed as one value and destructured from it.
            crate::LoweredPatternKind::Product { .. } if values.len() == 1 => {
                self.bind_pattern(owner, pattern, values[0].as_any_value_enum(), environment)
            }
            _ => Err(Diagnostic::new(
                span,
                "function pattern layout does not match its declared type",
            )),
        }
    }

    /// The top-level bound symbols of a parameter pattern, one entry per
    /// flattened parameter slot (`None` for a wildcard, singleton, or
    /// non-binding pattern).
    fn top_level_pattern_symbols(
        &self,
        owner: EmissionOwner,
        pattern: PatternId,
    ) -> Vec<Option<SymbolId>> {
        let Some(record) = self.view.pattern(owner, pattern) else {
            return vec![None];
        };
        match &record.kind {
            crate::LoweredPatternKind::Product { elements, .. } => elements
                .iter()
                .map(|element| self.top_level_pattern_symbol(owner, *element))
                .collect(),
            _ => vec![self.top_level_pattern_symbol(owner, pattern)],
        }
    }

    fn top_level_pattern_symbol(
        &self,
        owner: EmissionOwner,
        pattern: PatternId,
    ) -> Option<SymbolId> {
        match &self.view.pattern(owner, pattern)?.kind {
            crate::LoweredPatternKind::Binding { symbol, .. } => *symbol,
            crate::LoweredPatternKind::At { binding, .. } => {
                self.top_level_pattern_symbol(owner, *binding)
            }
            _ => None,
        }
    }

    fn bind_instance_captures(
        &self,
        body: &crate::LoweredInstanceBody,
        environment_pointer: PointerValue<'context>,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        if !body.captures.is_empty() {
            let fields = body
                .captures
                .iter()
                .map(|capture| self.capture_field_type(capture))
                .collect::<CodeGenerationResult<Vec<_>>>()?;
            let capture_type = self.backend.context.struct_type(&fields, false);
            let captures = self
                .backend
                .builder
                .build_load(capture_type, environment_pointer, "closure.environment")
                .map_err(compiler_diagnostic)?
                .into_struct_value();
            for (index, capture) in body.captures.iter().enumerate() {
                let value = self
                    .backend
                    .builder
                    .build_extract_value(captures, index as u32, "capture")
                    .map_err(compiler_diagnostic)?;
                let symbol = capture.capture.symbol;
                if self.capture_stores_pointer(capture) {
                    let pointer = value.into_pointer_value();
                    if self.capture_is_parameter_pointer(capture) {
                        environment.parameter_pointers.insert(symbol, pointer);
                    } else {
                        environment.binding_cells.insert(symbol, pointer);
                    }
                } else {
                    environment.locals.insert(symbol, value.as_any_value_enum());
                }
            }
        }
        Ok(())
    }

    /// Whether a capture stores a pointer: cells, initialization-state slots,
    /// derived cells, and borrowed parameter storage retain their pointer. Other
    /// captures store the value directly.
    fn capture_stores_pointer(&self, capture: &LoweredInstanceCapture) -> bool {
        capture.requires_initialization_state
            || capture.mutable_storage
            || capture.derived
            || capture.capture.borrowed
    }

    /// Whether a pointer-kind capture's pointer is borrowed parameter storage
    /// (a mutated or borrowed parameter) rather than a binding cell. Mirrors
    /// bind environment captures.
    fn capture_is_parameter_pointer(&self, capture: &LoweredInstanceCapture) -> bool {
        capture.capture.borrowed
            || self
                .view
                .symbol(capture.capture.symbol)
                .is_some_and(|symbol| symbol.mutated_parameter)
    }

    /// The LLVM field type of one capture, `capture_stores_pointer`'s choice.
    fn capture_field_type(
        &self,
        capture: &LoweredInstanceCapture,
    ) -> CodeGenerationResult<inkwell::types::BasicTypeEnum<'context>> {
        if self.capture_stores_pointer(capture) {
            Ok(self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .into())
        } else {
            self.backend.compile_type(&capture.value_type)
        }
    }

    /// Write a symbol's
    /// initialization state when it has one, and do nothing otherwise.
    fn store_initialization_state(&self, symbol: SymbolId, state: u64) -> CodeGenerationResult<()> {
        if let Some(slot) = self.initialization_states.get(&symbol) {
            self.backend
                .builder
                .build_store(
                    slot.as_pointer_value(),
                    self.backend.context.i8_type().const_int(state, false),
                )
                .map_err(compiler_diagnostic)?;
        }
        Ok(())
    }

    /// The binding cell layout: value and initialization state, followed by a
    /// metadata pointer for signal or derived storage.
    fn binding_cell_type(
        &self,
        owner: EmissionOwner,
        symbol: SymbolId,
    ) -> CodeGenerationResult<inkwell::types::StructType<'context>> {
        // A generic body's local symbol keeps its template type in the symbol
        // catalog; the owner's concrete binding/parameter/capture record is
        // authoritative.
        let value_type = self
            .view
            .owner_symbol_type(owner, symbol)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    format!(
                        "binding cell symbol {} has no concrete type in its owner",
                        symbol.0
                    ),
                )
            })?;
        let value_type = self.backend.compile_type(&value_type)?;
        let mut fields = vec![value_type, self.backend.context.i8_type().into()];
        // a signal or derived cell carries
        // a metadata pointer in field 2.
        if self
            .view
            .symbol(symbol)
            .is_some_and(|record| record.signal || record.derived)
        {
            fields.push(
                self.backend
                    .context
                    .ptr_type(AddressSpace::default())
                    .into(),
            );
        }
        Ok(self.backend.context.struct_type(&fields, false))
    }

    /// Allocates a binding cell on the GC heap when captured, otherwise on the
    /// stack. Initializes its state to zero and creates signal metadata when
    /// required. Recorded finalizer uses supply captured-value cleanup.
    fn allocate_binding_cell(
        &mut self,
        owner: EmissionOwner,
        environment: &mut FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            return Ok(cell);
        }
        let cell_type = self.binding_cell_type(owner, symbol)?;
        // a cell is GC-allocated exactly when
        // some function captures it.
        let captured = self
            .view
            .symbol(symbol)
            .is_some_and(|record| record.captured && (record.mutable_storage || record.derived));
        let cell = if captured {
            self.backend.build_gc_allocation(
                self.backend
                    .size_type
                    .const_int(self.backend.target_data.get_store_size(&cell_type), false),
                "binding.cell",
                span.clone(),
            )?
        } else {
            self.backend
                .builder
                .build_alloca(cell_type, "binding.cell")
                .map_err(compiler_diagnostic)?
        };
        let state = self
            .backend
            .builder
            .build_struct_gep(cell_type, cell, 1, "binding.state")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.backend
            .builder
            .build_store(state, self.backend.context.i8_type().const_zero())
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        // a signal cell creates its signal
        // before the value is evaluated and stores it in the metadata field.
        if self.view.symbol(symbol).is_some_and(|record| record.signal) {
            let metadata_slot = self
                .backend
                .builder
                .build_struct_gep(cell_type, cell, 2, "signal.metadata")
                .map_err(compiler_diagnostic)?;
            let signal = self.emit_signal_create(&staple_syntax::Span::Compiler)?;
            self.backend
                .builder
                .build_store(metadata_slot, signal)
                .map_err(compiler_diagnostic)?;
        }
        if captured {
            self.install_cell_finalizer(owner, symbol, cell, span)?;
        } else {
            self.register_owned_binding(owner, environment, symbol, span)?;
        }
        environment.binding_cells.insert(symbol, cell);
        Ok(cell)
    }

    /// Installs the finalizer named by a captured cell's recorded artifact use.
    /// No use means the cell requires no finalizer.
    fn install_cell_finalizer(
        &self,
        owner: EmissionOwner,
        symbol: SymbolId,
        cell: PointerValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let Some(record) = self.view.artifact_uses(owner).and_then(|uses| {
            uses.iter()
                .find(|use_| use_.site == crate::ArtifactUseSite::CellFinalizer(symbol))
        }) else {
            return Ok(());
        };
        let finalizer = self
            .artifacts
            .get(&record.artifact)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing cell finalizer declaration"))?;
        self.backend.set_gc_finalizer(cell, finalizer)
    }

    /// Write a cell-backed symbol's
    /// state byte, and do nothing when the symbol has no cell.
    fn store_local_initialization_state(
        &self,
        owner: EmissionOwner,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        state: u64,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let Some(cell) = environment.binding_cells.get(&symbol).copied() else {
            return Ok(());
        };
        let state_slot = if self.view.initialization_state_only(owner, symbol) {
            cell
        } else {
            let cell_type = self.binding_cell_type(owner, symbol)?;
            self.backend
                .builder
                .build_struct_gep(cell_type, cell, 1, "binding.state")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
        };
        self.backend
            .builder
            .build_store(
                state_slot,
                self.backend.context.i8_type().const_int(state, false),
            )
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// Registers an owned binding from the owner's recorded cleanup facts.
    /// Values get a fresh live flag set to true; cells use their initialization state.
    fn register_owned_binding(
        &self,
        owner: EmissionOwner,
        environment: &mut FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let Some(record) = self
            .view
            .owned_bindings(owner)
            .and_then(|bindings| bindings.iter().find(|record| record.symbol == symbol))
        else {
            return Ok(());
        };
        let Some(glue) = record.glue else {
            return Err(Diagnostic::new(
                span.clone(),
                "owned binding has no bound drop glue",
            ));
        };
        let newly_owned = !environment.owned_order.contains(&symbol);
        match record.storage {
            OwnedStorage::Value => {
                let Some(value) = environment.locals.get(&symbol).copied() else {
                    return Ok(());
                };
                let live = self
                    .backend
                    .builder
                    .build_alloca(self.backend.context.bool_type(), "drop.live")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(live, self.backend.context.bool_type().const_int(1, false))
                    .map_err(compiler_diagnostic)?;
                environment.owned.insert(
                    symbol,
                    OwnedValue {
                        storage: OwnedStorage::Value,
                        glue,
                        value: Some(value),
                        live: Some(live),
                    },
                );
            }
            OwnedStorage::Cell => {
                environment.owned.insert(
                    symbol,
                    OwnedValue {
                        storage: OwnedStorage::Cell,
                        glue,
                        value: None,
                        live: None,
                    },
                );
            }
        }
        if newly_owned {
            environment.owned_order.push(symbol);
        }
        Ok(())
    }

    /// Emit the conditional drop of every owned
    /// binding registered since `start`, in reverse registration order, and
    /// remove them from the environment.
    fn drop_owned_since(
        &self,
        environment: &mut FunctionEnvironment<'context>,
        start: usize,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let symbols = environment.owned_order[start..].to_vec();
        for symbol in symbols.into_iter().rev() {
            if let Some(record) = environment.owned.remove(&symbol) {
                self.emit_owned_binding_drop(environment, symbol, &record, span)?;
            }
        }
        environment.owned_order.truncate(start);
        Ok(())
    }

    /// Conditionally drops every owned binding in reverse registration order.
    /// Registrations remain available for other emitted control-flow paths.
    fn drop_all_owned(
        &self,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        for symbol in environment.owned_order.iter().rev() {
            if let Some(record) = environment.owned.get(symbol) {
                self.emit_owned_binding_drop(environment, *symbol, record, span)?;
            }
        }
        Ok(())
    }

    /// The emitter's compile-time-only cleanup on a diverged branch: forget the
    /// owned bindings registered since `start` without emitting a drop.
    fn forget_owned_since(environment: &mut FunctionEnvironment<'context>, start: usize) {
        let cleanup_start = start.min(environment.owned_order.len());
        for symbol in &environment.owned_order[cleanup_start..] {
            environment.owned.remove(symbol);
        }
        environment.owned_order.truncate(cleanup_start);
    }

    /// One owned binding's conditional drop: the live-flag
    /// skeleton around a value, or the cell-state skeleton around a cell.
    fn emit_owned_binding_drop(
        &self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        record: &OwnedValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let plan = self.drop_glue_plan(record.glue, span)?;
        match record.storage {
            OwnedStorage::Value => {
                let value = record.value.and_then(value_as_basic).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "owned value is not first-class")
                })?;
                let live = record
                    .live
                    .ok_or_else(|| Diagnostic::new(span.clone(), "owned value has no live flag"))?;
                let (_drop_block, done_block) =
                    self.backend.begin_conditional_drop(live, span.clone())?;
                self.emit_drop_glue(value, plan, span)?;
                self.backend.end_conditional_drop(done_block)
            }
            OwnedStorage::Cell => {
                let cell = environment
                    .binding_cells
                    .get(&symbol)
                    .copied()
                    .ok_or_else(|| Diagnostic::new(span.clone(), "owned cell is not available"))?;
                let llvm_type = self.backend.compile_type(&plan.value_type)?;
                let blocks =
                    self.backend
                        .begin_conditional_cell_drop(cell, llvm_type, span.clone())?;
                self.emit_drop_glue(blocks.value, plan, span)?;
                self.backend.end_conditional_cell_drop(&blocks)
            }
        }
    }

    /// One module initializer body: entry IO/reactive resources, the lowered
    /// body, then reactive-scope disposal and return.
    fn emit_initializer_body(&mut self, id: InitializerId) -> CodeGenerationResult<()> {
        let function = self.initializers[&id];
        let initializer = self.view.initializer(id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered initializer")
        })?;
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let mut environment = FunctionEnvironment::default();
        let providers = self.view.initializer_entry_providers(id).ok_or_else(|| {
            Diagnostic::new(
                initializer.origin.span.clone(),
                "lowered emitter: entry resource providers disagree with the initializer resources",
            )
        })?;
        for (resource, provider_id) in initializer.resources.iter().zip(providers) {
            let provider = self
                .view
                .resource_provider(EmissionOwner::Initializer(id), provider_id)
                .ok_or_else(|| {
                    Diagnostic::new(initializer.origin.span.clone(), "missing entry provider")
                })?;
            if provider.resource != resource.resource {
                return Err(Diagnostic::new(
                    initializer.origin.span.clone(),
                    "lowered emitter: entry provider disagrees with its entry resource",
                ));
            }
            let indirect = provider.indirect;
            match resource.kind {
                LoweredEntryResourceKind::Io => {
                    let ty = self.backend.compile_type(&resource.resource.value_type)?;
                    let slot = self
                        .backend
                        .builder
                        .build_alloca(ty, "io.resource")
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_store(slot, ty.const_zero())
                        .map_err(compiler_diagnostic)?;
                    environment.resources.insert(
                        provider_id,
                        BoundResource {
                            resource: resource.resource.clone(),
                            value: slot.as_any_value_enum(),
                            indirect,
                        },
                    );
                }
                LoweredEntryResourceKind::Reactive => {
                    let scope = self
                        .backend
                        .build_reactive_runtime_call(
                            "__staple_reactive_scope_create",
                            &[],
                            Some(
                                self.backend
                                    .context
                                    .ptr_type(AddressSpace::default())
                                    .into(),
                            ),
                            "reactive.scope",
                            initializer.origin.span.clone(),
                        )?
                        .ok_or_else(|| {
                            Diagnostic::new(
                                initializer.origin.span.clone(),
                                "reactive scope creation returned no value",
                            )
                        })?
                        .into_pointer_value();
                    environment.resources.insert(
                        provider_id,
                        BoundResource {
                            resource: resource.resource.clone(),
                            value: scope.as_any_value_enum(),
                            indirect,
                        },
                    );
                    environment.reactive_scopes.push(scope);
                }
            }
        }
        self.emit_block(
            EmissionOwner::Initializer(id),
            initializer.body,
            &mut environment,
        )?;
        if !environment.returned {
            for scope in environment.reactive_scopes.iter().rev() {
                self.backend.build_reactive_runtime_call(
                    "__staple_reactive_scope_dispose",
                    &[(*scope).into()],
                    None,
                    "reactive.dispose",
                    initializer.origin.span.clone(),
                )?;
            }
            self.backend
                .builder
                .build_return(None)
                .map_err(compiler_diagnostic)?;
        }
        Ok(())
    }

    /// Strict: attempt every initializer body, collecting one diagnostic per
    /// failure.
    fn emit_initializers(&mut self, diagnostics: &mut Vec<Diagnostic>) {
        for (id, _) in self.view.initializers() {
            if let Err(diagnostic) = self.emit_initializer_body(id) {
                diagnostics.push(diagnostic);
            }
        }
    }

    /// One artifact family that is a call shim. A
    /// constructor adapter rebuilds its product (or GC-allocates the managed
    /// reference and sets the planned payload finalizer); an extern adapter
    /// forwards the closure parameters to the foreign symbol. Every other
    /// family has no body to emit here.
    fn emit_artifact_body(&mut self, ordinal: ArtifactOrdinal) -> CodeGenerationResult<()> {
        let Some(artifact) = self.view.artifact(ordinal) else {
            return Ok(());
        };
        let Some(plan) = artifact.plan.as_ref() else {
            return Ok(());
        };
        match plan {
            LoweredArtifactPlan::ConstructorAdapter(plan) => {
                self.emit_constructor_adapter_body(ordinal, plan, &artifact.origin.span)
            }
            LoweredArtifactPlan::ExternAdapter(plan) => {
                self.emit_extern_adapter_body(ordinal, plan, &artifact.origin.span)
            }
            LoweredArtifactPlan::StructuralMethod(plan) => {
                self.emit_structural_body(ordinal, plan, &artifact.origin.span)
            }
            LoweredArtifactPlan::GcFinalizer(plan) => {
                self.emit_gc_finalizer_body(ordinal, plan, &artifact.origin.span)
            }
            // Drop glue expands inline at recorded use sites.
            LoweredArtifactPlan::DropGlue(_) => Ok(()),
            LoweredArtifactPlan::CoroutineCodes(plan) => {
                self.emit_coroutine_pair(ordinal, plan, &artifact.origin.span)
            }
            LoweredArtifactPlan::ReactionRunner(plan)
            | LoweredArtifactPlan::UntilRunner(plan)
            | LoweredArtifactPlan::DerivedRunner(plan) => {
                self.emit_runner_body(ordinal, plan, &artifact.origin.span)
            }
        }
    }

    /// Emits the recorded payload, cell, closure-environment, or buffer finalizer.
    fn emit_gc_finalizer_body(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &GcFinalizerPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let function = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing finalizer declaration"))?;
        let body = self
            .backend
            .enter_finalizer_function(function, span.clone())?;
        match plan {
            GcFinalizerPlan::Payload { value_type, glue } => {
                let drop_plan = self.finalizer_glue(glue, span)?;
                let payload_type = self.backend.compile_type(value_type)?;
                let value = self
                    .backend
                    .builder
                    .build_load(payload_type, body.payload, "finalizer.value")
                    .map_err(compiler_diagnostic)?;
                self.emit_drop_glue(value, drop_plan, span)?;
            }
            GcFinalizerPlan::Cell { value_type, glue } => {
                let drop_plan = self.finalizer_glue(glue, span)?;
                let llvm_type = self.backend.compile_type(value_type)?;
                let blocks = self.backend.begin_conditional_cell_drop(
                    body.payload,
                    llvm_type,
                    span.clone(),
                )?;
                self.emit_drop_glue(blocks.value, drop_plan, span)?;
                self.backend.end_conditional_cell_drop(&blocks)?;
            }
            GcFinalizerPlan::ClosureEnvironment { closure, drops, .. } => {
                let drops = drops.as_ref().ok_or_else(|| {
                    Diagnostic::new(span.clone(), "closure finalizer plan was never expanded")
                })?;
                // The environment layout is the closure instance's own capture
                // layout recorded by the capture plan.
                let capture_body = self
                    .view
                    .instance(*closure)
                    .and_then(|record| record.body.as_ref())
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "closure finalizer has no capture layout")
                    })?;
                let fields = capture_body
                    .captures
                    .iter()
                    .map(|capture| self.capture_field_type(capture))
                    .collect::<CodeGenerationResult<Vec<_>>>()?;
                let environment_type = self.backend.capture_environment_type(&fields);
                let environment = self
                    .backend
                    .builder
                    .build_load(
                        environment_type,
                        body.payload,
                        "closure.finalizer.environment",
                    )
                    .map_err(compiler_diagnostic)?
                    .into_struct_value();
                for capture in drops {
                    let value = self
                        .backend
                        .builder
                        .build_extract_value(
                            environment,
                            capture.index as u32,
                            "closure.finalizer.capture",
                        )
                        .map_err(compiler_diagnostic)?;
                    let drop_plan = self.planned_drop_glue(&capture.glue, span)?;
                    self.emit_drop_glue(value, drop_plan, span)?;
                }
            }
            GcFinalizerPlan::Buffer { element, glue } => {
                let drop_plan = self.finalizer_glue(glue, span)?;
                let llvm_element = self.backend.compile_type(element)?;
                let header = self.backend.buffer_header_type(llvm_element);
                let length = self.backend.build_buffer_length(
                    body.payload,
                    header,
                    "buffer.length.slot",
                    "buffer.length",
                )?;
                let index_slot = self
                    .backend
                    .builder
                    .build_alloca(self.backend.size_type, "buffer.finalize.index")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(index_slot, self.backend.size_type.const_zero())
                    .map_err(compiler_diagnostic)?;
                let check = self
                    .backend
                    .context
                    .append_basic_block(function, "buffer.finalize.check");
                let element_block = self
                    .backend
                    .context
                    .append_basic_block(function, "buffer.finalize.element");
                let done = self
                    .backend
                    .context
                    .append_basic_block(function, "buffer.finalize.done");
                self.backend
                    .builder
                    .build_unconditional_branch(check)
                    .map_err(compiler_diagnostic)?;
                self.backend.builder.position_at_end(check);
                let index = self
                    .backend
                    .builder
                    .build_load(self.backend.size_type, index_slot, "buffer.finalize.index")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let remaining = self
                    .backend
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::ULT,
                        index,
                        length,
                        "buffer.finalize.remaining",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_conditional_branch(remaining, element_block, done)
                    .map_err(compiler_diagnostic)?;
                self.backend.builder.position_at_end(element_block);
                let data = self
                    .backend
                    .buffer_data_pointer(body.payload, llvm_element)?;
                let (_, value) = self.backend.build_buffer_element_load(
                    data,
                    llvm_element,
                    index,
                    "buffer.finalize.slot",
                    "buffer.finalize.value",
                )?;
                self.emit_drop_glue(value, drop_plan, span)?;
                let next = self
                    .backend
                    .builder
                    .build_int_add(
                        index,
                        self.backend.size_type.const_int(1, false),
                        "buffer.finalize.next",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(index_slot, next)
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_unconditional_branch(check)
                    .map_err(compiler_diagnostic)?;
                self.backend.builder.position_at_end(done);
            }
        }
        self.backend.finish_finalizer_function(&body)
    }

    /// The required expanded drop glue of a `Payload`/`Cell`/`Buffer`
    /// finalizer plan.
    fn finalizer_glue(
        &self,
        glue: &Option<PlannedArtifact>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<&'program DropGluePlan> {
        let glue = glue.as_ref().ok_or_else(|| {
            Diagnostic::new(span.clone(), "finalizer glue plan was never expanded")
        })?;
        self.planned_drop_glue(glue, span)
    }

    /// Strict: attempt every artifact body, collecting one diagnostic per
    /// failure. A family without a body emitter reports its family instead of
    /// leaving an undefined declaration behind (an unlinked artifact would
    /// otherwise make a strict compile look runnable).
    fn emit_artifact_bodies(&mut self, diagnostics: &mut Vec<Diagnostic>) {
        for (_, artifact) in self.view.artifacts() {
            if artifact.plan.is_none() {
                continue;
            }
            if let Err(diagnostic) = self.emit_artifact_body(artifact.ordinal) {
                diagnostics.push(diagnostic);
            }
        }
    }

    /// Ensure constructor adapter's body: rebuild the product from
    /// the closure parameters (after the environment) and return it, or
    /// GC-allocate it as a `Ref` payload with the planned finalizer.
    fn emit_constructor_adapter_body(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &crate::ConstructorAdapterPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let function = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing constructor adapter declaration")
            })?;
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let parameters = function.get_params();
        let value = self
            .backend
            .build_product_value(&parameters[1..], span.clone())?;
        let result = match &plan.construction {
            crate::ConstructorConstruction::Value { .. } => value,
            crate::ConstructorConstruction::ManagedRef {
                payload, finalizer, ..
            } => {
                let payload_type = self.backend.compile_type(payload)?;
                let size = self.backend.target_data.get_store_size(&payload_type);
                let pointer = self.backend.build_gc_allocation(
                    self.backend.size_type.const_int(size, false),
                    "ref.allocate",
                    span.clone(),
                )?;
                self.backend
                    .builder
                    .build_store(pointer, value)
                    .map_err(compiler_diagnostic)?;
                if let Some(planned) = finalizer
                    && let Some(finalizer_ordinal) = planned.artifact
                {
                    let finalizer = self
                        .artifacts
                        .get(&finalizer_ordinal)
                        .and_then(|functions| functions.first())
                        .copied()
                        .ok_or_else(|| {
                            Diagnostic::new(span.clone(), "missing payload finalizer declaration")
                        })?;
                    self.backend.set_gc_finalizer(pointer, finalizer)?;
                }
                pointer.into()
            }
            crate::ConstructorConstruction::Unexpanded => {
                return Err(Diagnostic::new(
                    span.clone(),
                    "constructor adapter plan was never expanded",
                ));
            }
        };
        self.backend
            .builder
            .build_return(Some(&result))
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// Declare external functions's adapter body: forward the closure
    /// parameters (after the environment) to the foreign symbol. emission
    /// gives the adapter the closure ABI's parameter shapes, so every
    /// by-pointer parameter is loaded before the native call.
    fn emit_extern_adapter_body(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &crate::ExternAdapterPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let function = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing extern adapter declaration"))?;
        let foreign =
            self.externs.get(&plan.symbol).copied().ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing foreign symbol declaration")
            })?;
        if plan
            .callable_type
            .mutations
            .contains(&CheckedMutation::Whole)
        {
            return Err(Diagnostic::new(
                span.clone(),
                "extern adapter cannot forward a whole-mutation parameter",
            ));
        }
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let arguments = self.adapter_foreign_arguments(plan, function, span)?;
        let call = self
            .backend
            .builder
            .build_direct_call(foreign, &arguments, "extern.call")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let result = call.try_as_basic_value().basic().ok_or_else(|| {
            Diagnostic::new(span.clone(), "extern adapter result is not first-class")
        })?;
        self.backend
            .builder
            .build_return(Some(&result))
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// The adapter's foreign-call arguments: the closure parameters loaded
    /// where the recorded pass mode says the caller passed a pointer.
    fn adapter_foreign_arguments(
        &self,
        plan: &crate::ExternAdapterPlan,
        function: FunctionValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Vec<BasicMetadataValueEnum<'context>>> {
        let parameter_types = flattened_parameter_types(&plan.callable_type.parameter);
        let parameters = function.get_params();
        let parameters = parameters.get(1..).unwrap_or_default();
        if parameters.len() != parameter_types.len()
            || plan.indirect_parameters.len() != parameter_types.len()
        {
            return Err(Diagnostic::new(
                span.clone(),
                "extern adapter parameter layout does not match its recorded ABI",
            ));
        }
        let mut arguments = Vec::with_capacity(parameters.len());
        for (index, parameter) in parameters.iter().enumerate() {
            if plan.indirect_parameters[index] {
                let loaded = self
                    .backend
                    .builder
                    .build_load(
                        self.backend.compile_type(parameter_types[index])?,
                        parameter.clone().into_pointer_value(),
                        "extern.argument",
                    )
                    .map_err(compiler_diagnostic)?;
                arguments.push(loaded.into());
            } else {
                arguments.push(parameter.clone().into());
            }
        }
        Ok(arguments)
    }

    fn emit_main(&mut self) -> CodeGenerationResult<()> {
        let integer = self.backend.context.i32_type();
        let function =
            self.backend
                .llvm_module
                .add_function("main", integer.fn_type(&[], false), None);
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let stack_bottom = self
            .backend
            .builder
            .build_alloca(self.backend.context.i8_type(), "gc.stack.bottom")
            .map_err(compiler_diagnostic)?;
        let set_stack_bottom = self
            .backend
            .llvm_module
            .get_function("__staple_gc_set_stack_bottom")
            .ok_or_else(|| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "internal invariant violated: GC stack initializer runtime is linked before entry emission",
                )
            })?;
        self.backend
            .builder
            .build_direct_call(set_stack_bottom, &[stack_bottom.into()], "")
            .map_err(compiler_diagnostic)?;
        for (id, symbol) in self.view.symbols() {
            if !symbol.global_root {
                continue;
            }
            let global = self.storage.get(&id).ok_or_else(|| {
                Diagnostic::new(symbol.origin.span.clone(), "missing GC root global")
            })?;
            let llvm_type = self.backend.compile_type(&symbol.value_type)?;
            self.backend.register_gc_root_region(
                global.as_pointer_value(),
                self.backend.target_data.get_store_size(&llvm_type),
                symbol.origin.span.clone(),
            )?;
        }
        for (id, _) in self.view.initializers() {
            self.backend
                .builder
                .build_direct_call(self.initializers[&id], &[], "initialize")
                .map_err(compiler_diagnostic)?;
        }
        self.backend
            .builder
            .build_return(Some(&integer.const_zero()))
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    fn predeclare_checked_bindings(
        &mut self,
        owner: EmissionOwner,
        items: &[crate::ItemId],
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        for &id in items {
            let Some(item) = self.view.item(owner, id) else {
                continue;
            };
            let LoweredItemKind::Binding(binding) = &item.kind else {
                continue;
            };
            let Some(state_only) = binding.predeclare_state_only else {
                continue;
            };
            let Some(symbol) = binding.symbol else {
                continue;
            };
            if environment.binding_cells.contains_key(&symbol) {
                continue;
            }
            let span = item.origin.span.clone();
            let (cell, state) = if state_only {
                let state = self
                    .backend
                    .builder
                    .build_malloc(self.backend.context.i8_type(), "binding.state.cell")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                self.backend
                    .register_gc_root_region(state, 1, span.clone())?;
                (state, state)
            } else {
                let cell_type = self.binding_cell_type(owner, symbol)?;
                let cell = self
                    .backend
                    .builder
                    .build_malloc(cell_type, "binding.cell")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                self.backend.register_gc_root_region(
                    cell,
                    self.backend.target_data.get_store_size(&cell_type),
                    span.clone(),
                )?;
                let state = self
                    .backend
                    .builder
                    .build_struct_gep(cell_type, cell, 1, "binding.state")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                (cell, state)
            };
            self.backend
                .builder
                .build_store(state, self.backend.context.i8_type().const_zero())
                .map_err(|error| Diagnostic::new(span, error.to_string()))?;
            environment.binding_cells.insert(symbol, cell);
        }
        Ok(())
    }

    fn emit_block(
        &mut self,
        owner: EmissionOwner,
        id: BlockId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let block = self.view.block(owner, id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered block")
        })?;
        // every binding the block introduces is owned
        // until the block's normal exit.
        let owned_before = environment.owned_order.len();
        let span = block.origin.span.clone();
        let items = block.items.clone();
        let result = block.result;
        let initializer_root = matches!(owner, EmissionOwner::Initializer(initializer_id)
            if self.view.initializer(initializer_id).is_some_and(|initializer| initializer.body == id));
        if !initializer_root {
            self.predeclare_checked_bindings(owner, &items, environment)?;
        }
        for item in items {
            self.emit_item(owner, item, environment)?;
            if environment.returned {
                return Ok(self.backend.unit_value());
            }
        }
        let value = match result {
            Some(expression) => self.emit_expression(owner, expression, environment)?,
            None => self.backend.unit_value(),
        };
        if !environment.returned {
            self.drop_owned_since(environment, owned_before, &span)?;
        }
        Ok(value)
    }

    fn emit_item(
        &mut self,
        owner: EmissionOwner,
        id: crate::ItemId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let item = self
            .view
            .item(owner, id)
            .ok_or_else(|| Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered item"))?
            .clone();
        let unimplemented = |family| {
            Diagnostic::new(
                item.origin.span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            )
        };
        let item_span = item.origin.span.clone();
        match item.kind {
            LoweredItemKind::Binding(binding) => {
                if binding.compile_time_only {
                    return Ok(());
                }
                // The storage-only part of
                // a generic binding records state 1 then 2 and evaluates
                // nothing; a valued binding records state 1, evaluates,
                // stores a module global when the symbol owns one (a nested
                // local stays in the environment), then records state 2.
                if binding.generic {
                    if let Some(symbol) = binding.symbol {
                        self.store_local_initialization_state(
                            owner,
                            environment,
                            symbol,
                            1,
                            &item.origin.span,
                        )?;
                        self.store_local_initialization_state(
                            owner,
                            environment,
                            symbol,
                            2,
                            &item.origin.span,
                        )?;
                        self.store_initialization_state(symbol, 1)?;
                        self.store_initialization_state(symbol, 2)?;
                    }
                    return Ok(());
                }
                let Some(value_id) = binding.value else {
                    return Ok(());
                };
                if let Some(symbol) = binding.symbol {
                    if binding.cell {
                        // compile item allocates the cell before the
                        // state-1 store and the value evaluation.
                        self.allocate_binding_cell(owner, environment, symbol, &item.origin.span)?;
                    }
                    self.store_local_initialization_state(
                        owner,
                        environment,
                        symbol,
                        1,
                        &item.origin.span,
                    )?;
                    self.store_initialization_state(symbol, 1)?;
                }
                // A derived binding is created from its evaluator thunk, not
                // by evaluating the source initializer; a local cell or the
                // module global receives the output through the create call.
                if binding.derived {
                    let symbol = binding
                        .symbol
                        .ok_or_else(|| unimplemented("derived binding"))?;
                    let operation = binding
                        .reactive
                        .ok_or_else(|| unimplemented("derived binding"))?;
                    let (value_slot, metadata_slot) =
                        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
                            let cell_type = self.binding_cell_type(owner, symbol)?;
                            let value_slot = self
                                .backend
                                .builder
                                .build_struct_gep(cell_type, cell, 0, "derived.value")
                                .map_err(compiler_diagnostic)?;
                            let metadata_slot = self
                                .backend
                                .builder
                                .build_struct_gep(cell_type, cell, 2, "derived.metadata")
                                .map_err(compiler_diagnostic)?;
                            (value_slot, metadata_slot)
                        } else if let Some(global) = self.storage.get(&symbol).copied() {
                            let metadata = self
                                .derived_metadata
                                .get(&symbol)
                                .copied()
                                .ok_or_else(|| unimplemented("derived binding"))?;
                            (global.as_pointer_value(), metadata.as_pointer_value())
                        } else {
                            return Err(unimplemented("derived binding"));
                        };
                    self.emit_derived_create(
                        owner,
                        operation,
                        value_slot,
                        metadata_slot,
                        environment,
                        &item.origin.span,
                    )?;
                    self.store_local_initialization_state(
                        owner,
                        environment,
                        symbol,
                        2,
                        &item.origin.span,
                    )?;
                    self.store_initialization_state(symbol, 2)?;
                    return Ok(());
                }
                let value = self.emit_expression(owner, value_id, environment)?;
                if let Some(symbol) = binding.symbol {
                    // A module-level signal creates and records its signal
                    // between the value evaluation and the global store
                    // (compile top level item); a local signal cell
                    // already created it at allocation.
                    if binding.signal
                        && !environment.binding_cells.contains_key(&symbol)
                        && let Some(metadata) = self.signal_metadata.get(&symbol).copied()
                    {
                        let signal = self.emit_signal_create(&item.origin.span)?;
                        self.backend
                            .builder
                            .build_store(metadata.as_pointer_value(), signal)
                            .map_err(compiler_diagnostic)?;
                    }
                    if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
                        let cell_type = self.binding_cell_type(owner, symbol)?;
                        let slot = self
                            .backend
                            .builder
                            .build_struct_gep(cell_type, cell, 0, "binding.value")
                            .map_err(compiler_diagnostic)?;
                        let value =
                            value_as_basic(value).ok_or_else(|| unimplemented("binding value"))?;
                        self.backend
                            .builder
                            .build_store(slot, value)
                            .map_err(compiler_diagnostic)?;
                        self.store_local_initialization_state(
                            owner,
                            environment,
                            symbol,
                            2,
                            &item.origin.span,
                        )?;
                    } else if let Some(global) = self.storage.get(&symbol) {
                        let value =
                            value_as_basic(value).ok_or_else(|| unimplemented("binding value"))?;
                        self.backend
                            .builder
                            .build_store(global.as_pointer_value(), value)
                            .map_err(compiler_diagnostic)?;
                        self.store_initialization_state(symbol, 2)?;
                    } else {
                        environment.locals.insert(symbol, value);
                        self.register_owned_binding(owner, environment, symbol, &item.origin.span)?;
                    }
                }
                Ok(())
            }
            LoweredItemKind::Expression(statement) => {
                // compile item evaluates the statement, then drops
                // its result when one was recorded (the `DiscardedResult`
                // use record is the discriminant).
                let value = self.emit_expression(owner, statement.expression, environment)?;
                if !environment.returned {
                    let value =
                        value_as_basic(value).ok_or_else(|| unimplemented("statement result"))?;
                    self.emit_drop_site(
                        owner,
                        crate::ArtifactUseSite::DiscardedResult(id),
                        DropSource::Value(value),
                        &item.origin.span,
                    )?;
                }
                Ok(())
            }
            LoweredItemKind::Return(item) => {
                if matches!(owner, EmissionOwner::Initializer(_)) {
                    return Err(unimplemented("initializer return"));
                }
                let value = self.emit_expression(owner, item.value, environment)?;
                if environment.returned {
                    return Ok(());
                }
                let value = value_as_basic(value).ok_or_else(|| unimplemented("return value"))?;
                // compile item's return: dispose every reactive
                // scope, close every task scope, then drop every owned binding
                // before leaving the function .
                self.dispose_reactive_scopes(environment, 0, &item_span)?;
                self.close_task_scopes(environment, 0)?;
                self.drop_all_owned(environment, &item_span)?;
                self.backend
                    .builder
                    .build_return(Some(&value))
                    .map_err(compiler_diagnostic)?;
                environment.returned = true;
                Ok(())
            }
            LoweredItemKind::PatternBinding(binding) => {
                // compile item's pattern-binding order: state 1,
                // evaluate, bind, module-global stores, state 2.
                self.store_pattern_initialization_state(owner, binding.pattern, 1)?;
                let value = self.emit_expression(owner, binding.value, environment)?;
                if environment.returned {
                    return Ok(());
                }
                if binding.propagating {
                    let value = value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(
                            item.origin.span.clone(),
                            "destructured value is not first-class",
                        )
                    })?;
                    return self.emit_propagating_binding(owner, &binding, value, environment);
                }
                self.bind_pattern(owner, binding.pattern, value, environment)?;
                self.store_pattern_globals(owner, binding.pattern, environment)?;
                self.store_pattern_initialization_state(owner, binding.pattern, 2)
            }
            LoweredItemKind::Assignment(assignment) => {
                self.emit_assignment(owner, id, &assignment, environment)
            }
            LoweredItemKind::Break(break_item) => {
                let value = if let Some(expression) = break_item.value {
                    let value = self.emit_expression(owner, expression, environment)?;
                    if environment.returned {
                        return Ok(());
                    }
                    value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(
                            item.origin.span.clone(),
                            "loop result is not a first-class value",
                        )
                    })?
                } else {
                    value_as_basic(self.backend.unit_value()).ok_or_else(|| {
                        Diagnostic::new(
                            staple_syntax::Span::Compiler,
                            "internal invariant violated: Unit has an LLVM basic-value representation",
                        )
                    })?
                };
                let Some((exit, owned_before, reactive_before, tasks_before)) = environment
                    .loops
                    .iter()
                    .rev()
                    .find(|context| context.depth == break_item.loop_depth)
                    .map(|context| {
                        (
                            context.exit,
                            context.owned_before,
                            context.reactive_before,
                            context.tasks_before,
                        )
                    })
                else {
                    return Err(unimplemented("break target"));
                };
                // compile item's break disposes the reactive scopes,
                // closes the task scopes, and drops every binding owned since
                // the loop's marks .
                self.dispose_reactive_scopes(environment, reactive_before, &item_span)?;
                self.close_task_scopes(environment, tasks_before)?;
                self.drop_owned_since(environment, owned_before, &item_span)?;
                self.backend
                    .builder
                    .build_unconditional_branch(exit)
                    .map_err(compiler_diagnostic)?;
                let predecessor = self.backend.builder.get_insert_block().ok_or_else(|| {
                    Diagnostic::new(
                        staple_syntax::Span::Compiler,
                        "internal invariant violated: break emission has a current LLVM block",
                    )
                })?;
                environment
                    .loops
                    .iter_mut()
                    .rev()
                    .find(|context| context.depth == break_item.loop_depth)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            staple_syntax::Span::Compiler,
                            "internal invariant violated: break depth names an active loop context",
                        )
                    })?
                    .incoming
                    .push((value, predecessor));
                environment.returned = true;
                Ok(())
            }
            LoweredItemKind::Continue(item) => {
                let Some((header, owned_before, reactive_before, tasks_before)) = environment
                    .loops
                    .iter()
                    .rev()
                    .find(|context| context.depth == item.loop_depth)
                    .map(|context| {
                        (
                            context.header,
                            context.owned_before,
                            context.reactive_before,
                            context.tasks_before,
                        )
                    })
                else {
                    return Err(unimplemented("continue target"));
                };
                // compile item's continue disposes the reactive
                // scopes, closes the task scopes, and drops every binding
                // owned since the loop's marks .
                self.dispose_reactive_scopes(environment, reactive_before, &item_span)?;
                self.close_task_scopes(environment, tasks_before)?;
                self.drop_owned_since(environment, owned_before, &item_span)?;
                self.backend
                    .builder
                    .build_unconditional_branch(header)
                    .map_err(compiler_diagnostic)?;
                environment.returned = true;
                Ok(())
            }
        }
    }

    fn bind_pattern(
        &mut self,
        owner: EmissionOwner,
        id: crate::PatternId,
        value: AnyValueEnum<'context>,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let pattern = self
            .view
            .pattern(owner, id)
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered pattern")
            })?
            .clone();
        let unsupported = |family| {
            Diagnostic::new(
                pattern.origin.span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            )
        };
        let span = pattern.origin.span.clone();
        match &pattern.kind {
            LoweredPatternKind::Wildcard => {
                // bind pattern value drops the discarded subject when
                // lowering recorded a `WildcardDiscard` use.
                let value = value_as_basic(value).ok_or_else(|| unsupported("wildcard cleanup"))?;
                self.emit_drop_site(
                    owner,
                    crate::ArtifactUseSite::WildcardDiscard(id),
                    DropSource::Value(value),
                    &span,
                )
            }
            // A name-like pattern that selects a singleton binds nothing.
            LoweredPatternKind::Binding { symbol: None, .. } => Ok(()),
            LoweredPatternKind::Binding {
                symbol: Some(symbol),
                name,
                ..
            } => {
                // The emitter names every ordinary binding; singleton patterns
                // above bind no value. LLVM value names are not ABI symbols.
                if let Some(value) = value_as_basic(value) {
                    value.set_name(name);
                }
                self.bind_symbol(owner, *symbol, value, environment, &span)
            }
            LoweredPatternKind::Product { elements, .. } => match elements.len() {
                0 => Ok(()),
                1 => self.bind_pattern(owner, elements[0], value, environment),
                _ => {
                    let Some(BasicValueEnum::StructValue(product)) = value_as_basic(value) else {
                        return Err(Diagnostic::new(
                            span,
                            "nested product pattern requires a product value",
                        ));
                    };
                    for (index, element) in elements.iter().enumerate() {
                        let extracted = self
                            .backend
                            .builder
                            .build_extract_value(product, index as u32, "pattern.element")
                            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                        self.bind_pattern(
                            owner,
                            *element,
                            extracted.as_any_value_enum(),
                            environment,
                        )?;
                    }
                    Ok(())
                }
            },
            LoweredPatternKind::Nominal { argument, .. } => {
                // bind pattern value's nominal arm only loads the
                // payload for a `Ref` pattern; every other nominal form binds
                // its argument transparently.
                let value = if pattern.test.identity == crate::LoweredPatternIdentity::Ref {
                    let payload = self
                        .view
                        .pattern(owner, *argument)
                        .map(|argument| argument.test.subject.clone())
                        .unwrap_or(CheckedType::Error);
                    self.backend
                        .load_ref_payloads(value, std::slice::from_ref(&payload), span.clone())?
                        .as_any_value_enum()
                } else {
                    value
                };
                self.bind_pattern(owner, *argument, value, environment)
            }
            LoweredPatternKind::Literal { .. } => Ok(()),
            LoweredPatternKind::At {
                binding,
                pattern: nested,
            } => {
                self.bind_pattern(owner, *binding, value, environment)?;
                self.bind_pattern(owner, *nested, value, environment)
            }
        }
    }

    /// Binds a pattern symbol. Parameter pointers retain the value in locals;
    /// mutable local symbols get a binding cell with initialized state, and
    /// other symbols are plain locals. Owned bindings register their cleanup.
    fn bind_symbol(
        &mut self,
        owner: EmissionOwner,
        symbol: SymbolId,
        value: AnyValueEnum<'context>,
        environment: &mut FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        if environment.parameter_pointers.contains_key(&symbol) {
            environment.locals.insert(symbol, value);
            return Ok(());
        }
        let mutable = self
            .view
            .symbol(symbol)
            .is_some_and(|symbol| symbol.mutable_storage)
            && !self.storage.contains_key(&symbol);
        if mutable {
            let cell = self.allocate_binding_cell(owner, environment, symbol, span)?;
            let cell_type = self.binding_cell_type(owner, symbol)?;
            let slot = self
                .backend
                .builder
                .build_struct_gep(cell_type, cell, 0, "binding.value")
                .map_err(compiler_diagnostic)?;
            let value = value_as_basic(value)
                .ok_or_else(|| Diagnostic::new(span.clone(), "pattern value is not storable"))?;
            self.backend
                .builder
                .build_store(slot, value)
                .map_err(compiler_diagnostic)?;
            return self.store_local_initialization_state(owner, environment, symbol, 2, span);
        }
        environment.locals.insert(symbol, value);
        self.register_owned_binding(owner, environment, symbol, span)
    }

    /// After a module-level pattern binding
    /// has bound its symbols, write each symbol's local value into its module
    /// global.
    fn store_pattern_globals(
        &mut self,
        owner: EmissionOwner,
        pattern: PatternId,
        environment: &FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let Some(record) = self.view.pattern(owner, pattern).cloned() else {
            return Ok(());
        };
        match record.kind {
            LoweredPatternKind::Binding {
                symbol: Some(symbol),
                ..
            } => {
                let Some(global) = self.storage.get(&symbol) else {
                    return Ok(());
                };
                let value = environment.locals.get(&symbol).copied().ok_or_else(|| {
                    Diagnostic::new(record.origin.span.clone(), "unbound destructuring pattern")
                })?;
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(record.origin.span.clone(), "pattern value is not storable")
                })?;
                self.backend
                    .builder
                    .build_store(global.as_pointer_value(), value)
                    .map_err(compiler_diagnostic)?;
                Ok(())
            }
            LoweredPatternKind::At {
                binding,
                pattern: nested,
            } => {
                self.store_pattern_globals(owner, binding, environment)?;
                self.store_pattern_globals(owner, nested, environment)
            }
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.store_pattern_globals(owner, element, environment)?;
                }
                Ok(())
            }
            LoweredPatternKind::Nominal { argument, .. } => {
                self.store_pattern_globals(owner, argument, environment)
            }
            LoweredPatternKind::Wildcard | LoweredPatternKind::Literal { .. } => Ok(()),
            LoweredPatternKind::Binding { symbol: None, .. } => Ok(()),
        }
    }

    /// Write the module
    /// initialization state of every symbol a pattern binds.
    fn store_pattern_initialization_state(
        &mut self,
        owner: EmissionOwner,
        pattern: PatternId,
        state: u64,
    ) -> CodeGenerationResult<()> {
        let Some(record) = self.view.pattern(owner, pattern).cloned() else {
            return Ok(());
        };
        match record.kind {
            LoweredPatternKind::Binding {
                symbol: Some(symbol),
                ..
            } => self.store_initialization_state(symbol, state),
            LoweredPatternKind::At {
                binding,
                pattern: nested,
            } => {
                self.store_pattern_initialization_state(owner, binding, state)?;
                self.store_pattern_initialization_state(owner, nested, state)
            }
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.store_pattern_initialization_state(owner, element, state)?;
                }
                Ok(())
            }
            LoweredPatternKind::Nominal { argument, .. } => {
                self.store_pattern_initialization_state(owner, argument, state)
            }
            LoweredPatternKind::Wildcard | LoweredPatternKind::Literal { .. } => Ok(()),
            LoweredPatternKind::Binding { symbol: None, .. } => Ok(()),
        }
    }

    fn emit_expression(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let expression = self
            .view
            .expression(owner, id)
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered expression")
            })?
            .clone();
        let value = self.emit_expression_value(owner, id, &expression, environment)?;
        // a diverged body releases moved
        // ownership and returns without coercing.
        if environment.returned {
            self.release_moved_ownership(owner, environment, &expression)?;
            return Ok(value);
        }
        // compile expression's divergence handling: an expression of
        // type `Never` (or one coerced from `Never`) ends the block. Order
        // The `unreachable` instruction precedes the moved-
        // ownership release.
        let diverges = expression.value_type == CheckedType::Never
            || expression
                .coercion
                .as_ref()
                .is_some_and(|coercion| coercion.source == CheckedType::Never);
        if diverges {
            self.backend
                .builder
                .build_unreachable()
                .map_err(compiler_diagnostic)?;
            environment.returned = true;
            self.release_moved_ownership(owner, environment, &expression)?;
            return Ok(value);
        }
        // apply the recorded coercion plan. Lowering already
        // selected the alternatives, so emission never re-selects one.
        let value = match (&expression.coercion, &expression.coercion_plan) {
            (Some(coercion), Some(plan)) => self.emit_coercion(
                value,
                &coercion.source,
                &coercion.target,
                plan,
                &expression.origin.span,
            )?,
            (Some(_), None) => {
                return Err(Diagnostic::new(
                    expression.origin.span.clone(),
                    "lowered emitter: coercion",
                ));
            }
            (None, _) => value,
        };
        self.release_moved_ownership(owner, environment, &expression)?;
        Ok(value)
    }

    /// Compile expression's `release_moved_ownership`: clear the
    /// live flag of every moved owned value, then the initialization state of
    /// every symbol the expression moved out of a binding cell.
    fn release_moved_ownership(
        &mut self,
        owner: EmissionOwner,
        environment: &mut FunctionEnvironment<'context>,
        expression: &crate::LoweredExpression,
    ) -> CodeGenerationResult<()> {
        for symbol in &expression.moved_symbols {
            if let Some(record) = environment.owned.get(symbol)
                && let Some(live) = record.live
            {
                self.backend
                    .builder
                    .build_store(live, self.backend.context.bool_type().const_zero())
                    .map_err(compiler_diagnostic)?;
            }
            // The emitter clears the binding cell's state only for a symbol with
            // mutable storage (`has_mutable_storage`); a coroutine frame cell
            // for an ordinary `let` was not cleared because the emitter never
            // dropped it. emission drops live frame cells at
            // completion, so a moved-out frame binding's state is cleared too;
            // otherwise the completion or cancel drop would double-drop it.
            let frame_binding = environment
                .coroutine
                .as_ref()
                .is_some_and(|coroutine| coroutine.frame_bindings.contains(symbol));
            if frame_binding
                || self
                    .view
                    .symbol(*symbol)
                    .is_some_and(|symbol| symbol.mutable_storage)
            {
                self.store_local_initialization_state(
                    owner,
                    environment,
                    *symbol,
                    0,
                    &expression.origin.span,
                )?;
            }
        }
        Ok(())
    }

    /// Executes a recorded coercion plan. Source and target types follow the
    /// plan's recursive payload projections so nested layouts stay in lockstep.
    fn emit_coercion(
        &mut self,
        value: AnyValueEnum<'context>,
        source: &CheckedType,
        target: &CheckedType,
        plan: &crate::LoweredCoercionPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        match plan {
            crate::LoweredCoercionPlan::Identity => Ok(value),
            crate::LoweredCoercionPlan::SliceRef { length } => {
                let Some(BasicValueEnum::PointerValue(pointer)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid fixed reference representation",
                    ));
                };
                self.backend.build_slice_ref_value(pointer, *length)
            }
            crate::LoweredCoercionPlan::SumInject {
                alternative,
                payload,
            } => {
                let CheckedType::Sum(target_sum) = target else {
                    return Err(Diagnostic::new(span.clone(), "invalid sum coercion target"));
                };
                let storage = self.backend.begin_sum_storage(target_sum, span)?;
                let target_alternative = &target_sum.alternatives[*alternative];
                let value = self.emit_coercion(value, source, target_alternative, payload, span)?;
                self.backend.store_sum_payload(
                    value,
                    target_alternative,
                    *alternative,
                    &storage.storage,
                    span.clone(),
                )?;
                self.backend.load_sum_storage(&storage, span)
            }
            crate::LoweredCoercionPlan::SumWiden { arms } => {
                let CheckedType::Sum(source_sum) = source else {
                    return Err(Diagnostic::new(span.clone(), "invalid sum coercion source"));
                };
                let CheckedType::Sum(target_sum) = target else {
                    return Err(Diagnostic::new(span.clone(), "invalid sum coercion target"));
                };
                let storage = self.backend.begin_sum_storage(target_sum, span)?;
                let Some(BasicValueEnum::StructValue(source_value)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "sum value has an invalid representation",
                    ));
                };
                let source_tag = self.backend.build_sum_tag(source_value, "sum.source.tag")?;
                let function = self
                    .backend
                    .builder
                    .get_insert_block()
                    .and_then(|block| block.get_parent())
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "sum coercion is not in a function")
                    })?;
                let merge = self
                    .backend
                    .context
                    .append_basic_block(function, "sum.coerce.done");
                let cases = source_sum
                    .alternatives
                    .iter()
                    .enumerate()
                    .map(|(index, _)| {
                        (
                            self.backend
                                .context
                                .i32_type()
                                .const_int(index as u64, false),
                            self.backend
                                .context
                                .append_basic_block(function, "sum.coerce.case"),
                        )
                    })
                    .collect::<Vec<_>>();
                self.backend
                    .builder
                    .build_switch(source_tag, merge, &cases)
                    .map_err(compiler_diagnostic)?;
                for (source_index, alternative) in source_sum.alternatives.iter().enumerate() {
                    self.backend.builder.position_at_end(cases[source_index].1);
                    let Some(arm) = arms.get(source_index).and_then(Option::as_ref) else {
                        // Propagating bindings narrow away their selected
                        // success tag before widening the residual variants.
                        self.backend
                            .builder
                            .build_unreachable()
                            .map_err(compiler_diagnostic)?;
                        continue;
                    };
                    let payload = self.backend.extract_sum_alternative(
                        source_value,
                        source_sum,
                        source_index,
                        span.clone(),
                    )?;
                    let target_alternative = &target_sum.alternatives[arm.target];
                    let payload = self.emit_coercion(
                        payload.as_any_value_enum(),
                        alternative,
                        target_alternative,
                        &arm.payload,
                        span,
                    )?;
                    self.backend.store_sum_payload(
                        payload,
                        target_alternative,
                        arm.target,
                        &storage.storage,
                        span.clone(),
                    )?;
                    self.backend
                        .builder
                        .build_unconditional_branch(merge)
                        .map_err(compiler_diagnostic)?;
                }
                self.backend.builder.position_at_end(merge);
                self.backend.load_sum_storage(&storage, span)
            }
        }
    }

    fn emit_expression_value(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        expression: &crate::LoweredExpression,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        match &expression.kind {
            LoweredExpressionKind::Integer(integer) => Ok(self
                .backend
                .compile_integer_type(integer.integer_type)
                .const_int(integer.value, false)
                .into()),
            LoweredExpressionKind::Float(float) => Ok(self
                .backend
                .compile_float_type(float.float_type)
                .const_float(float.value)
                .into()),
            LoweredExpressionKind::Block(block) => self.emit_block(owner, *block, environment),
            LoweredExpressionKind::Name(name) => {
                // compile expression uncoerced's `Name` path returns
                // the unit value for a singleton before any storage read.
                if name.singleton.is_some() {
                    return Ok(self.backend.unit_value());
                }
                if let Some(reactive) = name.reactive {
                    let operation =
                        self.view
                            .reactive_operation(owner, reactive)
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    expression.origin.span.clone(),
                                    "missing reactive read record",
                                )
                            })?;
                    match operation.kind {
                        // A signal or derived read tracks through
                        // `load_symbol_value`, exactly using the recorded layout's
                        // compile symbol value.
                        LoweredReactiveOperationKind::SignalRead { .. }
                        | LoweredReactiveOperationKind::DerivedRead { .. } => {}
                        LoweredReactiveOperationKind::SignalCreate { .. }
                        | LoweredReactiveOperationKind::SignalNotify { .. }
                        | LoweredReactiveOperationKind::DerivedCreate { .. }
                        | LoweredReactiveOperationKind::Scope
                        | LoweredReactiveOperationKind::Reaction { .. }
                        | LoweredReactiveOperationKind::Batch { .. }
                        | LoweredReactiveOperationKind::Until { .. }
                        | LoweredReactiveOperationKind::Snapshot => {
                            return Err(Diagnostic::new(
                                expression.origin.span.clone(),
                                "invalid reactive read record",
                            ));
                        }
                    }
                }
                // Name checks the state whenever the read
                // requires one or the symbol has mutable storage. The load
                // reads the recorded uncoerced representation; emit_expression
                // applies the coercion afterward.
                self.load_symbol_value(
                    owner,
                    name.symbol,
                    name.requires_initialization_check || name.mutable,
                    expression
                        .coercion
                        .as_ref()
                        .map_or(&expression.value_type, |coercion| &coercion.source),
                    &expression.origin.span,
                    environment,
                )
            }
            LoweredExpressionKind::Deferred(_) => Err(Diagnostic::new(
                expression.origin.span.clone(),
                "internal invariant: deferred expression reached lowered emission",
            )),
            LoweredExpressionKind::String(string) => {
                // Literal emission uses the common String builder.
                self.backend
                    .build_string_literal(&string.value, expression.origin.span.clone())
                    .map(|value| value.as_any_value_enum())
            }
            LoweredExpressionKind::CString(string) => {
                let text =
                    std::str::from_utf8(&string.bytes[..string.bytes.len() - 1]).map_err(|_| {
                        Diagnostic::new(expression.origin.span.clone(), "invalid C string payload")
                    })?;
                // shared with build owned c string.
                self.backend
                    .build_owned_c_string(text, expression.origin.span.clone())
            }
            LoweredExpressionKind::Access(access) => {
                self.emit_access(owner, expression, access, environment)
            }
            LoweredExpressionKind::Product(product) => {
                self.emit_product(owner, expression, product, environment)
            }
            LoweredExpressionKind::RepeatedProduct(repeated) => {
                self.emit_repeated_product(owner, expression, repeated, environment)
            }
            // compile expression uncoerced's `Satisfies` path is
            // transparent: emit the operand and let this expression's own
            // header coercion apply in `emit_expression`.
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.emit_expression(owner, satisfies.value, environment)
            }
            LoweredExpressionKind::Logical(logical) => {
                self.emit_logical(owner, expression, logical, environment)
            }
            LoweredExpressionKind::Loop(loop_) => {
                self.emit_loop(owner, id, expression, loop_, environment)
            }
            LoweredExpressionKind::Match(match_) => {
                self.emit_match(owner, expression, match_, environment)
            }
            LoweredExpressionKind::Index(index) => {
                self.emit_index(owner, id, expression, index, environment)
            }
            LoweredExpressionKind::StringTemplate(template) => {
                self.emit_string_template(owner, id, expression, template, environment)
            }
            LoweredExpressionKind::Call(call) => self.emit_call(owner, *call, environment),
            LoweredExpressionKind::CallableValue(callable) => {
                self.emit_callable_value(owner, *callable, environment)
            }
            LoweredExpressionKind::Resource(use_id) => {
                self.emit_resource_read(owner, *use_id, environment)
            }
            LoweredExpressionKind::With(with_id) => self.emit_with(owner, *with_id, environment),
            LoweredExpressionKind::Coro(id) => {
                self.emit_coro(owner, *id, environment, &expression.origin.span)
            }
            LoweredExpressionKind::Await(id) => {
                self.emit_await(owner, *id, environment, &expression.origin.span)
            }
        }
    }

    /// Emits a structural access: load any Ref payload chain, then project the
    /// representation, scalar, product element, or bounds-checked slice element.
    fn emit_access(
        &mut self,
        owner: EmissionOwner,
        expression: &crate::LoweredExpression,
        access: &crate::LoweredAccess,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = &expression.origin.span;
        let base = self.emit_expression(owner, access.base, environment)?;
        if environment.returned {
            return Ok(self.backend.unit_value());
        }
        match &access.kind {
            crate::LoweredAccessKind::Representation { dereference } => {
                if dereference.is_empty() {
                    Ok(base)
                } else {
                    self.backend
                        .load_ref_payloads(base, dereference, span.clone())
                        .map(|value| value.as_any_value_enum())
                }
            }
            // The emitter returns a scalar access as-is, dereference chain included.
            crate::LoweredAccessKind::Scalar { .. } => Ok(base),
            crate::LoweredAccessKind::Product { index, dereference } => {
                let value = if dereference.is_empty() {
                    value_as_basic(base)
                } else {
                    Some(
                        self.backend
                            .load_ref_payloads(base, dereference, span.clone())?,
                    )
                };
                let Some(BasicValueEnum::StructValue(value)) = value else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "element access requires a product value",
                    ));
                };
                self.backend
                    .builder
                    .build_extract_value(value, *index as u32, "element")
                    .map(|value| value.as_any_value_enum())
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))
            }
            crate::LoweredAccessKind::Slice { index, dereference } => {
                let value = if dereference.is_empty() {
                    value_as_basic(base)
                } else {
                    Some(
                        self.backend
                            .load_ref_payloads(base, dereference, span.clone())?,
                    )
                };
                let Some(BasicValueEnum::StructValue(reference)) = value else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "slice has an invalid representation",
                    ));
                };
                let pointer = self
                    .backend
                    .builder
                    .build_extract_value(reference, 0, "slice.pointer")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let length = self
                    .backend
                    .builder
                    .build_extract_value(reference, 1, "slice.length")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let position = self.backend.size_type.const_int(*index as u64, false);
                let element_type = self.backend.compile_type(&expression.value_type)?;
                let pointer = self.backend.build_index_pointer(
                    pointer,
                    position,
                    length,
                    element_type,
                    span.clone(),
                )?;
                self.backend
                    .builder
                    .build_load(element_type, pointer, "index.value")
                    .map(|value| value.as_any_value_enum())
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))
            }
        }
    }

    /// Replays product steps in source order; later writes replace earlier slot
    /// values. Assembles the final field layout, collapsing one-element products.
    fn emit_product(
        &mut self,
        owner: EmissionOwner,
        expression: &crate::LoweredExpression,
        product: &crate::LoweredProduct,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = &expression.origin.span;
        let mut slots: Vec<Option<BasicValueEnum<'context>>> = vec![None; product.fields.len()];
        for step in &product.steps {
            let (value, slot, spread) = match step {
                crate::LoweredProductStep::Positional { expression, slot } => (
                    self.emit_expression(owner, *expression, environment)?,
                    Some(*slot),
                    None,
                ),
                crate::LoweredProductStep::Designated {
                    expression, slot, ..
                } => (
                    self.emit_expression(owner, *expression, environment)?,
                    Some(*slot),
                    None,
                ),
                crate::LoweredProductStep::Default {
                    expression, slot, ..
                } => (
                    self.emit_expression(owner, *expression, environment)?,
                    Some(*slot),
                    None,
                ),
                crate::LoweredProductStep::PositionalSpread {
                    expression,
                    mappings,
                } => (
                    self.emit_expression(owner, *expression, environment)?,
                    None,
                    Some(
                        mappings
                            .iter()
                            .map(|mapping| (mapping.source, mapping.slot))
                            .collect::<Vec<_>>(),
                    ),
                ),
                crate::LoweredProductStep::NamedSpread {
                    expression,
                    mappings,
                } => (
                    self.emit_expression(owner, *expression, environment)?,
                    None,
                    Some(
                        mappings
                            .iter()
                            .map(|mapping| (mapping.source, mapping.slot))
                            .collect::<Vec<_>>(),
                    ),
                ),
            };
            if environment.returned {
                return Ok(self.backend.unit_value());
            }
            match (slot, spread) {
                (Some(slot), _) => {
                    slots[slot] = Some(value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(span.clone(), "product element is not a first-class value")
                    })?);
                }
                (None, Some(mappings)) => {
                    if mappings.is_empty() {
                        continue;
                    }
                    let Some(BasicValueEnum::StructValue(product_value)) = value_as_basic(value)
                    else {
                        return Err(Diagnostic::new(
                            span.clone(),
                            "product spread operand has an invalid representation",
                        ));
                    };
                    for (source, slot) in mappings {
                        let element = self
                            .backend
                            .builder
                            .build_extract_value(
                                product_value,
                                source as u32,
                                "product.spread.element",
                            )
                            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                        slots[slot] = Some(element);
                    }
                }
                (None, None) => {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "internal invariant violated: product steps always place or spread",
                    ));
                }
            }
        }
        let values = slots
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.ok_or_else(|| {
                    Diagnostic::new(
                        staple_syntax::Span::Compiler,
                        format!("missing product element at position {index}"),
                    )
                })
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        Ok(self
            .backend
            .build_product_value(&values, span.clone())?
            .as_any_value_enum())
    }

    /// `(value; count)`. The emitter evaluates the element once
    /// and replicates it across the fixed arity, or returns it directly for
    /// the collapsed (arity one or symbolic) representation.
    fn emit_repeated_product(
        &mut self,
        owner: EmissionOwner,
        expression: &crate::LoweredExpression,
        repeated: &crate::LoweredRepeatedProduct,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let value = self.emit_expression(owner, repeated.expression, environment)?;
        if environment.returned {
            return Ok(self.backend.unit_value());
        }
        let count = if repeated.collapsed {
            1
        } else {
            match repeated.count {
                crate::LoweredRepeatCount::Fixed(count) => count,
                crate::LoweredRepeatCount::Symbolic(_) => 1,
            }
        };
        if count == 1 {
            return Ok(value);
        }
        let value = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(
                expression.origin.span.clone(),
                "repeated product element has an invalid representation",
            )
        })?;
        let values = vec![value; count];
        Ok(self
            .backend
            .build_product_value(&values, expression.origin.span.clone())?
            .as_any_value_enum())
    }

    /// Constructs a formatter, writes literal parts, invokes each interpolation's
    /// bound Display or Debug method, then finishes the formatted String.
    fn emit_string_template(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        expression: &crate::LoweredExpression,
        template: &crate::LoweredStringTemplate,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = expression.origin.span.clone();
        let constructor = self.bound_function(
            owner,
            crate::LoweredBindingSite::FormattingConstructor(id),
            &span,
        )?;
        let formatter = self
            .backend
            .builder
            .build_direct_call(
                constructor,
                &[self.null_environment()],
                "template.formatter",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic();
        let storage = self
            .backend
            .builder
            .build_alloca(formatter.get_type(), "template.formatter.storage")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(storage, formatter)
            .map_err(compiler_diagnostic)?;
        let mut write = None;
        for (part_index, part) in template.parts.iter().enumerate() {
            match part {
                crate::LoweredStringTemplatePart::Literal(literal) => {
                    let write = match write {
                        Some(write) => write,
                        None => {
                            let function = self.bound_function(
                                owner,
                                crate::LoweredBindingSite::FormattingWrite(id),
                                &span,
                            )?;
                            write = Some(function);
                            function
                        }
                    };
                    self.backend.build_formatter_write_literal(
                        write,
                        storage.into(),
                        literal,
                        span.clone(),
                    )?;
                }
                crate::LoweredStringTemplatePart::Interpolation(interpolation) => {
                    let value =
                        self.emit_expression(owner, interpolation.expression, environment)?;
                    if environment.returned {
                        return Ok(self.backend.unit_value());
                    }
                    let value = value_as_basic(value).ok_or_else(|| {
                        Diagnostic::new(
                            span.clone(),
                            "interpolation value has no runtime representation",
                        )
                    })?;
                    let function = self.bound_function(
                        owner,
                        crate::LoweredBindingSite::Interpolation {
                            template: id,
                            part: part_index,
                        },
                        &span,
                    )?;
                    self.backend
                        .builder
                        .build_direct_call(
                            function,
                            &[self.null_environment(), value.into(), storage.into()],
                            "template.fmt",
                        )
                        .map_err(compiler_diagnostic)?;
                }
            }
        }
        let finish = self.bound_function(
            owner,
            crate::LoweredBindingSite::FormattingFinish(id),
            &span,
        )?;
        let formatter = self
            .backend
            .builder
            .build_load(formatter.get_type(), storage, "template.finished.formatter")
            .map_err(compiler_diagnostic)?;
        Ok(self
            .backend
            .builder
            .build_direct_call(
                finish,
                &[self.null_environment(), formatter.into()],
                "template.finish",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .as_any_value_enum())
    }

    /// The declared function of one binding site: a direct instance or a
    /// structural artifact.
    fn bound_function(
        &self,
        owner: EmissionOwner,
        site: crate::LoweredBindingSite,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let binding = self
            .view
            .binding(owner, site)
            .ok_or_else(|| Diagnostic::new(span.clone(), "lowered site has no binding"))?;
        match binding {
            crate::LoweredBoundTarget::Instance(instance) => self
                .instances
                .get(instance)
                .copied()
                .ok_or_else(|| Diagnostic::new(span.clone(), "bound instance is not declared")),
            crate::LoweredBoundTarget::Artifact(ordinal) => self
                .artifacts
                .get(ordinal)
                .and_then(|functions| functions.first())
                .copied()
                .ok_or_else(|| Diagnostic::new(span.clone(), "bound artifact is not declared")),
            LoweredBoundTarget::Route(_) => Err(Diagnostic::new(
                span.clone(),
                "bound site is not a function",
            )),
        }
    }

    fn null_environment(&self) -> BasicMetadataValueEnum<'context> {
        self.backend
            .context
            .ptr_type(AddressSpace::default())
            .const_null()
            .into()
    }

    /// Emits a loop. Normal iteration drops its discarded result before the back
    /// edge; breaks contribute value-phi inputs. An unreachable exit emits unreachable.
    fn emit_loop(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        expression: &crate::LoweredExpression,
        loop_: &crate::LoweredLoop,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = expression.origin.span.clone();
        let function = self
            .backend
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "loop is not in a function"))?;
        let header = self
            .backend
            .context
            .append_basic_block(function, "loop.body");
        let exit = self
            .backend
            .context
            .append_basic_block(function, "loop.exit");
        self.backend
            .builder
            .build_unconditional_branch(header)
            .map(|_| ())
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(header);
        environment.loops.push(LoopContext {
            depth: loop_.depth,
            header,
            exit,
            owned_before: environment.owned_order.len(),
            reactive_before: environment.reactive_scopes.len(),
            tasks_before: environment.task_scopes.len(),
            incoming: Vec::new(),
        });
        environment.returned = false;
        let value = self.emit_block(owner, loop_.body, environment)?;
        if !environment.returned {
            // compile loop expression drops a droppable body result
            // before the back edge (the `LoopBodyResult` use record).
            if loop_.drops_body_result {
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "loop body result is not first-class")
                })?;
                self.emit_drop_site(
                    owner,
                    crate::ArtifactUseSite::LoopBodyResult(id),
                    DropSource::Value(value),
                    &span,
                )?;
            }
            self.backend
                .builder
                .build_unconditional_branch(header)
                .map(|_| ())
                .map_err(compiler_diagnostic)?;
        }
        let context = environment.loops.pop().ok_or_else(|| {
            Diagnostic::new(
                staple_syntax::Span::Compiler,
                "internal invariant violated: loop context remains active until its body is emitted",
            )
        })?;
        self.backend.builder.position_at_end(exit);
        if context.incoming.is_empty() {
            self.backend
                .builder
                .build_unreachable()
                .map_err(compiler_diagnostic)?;
            environment.returned = true;
            return Ok(self.backend.unit_value());
        }
        environment.returned = false;
        let result_type = self.backend.compile_type(&loop_.result_type)?;
        Ok(self
            .backend
            .build_phi_value(result_type, &context.incoming, "loop.value")?
            .as_any_value_enum())
    }

    /// Evaluates the subject once and emits each arm's recorded pattern test.
    /// Divergent arms contribute no phi input; fall-through emits unreachable.
    fn emit_match(
        &mut self,
        owner: EmissionOwner,
        expression: &crate::LoweredExpression,
        match_: &crate::LoweredMatch,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = expression.origin.span.clone();
        let subject = self.emit_expression(owner, match_.subject, environment)?;
        if environment.returned {
            return Ok(self.backend.unit_value());
        }
        let Some(subject) = value_as_basic(subject) else {
            return Err(Diagnostic::new(
                span.clone(),
                "match subject is not first-class",
            ));
        };
        let function = self
            .backend
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "match is not in a function"))?;
        let merge_block = self
            .backend
            .context
            .append_basic_block(function, "match.merge");
        let mut incoming: Vec<(BasicValueEnum<'context>, BasicBlock<'context>)> = Vec::new();
        let branch_base = environment.clone();
        let mut continuing_state = None;
        let mut terminating_state = None;
        for arm in &match_.arms {
            environment.restore_local_state(&branch_base);
            let owned_before = environment.owned_order.len();
            let arm_block = self
                .backend
                .context
                .append_basic_block(function, "match.arm");
            let failure_block = self
                .backend
                .context
                .append_basic_block(function, "match.next");
            self.emit_match_pattern_branch(
                owner,
                arm.pattern,
                subject,
                arm_block,
                failure_block,
                environment,
            )?;
            self.backend.builder.position_at_end(arm_block);
            environment.returned = false;
            let value = self.emit_expression(owner, arm.body, environment)?;
            if !environment.returned {
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        arm.origin.span.clone(),
                        "match arm result is not first-class",
                    )
                })?;
                // compile match expression drops the arm's pattern
                // bindings and locals at the arm's normal exit.
                self.drop_owned_since(environment, owned_before, &arm.origin.span)?;
                self.backend
                    .builder
                    .build_unconditional_branch(merge_block)
                    .map_err(compiler_diagnostic)?;
                let predecessor = self.backend.builder.get_insert_block().ok_or_else(|| {
                    Diagnostic::new(
                        staple_syntax::Span::Compiler,
                        "internal invariant violated: continuing match arm has a current LLVM block",
                    )
                })?;
                incoming.push((value, predecessor));
                continuing_state = Some(environment.clone());
            } else {
                Self::forget_owned_since(environment, owned_before);
                terminating_state = Some(environment.clone());
            }
            self.backend.builder.position_at_end(failure_block);
        }
        self.backend
            .builder
            .build_unreachable()
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(merge_block);
        if incoming.is_empty() {
            self.backend
                .builder
                .build_unreachable()
                .map_err(compiler_diagnostic)?;
            environment.returned = true;
            if let Some(state) = terminating_state {
                environment.restore_local_state(&state);
            }
            return Ok(self.backend.unit_value());
        }
        if let Some(state) = &continuing_state {
            environment.restore_local_state(state);
        }
        environment.returned = false;
        let result_type = self.backend.compile_type(&expression.value_type)?;
        Ok(self
            .backend
            .build_phi_value(result_type, &incoming, "match.value")?
            .as_any_value_enum())
    }

    /// Emits one match arm's recorded conditional test.
    /// Every decision comes from the lowered test plan; the emitter only
    /// builds the blocks, compares, and payload loads around them.
    #[allow(clippy::too_many_arguments)]
    fn emit_match_pattern_branch(
        &mut self,
        owner: EmissionOwner,
        pattern_id: PatternId,
        value: BasicValueEnum<'context>,
        success: BasicBlock<'context>,
        failure: BasicBlock<'context>,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let pattern = self
            .view
            .pattern(owner, pattern_id)
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered pattern")
            })?
            .clone();
        let span = pattern.origin.span.clone();
        let subject = pattern.test.subject.clone();
        let sum_of_subject = |span: &staple_syntax::Span| match &subject {
            CheckedType::Sum(sum) => Ok(sum.clone()),
            _ => Err(Diagnostic::new(
                span.clone(),
                "checked match pattern has a non-sum value",
            )),
        };
        match &pattern.kind {
            crate::LoweredPatternKind::At {
                binding,
                pattern: nested,
            } => {
                self.bind_pattern(owner, *binding, value.as_any_value_enum(), environment)?;
                self.emit_match_pattern_branch(owner, *nested, value, success, failure, environment)
            }
            crate::LoweredPatternKind::Binding { .. } => {
                if pattern.test.identity == crate::LoweredPatternIdentity::Singleton {
                    // A singleton sum test compares the tag; a singleton
                    // distinct match succeeds unconditionally.
                    if let Some(index) = pattern.test.sum_alternative {
                        let sum = sum_of_subject(&span)?;
                        let Some(BasicValueEnum::StructValue(sum_value)) =
                            value_as_basic(value.into())
                        else {
                            return Err(Diagnostic::new(
                                span.clone(),
                                "sum match value has an invalid representation",
                            ));
                        };
                        let _ = sum;
                        let tag = self.backend.build_sum_tag(sum_value, "match.tag")?;
                        let matches = self.backend.build_sum_tag_compare(
                            tag,
                            index,
                            "match.singleton.tag",
                        )?;
                        self.backend
                            .builder
                            .build_conditional_branch(matches, success, failure)
                            .map(|_| ())
                            .map_err(compiler_diagnostic)
                    } else {
                        self.backend
                            .builder
                            .build_unconditional_branch(success)
                            .map(|_| ())
                            .map_err(compiler_diagnostic)
                    }
                } else if let Some(index) = pattern.test.sum_alternative {
                    let sum = sum_of_subject(&span)?;
                    let Some(BasicValueEnum::StructValue(sum_value)) = value_as_basic(value.into())
                    else {
                        return Err(Diagnostic::new(
                            span.clone(),
                            "sum match value has an invalid representation",
                        ));
                    };
                    let tag = self.backend.build_sum_tag(sum_value, "match.tag")?;
                    let selected = self.backend.context.append_basic_block(
                        success.get_parent().ok_or_else(|| {
                            Diagnostic::new(
                                staple_syntax::Span::Compiler,
                                "internal invariant violated: match block belongs to an LLVM function",
                            )
                        })?,
                        "match.typed.selected",
                    );
                    let matches =
                        self.backend
                            .build_sum_tag_compare(tag, index, "match.typed.tag")?;
                    self.backend
                        .builder
                        .build_conditional_branch(matches, selected, failure)
                        .map(|_| ())
                        .map_err(compiler_diagnostic)?;
                    self.backend.builder.position_at_end(selected);
                    let payload = self.backend.extract_sum_alternative(
                        sum_value,
                        &sum,
                        index,
                        span.clone(),
                    )?;
                    self.bind_pattern(owner, pattern_id, payload.as_any_value_enum(), environment)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(success)
                        .map(|_| ())
                        .map_err(compiler_diagnostic)
                } else {
                    self.bind_pattern(owner, pattern_id, value.as_any_value_enum(), environment)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(success)
                        .map(|_| ())
                        .map_err(compiler_diagnostic)
                }
            }
            crate::LoweredPatternKind::Literal { .. } => {
                let Some(bytes) = &pattern.test.literal else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "string pattern has no decoded literal",
                    ));
                };
                let Ok(literal) = std::str::from_utf8(bytes) else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "string pattern literal is not UTF-8",
                    ));
                };
                let value = if let Some(index) = pattern.test.sum_alternative {
                    let sum = sum_of_subject(&span)?;
                    let Some(BasicValueEnum::StructValue(sum_value)) = value_as_basic(value.into())
                    else {
                        return Err(Diagnostic::new(
                            span.clone(),
                            "sum match value has an invalid representation",
                        ));
                    };
                    let tag = self.backend.build_sum_tag(sum_value, "match.tag")?;
                    let selected = self.backend.context.append_basic_block(
                        success.get_parent().ok_or_else(|| {
                            Diagnostic::new(
                                staple_syntax::Span::Compiler,
                                "internal invariant violated: match block belongs to an LLVM function",
                            )
                        })?,
                        "match.string.selected",
                    );
                    let matches =
                        self.backend
                            .build_sum_tag_compare(tag, index, "match.string.tag")?;
                    self.backend
                        .builder
                        .build_conditional_branch(matches, selected, failure)
                        .map(|_| ())
                        .map_err(compiler_diagnostic)?;
                    self.backend.builder.position_at_end(selected);
                    self.backend
                        .extract_sum_alternative(sum_value, &sum, index, span.clone())?
                        .as_any_value_enum()
                } else {
                    value.as_any_value_enum()
                };
                let Some(BasicValueEnum::StructValue(string)) = value_as_basic(value) else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "string match value has an invalid representation",
                    ));
                };
                self.backend
                    .build_string_literal_pattern_compare(string, literal, success, failure, span)
            }
            crate::LoweredPatternKind::Product { elements, .. } if elements.len() == 1 => self
                .emit_match_pattern_branch(
                    owner,
                    elements[0],
                    value,
                    success,
                    failure,
                    environment,
                ),
            crate::LoweredPatternKind::Product { elements, .. } => {
                let CheckedType::Product(_) = &subject else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "checked product pattern has a non-product value",
                    ));
                };
                let Some(BasicValueEnum::StructValue(product_value)) = value_as_basic(value.into())
                else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "product match value has an invalid representation",
                    ));
                };
                if elements.is_empty() {
                    return self
                        .backend
                        .builder
                        .build_unconditional_branch(success)
                        .map(|_| ())
                        .map_err(compiler_diagnostic);
                }
                for (index, element_pattern) in elements.iter().enumerate() {
                    let element = self
                        .backend
                        .builder
                        .build_extract_value(product_value, index as u32, "match.element")
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                    let next = if index + 1 == elements.len() {
                        success
                    } else {
                        self.backend.context.append_basic_block(
                            success.get_parent().ok_or_else(|| {
                                Diagnostic::new(
                                    staple_syntax::Span::Compiler,
                                    "internal invariant violated: match block belongs to an LLVM function",
                                )
                            })?,
                            "match.pattern",
                        )
                    };
                    self.emit_match_pattern_branch(
                        owner,
                        *element_pattern,
                        element,
                        next,
                        failure,
                        environment,
                    )?;
                    if next != success {
                        self.backend.builder.position_at_end(next);
                    }
                }
                Ok(())
            }
            crate::LoweredPatternKind::Nominal { argument, .. } => match &subject {
                CheckedType::String
                    if pattern.test.identity == crate::LoweredPatternIdentity::String =>
                {
                    self.emit_match_pattern_branch(
                        owner,
                        *argument,
                        value,
                        success,
                        failure,
                        environment,
                    )
                }
                CheckedType::Ref(payload)
                    if pattern.test.identity == crate::LoweredPatternIdentity::Ref =>
                {
                    let payload_value = self.backend.load_ref_payloads(
                        value.as_any_value_enum(),
                        std::slice::from_ref(payload),
                        span.clone(),
                    )?;
                    self.emit_match_pattern_branch(
                        owner,
                        *argument,
                        payload_value,
                        success,
                        failure,
                        environment,
                    )
                }
                CheckedType::Sum(sum) => {
                    let Some(index) = pattern.test.sum_alternative else {
                        return Err(Diagnostic::new(
                            span.clone(),
                            "match pattern does not select a sum alternative",
                        ));
                    };
                    let Some(BasicValueEnum::StructValue(sum_value)) = value_as_basic(value.into())
                    else {
                        return Err(Diagnostic::new(
                            span.clone(),
                            "sum match value has an invalid representation",
                        ));
                    };
                    let tag = self.backend.build_sum_tag(sum_value, "match.tag")?;
                    let selected = self.backend.context.append_basic_block(
                        success.get_parent().ok_or_else(|| {
                            Diagnostic::new(
                                staple_syntax::Span::Compiler,
                                "internal invariant violated: match block belongs to an LLVM function",
                            )
                        })?,
                        "match.selected",
                    );
                    let matches =
                        self.backend
                            .build_sum_tag_compare(tag, index, "match.tag.matches")?;
                    self.backend
                        .builder
                        .build_conditional_branch(matches, selected, failure)
                        .map(|_| ())
                        .map_err(compiler_diagnostic)?;
                    self.backend.builder.position_at_end(selected);
                    let payload = self.backend.extract_sum_alternative(
                        sum_value,
                        sum,
                        index,
                        span.clone(),
                    )?;
                    self.emit_match_pattern_branch(
                        owner,
                        *argument,
                        payload,
                        success,
                        failure,
                        environment,
                    )
                }
                CheckedType::Distinct { .. }
                    if pattern.test.identity == crate::LoweredPatternIdentity::Representation =>
                {
                    self.emit_match_pattern_branch(
                        owner,
                        *argument,
                        value,
                        success,
                        failure,
                        environment,
                    )
                }
                _ => Err(Diagnostic::new(
                    span.clone(),
                    "checked nominal pattern has an incompatible value",
                )),
            },
            crate::LoweredPatternKind::Wildcard => {
                self.bind_pattern(owner, pattern_id, value.as_any_value_enum(), environment)?;
                self.backend
                    .builder
                    .build_unconditional_branch(success)
                    .map(|_| ())
                    .map_err(compiler_diagnostic)
            }
        }
    }

    /// Emits short-circuiting logical operators with Bool tag tests and a value phi.
    fn emit_logical(
        &mut self,
        owner: EmissionOwner,
        expression: &crate::LoweredExpression,
        logical: &crate::LoweredLogical,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = expression.origin.span.clone();
        let left = self.emit_expression(owner, logical.left, environment)?;
        if environment.returned {
            return Ok(left);
        }
        let Some(left_value) = value_as_basic(left) else {
            return Err(Diagnostic::new(
                span.clone(),
                "logical operand is not first-class",
            ));
        };
        let BasicValueEnum::StructValue(left_struct) = left_value else {
            return Err(Diagnostic::new(
                span.clone(),
                "`Bool` value has an invalid representation",
            ));
        };
        if !matches!(logical.bool_type, CheckedType::Sum(_)) {
            return Err(Diagnostic::new(
                span.clone(),
                "`&&`/`||` require `Bool` to be a sum type",
            ));
        }
        let tag = self.backend.build_sum_tag(left_struct, "logical.tag")?;
        let is_true =
            self.backend
                .build_sum_tag_compare(tag, logical.true_index, "logical.is_true")?;
        let function = self
            .backend
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "`&&`/`||` is not in a function"))?;
        let merge_block = self
            .backend
            .context
            .append_basic_block(function, "logical.merge");
        let right_block = self
            .backend
            .context
            .append_basic_block(function, "logical.right");
        let short_circuit_block = self
            .backend
            .context
            .append_basic_block(function, "logical.short_circuit");
        let (true_target, false_target) = match logical.operator {
            staple_syntax::LogicalOperator::And => (right_block, short_circuit_block),
            staple_syntax::LogicalOperator::Or => (short_circuit_block, right_block),
        };
        self.backend
            .builder
            .build_conditional_branch(is_true, true_target, false_target)
            .map(|_| ())
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(short_circuit_block);
        self.backend
            .builder
            .build_unconditional_branch(merge_block)
            .map(|_| ())
            .map_err(compiler_diagnostic)?;
        let mut incoming = vec![(left_value, short_circuit_block)];
        self.backend.builder.position_at_end(right_block);
        let owned_before = environment.owned_order.len();
        environment.returned = false;
        let right = self.emit_expression(owner, logical.right, environment)?;
        if !environment.returned {
            let right_value = value_as_basic(right).ok_or_else(|| {
                Diagnostic::new(span.clone(), "logical operand is not first-class")
            })?;
            // compile logical expression drops the right operand's
            // own bindings at the end of its block.
            self.drop_owned_since(environment, owned_before, &span)?;
            self.backend
                .builder
                .build_unconditional_branch(merge_block)
                .map_err(compiler_diagnostic)?;
            let predecessor = self.backend.builder.get_insert_block().ok_or_else(|| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "internal invariant violated: continuing logical operand has a current LLVM block",
                )
            })?;
            incoming.push((right_value, predecessor));
        } else {
            Self::forget_owned_since(environment, owned_before);
        }
        self.backend.builder.position_at_end(merge_block);
        environment.returned = false;
        let result_type = self.backend.compile_type(&logical.bool_type)?;
        Ok(self
            .backend
            .build_phi_value(result_type, &incoming, "logical.value")?
            .as_any_value_enum())
    }

    /// Tests a propagating binding's success tag and returns its failure through
    /// the recorded residual coercion. Binds the success payload and any at bindings.
    fn emit_propagating_binding(
        &mut self,
        owner: EmissionOwner,
        binding: &crate::LoweredPatternBindingItem,
        value: BasicValueEnum<'context>,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let span = self
            .view
            .pattern(owner, binding.pattern)
            .map(|pattern| pattern.origin.span.clone())
            .unwrap_or(staple_syntax::Span::Compiler);
        let Some(propagation) = &binding.propagation else {
            return Err(Diagnostic::new(span, "missing checked propagation"));
        };
        let CheckedType::Sum(source_sum) = &propagation.source else {
            return Err(Diagnostic::new(
                span.clone(),
                "propagation source is not a sum",
            ));
        };
        let BasicValueEnum::StructValue(sum_value) = value else {
            return Err(Diagnostic::new(
                span.clone(),
                "propagation source has an invalid representation",
            ));
        };
        let tag = self.backend.build_sum_tag(sum_value, "propagate.tag")?;
        let success = self.backend.build_sum_tag_compare(
            tag,
            propagation.success_index,
            "propagate.success",
        )?;
        let function = self
            .backend
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "propagation is not inside a function"))?;
        let success_block = self
            .backend
            .context
            .append_basic_block(function, "propagate.ok");
        let failure_block = self
            .backend
            .context
            .append_basic_block(function, "propagate.return");
        self.backend
            .builder
            .build_conditional_branch(success, success_block, failure_block)
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(failure_block);
        let failure_value = if propagation.source == propagation.result {
            sum_value.as_any_value_enum()
        } else if matches!(propagation.result, CheckedType::Sum(_)) {
            let Some(plan) = &binding.propagation_plan else {
                return Err(Diagnostic::new(
                    span.clone(),
                    "propagation coercion has no emission plan",
                ));
            };
            self.emit_coercion(
                sum_value.as_any_value_enum(),
                &propagation.source,
                &propagation.result,
                plan,
                &span,
            )?
        } else {
            // lowering records the residual alternative; the emitter
            // never selects one by comparing types.
            let index = binding.propagation_residual.ok_or_else(|| {
                Diagnostic::new(
                    span.clone(),
                    "propagated result is missing its residual variant",
                )
            })?;
            self.backend
                .extract_sum_alternative(sum_value, source_sum, index, span.clone())?
                .as_any_value_enum()
        };
        let failure_value = value_as_basic(failure_value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "propagated result is not first-class"))?;
        // compile propagating binding's failure path drops every
        // owned binding before returning the residual value.
        self.drop_all_owned(environment, &span)?;
        self.backend
            .builder
            .build_return(Some(&failure_value))
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(success_block);
        let success_value = self.backend.extract_sum_alternative(
            sum_value,
            source_sum,
            propagation.success_index,
            span.clone(),
        )?;
        let mut root = binding.pattern;
        loop {
            let Some(record) = self.view.pattern(owner, root).cloned() else {
                return Err(Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    "missing lowered pattern",
                ));
            };
            match record.kind {
                crate::LoweredPatternKind::At {
                    binding: at_binding,
                    pattern,
                } => {
                    self.bind_pattern(
                        owner,
                        at_binding,
                        sum_value.as_any_value_enum(),
                        environment,
                    )?;
                    root = pattern;
                }
                crate::LoweredPatternKind::Nominal { argument, .. } => {
                    return self.bind_pattern(
                        owner,
                        argument,
                        success_value.as_any_value_enum(),
                        environment,
                    );
                }
                _ => {
                    return Err(Diagnostic::new(
                        span,
                        "checked propagating pattern is nominal",
                    ));
                }
            }
        }
    }

    /// Builds a first-class callable using its recorded environment route. Fresh
    /// captures build an environment; Stored loads an existing closure; Current
    /// reuses the enclosing environment. Adapters use a null environment.
    fn emit_callable_value(
        &mut self,
        owner: EmissionOwner,
        id: LoweredCallableValueId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let callable = self.view.callable_value(owner, id).ok_or_else(|| {
            Diagnostic::new(
                staple_syntax::Span::Compiler,
                "missing lowered callable value",
            )
        })?;
        let unsupported = |family| {
            Diagnostic::new(
                callable.origin.span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            )
        };
        // compile symbol value runs the symbol's initialization check
        // before producing the closure value.
        if callable.requires_initialization_check
            && let Some(symbol) = self.callable_symbol(callable)
        {
            self.check_symbol_initialization(owner, environment, symbol, &callable.origin.span)?;
        }
        let binding = self
            .view
            .binding(owner, LoweredBindingSite::CallableValue(id));
        // Constructor and extern adapter values use the declared adapter
        // artifact (ensure constructor adapter and `closure_codes`);
        // both carry a null environment.
        if matches!(
            callable.adapter,
            LoweredCallableAdapter::Constructor | LoweredCallableAdapter::External
        ) {
            // compile symbol value resolves a parameter pointer, a
            // local (including a captured closure value), or a binding cell
            // before it falls back to the adapter code: a thunk that captures
            // an extern value calls the captured closure, not a rebuilt one.
            if let Some(symbol) = self.callable_symbol(callable)
                && (environment.parameter_pointers.contains_key(&symbol)
                    || environment.locals.contains_key(&symbol)
                    || environment.binding_cells.contains_key(&symbol))
            {
                let value_type = CheckedType::Function(callable.function_type.clone());
                let span = callable.origin.span.clone();
                return self.load_symbol_value(
                    owner,
                    symbol,
                    false,
                    &value_type,
                    &span,
                    environment,
                );
            }
            let code = if callable.adapter == LoweredCallableAdapter::External {
                // An extern value keeps a `Route` binding; its adapter is
                // named by the `ExternAdapterValue` artifact use.
                self.extern_adapter_code(owner, id)
                    .ok_or_else(|| unsupported("callable adapter binding"))?
            } else {
                self.callable_artifact_code(
                    binding,
                    &callable.origin.span,
                    "callable adapter binding",
                )?
            };
            return self
                .backend
                .build_closure_value(
                    code,
                    self.backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                )
                .map(|closure| closure.as_any_value_enum());
        }
        let code = match &callable.target {
            LoweredCallableTarget::DirectFunction { .. }
            | LoweredCallableTarget::TraitImplementation { .. } => {
                let Some(LoweredBoundTarget::Instance(instance)) = binding else {
                    return Err(unsupported("callable instance binding"));
                };
                self.instances
                    .get(instance)
                    .copied()
                    .ok_or_else(|| unsupported("callable instance declaration"))?
            }
            LoweredCallableTarget::Constructor { .. }
            | LoweredCallableTarget::StructuralTraitMethod { .. }
            | LoweredCallableTarget::ExternalFunction { .. } => self.callable_artifact_code(
                binding,
                &callable.origin.span,
                "callable artifact binding",
            )?,
            LoweredCallableTarget::IndirectClosure { callee } => {
                return self.emit_expression(owner, *callee, environment);
            }
            LoweredCallableTarget::Intrinsic { .. } => {
                return Err(unsupported("intrinsic callable value"));
            }
        };
        let pointer = match &callable.closure {
            Some(closure) => match closure.environment {
                LoweredClosureEnvironment::Fresh => {
                    let pointer = self.build_closure_environment_value(
                        owner,
                        closure,
                        environment,
                        &callable.origin.span,
                    )?;
                    // The emitter installs the closure-environment finalizer exactly
                    // when the recorded use exists.
                    if let Some(finalizer) = self.closure_environment_finalizer(owner, id) {
                        self.backend.set_gc_finalizer(pointer, finalizer)?;
                    }
                    pointer
                }
                LoweredClosureEnvironment::Stored => {
                    return self.load_stored_closure(owner, callable, environment);
                }
                LoweredClosureEnvironment::Current => environment
                    .closure_environment
                    .ok_or_else(|| unsupported("current closure environment"))?,
                LoweredClosureEnvironment::None => self
                    .backend
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null(),
            },
            None => self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null(),
        };
        self.backend
            .build_closure_value(code, pointer)
            .map(|closure| closure.as_any_value_enum())
    }

    /// The catalog symbol whose initialization state a callable value checks.
    fn callable_symbol(&self, callable: &crate::LoweredCallableValue) -> Option<SymbolId> {
        match &callable.target {
            LoweredCallableTarget::DirectFunction { function, .. } => {
                self.function_symbols.get(function).copied()
            }
            LoweredCallableTarget::TraitImplementation {
                function: Some(function),
                ..
            } => self.function_symbols.get(function).copied(),
            LoweredCallableTarget::ExternalFunction { symbol }
            | LoweredCallableTarget::Constructor { symbol, .. }
            | LoweredCallableTarget::Intrinsic { symbol, .. } => Some(*symbol),
            _ => callable
                .closure
                .as_ref()
                .and_then(|closure| self.function_symbols.get(&closure.function).copied()),
        }
    }

    /// The declared adapter of one extern callable value, named by the
    /// value's `ExternAdapterValue` artifact use (the binding stays a
    /// `Route`, because the foreign symbol keeps its non-source route).
    fn extern_adapter_code(
        &self,
        owner: EmissionOwner,
        id: LoweredCallableValueId,
    ) -> Option<FunctionValue<'context>> {
        let uses = self.view.artifact_uses(owner)?;
        let ordinal = uses.iter().find_map(|use_| {
            (use_.site == crate::ArtifactUseSite::ExternAdapterValue(id)).then_some(use_.artifact)
        })?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
    }

    /// The declared function of one callable value's artifact binding.
    fn callable_artifact_code(
        &self,
        binding: Option<&LoweredBoundTarget>,
        span: &staple_syntax::Span,
        family: &str,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let Some(LoweredBoundTarget::Artifact(ordinal)) = binding else {
            return Err(Diagnostic::new(
                span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            ));
        };
        self.artifacts
            .get(ordinal)
            .and_then(|values| values.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing callable artifact declaration"))
    }

    /// Loads an existing closure from the function symbol's local, cell, or global.
    fn load_stored_closure(
        &mut self,
        owner: EmissionOwner,
        callable: &crate::LoweredCallableValue,
        environment: &FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let unsupported = |family| {
            Diagnostic::new(
                callable.origin.span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            )
        };
        let LoweredCallableTarget::DirectFunction { function, .. } = &callable.target else {
            return Err(unsupported("stored closure"));
        };
        let Some(symbol) = self.function_symbols.get(function).copied() else {
            return Err(unsupported("stored closure storage"));
        };
        let closure_type = self.backend.closure_type();
        if let Some(value) = environment.locals.get(&symbol) {
            return Ok(*value);
        }
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            let cell_type = self.binding_cell_type(owner, symbol)?;
            if !callable.requires_initialization_check {
                // The emitter's compile symbol value builds the state slot even
                // when the read needs no check; the caller already emitted
                // the check when one is required.
                self.backend
                    .builder
                    .build_struct_gep(cell_type, cell, 1, "binding.state")
                    .map_err(compiler_diagnostic)?;
            }
            let slot = self
                .backend
                .builder
                .build_struct_gep(cell_type, cell, 0, "binding.value")
                .map_err(compiler_diagnostic)?;
            return self
                .backend
                .builder
                .build_load(closure_type, slot, "binding")
                .map(|value| value.as_any_value_enum())
                .map_err(compiler_diagnostic);
        }
        let Some(global) = self.storage.get(&symbol).copied() else {
            return Err(unsupported("stored closure storage"));
        };
        self.backend
            .builder
            .build_load(closure_type, global.as_pointer_value(), "global")
            .map(|value| value.as_any_value_enum())
            .map_err(compiler_diagnostic)
    }

    /// The declared closure-environment finalizer of one callable value, named
    /// by the value's `ClosureEnvironment` artifact use. A closure with no
    /// droppable capture has no use.
    fn closure_environment_finalizer(
        &self,
        owner: EmissionOwner,
        id: LoweredCallableValueId,
    ) -> Option<FunctionValue<'context>> {
        self.site_finalizer(owner, crate::ArtifactUseSite::ClosureEnvironment(id))
    }

    /// The declared finalizer function one artifact use site names, if the
    /// owner recorded that use. Lowering decides whether a finalizer is
    /// installed; the emitter only reads the use.
    fn site_finalizer(
        &self,
        owner: EmissionOwner,
        site: crate::ArtifactUseSite,
    ) -> Option<FunctionValue<'context>> {
        let uses = self.view.artifact_uses(owner)?;
        let ordinal = uses
            .iter()
            .find_map(|use_| (use_.site == site).then_some(use_.artifact))?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
    }

    /// Builds a closure plan's captures from the current scope. Empty captures
    /// produce a null environment pointer.
    fn build_closure_environment_value(
        &mut self,
        owner: EmissionOwner,
        closure: &crate::LoweredClosureConstruction,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        if closure.captures.is_empty() {
            return Ok(self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null());
        }
        let fields = closure
            .captures
            .iter()
            .map(|capture| {
                if capture.access == crate::LoweredCaptureAccess::ByValue {
                    self.backend.compile_type(&capture.value_type)
                } else {
                    Ok(self
                        .backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .into())
                }
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let environment_type = self.backend.capture_environment_type(&fields);
        let mut environment_value = environment_type.const_zero();
        for (index, capture) in closure.captures.iter().enumerate() {
            let stored = self.closure_capture_value(owner, capture, environment, span)?;
            environment_value = self.backend.insert_capture(
                environment_value,
                stored,
                index as u32,
                span.clone(),
            )?;
        }
        self.backend
            .allocate_capture_environment(environment_type, environment_value, span.clone())
    }

    /// Reads a capture from the current scope. Shared cells and borrowed values
    /// store pointers; by-value captures store the value itself.
    fn closure_capture_value(
        &mut self,
        owner: EmissionOwner,
        capture: &crate::LoweredClosureCapture,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        use crate::LoweredCaptureAccess;
        let symbol = capture.capture.symbol;
        let pointer = match capture.access {
            LoweredCaptureAccess::SharedCell => environment
                .parameter_pointers
                .get(&symbol)
                .copied()
                .or_else(|| environment.binding_cells.get(&symbol).copied()),
            LoweredCaptureAccess::Borrowed => environment.parameter_pointers.get(&symbol).copied(),
            LoweredCaptureAccess::ByValue => None,
        };
        match capture.access {
            LoweredCaptureAccess::SharedCell | LoweredCaptureAccess::Borrowed => {
                let pointer = pointer.ok_or_else(|| {
                    Diagnostic::new(span.clone(), "closure capture storage is not available")
                })?;
                Ok(pointer.into())
            }
            LoweredCaptureAccess::ByValue => {
                let value = self.load_symbol_value(
                    owner,
                    symbol,
                    false,
                    &capture.value_type,
                    span,
                    environment,
                )?;
                value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "captured value is not first-class")
                })
            }
        }
    }

    fn emit_call(
        &mut self,
        owner: EmissionOwner,
        id: LoweredCallId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let call = self
            .view
            .call(owner, id)
            .ok_or_else(|| Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered call"))?
            .clone();
        let unsupported = |family| {
            Diagnostic::new(
                call.origin.span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            )
        };
        let native_extern = matches!(call.target, LoweredCallableTarget::ExternalFunction { .. });
        // compile intrinsic and the extern route
        // evaluate arguments through compile arguments (by value), never
        // through an ABI pass mode, so the lowered records' pass modes are
        // ignored there. (A variadic extern's extra parameter slots can even
        // record an indirect mode for the variadic tail, which the emitter never
        // materializes.)
        let by_value_route =
            matches!(call.target, LoweredCallableTarget::Intrinsic { .. }) || native_extern;
        // a reactive intrinsic call names the operation it
        // performs. The plain scope call emits through the intrinsic route
        // (its unit argument is evaluated with the other arguments below);
        // every other operation evaluates its own operands in the emitter order and
        // is emitted before the generic argument loop.
        let reactive_call = if let Some(reactive) = call.reactive {
            let operation = self
                .view
                .reactive_operation(owner, reactive)
                .ok_or_else(|| {
                    Diagnostic::new(call.origin.span.clone(), "missing reactive operation")
                })?;
            match &operation.kind {
                LoweredReactiveOperationKind::Scope => None,
                LoweredReactiveOperationKind::Reaction { .. }
                | LoweredReactiveOperationKind::Batch { .. }
                | LoweredReactiveOperationKind::Until { .. }
                | LoweredReactiveOperationKind::Snapshot => Some(reactive),
                other => {
                    return Err(Diagnostic::new(
                        call.origin.span.clone(),
                        format!("invalid reactive call record: {other:?}"),
                    ));
                }
            }
        } else {
            None
        };
        if let Some(reactive) = reactive_call {
            return self.emit_reactive_call(owner, &call, reactive, environment);
        }
        // The emitter checks the callee symbol's initialization before evaluating
        // any argument.
        for symbol in &call.initialization_checks {
            self.check_symbol_initialization(owner, environment, *symbol, &call.origin.span)?;
        }
        // A whole-mutation call passes one pointer whatever the logical
        // parameter's flattened arity (`compile_closure_function_type`).
        let mut parameter_count = if call
            .function_type
            .mutations
            .contains(&CheckedMutation::Whole)
        {
            1
        } else {
            flattened_parameter_types(&call.function_type.parameter).len()
        };
        let variadic = matches!(
            call.function_type.parameter.as_ref(),
            CheckedType::Product(product) if product.variadic
        );
        if variadic {
            for step in &call.steps {
                let slot = match step {
                    LoweredCallStep::Argument { argument } => call
                        .arguments
                        .get(*argument)
                        .and_then(|argument| argument.slot),
                    LoweredCallStep::ProductElement { slot, .. }
                    | LoweredCallStep::Default { slot, .. } => Some(*slot),
                    LoweredCallStep::ProductSpread { mappings, .. } => {
                        mappings.iter().map(|mapping| mapping.slot).max()
                    }
                    LoweredCallStep::NamedProductSpread { mappings, .. } => {
                        mappings.iter().map(|mapping| mapping.slot).max()
                    }
                    LoweredCallStep::Callee { .. }
                    | LoweredCallStep::Resource { .. }
                    | LoweredCallStep::Invoke => None,
                };
                if let Some(slot) = slot {
                    parameter_count = parameter_count.max(slot + 1);
                }
            }
        }
        // compile call expression's constructor branch compiles its
        // single argument whole and never runs the flattened ABI argument
        // path, so a constructor's slot count is its record count.
        if matches!(call.target, LoweredCallableTarget::Constructor { .. }) {
            parameter_count = call.arguments.len();
        }
        let mut slots: Vec<Option<BasicMetadataValueEnum<'context>>> = vec![None; parameter_count];
        // Hidden effect-row resource arguments, in row order. The emitter evaluates
        // its visible arguments first and appends the hidden ones, then passes
        // `[environment, hidden..., visible...]` (compile resource arguments).
        let mut hidden: Vec<BasicMetadataValueEnum<'context>> = Vec::new();
        // Mutation temporaries whose value needs drop after the call, in
        // evaluation order with their argument record index; `emit_call_cleanup`
        // drops them in reverse.
        let mut cleanups: Vec<(usize, PointerValue<'context>)> = Vec::new();
        let mut invoked = false;
        // A whole-product argument against a flattened multi-element parameter
        // records no ABI slot; compile arguments unpacks the single
        // struct after evaluating it. These values are placed at the end.
        let mut unplaced: Vec<BasicMetadataValueEnum<'context>> = Vec::new();
        let mut callee_value = None;
        // The emitter extracts `closure.code`/`closure.environment` after the
        // visible arguments and before the hidden resources; the parts are
        // materialized lazily at the first resource step (or after the loop).
        let mut callee_parts: Option<(PointerValue<'context>, PointerValue<'context>)> = None;
        for step in &call.steps {
            // An argument may return from the enclosing function. Do not
            // evaluate another step or invoke the callee after its terminator.
            if environment.returned {
                return Ok(self.backend.unit_value());
            }
            match step {
                LoweredCallStep::Invoke => {
                    invoked = true;
                    break;
                }
                LoweredCallStep::Callee { expression } => {
                    let value = self.emit_expression(owner, *expression, environment)?;
                    if environment.returned {
                        return Ok(self.backend.unit_value());
                    }
                    let AnyValueEnum::StructValue(closure) = value else {
                        return Err(unsupported("indirect closure value"));
                    };
                    callee_value = Some(closure);
                    continue;
                }
                LoweredCallStep::Argument { argument } => {
                    let record = call
                        .arguments
                        .get(*argument)
                        .ok_or_else(|| unsupported("call argument"))?;
                    let value = self.assemble_call_argument(
                        owner,
                        id,
                        *argument,
                        record,
                        record.expression,
                        by_value_route,
                        environment,
                        &mut cleanups,
                        &call.origin.span,
                    )?;
                    match record.slot {
                        Some(slot) => {
                            place_argument_slot(&mut slots, slot, value, &call.origin.span)?
                        }
                        None => unplaced.push(value),
                    }
                }
                LoweredCallStep::ProductElement {
                    argument,
                    slot,
                    expression,
                } => {
                    let record = call
                        .arguments
                        .get(*argument)
                        .ok_or_else(|| unsupported("product argument"))?;
                    let value = self.assemble_call_argument(
                        owner,
                        id,
                        *argument,
                        record,
                        Some(*expression),
                        by_value_route,
                        environment,
                        &mut cleanups,
                        &call.origin.span,
                    )?;
                    place_argument_slot(&mut slots, *slot, value, &call.origin.span)?;
                }
                LoweredCallStep::ProductSpread {
                    expression,
                    mappings,
                    ..
                } => {
                    let mappings = mappings
                        .iter()
                        .map(|mapping| (mapping.source, mapping.slot))
                        .collect::<Vec<_>>();
                    self.emit_spread_arguments(
                        owner,
                        &call,
                        *expression,
                        &mappings,
                        by_value_route,
                        environment,
                        &mut cleanups,
                        &mut slots,
                    )?;
                }
                LoweredCallStep::NamedProductSpread {
                    expression,
                    mappings,
                    ..
                } => {
                    let mappings = mappings
                        .iter()
                        .map(|mapping| (mapping.source, mapping.slot))
                        .collect::<Vec<_>>();
                    self.emit_spread_arguments(
                        owner,
                        &call,
                        *expression,
                        &mappings,
                        by_value_route,
                        environment,
                        &mut cleanups,
                        &mut slots,
                    )?;
                }
                LoweredCallStep::Default { argument, slot, .. } => {
                    let record = call
                        .arguments
                        .get(*argument)
                        .ok_or_else(|| unsupported("default argument"))?;
                    let value = self.assemble_call_argument(
                        owner,
                        id,
                        *argument,
                        record,
                        record.expression,
                        by_value_route,
                        environment,
                        &mut cleanups,
                        &call.origin.span,
                    )?;
                    place_argument_slot(&mut slots, *slot, value, &call.origin.span)?;
                }
                LoweredCallStep::Resource { resource } => {
                    // Intrinsic resources are activation metadata, not hidden
                    // ABI arguments. The intrinsic emitter reads the records.
                    if matches!(call.target, LoweredCallableTarget::Intrinsic { .. }) {
                        continue;
                    }
                    self.ensure_callee_parts(&callee_value, &mut callee_parts)?;
                    hidden.push(self.emit_hidden_resource_argument(
                        owner,
                        &call,
                        *resource,
                        environment,
                    )?);
                }
            }
        }
        // compile arguments' whole-product fallback: one argument
        // value against a flattened multi-element parameter unpacks the
        // struct, and against a single slot it is that slot's value.
        if slots.iter().all(Option::is_none) && unplaced.len() == 1 {
            let value = unplaced[0];
            if parameter_count == 1 {
                slots[0] = Some(value);
            } else if let BasicMetadataValueEnum::StructValue(product) = value
                && product.get_type().count_fields() as usize == parameter_count
            {
                for index in 0..parameter_count {
                    slots[index] = Some(
                        self.backend
                            .builder
                            .build_extract_value(product, index as u32, "argument.element")
                            .map_err(compiler_diagnostic)?
                            .into(),
                    );
                }
            }
        }
        if !invoked || slots.iter().any(Option::is_none) {
            return Err(unsupported("incomplete call"));
        }
        self.ensure_callee_parts(&callee_value, &mut callee_parts)?;
        let mut values = hidden;
        values.extend(slots.into_iter().map(Option::unwrap));
        // A C-string temporary is the first visible argument (the emitter's
        // `scoped_c_string_temporary` check). The direct extern route passes
        // the CString value itself; a closure route passes a pointer to the
        // borrowed CString slot , so the temporary's value is
        // loaded before it is released.
        let cleanup_c_string = if call.c_string_temporary {
            match values.first() {
                Some(BasicMetadataValueEnum::PointerValue(pointer)) if native_extern => {
                    Some(*pointer)
                }
                Some(BasicMetadataValueEnum::PointerValue(pointer)) => Some(
                    self.backend
                        .builder
                        .build_load(
                            self.backend.compile_type(&CheckedType::CString)?,
                            *pointer,
                            "c_string.temporary",
                        )
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value(),
                ),
                _ => None,
            }
        } else {
            None
        };
        let binding = self
            .view
            .binding(owner, LoweredBindingSite::Call(id))
            .ok_or_else(|| unsupported("call binding"))?;
        let result = match &call.target {
            LoweredCallableTarget::DirectFunction {
                environment: route, ..
            } => {
                let LoweredBoundTarget::Instance(instance) = binding else {
                    return Err(unsupported("direct call binding"));
                };
                let function = self
                    .instances
                    .get(instance)
                    .copied()
                    .ok_or_else(|| unsupported("direct function declaration"))?;
                let pointer = match route {
                    LoweredCallEnvironment::None => self
                        .backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    LoweredCallEnvironment::Current => environment
                        .closure_environment
                        .ok_or_else(|| unsupported("current closure environment"))?,
                };
                let mut arguments = vec![pointer.into()];
                arguments.extend(values);
                let result = self
                    .backend
                    .builder
                    .build_direct_call(function, &arguments, "call")
                    .map_err(compiler_diagnostic)?;
                Ok(result.try_as_basic_value().basic().map_or_else(
                    || self.backend.unit_value(),
                    |value| value.as_any_value_enum(),
                ))
            }
            LoweredCallableTarget::Intrinsic { intrinsic, .. } => {
                if !matches!(binding, LoweredBoundTarget::Route(_)) {
                    return Err(unsupported("intrinsic call binding"));
                }
                self.emit_intrinsic(owner, &call, id, *intrinsic, &values, environment)
            }
            LoweredCallableTarget::IndirectClosure { .. } => {
                if !matches!(binding, LoweredBoundTarget::Route(_)) {
                    return Err(unsupported("indirect call binding"));
                }
                let (code, pointer) = callee_parts.ok_or_else(|| unsupported("indirect callee"))?;
                let mut arguments = vec![pointer.into()];
                arguments.extend(values);
                let signature = self
                    .backend
                    .compile_closure_function_type(&call.function_type)?;
                let result = self
                    .backend
                    .builder
                    .build_indirect_call(signature, code, &arguments, "closure.call")
                    .map_err(compiler_diagnostic)?;
                Ok(result.try_as_basic_value().basic().map_or_else(
                    || self.backend.unit_value(),
                    |value| value.as_any_value_enum(),
                ))
            }
            LoweredCallableTarget::ExternalFunction { symbol } => {
                if !matches!(binding, LoweredBoundTarget::Route(_)) {
                    return Err(unsupported("extern call binding"));
                }
                let function = self
                    .externs
                    .get(symbol)
                    .copied()
                    .ok_or_else(|| unsupported("foreign symbol declaration"))?;
                let result = self
                    .backend
                    .builder
                    .build_direct_call(function, &values, "extern.call")
                    .map_err(compiler_diagnostic)?;
                Ok(result.try_as_basic_value().basic().map_or_else(
                    || self.backend.unit_value(),
                    |value| value.as_any_value_enum(),
                ))
            }
            LoweredCallableTarget::Constructor { recursive, .. } => {
                let basic = values
                    .iter()
                    .map(|value| {
                        BasicValueEnum::try_from(*value).map_err(|_| {
                            Diagnostic::new(
                                call.origin.span.clone(),
                                "constructor argument is not first-class",
                            )
                        })
                    })
                    .collect::<CodeGenerationResult<Vec<_>>>()?;
                if matches!(
                    recursive,
                    Some(crate::RecursiveConstruction::ManagedReference)
                ) {
                    return self.emit_managed_ref(owner, id, &call, &basic);
                }
                self.backend
                    .build_product_value(&basic, call.origin.span.clone())
                    .map(|value| value.as_any_value_enum())
            }
            // Trait implementation selection is already closed into a catalog binding.
            LoweredCallableTarget::TraitImplementation { .. }
            | LoweredCallableTarget::StructuralTraitMethod { .. } => {
                let function =
                    self.bound_function(owner, LoweredBindingSite::Call(id), &call.origin.span)?;
                self.emit_structural_call(owner, &call, function, &values)
            }
        };
        let value = result?;
        self.emit_call_cleanup(owner, id, &cleanups, cleanup_c_string, &call.origin.span)?;
        Ok(value)
    }

    /// The post-call cleanup hook. The emitter
    /// drop mutation temporaries drops mutation temporaries in reverse
    /// collection order and then releases a C-string temporary; each drop
    /// expands the glue named by its own `CallTemporary`, or `CStringTemporary`,
    /// use record.
    fn emit_call_cleanup(
        &self,
        owner: EmissionOwner,
        call_id: LoweredCallId,
        temporaries: &[(usize, PointerValue<'context>)],
        c_string: Option<PointerValue<'context>>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        for (argument, pointer) in temporaries.iter().rev() {
            self.emit_drop_site(
                owner,
                crate::ArtifactUseSite::CallTemporary {
                    call: call_id,
                    argument: *argument,
                },
                DropSource::Temporary(*pointer),
                span,
            )?;
        }
        if let Some(pointer) = c_string {
            self.emit_drop_site(
                owner,
                crate::ArtifactUseSite::CStringTemporary(call_id),
                DropSource::Value(pointer.into()),
                span,
            )?;
        }
        Ok(())
    }

    /// Compile call expression's trait/structural branch: a direct
    /// call with a null environment (`trait.call`).
    fn emit_structural_call(
        &mut self,
        _owner: EmissionOwner,
        call: &crate::LoweredCall,
        function: FunctionValue<'context>,
        values: &[BasicMetadataValueEnum<'context>],
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let mut arguments = vec![
            self.backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null()
                .into(),
        ];
        arguments.extend(values.iter().copied());
        let result = self
            .backend
            .builder
            .build_direct_call(function, &arguments, "trait.call")
            .map_err(|error| Diagnostic::new(call.origin.span.clone(), error.to_string()))?;
        Ok(result.try_as_basic_value().basic().map_or_else(
            || self.backend.unit_value(),
            |value| value.as_any_value_enum(),
        ))
    }

    /// A `ManagedRef` constructor: GC-allocate the payload, store it, and set
    /// the declared payload finalizer from the call's `RefConstruction`
    /// artifact use.
    fn emit_managed_ref(
        &mut self,
        owner: EmissionOwner,
        id: LoweredCallId,
        call: &crate::LoweredCall,
        values: &[BasicValueEnum<'context>],
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let CheckedType::Ref(payload) = &call.result_type else {
            return Err(Diagnostic::new(
                call.origin.span.clone(),
                "Ref constructor has an invalid result type",
            ));
        };
        let value = self
            .backend
            .build_product_value(values, call.origin.span.clone())?;
        let payload_type = self.backend.compile_type(payload)?;
        let size = self.backend.target_data.get_store_size(&payload_type);
        let pointer = self.backend.build_gc_allocation(
            self.backend.size_type.const_int(size, false),
            "ref.allocate",
            call.origin.span.clone(),
        )?;
        self.backend
            .builder
            .build_store(pointer, value)
            .map_err(compiler_diagnostic)?;
        if let Some(finalizer) = self.ref_construction_finalizer(owner, id) {
            self.backend.set_gc_finalizer(pointer, finalizer)?;
        }
        Ok(pointer.as_any_value_enum())
    }

    /// The declared payload finalizer for one `Ref` construction, named by the
    /// call's `RefConstruction` artifact use. A payload that needs no
    /// finalizer has no use.
    fn ref_construction_finalizer(
        &self,
        owner: EmissionOwner,
        id: LoweredCallId,
    ) -> Option<FunctionValue<'context>> {
        let uses = self.view.artifact_uses(owner)?;
        let ordinal = uses.iter().find_map(|use_| {
            (use_.site == crate::ArtifactUseSite::RefConstruction(id)).then_some(use_.artifact)
        })?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
    }

    /// Check the symbol's binding cell
    /// state when it has a cell, else the module global's state when it has
    /// one, and do nothing otherwise.
    fn check_symbol_initialization(
        &self,
        owner: EmissionOwner,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            let state = if self.view.initialization_state_only(owner, symbol) {
                cell
            } else {
                let cell_type = self.binding_cell_type(owner, symbol)?;
                self.backend
                    .builder
                    .build_struct_gep(cell_type, cell, 1, "binding.state")
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            };
            return self.backend.build_initialization_check(state, span.clone());
        }
        if let Some(state) = self.initialization_states.get(&symbol) {
            return self
                .backend
                .build_initialization_check(state.as_pointer_value(), span.clone());
        }
        Ok(())
    }

    /// One call argument's value: a place-backed pointer for the pointer pass
    /// modes, or the evaluated value materialized according to the recorded
    /// pass mode. Intrinsic routes evaluate by value using the recorded layout
    /// compile intrinsic.
    #[allow(clippy::too_many_arguments)]
    fn assemble_call_argument(
        &mut self,
        owner: EmissionOwner,
        call_id: LoweredCallId,
        record_index: usize,
        record: &LoweredCallArgument,
        expression: Option<ExpressionId>,
        by_value_route: bool,
        environment: &mut FunctionEnvironment<'context>,
        cleanups: &mut Vec<(usize, PointerValue<'context>)>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<BasicMetadataValueEnum<'context>> {
        let Some(expression) = expression else {
            // An implicit thunk argument: compile adapted call argument
            // builds the thunk's closure over the current environment.
            let value =
                self.build_thunk_closure(owner, call_id, record_index, environment, span)?;
            return self.pass_computed_argument(
                record,
                record_index,
                value,
                by_value_route,
                cleanups,
            );
        };
        if !by_value_route
            && matches!(
                record.pass_mode,
                LoweredArgumentPassMode::BorrowedPointer | LoweredArgumentPassMode::MutablePlace
            )
            && let Some(place) = record.place
        {
            match self.emit_place_pointer(owner, place, environment) {
                Ok(pointer) => return Ok(pointer.into()),
                // compile indirect argument pointer silently falls
                // back to a materialized copy when a possibly-place-rooted
                // borrow is not actually addressable; the mutation path has no
                // fallback. An indexed place stays a diagnostic.
                Err(error) => {
                    if record.pass_mode != LoweredArgumentPassMode::BorrowedPointer
                        || is_indexed_place_error(&error)
                    {
                        return Err(error);
                    }
                }
            }
        }
        let value = self.emit_expression(owner, expression, environment)?;
        if environment.returned {
            return Ok(value_as_basic(self.backend.unit_value())
                .ok_or_else(|| {
                    Diagnostic::new(
                        staple_syntax::Span::Compiler,
                        "internal invariant violated: Unit has an LLVM basic-value representation",
                    )
                })?
                .into());
        }
        let value = value_as_basic(value).ok_or_else(|| {
            Diagnostic::new(
                self.view
                    .expression(owner, expression)
                    .map(|record| record.origin.span.clone())
                    .unwrap_or(staple_syntax::Span::Compiler),
                "call argument is not a first-class value",
            )
        })?;
        self.pass_computed_argument(record, record_index, value, by_value_route, cleanups)
    }

    /// One implicit thunk argument's closure: the thunk instance's function
    /// paired with a fresh capture environment built from the current scope.
    fn build_thunk_closure(
        &mut self,
        owner: EmissionOwner,
        call_id: LoweredCallId,
        record_index: usize,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let binding = self
            .view
            .binding(
                owner,
                LoweredBindingSite::CallArgumentThunk {
                    call: call_id,
                    argument: record_index,
                },
            )
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "implicit thunk argument has no binding")
            })?;
        let LoweredBoundTarget::Instance(instance) = binding else {
            return Err(Diagnostic::new(
                span.clone(),
                "implicit thunk argument is not bound to an instance",
            ));
        };
        let instance = *instance;
        let function = self
            .instances
            .get(&instance)
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "closure instance is not declared"))?;
        let body = self
            .view
            .instance(instance)
            .and_then(|record| record.body.as_ref())
            .ok_or_else(|| Diagnostic::new(span.clone(), "closure instance has no body"))?;
        let pointer = self.build_capture_environment_value(owner, body, environment, span)?;
        // build closure installs the environment finalizer exactly
        // when lowering recorded the thunk argument's environment use.
        if let Some(finalizer) = self.site_finalizer(
            owner,
            crate::ArtifactUseSite::ThunkArgumentEnvironment {
                call: call_id,
                argument: record_index,
            },
        ) {
            self.backend.set_gc_finalizer(pointer, finalizer)?;
        }
        self.backend
            .build_closure_value(function, pointer)
            .map(|closure| closure.into())
    }

    /// Builds an instance's capture environment from the current scope.
    /// Empty captures produce a null pointer. Capture order, storage kinds,
    /// and finalizer uses come from the lowered body and artifact plans.
    fn build_capture_environment_value(
        &mut self,
        owner: EmissionOwner,
        body: &crate::LoweredInstanceBody,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        if body.captures.is_empty() {
            return Ok(self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null());
        }
        let fields = body
            .captures
            .iter()
            .map(|capture| self.capture_field_type(capture))
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let environment_type = self.backend.capture_environment_type(&fields);
        let mut environment_value = environment_type.const_zero();
        for (index, capture) in body.captures.iter().enumerate() {
            let stored = self.capture_value(owner, capture, environment, span)?;
            environment_value = self.backend.insert_capture(
                environment_value,
                stored,
                index as u32,
                span.clone(),
            )?;
        }
        self.backend
            .allocate_capture_environment(environment_type, environment_value, span.clone())
    }

    /// Reads an instance capture. Cell, initialization-state, derived, and borrowed
    /// captures store pointers; other captures store their values.
    fn capture_value(
        &mut self,
        owner: EmissionOwner,
        capture: &LoweredInstanceCapture,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let symbol = capture.capture.symbol;
        if capture.requires_initialization_state || capture.mutable_storage || capture.derived {
            let pointer = environment
                .parameter_pointers
                .get(&symbol)
                .copied()
                .or_else(|| environment.binding_cells.get(&symbol).copied())
                .ok_or_else(|| {
                    Diagnostic::new(span.clone(), "captured binding cell is not available")
                })?;
            return Ok(pointer.into());
        }
        if capture.capture.borrowed {
            let pointer = environment
                .parameter_pointers
                .get(&symbol)
                .copied()
                .ok_or_else(|| {
                    Diagnostic::new(span.clone(), "borrowed parameter storage is not available")
                })?;
            return Ok(pointer.into());
        }
        let value =
            self.load_symbol_value(owner, symbol, false, &capture.value_type, span, environment)?;
        value_as_basic(value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "captured value is not first-class"))
    }

    /// Materialize an already-evaluated value according to its recorded pass
    /// mode. Spread elements are evaluated once, so they cannot re-evaluate
    /// their expression; their placements never have a source place.
    fn pass_computed_argument(
        &self,
        record: &LoweredCallArgument,
        record_index: usize,
        value: BasicValueEnum<'context>,
        by_value_route: bool,
        cleanups: &mut Vec<(usize, PointerValue<'context>)>,
    ) -> CodeGenerationResult<BasicMetadataValueEnum<'context>> {
        if by_value_route || record.pass_mode == LoweredArgumentPassMode::Value {
            return Ok(value.into());
        }
        let llvm_type = self.backend.compile_type(&record.expected)?;
        match record.pass_mode {
            LoweredArgumentPassMode::Value => Ok(value.into()),
            LoweredArgumentPassMode::BorrowedPointer
            | LoweredArgumentPassMode::MaterializedTemporary => Ok(self
                .backend
                .build_argument_temporary(
                    value,
                    llvm_type,
                    "borrow.temporary",
                    staple_syntax::Span::Compiler,
                )?
                .into()),
            LoweredArgumentPassMode::MutablePlace => {
                let pointer = self.backend.build_argument_temporary(
                    value,
                    llvm_type,
                    "mutation.temporary",
                    staple_syntax::Span::Compiler,
                )?;
                if record.drops_after_call {
                    cleanups.push((record_index, pointer));
                }
                Ok(pointer.into())
            }
        }
    }

    /// One spread step: evaluate the operand once and extract each mapped
    /// element into its destination slot per the slot's recorded pass mode.
    #[allow(clippy::too_many_arguments)]
    fn emit_spread_arguments(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        expression: ExpressionId,
        mappings: &[(usize, usize)],
        by_value_route: bool,
        environment: &mut FunctionEnvironment<'context>,
        cleanups: &mut Vec<(usize, PointerValue<'context>)>,
        slots: &mut [Option<BasicMetadataValueEnum<'context>>],
    ) -> CodeGenerationResult<()> {
        let value = self.emit_expression(owner, expression, environment)?;
        if environment.returned {
            return Ok(());
        }
        let Some(BasicValueEnum::StructValue(product)) = value_as_basic(value) else {
            return Err(Diagnostic::new(
                call.origin.span.clone(),
                "product spread operand has an invalid representation",
            ));
        };
        for (source, slot) in mappings {
            let record = call.arguments.get(*slot).ok_or_else(|| {
                Diagnostic::new(call.origin.span.clone(), "missing spread argument record")
            })?;
            let element = self
                .backend
                .builder
                .build_extract_value(product, *source as u32, "product.spread.element")
                .map_err(compiler_diagnostic)?;
            let passed =
                self.pass_computed_argument(record, *slot, element, by_value_route, cleanups)?;
            place_argument_slot(slots, *slot, passed, &call.origin.span)?;
        }
        Ok(())
    }

    /// Emits a place pointer using recorded projections and resource providers.
    /// Symbol lookup prefers parameter pointers, then cells, then module globals.
    fn emit_place_pointer(
        &mut self,
        owner: EmissionOwner,
        id: crate::PlaceId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let place = self.view.place(owner, id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered place")
        })?;
        let unsupported = |family| {
            Diagnostic::new(
                place.origin.span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            )
        };
        match &place.kind {
            crate::LoweredPlaceKind::Symbol { symbol } => {
                if let Some(pointer) = environment.parameter_pointers.get(symbol).copied() {
                    return Ok(pointer);
                }
                if let Some(cell) = environment.binding_cells.get(symbol).copied() {
                    let cell_type = self.binding_cell_type(owner, *symbol)?;
                    return self
                        .backend
                        .builder
                        .build_struct_gep(cell_type, cell, 0, "binding.value")
                        .map_err(compiler_diagnostic);
                }
                if let Some(global) = self.storage.get(symbol).copied() {
                    return Ok(global.as_pointer_value());
                }
                Err(unsupported("place"))
            }
            crate::LoweredPlaceKind::CapturedCell { symbol } => {
                let cell = environment
                    .binding_cells
                    .get(symbol)
                    .copied()
                    .ok_or_else(|| {
                        Diagnostic::new(place.origin.span.clone(), "captured cell is not available")
                    })?;
                let cell_type = self.binding_cell_type(owner, *symbol)?;
                self.backend
                    .builder
                    .build_struct_gep(cell_type, cell, 0, "binding.value")
                    .map_err(compiler_diagnostic)
            }
            crate::LoweredPlaceKind::Resource { use_ } => {
                let use_record = self.view.resource_use(owner, *use_).ok_or_else(|| {
                    Diagnostic::new(place.origin.span.clone(), "missing resource use")
                })?;
                let provider = use_record.provider.ok_or_else(|| {
                    Diagnostic::new(
                        place.origin.span.clone(),
                        "resource place has no selected provider",
                    )
                })?;
                let bound = environment.resources.get(&provider).ok_or_else(|| {
                    Diagnostic::new(
                        place.origin.span.clone(),
                        format!(
                            "resource `{}` is not available",
                            use_record.resource.value_type
                        ),
                    )
                })?;
                if !bound.indirect {
                    return Err(Diagnostic::new(
                        place.origin.span.clone(),
                        format!("resource `{}` is not mutable", bound.resource.value_type),
                    ));
                }
                value_as_basic(bound.value)
                    .map(|value| value.into_pointer_value())
                    .ok_or_else(|| {
                        Diagnostic::new(
                            place.origin.span.clone(),
                            "resource address is not first-class",
                        )
                    })
            }
            // a non-place base materialized so it can be
            // mutated (compile mutation argument pointer).
            crate::LoweredPlaceKind::Temporary { expression } => {
                let value = self.emit_expression(owner, *expression, environment)?;
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(place.origin.span.clone(), "argument is not storable")
                })?;
                let llvm_type = self.backend.compile_type(&place.value_type)?;
                self.backend.build_argument_temporary(
                    value,
                    llvm_type,
                    "mutation.temporary",
                    place.origin.span.clone(),
                )
            }
            // A `Ref` payload chain: ref payload pointer leaves the
            // final payload address in place.
            crate::LoweredPlaceKind::Dereference {
                reference,
                dereference,
            } => {
                let value = self.emit_expression(owner, *reference, environment)?;
                self.backend
                    .ref_payload_pointer(value, dereference, place.origin.span.clone())
            }
            crate::LoweredPlaceKind::ProductElement { base, index, slice } => {
                let base_place = self.view.place(owner, *base).ok_or_else(|| {
                    Diagnostic::new(place.origin.span.clone(), "missing base place")
                })?;
                if *slice {
                    // The emitter evaluates the slice value, then loads its pointer
                    // and length and bounds-checks the fixed index.
                    let value = match &base_place.kind {
                        crate::LoweredPlaceKind::Symbol { symbol }
                        | crate::LoweredPlaceKind::CapturedCell { symbol } => {
                            let check = self.view.symbol(*symbol).is_some_and(|symbol| {
                                symbol.requires_initialization_check || symbol.mutable_storage
                            });
                            self.load_symbol_value(
                                owner,
                                *symbol,
                                check,
                                &base_place.value_type,
                                &place.origin.span,
                                environment,
                            )?
                        }
                        _ => {
                            let pointer = self.emit_place_pointer(owner, *base, environment)?;
                            let llvm_type = self.backend.compile_type(&base_place.value_type)?;
                            self.backend
                                .builder
                                .build_load(llvm_type, pointer, "slice.place.value")
                                .map(|value| value.as_any_value_enum())
                                .map_err(compiler_diagnostic)?
                        }
                    };
                    let Some(BasicValueEnum::StructValue(reference)) = value_as_basic(value) else {
                        return Err(Diagnostic::new(
                            place.origin.span.clone(),
                            "invalid slice place",
                        ));
                    };
                    let pointer = self
                        .backend
                        .builder
                        .build_extract_value(reference, 0, "place.pointer")
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value();
                    let length = self
                        .backend
                        .builder
                        .build_extract_value(reference, 1, "place.length")
                        .map_err(compiler_diagnostic)?
                        .into_int_value();
                    let position = self.backend.size_type.const_int(*index as u64, false);
                    let out = self
                        .backend
                        .builder
                        .build_int_compare(
                            inkwell::IntPredicate::UGE,
                            position,
                            length,
                            "place.out_of_bounds",
                        )
                        .map_err(compiler_diagnostic)?;
                    self.backend.build_trap_if(out, place.origin.span.clone())?;
                    let element_type = self.backend.compile_type(&place.value_type)?;
                    return unsafe {
                        self.backend.builder.build_gep(
                            element_type,
                            pointer,
                            &[position],
                            "place.element",
                        )
                    }
                    .map_err(compiler_diagnostic);
                }
                // The emitter checks a mutable symbol base's initialization before
                // projecting a field.
                if let crate::LoweredPlaceKind::Symbol { symbol }
                | crate::LoweredPlaceKind::CapturedCell { symbol } = &base_place.kind
                    && self
                        .view
                        .symbol(*symbol)
                        .is_some_and(|symbol| symbol.mutable_storage)
                {
                    self.check_symbol_initialization(
                        owner,
                        environment,
                        *symbol,
                        &place.origin.span,
                    )?;
                }
                let pointer = self.emit_place_pointer(owner, *base, environment)?;
                let container_type =
                    super::layout::strip_place_wrappers(base_place.value_type.clone());
                let BasicTypeEnum::StructType(container_llvm) =
                    self.backend.compile_type(&container_type)?
                else {
                    return Err(Diagnostic::new(
                        place.origin.span.clone(),
                        "access place is not a product",
                    ));
                };
                self.backend
                    .builder
                    .build_struct_gep(container_llvm, pointer, *index as u32, "place.field")
                    .map_err(compiler_diagnostic)
            }
            // A distinct representation read is the base place itself.
            crate::LoweredPlaceKind::Representation { base } => {
                self.emit_place_pointer(owner, *base, environment)
            }
            // An indexed place is an assignment target dispatched through
            // `MutateIndex`, never a pointer.
            crate::LoweredPlaceKind::Indexed { .. } => Err(unsupported("indexed place")),
        }
    }

    /// Emits an assignment. Indexed targets use MutateIndex; other targets drop
    /// the replaced value, store through the place pointer, update initialization
    /// state, and notify the recorded signal root.
    fn emit_assignment(
        &mut self,
        owner: EmissionOwner,
        id: crate::ItemId,
        assignment: &crate::LoweredAssignmentItem,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let place = self
            .view
            .place(owner, assignment.target)
            .ok_or_else(|| Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered place"))?
            .clone();
        if matches!(place.kind, crate::LoweredPlaceKind::Indexed { .. }) {
            return self.emit_mutate_index_assignment(owner, id, assignment, &place, environment);
        }
        let span = place.origin.span.clone();
        let pointer = self.emit_place_pointer(owner, assignment.target, environment)?;
        let value = self.emit_expression(owner, assignment.value, environment)?;
        if environment.returned {
            return Ok(());
        }
        let value = value_as_basic(value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "assigned value is not storable"))?;
        // compile assignment conditionally drops a binding cell's
        // old value, or loads `assignment.old` from the place; the owner's
        // `ReplacedValue` use record is the discriminant.
        if assignment.drop_previous {
            let cell = self
                .place_root_symbol(owner, assignment.target)
                .and_then(|symbol| environment.binding_cells.get(&symbol).copied());
            let source = match cell {
                Some(cell) => DropSource::Cell(cell),
                None => DropSource::Place(pointer),
            };
            self.emit_drop_site(
                owner,
                crate::ArtifactUseSite::ReplacedValue(id),
                source,
                &span,
            )?;
        }
        self.backend
            .builder
            .build_store(pointer, value)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        // a field projection never initializes its base; the
        // base is already initialized (or the projection's own check traps).
        // Its root symbol exists here for notification only.
        if let Some(symbol) = assignment.initialization_symbol
            && !matches!(place.kind, crate::LoweredPlaceKind::ProductElement { .. })
        {
            self.store_local_initialization_state(owner, environment, symbol, 2, &span)?;
            self.store_initialization_state(symbol, 2)?;
        }
        if let Some(operation) = assignment.signal_notify {
            let symbol = self
                .view
                .reactive_operation(owner, operation)
                .and_then(|record| match &record.kind {
                    LoweredReactiveOperationKind::SignalNotify { symbol } => Some(*symbol),
                    _ => None,
                })
                .ok_or_else(|| Diagnostic::new(span.clone(), "invalid signal notify record"))?;
            if let Some(signal) = self.signal_metadata_value(owner, environment, symbol, &span)? {
                self.backend.build_reactive_runtime_call(
                    "__staple_signal_notify",
                    &[signal.into()],
                    None,
                    "signal.notify",
                    span.clone(),
                )?;
            }
        }
        Ok(())
    }

    /// The root symbol compile place pointer returns for a place: a
    /// symbol place (direct or captured cell) or a representation base;
    /// product elements, temporaries, dereferences, resources, and indexed
    /// targets return `None`.
    fn place_root_symbol(&self, owner: EmissionOwner, id: crate::PlaceId) -> Option<SymbolId> {
        let place = self.view.place(owner, id)?;
        match &place.kind {
            crate::LoweredPlaceKind::Symbol { symbol }
            | crate::LoweredPlaceKind::CapturedCell { symbol } => Some(*symbol),
            crate::LoweredPlaceKind::Representation { base } => {
                self.place_root_symbol(owner, *base)
            }
            crate::LoweredPlaceKind::Temporary { .. }
            | crate::LoweredPlaceKind::Resource { .. }
            | crate::LoweredPlaceKind::Dereference { .. }
            | crate::LoweredPlaceKind::ProductElement { .. }
            | crate::LoweredPlaceKind::Indexed { .. } => None,
        }
    }

    /// Emits an indexed assignment through its bound MutateIndex callee. The
    /// base uses a place pointer or mutation temporary; the call passes base,
    /// position, and replacement after the null environment.
    fn emit_mutate_index_assignment(
        &mut self,
        owner: EmissionOwner,
        id: crate::ItemId,
        assignment: &crate::LoweredAssignmentItem,
        place: &crate::LoweredPlace,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let span = place.origin.span.clone();
        let crate::LoweredPlaceKind::Indexed { base, index } = &place.kind else {
            return Err(Diagnostic::new(
                span,
                "internal invariant violated: indexed assignment dispatch requires an indexed place",
            ));
        };
        let binding = self
            .view
            .binding(owner, crate::LoweredBindingSite::IndexedAssignment(id))
            .ok_or_else(|| Diagnostic::new(span.clone(), "indexed assignment has no binding"))?;
        let function = match binding {
            crate::LoweredBoundTarget::Instance(instance) => {
                self.instances.get(instance).copied().ok_or_else(|| {
                    Diagnostic::new(span.clone(), "MutateIndex instance is not declared")
                })?
            }
            crate::LoweredBoundTarget::Artifact(ordinal) => self
                .artifacts
                .get(ordinal)
                .and_then(|functions| functions.first())
                .copied()
                .ok_or_else(|| {
                    Diagnostic::new(span.clone(), "MutateIndex artifact is not declared")
                })?,
            _ => {
                return Err(Diagnostic::new(
                    span.clone(),
                    "indexed assignment is not bound to a function",
                ));
            }
        };
        // The emitter compiles the base pointer first, then the position, then the
        // replacement.
        self.view.place(owner, *base).ok_or_else(|| {
            Diagnostic::new(span.clone(), "missing indexed assignment base place")
        })?;
        let pointer = self.emit_place_pointer(owner, *base, environment)?;
        let position = self.emit_expression(owner, *index, environment)?;
        if environment.returned {
            return Ok(());
        }
        let replacement = self.emit_expression(owner, assignment.value, environment)?;
        if environment.returned {
            return Ok(());
        }
        let position = value_as_basic(position).ok_or_else(|| {
            Diagnostic::new(span.clone(), "MutateIndex position is not first-class")
        })?;
        let replacement = value_as_basic(replacement).ok_or_else(|| {
            Diagnostic::new(span.clone(), "MutateIndex replacement is not first-class")
        })?;
        let arguments = [
            self.backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null()
                .into(),
            pointer.into(),
            position.into(),
            replacement.into(),
        ];
        self.backend
            .builder
            .build_direct_call(function, &arguments, "mutate_index.call")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        // drop mutation temporaries drops the materialized base when
        // lowering recorded it.
        if assignment.drops_base_temporary {
            self.emit_drop_site(
                owner,
                crate::ArtifactUseSite::MutateIndexTemporary(id),
                DropSource::Temporary(pointer),
                &span,
            )?;
        }
        Ok(())
    }

    /// Emits an Index call. Recorded pass modes select a place pointer, mutation
    /// temporary, or borrowed temporary. Hidden resources precede visible operands.
    fn emit_index(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        expression: &crate::LoweredExpression,
        index: &crate::LoweredIndex,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = expression.origin.span.clone();
        let unsupported = |family| {
            Diagnostic::new(
                span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            )
        };
        let binding = self
            .view
            .binding(owner, crate::LoweredBindingSite::Index(id))
            .ok_or_else(|| unsupported("index binding"))?;
        let function = match binding {
            crate::LoweredBoundTarget::Instance(instance) => self
                .instances
                .get(instance)
                .copied()
                .ok_or_else(|| unsupported("index instance declaration"))?,
            // A structural `Index` method's body is emission; the declared
            // artifact is called like any other function.
            crate::LoweredBoundTarget::Artifact(ordinal) => self
                .artifacts
                .get(ordinal)
                .and_then(|functions| functions.first())
                .copied()
                .ok_or_else(|| unsupported("index artifact declaration"))?,
            _ => return Err(unsupported("index binding")),
        };
        let Some(method_type) = &index.method_type else {
            return Err(unsupported("index method type"));
        };
        if !method_type.effects.resources.is_empty() {
            return Err(unsupported("index resources"));
        }
        let types = flattened_parameter_types(&method_type.parameter);
        // Lowering records which operands pass by address; the
        // mutation mask is the checked method type's own fact.
        let mask = index.operands.indirect.clone();
        if mask.len() != types.len() {
            return Err(unsupported("index operand facts"));
        }
        let mutation_mask =
            super::abi::mutation_parameter_mask(types.len(), &method_type.mutations);
        let mut values: Vec<BasicMetadataValueEnum<'context>> = Vec::new();
        // Mutation temporaries drop mutation temporaries drops after
        // the call, in collection order; the owned `IndexTemporary` use record
        // names each site's glue.
        let mut whole_temporary: Option<PointerValue<'context>> = None;
        let mut temporaries: Vec<(usize, PointerValue<'context>)> = Vec::new();
        if !mask.iter().any(|indirect| *indirect) {
            for operand in [index.base, index.index] {
                let value = self.emit_expression(owner, operand, environment)?;
                if environment.returned {
                    return Ok(self.backend.unit_value());
                }
                values.push(
                    value_as_basic(value)
                        .ok_or_else(|| unsupported("index argument"))?
                        .into(),
                );
            }
        } else if index.whole_temporary {
            let mut elements = Vec::with_capacity(2);
            for operand in [index.base, index.index] {
                let value = self.emit_expression(owner, operand, environment)?;
                if environment.returned {
                    return Ok(self.backend.unit_value());
                }
                elements.push(value_as_basic(value).ok_or_else(|| unsupported("index argument"))?);
            }
            let product = self.backend.build_product_value(&elements, span.clone())?;
            let llvm_type = self.backend.compile_type(&method_type.parameter)?;
            let pointer = self.backend.build_argument_temporary(
                product,
                llvm_type,
                "mutation.temporary",
                span.clone(),
            )?;
            whole_temporary = Some(pointer);
            values.push(pointer.into());
        } else {
            for (element, operand) in [(0usize, index.base), (1usize, index.index)] {
                if element >= types.len() {
                    break;
                }
                let place = if element == 0 {
                    index.base_place
                } else {
                    index.index_place
                };
                if mask[element] {
                    if mutation_mask[element] {
                        if let Some(place) = place {
                            let pointer = self.emit_place_pointer(owner, place, environment)?;
                            values.push(pointer.into());
                        } else {
                            let value = self.emit_expression(owner, operand, environment)?;
                            if environment.returned {
                                return Ok(self.backend.unit_value());
                            }
                            let value = value_as_basic(value)
                                .ok_or_else(|| unsupported("index argument"))?;
                            let llvm_type = self.backend.compile_type(&types[element])?;
                            let pointer = self.backend.build_argument_temporary(
                                value,
                                llvm_type,
                                "mutation.temporary",
                                span.clone(),
                            )?;
                            if index
                                .operands
                                .drops_after_call
                                .get(element)
                                .copied()
                                .unwrap_or(false)
                            {
                                temporaries.push((element, pointer));
                            }
                            values.push(pointer.into());
                        }
                    } else {
                        // A recorded place is always used; a place that
                        // cannot be emitted is a diagnostic, never a silent
                        // fallback to a borrowed copy.
                        let pointer = match place {
                            Some(place) => {
                                Some(self.emit_place_pointer(owner, place, environment)?)
                            }
                            None => None,
                        };
                        match pointer {
                            Some(pointer) => values.push(pointer.into()),
                            None => {
                                let value = self.emit_expression(owner, operand, environment)?;
                                if environment.returned {
                                    return Ok(self.backend.unit_value());
                                }
                                let value = value_as_basic(value)
                                    .ok_or_else(|| unsupported("index argument"))?;
                                let llvm_type = self.backend.compile_type(&types[element])?;
                                let pointer = self.backend.build_argument_temporary(
                                    value,
                                    llvm_type,
                                    "borrow.temporary",
                                    span.clone(),
                                )?;
                                values.push(pointer.into());
                            }
                        }
                    }
                } else {
                    let value = self.emit_expression(owner, operand, environment)?;
                    if environment.returned {
                        return Ok(self.backend.unit_value());
                    }
                    values.push(
                        value_as_basic(value)
                            .ok_or_else(|| unsupported("index argument"))?
                            .into(),
                    );
                }
            }
        }
        let mut arguments = vec![
            self.backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null()
                .into(),
        ];
        arguments.extend(values);
        let result = self
            .backend
            .builder
            .build_direct_call(function, &arguments, "index.call")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| Diagnostic::new(span.clone(), "Index result is not first-class"))?;
        // drop mutation temporaries drops the recorded operand
        // temporaries after the call in reverse collection order.
        if index.operands.whole_drops_after_call
            && let Some(pointer) = whole_temporary
        {
            self.emit_drop_site(
                owner,
                crate::ArtifactUseSite::IndexTemporary {
                    expression: id,
                    operand: None,
                },
                DropSource::Temporary(pointer),
                &span,
            )?;
        }
        for (operand, pointer) in temporaries.into_iter().rev() {
            self.emit_drop_site(
                owner,
                crate::ArtifactUseSite::IndexTemporary {
                    expression: id,
                    operand: Some(operand),
                },
                DropSource::Temporary(pointer),
                &span,
            )?;
        }
        Ok(result.as_any_value_enum())
    }

    /// Extracts an indirect call's code and environment pointers once, after
    /// visible argument evaluation and before hidden resources.
    fn ensure_callee_parts(
        &self,
        closure: &Option<inkwell::values::StructValue<'context>>,
        parts: &mut Option<(PointerValue<'context>, PointerValue<'context>)>,
    ) -> CodeGenerationResult<()> {
        if parts.is_some() {
            return Ok(());
        }
        let Some(closure) = closure else {
            return Ok(());
        };
        let code = self
            .backend
            .builder
            .build_extract_value(*closure, 0, "closure.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let environment = self
            .backend
            .builder
            .build_extract_value(*closure, 1, "closure.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        *parts = Some((code, environment));
        Ok(())
    }

    /// One hidden effect-row resource argument, resolved
    /// through the provider the use records (compile resource arguments).
    fn emit_hidden_resource_argument(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        index: usize,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<BasicMetadataValueEnum<'context>> {
        let use_id = *call.resource_bindings.get(index).ok_or_else(|| {
            Diagnostic::new(call.origin.span.clone(), "missing hidden resource binding")
        })?;
        let use_record = self.view.resource_use(owner, use_id).ok_or_else(|| {
            Diagnostic::new(call.origin.span.clone(), "missing hidden resource use")
        })?;
        let value = self.bound_resource_value(environment, use_record)?;
        value_as_basic(value)
            .map(Into::into)
            .ok_or_else(|| Diagnostic::new(call.origin.span.clone(), "resource is not first-class"))
    }

    /// The value one resource use passes or reads: a borrow pointer for a
    /// mutable or non-`Copy` requirement (which needs an indirect provider),
    /// else a direct value, loading `resource.copy` when the provider is
    /// indirect. compile resource arguments' rule.
    fn bound_resource_value(
        &self,
        environment: &FunctionEnvironment<'context>,
        use_record: &crate::LoweredResourceUse,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let provider = use_record.provider.ok_or_else(|| {
            Diagnostic::new(
                use_record.origin.span.clone(),
                "resource use has no selected provider",
            )
        })?;
        let bound = environment.resources.get(&provider).ok_or_else(|| {
            Diagnostic::new(
                use_record.origin.span.clone(),
                format!(
                    "resource `{}` is not available",
                    use_record.resource.value_type
                ),
            )
        })?;
        if use_record.pass_mode != LoweredArgumentPassMode::Value {
            if !bound.indirect {
                return Err(Diagnostic::new(
                    use_record.origin.span.clone(),
                    format!(
                        "resource `{}` is not borrowable",
                        use_record.resource.value_type
                    ),
                ));
            }
            return Ok(bound.value);
        }
        if bound.indirect {
            let pointer = value_as_basic(bound.value)
                .and_then(|value| match value {
                    BasicValueEnum::PointerValue(pointer) => Some(pointer),
                    _ => None,
                })
                .ok_or_else(|| {
                    Diagnostic::new(
                        use_record.origin.span.clone(),
                        "borrowed resource pointer is not first-class",
                    )
                })?;
            let llvm_type = self.backend.compile_type(&use_record.resource.value_type)?;
            return self
                .backend
                .builder
                .build_load(llvm_type, pointer, "resource.copy")
                .map(|value| value.as_any_value_enum())
                .map_err(compiler_diagnostic);
        }
        Ok(bound.value)
    }

    /// A `resource` read. The emitter loads `resource.borrow`
    /// through an indirect provider and takes the value directly otherwise.
    fn emit_resource_read(
        &mut self,
        owner: EmissionOwner,
        id: crate::LoweredResourceUseId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let use_record = self.view.resource_use(owner, id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing resource use")
        })?;
        let provider = use_record.provider.ok_or_else(|| {
            Diagnostic::new(
                use_record.origin.span.clone(),
                "resource read has no selected provider",
            )
        })?;
        let bound = environment.resources.get(&provider).ok_or_else(|| {
            Diagnostic::new(
                use_record.origin.span.clone(),
                format!(
                    "resource `{}` is not available",
                    use_record.resource.value_type
                ),
            )
        })?;
        if bound.indirect {
            let pointer = value_as_basic(bound.value)
                .map(|value| value.into_pointer_value())
                .ok_or_else(|| {
                    Diagnostic::new(
                        use_record.origin.span.clone(),
                        "borrowed resource pointer is not first-class",
                    )
                })?;
            let llvm_type = self.backend.compile_type(&use_record.resource.value_type)?;
            return self
                .backend
                .builder
                .build_load(llvm_type, pointer, "resource.borrow")
                .map(|value| value.as_any_value_enum())
                .map_err(compiler_diagnostic);
        }
        Ok(bound.value)
    }

    /// A `with` provider and its body. The emitter evaluates the
    /// provider value, stores it in the source place (`Place`) or a
    /// `resource.provider` alloca (`Materialized`), binds it while the body
    /// runs, disposes a reactive scope and closes a `Tasks` scope on a normal
    /// exit. Task scopes close only on the normal exit.
    fn emit_with(
        &mut self,
        owner: EmissionOwner,
        id: crate::LoweredWithId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let with = self.view.with(owner, id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered with")
        })?;
        let provider = self
            .view
            .resource_provider(owner, with.provider)
            .ok_or_else(|| Diagnostic::new(with.origin.span.clone(), "missing `with` provider"))?;
        let value = self.emit_expression(owner, with.value, environment)?;
        let stored = match provider.storage {
            LoweredProviderStorage::Place => {
                let place = with.place.ok_or_else(|| {
                    Diagnostic::new(
                        with.origin.span.clone(),
                        "`with` place provider has no place",
                    )
                })?;
                self.emit_place_pointer(owner, place, environment)?
                    .as_any_value_enum()
            }
            LoweredProviderStorage::Materialized => {
                let llvm_type = self.backend.compile_type(&provider.resource.value_type)?;
                let slot = self
                    .backend
                    .builder
                    .build_alloca(llvm_type, "resource.provider")
                    .map_err(compiler_diagnostic)?;
                let value = value_as_basic(value).ok_or_else(|| {
                    Diagnostic::new(
                        with.origin.span.clone(),
                        "resource provider is not first-class",
                    )
                })?;
                self.backend
                    .builder
                    .build_store(slot, value)
                    .map_err(compiler_diagnostic)?;
                slot.as_any_value_enum()
            }
        };
        environment.resources.insert(
            with.provider,
            BoundResource {
                resource: provider.resource.clone(),
                value: stored,
                indirect: true,
            },
        );
        let reactive = with.scope_exit == LoweredScopeExit::Reactive;
        if reactive {
            let scope = value_as_basic(value)
                .map(|value| value.into_pointer_value())
                .ok_or_else(|| {
                    Diagnostic::new(
                        with.origin.span.clone(),
                        "reactive scope is not first-class",
                    )
                })?;
            environment.reactive_scopes.push(scope);
        }
        let tasks = with.scope_exit == LoweredScopeExit::Tasks;
        if tasks {
            let scope = value_as_basic(value)
                .map(|value| value.into_pointer_value())
                .ok_or_else(|| {
                    Diagnostic::new(with.origin.span.clone(), "Tasks scope is not first-class")
                })?;
            environment.task_scopes.push(scope);
        }
        let result = self.emit_block(owner, with.body, environment);
        if !environment.returned && reactive {
            self.dispose_reactive_scopes(
                environment,
                environment.reactive_scopes.len().saturating_sub(1),
                &with.origin.span,
            )?;
        }
        if reactive {
            environment.reactive_scopes.pop();
        }
        if tasks {
            if !environment.returned {
                self.close_task_scopes(
                    environment,
                    environment.task_scopes.len().saturating_sub(1),
                )?;
            }
            environment.task_scopes.pop();
        }
        environment.resources.remove(&with.provider);
        result
    }

    /// Close every scope from `keep` on, in
    /// reverse order. emission closes abandoned scopes on `return`,
    /// `break`, and `continue`, right after reactive disposal and before the
    /// owned drops; a `with Tasks` normal exit still closes its own scope.
    fn close_task_scopes(
        &self,
        environment: &FunctionEnvironment<'context>,
        keep: usize,
    ) -> CodeGenerationResult<()> {
        if environment.task_scopes.len() <= keep {
            return Ok(());
        }
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let close = self.backend.declare_named_function(
            "__staple_task_scope_close",
            self.backend
                .context
                .void_type()
                .fn_type(&[pointer.into()], false),
        );
        for scope in environment.task_scopes[keep..].iter().rev() {
            self.backend
                .build_runtime_call(close, &[(*scope).into()], "")?;
        }
        Ok(())
    }

    /// Dispose every scope from `keep` on, in
    /// reverse order.
    fn dispose_reactive_scopes(
        &self,
        environment: &FunctionEnvironment<'context>,
        keep: usize,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        for scope in environment.reactive_scopes[keep..].iter().rev() {
            self.backend.build_reactive_runtime_call(
                "__staple_reactive_scope_dispose",
                &[(*scope).into()],
                None,
                "reactive.dispose",
                span.clone(),
            )?;
        }
        Ok(())
    }

    /// A parameter pointer is reloaded on every
    /// read (the binding's own load stays behind, unused), then a local, then
    /// a binding cell, then module storage.
    fn load_symbol_value(
        &mut self,
        owner: EmissionOwner,
        symbol: SymbolId,
        check_initialization: bool,
        value_type: &CheckedType,
        span: &staple_syntax::Span,
        environment: &FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        if let Some(pointer) = environment.parameter_pointers.get(&symbol).copied() {
            let llvm_type = self.backend.compile_type(value_type)?;
            return self
                .backend
                .builder
                .build_load(llvm_type, pointer, "parameter")
                .map(|value| value.as_any_value_enum())
                .map_err(compiler_diagnostic);
        }
        if let Some(value) = environment.locals.get(&symbol) {
            return Ok(*value);
        }
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            // The emitter's binding-cell arm: force a stale derived read, build the
            // state slot, run the shared check when the read needs one, track
            // a signal read, then load the value slot.
            self.force_derived_read(owner, environment, symbol, span)?;
            let cell_type = self.binding_cell_type(owner, symbol)?;
            let state_slot = self
                .backend
                .builder
                .build_struct_gep(cell_type, cell, 1, "binding.state")
                .map_err(compiler_diagnostic)?;
            if check_initialization {
                self.backend
                    .build_initialization_check(state_slot, span.clone())?;
            }
            let value_slot = self
                .backend
                .builder
                .build_struct_gep(cell_type, cell, 0, "binding.value")
                .map_err(compiler_diagnostic)?;
            let llvm_type = self.backend.compile_type(value_type)?;
            self.track_signal_read(owner, environment, symbol, span)?;
            return self
                .backend
                .builder
                .build_load(llvm_type, value_slot, "binding")
                .map(|value| value.as_any_value_enum())
                .map_err(compiler_diagnostic);
        }
        // compile symbol value's `closure_codes` arm: an extern used
        // as a first-class value is read as its adapter closure (with a null
        // environment).
        if let Some(code) = self.extern_adapters.get(&symbol).copied() {
            let environment_pointer = self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null();
            return self
                .backend
                .build_closure_value(code, environment_pointer)
                .map(|value| value.as_any_value_enum());
        }
        let global = self
            .storage
            .get(&symbol)
            .ok_or_else(|| Diagnostic::new(span.clone(), "symbol storage is not available here"))?;
        self.force_derived_read(owner, environment, symbol, span)?;
        if check_initialization && let Some(state) = self.initialization_states.get(&symbol) {
            self.backend
                .build_initialization_check(state.as_pointer_value(), span.clone())?;
        }
        let llvm_type = self.backend.compile_type(value_type)?;
        self.track_signal_read(owner, environment, symbol, span)?;
        self.backend
            .builder
            .build_load(llvm_type, global.as_pointer_value(), "global")
            .map(|value| value.as_any_value_enum())
            .map_err(compiler_diagnostic)
    }

    /// Builds the lowered Bool representation of a comparison result.
    fn build_intrinsic_bool(
        &self,
        condition: inkwell::values::IntValue<'context>,
        result_type: &CheckedType,
        span: staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let CheckedType::Sum(sum) = result_type else {
            return Err(Diagnostic::new(span, "comparison result must be Bool"));
        };
        if sum.alternatives.len() != 2 {
            return Err(Diagnostic::new(span, "comparison result must be Bool"));
        }
        let sum_type = self.backend.compile_sum_type(sum)?;
        self.backend.build_bool_value(condition, sum_type, span)
    }

    fn emit_intrinsic(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        call_id: LoweredCallId,
        intrinsic: IntrinsicFunction,
        arguments: &[BasicMetadataValueEnum<'context>],
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = call.origin.span.clone();
        let result_type = &call.result_type;
        let unsupported = |family| {
            Diagnostic::new(
                span.clone(),
                format!(
                    "lowered emitter: internal invariant violated: {family} is missing or malformed"
                ),
            )
        };
        match intrinsic {
            IntrinsicFunction::IntegerBinary { integer, operation } => {
                let [
                    BasicMetadataValueEnum::IntValue(left),
                    BasicMetadataValueEnum::IntValue(right),
                ] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        "integer arithmetic operands must be integers",
                    ));
                };
                // Use the common numeric conversion helper.
                let value = self
                    .backend
                    .build_integer_binary(integer, operation, *left, *right)?;
                Ok(value.as_any_value_enum())
            }
            IntrinsicFunction::ToString { value: numeric } => {
                let [argument] = arguments else {
                    return Err(Diagnostic::new(
                        span,
                        "numeric conversion needs one argument",
                    ));
                };
                // Use the common String conversion helper.
                let argument = BasicValueEnum::try_from(*argument).map_err(|_| {
                    Diagnostic::new(span.clone(), "numeric conversion requires a numeric value")
                })?;
                self.backend
                    .build_numeric_to_string(numeric, argument, span)
            }
            IntrinsicFunction::IntegerCompare { integer, operation } => {
                let [
                    BasicMetadataValueEnum::IntValue(left),
                    BasicMetadataValueEnum::IntValue(right),
                ] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        "integer comparison operands must be integers",
                    ));
                };
                let condition = self
                    .backend
                    .build_integer_compare(integer, operation, *left, *right)?;
                self.build_intrinsic_bool(condition, result_type, span)
            }
            IntrinsicFunction::FloatBinary { float, operation } => {
                let [
                    BasicMetadataValueEnum::FloatValue(left),
                    BasicMetadataValueEnum::FloatValue(right),
                ] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        "float arithmetic intrinsic operands must be floats",
                    ));
                };
                let value = self
                    .backend
                    .build_float_binary(float, operation, *left, *right)?;
                Ok(value.as_any_value_enum())
            }
            IntrinsicFunction::FloatCompare { float, operation } => {
                let [
                    BasicMetadataValueEnum::FloatValue(left),
                    BasicMetadataValueEnum::FloatValue(right),
                ] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        "float comparison intrinsic operands must be floats",
                    ));
                };
                let condition = self
                    .backend
                    .build_float_compare(float, operation, *left, *right)?;
                self.build_intrinsic_bool(condition, result_type, span)
            }
            IntrinsicFunction::StringFromCString => {
                let [BasicMetadataValueEnum::PointerValue(source)] = arguments else {
                    return Err(Diagnostic::new(
                        span,
                        "CString conversion requires a pointer",
                    ));
                };
                // shared conversion core; the CString
                // release expands the recorded `CStringConversion` glue.
                let result = self
                    .backend
                    .build_string_from_c_string(*source, span.clone())?;
                self.emit_drop_site(
                    owner,
                    crate::ArtifactUseSite::CStringConversion(call_id),
                    DropSource::Value((*source).into()),
                    &span,
                )?;
                Ok(result.as_any_value_enum())
            }
            IntrinsicFunction::StringToCString => {
                let [BasicMetadataValueEnum::StructValue(string)] = arguments else {
                    return Err(Diagnostic::new(
                        span,
                        "String conversion requires a String value",
                    ));
                };
                // Use the common runtime helper.
                Ok(self
                    .backend
                    .build_string_to_c_string(*string, span)?
                    .as_any_value_enum())
            }
            IntrinsicFunction::StringAdd => {
                let [
                    BasicMetadataValueEnum::StructValue(left),
                    BasicMetadataValueEnum::StructValue(right),
                ] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        "string concatenation requires two String values",
                    ));
                };
                self.backend.build_string_add(*left, *right, span)
            }
            IntrinsicFunction::SliceLength => {
                let [BasicMetadataValueEnum::StructValue(slice)] = arguments else {
                    return Err(Diagnostic::new(span, "length requires a slice"));
                };
                self.backend
                    .build_slice_length(*slice)
                    .map(|value| value.as_any_value_enum())
            }
            IntrinsicFunction::SliceGetRef => {
                let [
                    BasicMetadataValueEnum::StructValue(slice),
                    BasicMetadataValueEnum::IntValue(position),
                ] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        "get_ref requires a slice and a position",
                    ));
                };
                let CheckedType::Ref(payload) = result_type else {
                    return Err(Diagnostic::new(span, "unchecked get_ref result"));
                };
                let element_type = self.backend.compile_type(payload)?;
                let pointer =
                    self.backend
                        .build_slice_get_ref(*slice, *position, element_type, span)?;
                Ok(pointer.as_any_value_enum())
            }
            IntrinsicFunction::BufferWithCapacity => {
                let [BasicMetadataValueEnum::IntValue(capacity)] = arguments else {
                    return Err(Diagnostic::new(
                        span,
                        "Buffer.with_capacity requires a USize capacity",
                    ));
                };
                let CheckedType::Buffer(element) = result_type else {
                    return Err(Diagnostic::new(span, "invalid Buffer result type"));
                };
                let llvm_element = self.backend.compile_type(element)?;
                let header = self.backend.buffer_header_type(llvm_element);
                self.backend.trap_if_buffer_capacity_overflows(
                    *capacity,
                    llvm_element,
                    header,
                    span.clone(),
                )?;
                let buffer = self.backend.build_buffer_allocation(
                    *capacity,
                    llvm_element,
                    header,
                    "buffer",
                    span.clone(),
                )?;
                self.backend.build_buffer_capacity_store(
                    buffer,
                    header,
                    *capacity,
                    "buffer.capacity.slot",
                )?;
                // The `BufferAllocation` use records the element finalizer
                // ensure buffer finalizer installs.
                if let Some(finalizer) = self
                    .artifact_use_function(owner, crate::ArtifactUseSite::BufferAllocation(call_id))
                {
                    self.backend.set_gc_finalizer(buffer, finalizer)?;
                }
                Ok(buffer.as_any_value_enum())
            }
            IntrinsicFunction::BufferLength => {
                self.emit_buffer_metadata(call, arguments, 0, "buffer.length", &span)
            }
            IntrinsicFunction::BufferCapacity => {
                self.emit_buffer_metadata(call, arguments, 1, "buffer.capacity", &span)
            }
            IntrinsicFunction::BufferPush => {
                let [BasicMetadataValueEnum::PointerValue(buffer), replacement] = arguments else {
                    return Err(Diagnostic::new(
                        span,
                        "Buffer.push requires a buffer and value",
                    ));
                };
                let element = intrinsic_buffer_element(call, 0, &span)?;
                let llvm_element = self.backend.compile_type(&element)?;
                let header = self.backend.buffer_header_type(llvm_element);
                self.backend
                    .trap_if_buffer_frozen(*buffer, header, span.clone())?;
                let length_slot = self
                    .backend
                    .builder
                    .build_struct_gep(header, *buffer, 0, "buffer.length.slot")
                    .map_err(compiler_diagnostic)?;
                let capacity_slot = self
                    .backend
                    .builder
                    .build_struct_gep(header, *buffer, 1, "buffer.capacity.slot")
                    .map_err(compiler_diagnostic)?;
                let length = self
                    .backend
                    .builder
                    .build_load(self.backend.size_type, length_slot, "buffer.length")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let capacity = self
                    .backend
                    .builder
                    .build_load(self.backend.size_type, capacity_slot, "buffer.capacity")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let full = self
                    .backend
                    .builder
                    .build_int_compare(inkwell::IntPredicate::UGE, length, capacity, "buffer.full")
                    .map_err(compiler_diagnostic)?;
                self.backend.build_trap_if(full, span.clone())?;
                let data = self.backend.buffer_data_pointer(*buffer, llvm_element)?;
                let replacement = BasicValueEnum::try_from(*replacement)
                    .map_err(|_| Diagnostic::new(span.clone(), "Buffer element is not storable"))?;
                self.backend.build_buffer_element_store(
                    data,
                    llvm_element,
                    length,
                    replacement,
                    "buffer.push.slot",
                    span.clone(),
                )?;
                let next = self
                    .backend
                    .builder
                    .build_int_add(
                        length,
                        self.backend.size_type.const_int(1, false),
                        "buffer.next.length",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(length_slot, next)
                    .map_err(compiler_diagnostic)?;
                Ok(self.backend.unit_value())
            }
            IntrinsicFunction::BufferGet => {
                let [
                    BasicMetadataValueEnum::PointerValue(buffer),
                    BasicMetadataValueEnum::IntValue(position),
                ] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        "Buffer.get_ref requires a buffer and USize index",
                    ));
                };
                let CheckedType::Ref(element) = result_type else {
                    return Err(Diagnostic::new(span, "invalid Buffer.get_ref result"));
                };
                let llvm_element = self.backend.compile_type(element)?;
                let header = self.backend.buffer_header_type(llvm_element);
                let length = self.backend.build_buffer_length(
                    *buffer,
                    header,
                    "buffer.length.slot",
                    "buffer.length",
                )?;
                let out = self
                    .backend
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::UGE,
                        *position,
                        length,
                        "buffer.get.out_of_bounds",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend.build_trap_if(out, span.clone())?;
                let data = self.backend.buffer_data_pointer(*buffer, llvm_element)?;
                let reference = self.backend.build_buffer_element_pointer(
                    data,
                    llvm_element,
                    *position,
                    "buffer.get.reference",
                )?;
                self.backend.register_gc_interior(reference, *buffer)?;
                Ok(reference.as_any_value_enum())
            }
            IntrinsicFunction::BufferPop => self.emit_buffer_pop(call, arguments, &span),
            IntrinsicFunction::BufferFreeze => {
                let [BasicMetadataValueEnum::PointerValue(buffer)] = arguments else {
                    return Err(Diagnostic::new(span, "invalid Buffer handle"));
                };
                let element = intrinsic_buffer_element(call, 0, &span)?;
                let llvm_element = self.backend.compile_type(&element)?;
                let header = self.backend.buffer_header_type(llvm_element);
                let frozen_slot = self
                    .backend
                    .builder
                    .build_struct_gep(header, *buffer, 2, "buffer.frozen.slot")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(
                        frozen_slot,
                        self.backend.context.i8_type().const_int(1, false),
                    )
                    .map_err(compiler_diagnostic)?;
                let length = self.backend.build_buffer_length(
                    *buffer,
                    header,
                    "buffer.length.slot",
                    "buffer.length",
                )?;
                let data = self.backend.buffer_data_pointer(*buffer, llvm_element)?;
                self.backend.register_gc_interior(data, *buffer)?;
                let mut result = self.backend.slice_type().const_zero();
                result = self
                    .backend
                    .builder
                    .build_insert_value(result, data, 0, "buffer.slice.pointer")
                    .map_err(compiler_diagnostic)?
                    .into_struct_value();
                result = self
                    .backend
                    .builder
                    .build_insert_value(result, length, 1, "buffer.slice.length")
                    .map_err(compiler_diagnostic)?
                    .into_struct_value();
                Ok(result.as_any_value_enum())
            }
            IntrinsicFunction::BufferTransfer => self.emit_buffer_transfer(call, arguments, &span),
            IntrinsicFunction::BufferClone => {
                self.emit_buffer_clone(owner, call, call_id, arguments, &span)
            }
            IntrinsicFunction::RefReplace => {
                // read the old payload, store the
                // replacement, return the old value (its drop is the
                // surrounding site's recorded cleanup).
                let [BasicMetadataValueEnum::PointerValue(reference), replacement] = arguments
                else {
                    return Err(Diagnostic::new(
                        span,
                        "replace requires a fixed Ref and a replacement value",
                    ));
                };
                let payload_type = self.backend.compile_type(result_type)?;
                let old = self
                    .backend
                    .builder
                    .build_load(payload_type, *reference, "ref.replace.old")
                    .map_err(compiler_diagnostic)?;
                let replacement = BasicValueEnum::try_from(*replacement)
                    .map_err(|_| Diagnostic::new(span.clone(), "replacement is not storable"))?;
                self.backend
                    .builder
                    .build_store(*reference, replacement)
                    .map_err(compiler_diagnostic)?;
                Ok(old.as_any_value_enum())
            }
            IntrinsicFunction::Drop => {
                // The emitter evaluates the argument, drops it through the
                // recorded `DropIntrinsic` glue, and returns unit.
                let Some(value) = arguments.first().copied() else {
                    return Err(Diagnostic::new(span, "Drop requires a value"));
                };
                let value = BasicValueEnum::try_from(value).map_err(|_| {
                    Diagnostic::new(span.clone(), "Drop argument is not first-class")
                })?;
                self.emit_drop_site(
                    owner,
                    crate::ArtifactUseSite::DropIntrinsic(call_id),
                    DropSource::Value(value),
                    &span,
                )?;
                Ok(self.backend.unit_value())
            }
            IntrinsicFunction::ReactiveScope => {
                // the unit argument is
                // evaluated by the call route; then create the ambient scope.
                Ok(self
                    .backend
                    .build_reactive_runtime_call(
                        "__staple_reactive_scope_create",
                        &[],
                        Some(
                            self.backend
                                .context
                                .ptr_type(AddressSpace::default())
                                .into(),
                        ),
                        "reactive.scope",
                        span.clone(),
                    )?
                    .ok_or_else(|| {
                        Diagnostic::new(
                            call.origin.span.clone(),
                            "reactive scope creation returned no value",
                        )
                    })?
                    .as_any_value_enum())
            }
            IntrinsicFunction::Reaction => Err(unsupported("reaction")),
            IntrinsicFunction::Batch => Err(unsupported("batch")),
            IntrinsicFunction::Snapshot => Err(unsupported("snapshot")),
            IntrinsicFunction::CoroutineBlockOn => {
                let [BasicMetadataValueEnum::PointerValue(frame)] = arguments else {
                    return Err(Diagnostic::new(
                        span,
                        "`block_on` operand is not a coroutine value",
                    ));
                };
                self.emit_coroutine_drive(owner, call, *frame, environment, &span)
            }
            IntrinsicFunction::SchedulerCreate
            | IntrinsicFunction::TaskScope
            | IntrinsicFunction::Spawn
            | IntrinsicFunction::Pump
            | IntrinsicFunction::YieldNow
            | IntrinsicFunction::TaskIsFinished
            | IntrinsicFunction::TaskCancel => {
                self.emit_scheduler_intrinsic(owner, call, intrinsic, arguments, environment)
            }
            IntrinsicFunction::Completion
            | IntrinsicFunction::CompletionWithCancel
            | IntrinsicFunction::CompletionToken
            | IntrinsicFunction::CompletionTokenResolve
            | IntrinsicFunction::CompletionTokenCancel
            | IntrinsicFunction::ResolverComplete
            | IntrinsicFunction::ResolverCancel => {
                self.emit_completion_intrinsic(owner, call, call_id, intrinsic, arguments)
            }
            IntrinsicFunction::Until => Err(unsupported("until")),
        }
    }

    /// The buffer handle's field 0
    /// (`buffer.length`) or 1 (`buffer.capacity`), with the GEP and the load
    /// sharing one name.
    fn emit_buffer_metadata(
        &self,
        call: &crate::LoweredCall,
        arguments: &[BasicMetadataValueEnum<'context>],
        field: u32,
        name: &str,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let [BasicMetadataValueEnum::PointerValue(buffer)] = arguments else {
            return Err(Diagnostic::new(span.clone(), "invalid Buffer handle"));
        };
        let element = intrinsic_buffer_element(call, 0, span)?;
        let llvm_element = self.backend.compile_type(&element)?;
        let header = self.backend.buffer_header_type(llvm_element);
        let slot = self
            .backend
            .builder
            .build_struct_gep(header, *buffer, field, name)
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_load(self.backend.size_type, slot, name)
            .map(|value| value.as_any_value_enum())
            .map_err(compiler_diagnostic)
    }

    /// Empty returns `None`, otherwise the last
    /// element moves out into `Some`, the length decrements, and the vacated
    /// slot is zeroed.
    fn emit_buffer_pop(
        &self,
        call: &crate::LoweredCall,
        arguments: &[BasicMetadataValueEnum<'context>],
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let [BasicMetadataValueEnum::PointerValue(buffer)] = arguments else {
            return Err(Diagnostic::new(span.clone(), "invalid Buffer handle"));
        };
        let element = intrinsic_buffer_element(call, 0, span)?;
        let CheckedType::Sum(option) = &call.result_type else {
            return Err(Diagnostic::new(
                span.clone(),
                "Buffer.pop must return Option T",
            ));
        };
        // Lowering records the `None`/`Some` alternatives per concrete
        // instance; the emitter never searches the sum for them.
        let crate::LoweredOptionAlternatives {
            none: none_index,
            some: some_index,
        } = call.buffer_pop.ok_or_else(|| {
            Diagnostic::new(
                span.clone(),
                "Buffer.pop has no recorded Option alternatives",
            )
        })?;
        let llvm_element = self.backend.compile_type(&element)?;
        let header = self.backend.buffer_header_type(llvm_element);
        self.backend
            .trap_if_buffer_frozen(*buffer, header, span.clone())?;
        let length_slot = self
            .backend
            .builder
            .build_struct_gep(header, *buffer, 0, "buffer.length.slot")
            .map_err(compiler_diagnostic)?;
        let length = self
            .backend
            .builder
            .build_load(self.backend.size_type, length_slot, "buffer.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let empty = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                length,
                self.backend.size_type.const_zero(),
                "buffer.empty",
            )
            .map_err(compiler_diagnostic)?;
        let function = self
            .backend
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "Buffer.pop is in a function"))?;
        let none_block = self
            .backend
            .context
            .append_basic_block(function, "buffer.pop.none");
        let some_block = self
            .backend
            .context
            .append_basic_block(function, "buffer.pop.some");
        let merge = self
            .backend
            .context
            .append_basic_block(function, "buffer.pop.done");
        let option_type = self.backend.compile_sum_type(option)?;
        let result_slot = self
            .backend
            .builder
            .build_alloca(option_type, "buffer.pop.result")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(result_slot, option_type.const_zero())
            .map_err(compiler_diagnostic)?;
        let tag_slot = self
            .backend
            .builder
            .build_struct_gep(option_type, result_slot, 0, "buffer.pop.tag")
            .map_err(compiler_diagnostic)?;
        let payload_slot = self
            .backend
            .builder
            .build_struct_gep(option_type, result_slot, 1, "buffer.pop.payload")
            .map_err(compiler_diagnostic)?;
        let payload_type = option_type.get_field_type_at_index(1).ok_or_else(|| {
            Diagnostic::new(
                staple_syntax::Span::Compiler,
                "internal invariant violated: Option layout has a payload field at index 1",
            )
        })?;
        let storage = super::layout::SumStorage {
            tag: tag_slot,
            payload: payload_slot,
            alignment: self.backend.target_data.get_abi_alignment(&payload_type),
        };
        self.backend
            .builder
            .build_conditional_branch(empty, none_block, some_block)
            .map_err(compiler_diagnostic)?;

        self.backend.builder.position_at_end(none_block);
        self.backend
            .builder
            .build_store(
                tag_slot,
                self.backend
                    .context
                    .i32_type()
                    .const_int(none_index as u64, false),
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_unconditional_branch(merge)
            .map_err(compiler_diagnostic)?;

        self.backend.builder.position_at_end(some_block);
        let next = self
            .backend
            .builder
            .build_int_sub(
                length,
                self.backend.size_type.const_int(1, false),
                "buffer.pop.index",
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(length_slot, next)
            .map_err(compiler_diagnostic)?;
        let data = self.backend.buffer_data_pointer(*buffer, llvm_element)?;
        let (slot, popped) = self.backend.build_buffer_element_load(
            data,
            llvm_element,
            next,
            "buffer.pop.slot",
            "buffer.pop.value",
        )?;
        self.backend.store_sum_payload(
            popped.as_any_value_enum(),
            &option.alternatives[some_index],
            some_index,
            &storage,
            span.clone(),
        )?;
        self.backend
            .builder
            .build_memset(
                slot,
                self.backend.target_data.get_abi_alignment(&llvm_element),
                self.backend.context.i8_type().const_zero(),
                self.backend.size_type.const_int(
                    self.backend.target_data.get_store_size(&llvm_element),
                    false,
                ),
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_unconditional_branch(merge)
            .map_err(compiler_diagnostic)?;

        self.backend.builder.position_at_end(merge);
        self.backend
            .builder
            .build_load(option_type, result_slot, "buffer.pop.option")
            .map(|value| value.as_any_value_enum())
            .map_err(compiler_diagnostic)
    }

    /// Alias and frozen traps, capacity
    /// check, element memcpy, then length updates.
    fn emit_buffer_transfer(
        &self,
        call: &crate::LoweredCall,
        arguments: &[BasicMetadataValueEnum<'context>],
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let [
            BasicMetadataValueEnum::PointerValue(source),
            BasicMetadataValueEnum::PointerValue(destination),
        ] = arguments
        else {
            return Err(Diagnostic::new(
                span.clone(),
                "Buffer.transfer requires two buffers",
            ));
        };
        let element = intrinsic_buffer_element(call, 0, span)?;
        let llvm_element = self.backend.compile_type(&element)?;
        let header = self.backend.buffer_header_type(llvm_element);
        let aliased = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                *source,
                *destination,
                "buffer.transfer.aliased",
            )
            .map_err(compiler_diagnostic)?;
        self.backend.build_trap_if(aliased, span.clone())?;
        self.backend
            .trap_if_buffer_frozen(*source, header, span.clone())?;
        self.backend
            .trap_if_buffer_frozen(*destination, header, span.clone())?;
        let source_length_slot = self
            .backend
            .builder
            .build_struct_gep(header, *source, 0, "buffer.transfer.source.length.slot")
            .map_err(compiler_diagnostic)?;
        let source_length = self
            .backend
            .builder
            .build_load(
                self.backend.size_type,
                source_length_slot,
                "buffer.transfer.source.length",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let dest_length_slot = self
            .backend
            .builder
            .build_struct_gep(header, *destination, 0, "buffer.transfer.dest.length.slot")
            .map_err(compiler_diagnostic)?;
        let dest_length = self
            .backend
            .builder
            .build_load(
                self.backend.size_type,
                dest_length_slot,
                "buffer.transfer.dest.length",
            )
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let dest_capacity = self.backend.build_buffer_capacity(
            *destination,
            header,
            "buffer.transfer.dest.capacity.slot",
            "buffer.transfer.dest.capacity",
        )?;
        let dest_remaining = self
            .backend
            .builder
            .build_int_sub(dest_capacity, dest_length, "buffer.transfer.dest.remaining")
            .map_err(compiler_diagnostic)?;
        let insufficient = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                dest_remaining,
                source_length,
                "buffer.transfer.insufficient_capacity",
            )
            .map_err(compiler_diagnostic)?;
        self.backend.build_trap_if(insufficient, span.clone())?;
        let source_data = self.backend.buffer_data_pointer(*source, llvm_element)?;
        let dest_data = self
            .backend
            .buffer_data_pointer(*destination, llvm_element)?;
        let dest_write = self.backend.build_buffer_element_pointer(
            dest_data,
            llvm_element,
            dest_length,
            "buffer.transfer.dest.write",
        )?;
        let stride = self.backend.target_data.get_abi_size(&llvm_element);
        let bytes = self
            .backend
            .builder
            .build_int_mul(
                source_length,
                self.backend.size_type.const_int(stride, false),
                "buffer.transfer.bytes",
            )
            .map_err(compiler_diagnostic)?;
        let alignment = self.backend.target_data.get_abi_alignment(&llvm_element);
        self.backend
            .builder
            .build_memcpy(dest_write, alignment, source_data, alignment, bytes)
            .map_err(compiler_diagnostic)?;
        let new_dest_length = self
            .backend
            .builder
            .build_int_add(
                dest_length,
                source_length,
                "buffer.transfer.dest.next_length",
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(dest_length_slot, new_dest_length)
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(source_length_slot, self.backend.size_type.const_zero())
            .map_err(compiler_diagnostic)?;
        Ok(self.backend.unit_value())
    }

    /// Allocate a destination with the source's
    /// capacity, install the recorded destination finalizer, then call the
    /// recorded element `Clone` instance per live element.
    fn emit_buffer_clone(
        &self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        call_id: LoweredCallId,
        arguments: &[BasicMetadataValueEnum<'context>],
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let [BasicMetadataValueEnum::PointerValue(source)] = arguments else {
            return Err(Diagnostic::new(span.clone(), "invalid Buffer handle"));
        };
        let element = intrinsic_buffer_element(call, 0, span)?;
        let llvm_element = self.backend.compile_type(&element)?;
        let header = self.backend.buffer_header_type(llvm_element);
        let length = self.backend.build_buffer_length(
            *source,
            header,
            "buffer.clone.source.length.slot",
            "buffer.clone.length",
        )?;
        let capacity = self.backend.build_buffer_capacity(
            *source,
            header,
            "buffer.clone.source.capacity.slot",
            "buffer.clone.capacity",
        )?;
        let destination = self.backend.build_buffer_allocation(
            capacity,
            llvm_element,
            header,
            "buffer.clone",
            span.clone(),
        )?;
        self.backend.build_buffer_capacity_store(
            destination,
            header,
            capacity,
            "buffer.clone.destination.capacity.slot",
        )?;
        if let Some(finalizer) =
            self.artifact_use_function(owner, crate::ArtifactUseSite::BufferCloneFinalizer(call_id))
        {
            self.backend.set_gc_finalizer(destination, finalizer)?;
        }
        let clone_function = self
            .instance_use_function(owner, crate::ArtifactUseSite::BufferCloneElement(call_id))
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "buffer clone has no element Clone instance")
            })?;
        let source_data = self.backend.buffer_data_pointer(*source, llvm_element)?;
        let destination_data = self
            .backend
            .buffer_data_pointer(destination, llvm_element)?;
        let destination_length_slot = self
            .backend
            .builder
            .build_struct_gep(
                header,
                destination,
                0,
                "buffer.clone.destination.length.slot",
            )
            .map_err(compiler_diagnostic)?;
        let index_slot = self
            .backend
            .builder
            .build_alloca(self.backend.size_type, "buffer.clone.index.slot")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(index_slot, self.backend.size_type.const_zero())
            .map_err(compiler_diagnostic)?;
        let function = self
            .backend
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "buffer clone function"))?;
        let condition = self
            .backend
            .context
            .append_basic_block(function, "buffer.clone.condition");
        let body = self
            .backend
            .context
            .append_basic_block(function, "buffer.clone.body");
        let done = self
            .backend
            .context
            .append_basic_block(function, "buffer.clone.done");
        self.backend
            .builder
            .build_unconditional_branch(condition)
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(condition);
        let index = self
            .backend
            .builder
            .build_load(self.backend.size_type, index_slot, "buffer.clone.index")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let has_element = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                index,
                length,
                "buffer.clone.has.element",
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_conditional_branch(has_element, body, done)
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(body);
        let source_slot = self.backend.build_buffer_element_pointer(
            source_data,
            llvm_element,
            index,
            "buffer.clone.source.slot",
        )?;
        let source_element = self
            .backend
            .builder
            .build_load(llvm_element, source_slot, "buffer.clone.source.element")
            .map_err(compiler_diagnostic)?;
        let closure_environment = self
            .backend
            .context
            .ptr_type(AddressSpace::default())
            .const_null();
        let clone_argument: BasicMetadataValueEnum<'context> = if matches!(
            clone_function.get_type().get_param_types().get(1),
            Some(inkwell::types::BasicMetadataTypeEnum::PointerType(_))
        ) {
            source_slot.into()
        } else {
            source_element.into()
        };
        let cloned = self
            .backend
            .builder
            .build_direct_call(
                clone_function,
                &[closure_environment.into(), clone_argument],
                "buffer.clone.element",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| Diagnostic::new(span.clone(), "Clone result is not first-class"))?;
        self.backend.build_buffer_element_store(
            destination_data,
            llvm_element,
            index,
            cloned,
            "buffer.clone.destination.slot",
            span.clone(),
        )?;
        let next = self
            .backend
            .builder
            .build_int_add(
                index,
                self.backend.size_type.const_int(1, false),
                "buffer.clone.next",
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(destination_length_slot, next)
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(index_slot, next)
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_unconditional_branch(condition)
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(done);
        Ok(destination.as_any_value_enum())
    }

    /// The declared function of one owner artifact-use site, when the owner
    /// records it.
    fn artifact_use_function(
        &self,
        owner: EmissionOwner,
        site: crate::ArtifactUseSite,
    ) -> Option<FunctionValue<'context>> {
        let uses = self.view.artifact_uses(owner)?;
        let ordinal = uses
            .iter()
            .find(|use_| use_.site == site)
            .map(|use_| use_.artifact)?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
    }

    /// The declared function of one owner instance-use site, when the owner
    /// records it.
    fn instance_use_function(
        &self,
        owner: EmissionOwner,
        site: crate::ArtifactUseSite,
    ) -> Option<FunctionValue<'context>> {
        let uses = self.view.instance_uses(owner)?;
        let instance = uses
            .iter()
            .find(|use_| use_.site == site)
            .map(|use_| use_.instance)?;
        self.instances.get(&instance).copied()
    }
}

/// The buffer element type from an intrinsic call's argument: the
/// first element of a product argument, or the argument type itself.
fn intrinsic_buffer_element(
    call: &crate::LoweredCall,
    argument: usize,
    span: &staple_syntax::Span,
) -> CodeGenerationResult<Box<CheckedType>> {
    let Some(record) = call.arguments.get(argument) else {
        return Err(Diagnostic::new(
            span.clone(),
            "missing Buffer argument record",
        ));
    };
    let buffer_type = match &record.expected {
        CheckedType::Product(product) if !product.elements.is_empty() => {
            &product.elements[0].value_type
        }
        other => other,
    };
    match buffer_type {
        CheckedType::Buffer(element) => Ok(element.clone()),
        _ => Err(Diagnostic::new(span.clone(), "invalid Buffer type")),
    }
}

/// Store one assembled argument in its final slot, failing
/// when the slot is out of range or already filled (an internal inconsistency
/// in the lowered record).
/// Whether a place-pointer failure is the invariant error for an indexed
/// place: indexed targets dispatch through `MutateIndex` and never have an
/// address, so a borrowed argument must not silently materialize a copy.
fn is_indexed_place_error(diagnostic: &Diagnostic) -> bool {
    diagnostic.message
        == "lowered emitter: internal invariant violated: indexed place is missing or malformed"
}

/// A record- or expression-derived value that must be a pointer; a malformed
/// plan reports a diagnostic instead of panicking.
fn pointer_operand<'context>(
    value: BasicValueEnum<'context>,
    what: &str,
    span: &staple_syntax::Span,
) -> CodeGenerationResult<PointerValue<'context>> {
    match value {
        BasicValueEnum::PointerValue(pointer) => Ok(pointer),
        _ => Err(Diagnostic::new(
            span.clone(),
            format!("{what} is not a pointer"),
        )),
    }
}

/// A record- or expression-derived value that must be a struct (a closure,
/// product, or sum); a malformed plan reports a diagnostic.
fn struct_operand<'context>(
    value: BasicValueEnum<'context>,
    what: &str,
    span: &staple_syntax::Span,
) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
    match value {
        BasicValueEnum::StructValue(value) => Ok(value),
        _ => Err(Diagnostic::new(
            span.clone(),
            format!("{what} is not a struct value"),
        )),
    }
}

fn place_argument_slot<'context>(
    slots: &mut [Option<BasicMetadataValueEnum<'context>>],
    slot: usize,
    value: BasicMetadataValueEnum<'context>,
    span: &staple_syntax::Span,
) -> CodeGenerationResult<()> {
    let Some(destination) = slots.get_mut(slot) else {
        return Err(Diagnostic::new(
            span.clone(),
            "lowered emitter: call argument slot is out of range",
        ));
    };
    if destination.is_some() {
        return Err(Diagnostic::new(
            span.clone(),
            "lowered emitter: duplicate call argument slot",
        ));
    }
    *destination = Some(value);
    Ok(())
}

fn invalid_module_diagnostic(message: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::new(
        staple_syntax::Span::Compiler,
        format!("invalid LLVM module: {message}"),
    )
}
