//! Parallel LLVM emitter over the read-only lowered program view.

use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    module::{Linkage, Module as LlvmModule},
    targets::TargetMachine,
    values::{AnyValue, AnyValueEnum, FunctionValue, GlobalValue, PointerValue},
};

use crate::specialization::ArtifactOrdinal;
use crate::{
    BlockId, CheckedMutation, CheckedResource, CheckedType, EmissionView, ExpressionId,
    FunctionInstanceId, InitializerId, LoweredArtifactPlan, LoweredEntryResourceKind,
    LoweredExpressionKind, LoweredItemKind, ModuleId, RuntimeRequirement, SymbolId,
};

use super::abi::flattened_parameter_types;
use super::{
    Backend, CodeGenerationResult, Diagnostic, LayoutContext, compiler_diagnostic,
    ir::value_as_basic,
};
use crate::lower::EmissionOwner;

#[derive(Default)]
struct FunctionEnvironment<'context> {
    locals: HashMap<SymbolId, AnyValueEnum<'context>>,
    binding_cells: HashMap<SymbolId, PointerValue<'context>>,
    parameter_pointers: HashMap<SymbolId, PointerValue<'context>>,
    resources: Vec<(CheckedResource, AnyValueEnum<'context>, bool)>,
    reactive_scopes: Vec<PointerValue<'context>>,
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

    pub(super) fn compile(
        mut self,
        target_machine: &TargetMachine,
    ) -> CodeGenerationResult<LlvmModule<'context>> {
        self.declare_program(target_machine)?;
        self.emit_instance_bodies()?;
        self.emit_initializers()?;
        self.emit_main()?;
        self.backend.llvm_module.verify().map_err(|message| {
            Diagnostic::new(
                staple_syntax::Span::Compiler,
                format!("invalid LLVM module: {message}"),
            )
        })?;
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
    ) -> CodeGenerationResult<HashMap<String, String>> {
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
                function.get_type().print_to_string().to_string(),
            );
        }
        for function in self.initializers.values() {
            let name = function
                .get_name()
                .to_str()
                .expect("initializer names are UTF-8");
            types.insert(
                name.to_owned(),
                function.get_type().print_to_string().to_string(),
            );
        }
        Ok(types)
    }

    fn declare_instances(&mut self) -> CodeGenerationResult<()> {
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
            let function = self
                .backend
                .llvm_module
                .add_function(name, function_type, None);
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
                    vec![
                        self.backend
                            .llvm_module
                            .add_function(name, ty, Some(Linkage::Internal)),
                    ]
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
                    let status = self.backend.context.struct_type(
                        &[self.backend.context.i8_type().into(), pointer.into()],
                        false,
                    );
                    let resume = self.backend.llvm_module.add_function(
                        &format!("{name}_resume"),
                        status.fn_type(&[pointer.into()], false),
                        Some(Linkage::Internal),
                    );
                    let cleanup = self.backend.llvm_module.add_function(
                        &format!("{name}_cleanup"),
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

    fn emit_instance_bodies(&mut self) -> CodeGenerationResult<()> {
        for (id, instance) in self.view.instances() {
            let Some(body) = instance.body.as_ref() else {
                continue;
            };
            let Some(function) = self.instances.get(&id).copied() else {
                continue;
            };
            let Some(root) = body.root else {
                continue;
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
        }
        Ok(())
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

    fn emit_initializers(&mut self) -> CodeGenerationResult<()> {
        for (id, initializer) in self.view.initializers() {
            let function = self.initializers[&id];
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
        }
        Ok(())
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
            LoweredItemKind::PatternBinding(_) => Err(unimplemented("pattern binding")),
            LoweredItemKind::Assignment(_) => Err(unimplemented("assignment")),
            LoweredItemKind::Break(_) => Err(unimplemented("break")),
            LoweredItemKind::Continue(_) => Err(unimplemented("continue")),
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
            LoweredExpressionKind::CString(_) => Err(unimplemented("C string")),
            LoweredExpressionKind::Access(_) => Err(unimplemented("access")),
            LoweredExpressionKind::Product(_) => Err(unimplemented("product")),
            LoweredExpressionKind::RepeatedProduct(_) => Err(unimplemented("repeated product")),
            LoweredExpressionKind::Satisfies(_) => Err(unimplemented("satisfies")),
            LoweredExpressionKind::Logical(_) => Err(unimplemented("logical")),
            LoweredExpressionKind::Loop(_) => Err(unimplemented("loop")),
            LoweredExpressionKind::Match(_) => Err(unimplemented("match")),
            LoweredExpressionKind::Index(_) => Err(unimplemented("index")),
            LoweredExpressionKind::StringTemplate(_) => Err(unimplemented("string template")),
            LoweredExpressionKind::Call(_) => Err(unimplemented("call")),
            LoweredExpressionKind::CallableValue(_) => Err(unimplemented("callable value")),
            LoweredExpressionKind::Resource(_) => Err(unimplemented("resource")),
            LoweredExpressionKind::With(_) => Err(unimplemented("with")),
            LoweredExpressionKind::Coro(_) => Err(unimplemented("coro")),
            LoweredExpressionKind::Await(_) => Err(unimplemented("await")),
        }
    }
}
