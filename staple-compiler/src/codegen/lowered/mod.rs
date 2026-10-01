//! Parallel LLVM emitter over the read-only lowered program view.

mod coroutines;
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
    Backend, CodeGenerationResult, Diagnostic, LayoutContext, LoweredCatalogEntry,
    LoweredEmissionReport, compiler_diagnostic, ir::value_as_basic,
};
use crate::lower::{ArenaId, EmissionOwner};

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
    loops: Vec<LoopContext<'context>>,
    /// Stage 5.6 Step 4 (O3): the owned bindings currently in scope, keyed by
    /// symbol. `owned_order` is legacy's `owned_order` registration order;
    /// scope exits drop in reverse from a mark.
    owned: HashMap<SymbolId, OwnedValue<'context>>,
    owned_order: Vec<SymbolId>,
    returned: bool,
    coroutine: Option<coroutines::CoroutineContext<'context>>,
}

/// Stage 5.6 Step 4 (O3): one registered owned binding. A `Value` owns its
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
    /// Legacy `FunctionEnvironment::restore_local_state`: match arms and
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

/// Stage 5.6 Step 3: how a drop position obtains the value it drops.
enum DropSource<'context> {
    /// The value is already evaluated.
    Value(BasicValueEnum<'context>),
    /// Legacy `assignment.old`: load from the target place pointer.
    Place(PointerValue<'context>),
    /// Legacy `mutation.temporary.final`: load from a mutation temporary.
    Temporary(PointerValue<'context>),
    /// Legacy `compile_conditional_cell_drop`: test the cell state, load the
    /// value, expand the glue, then clear the state.
    Cell(PointerValue<'context>),
}

/// One provider's bound resource value. Stage 5.4 reads these when it emits
/// `LoweredResourceUse` reads and call `resource_bindings`.
#[derive(Clone)]
#[allow(dead_code)]
struct BoundResource<'context> {
    resource: CheckedResource,
    value: AnyValueEnum<'context>,
    /// Reads pass through a pointer (`LoweredResourceProvider::indirect`).
    indirect: bool,
}

/// Stage 5.5 Step 8: one active loop's context. Legacy
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
    incoming: Vec<(BasicValueEnum<'context>, BasicBlock<'context>)>,
}

pub(super) struct LoweredEmitter<'program, 'context> {
    view: EmissionView<'program>,
    backend: Backend<'program, 'context>,
    instances: HashMap<FunctionInstanceId, FunctionValue<'context>>,
    artifacts: HashMap<ArtifactOrdinal, Vec<FunctionValue<'context>>>,
    externs: HashMap<SymbolId, FunctionValue<'context>>,
    /// The declared adapter of each extern symbol used as a first-class value
    /// (legacy `closure_codes`). A capture or name read of an extern value
    /// builds this closure instead of looking up local storage.
    extern_adapters: HashMap<SymbolId, FunctionValue<'context>>,
    /// The declared binding symbol of each function template. A `Stored`
    /// callable value loads its closure from that symbol's storage, mirroring
    /// legacy `compile_symbol_value`.
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
    /// (F8). The module is only verified when no body failed; a module with
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

    /// Partial emission (Stage 5.3 Step 2): attempt every body, stub the ones
    /// that fail, stub every artifact whose family has no body emitter yet,
    /// and collect the report. `main` is never stubbed. The returned module
    /// always passes LLVM verification.
    pub(super) fn compile_partial(
        mut self,
        target_machine: &TargetMachine,
    ) -> Result<(LlvmModule<'context>, LoweredEmissionReport), Vec<Diagnostic>> {
        self.declare_program(target_machine)
            .map_err(|diagnostic| vec![diagnostic])?;
        let mut report = LoweredEmissionReport::default();
        self.emit_instance_bodies_partial(&mut report);
        self.emit_artifact_bodies_partial(&mut report);
        self.emit_artifact_stubs(&mut report);
        self.emit_initializers_partial(&mut report);
        self.emit_main().map_err(|diagnostic| vec![diagnostic])?;
        #[cfg(test)]
        self.collect_stage58_blockers(&mut report);
        report.finish();
        self.backend
            .llvm_module
            .verify()
            .map_err(|message| vec![invalid_module_diagnostic(message)])?;
        Ok((self.backend.llvm_module, report))
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
            let name = function
                .get_name()
                .to_str()
                .expect("planned names are UTF-8");
            types.insert(
                name.to_owned(),
                (
                    function.get_type().print_to_string().to_string(),
                    function.get_linkage() == Linkage::Internal,
                ),
            );
        }
        for function in self.initializers.values() {
            let name = function
                .get_name()
                .to_str()
                .expect("initializer names are UTF-8");
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
        // Linkage rule per family (F2, matching legacy): an instance whose
        // template signature still has a type parameter is declared on demand
        // by legacy `ensure_function_specialization` with `Internal` linkage
        // (the recorded lowering fact); an instance of a non-generic template
        // keeps the eager declaration's default (external) linkage. Names
        // always come from the catalog, never from the backend (D2).
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
        // Linkage rule per family (F2, matching legacy): constructor adapters,
        // extern adapters, runners, and the coroutine `resume`/`cleanup` pair
        // are `Internal`; structural methods and GC finalizers keep the
        // default (external) linkage; drop glue emits no function (D3).
        // Coroutine pair names come from the catalog (F3), where
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
            let prefix = self.module_prefix(initializer.module)?;
            let name = format!("__staple_init_m{prefix}");
            if self.backend.llvm_module.get_function(&name).is_some() {
                return Err(Diagnostic::new(
                    initializer.origin.span.clone(),
                    format!("initializer name `{name}` collides with a function"),
                ));
            }
            let function = self.backend.llvm_module.add_function(
                &name,
                function_type,
                Some(Linkage::Internal),
            );
            self.initializers.insert(id, function);
        }
        Ok(())
    }

    /// Stage 5.4 Step 1: a body that owns a droppable binding needs scope-exit
    /// cleanup, which is Stage 5.6's job. Until 5.6 emits it, the body fails
    /// up front, before its root block is emitted, so no drop is silently
    /// skipped (Contract 2). Every collected record has drop glue (the
    /// collector only records types that need drop), and a cell-storage record
    /// is dropped through its cell state, so either fact makes the body 5.6's.
    ///
    /// Step 3 extends the same guard to a captured binding cell whose value
    /// needs drop: legacy attaches a GC finalizer to the cell, which is 5.6's
    /// `GcFinalizer` work, and emitting the cell without it would silently
    /// change behavior.
    ///
    /// Stage 5.6 Step 3 (O1): a drop position looks up its exact artifact-use
    /// record and expands the named drop glue. No record means the value needs
    /// no drop, so the position emits nothing (exactly as legacy does). The
    /// check is record-driven, never type-driven (Contract 1).
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

    /// Stage 5.6 Step 3 (O2): expand one `DropGlueBody` inline at its site,
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
            // Legacy `compile_function`: after the body expression's own
            // scope drops, drop every remaining owned binding (the
            // parameters) before returning (O3).
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
        // Stage 5.5 Step 7: the body block expression's header, which legacy
        // `compile_expression` applies after the block itself. The moved
        // symbols release on every path, like legacy.
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
    /// failure (F8).
    fn emit_instance_bodies(&mut self, diagnostics: &mut Vec<Diagnostic>) {
        for (id, _) in self.view.instances() {
            if let Err(diagnostic) = self.emit_instance_body(id) {
                diagnostics.push(diagnostic);
            }
        }
    }

    /// Partial: attempt every instance body and stub the failed ones.
    fn emit_instance_bodies_partial(&mut self, report: &mut LoweredEmissionReport) {
        for (id, _) in self.view.instances() {
            let Err(diagnostic) = self.emit_instance_body(id) else {
                continue;
            };
            let Some(function) = self.instances.get(&id).copied() else {
                continue;
            };
            self.emit_stub_body(function);
            report.push_stub(
                function_name(function),
                LoweredCatalogEntry::Instance(id.index()),
                diagnostic,
            );
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
            .expect("emitted instance has a body");
        let parameters = function.get_params();
        let environment_pointer = parameters
            .first()
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(body.origin.span.clone(), "missing closure environment")
            })?
            .into_pointer_value();
        environment.closure_environment = Some(environment_pointer);

        // F7: bind the concrete effect-row resources from the body's
        // `function_providers`, in row order, keeping each provider's
        // indirect/borrowed fact. 5.4 resolves `LoweredResourceUse` reads and
        // call `resource_bindings` against these entries. Legacy binds the
        // same list from the checked effect row (`bind_function_parameters`).
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

        // Every capture storage kind, with the same field layout legacy
        // `build_capture_environment` uses (Stage 4.4's `ClosureEnvironment`
        // finalizer plan fixes the order).
        self.bind_instance_captures(body, environment_pointer, environment)?;
        let raw = parameters.get(1 + resource_count..).ok_or_else(|| {
            Diagnostic::new(body.origin.span.clone(), "missing function resources")
        })?;
        let logical_types = flattened_parameter_types(&body.signature.parameter);
        let indirect_mask = self.backend.indirect_parameter_mask(&body.signature);
        let whole = body.signature.mutations.contains(&CheckedMutation::Whole);
        // Legacy `bind_function_parameters`: load every indirect parameter
        // through its pointer (or the single whole-mutation pointer) and keep
        // every one as a mutable pointer for `compile_place_pointer`.
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

    /// Legacy `bind_mutable_parameter_pointers`: every indirect parameter
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

    /// Legacy `bind_top_level_pattern`: a top-level non-product parameter
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

    /// Whether one capture's environment field is a pointer: a cell,
    /// initialization-state slot, derived cell, or borrowed parameter storage
    /// is written and read through its pointer; every other capture (a plain
    /// value or a `Copy` value) stores the value directly. The rule mirrors
    /// legacy `build_capture_environment`'s field selection.
    fn capture_stores_pointer(&self, capture: &LoweredInstanceCapture) -> bool {
        capture.requires_initialization_state
            || capture.mutable_storage
            || capture.derived
            || capture.capture.borrowed
    }

    /// Whether a pointer-kind capture's pointer is borrowed parameter storage
    /// (a mutated or borrowed parameter) rather than a binding cell. Mirrors
    /// legacy `bind_environment_captures`.
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

    /// Legacy `store_global_initialization_state`: write a symbol's
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

    /// Legacy `compile_binding_cell_type` for a plain (non-signal, non-derived)
    /// cell: `{value, state}`. Signal and derived cells carry a third metadata
    /// field and are 5.8 diagnostics, so they never reach this helper.
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
        Ok(self
            .backend
            .context
            .struct_type(&[value_type, self.backend.context.i8_type().into()], false))
    }

    /// Legacy `allocate_binding_cell` for a symbol without reactive storage:
    /// allocate the cell (GC when some function captures it, else the stack),
    /// initialize its state byte to 0, and bind it in the environment. Drop
    /// tracking and the captured-cell finalizer are 5.6, and the Step 1 guard
    /// stops a body whose cell would need either.
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
        // Legacy `captured_cell_symbols`: a cell is GC-allocated exactly when
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
        if captured {
            self.install_cell_finalizer(owner, symbol, cell, span)?;
        } else {
            self.register_owned_binding(owner, environment, symbol, span)?;
        }
        environment.binding_cells.insert(symbol, cell);
        Ok(cell)
    }

    /// Stage 5.6 Step 5 install, needed to delete the owned-binding guard: a
    /// captured droppable binding cell gets the `CellFinalizer` artifact its
    /// `CellFinalizer` use record names (legacy `ensure_cell_finalizer`). No
    /// record means the cell needs no finalizer.
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

    /// Legacy `store_local_initialization_state`: write a cell-backed symbol's
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
        let cell_type = self.binding_cell_type(owner, symbol)?;
        let state_slot = self
            .backend
            .builder
            .build_struct_gep(cell_type, cell, 1, "binding.state")
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        self.backend
            .builder
            .build_store(
                state_slot,
                self.backend.context.i8_type().const_int(state, false),
            )
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// Stage 5.6 Step 4 (O3): register an owned binding from the owner's
    /// recorded `owned_bindings`, mirroring legacy `track_symbol_ownership`
    /// for a value and the owned-cell path of `allocate_binding_cell` for a
    /// cell. A `Value` registration allocates a fresh `i1` live flag set
    /// true, like legacy; a cell registration relies on the cell state.
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

    /// Legacy `drop_owned_since`: emit the conditional drop of every owned
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

    /// Legacy `drop_all_owned`: emit the conditional drop of every owned
    /// binding in reverse registration order. The registrations stay in the
    /// environment, as legacy's do.
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

    /// Legacy's compile-time-only cleanup on a diverged branch: forget the
    /// owned bindings registered since `start` without emitting a drop.
    fn forget_owned_since(environment: &mut FunctionEnvironment<'context>, start: usize) {
        let cleanup_start = start.min(environment.owned_order.len());
        for symbol in &environment.owned_order[cleanup_start..] {
            environment.owned.remove(symbol);
        }
        environment.owned_order.truncate(cleanup_start);
    }

    /// Stage 5.6 Step 4: one owned binding's conditional drop: the live-flag
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
    /// failure (F8).
    fn emit_initializers(&mut self, diagnostics: &mut Vec<Diagnostic>) {
        for (id, _) in self.view.initializers() {
            if let Err(diagnostic) = self.emit_initializer_body(id) {
                diagnostics.push(diagnostic);
            }
        }
    }

    /// Partial: attempt every initializer body and stub the failed ones.
    fn emit_initializers_partial(&mut self, report: &mut LoweredEmissionReport) {
        for (id, _) in self.view.initializers() {
            let Err(diagnostic) = self.emit_initializer_body(id) else {
                continue;
            };
            let function = self.initializers[&id];
            self.emit_stub_body(function);
            report.push_stub(
                function_name(function),
                LoweredCatalogEntry::Initializer(id.index()),
                diagnostic,
            );
        }
    }

    /// Stage 5.4 Step 8: one artifact family that is a call shim. A
    /// constructor adapter rebuilds its product (or GC-allocates the managed
    /// reference and sets the planned payload finalizer); an extern adapter
    /// forwards the closure parameters to the foreign symbol. Every other
    /// family is left to `emit_artifact_stubs`.
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
            // Drop glue emits no function (D3); every other family's body is
            // owned by a later substage, so strict emission reports it rather
            // than leaving an undefined declaration behind.
            LoweredArtifactPlan::DropGlue(_) => Ok(()),
            LoweredArtifactPlan::CoroutineCodes(plan) => {
                self.emit_coroutine_pair(ordinal, plan, &artifact.origin.span)
            }
            other @ (LoweredArtifactPlan::ReactionRunner(_)
            | LoweredArtifactPlan::UntilRunner(_)
            | LoweredArtifactPlan::DerivedRunner(_)) => Err(Diagnostic::new(
                artifact.origin.span.clone(),
                format!(
                    "lowered emitter: {} is not implemented yet",
                    artifact_family(other)
                ),
            )),
        }
    }

    /// Stage 5.6 Step 5: one `GcFinalizer` body, mirroring legacy
    /// `ensure_gc_finalizer` (`Payload`), `ensure_cell_finalizer` (`Cell`),
    /// `ensure_closure_finalizer` (`ClosureEnvironment`), and
    /// `ensure_buffer_finalizer` (`Buffer`).
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
                // layout (legacy's `compile_capture_type`).
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

    /// Partial: attempt every adapter body and stub the failed ones.
    fn emit_artifact_bodies_partial(&mut self, report: &mut LoweredEmissionReport) {
        for (_, artifact) in self.view.artifacts() {
            let Some(plan) = artifact.plan.as_ref() else {
                continue;
            };
            if !matches!(
                plan,
                LoweredArtifactPlan::ConstructorAdapter(_)
                    | LoweredArtifactPlan::ExternAdapter(_)
                    | LoweredArtifactPlan::GcFinalizer(_)
                    | LoweredArtifactPlan::StructuralMethod(_)
                    | LoweredArtifactPlan::CoroutineCodes(_)
            ) {
                continue;
            }
            let ordinal = artifact.ordinal;
            let Err(diagnostic) = self.emit_artifact_body(ordinal) else {
                continue;
            };
            let Some(functions) = self.artifacts.get(&ordinal) else {
                continue;
            };
            for function in functions.clone() {
                self.emit_stub_body(function);
                report.push_stub(
                    function_name(function),
                    LoweredCatalogEntry::Artifact(ordinal.index()),
                    diagnostic.clone(),
                );
            }
        }
    }

    /// Legacy `ensure_constructor_adapter`'s body: rebuild the product from
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

    /// Legacy `declare_external_functions`'s adapter body: forward the closure
    /// parameters (after the environment) to the foreign symbol.
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
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let arguments = function
            .get_params()
            .into_iter()
            .skip(1)
            .map(Into::into)
            .collect::<Vec<_>>();
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

    /// Partial: every artifact function still without a body belongs to a
    /// family with no body emitter yet, so it gets a stub with a missing-family
    /// diagnostic (which resolves F4 for the harness).
    fn emit_artifact_stubs(&mut self, report: &mut LoweredEmissionReport) {
        for (_, artifact) in self.view.artifacts() {
            let Some(plan) = artifact.plan.as_ref() else {
                continue;
            };
            let Some(functions) = self.artifacts.get(&artifact.ordinal) else {
                continue;
            };
            let family = artifact_family(plan);
            for function in functions.clone() {
                if function.count_basic_blocks() > 0 {
                    continue;
                }
                let diagnostic = Diagnostic::new(
                    artifact.origin.span.clone(),
                    format!("lowered emitter: {family} is not implemented yet"),
                );
                self.emit_stub_body(function);
                report.push_stub(
                    function_name(function),
                    LoweredCatalogEntry::Artifact(artifact.ordinal.index()),
                    diagnostic,
                );
            }
        }
    }

    #[cfg(test)]
    fn collect_stage58_blockers(&self, report: &mut LoweredEmissionReport) {
        for stub in report.stubbed().to_vec() {
            let owner = match stub.entry() {
                LoweredCatalogEntry::Instance(index) => Some(EmissionOwner::Instance(
                    crate::FunctionInstanceId::from_index(index),
                )),
                LoweredCatalogEntry::Initializer(index) => Some(EmissionOwner::Initializer(
                    crate::InitializerId::from_index(index),
                )),
                LoweredCatalogEntry::Artifact(index) => {
                    match self
                        .view
                        .artifacts()
                        .find(|(_, artifact)| artifact.ordinal.index() == index)
                        .map(|(_, artifact)| artifact)
                        .and_then(|artifact| artifact.plan.as_ref())
                    {
                        Some(LoweredArtifactPlan::CoroutineCodes(plan)) => {
                            Some(EmissionOwner::Instance(plan.body))
                        }
                        Some(LoweredArtifactPlan::ConstructorAdapter(_))
                        | Some(LoweredArtifactPlan::StructuralMethod(_))
                        | Some(LoweredArtifactPlan::DropGlue(_))
                        | Some(LoweredArtifactPlan::GcFinalizer(_))
                        | Some(LoweredArtifactPlan::ReactionRunner(_))
                        | Some(LoweredArtifactPlan::UntilRunner(_))
                        | Some(LoweredArtifactPlan::DerivedRunner(_))
                        | Some(LoweredArtifactPlan::ExternAdapter(_))
                        | None => None,
                    }
                }
            };
            let mut families = owner
                .map(|owner| self.view.stage58_blockers(owner))
                .unwrap_or_default();
            families.push(super::diagnostic_family(stub.diagnostic()));
            families.sort();
            families.dedup();
            for family in families {
                *report.reached_families.entry(family).or_default() += 1;
            }
        }
    }

    /// A `llvm.trap` followed by `unreachable`: the stub body partial mode
    /// gives a function whose real body is missing or failed. The function
    /// keeps its declaration, name, type, and linkage, so the declaration
    /// census still sees it.
    fn emit_stub_body(&self, function: FunctionValue<'context>) {
        // The failed body's blocks are removed so the stub is the only body.
        // Uses are detached first (every result is replaced by poison, then
        // every instruction is erased) so no deleted value is still used by an
        // instruction in another block, whatever order the uses appear in.
        // Deleting blocks with live cross-block uses trips LLVM's assertions.
        let blocks = function.get_basic_blocks();
        for block in &blocks {
            let mut instruction = block.get_first_instruction();
            while let Some(current) = instruction {
                detach_uses(current);
                instruction = current.get_next_instruction();
            }
        }
        for block in &blocks {
            while let Some(instruction) = block.get_first_instruction() {
                debug_assert!(
                    instruction.get_first_use().is_none(),
                    "a failed-body instruction is still used while the stub replaces it"
                );
                instruction.erase_from_basic_block();
            }
        }
        for block in blocks {
            unsafe {
                block
                    .delete()
                    .expect("a stub target block still belongs to its function");
            }
        }
        let entry = self.backend.context.append_basic_block(function, "stub");
        self.backend.builder.position_at_end(entry);
        let trap = self
            .backend
            .llvm_module
            .get_function("llvm.trap")
            .unwrap_or_else(|| {
                self.backend.llvm_module.add_function(
                    "llvm.trap",
                    self.backend.context.void_type().fn_type(&[], false),
                    None,
                )
            });
        self.backend
            .builder
            .build_direct_call(trap, &[], "stub.trap")
            .expect("stub trap call");
        self.backend
            .builder
            .build_unreachable()
            .expect("stub unreachable");
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
            .expect("GC runtime stack initializer");
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

    fn emit_block(
        &mut self,
        owner: EmissionOwner,
        id: BlockId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let block = self.view.block(owner, id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered block")
        })?;
        // Legacy `compile_block`: every binding the block introduces is owned
        // until the block's normal exit (O3).
        let owned_before = environment.owned_order.len();
        let span = block.origin.span.clone();
        let items = block.items.clone();
        let result = block.result;
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
                format!("lowered emitter: {family} is not implemented yet"),
            )
        };
        let item_span = item.origin.span.clone();
        match item.kind {
            LoweredItemKind::Binding(binding) => {
                if binding.compile_time_only {
                    return Ok(());
                }
                if binding.derived || binding.signal {
                    return Err(unimplemented(if binding.derived {
                        "derived binding"
                    } else {
                        "signal binding"
                    }));
                }
                // The storage-only part of legacy `compile_top_level_item`:
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
                        // Legacy `compile_item` allocates the cell before the
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
                let value = self.emit_expression(owner, value_id, environment)?;
                if let Some(symbol) = binding.symbol {
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
                // Legacy `compile_item` evaluates the statement, then drops
                // its result when one was recorded (O1: the `DiscardedResult`
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
                // Legacy `compile_item`'s return: dispose every reactive
                // scope, then drop every owned binding before leaving the
                // function (O3).
                self.dispose_reactive_scopes(environment, 0, &item_span)?;
                self.drop_all_owned(environment, &item_span)?;
                self.backend
                    .builder
                    .build_return(Some(&value))
                    .map_err(compiler_diagnostic)?;
                environment.returned = true;
                Ok(())
            }
            LoweredItemKind::PatternBinding(binding) => {
                // Legacy `compile_item`'s pattern-binding order: state 1,
                // evaluate, bind, module-global stores, state 2.
                self.store_pattern_initialization_state(owner, binding.pattern, 1)?;
                let value = self.emit_expression(owner, binding.value, environment)?;
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
                    value_as_basic(self.backend.unit_value()).expect("unit is a basic value")
                };
                let Some((exit, owned_before, reactive_before)) = environment
                    .loops
                    .iter()
                    .rev()
                    .find(|context| context.depth == break_item.loop_depth)
                    .map(|context| (context.exit, context.owned_before, context.reactive_before))
                else {
                    return Err(unimplemented("break target"));
                };
                // Legacy `compile_item`'s break disposes the reactive scopes
                // and drops every binding owned since the loop's marks (O3).
                self.dispose_reactive_scopes(environment, reactive_before, &item_span)?;
                self.drop_owned_since(environment, owned_before, &item_span)?;
                self.backend
                    .builder
                    .build_unconditional_branch(exit)
                    .map_err(compiler_diagnostic)?;
                let predecessor = self
                    .backend
                    .builder
                    .get_insert_block()
                    .expect("break block");
                environment
                    .loops
                    .iter_mut()
                    .rev()
                    .find(|context| context.depth == break_item.loop_depth)
                    .expect("break loop context")
                    .incoming
                    .push((value, predecessor));
                environment.returned = true;
                Ok(())
            }
            LoweredItemKind::Continue(item) => {
                let Some((header, owned_before, reactive_before)) = environment
                    .loops
                    .iter()
                    .rev()
                    .find(|context| context.depth == item.loop_depth)
                    .map(|context| {
                        (
                            context.header,
                            context.owned_before,
                            context.reactive_before,
                        )
                    })
                else {
                    return Err(unimplemented("continue target"));
                };
                // Legacy `compile_item`'s continue disposes the reactive scopes
                // and drops every binding owned since the loop's marks (O3).
                self.dispose_reactive_scopes(environment, reactive_before, &item_span)?;
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
                format!("lowered emitter: {family} is not implemented yet"),
            )
        };
        let span = pattern.origin.span.clone();
        match &pattern.kind {
            LoweredPatternKind::Wildcard => {
                // Legacy `bind_pattern_value` drops the discarded subject when
                // lowering recorded a `WildcardDiscard` use (O1).
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
                ..
            } => self.bind_symbol(owner, *symbol, value, environment, &span),
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
                // Legacy `bind_pattern_value`'s nominal arm only loads the
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

    /// One binding pattern's symbol, mirroring legacy `bind_pattern_value`:
    /// a parameter pointer keeps the value in `locals`, a mutable symbol with
    /// no module storage gets a binding cell and state 2, and every other
    /// binding is a plain local. Owned droppable bindings stay stopped by the
    /// body-level guard until 5.6.
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

    /// Legacy `store_pattern_globals`: after a module-level pattern binding
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

    /// Legacy `store_pattern_initialization_state`: write the module
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
        // Legacy `compile_expression`: a diverged body releases moved
        // ownership and returns without coercing.
        if environment.returned {
            self.release_moved_ownership(owner, environment, &expression)?;
            return Ok(value);
        }
        // Legacy `compile_expression`'s divergence handling: an expression of
        // type `Never` (or one coerced from `Never`) ends the block. Order
        // matches legacy: the `unreachable` comes first, then the moved-
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
        // Stage 5.5 Step 4: apply the recorded coercion plan. Lowering already
        // selected the alternatives (E1), so emission never re-selects one.
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

    /// Legacy `compile_expression`'s `release_moved_ownership`: clear the
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
            // Legacy clears the binding cell's state only for a symbol with
            // mutable storage (`has_mutable_storage`); a coroutine frame cell
            // for an ordinary `let` is not cleared.
            if self
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

    /// Stage 5.5 Step 4: execute one `LoweredCoercionPlan`, mirroring legacy
    /// `coerce_value`/`coerce_sum_value`/`coerce_slice_ref_value` instruction
    /// for instruction. `source`/`target` navigate the same recursion so the
    /// nested payload plans and types stay in lockstep.
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
        let unimplemented = |family| {
            Diagnostic::new(
                expression.origin.span.clone(),
                format!("lowered emitter: {family} is not implemented yet"),
            )
        };
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
                // Legacy `compile_expression_uncoerced`'s `Name` path returns
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
                    let family = match operation.kind {
                        LoweredReactiveOperationKind::SignalRead { .. } => "signal read",
                        LoweredReactiveOperationKind::DerivedRead { .. } => "derived read",
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
                    };
                    return Err(unimplemented(family));
                }
                // Legacy `Expression::Name` checks the state whenever the read
                // requires one or the symbol has mutable storage.
                self.load_symbol_value(
                    owner,
                    name.symbol,
                    name.requires_initialization_check || name.mutable,
                    &expression.value_type,
                    &expression.origin.span,
                    environment,
                )
            }
            LoweredExpressionKind::Deferred(_) | LoweredExpressionKind::Stage26Deferred(_) => {
                Err(Diagnostic::new(
                    expression.origin.span.clone(),
                    "internal invariant: deferred expression reached lowered emission",
                ))
            }
            LoweredExpressionKind::String(string) => {
                // Stage 5.4 Step 3: the literal core is shared with legacy.
                self.backend
                    .build_string_literal(&string.value, expression.origin.span.clone())
                    .map(|value| value.as_any_value_enum())
            }
            LoweredExpressionKind::CString(string) => {
                let text =
                    std::str::from_utf8(&string.bytes[..string.bytes.len() - 1]).map_err(|_| {
                        Diagnostic::new(expression.origin.span.clone(), "invalid C string payload")
                    })?;
                // Stage 5.3 Step 5: shared with legacy `build_owned_c_string`.
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
            // Legacy `compile_expression_uncoerced`'s `Satisfies` path is
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

    /// Stage 5.5 Step 4: one structural access read, mirroring legacy
    /// `compile_expression_uncoerced`'s `Access` value path: a dereference
    /// chain of `Ref` payload loads, then the representation, scalar,
    /// product-element, or bounds-checked slice load.
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
            // Legacy returns a scalar access as-is, dereference chain included.
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

    /// Stage 5.5 Step 4: one product construction. Replays `steps` in source
    /// evaluation order (later writes to a slot override earlier ones, exactly
    /// as legacy's designated/named-spread fill does), then assembles the
    /// final `fields` layout through the shared `build_product_value`, whose
    /// one-element collapse matches legacy `compile_product_expression`.
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
                (None, None) => unreachable!("product steps always place or spread"),
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

    /// Stage 5.5 Step 4: `(value; count)`. Legacy evaluates the element once
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

    /// Stage 5.5 Step 9: one string template, mirroring legacy
    /// `compile_string_template`: construct the formatter, write literal parts
    /// through the shared literal core, call each interpolation's bound
    /// `Display`/`Debug` method with the formatter storage, and finish.
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

    /// Stage 5.5 Step 8: one `loop`, mirroring legacy
    /// `compile_loop_expression`. A body that finishes normally drops its
    /// (discarded) result through the E2 hook and takes the back edge; breaks
    /// contribute `loop.value` phi inputs; a loop no `break` reaches ends in
    /// `unreachable`.
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
            incoming: Vec::new(),
        });
        environment.returned = false;
        let value = self.emit_block(owner, loop_.body, environment)?;
        if !environment.returned {
            // Legacy `compile_loop_expression` drops a droppable body result
            // before the back edge (O1: the `LoopBodyResult` use record).
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
        let context = environment
            .loops
            .pop()
            .expect("loop code generation context");
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

    /// Stage 5.5 Step 7: one `match` expression, mirroring legacy
    /// `compile_match_expression` exactly: the subject is evaluated once, each
    /// arm's pattern test runs against the recorded plan, a divergent arm
    /// contributes no phi input, and the fall-through ends in `unreachable`.
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
                // Legacy `compile_match_expression` drops the arm's pattern
                // bindings and locals at the arm's normal exit (O3).
                self.drop_owned_since(environment, owned_before, &arm.origin.span)?;
                self.backend
                    .builder
                    .build_unconditional_branch(merge_block)
                    .map_err(compiler_diagnostic)?;
                let predecessor = self
                    .backend
                    .builder
                    .get_insert_block()
                    .expect("match arm block");
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

    /// One match arm's conditional test, mirroring legacy
    /// `compile_match_pattern_branch` branch for branch and name for name.
    /// Every decision comes from the Step 2 test plan; the emitter only
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
                        success.get_parent().expect("match function"),
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
                        success.get_parent().expect("match function"),
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
                            success.get_parent().expect("match function"),
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
                        success.get_parent().expect("match function"),
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

    /// Stage 5.5 Step 7: a short-circuiting `&&`/`||`, mirroring legacy
    /// `compile_logical_expression` (the shared tag compare and phi builders).
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
            // Legacy `compile_logical_expression` drops the right operand's
            // own bindings at the end of its block (O3).
            self.drop_owned_since(environment, owned_before, &span)?;
            self.backend
                .builder
                .build_unconditional_branch(merge_block)
                .map_err(compiler_diagnostic)?;
            let predecessor = self
                .backend
                .builder
                .get_insert_block()
                .expect("logical right block");
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

    /// Stage 5.5 Step 7: `let pattern? = value`, mirroring legacy
    /// `compile_propagating_binding`: test the success tag, return the failure
    /// value (widened through the recorded plan or extracted as the residual
    /// variant), then bind the success payload and any `at` bindings.
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
            // E1: lowering records the residual alternative; the emitter
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
        // Legacy `compile_propagating_binding`'s failure path drops every
        // owned binding before returning the residual value (O3).
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

    /// Stage 5.3/5.4 Step 7: build one first-class callable value. A `Fresh`
    /// environment is built from the closure plan's captures (legacy
    /// `build_closure`), `Stored` loads the existing closure from the function
    /// binding's local, cell, or module storage, `Current` reuses the
    /// enclosing environment, and the `Constructor`/`External` adapters call
    /// their declared artifact with a null environment.
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
                format!("lowered emitter: {family} is not implemented yet"),
            )
        };
        // Legacy `compile_symbol_value` runs the symbol's initialization check
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
        // artifact (legacy `ensure_constructor_adapter` and `closure_codes`);
        // both carry a null environment.
        if matches!(
            callable.adapter,
            LoweredCallableAdapter::Constructor | LoweredCallableAdapter::External
        ) {
            // Legacy `compile_symbol_value` resolves a parameter pointer, a
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
            LoweredCallableTarget::CompilerHelper { .. } => {
                return Err(unsupported("compiler helper callable value"));
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
                    // Legacy installs the closure-environment finalizer exactly
                    // when the recorded use exists; its body is 5.6.
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
                format!("lowered emitter: {family} is not implemented yet"),
            ));
        };
        self.artifacts
            .get(ordinal)
            .and_then(|values| values.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing callable artifact declaration"))
    }

    /// A `Stored` callable value is an existing closure: legacy
    /// `compile_symbol_value` loads it from the function binding symbol's
    /// local, binding cell, or module storage.
    fn load_stored_closure(
        &mut self,
        owner: EmissionOwner,
        callable: &crate::LoweredCallableValue,
        environment: &FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let unsupported = |family| {
            Diagnostic::new(
                callable.origin.span.clone(),
                format!("lowered emitter: {family} is not implemented yet"),
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
    /// installed; the emitter only reads the use (Contract 1).
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

    /// The capture environment of one closure plan, filled from the current
    /// scope. Empty captures produce a null pointer (legacy
    /// `build_capture_environment`).
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

    /// One closure capture's stored value, mirroring legacy
    /// `build_capture_environment`: a shared cell or borrowed capture stores a
    /// pointer from the current scope; a by-value capture stores its value.
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
                format!("lowered emitter: {family} is not implemented yet"),
            )
        };
        let native_extern = matches!(call.target, LoweredCallableTarget::ExternalFunction { .. });
        // Stage 5.4 Step 4: legacy `compile_intrinsic` and the extern route
        // evaluate arguments through `compile_arguments` (by value), never
        // through an ABI pass mode, so the lowered records' pass modes are
        // ignored there. (A variadic extern's extra parameter slots can even
        // record an indirect mode for the variadic tail, which legacy never
        // materializes.)
        let by_value_route =
            matches!(call.target, LoweredCallableTarget::Intrinsic { .. }) || native_extern;
        // Stage 5.6 Step 7: a reactive intrinsic call names the operation it
        // performs. The plain scope call emits through the intrinsic route
        // (its unit argument is evaluated with the other arguments below);
        // 5.8 owns every other operation.
        if let Some(reactive) = call.reactive {
            let operation = self
                .view
                .reactive_operation(owner, reactive)
                .ok_or_else(|| {
                    Diagnostic::new(call.origin.span.clone(), "missing reactive operation")
                })?;
            if !matches!(operation.kind, LoweredReactiveOperationKind::Scope) {
                return Err(unsupported(reactive_call_family(&operation.kind)));
            }
        }
        // Legacy checks the callee symbol's initialization before evaluating
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
        // Legacy `compile_call_expression`'s constructor branch compiles its
        // single argument whole and never runs the flattened ABI argument
        // path, so a constructor's slot count is its record count.
        if matches!(call.target, LoweredCallableTarget::Constructor { .. }) {
            parameter_count = call.arguments.len();
        }
        let mut slots: Vec<Option<BasicMetadataValueEnum<'context>>> = vec![None; parameter_count];
        // Hidden effect-row resource arguments, in row order. Legacy evaluates
        // its visible arguments first and appends the hidden ones, then passes
        // `[environment, hidden..., visible...]` (`compile_resource_arguments`).
        let mut hidden: Vec<BasicMetadataValueEnum<'context>> = Vec::new();
        // Mutation temporaries whose value needs drop after the call, in
        // evaluation order with their argument record index; `emit_call_cleanup`
        // drops them in reverse.
        let mut cleanups: Vec<(usize, PointerValue<'context>)> = Vec::new();
        let mut invoked = false;
        // A whole-product argument against a flattened multi-element parameter
        // records no ABI slot; legacy `compile_arguments` unpacks the single
        // struct after evaluating it. These values are placed at the end.
        let mut unplaced: Vec<BasicMetadataValueEnum<'context>> = Vec::new();
        let mut callee_value = None;
        // Legacy extracts `closure.code`/`closure.environment` after the
        // visible arguments and before the hidden resources; the parts are
        // materialized lazily at the first resource step (or after the loop).
        let mut callee_parts: Option<(PointerValue<'context>, PointerValue<'context>)> = None;
        for step in &call.steps {
            match step {
                LoweredCallStep::Invoke => {
                    invoked = true;
                    break;
                }
                LoweredCallStep::Callee { expression } => {
                    let value = self.emit_expression(owner, *expression, environment)?;
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
        // Legacy `compile_arguments`' whole-product fallback: one argument
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
        // A C-string temporary is the first visible argument (legacy's
        // `scoped_c_string_temporary` check).
        let cleanup_c_string = if call.c_string_temporary {
            match values.first() {
                Some(BasicMetadataValueEnum::PointerValue(pointer)) => Some(*pointer),
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
            LoweredCallableTarget::CompilerHelper { .. } => {
                Err(unsupported("compiler helper call"))
            }
        };
        let value = result?;
        self.emit_call_cleanup(owner, id, &cleanups, cleanup_c_string, &call.origin.span)?;
        Ok(value)
    }

    /// Stage 5.6 Step 3: the post-call cleanup hook. Legacy
    /// `drop_mutation_temporaries` drops mutation temporaries in reverse
    /// collection order and then releases a C-string temporary; each drop
    /// expands the glue named by its own `CallTemporary`, or `CStringTemporary`,
    /// use record (O1).
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

    /// Legacy `compile_call_expression`'s trait/structural branch: a direct
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
    /// artifact use (legacy `build_ref_value`; the finalizer body is 5.6).
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

    /// Legacy `check_symbol_initialization`: check the symbol's binding cell
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
            let cell_type = self.binding_cell_type(owner, symbol)?;
            let state = self
                .backend
                .builder
                .build_struct_gep(cell_type, cell, 1, "binding.state")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
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
    /// pass mode. Intrinsic routes evaluate by value like legacy
    /// `compile_intrinsic`.
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
        if record.writeback {
            // Lowering does not record a writeback today; diagnose rather
            // than silently dropping the write-back the record demands.
            return Err(Diagnostic::new(
                staple_syntax::Span::Compiler,
                "lowered emitter: call argument writeback is not implemented yet",
            ));
        }
        let Some(expression) = expression else {
            // An implicit thunk argument: legacy `compile_adapted_call_argument`
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
                // Legacy `compile_indirect_argument_pointer` silently falls
                // back to a materialized copy when a possibly-place-rooted
                // borrow is not actually addressable; the mutation path has no
                // fallback. A place kind this substage has not ported yet
                // stays a diagnostic: silently materializing would emit a
                // different body than legacy instead of stubbing.
                Err(error) => {
                    if record.pass_mode != LoweredArgumentPassMode::BorrowedPointer
                        || is_unimplemented_place(&error)
                    {
                        return Err(error);
                    }
                }
            }
        }
        let value = self.emit_expression(owner, expression, environment)?;
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
        // Legacy `build_closure` installs the environment finalizer exactly
        // when lowering recorded the thunk argument's environment use (the
        // finalizer body is 5.6).
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

    /// The capture environment of one instance's body, filled from the current
    /// scope. Empty captures produce a null pointer (legacy
    /// `build_capture_environment`). The closure-environment finalizer is
    /// 5.6's `GcFinalizer`; until then, a capture that legacy would finalize
    /// is a diagnostic rather than a silently missing finalizer.
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

    /// One capture's stored value, mirroring legacy `build_capture_environment`:
    /// a cell/initialization-state/derived/borrowed capture stores a pointer
    /// from the current scope; every other capture stores its value.
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

    /// Stage 5.4 Step 4: the place pointer of a symbol-rooted place. Legacy
    /// `compile_place_pointer`'s lookup order (parameter pointer, then binding
    /// cell, then module global) with the recorded provider for a resource
    /// place. Every other place kind is 5.5's and extends this function; there
    /// is no second place emitter.
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
                format!("lowered emitter: {family} is not implemented yet"),
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
            // Stage 5.5 Step 5: a non-place base materialized so it can be
            // mutated (legacy `compile_mutation_argument_pointer`).
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
            // A `Ref` payload chain: legacy `ref_payload_pointer` leaves the
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
                    // Legacy evaluates the slice value, then loads its pointer
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
                // Legacy checks a mutable symbol base's initialization before
                // projecting a field.
                if let crate::LoweredPlaceKind::Symbol { symbol } = &base_place.kind
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

    /// Stage 5.5 Step 5: one assignment item. An indexed target dispatches
    /// through `MutateIndex`; every other target stores through its place
    /// pointer, with the E2 replaced-value hook, the initialization-state
    /// writeback, and the signal-notification diagnostic in legacy's order.
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
        // Legacy `compile_assignment` conditionally drops a binding cell's
        // old value, or loads `assignment.old` from the place; the owner's
        // `ReplacedValue` use record is the discriminant (O1).
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
        if let Some(symbol) = assignment.initialization_symbol {
            self.store_local_initialization_state(owner, environment, symbol, 2, &span)?;
            self.store_initialization_state(symbol, 2)?;
        }
        if assignment.signal_notify.is_some() {
            return Err(Diagnostic::new(
                span,
                "lowered emitter: signal notify is not implemented yet",
            ));
        }
        Ok(())
    }

    /// The root symbol legacy `compile_place_pointer` returns for a place: a
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

    /// Stage 5.5 Step 5: `base[index] = value`, mirroring legacy
    /// `compile_mutate_index_assignment`: the `IndexedAssignment` binding names
    /// the `MutateIndex` instance (or structural artifact), the base is a
    /// place pointer or a materialized mutation temporary, and the call passes
    /// the base, position, and replacement after the null environment.
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
            unreachable!("indexed assignment is dispatched for an indexed place")
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
        // Legacy compiles the base pointer first, then the position, then the
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
        // Legacy `drop_mutation_temporaries` drops the materialized base when
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

    /// Stage 5.5 Step 5: one `base[index]` read through the `Index` binding.
    /// The operand ABI mask decides a place pointer, a mutation temporary, or a
    /// borrowed temporary; the call is `index.call` with the null environment
    /// and the hidden resources first, exactly as legacy
    /// `compile_index_expression`.
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
                format!("lowered emitter: {family} is not implemented yet"),
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
            // A structural `Index` method's body is Stage 5.7; the declared
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
        // Lowering records which operands pass by address (Contract 1); the
        // mutation mask is the checked method type's own fact.
        let mask = index.operands.indirect.clone();
        if mask.len() != types.len() {
            return Err(unsupported("index operand facts"));
        }
        let mutation_mask =
            super::abi::mutation_parameter_mask(types.len(), &method_type.mutations);
        let mut values: Vec<BasicMetadataValueEnum<'context>> = Vec::new();
        // Mutation temporaries legacy `drop_mutation_temporaries` drops after
        // the call, in collection order; the owned `IndexTemporary` use record
        // names each site's glue (O1).
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
        // Legacy `drop_mutation_temporaries` drops the recorded operand
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

    /// Materialize an indirect call's `closure.code`/`closure.environment`
    /// parts once, at legacy's position (after the visible arguments, before
    /// the hidden resources).
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

    /// Stage 5.4 Step 5: one hidden effect-row resource argument, resolved
    /// through the provider the use records (`compile_resource_arguments`).
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
    /// indirect. Legacy `compile_resource_arguments`' rule.
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

    /// Stage 5.4 Step 5: a `resource` read. Legacy loads `resource.borrow`
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

    /// Stage 5.4 Step 5: a `with` provider and its body. Legacy evaluates the
    /// provider value, stores it in the source place (`Place`) or a
    /// `resource.provider` alloca (`Materialized`), binds it while the body
    /// runs, and disposes a reactive scope on a normal exit. A `Tasks` scope
    /// is 5.8.
    fn emit_with(
        &mut self,
        owner: EmissionOwner,
        id: crate::LoweredWithId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let with = self.view.with(owner, id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered with")
        })?;
        if with.scope_exit == LoweredScopeExit::Tasks {
            return Err(Diagnostic::new(
                with.origin.span.clone(),
                "lowered emitter: task scope is not implemented yet",
            ));
        }
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
        environment.resources.remove(&with.provider);
        result
    }

    /// Legacy `dispose_reactive_scopes`: dispose every scope from `keep` on, in
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

    /// Legacy `compile_symbol_value`: a parameter pointer is reloaded on every
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
            // Legacy's binding-cell arm: build the state slot, run the shared
            // check when the read needs one, then load the value slot.
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
            return self
                .backend
                .builder
                .build_load(llvm_type, value_slot, "binding")
                .map(|value| value.as_any_value_enum())
                .map_err(compiler_diagnostic);
        }
        // Legacy `compile_symbol_value`'s `closure_codes` arm: an extern used
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
        if check_initialization && let Some(state) = self.initialization_states.get(&symbol) {
            self.backend
                .build_initialization_check(state.as_pointer_value(), span.clone())?;
        }
        let llvm_type = self.backend.compile_type(value_type)?;
        self.backend
            .builder
            .build_load(llvm_type, global.as_pointer_value(), "global")
            .map(|value| value.as_any_value_enum())
            .map_err(compiler_diagnostic)
    }

    /// The checked `Bool` representation of a comparison result (legacy
    /// `compile_bool`).
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
                format!("lowered emitter: {family} is not implemented yet"),
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
                // Stage 5.3 Step 5: shared with legacy, names included.
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
                // Stage 5.4 Step 9: shared with legacy's conversion core.
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
                // Stage 5.3 Step 5: shared conversion core; the CString
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
                // Stage 5.3 Step 5: shared with legacy.
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
                // legacy `ensure_buffer_finalizer` installs.
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
                // Legacy `RefReplace`: read the old payload, store the
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
                // Legacy evaluates the argument, drops it through the
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
                // Legacy `compile_intrinsic_call`: the unit argument is
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
            IntrinsicFunction::SchedulerCreate => Err(unsupported("scheduler")),
            IntrinsicFunction::TaskScope => Err(unsupported("task scope")),
            IntrinsicFunction::Spawn => Err(unsupported("spawn")),
            IntrinsicFunction::Pump => Err(unsupported("pump")),
            IntrinsicFunction::YieldNow => Err(unsupported("yield_now")),
            IntrinsicFunction::TaskIsFinished => Err(unsupported("task is_finished")),
            IntrinsicFunction::TaskCancel => Err(unsupported("task cancel")),
            IntrinsicFunction::Completion => Err(unsupported("completion")),
            IntrinsicFunction::CompletionWithCancel => Err(unsupported("completion with cancel")),
            IntrinsicFunction::CompletionToken => Err(unsupported("completion token")),
            IntrinsicFunction::CompletionTokenResolve => {
                Err(unsupported("completion token resolve"))
            }
            IntrinsicFunction::CompletionTokenCancel => Err(unsupported("completion token cancel")),
            IntrinsicFunction::ResolverComplete => Err(unsupported("resolver complete")),
            IntrinsicFunction::ResolverCancel => Err(unsupported("resolver cancel")),
            IntrinsicFunction::Until => Err(unsupported("until")),
        }
    }

    /// Legacy `compile_buffer_metadata`: the buffer handle's field 0
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

    /// Legacy `compile_buffer_pop`: empty returns `None`, otherwise the last
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
        // instance (O1); the emitter never searches the sum for them.
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
        let payload_type = option_type
            .get_field_type_at_index(1)
            .expect("Option payload");
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

    /// Legacy `compile_buffer_transfer`: alias and frozen traps, capacity
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

    /// Legacy `compile_buffer_clone`: allocate a destination with the source's
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

/// The buffer element type legacy reads from an intrinsic call's argument: the
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

/// Stage 5.4 Step 4: store one assembled argument in its final slot, failing
/// when the slot is out of range or already filled (an internal inconsistency
/// in the lowered record).
/// Whether a place-pointer failure is an unported place kind (a construct
/// family) rather than a genuinely unavailable address. The former must stay
/// a stub so the differential body comparison never sees a materialized
/// substitute for legacy's direct pointer.
fn is_unimplemented_place(diagnostic: &Diagnostic) -> bool {
    [
        "temporary place",
        "dereference place",
        "product element place",
        "representation place",
        "indexed place",
    ]
    .iter()
    .any(|family| diagnostic.message == format!("lowered emitter: {family} is not implemented yet"))
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

/// Replaces every use of one instruction's result with a poison value of the
/// same type, so the instruction can be erased while other instructions of
/// the failed body still refer to it. inkwell classifies an instruction by its
/// result type (a phi is an `IntValue`, `PointerValue`, …), so these arms
/// cover every value-producing instruction; void instructions have no uses.
fn detach_uses(instruction: inkwell::values::InstructionValue<'_>) {
    use inkwell::values::AnyValueEnum as Value;
    if instruction.get_first_use().is_none() {
        return;
    }
    match instruction.as_any_value_enum() {
        Value::IntValue(value) => value.replace_all_uses_with(value.get_type().get_poison()),
        Value::FloatValue(value) => value.replace_all_uses_with(value.get_type().get_poison()),
        Value::PointerValue(value) => value.replace_all_uses_with(value.get_type().get_poison()),
        Value::StructValue(value) => value.replace_all_uses_with(value.get_type().get_poison()),
        Value::ArrayValue(value) => value.replace_all_uses_with(value.get_type().get_poison()),
        Value::VectorValue(value) => value.replace_all_uses_with(value.get_type().get_poison()),
        Value::ScalableVectorValue(value) => {
            value.replace_all_uses_with(value.get_type().get_poison())
        }
        _ => {}
    }
}

/// The final LLVM name of a declared function, used by the partial-mode stub
/// records.
fn function_name(function: FunctionValue<'_>) -> String {
    function.get_name().to_string_lossy().into_owned()
}

/// The construct family of one artifact plan, the `<family>` in a partial-mode
/// missing-body diagnostic (`lowered emitter: <family> is not implemented
/// yet`).
fn artifact_family(plan: &LoweredArtifactPlan) -> &'static str {
    match plan {
        LoweredArtifactPlan::ConstructorAdapter(_) => "constructor adapter artifact",
        LoweredArtifactPlan::StructuralMethod(_) => "structural method artifact",
        LoweredArtifactPlan::DropGlue(_) => "drop glue artifact",
        // Stage 5.6 Step 1: the four finalizer subkinds are separate families
        // so progress is tracked per body shape.
        LoweredArtifactPlan::GcFinalizer(finalizer) => match finalizer {
            GcFinalizerPlan::Payload { .. } => "payload finalizer",
            GcFinalizerPlan::Cell { .. } => "cell finalizer",
            GcFinalizerPlan::ClosureEnvironment { .. } => "closure environment finalizer",
            GcFinalizerPlan::Buffer { .. } => "buffer finalizer",
        },
        LoweredArtifactPlan::CoroutineCodes(_) => "coroutine pair artifact",
        LoweredArtifactPlan::ReactionRunner(_) => "reaction runner artifact",
        LoweredArtifactPlan::UntilRunner(_) => "until runner artifact",
        LoweredArtifactPlan::DerivedRunner(_) => "derived runner artifact",
        LoweredArtifactPlan::ExternAdapter(_) => "extern adapter artifact",
    }
}

/// Stage 5.6 Step 1: the construct family of one reactive operation carried by
/// a call. Only the five reactive intrinsics (scope, reaction, batch, `until`,
/// snapshot) are ever attached to a call; the binding and name operations are
/// diagnosed under the general family if one ever appears.
fn reactive_call_family(kind: &LoweredReactiveOperationKind) -> &'static str {
    match kind {
        LoweredReactiveOperationKind::Scope => "reactive scope call",
        LoweredReactiveOperationKind::Reaction { .. } => "reaction call",
        LoweredReactiveOperationKind::Batch { .. } => "batch call",
        LoweredReactiveOperationKind::Until { .. } => "until call",
        LoweredReactiveOperationKind::Snapshot => "snapshot call",
        LoweredReactiveOperationKind::SignalCreate { .. }
        | LoweredReactiveOperationKind::SignalRead { .. }
        | LoweredReactiveOperationKind::SignalNotify { .. }
        | LoweredReactiveOperationKind::DerivedRead { .. }
        | LoweredReactiveOperationKind::DerivedCreate { .. } => "reactive call",
    }
}

fn invalid_module_diagnostic(message: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::new(
        staple_syntax::Span::Compiler,
        format!("invalid LLVM module: {message}"),
    )
}
