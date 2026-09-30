//! Parallel LLVM emitter over the read-only lowered program view.

use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    module::{Linkage, Module as LlvmModule},
    targets::TargetMachine,
    values::{
        AnyValue, AnyValueEnum, BasicMetadataValueEnum, BasicValueEnum, FunctionValue, GlobalValue,
        PointerValue,
    },
};

use crate::specialization::ArtifactOrdinal;
use crate::{
    BlockId, CheckedMutation, CheckedResource, CheckedType, EmissionView, ExpressionId,
    FunctionInstanceId, InitializerId, IntrinsicFunction, LoweredArgumentPassMode,
    LoweredArtifactPlan, LoweredBindingSite, LoweredBoundTarget, LoweredCallArgument,
    LoweredCallEnvironment, LoweredCallId, LoweredCallStep, LoweredCallableAdapter,
    LoweredCallableTarget, LoweredCallableValueId, LoweredClosureEnvironment,
    LoweredEntryResourceKind, LoweredExpressionKind, LoweredInstanceCapture, LoweredItemKind,
    LoweredPatternKind, LoweredProviderStorage, LoweredResourceProviderId, LoweredScopeExit,
    ModuleId, OwnedStorage, RuntimeRequirement, SymbolId,
};

use super::abi::flattened_parameter_types;
use super::{
    Backend, CodeGenerationResult, Diagnostic, LayoutContext, LoweredCatalogEntry,
    LoweredEmissionReport, compiler_diagnostic, ir::value_as_basic,
};
use crate::lower::{ArenaId, EmissionOwner};

#[derive(Default)]
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
    loops: Vec<(usize, inkwell::basic_block::BasicBlock<'context>)>,
    returned: bool,
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

pub(super) struct LoweredEmitter<'program, 'context> {
    view: EmissionView<'program>,
    backend: Backend<'program, 'context>,
    instances: HashMap<FunctionInstanceId, FunctionValue<'context>>,
    artifacts: HashMap<ArtifactOrdinal, Vec<FunctionValue<'context>>>,
    externs: HashMap<SymbolId, FunctionValue<'context>>,
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
                    vec![
                        self.backend
                            .llvm_module
                            .add_function(name, ty, Some(Linkage::Internal)),
                    ]
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
    fn guard_owned_bindings(
        &self,
        owner: EmissionOwner,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let owned = self.view.owned_bindings(owner).is_some_and(|bindings| {
            bindings
                .iter()
                .any(|record| record.glue.is_some() || matches!(record.storage, OwnedStorage::Cell))
        });
        let cell_finalizer = self.view.artifact_uses(owner).is_some_and(|uses| {
            uses.iter()
                .any(|use_| matches!(use_.site, crate::ArtifactUseSite::CellFinalizer(_)))
        });
        if owned || cell_finalizer {
            return Err(Diagnostic::new(
                span.clone(),
                "lowered emitter: owned binding cleanup is not implemented yet",
            ));
        }
        Ok(())
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
        self.guard_owned_bindings(EmissionOwner::Instance(id), &body.origin.span)?;
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let mut environment = FunctionEnvironment::default();
        self.bind_parameters(id, function, &mut environment)?;
        let value = self.emit_block(EmissionOwner::Instance(id), root, &mut environment)?;
        if !environment.returned {
            let result = value_as_basic(value).ok_or_else(|| {
                Diagnostic::new(
                    body.origin.span.clone(),
                    "function result is not a first-class value",
                )
            })?;
            self.backend
                .builder
                .build_return(Some(&result))
                .map_err(compiler_diagnostic)?;
        }
        Ok(())
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
        &self,
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
        let raw = parameters.get(1 + resource_count..).ok_or_else(|| {
            Diagnostic::new(body.origin.span.clone(), "missing function resources")
        })?;
        let logical_types = flattened_parameter_types(&body.signature.parameter);
        let indirect_mask = self.backend.indirect_parameter_mask(&body.signature);
        let whole = body.signature.mutations.contains(&CheckedMutation::Whole);
        if body.parameters.len() != logical_types.len() || (whole && logical_types.len() > 1) {
            return Err(Diagnostic::new(
                body.origin.span.clone(),
                "lowered emitter: parameter destructuring is not implemented yet",
            ));
        }
        // A nominal destructuring parameter (`Ref left`) binds a payload
        // symbol whose concrete type differs from the signature slot; real
        // pattern traversal is 5.5's, so stop before emitting a wrongly typed
        // binding.
        for (parameter, logical) in body.parameters.iter().zip(logical_types.iter()) {
            if &parameter.value_type != *logical {
                return Err(Diagnostic::new(
                    body.origin.span.clone(),
                    "lowered emitter: parameter destructuring is not implemented yet",
                ));
            }
        }
        for (index, parameter) in body.parameters.iter().enumerate() {
            let (value, pointer) = if whole || indirect_mask[index] {
                let pointer = raw
                    .get(if whole { 0 } else { index })
                    .ok_or_else(|| {
                        Diagnostic::new(body.origin.span.clone(), "missing function parameter")
                    })?
                    .into_pointer_value();
                let llvm_type = self.backend.compile_type(logical_types[index])?;
                let value = self
                    .backend
                    .builder
                    .build_load(llvm_type, pointer, "parameter.value")
                    .map_err(compiler_diagnostic)?;
                (value, Some(pointer))
            } else {
                (
                    *raw.get(index).ok_or_else(|| {
                        Diagnostic::new(body.origin.span.clone(), "missing function parameter")
                    })?,
                    None,
                )
            };
            environment
                .locals
                .insert(parameter.symbol, value.as_any_value_enum());
            if let Some(pointer) = pointer {
                // F7/legacy `bind_mutable_parameter_pointers`: a whole or
                // indirect parameter pointer is the place every mutable read
                // and write goes through, so it replaces any capture cell
                // registered for the same symbol.
                environment.binding_cells.remove(&parameter.symbol);
                environment
                    .parameter_pointers
                    .insert(parameter.symbol, pointer);
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
        symbol: SymbolId,
    ) -> CodeGenerationResult<inkwell::types::StructType<'context>> {
        let record = self.view.symbol(symbol).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing binding cell symbol")
        })?;
        let value_type = self.backend.compile_type(&record.value_type)?;
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
        environment: &mut FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            return Ok(cell);
        }
        let cell_type = self.binding_cell_type(symbol)?;
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
        environment.binding_cells.insert(symbol, cell);
        Ok(cell)
    }

    /// Legacy `store_local_initialization_state`: write a cell-backed symbol's
    /// state byte, and do nothing when the symbol has no cell.
    fn store_local_initialization_state(
        &self,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        state: u64,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let Some(cell) = environment.binding_cells.get(&symbol).copied() else {
            return Ok(());
        };
        let cell_type = self.binding_cell_type(symbol)?;
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

    /// One module initializer body: entry IO/reactive resources, the lowered
    /// body, then reactive-scope disposal and return.
    fn emit_initializer_body(&mut self, id: InitializerId) -> CodeGenerationResult<()> {
        let function = self.initializers[&id];
        let initializer = self.view.initializer(id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered initializer")
        })?;
        self.guard_owned_bindings(EmissionOwner::Initializer(id), &initializer.origin.span)?;
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
            _ => Ok(()),
        }
    }

    /// Strict: attempt every adapter body, collecting one diagnostic per
    /// failure.
    fn emit_artifact_bodies(&mut self, diagnostics: &mut Vec<Diagnostic>) {
        for (_, artifact) in self.view.artifacts() {
            if !matches!(
                artifact.plan,
                Some(LoweredArtifactPlan::ConstructorAdapter(_))
                    | Some(LoweredArtifactPlan::ExternAdapter(_))
            ) {
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
                LoweredArtifactPlan::ConstructorAdapter(_) | LoweredArtifactPlan::ExternAdapter(_)
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
        let items = block.items.clone();
        let result = block.result;
        for item in items {
            self.emit_item(owner, item, environment)?;
            if environment.returned {
                return Ok(self.backend.unit_value());
            }
        }
        match result {
            Some(expression) => self.emit_expression(owner, expression, environment),
            None => Ok(self.backend.unit_value()),
        }
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
        match item.kind {
            LoweredItemKind::Binding(binding) => {
                if binding.compile_time_only {
                    return Ok(());
                }
                if binding.derived || binding.signal {
                    return Err(unimplemented("reactive or cell binding"));
                }
                // The storage-only part of legacy `compile_top_level_item`:
                // a generic binding records state 1 then 2 and evaluates
                // nothing; a valued binding records state 1, evaluates,
                // stores a module global when the symbol owns one (a nested
                // local stays in the environment), then records state 2.
                if binding.generic {
                    if let Some(symbol) = binding.symbol {
                        self.store_local_initialization_state(
                            environment,
                            symbol,
                            1,
                            &item.origin.span,
                        )?;
                        self.store_local_initialization_state(
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
                        self.allocate_binding_cell(environment, symbol, &item.origin.span)?;
                    }
                    self.store_local_initialization_state(
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
                        let cell_type = self.binding_cell_type(symbol)?;
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
                    }
                }
                Ok(())
            }
            LoweredItemKind::Expression(statement) => {
                if statement.drop_result {
                    return Err(unimplemented("expression result cleanup"));
                }
                self.emit_expression(owner, statement.expression, environment)?;
                Ok(())
            }
            LoweredItemKind::Return(item) => {
                if matches!(owner, EmissionOwner::Initializer(_)) {
                    return Err(unimplemented("initializer return"));
                }
                let value = self.emit_expression(owner, item.value, environment)?;
                let value = value_as_basic(value).ok_or_else(|| unimplemented("return value"))?;
                self.backend
                    .builder
                    .build_return(Some(&value))
                    .map_err(compiler_diagnostic)?;
                environment.returned = true;
                Ok(())
            }
            LoweredItemKind::PatternBinding(binding) => {
                if binding.propagating {
                    return Err(unimplemented("propagating pattern binding"));
                }
                let value = self.emit_expression(owner, binding.value, environment)?;
                self.bind_pattern(owner, binding.pattern, value, environment)
            }
            LoweredItemKind::Assignment(_) => Err(unimplemented("assignment")),
            LoweredItemKind::Break(_) => Err(unimplemented("break")),
            LoweredItemKind::Continue(item) => {
                let Some((_, header)) = environment
                    .loops
                    .iter()
                    .rev()
                    .find(|(depth, _)| *depth == item.loop_depth)
                else {
                    return Err(unimplemented("continue target"));
                };
                self.backend
                    .builder
                    .build_unconditional_branch(*header)
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
        match pattern.kind {
            LoweredPatternKind::Wildcard => {
                if !self.view.concrete_is_copy(&pattern.value_type) {
                    return Err(unsupported("wildcard cleanup"));
                }
                Ok(())
            }
            LoweredPatternKind::Binding {
                symbol: Some(symbol),
                mutable: false,
                moved: false,
                ..
            } => {
                environment.locals.insert(symbol, value);
                Ok(())
            }
            LoweredPatternKind::Binding { .. } => {
                Err(unsupported("mutable or moved pattern binding"))
            }
            LoweredPatternKind::Product { .. } => Err(unsupported("product pattern binding")),
            LoweredPatternKind::Nominal { argument, .. }
                if pattern.value_type == CheckedType::String =>
            {
                self.bind_pattern(owner, argument, value, environment)
            }
            LoweredPatternKind::Nominal { .. } => Err(unsupported("nominal pattern binding")),
            LoweredPatternKind::Literal { .. } => Err(unsupported("literal pattern binding")),
            LoweredPatternKind::At { .. } => Err(unsupported("at pattern binding")),
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
        let value = self.emit_expression_value(owner, &expression, environment)?;
        // Legacy `compile_expression`'s divergence handling: an expression of
        // type `Never` (or one coerced from `Never`) ends the block. Order
        // matches legacy: the `unreachable` comes first, then the moved-
        // ownership release.
        let diverges = !environment.returned
            && (expression.value_type == CheckedType::Never
                || expression
                    .coercion
                    .as_ref()
                    .is_some_and(|coercion| coercion.source == CheckedType::Never));
        if diverges {
            self.backend
                .builder
                .build_unreachable()
                .map_err(compiler_diagnostic)?;
            environment.returned = true;
        }
        // Legacy `compile_expression`'s `release_moved_ownership`: clear the
        // initialization state of every symbol the expression moved out of a
        // binding cell. The live-flag store for an owned droppable value is
        // 5.6; the Step 1 guard already stopped bodies that own one.
        for symbol in &expression.moved_symbols {
            self.store_local_initialization_state(
                environment,
                *symbol,
                0,
                &expression.origin.span,
            )?;
        }
        Ok(value)
    }

    fn emit_expression_value(
        &mut self,
        owner: EmissionOwner,
        expression: &crate::LoweredExpression,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let unimplemented = |family| {
            Diagnostic::new(
                expression.origin.span.clone(),
                format!("lowered emitter: {family} is not implemented yet"),
            )
        };
        if expression.coercion.is_some() {
            return Err(unimplemented("coercion or move"));
        }
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
                if name.reactive.is_some() {
                    return Err(unimplemented("checked or reactive name"));
                }
                // Legacy `Expression::Name` checks the state whenever the read
                // requires one or the symbol has mutable storage.
                self.load_symbol_value(
                    name.symbol,
                    name.requires_initialization_check || name.mutable,
                    &expression.value_type,
                    &expression.origin.span,
                    environment,
                )
            }
            LoweredExpressionKind::Deferred(_) => Err(unimplemented("deferred expression")),
            LoweredExpressionKind::Stage26Deferred(_) => Err(unimplemented("Stage 2.6 expression")),
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
            LoweredExpressionKind::Access(_) => Err(unimplemented("access")),
            LoweredExpressionKind::Product(product) if product.fields.is_empty() => {
                Ok(self.backend.unit_value())
            }
            LoweredExpressionKind::Product(_) => Err(unimplemented("product")),
            LoweredExpressionKind::RepeatedProduct(_) => Err(unimplemented("repeated product")),
            LoweredExpressionKind::Satisfies(_) => Err(unimplemented("satisfies")),
            LoweredExpressionKind::Logical(_) => Err(unimplemented("logical")),
            LoweredExpressionKind::Loop(loop_) => {
                if loop_.drops_body_result || loop_.result_type != CheckedType::Never {
                    return Err(unimplemented("loop value or cleanup"));
                }
                let function = self
                    .backend
                    .builder
                    .get_insert_block()
                    .and_then(|block| block.get_parent())
                    .ok_or_else(|| {
                        Diagnostic::new(expression.origin.span.clone(), "loop is not in a function")
                    })?;
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
                    .map_err(compiler_diagnostic)?;
                self.backend.builder.position_at_end(header);
                environment.loops.push((loop_.depth, header));
                environment.returned = false;
                self.emit_block(owner, loop_.body, environment)?;
                if !environment.returned {
                    self.backend
                        .builder
                        .build_unconditional_branch(header)
                        .map_err(compiler_diagnostic)?;
                }
                environment.loops.pop();
                self.backend.builder.position_at_end(exit);
                self.backend
                    .builder
                    .build_unreachable()
                    .map_err(compiler_diagnostic)?;
                environment.returned = true;
                Ok(self.backend.unit_value())
            }
            LoweredExpressionKind::Match(_) => Err(unimplemented("match")),
            LoweredExpressionKind::Index(_) => Err(unimplemented("index")),
            LoweredExpressionKind::StringTemplate(_) => Err(unimplemented("string template")),
            LoweredExpressionKind::Call(call) => self.emit_call(owner, *call, environment),
            LoweredExpressionKind::CallableValue(callable) => {
                self.emit_callable_value(owner, *callable, environment)
            }
            LoweredExpressionKind::Resource(use_id) => {
                self.emit_resource_read(owner, *use_id, environment)
            }
            LoweredExpressionKind::With(with_id) => self.emit_with(owner, *with_id, environment),
            LoweredExpressionKind::Coro(_) => Err(unimplemented("coro")),
            LoweredExpressionKind::Await(_) => Err(unimplemented("await")),
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
            self.check_symbol_initialization(environment, symbol, &callable.origin.span)?;
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
            let code = self.callable_artifact_code(
                binding,
                &callable.origin.span,
                "callable adapter binding",
            )?;
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
                    return self.load_stored_closure(callable, environment);
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
            let cell_type = self.binding_cell_type(symbol)?;
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
        let uses = self.view.artifact_uses(owner)?;
        let ordinal = uses.iter().find_map(|use_| {
            (use_.site == crate::ArtifactUseSite::ClosureEnvironment(id)).then_some(use_.artifact)
        })?;
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
            let stored = self.closure_capture_value(capture, environment, span)?;
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
                let value =
                    self.load_symbol_value(symbol, false, &capture.value_type, span, environment)?;
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
        // 5.8 owns reactive calls.
        if call.reactive.is_some() {
            return Err(unsupported("reactive call"));
        }
        // Legacy checks the callee symbol's initialization before evaluating
        // any argument.
        for symbol in &call.initialization_checks {
            self.check_symbol_initialization(environment, *symbol, &call.origin.span)?;
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
        let mut slots: Vec<Option<BasicMetadataValueEnum<'context>>> = vec![None; parameter_count];
        // Hidden effect-row resource arguments, in row order. Legacy evaluates
        // its visible arguments first and appends the hidden ones, then passes
        // `[environment, hidden..., visible...]` (`compile_resource_arguments`).
        let mut hidden: Vec<BasicMetadataValueEnum<'context>> = Vec::new();
        // Mutation temporaries whose value needs drop after the call, in
        // evaluation order; `emit_call_cleanup` drops them in reverse.
        let mut cleanups: Vec<(PointerValue<'context>, CheckedType)> = Vec::new();
        let mut invoked = false;
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
                    let slot = record.slot.unwrap_or(0);
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
                    place_argument_slot(&mut slots, slot, value, &call.origin.span)?;
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
        if !invoked || slots.iter().any(Option::is_none) {
            return Err(unsupported("incomplete call"));
        }
        self.ensure_callee_parts(&callee_value, &mut callee_parts)?;
        let mut values = hidden;
        values.extend(slots.into_iter().map(Option::unwrap));
        // A non-extern C-string temporary is the first visible argument
        // (legacy's `scoped_c_string_temporary` check).
        let cleanup_c_string = if !native_extern && call.c_string_temporary {
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
                self.emit_intrinsic(
                    *intrinsic,
                    &values,
                    &call.result_type,
                    call.origin.span.clone(),
                )
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
                if call.c_string_temporary
                    && let Some(BasicMetadataValueEnum::PointerValue(pointer)) = values.first()
                {
                    // Stage 5.3 Step 5: the shared CString release.
                    self.backend
                        .build_free_c_string(*pointer, call.origin.span.clone())?;
                }
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
            // Legacy `compile_call_expression`'s trait branch: a direct call
            // with a null environment, whatever the evidence recipe says.
            LoweredCallableTarget::TraitImplementation { .. } => {
                let LoweredBoundTarget::Instance(instance) = binding else {
                    return Err(unsupported("trait call binding"));
                };
                let function = self
                    .instances
                    .get(instance)
                    .copied()
                    .ok_or_else(|| unsupported("trait function declaration"))?;
                self.emit_structural_call(owner, &call, function, &values)
            }
            LoweredCallableTarget::StructuralTraitMethod { .. } => {
                let LoweredBoundTarget::Artifact(ordinal) = binding else {
                    return Err(unsupported("structural call binding"));
                };
                let function = self
                    .artifacts
                    .get(ordinal)
                    .and_then(|functions| functions.first())
                    .copied()
                    .ok_or_else(|| unsupported("structural function declaration"))?;
                self.emit_structural_call(owner, &call, function, &values)
            }
            LoweredCallableTarget::CompilerHelper { .. } => {
                Err(unsupported("compiler helper call"))
            }
        };
        let value = result?;
        self.emit_call_cleanup(&cleanups, cleanup_c_string, &call.origin.span)?;
        Ok(value)
    }

    /// Stage 5.4 Step 4: the post-call cleanup hook. Legacy drops mutation
    /// temporaries in reverse argument order (only when their value needs
    /// drop) and then frees a non-extern call's C-string temporary. The
    /// signature and placement are final for 5.6, which fills in the drop
    /// bodies; until then any recorded cleanup remains a 5.6 diagnostic
    /// instead of being silently skipped.
    fn emit_call_cleanup(
        &mut self,
        temporaries: &[(PointerValue<'context>, CheckedType)],
        c_string: Option<PointerValue<'context>>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        if !temporaries.is_empty() || c_string.is_some() {
            return Err(Diagnostic::new(
                span.clone(),
                "lowered emitter: call argument cleanup is not implemented yet",
            ));
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
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            let cell_type = self.binding_cell_type(symbol)?;
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
        cleanups: &mut Vec<(PointerValue<'context>, CheckedType)>,
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
            return self.pass_computed_argument(record, value, by_value_route, cleanups);
        };
        if !by_value_route
            && matches!(
                record.pass_mode,
                LoweredArgumentPassMode::BorrowedPointer | LoweredArgumentPassMode::MutablePlace
            )
            && let Some(place) = record.place
        {
            return Ok(self.emit_place_pointer(owner, place, environment)?.into());
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
        self.pass_computed_argument(record, value, by_value_route, cleanups)
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
        // Legacy `build_closure` installs a GC finalizer when a capture needs
        // drop. No closure-environment use is recorded for a thunk argument,
        // so the finalizer body (5.6) cannot be attached yet; fail instead of
        // building an environment legacy would finalize.
        if self.instance_capture_needs_finalizer(*instance) {
            return Err(Diagnostic::new(
                span.clone(),
                "lowered emitter: fresh closure environment is not implemented yet",
            ));
        }
        self.build_instance_closure(*instance, environment, span)
            .map(|closure| closure.into())
    }

    /// Whether an instance's fresh capture environment needs a GC finalizer,
    /// legacy `build_capture_environment`'s install gate (a captured value
    /// that neither carries initialization state nor is borrowed needs drop).
    fn instance_capture_needs_finalizer(&self, instance: FunctionInstanceId) -> bool {
        self.view
            .instance(instance)
            .and_then(|record| record.body.as_ref())
            .is_some_and(|body| {
                body.captures.iter().any(|capture| {
                    !capture.requires_initialization_state
                        && !capture.capture.borrowed
                        && self.view.concrete_needs_drop(&capture.value_type)
                })
            })
    }

    /// Build one instance's closure value over the current environment: the
    /// catalog function's code pointer and a fresh capture environment filled
    /// from the instance's capture records (legacy `build_closure`).
    fn build_instance_closure(
        &mut self,
        instance: FunctionInstanceId,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<inkwell::values::StructValue<'context>> {
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
        let pointer = self.build_capture_environment_value(body, environment, span)?;
        self.backend.build_closure_value(function, pointer)
    }

    /// The capture environment of one instance's body, filled from the current
    /// scope. Empty captures produce a null pointer (legacy
    /// `build_capture_environment`). The closure-environment finalizer is
    /// 5.6's `GcFinalizer`; until then, a capture that legacy would finalize
    /// is a diagnostic rather than a silently missing finalizer.
    fn build_capture_environment_value(
        &mut self,
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
            let stored = self.capture_value(capture, environment, span)?;
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
            self.load_symbol_value(symbol, false, &capture.value_type, span, environment)?;
        value_as_basic(value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "captured value is not first-class"))
    }

    /// Materialize an already-evaluated value according to its recorded pass
    /// mode. Spread elements are evaluated once, so they cannot re-evaluate
    /// their expression; their placements never have a source place.
    fn pass_computed_argument(
        &self,
        record: &LoweredCallArgument,
        value: BasicValueEnum<'context>,
        by_value_route: bool,
        cleanups: &mut Vec<(PointerValue<'context>, CheckedType)>,
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
                    cleanups.push((pointer, record.expected.clone()));
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
        cleanups: &mut Vec<(PointerValue<'context>, CheckedType)>,
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
            let passed = self.pass_computed_argument(record, element, by_value_route, cleanups)?;
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
                    let cell_type = self.binding_cell_type(*symbol)?;
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
                let cell_type = self.binding_cell_type(*symbol)?;
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
            // 5.5 extends `emit_place_pointer` for these place kinds.
            crate::LoweredPlaceKind::Temporary { .. } => Err(unsupported("temporary place")),
            crate::LoweredPlaceKind::Dereference { .. } => Err(unsupported("dereference place")),
            crate::LoweredPlaceKind::ProductElement { .. } => {
                Err(unsupported("product element place"))
            }
            crate::LoweredPlaceKind::Representation { .. } => {
                Err(unsupported("representation place"))
            }
            crate::LoweredPlaceKind::Indexed { .. } => Err(unsupported("indexed place")),
        }
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
            let cell_type = self.binding_cell_type(symbol)?;
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
        &self,
        intrinsic: IntrinsicFunction,
        arguments: &[BasicMetadataValueEnum<'context>],
        result_type: &CheckedType,
        span: staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
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
                // Stage 5.3 Step 5: shared with legacy.
                let result = self
                    .backend
                    .build_string_from_c_string(*source, span.clone())?;
                self.backend.build_free_c_string(*source, span)?;
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
            IntrinsicFunction::BufferWithCapacity => Err(unsupported("buffer allocation")),
            IntrinsicFunction::BufferLength => Err(unsupported("buffer length")),
            IntrinsicFunction::BufferCapacity => Err(unsupported("buffer capacity")),
            IntrinsicFunction::BufferPush => Err(unsupported("buffer push")),
            IntrinsicFunction::BufferPop => Err(unsupported("buffer pop")),
            IntrinsicFunction::BufferGet => Err(unsupported("buffer get")),
            IntrinsicFunction::BufferFreeze => Err(unsupported("buffer freeze")),
            IntrinsicFunction::BufferTransfer => Err(unsupported("buffer transfer")),
            IntrinsicFunction::BufferClone => Err(unsupported("buffer clone")),
            IntrinsicFunction::RefReplace => Err(unsupported("reference replacement")),
            IntrinsicFunction::Drop => Err(unsupported("drop")),
            IntrinsicFunction::ReactiveScope => Err(unsupported("reactive scope")),
            IntrinsicFunction::Reaction => Err(unsupported("reaction")),
            IntrinsicFunction::Batch => Err(unsupported("batch")),
            IntrinsicFunction::Snapshot => Err(unsupported("snapshot")),
            IntrinsicFunction::CoroutineBlockOn => Err(unsupported("coroutine block_on")),
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
}

/// Stage 5.4 Step 4: store one assembled argument in its final slot, failing
/// when the slot is out of range or already filled (an internal inconsistency
/// in the lowered record).
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
        LoweredArtifactPlan::GcFinalizer(_) => "GC finalizer artifact",
        LoweredArtifactPlan::CoroutineCodes(_) => "coroutine pair artifact",
        LoweredArtifactPlan::ReactionRunner(_) => "reaction runner artifact",
        LoweredArtifactPlan::UntilRunner(_) => "until runner artifact",
        LoweredArtifactPlan::DerivedRunner(_) => "derived runner artifact",
        LoweredArtifactPlan::ExternAdapter(_) => "extern adapter artifact",
    }
}

fn invalid_module_diagnostic(message: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::new(
        staple_syntax::Span::Compiler,
        format!("invalid LLVM module: {message}"),
    )
}
