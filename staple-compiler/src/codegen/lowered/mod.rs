//! Parallel LLVM emitter over the read-only lowered program view.

use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    module::{Linkage, Module as LlvmModule},
    targets::TargetMachine,
    values::{
        AnyValue, AnyValueEnum, BasicMetadataValueEnum, FunctionValue, GlobalValue, PointerValue,
    },
};

use crate::specialization::ArtifactOrdinal;
use crate::{
    BlockId, CheckedMutation, CheckedResource, CheckedType, EmissionView, ExpressionId,
    FunctionInstanceId, InitializerId, IntegerBinaryOperation, IntrinsicFunction,
    LoweredArgumentPassMode, LoweredArtifactPlan, LoweredBindingSite, LoweredBoundTarget,
    LoweredCallArgument, LoweredCallEnvironment, LoweredCallId, LoweredCallStep,
    LoweredCallableAdapter, LoweredCallableTarget, LoweredCallableValueId,
    LoweredClosureEnvironment, LoweredEntryResourceKind, LoweredExpressionKind, LoweredItemKind,
    LoweredPatternKind, ModuleId, RuntimeRequirement, SymbolId,
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
    resources: Vec<(CheckedResource, AnyValueEnum<'context>, bool)>,
    reactive_scopes: Vec<PointerValue<'context>>,
    loops: Vec<(usize, inkwell::basic_block::BasicBlock<'context>)>,
    returned: bool,
}

pub(super) struct LoweredEmitter<'program, 'context> {
    view: EmissionView<'program>,
    backend: Backend<'program, 'context>,
    instances: HashMap<FunctionInstanceId, FunctionValue<'context>>,
    artifacts: HashMap<ArtifactOrdinal, Vec<FunctionValue<'context>>>,
    externs: HashMap<SymbolId, FunctionValue<'context>>,
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
        if !body.captures.is_empty() {
            let fields = body
                .captures
                .iter()
                .map(|capture| {
                    if capture.requires_initialization_state
                        || capture.mutable_storage
                        || capture.derived
                        || capture.capture.borrowed
                    {
                        Ok(self
                            .backend
                            .context
                            .ptr_type(AddressSpace::default())
                            .into())
                    } else {
                        self.backend.compile_type(&capture.value_type)
                    }
                })
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
                if capture.requires_initialization_state
                    || capture.mutable_storage
                    || capture.derived
                    || capture.capture.borrowed
                {
                    let pointer = value.into_pointer_value();
                    if capture.capture.borrowed
                        || self
                            .view
                            .symbol(symbol)
                            .is_some_and(|s| s.mutated_parameter)
                    {
                        environment.parameter_pointers.insert(symbol, pointer);
                    } else {
                        environment.binding_cells.insert(symbol, pointer);
                    }
                } else {
                    environment.locals.insert(symbol, value.as_any_value_enum());
                }
            }
        }
        let resource_count = body.signature.effects.resources.len();
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
                environment
                    .parameter_pointers
                    .insert(parameter.symbol, pointer);
            }
        }
        Ok(())
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
        for resource in &initializer.resources {
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
                    environment.resources.push((
                        resource.resource.clone(),
                        slot.as_any_value_enum(),
                        true,
                    ));
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
                    environment.resources.push((
                        resource.resource.clone(),
                        scope.as_any_value_enum(),
                        false,
                    ));
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
        // The failed body's blocks are deleted so the stub is the only body.
        // The builder is immediately repositioned, and no deleted value is
        // reused afterwards.
        for block in function.get_basic_blocks() {
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
                if binding.derived || binding.signal || binding.cell {
                    return Err(unimplemented("reactive or cell binding"));
                }
                if let Some(symbol) = binding.symbol
                    && let Some(state) = self.initialization_states.get(&symbol)
                {
                    self.backend
                        .builder
                        .build_store(
                            state.as_pointer_value(),
                            self.backend.context.i8_type().const_int(1, false),
                        )
                        .map_err(compiler_diagnostic)?;
                }
                if binding.generic {
                    if let Some(symbol) = binding.symbol
                        && let Some(state) = self.initialization_states.get(&symbol)
                    {
                        self.backend
                            .builder
                            .build_store(
                                state.as_pointer_value(),
                                self.backend.context.i8_type().const_int(2, false),
                            )
                            .map_err(compiler_diagnostic)?;
                    }
                    return Ok(());
                }
                let Some(value_id) = binding.value else {
                    return Ok(());
                };
                let value = self.emit_expression(owner, value_id, environment)?;
                if let Some(symbol) = binding.symbol {
                    if let Some(global) = self.storage.get(&symbol) {
                        let value =
                            value_as_basic(value).ok_or_else(|| unimplemented("binding value"))?;
                        self.backend
                            .builder
                            .build_store(global.as_pointer_value(), value)
                            .map_err(compiler_diagnostic)?;
                    } else {
                        environment.locals.insert(symbol, value);
                    }
                    if let Some(state) = self.initialization_states.get(&symbol) {
                        self.backend
                            .builder
                            .build_store(
                                state.as_pointer_value(),
                                self.backend.context.i8_type().const_int(2, false),
                            )
                            .map_err(compiler_diagnostic)?;
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
        let unimplemented = |family| {
            Diagnostic::new(
                expression.origin.span.clone(),
                format!("lowered emitter: {family} is not implemented yet"),
            )
        };
        if expression.coercion.is_some() || !expression.moved_symbols.is_empty() {
            return Err(unimplemented("coercion or move"));
        }
        match expression.kind {
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
            LoweredExpressionKind::Block(block) => self.emit_block(owner, block, environment),
            LoweredExpressionKind::Name(name) => {
                if name.requires_initialization_check || name.reactive.is_some() {
                    return Err(unimplemented("checked or reactive name"));
                }
                if let Some(value) = environment.locals.get(&name.symbol) {
                    return Ok(*value);
                }
                if environment.binding_cells.contains_key(&name.symbol)
                    || environment.parameter_pointers.contains_key(&name.symbol)
                {
                    return Err(unimplemented("cell or parameter pointer read"));
                }
                let global = self
                    .storage
                    .get(&name.symbol)
                    .ok_or_else(|| unimplemented("name"))?;
                let llvm_type = self.backend.compile_type(&expression.value_type)?;
                self.backend
                    .builder
                    .build_load(llvm_type, global.as_pointer_value(), "global")
                    .map(|value| value.as_any_value_enum())
                    .map_err(compiler_diagnostic)
            }
            LoweredExpressionKind::Deferred(_) => Err(unimplemented("deferred expression")),
            LoweredExpressionKind::Stage26Deferred(_) => Err(unimplemented("Stage 2.6 expression")),
            LoweredExpressionKind::String(_) => Err(unimplemented("string")),
            LoweredExpressionKind::CString(string) => {
                let text =
                    std::str::from_utf8(&string.bytes[..string.bytes.len() - 1]).map_err(|_| {
                        Diagnostic::new(expression.origin.span.clone(), "invalid C string payload")
                    })?;
                let source = self
                    .backend
                    .builder
                    .build_global_string_ptr(text, "c_string.literal")
                    .map_err(compiler_diagnostic)?
                    .as_pointer_value();
                let length = self
                    .backend
                    .size_type
                    .const_int(string.bytes.len() as u64, false);
                let pointer = self
                    .backend
                    .builder
                    .build_array_malloc(self.backend.context.i8_type(), length, "c_string.data")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_memcpy(pointer, 1, source, 1, length)
                    .map_err(compiler_diagnostic)?;
                Ok(pointer.as_any_value_enum())
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
            LoweredExpressionKind::Call(call) => self.emit_call(owner, call, environment),
            LoweredExpressionKind::CallableValue(callable) => {
                self.emit_callable_value(owner, callable, environment)
            }
            LoweredExpressionKind::Resource(_) => Err(unimplemented("resource")),
            LoweredExpressionKind::With(_) => Err(unimplemented("with")),
            LoweredExpressionKind::Coro(_) => Err(unimplemented("coro")),
            LoweredExpressionKind::Await(_) => Err(unimplemented("await")),
        }
    }

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
        if callable.requires_initialization_check
            || callable.adapter != LoweredCallableAdapter::None
        {
            return Err(unsupported("callable adapter or initialization check"));
        }
        let pointer = match callable.closure.as_ref().map(|plan| plan.environment) {
            Some(LoweredClosureEnvironment::Fresh) => {
                return Err(unsupported("fresh closure environment"));
            }
            Some(LoweredClosureEnvironment::Stored) => match &callable.target {
                LoweredCallableTarget::DirectFunction { function, .. }
                    if self
                        .view
                        .function(*function)
                        .is_some_and(|template| template.captures.is_empty()) =>
                {
                    self.backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null()
                }
                _ => return Err(unsupported("stored closure")),
            },
            Some(LoweredClosureEnvironment::Current) => environment
                .closure_environment
                .ok_or_else(|| unsupported("current closure environment"))?,
            Some(LoweredClosureEnvironment::None) | None => self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null(),
        };
        let binding = self
            .view
            .binding(owner, LoweredBindingSite::CallableValue(id))
            .ok_or_else(|| unsupported("callable binding"))?;
        let code = match &callable.target {
            LoweredCallableTarget::DirectFunction { .. }
            | LoweredCallableTarget::TraitImplementation { .. } => {
                let LoweredBoundTarget::Instance(instance) = binding else {
                    return Err(unsupported("callable instance binding"));
                };
                self.instances
                    .get(instance)
                    .copied()
                    .ok_or_else(|| unsupported("callable instance declaration"))?
            }
            LoweredCallableTarget::Constructor { .. }
            | LoweredCallableTarget::StructuralTraitMethod { .. }
            | LoweredCallableTarget::ExternalFunction { .. } => {
                let LoweredBoundTarget::Artifact(ordinal) = binding else {
                    return Err(unsupported("callable artifact binding"));
                };
                self.artifacts
                    .get(ordinal)
                    .and_then(|values| values.first())
                    .copied()
                    .ok_or_else(|| unsupported("callable artifact declaration"))?
            }
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
        let mut closure = self.backend.closure_type().const_zero();
        closure = self
            .backend
            .builder
            .build_insert_value(
                closure,
                code.as_global_value().as_pointer_value(),
                0,
                "closure.code",
            )
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        closure = self
            .backend
            .builder
            .build_insert_value(closure, pointer, 1, "closure.environment")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        Ok(closure.as_any_value_enum())
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
        let c_string_conversion = matches!(
            call.target,
            LoweredCallableTarget::Intrinsic {
                intrinsic: IntrinsicFunction::StringFromCString
                    | IntrinsicFunction::StringToCString,
                ..
            }
        );
        let native_extern = matches!(call.target, LoweredCallableTarget::ExternalFunction { .. });
        let string_constructor = matches!(call.target, LoweredCallableTarget::Constructor { .. })
            && call.result_type == CheckedType::String;
        if !call.resource_bindings.is_empty()
            || !call.initialization_checks.is_empty()
            || (call.c_string_temporary && !native_extern)
            || call.reactive.is_some()
            || !call.mutations.is_empty()
            || (!c_string_conversion && !string_constructor && !call.moves.is_empty())
        {
            return Err(unsupported("call resources, mutation, or cleanup"));
        }
        if native_extern
            && matches!(
                call.function_type.parameter.as_ref(),
                CheckedType::Product(product) if product.variadic
            )
        {
            // Legacy routes variadic extern arguments through
            // `compile_arguments(.., is_var_arg)`; 5.4 ports that handling.
            return Err(unsupported("variadic extern call"));
        }
        let mut parameter_count = flattened_parameter_types(&call.function_type.parameter).len();
        if matches!(call.function_type.parameter.as_ref(), CheckedType::Product(product) if product.variadic)
        {
            for step in &call.steps {
                let slot = match step {
                    LoweredCallStep::Argument { argument } => call
                        .arguments
                        .get(*argument)
                        .and_then(|argument| argument.slot),
                    LoweredCallStep::ProductElement { slot, .. }
                    | LoweredCallStep::Default { slot, .. } => Some(*slot),
                    LoweredCallStep::Callee { .. }
                    | LoweredCallStep::ProductSpread { .. }
                    | LoweredCallStep::NamedProductSpread { .. }
                    | LoweredCallStep::Resource { .. }
                    | LoweredCallStep::Invoke => None,
                };
                if let Some(slot) = slot {
                    parameter_count = parameter_count.max(slot + 1);
                }
            }
        }
        let mut slots: Vec<Option<BasicMetadataValueEnum<'context>>> = vec![None; parameter_count];
        let mut invoked = false;
        let mut callee_value = None;
        for step in &call.steps {
            let (slot, expression) = match step {
                LoweredCallStep::Argument { argument } => {
                    let record = call
                        .arguments
                        .get(*argument)
                        .ok_or_else(|| unsupported("call argument"))?;
                    let slot = record
                        .slot
                        .ok_or_else(|| unsupported("materialized argument"))?;
                    let expression = record
                        .expression
                        .ok_or_else(|| unsupported("implicit thunk"))?;
                    if let Some(family) = call_argument_diagnostic(record, c_string_conversion) {
                        return Err(unsupported(family));
                    }
                    (slot, expression)
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
                    if let Some(family) = call_argument_diagnostic(record, c_string_conversion) {
                        return Err(unsupported(family));
                    }
                    (*slot, *expression)
                }
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
                LoweredCallStep::ProductSpread { .. } => {
                    return Err(unsupported("product spread call"));
                }
                LoweredCallStep::NamedProductSpread { .. } => {
                    return Err(unsupported("named spread call"));
                }
                LoweredCallStep::Default { .. } => return Err(unsupported("default argument")),
                LoweredCallStep::Resource { .. } => return Err(unsupported("resource argument")),
            };
            let value = self.emit_expression(owner, expression, environment)?;
            let value = value_as_basic(value).ok_or_else(|| unsupported("call argument value"))?;
            let destination = slots
                .get_mut(slot)
                .ok_or_else(|| unsupported("call argument slot"))?;
            if destination.is_some() {
                return Err(unsupported("duplicate argument slot"));
            }
            *destination = Some(value.into());
        }
        if !invoked || slots.iter().any(Option::is_none) {
            return Err(unsupported("incomplete call"));
        }
        let values = slots.into_iter().map(Option::unwrap).collect::<Vec<_>>();
        let binding = self
            .view
            .binding(owner, LoweredBindingSite::Call(id))
            .ok_or_else(|| unsupported("call binding"))?;
        match &call.target {
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
                self.emit_intrinsic(*intrinsic, &values, call.origin.span.clone())
            }
            LoweredCallableTarget::IndirectClosure { .. } => {
                if !matches!(binding, LoweredBoundTarget::Route(_)) {
                    return Err(unsupported("indirect call binding"));
                }
                let closure = callee_value.ok_or_else(|| unsupported("indirect callee"))?;
                let code = self
                    .backend
                    .builder
                    .build_extract_value(closure, 0, "closure.code")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let pointer = self
                    .backend
                    .builder
                    .build_extract_value(closure, 1, "closure.environment")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
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
                    let free = self
                        .backend
                        .llvm_module
                        .get_function("free")
                        .ok_or_else(|| unsupported("CString cleanup function"))?;
                    self.backend
                        .builder
                        .build_direct_call(free, &[(*pointer).into()], "c_string.drop")
                        .map_err(compiler_diagnostic)?;
                }
                Ok(result.try_as_basic_value().basic().map_or_else(
                    || self.backend.unit_value(),
                    |value| value.as_any_value_enum(),
                ))
            }
            LoweredCallableTarget::Constructor { .. } if string_constructor => {
                let [BasicMetadataValueEnum::StructValue(value)] = values.as_slice() else {
                    return Err(unsupported("String constructor representation"));
                };
                Ok(value.as_any_value_enum())
            }
            LoweredCallableTarget::Constructor { .. } => Err(unsupported("constructor call")),
            LoweredCallableTarget::TraitImplementation { .. } => Err(unsupported("trait call")),
            LoweredCallableTarget::StructuralTraitMethod { .. } => {
                Err(unsupported("structural call"))
            }
            LoweredCallableTarget::CompilerHelper { .. } => {
                Err(unsupported("compiler helper call"))
            }
        }
    }

    fn emit_intrinsic(
        &self,
        intrinsic: IntrinsicFunction,
        arguments: &[BasicMetadataValueEnum<'context>],
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
                let name = format!("{}.arithmetic", integer.intrinsic_name());
                let value = match operation {
                    IntegerBinaryOperation::Add => {
                        self.backend.builder.build_int_add(*left, *right, &name)
                    }
                    IntegerBinaryOperation::Subtract => {
                        self.backend.builder.build_int_sub(*left, *right, &name)
                    }
                    IntegerBinaryOperation::Multiply => {
                        self.backend.builder.build_int_mul(*left, *right, &name)
                    }
                    IntegerBinaryOperation::Divide if integer.is_signed() => self
                        .backend
                        .builder
                        .build_int_signed_div(*left, *right, &name),
                    IntegerBinaryOperation::Divide => self
                        .backend
                        .builder
                        .build_int_unsigned_div(*left, *right, &name),
                }
                .map_err(compiler_diagnostic)?;
                Ok(value.as_any_value_enum())
            }
            IntrinsicFunction::ToString { .. } => Err(unsupported("numeric string conversion")),
            IntrinsicFunction::IntegerCompare { .. } => Err(unsupported("integer comparison")),
            IntrinsicFunction::FloatBinary { .. } => Err(unsupported("float arithmetic")),
            IntrinsicFunction::FloatCompare { .. } => Err(unsupported("float comparison")),
            IntrinsicFunction::StringFromCString => self.string_from_c_string(arguments, span),
            IntrinsicFunction::StringToCString => self.string_to_c_string(arguments, span),
            IntrinsicFunction::StringAdd => Err(unsupported("string addition")),
            IntrinsicFunction::SliceLength => Err(unsupported("slice length")),
            IntrinsicFunction::SliceGetRef => Err(unsupported("slice reference")),
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

    fn string_from_c_string(
        &self,
        arguments: &[BasicMetadataValueEnum<'context>],
        span: staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let [BasicMetadataValueEnum::PointerValue(source)] = arguments else {
            return Err(Diagnostic::new(
                span,
                "CString conversion requires a pointer",
            ));
        };
        let strlen = self
            .backend
            .llvm_module
            .get_function("strlen")
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing CString length function"))?;
        let length = self
            .backend
            .builder
            .build_direct_call(strlen, &[(*source).into()], "c_string.length")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let validator = self
            .backend
            .llvm_module
            .get_function("__staple_is_valid_utf8")
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing UTF-8 validator"))?;
        let valid = self
            .backend
            .builder
            .build_direct_call(
                validator,
                &[(*source).into(), length.into()],
                "c_string.valid_utf8",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let invalid = self
            .backend
            .builder
            .build_not(valid, "c_string.invalid_utf8")
            .map_err(compiler_diagnostic)?;
        self.backend.build_trap_if(invalid, span.clone())?;
        let pointer = self
            .backend
            .build_gc_allocation(length, "string.data", span.clone())?;
        self.backend
            .builder
            .build_memcpy(pointer, 1, *source, 1, length)
            .map_err(compiler_diagnostic)?;
        let mut result = self.backend.slice_type().const_zero();
        result = self
            .backend
            .builder
            .build_insert_value(result, pointer, 0, "string.pointer")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        result = self
            .backend
            .builder
            .build_insert_value(result, length, 1, "string.length")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        let free = self
            .backend
            .llvm_module
            .get_function("free")
            .ok_or_else(|| Diagnostic::new(span, "missing CString release function"))?;
        self.backend
            .builder
            .build_direct_call(free, &[(*source).into()], "c_string.drop")
            .map_err(compiler_diagnostic)?;
        Ok(result.as_any_value_enum())
    }

    fn string_to_c_string(
        &self,
        arguments: &[BasicMetadataValueEnum<'context>],
        span: staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let [BasicMetadataValueEnum::StructValue(string)] = arguments else {
            return Err(Diagnostic::new(
                span,
                "String conversion requires a String value",
            ));
        };
        let pointer = self
            .backend
            .builder
            .build_extract_value(*string, 0, "string.pointer")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let length = self
            .backend
            .builder
            .build_extract_value(*string, 1, "string.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let memchr = self
            .backend
            .llvm_module
            .get_function("memchr")
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing interior-NUL check function"))?;
        let nul = self
            .backend
            .builder
            .build_direct_call(
                memchr,
                &[
                    pointer.into(),
                    self.backend.context.i32_type().const_zero().into(),
                    length.into(),
                ],
                "string.interior_nul",
            )
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_pointer_value();
        let has_nul = self
            .backend
            .builder
            .build_is_not_null(nul, "string.has_interior_nul")
            .map_err(compiler_diagnostic)?;
        self.backend.build_trap_if(has_nul, span.clone())?;
        let allocation_length = self
            .backend
            .builder
            .build_int_add(
                length,
                self.backend.size_type.const_int(1, false),
                "c_string.length",
            )
            .map_err(compiler_diagnostic)?;
        let overflow = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULT,
                allocation_length,
                length,
                "c_string.length_overflow",
            )
            .map_err(compiler_diagnostic)?;
        self.backend.build_trap_if(overflow, span.clone())?;
        let result = self
            .backend
            .builder
            .build_array_malloc(
                self.backend.context.i8_type(),
                allocation_length,
                "c_string.data",
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_memcpy(result, 1, pointer, 1, length)
            .map_err(compiler_diagnostic)?;
        let terminator = unsafe {
            self.backend.builder.build_gep(
                self.backend.context.i8_type(),
                result,
                &[length],
                "c_string.terminator",
            )
        }
        .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(terminator, self.backend.context.i8_type().const_zero())
            .map_err(compiler_diagnostic)?;
        Ok(result.as_any_value_enum())
    }
}

/// Stage 5.3 F5: the argument-record facts the supported call routes can
/// compile. Legacy's intrinsic conversions (`StringFromCString`/
/// `StringToCString`) evaluate their argument expression directly, so the
/// recorded pass mode is unused there; every other supported route passes each
/// argument by value in its ABI slot. A materialized argument, a writeback, or
/// a post-call cleanup has no legacy treatment on these routes and stays a
/// diagnostic instead of being silently skipped (Contract 2).
fn call_argument_diagnostic(
    record: &LoweredCallArgument,
    c_string_conversion: bool,
) -> Option<&'static str> {
    if record.writeback {
        return Some("call argument writeback");
    }
    if record.drops_after_call {
        return Some("call argument cleanup");
    }
    if record.temporary {
        return Some("materialized call argument");
    }
    if !c_string_conversion && record.pass_mode != LoweredArgumentPassMode::Value {
        return Some("call argument pass mode");
    }
    None
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
