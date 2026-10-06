//! Descriptor ABI emission. Unsupported dynamic constructs fail explicitly;
//! they cannot accidentally fall through to the concrete SSA emitter.
use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    values::{BasicValue, FunctionValue, PointerValue},
};

use super::{CodeGenerationResult, Diagnostic, LoweredEmitter, compiler_diagnostic};
use crate::lower::{ArenaId, EmissionOwner};
use crate::{
    BlockId, CheckedType, ExpressionId, FunctionInstanceId, LoweredBindingSite, LoweredBoundTarget,
    LoweredCallEnvironment, LoweredCallStep, LoweredCallableTarget, LoweredCoercionPlan,
    LoweredExpressionKind, LoweredInstanceBody, LoweredItemKind, LoweredPatternKind, PatternId,
    PlaceId, SymbolId, TypeParameterId,
};

#[derive(Clone, Copy)]
struct DynamicBinding<'context> {
    pointer: PointerValue<'context>,
    descriptor: PointerValue<'context>,
    owned: bool,
    live: bool,
}

/// One bound resource provider in a dynamic body. The dynamic ABI passes every
/// resource as a pointer, so reads load the value through it.
#[derive(Clone)]
struct DynamicResource<'context> {
    value_type: CheckedType,
    pointer: PointerValue<'context>,
}

/// One active dynamic loop. `break` writes its value into `result` (allocated
/// once in the entry block) and branches to `exit`; `breaks` proves the exit
/// is reachable.
struct DynamicLoopContext<'context> {
    depth: usize,
    header: inkwell::basic_block::BasicBlock<'context>,
    exit: inkwell::basic_block::BasicBlock<'context>,
    result: Option<PointerValue<'context>>,
    result_descriptor: PointerValue<'context>,
    binding_mark: usize,
    breaks: bool,
}

/// Runtime buffer layout from an element descriptor.
struct DynamicBufferLayout<'context> {
    store_size: inkwell::values::IntValue<'context>,
    stride: inkwell::values::IntValue<'context>,
    header_offset: inkwell::values::IntValue<'context>,
}

/// The branch-local part of the dynamic environment. Match arms and loop
/// bodies restore it so bindings introduced on one path never leak to another.
#[derive(Clone)]
struct DynamicState<'context> {
    bindings: HashMap<SymbolId, DynamicBinding<'context>>,
    binding_order: Vec<SymbolId>,
    returned: bool,
}

struct DynamicEnvironment<'context> {
    descriptors: HashMap<TypeParameterId, PointerValue<'context>>,
    compound_descriptors: Vec<(CheckedType, PointerValue<'context>)>,
    bindings: HashMap<SymbolId, DynamicBinding<'context>>,
    binding_order: Vec<SymbolId>,
    temporaries: HashMap<ExpressionId, PointerValue<'context>>,
    packed_arguments: HashMap<ExpressionId, PointerValue<'context>>,
    coercions: HashMap<ExpressionId, PointerValue<'context>>,
    loop_results: HashMap<ExpressionId, PointerValue<'context>>,
    cells: HashMap<SymbolId, PointerValue<'context>>,
    initialization_states: HashMap<SymbolId, PointerValue<'context>>,
    resources: HashMap<crate::lower::LoweredResourceProviderId, DynamicResource<'context>>,
    loops: Vec<DynamicLoopContext<'context>>,
    closure: PointerValue<'context>,
    result: PointerValue<'context>,
    result_descriptor: PointerValue<'context>,
    returned: bool,
}

impl<'context> DynamicEnvironment<'context> {
    fn snapshot(&self) -> DynamicState<'context> {
        DynamicState {
            bindings: self.bindings.clone(),
            binding_order: self.binding_order.clone(),
            returned: self.returned,
        }
    }

    fn restore(&mut self, state: &DynamicState<'context>) {
        self.bindings = state.bindings.clone();
        self.binding_order = state.binding_order.clone();
        self.returned = state.returned;
    }
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    /// Dynamic ABI pointers can refer to fields in alignment-one products.
    fn load_dynamic_storage<T: inkwell::types::BasicType<'context>>(
        &self,
        ty: T,
        pointer: PointerValue<'context>,
        name: &str,
    ) -> Result<inkwell::values::BasicValueEnum<'context>, inkwell::builder::BuilderError> {
        let value = self.backend.builder.build_load(ty, pointer, name)?;
        value
            .as_instruction_value()
            .expect("load is an instruction")
            .set_alignment(1)
            .expect("one is a valid load alignment");
        Ok(value)
    }

    pub(super) fn emit_compound_descriptor_drop(
        &self,
        ordinal: crate::specialization::ArtifactOrdinal,
        plan: crate::specialization::CompoundDescriptorDropKey,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        use crate::specialization::CompoundDescriptorShape;
        use inkwell::values::BasicValue;
        let function = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing compound cleanup declaration"))?;
        let body = self
            .backend
            .enter_finalizer_function(function, span.clone())?;
        let descriptor = body.payload;
        let value = function
            .get_nth_param(1)
            .expect("cleanup value argument")
            .into_pointer_value();
        let components = (0..plan.count)
            .map(|index| self.backend.load_descriptor_component(descriptor, index))
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let layouts = components
            .iter()
            .map(|component| self.backend.load_descriptor_layout(*component))
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        match plan.shape {
            CompoundDescriptorShape::Product => {
                let mut offset = self.backend.size_type.const_zero();
                let mut fields = Vec::with_capacity(components.len());
                for (component, layout) in components.into_iter().zip(layouts) {
                    let field = self.backend.dynamic_byte_offset(value, offset)?;
                    fields.push((component, field));
                    offset = self
                        .backend
                        .builder
                        .build_int_add(offset, layout.store_size, "product.drop.offset")
                        .map_err(compiler_diagnostic)?;
                }
                for (component, field) in fields.into_iter().rev() {
                    self.backend.drop_dynamic_value(component, field)?;
                }
            }
            CompoundDescriptorShape::Sum => {
                let (_, offset) = self.backend.runtime_sum_layout(&layouts)?;
                let payload = self.backend.dynamic_byte_offset(value, offset)?;
                let tag = self
                    .load_dynamic_storage(self.backend.context.i32_type(), value, "sum.drop.tag")
                    .map_err(compiler_diagnostic)?;
                tag.as_instruction_value()
                    .expect("tag load")
                    .set_alignment(1)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                let done = self
                    .backend
                    .context
                    .append_basic_block(function, "sum.drop.done");
                let cases = (0..plan.count)
                    .map(|index| {
                        (
                            self.backend
                                .context
                                .i32_type()
                                .const_int(index as u64, false),
                            self.backend
                                .context
                                .append_basic_block(function, "sum.drop.case"),
                        )
                    })
                    .collect::<Vec<_>>();
                self.backend
                    .builder
                    .build_switch(tag.into_int_value(), done, &cases)
                    .map_err(compiler_diagnostic)?;
                for ((_, block), component) in cases.into_iter().zip(components) {
                    self.backend.builder.position_at_end(block);
                    self.backend.drop_dynamic_value(component, payload)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(done)
                        .map_err(compiler_diagnostic)?;
                }
                self.backend.builder.position_at_end(done);
            }
        }
        self.backend.finish_finalizer_function(&body)
    }

    /// Reuse concrete argument evaluation and ownership bookkeeping, then
    /// translate the concrete ABI slots into descriptor ABI storage pointers.
    pub(super) fn emit_concrete_dynamic_call(
        &self,
        owner: EmissionOwner,
        call_id: crate::LoweredCallId,
        instance: FunctionInstanceId,
        call: &crate::LoweredCall,
        closure: PointerValue<'context>,
        values: &[inkwell::values::BasicMetadataValueEnum<'context>],
    ) -> CodeGenerationResult<inkwell::values::AnyValueEnum<'context>> {
        use inkwell::values::{AnyValue, BasicValueEnum};
        let span = &call.origin.span;
        let runtime = self
            .view
            .instance_runtime_order(instance)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic callee key"))?;
        let signature = self
            .view
            .instance_signature(instance)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic callee signature"))?;
        let mut argument_conversions = std::collections::BTreeMap::new();
        let mut result_conversion = None;
        let mut writebacks = Vec::new();
        if let Some(conversions) = self.view.conversions(owner) {
            for ((site, argument), conversion) in conversions {
                if *site != call_id {
                    continue;
                }
                match argument {
                    Some(slot) => {
                        argument_conversions.insert(*slot, conversion);
                    }
                    None => result_conversion = Some(conversion),
                }
            }
        }
        let mut arguments = vec![closure.into()];
        for parameter in &runtime {
            let ty = call
                .substitutions
                .types
                .iter()
                .find(|entry| entry.parameter == *parameter)
                .map(|entry| &entry.value_type)
                .ok_or_else(|| {
                    Diagnostic::new(span.clone(), "missing concrete runtime substitution")
                })?;
            arguments.push(self.static_dynamic_descriptor(ty, span)?.into());
        }
        let result_type = self.backend.compile_type(&call.result_type)?;
        let result = self
            .backend
            .entry_builder()
            .build_alloca(result_type, "dynamic.call.result")
            .map_err(compiler_diagnostic)?;
        arguments.push(result.into());
        let resource_count = call.resource_bindings.len();
        let (resources, values) = values.split_at(resource_count);
        for (index, value) in resources.iter().enumerate() {
            let use_id = *call.resource_bindings.get(index).ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing concrete resource binding")
            })?;
            let use_record = self
                .view
                .resource_use(owner, use_id)
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing concrete resource use"))?;
            let value = BasicValueEnum::try_from(*value)
                .map_err(|_| Diagnostic::new(span.clone(), "invalid concrete resource"))?;
            let pointer = if use_record.pass_mode != crate::lower::LoweredArgumentPassMode::Value {
                value.into_pointer_value()
            } else {
                let storage = self
                    .backend
                    .entry_builder()
                    .build_alloca(value.get_type(), "dynamic.resource")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(storage, value)
                    .map_err(compiler_diagnostic)?;
                storage
            };
            arguments.push(pointer.into());
        }
        let parameter_types = super::flattened_parameter_types(&call.function_type.parameter);
        let indirect = self.backend.indirect_parameter_mask(&call.function_type);
        let whole = call
            .function_type
            .mutations
            .contains(&crate::CheckedMutation::Whole);
        let pack = !whole
            && !matches!(signature.parameter.as_ref(), CheckedType::Product(_))
            && matches!(
                call.function_type.parameter.as_ref(),
                CheckedType::Product(_)
            );
        if pack {
            if !argument_conversions.is_empty() {
                return Err(Diagnostic::new(
                    span.clone(),
                    "reabstraction inside a packed argument is not yet implemented",
                ));
            }
            let fields = values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let value = BasicValueEnum::try_from(*value)
                        .map_err(|_| Diagnostic::new(span.clone(), "invalid concrete argument"))?;
                    if indirect[index] {
                        self.load_dynamic_storage(
                            self.backend.compile_type(parameter_types[index])?,
                            value.into_pointer_value(),
                            "dynamic.argument.value",
                        )
                        .map_err(compiler_diagnostic)
                    } else {
                        Ok(value)
                    }
                })
                .collect::<CodeGenerationResult<Vec<_>>>()?;
            let product = self.backend.build_product_value(&fields, span.clone())?;
            let storage = self
                .backend
                .entry_builder()
                .build_alloca(product.get_type(), "dynamic.packed.argument")
                .map_err(compiler_diagnostic)?;
            self.backend
                .builder
                .build_store(storage, product)
                .map_err(compiler_diagnostic)?;
            arguments.push(storage.into());
        } else {
            for (index, value) in values.iter().enumerate() {
                let value = BasicValueEnum::try_from(*value)
                    .map_err(|_| Diagnostic::new(span.clone(), "invalid concrete argument"))?;
                if let Some(conversion) = argument_conversions.get(&index) {
                    if conversion.direction
                        != crate::LoweredReabstractionDirection::ConcreteToDynamic
                    {
                        return Err(Diagnostic::new(
                            span.clone(),
                            "dynamic-to-concrete argument conversion at a concrete caller",
                        ));
                    }
                    let source = if whole || indirect[index] {
                        value.into_pointer_value()
                    } else {
                        let source = self
                            .backend
                            .entry_builder()
                            .build_alloca(value.get_type(), "conversion.source")
                            .map_err(compiler_diagnostic)?;
                        self.backend
                            .builder
                            .build_store(source, value)
                            .map_err(compiler_diagnostic)?;
                        source
                    };
                    let destination = self
                        .backend
                        .entry_builder()
                        .build_alloca(
                            self.backend.compile_type(&conversion.concrete)?,
                            "conversion.argument",
                        )
                        .map_err(compiler_diagnostic)?;
                    let function =
                        self.reabstraction_thunk_code(owner, call_id, Some(index), span)?;
                    self.backend
                        .builder
                        .build_direct_call(function, &[destination.into(), source.into()], "")
                        .map_err(compiler_diagnostic)?;
                    if conversion.writeback {
                        let function =
                            self.reabstraction_writeback_code(owner, call_id, index, span)?;
                        writebacks.push((function, source, destination));
                    }
                    arguments.push(destination.into());
                    continue;
                }
                let pointer = if whole || indirect[index] {
                    value.into_pointer_value()
                } else {
                    let storage = self
                        .backend
                        .entry_builder()
                        .build_alloca(value.get_type(), "dynamic.argument")
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_store(storage, value)
                        .map_err(compiler_diagnostic)?;
                    storage
                };
                arguments.push(pointer.into());
            }
        }
        let function =
            self.instances.get(&instance).copied().ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing dynamic callee declaration")
            })?;
        self.backend
            .builder
            .build_direct_call(function, &arguments, "")
            .map_err(compiler_diagnostic)?;
        for (function, destination, source) in writebacks {
            self.backend
                .builder
                .build_direct_call(function, &[destination.into(), source.into()], "")
                .map_err(compiler_diagnostic)?;
        }
        let value = self
            .load_dynamic_storage(result_type, result, "dynamic.call.value")
            .map_err(compiler_diagnostic)?;
        if let Some(conversion) = result_conversion {
            if conversion.direction != crate::LoweredReabstractionDirection::DynamicToConcrete {
                return Err(Diagnostic::new(
                    span.clone(),
                    "concrete-to-dynamic result conversion at a concrete caller",
                ));
            }
            return Ok(self
                .convert_boundary_value(value, owner, call_id, None, span)?
                .as_any_value_enum());
        }
        Ok(value.as_any_value_enum())
    }

    /// The declared reabstraction thunk for one boundary conversion.
    pub(super) fn reabstraction_thunk_code(
        &self,
        owner: EmissionOwner,
        call: crate::LoweredCallId,
        argument: Option<usize>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let uses = self
            .view
            .artifact_uses(owner)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reabstraction use records"))?;
        let ordinal = uses
            .iter()
            .find_map(|use_| {
                (use_.site == crate::ArtifactUseSite::ReabstractionThunk { call, argument })
                    .then_some(use_.artifact)
            })
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reabstraction plan"))?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reabstraction declaration"))
    }

    /// The declared function of one binding site in a dynamic body. Formatting
    /// helpers and interpolation dispatches are concrete functions, so the
    /// dynamic body calls them with the concrete ABI.
    fn dynamic_bound_function(
        &self,
        owner: EmissionOwner,
        site: LoweredBindingSite,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let binding = self
            .view
            .binding(owner, site)
            .ok_or_else(|| Diagnostic::new(span.clone(), "lowered site has no binding"))?;
        match binding {
            LoweredBoundTarget::Instance(instance) => self
                .instances
                .get(instance)
                .copied()
                .ok_or_else(|| Diagnostic::new(span.clone(), "bound instance is not declared")),
            LoweredBoundTarget::Artifact(ordinal) => self
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

    /// One hidden resource argument at a dynamic call. A dynamic callee
    /// receives the provider pointer; a concrete callee receives the value or
    /// pointer its concrete ABI expects.
    fn dynamic_resource_argument(
        &self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        index: usize,
        dynamic_callee: bool,
        environment: &DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<inkwell::values::BasicMetadataValueEnum<'context>> {
        let use_id = *call.resource_bindings.get(index).ok_or_else(|| {
            Diagnostic::new(call.origin.span.clone(), "missing hidden resource binding")
        })?;
        let use_record = self.view.resource_use(owner, use_id).ok_or_else(|| {
            Diagnostic::new(call.origin.span.clone(), "missing hidden resource use")
        })?;
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
        if dynamic_callee || use_record.pass_mode != crate::lower::LoweredArgumentPassMode::Value {
            return Ok(bound.pointer.into());
        }
        let ty = self.backend.compile_type(&bound.value_type)?;
        Ok(self
            .load_dynamic_storage(ty, bound.pointer, "resource.copy")
            .map_err(compiler_diagnostic)?
            .into())
    }

    /// A resource value read in a dynamic body (R7). The provider pointer is
    /// registered by the function's effect row or a `with` scope; the value is
    /// loaded into the expression's dynamic storage.
    fn emit_dynamic_resource_read(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        use_id: crate::LoweredResourceUseId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let span = self
            .view
            .expression(owner, id)
            .map(|expression| expression.origin.span.clone())
            .unwrap_or(staple_syntax::Span::Compiler);
        let storage = *environment
            .temporaries
            .get(&id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic resource storage"))?;
        let use_record = self
            .view
            .resource_use(owner, use_id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing resource use"))?;
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
        let ty = self.backend.compile_type(&bound.value_type)?;
        let value = self
            .load_dynamic_storage(ty, bound.pointer, "resource.value")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(storage, value)
            .map_err(compiler_diagnostic)?;
        Ok(storage)
    }

    /// A `with` scope in a dynamic body (R7). The provider value is evaluated
    /// once, registered for the body, and removed after it. Reactive and task
    /// scopes stay Stage 8.
    fn emit_dynamic_with(
        &mut self,
        owner: EmissionOwner,
        with_id: crate::LoweredWithId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let with = self.view.with(owner, with_id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing lowered with")
        })?;
        if with.scope_exit != crate::lower::LoweredScopeExit::Ordinary {
            return Err(Diagnostic::new(
                with.origin.span.clone(),
                "dynamic reactive or task with scopes are not yet implemented",
            ));
        }
        let provider = self
            .view
            .resource_provider(owner, with.provider)
            .ok_or_else(|| Diagnostic::new(with.origin.span.clone(), "missing `with` provider"))?;
        let value = self.emit_dynamic_expression(owner, with.value, environment)?;
        let pointer = match provider.storage {
            crate::lower::LoweredProviderStorage::Place => {
                let place = with.place.ok_or_else(|| {
                    Diagnostic::new(
                        with.origin.span.clone(),
                        "`with` place provider has no place",
                    )
                })?;
                self.emit_dynamic_place(owner, place, environment)?.0
            }
            crate::lower::LoweredProviderStorage::Materialized => value,
        };
        environment.resources.insert(
            with.provider,
            DynamicResource {
                value_type: provider.resource.value_type.clone(),
                pointer,
            },
        );
        let result = self.emit_dynamic_block(owner, with.body, environment);
        environment.resources.remove(&with.provider);
        let result = result?;
        Ok(result.unwrap_or_else(|| {
            self.backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null()
        }))
    }

    /// A string template whose interpolations have concrete types (R7). The
    /// formatter constructor, literal writes, interpolation dispatches, and
    /// finish are concrete functions called with the concrete ABI; each
    /// interpolated value is loaded from dynamic storage as its concrete type
    /// and the resulting `String` is stored into dynamic storage.
    fn emit_dynamic_string_template(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        template: &crate::LoweredStringTemplate,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let span = self
            .view
            .expression(owner, id)
            .map(|expression| expression.origin.span.clone())
            .unwrap_or(staple_syntax::Span::Compiler);
        let storage = *environment
            .temporaries
            .get(&id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic template storage"))?;
        let null = self
            .backend
            .context
            .ptr_type(AddressSpace::default())
            .const_null();
        let constructor = self.dynamic_bound_function(
            owner,
            LoweredBindingSite::FormattingConstructor(id),
            &span,
        )?;
        let formatter = self
            .backend
            .builder
            .build_direct_call(constructor, &[null.into()], "template.formatter")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic();
        let formatter_storage = self
            .backend
            .entry_builder()
            .build_alloca(formatter.get_type(), "template.formatter.storage")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(formatter_storage, formatter)
            .map_err(compiler_diagnostic)?;
        let mut write = None;
        for (part_index, part) in template.parts.iter().enumerate() {
            match part {
                crate::LoweredStringTemplatePart::Literal(literal) => {
                    let function = match write {
                        Some(function) => function,
                        None => {
                            let function = self.dynamic_bound_function(
                                owner,
                                LoweredBindingSite::FormattingWrite(id),
                                &span,
                            )?;
                            write = Some(function);
                            function
                        }
                    };
                    self.backend.build_formatter_write_literal(
                        function,
                        formatter_storage.into(),
                        literal,
                        span.clone(),
                    )?;
                }
                crate::LoweredStringTemplatePart::Interpolation(interpolation) => {
                    let value =
                        self.emit_dynamic_expression(owner, interpolation.expression, environment)?;
                    if environment.returned {
                        return Ok(storage);
                    }
                    let ty = self.backend.compile_type(&interpolation.value_type)?;
                    let value = self
                        .load_dynamic_storage(ty, value, "template.value")
                        .map_err(compiler_diagnostic)?;
                    let function = self.dynamic_bound_function(
                        owner,
                        LoweredBindingSite::Interpolation {
                            template: id,
                            part: part_index,
                        },
                        &span,
                    )?;
                    self.backend
                        .builder
                        .build_direct_call(
                            function,
                            &[null.into(), value.into(), formatter_storage.into()],
                            "template.fmt",
                        )
                        .map_err(compiler_diagnostic)?;
                }
            }
        }
        let finish =
            self.dynamic_bound_function(owner, LoweredBindingSite::FormattingFinish(id), &span)?;
        let formatter = self
            .load_dynamic_storage(
                formatter.get_type(),
                formatter_storage,
                "template.finished.formatter",
            )
            .map_err(compiler_diagnostic)?;
        let result = self
            .backend
            .builder
            .build_direct_call(finish, &[null.into(), formatter.into()], "template.finish")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic();
        self.backend
            .builder
            .build_store(storage, result)
            .map_err(compiler_diagnostic)?;
        Ok(storage)
    }

    /// Materialize a written argument or its lowered implicit closure.
    fn emit_dynamic_argument(
        &mut self,
        owner: EmissionOwner,
        expression: ExpressionId,
        call: &crate::LoweredCall,
        index: usize,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let record = &call.arguments[index];
        if let Some(value) = record.expression {
            return self.emit_dynamic_expression(owner, value, environment);
        }
        let span = &call.origin.span;
        let Some(LoweredExpressionKind::Call(call_id)) = self
            .view
            .expression(owner, expression)
            .map(|record| &record.kind)
        else {
            return Err(Diagnostic::new(
                span.clone(),
                "argument owner is not a call",
            ));
        };
        let call_id = *call_id;
        let Some(LoweredBoundTarget::Instance(instance)) = self.view.binding(
            owner,
            LoweredBindingSite::CallArgumentThunk {
                call: call_id,
                argument: index,
            },
        ) else {
            return Err(Diagnostic::new(
                span.clone(),
                "missing implicit thunk instance",
            ));
        };
        let instance = *instance;
        let body = self
            .view
            .instance(instance)
            .and_then(|record| record.body.as_ref())
            .cloned()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing implicit thunk body"))?;
        let runtime = self
            .view
            .instance_runtime_order(instance)
            .unwrap_or_default();
        let (code, captures) = if runtime.is_empty() {
            let types = body
                .captures
                .iter()
                .map(|capture| self.capture_field_type(capture))
                .collect::<CodeGenerationResult<Vec<_>>>()?;
            let ty = self.backend.capture_environment_type(&types);
            let mut value = ty.const_zero();
            for (index, capture) in body.captures.iter().enumerate() {
                let binding = environment
                    .bindings
                    .get(&capture.capture.symbol)
                    .filter(|binding| binding.live)
                    .ok_or_else(|| Diagnostic::new(span.clone(), "missing thunk capture"))?;
                let stored = if capture.requires_initialization_state
                    || capture.mutable_storage
                    || capture.derived
                    || capture.capture.borrowed
                {
                    binding.pointer.into()
                } else {
                    self.load_dynamic_storage(
                        self.backend.compile_type(&capture.value_type)?,
                        binding.pointer,
                        "thunk.capture",
                    )
                    .map_err(compiler_diagnostic)?
                };
                value = self
                    .backend
                    .insert_capture(value, stored, index as u32, span.clone())?;
            }
            let captures = if body.captures.is_empty() {
                self.backend
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null()
            } else {
                self.backend
                    .allocate_capture_environment(ty, value, span.clone())?
            };
            if let Some(finalizer) = self.site_finalizer(
                owner,
                crate::ArtifactUseSite::ThunkArgumentEnvironment {
                    call: call_id,
                    argument: index,
                },
            ) {
                self.backend.set_gc_finalizer(captures, finalizer)?;
            }
            (
                *self
                    .instances
                    .get(&instance)
                    .ok_or_else(|| Diagnostic::new(span.clone(), "missing thunk declaration"))?,
                captures,
            )
        } else {
            let captures = body
                .captures
                .iter()
                .map(|capture| crate::LoweredClosureCapture {
                    capture: capture.capture.clone(),
                    value_type: capture.value_type.clone(),
                    access: if capture.requires_initialization_state
                        || capture.mutable_storage
                        || capture.derived
                    {
                        crate::LoweredCaptureAccess::SharedCell
                    } else if capture.capture.borrowed {
                        crate::LoweredCaptureAccess::Borrowed
                    } else {
                        crate::LoweredCaptureAccess::ByValue
                    },
                    owns_value: !capture.capture.borrowed && !capture.capture.non_owning,
                    requires_initialization_state: capture.requires_initialization_state,
                })
                .collect::<Vec<_>>();
            let substitutions = crate::CallSubstitutions {
                types: runtime
                    .iter()
                    .map(|parameter| crate::CallTypeSubstitution {
                        parameter: *parameter,
                        value_type: CheckedType::Parameter {
                            id: *parameter,
                            name: String::new(),
                            sized: true,
                            parameter_product_capable: false,
                        },
                    })
                    .collect(),
                ..Default::default()
            };
            let captures = self.build_descriptor_environment(
                instance,
                &runtime,
                &substitutions,
                &captures,
                environment,
                span,
            )?;
            (self.descriptor_bound_entry_code(instance, span)?, captures)
        };
        let closure = self.backend.build_closure_value(code, captures)?;
        let storage = self
            .backend
            .entry_builder()
            .build_alloca(closure.get_type(), "thunk.argument")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_store(storage, closure)
            .map_err(compiler_diagnostic)?;
        Ok(storage)
    }

    /// A structural constructor builds its product or wrapper representation
    /// from one flattened slot per written field. The result storage was
    /// reserved from the call's result descriptor at entry.
    fn emit_dynamic_constructor_call(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        call: &crate::LoweredCall,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let span = &call.origin.span;
        let storage = *environment
            .temporaries
            .get(&id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic constructor storage"))?;
        let mut slots =
            vec![None; super::flattened_parameter_types(&call.function_type.parameter).len()];
        let mut unplaced = Vec::new();
        for step in &call.steps {
            match step {
                LoweredCallStep::Argument { argument } => {
                    let record = &call.arguments[*argument];
                    let value = record.expression.ok_or_else(|| {
                        Diagnostic::new(
                            span.clone(),
                            "dynamic thunk argument is not yet implemented",
                        )
                    })?;
                    let value = self.emit_dynamic_expression(owner, value, environment)?;
                    match record.slot {
                        Some(slot) => slots[slot] = Some(value),
                        None => unplaced.push(value),
                    }
                }
                LoweredCallStep::ProductElement {
                    expression, slot, ..
                }
                | LoweredCallStep::Default {
                    expression, slot, ..
                } => {
                    let value = self.emit_dynamic_expression(owner, *expression, environment)?;
                    slots[*slot] = Some(value);
                }
                LoweredCallStep::Invoke => break,
                _ => {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "dynamic constructor argument step is not yet implemented",
                    ));
                }
            }
        }
        if slots.iter().all(Option::is_none) && unplaced.len() == 1 {
            let descriptor =
                self.dynamic_descriptor(&call.function_type.parameter, environment, span)?;
            for (index, slot) in slots.iter_mut().enumerate() {
                *slot = Some(
                    self.dynamic_product_field(descriptor, unplaced[0], index as u32)?
                        .0,
                );
            }
        }
        let representation = match &call.result_type {
            CheckedType::Wrapper { representation, .. } => representation.as_ref(),
            other => other,
        };
        let descriptor = self.dynamic_descriptor(&call.result_type, environment, span)?;
        match representation {
            CheckedType::Product(product) => {
                for (index, _) in product.elements.iter().enumerate() {
                    let value = slots.get(index).and_then(|slot| *slot).ok_or_else(|| {
                        Diagnostic::new(span.clone(), "missing dynamic constructor field")
                    })?;
                    let (field, component) =
                        self.dynamic_product_field(descriptor, storage, index as u32)?;
                    self.backend.copy_dynamic_value(component, value, field)?;
                }
            }
            _ => {
                let value = slots.first().and_then(|slot| *slot).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "missing dynamic constructor value")
                })?;
                self.backend
                    .copy_dynamic_value(descriptor, value, storage)?;
            }
        }
        Ok(storage)
    }

    pub(super) fn emit_dynamic_constructor_adapter(
        &mut self,
        function: FunctionValue<'context>,
        plan: &crate::ConstructorAdapterPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let parameters = function.get_params();
        let closure = parameters[0].into_pointer_value();
        let result = parameters[1].into_pointer_value();
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let mut descriptors = HashMap::new();
        for (index, parameter) in plan.runtime_parameters().into_iter().enumerate() {
            let slot = self.backend.dynamic_byte_offset(
                closure,
                self.backend
                    .size_type
                    .const_int(index as u64 * pointer_bytes, false),
            )?;
            let descriptor = self
                .load_dynamic_storage(pointer, slot, "constructor.descriptor")
                .map_err(compiler_diagnostic)?
                .into_pointer_value();
            descriptors.insert(parameter, descriptor);
        }
        let mut environment = DynamicEnvironment {
            descriptors,
            compound_descriptors: Vec::new(),
            bindings: HashMap::new(),
            binding_order: Vec::new(),
            temporaries: HashMap::new(),
            packed_arguments: HashMap::new(),
            coercions: HashMap::new(),
            loop_results: HashMap::new(),
            cells: HashMap::new(),
            initialization_states: HashMap::new(),
            resources: HashMap::new(),
            loops: Vec::new(),
            closure,
            result,
            result_descriptor: pointer.const_null(),
            returned: false,
        };
        let (product, managed) = match &plan.construction {
            crate::ConstructorConstruction::Value { product, .. } => (product, false),
            crate::ConstructorConstruction::ManagedRef { product, .. } => (product, true),
            _ => {
                return Err(Diagnostic::new(
                    span.clone(),
                    "unexpanded dynamic constructor",
                ));
            }
        };
        let descriptor = self.prepare_dynamic_descriptor(product, &mut environment, span)?;
        let result = if managed {
            let descriptor = self.copy_descriptor_to_gc(descriptor, span)?;
            let allocation = self.allocate_dynamic_cell(descriptor, true, span)?;
            self.backend
                .builder
                .build_store(result, allocation)
                .map_err(compiler_diagnostic)?;
            allocation
        } else {
            result
        };
        if let CheckedType::Product(product) = product {
            for index in 0..product.elements.len() {
                let (field, descriptor) =
                    self.dynamic_product_field(descriptor, result, index as u32)?;
                self.backend.copy_dynamic_value(
                    descriptor,
                    parameters[2 + index].into_pointer_value(),
                    field,
                )?;
            }
        } else if let Some(value) = parameters.get(2) {
            self.backend
                .copy_dynamic_value(descriptor, value.into_pointer_value(), result)?;
        }
        self.backend
            .builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    /// Build a closure value in a dynamic body (R-B). A concrete function
    /// type uses the concrete ABI; a value that reads a function-typed binding
    /// returns the binding's storage. Captures and dynamic-shape entries are
    /// later steps (R3, R4).
    fn emit_dynamic_index(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        expression: &crate::LoweredExpression,
        index: &crate::LoweredIndex,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let span = &expression.origin.span;
        let signature = index
            .method_type
            .as_ref()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing concrete Index signature"))?;
        let types = super::flattened_parameter_types(&signature.parameter);
        let mutations =
            super::super::abi::mutation_parameter_mask(types.len(), &signature.mutations);
        let mut values: Vec<inkwell::values::BasicMetadataValueEnum<'context>> = Vec::new();
        let mut writebacks = Vec::new();
        let mut drops = Vec::new();
        for (slot, operand) in [index.base, index.index].into_iter().enumerate() {
            let place = if slot == 0 {
                index.base_place
            } else {
                index.index_place
            };
            let source = if let Some(place) = place {
                self.emit_dynamic_place(owner, place, environment)?.0
            } else {
                self.emit_dynamic_expression(owner, operand, environment)?
            };
            if environment.returned {
                return Ok(source);
            }
            let ty = &types[slot];
            if index.operands.indirect[slot] && !index.whole_temporary {
                let native = self
                    .backend
                    .entry_builder()
                    .build_alloca(self.backend.compile_type(ty)?, "index.argument")
                    .map_err(compiler_diagnostic)?;
                let descriptor = self.dynamic_descriptor(ty, environment, span)?;
                self.backend
                    .copy_dynamic_value(descriptor, source, native)?;
                values.push(native.into());
                if mutations[slot] && place.is_some() {
                    writebacks.push((descriptor, native, source));
                }
                if index
                    .operands
                    .drops_after_call
                    .get(slot)
                    .copied()
                    .unwrap_or(false)
                {
                    drops.push((descriptor, native));
                }
            } else {
                let value = self
                    .load_dynamic_storage(self.backend.compile_type(ty)?, source, "index.argument")
                    .map_err(compiler_diagnostic)?;
                value
                    .as_instruction_value()
                    .unwrap()
                    .set_alignment(1)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                values.push(value.into());
            }
        }
        let mut arguments = vec![
            self.backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null()
                .into(),
        ];
        if index.whole_temporary {
            let elements = values
                .iter()
                .map(|value| {
                    inkwell::values::BasicValueEnum::try_from(*value)
                        .map_err(|_| Diagnostic::new(span.clone(), "invalid whole Index argument"))
                })
                .collect::<CodeGenerationResult<Vec<_>>>()?;
            let value = self.backend.build_product_value(&elements, span.clone())?;
            let storage = self
                .backend
                .entry_builder()
                .build_alloca(value.get_type(), "index.whole")
                .map_err(compiler_diagnostic)?;
            self.backend
                .builder
                .build_store(storage, value)
                .map_err(compiler_diagnostic)?;
            arguments.push(storage.into());
            if index.operands.whole_drops_after_call {
                drops.push((
                    self.dynamic_descriptor(&signature.parameter, environment, span)?,
                    storage,
                ));
            }
        } else {
            arguments.extend(values);
        }
        let function = self.dynamic_bound_function(owner, LoweredBindingSite::Index(id), span)?;
        let value = self
            .backend
            .builder
            .build_direct_call(function, &arguments, "index.call")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| Diagnostic::new(span.clone(), "Index has no result"))?;
        let result = *environment
            .temporaries
            .get(&id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing Index result storage"))?;
        self.backend
            .builder
            .build_store(result, value)
            .map_err(compiler_diagnostic)?
            .set_alignment(1)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        for (descriptor, source, destination) in writebacks {
            self.backend
                .copy_dynamic_value(descriptor, source, destination)?;
        }
        for (descriptor, value) in drops.into_iter().rev() {
            self.backend.drop_dynamic_value(descriptor, value)?;
        }
        Ok(result)
    }

    fn emit_dynamic_callable_value(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        value_id: crate::LoweredCallableValueId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let callable = self
            .view
            .callable_value(owner, value_id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing callable value")
            })?;
        let span = callable.origin.span.clone();
        if callable.requires_initialization_check {
            if let Some(symbol) = self.callable_symbol(&callable) {
                if let Some(state) = environment.initialization_states.get(&symbol) {
                    self.backend
                        .build_initialization_check(*state, span.clone())?;
                } else if let Some(state) = self.initialization_states.get(&symbol) {
                    self.backend
                        .build_initialization_check(state.as_pointer_value(), span.clone())?;
                }
            }
        }
        let storage = *environment
            .temporaries
            .get(&id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic callable storage"))?;
        if let Some(symbol) = self.callable_symbol(&callable)
            && let Some(binding) = environment
                .bindings
                .get(&symbol)
                .filter(|binding| binding.live)
        {
            return Ok(binding.pointer);
        }
        if let LoweredCallableTarget::IndirectClosure { callee } = &callable.target {
            return self.emit_dynamic_expression(owner, *callee, environment);
        }
        let binding = self
            .view
            .binding(owner, LoweredBindingSite::CallableValue(value_id));
        if matches!(callable.target, LoweredCallableTarget::Constructor { .. })
            && crate::contains_type_parameter(&CheckedType::Function(
                callable.function_type.clone(),
            ))
        {
            let Some(LoweredBoundTarget::Artifact(ordinal)) = binding else {
                return Err(Diagnostic::new(
                    span.clone(),
                    "missing dynamic constructor artifact",
                ));
            };
            let Some(crate::LoweredArtifactPlan::ConstructorAdapter(plan)) = self
                .view
                .artifact(*ordinal)
                .and_then(|artifact| artifact.plan.as_ref())
            else {
                return Err(Diagnostic::new(
                    span.clone(),
                    "missing dynamic constructor plan",
                ));
            };
            let runtime = plan.runtime_parameters();
            let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
            let closure_environment = self.backend.build_gc_allocation(
                self.backend
                    .size_type
                    .const_int(runtime.len() as u64 * pointer_bytes, false),
                "constructor.environment",
                span.clone(),
            )?;
            for (index, parameter) in runtime.iter().enumerate() {
                let descriptor =
                    environment
                        .descriptors
                        .get(parameter)
                        .copied()
                        .ok_or_else(|| {
                            Diagnostic::new(span.clone(), "missing constructor runtime descriptor")
                        })?;
                let copy = self.copy_descriptor_to_gc(descriptor, &span)?;
                let slot = self.backend.dynamic_byte_offset(
                    closure_environment,
                    self.backend
                        .size_type
                        .const_int(index as u64 * pointer_bytes, false),
                )?;
                self.backend
                    .builder
                    .build_store(slot, copy)
                    .map_err(compiler_diagnostic)?;
            }
            let code =
                self.callable_artifact_code(binding, &span, "dynamic constructor binding")?;
            let closure = self
                .backend
                .build_closure_value(code, closure_environment)?;
            self.backend
                .builder
                .build_store(storage, closure)
                .map_err(compiler_diagnostic)?;
            return Ok(storage);
        }
        let code = match &callable.target {
            LoweredCallableTarget::DirectFunction { .. }
            | LoweredCallableTarget::TraitImplementation { .. } => {
                let Some(LoweredBoundTarget::Instance(instance)) = binding else {
                    return Err(Diagnostic::new(
                        span,
                        "missing dynamic callable instance binding",
                    ));
                };
                let runtime = self.view.instance_runtime_order(*instance).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "missing callable instance key")
                })?;
                if !runtime.is_empty() {
                    // A dynamic-shape closure (R-C): the environment stores the
                    // instance's descriptors and the entry forwards the call.
                    let captures = callable
                        .closure
                        .as_ref()
                        .map_or(&[][..], |closure| closure.captures.as_slice());
                    let closure_environment = self.build_descriptor_environment(
                        *instance,
                        &runtime,
                        &callable.substitutions,
                        captures,
                        environment,
                        &span,
                    )?;
                    let code = self.descriptor_bound_entry_code(*instance, &span)?;
                    let closure = self
                        .backend
                        .build_closure_value(code, closure_environment)?;
                    let closure = if !crate::contains_type_parameter(&CheckedType::Function(
                        callable.function_type.clone(),
                    )) {
                        let ordinal = self
                            .view
                            .artifact_uses(owner)
                            .and_then(|uses| {
                                uses.iter().find(|use_| {
                                    use_.site
                                        == crate::ArtifactUseSite::ConcreteCapturedClosure(value_id)
                                })
                            })
                            .map(|use_| use_.artifact)
                            .ok_or_else(|| {
                                Diagnostic::new(span.clone(), "missing concrete capture adapter")
                            })?;
                        let code = self
                            .artifacts
                            .get(&ordinal)
                            .and_then(|functions| functions.first())
                            .copied()
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    span.clone(),
                                    "missing concrete capture adapter code",
                                )
                            })?;
                        let saved = self.backend.build_gc_allocation(
                            self.backend.size_type.const_int(
                                2 * self.backend.target_data.get_pointer_byte_size(None) as u64,
                                false,
                            ),
                            "captured.closure.adapter",
                            span.clone(),
                        )?;
                        self.backend
                            .builder
                            .build_store(saved, closure)
                            .map_err(compiler_diagnostic)?;
                        self.backend.build_closure_value(code, saved)?
                    } else {
                        closure
                    };
                    self.backend
                        .builder
                        .build_store(storage, closure)
                        .map_err(compiler_diagnostic)?;
                    return Ok(storage);
                }
                self.instances
                    .get(instance)
                    .copied()
                    .ok_or_else(|| Diagnostic::new(span.clone(), "missing callable declaration"))?
            }
            LoweredCallableTarget::ExternalFunction { .. } => self
                .extern_adapter_code(owner, value_id)
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing extern adapter"))?,
            LoweredCallableTarget::Constructor { .. }
            | LoweredCallableTarget::StructuralTraitMethod { .. } => {
                self.callable_artifact_code(binding, &span, "callable artifact binding")?
            }
            LoweredCallableTarget::IndirectClosure { .. } => unreachable!(),
            LoweredCallableTarget::Intrinsic { .. } => {
                return Err(Diagnostic::new(
                    span,
                    "intrinsic callable values are not supported",
                ));
            }
        };
        let captures = callable
            .closure
            .as_ref()
            .map_or(&[][..], |closure| closure.captures.as_slice());
        let closure_environment = if captures.is_empty() {
            self.backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null()
        } else {
            let fields = captures
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
            let ty = self.backend.capture_environment_type(&fields);
            let mut value = ty.const_zero();
            for (index, capture) in captures.iter().enumerate() {
                let pointer = environment
                    .bindings
                    .get(&capture.capture.symbol)
                    .filter(|binding| binding.live)
                    .map(|binding| binding.pointer)
                    .or_else(|| {
                        environment
                            .initialization_states
                            .get(&capture.capture.symbol)
                            .copied()
                    })
                    .or_else(|| environment.cells.get(&capture.capture.symbol).copied())
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "missing concrete closure capture")
                    })?;
                let stored = if capture.access == crate::LoweredCaptureAccess::ByValue {
                    let stored = self
                        .load_dynamic_storage(
                            self.backend.compile_type(&capture.value_type)?,
                            pointer,
                            "closure.capture",
                        )
                        .map_err(compiler_diagnostic)?;
                    stored
                        .as_instruction_value()
                        .unwrap()
                        .set_alignment(1)
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                    stored
                } else {
                    pointer.into()
                };
                value = self
                    .backend
                    .insert_capture(value, stored, index as u32, span.clone())?;
            }
            let allocated = self
                .backend
                .allocate_capture_environment(ty, value, span.clone())?;
            if let Some(finalizer) =
                self.site_finalizer(owner, crate::ArtifactUseSite::ClosureEnvironment(value_id))
            {
                self.backend.set_gc_finalizer(allocated, finalizer)?;
            }
            allocated
        };
        let closure = self
            .backend
            .build_closure_value(code, closure_environment)?;
        self.backend
            .builder
            .build_store(storage, closure)
            .map_err(compiler_diagnostic)?;
        Ok(storage)
    }

    /// A GC-allocated closure environment holding one deep-copied descriptor
    /// per run-time parameter, in descriptor ABI order (R-D), then the
    /// captures at descriptor-computed offsets.
    fn build_descriptor_environment(
        &self,
        instance: FunctionInstanceId,
        runtime: &[TypeParameterId],
        substitutions: &crate::CallSubstitutions,
        captures: &[crate::LoweredClosureCapture],
        environment: &DynamicEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let mut descriptors = Vec::with_capacity(runtime.len());
        for parameter in runtime {
            // The callee's run-time parameter is supplied at this site by the
            // substitution's value, which may itself mention the enclosing
            // instance's run-time parameters.
            let descriptor = if let Some(entry) = substitutions
                .types
                .iter()
                .find(|entry| entry.parameter == *parameter)
            {
                self.dynamic_descriptor(&entry.value_type, environment, span)?
            } else {
                *environment
                    .descriptors
                    .get(parameter)
                    .ok_or_else(|| Diagnostic::new(span.clone(), "missing closure substitution"))?
            };
            descriptors.push(descriptor);
        }
        let capture_descriptors = captures
            .iter()
            .map(|capture| {
                self.capture_descriptor(
                    &capture.value_type,
                    runtime,
                    &descriptors,
                    environment,
                    span,
                )
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let accesses = captures
            .iter()
            .map(|capture| capture.access)
            .collect::<Vec<_>>();
        let (offsets, total) = self.dynamic_capture_offsets(
            runtime.len() + captures.len(),
            &accesses,
            &capture_descriptors,
        )?;
        let closure_environment =
            self.backend
                .build_gc_allocation(total, "closure.environment", span.clone())?;
        for (index, descriptor) in descriptors.iter().enumerate() {
            let copy = self.copy_descriptor_to_gc(*descriptor, span)?;
            let offset = self
                .backend
                .size_type
                .const_int(index as u64 * pointer_bytes, false);
            let slot = self
                .backend
                .dynamic_byte_offset(closure_environment, offset)?;
            self.backend
                .builder
                .build_store(slot, copy)
                .map_err(compiler_diagnostic)?;
        }
        // Capture descriptors follow the runtime descriptors so the closure
        // body and finalizer read them instead of rebuilding compound ones.
        for (index, descriptor) in capture_descriptors.iter().enumerate() {
            let offset = self
                .backend
                .size_type
                .const_int((runtime.len() + index) as u64 * pointer_bytes, false);
            let slot = self
                .backend
                .dynamic_byte_offset(closure_environment, offset)?;
            let value = match descriptor {
                Some(descriptor) => self.copy_descriptor_to_gc(*descriptor, span)?,
                None => self
                    .backend
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null(),
            };
            self.backend
                .builder
                .build_store(slot, value)
                .map_err(compiler_diagnostic)?;
        }
        for (index, capture) in captures.iter().enumerate() {
            let field = self
                .backend
                .dynamic_byte_offset(closure_environment, offsets[index])?;
            let binding = environment
                .bindings
                .get(&capture.capture.symbol)
                .filter(|binding| binding.live)
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing captured dynamic binding"))?;
            match capture.access {
                crate::LoweredCaptureAccess::ByValue => {
                    let descriptor = capture_descriptors[index].ok_or_else(|| {
                        Diagnostic::new(span.clone(), "missing by-value capture descriptor")
                    })?;
                    self.backend
                        .copy_dynamic_value(descriptor, binding.pointer, field)?;
                }
                crate::LoweredCaptureAccess::Borrowed => {
                    self.backend
                        .builder
                        .build_store(field, binding.pointer)
                        .map_err(compiler_diagnostic)?;
                }
                crate::LoweredCaptureAccess::SharedCell => {
                    // The environment stores the shared cell pointer; the cell
                    // is GC-allocated by the enclosing body and owned by its
                    // own finalizer, so the environment does not drop it.
                    self.backend
                        .builder
                        .build_store(field, binding.pointer)
                        .map_err(compiler_diagnostic)?;
                }
            }
        }
        if !captures.is_empty() {
            let finalizer = self.dynamic_closure_finalizer_code(instance, span)?;
            self.backend
                .set_gc_finalizer(closure_environment, finalizer)?;
        }
        Ok(closure_environment)
    }

    /// The declared dynamic closure-environment finalizer for one closure
    /// instance.
    fn dynamic_closure_finalizer_code(
        &self,
        instance: FunctionInstanceId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let ordinal = self
            .view
            .artifacts()
            .find_map(|(_, artifact)| match &artifact.plan {
                Some(crate::LoweredArtifactPlan::GcFinalizer(
                    crate::GcFinalizerPlan::DynamicClosureEnvironment { closure, .. },
                )) if *closure == instance => Some(artifact.ordinal),
                _ => None,
            })
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing dynamic closure finalizer plan")
            })?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(
                    span.clone(),
                    "missing dynamic closure finalizer declaration",
                )
            })
    }

    /// The descriptor one capture's storage uses: the stored run-time
    /// descriptor for a bare parameter, a static descriptor for a concrete
    /// type, or the pointer layout for a borrowed capture.
    fn capture_descriptor(
        &self,
        value_type: &CheckedType,
        runtime: &[TypeParameterId],
        descriptors: &[PointerValue<'context>],
        environment: &DynamicEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Option<PointerValue<'context>>> {
        match value_type {
            CheckedType::Parameter { id, .. } => {
                let index = runtime
                    .iter()
                    .position(|parameter| parameter == id)
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "capture parameter is not run-time")
                    })?;
                Ok(Some(descriptors[index]))
            }
            ty if !crate::contains_type_parameter(ty) => {
                Ok(Some(self.static_dynamic_descriptor(ty, span)?))
            }
            // A function value keeps its static closure layout, and a compound
            // run-time type uses its prepared descriptor (R4).
            ty => Ok(Some(self.dynamic_descriptor(ty, environment, span)?)),
        }
    }

    /// Capture offsets after the descriptor region, following the concrete
    /// environment's alignment rule. A `None` descriptor is a borrowed
    /// pointer capture.
    fn dynamic_capture_offsets(
        &self,
        runtime_count: usize,
        accesses: &[crate::LoweredCaptureAccess],
        descriptors: &[Option<PointerValue<'context>>],
    ) -> CodeGenerationResult<(
        Vec<inkwell::values::IntValue<'context>>,
        inkwell::values::IntValue<'context>,
    )> {
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let mut offset = self
            .backend
            .size_type
            .const_int(runtime_count as u64 * pointer_bytes, false);
        let mut offsets = Vec::with_capacity(accesses.len());
        for (index, access) in accesses.iter().enumerate() {
            let (size, align) = match (access, descriptors.get(index).copied().flatten()) {
                (
                    crate::LoweredCaptureAccess::Borrowed | crate::LoweredCaptureAccess::SharedCell,
                    _,
                ) => (
                    self.backend.size_type.const_int(pointer_bytes, false),
                    self.backend.size_type.const_int(pointer_bytes, false),
                ),
                (_, Some(descriptor)) => {
                    let layout = self.backend.load_descriptor_layout(descriptor)?;
                    (layout.store_size, layout.align)
                }
                (_, None) => {
                    return Err(Diagnostic::new(
                        staple_syntax::Span::Compiler,
                        "missing dynamic capture descriptor",
                    ));
                }
            };
            offset = self.runtime_align_up(offset, align)?;
            offsets.push(offset);
            offset = self
                .backend
                .builder
                .build_int_add(offset, size, "environment.offset")
                .map_err(compiler_diagnostic)?;
        }
        Ok((offsets, offset))
    }

    fn runtime_align_up(
        &self,
        value: inkwell::values::IntValue<'context>,
        alignment: inkwell::values::IntValue<'context>,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        let one = self.backend.size_type.const_int(1, false);
        let padding = self
            .backend
            .builder
            .build_int_sub(alignment, one, "environment.padding")
            .map_err(compiler_diagnostic)?;
        let padded = self
            .backend
            .builder
            .build_int_add(value, padding, "environment.padded")
            .map_err(compiler_diagnostic)?;
        let units = self
            .backend
            .builder
            .build_int_unsigned_div(padded, alignment, "environment.units")
            .map_err(compiler_diagnostic)?;
        Ok(self
            .backend
            .builder
            .build_int_mul(units, alignment, "environment.aligned")
            .map_err(compiler_diagnostic)?)
    }

    /// The element descriptor of a buffer intrinsic, from its `T` argument.
    fn intrinsic_element_descriptor(
        &self,
        call: &crate::LoweredCall,
        environment: &DynamicEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let element_type = call
            .substitutions
            .types
            .first()
            .map(|entry| entry.value_type.clone())
            .ok_or_else(|| Diagnostic::new(span.clone(), "buffer intrinsic has no element type"))?;
        self.dynamic_descriptor(&element_type, environment, span)
    }

    /// Load a buffer pointer from a dynamic value slot.
    fn load_buffer_pointer(
        &self,
        slot: PointerValue<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        Ok(self
            .load_dynamic_storage(
                self.backend.context.ptr_type(AddressSpace::default()),
                slot,
                "buffer.pointer",
            )
            .map_err(compiler_diagnostic)?
            .into_pointer_value())
    }

    /// Load a buffer header metadata word (0 length, 1 capacity).
    fn load_buffer_metadata(
        &self,
        buffer: PointerValue<'context>,
        field: u64,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let slot = self.backend.dynamic_byte_offset(
            buffer,
            self.backend
                .size_type
                .const_int(field * pointer_bytes, false),
        )?;
        Ok(self
            .load_dynamic_storage(self.backend.size_type, slot, "buffer.metadata")
            .map_err(compiler_diagnostic)?
            .into_int_value())
    }

    /// Trap when a buffer's frozen flag is set.
    fn trap_if_buffer_frozen(
        &self,
        buffer: PointerValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let slot = self.backend.dynamic_byte_offset(
            buffer,
            self.backend.size_type.const_int(2 * pointer_bytes, false),
        )?;
        let frozen = self
            .load_dynamic_storage(self.backend.context.i8_type(), slot, "buffer.frozen")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let frozen = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                frozen,
                self.backend.context.i8_type().const_zero(),
                "buffer.is_frozen",
            )
            .map_err(compiler_diagnostic)?;
        self.backend.build_trap_if(frozen, span.clone())
    }

    /// The runtime layout of a dynamic buffer: its header offset, element
    /// store size, and element stride, all from the element descriptor.
    fn dynamic_buffer_layout(
        &self,
        element: PointerValue<'context>,
    ) -> CodeGenerationResult<DynamicBufferLayout<'context>> {
        let layout = self.backend.load_descriptor_layout(element)?;
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let base = self
            .backend
            .size_type
            .const_int(2 * pointer_bytes + 1, false);
        let header_offset = self.runtime_align_up(base, layout.align)?;
        Ok(DynamicBufferLayout {
            store_size: layout.store_size,
            stride: layout.stride,
            header_offset,
        })
    }

    /// The one shared dynamic buffer finalizer (R6): read the element
    /// descriptor from the allocation's trailing word and drop every live
    /// element.
    pub(super) fn emit_dynamic_buffer_finalizer(
        &mut self,
        buffer: PointerValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let end_function = self
            .backend
            .llvm_module
            .get_function("__staple_gc_payload_end")
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing GC payload-end helper"))?;
        let end = self
            .backend
            .builder
            .build_call(end_function, &[buffer.into()], "buffer.payload.end")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| Diagnostic::new(span.clone(), "payload end has no value"))?
            .into_int_value();
        let word = self
            .backend
            .builder
            .build_int_sub(
                end,
                self.backend.size_type.const_int(pointer_bytes, false),
                "buffer.descriptor.address",
            )
            .map_err(compiler_diagnostic)?;
        let word = self
            .backend
            .builder
            .build_int_to_ptr(word, pointer, "buffer.descriptor.slot")
            .map_err(compiler_diagnostic)?;
        let element = self
            .load_dynamic_storage(pointer, word, "buffer.element.descriptor")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let layout = self.dynamic_buffer_layout(element)?;
        let length = self
            .load_dynamic_storage(self.backend.size_type, buffer, "buffer.length")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let data = self
            .backend
            .dynamic_byte_offset(buffer, layout.header_offset)?;
        let function = self
            .backend
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "buffer finalizer outside a function"))?;
        let index_slot = self
            .backend
            .entry_builder()
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
            .load_dynamic_storage(self.backend.size_type, index_slot, "buffer.finalize.index")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let finished = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::UGE,
                index,
                length,
                "buffer.finalize.done",
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_conditional_branch(finished, done, element_block)
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(element_block);
        let offset = self
            .backend
            .builder
            .build_int_mul(index, layout.stride, "buffer.finalize.offset")
            .map_err(compiler_diagnostic)?;
        let field = self.backend.dynamic_byte_offset(data, offset)?;
        self.backend.drop_dynamic_value(element, field)?;
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
        let _ = layout.store_size;
        Ok(())
    }

    /// The declared shared dynamic buffer finalizer.
    fn dynamic_buffer_finalizer_code(
        &self,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let ordinal = self
            .view
            .artifacts()
            .find_map(|(_, artifact)| match &artifact.plan {
                Some(crate::LoweredArtifactPlan::GcFinalizer(
                    crate::GcFinalizerPlan::DynamicBuffer,
                )) => Some(artifact.ordinal),
                _ => None,
            })
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing dynamic buffer finalizer plan")
            })?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing dynamic buffer finalizer declaration")
            })
    }

    /// A dynamic shared cell's finalizer (R-D): drop the value through the
    /// descriptor stored in the allocation's trailing word.
    pub(super) fn emit_dynamic_cell_finalizer(
        &mut self,
        cell: PointerValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let end_function = self
            .backend
            .llvm_module
            .get_function("__staple_gc_payload_end")
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing GC payload-end helper"))?;
        let end = self
            .backend
            .builder
            .build_call(end_function, &[cell.into()], "cell.payload.end")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| Diagnostic::new(span.clone(), "payload end has no value"))?
            .into_int_value();
        let word = self
            .backend
            .builder
            .build_int_sub(
                end,
                self.backend.size_type.const_int(pointer_bytes, false),
                "cell.descriptor.address",
            )
            .map_err(compiler_diagnostic)?;
        let word = self
            .backend
            .builder
            .build_int_to_ptr(word, pointer, "cell.descriptor.slot")
            .map_err(compiler_diagnostic)?;
        let descriptor = self
            .load_dynamic_storage(pointer, word, "cell.value.descriptor")
            .map_err(compiler_diagnostic)?;
        if let Some(instruction) = descriptor.as_instruction_value() {
            instruction
                .set_alignment(1)
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        }
        let descriptor = descriptor.into_pointer_value();
        self.backend.drop_dynamic_value(descriptor, cell)?;
        Ok(())
    }

    /// The declared shared dynamic cell finalizer.
    fn dynamic_cell_finalizer_code(
        &self,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let ordinal = self
            .view
            .artifacts()
            .find_map(|(_, artifact)| match &artifact.plan {
                Some(crate::LoweredArtifactPlan::GcFinalizer(
                    crate::GcFinalizerPlan::DynamicCell,
                )) => Some(artifact.ordinal),
                _ => None,
            })
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic cell finalizer plan"))?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing dynamic cell finalizer declaration")
            })
    }

    /// A mutable local's cell. A captured cell is a dynamic GC payload with a
    /// trailing descriptor word and the shared dynamic cell finalizer (R-D);
    /// an uncaptured cell is entry-block stack storage.
    fn allocate_dynamic_cell(
        &self,
        descriptor: PointerValue<'context>,
        captured: bool,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        if !captured {
            return self.backend.allocate_dynamic_value(descriptor);
        }
        let descriptor = self.copy_descriptor_to_gc(descriptor, span)?;
        let layout = self.backend.load_descriptor_layout(descriptor)?;
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let total = self
            .backend
            .builder
            .build_int_add(
                layout.store_size,
                self.backend.size_type.const_int(pointer_bytes + 1, false),
                "cell.total.size",
            )
            .map_err(compiler_diagnostic)?;
        let cell = self
            .backend
            .build_gc_allocation(total, "binding.cell", span.clone())?;
        let state = self.backend.dynamic_byte_offset(cell, layout.store_size)?;
        self.backend
            .builder
            .build_store(state, self.backend.context.i8_type().const_zero())
            .map_err(compiler_diagnostic)?;
        let descriptor_offset = self
            .backend
            .builder
            .build_int_add(
                layout.store_size,
                self.backend.size_type.const_int(1, false),
                "cell.descriptor.offset",
            )
            .map_err(compiler_diagnostic)?;
        let word = self.backend.dynamic_byte_offset(cell, descriptor_offset)?;
        self.backend
            .builder
            .build_store(word, descriptor)
            .map_err(compiler_diagnostic)?
            .set_alignment(1)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let finalizer = self.dynamic_cell_finalizer_code(span)?;
        self.backend.set_gc_finalizer(cell, finalizer)?;
        Ok(cell)
    }

    /// The dynamic closure environment's finalizer (R-D): drop every owned
    /// capture through the descriptor stored in the environment.
    pub(super) fn emit_dynamic_closure_finalizer(
        &mut self,
        plan: &crate::GcFinalizerPlan,
        environment: PointerValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let crate::GcFinalizerPlan::DynamicClosureEnvironment {
            runtime, captures, ..
        } = plan
        else {
            return Err(Diagnostic::new(
                span.clone(),
                "mismatched dynamic finalizer plan",
            ));
        };
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let capture_descriptors = captures
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let offset = self
                    .backend
                    .size_type
                    .const_int((runtime.len() + index) as u64 * pointer_bytes, false);
                let slot = self.backend.dynamic_byte_offset(environment, offset)?;
                Ok(Some(
                    self.load_dynamic_storage(pointer, slot, "finalizer.capture.descriptor")
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value(),
                ))
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let accesses = captures
            .iter()
            .map(|capture| capture.access)
            .collect::<Vec<_>>();
        let (offsets, _) = self.dynamic_capture_offsets(
            runtime.len() + captures.len(),
            &accesses,
            &capture_descriptors,
        )?;
        for (index, capture) in captures.iter().enumerate().rev() {
            if !capture.owns || capture.access != crate::LoweredCaptureAccess::ByValue {
                continue;
            }
            let Some(descriptor) = capture_descriptors[index] else {
                continue;
            };
            let field = self
                .backend
                .dynamic_byte_offset(environment, offsets[index])?;
            self.backend.drop_dynamic_value(descriptor, field)?;
        }
        Ok(())
    }

    /// Deep-copy one descriptor into a GC block so a closure environment never
    /// points at the building function's stack slots.
    fn copy_descriptor_to_gc(
        &self,
        descriptor: PointerValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        self.backend.copy_dynamic_descriptor(descriptor, span)
    }

    /// The declared descriptor-bound entry for one dynamic instance.
    fn descriptor_bound_entry_code(
        &self,
        instance: FunctionInstanceId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let ordinal = self
            .view
            .artifacts()
            .find_map(|(_, artifact)| match &artifact.plan {
                Some(crate::LoweredArtifactPlan::DescriptorBoundEntry(plan))
                    if plan.instance == instance =>
                {
                    Some(artifact.ordinal)
                }
                _ => None,
            })
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing descriptor-bound entry plan"))?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing descriptor-bound entry declaration")
            })
    }

    /// An indirect call through a concrete closure value (R2). The callee's
    /// function type is concrete, so the concrete closure ABI applies; the
    /// arguments and result move between dynamic storage and the concrete ABI.
    fn emit_dynamic_indirect_concrete_call(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        call: &crate::LoweredCall,
        callee: ExpressionId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let span = &call.origin.span;
        let dynamic_shape =
            crate::contains_type_parameter(&CheckedType::Function(call.function_type.clone()));
        let storage = *environment
            .temporaries
            .get(&id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic closure call storage"))?;
        let closure = self.emit_dynamic_expression(owner, callee, environment)?;
        let closure_type = self.backend.closure_type();
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        let code = self
            .backend
            .builder
            .build_struct_gep(closure_type, closure, 0, "closure.code.slot")
            .map_err(compiler_diagnostic)?;
        let code = self
            .load_dynamic_storage(pointer_type, code, "closure.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let closure_environment = self
            .backend
            .builder
            .build_struct_gep(closure_type, closure, 1, "closure.environment.slot")
            .map_err(compiler_diagnostic)?;
        let closure_environment = self
            .load_dynamic_storage(pointer_type, closure_environment, "closure.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let types = super::flattened_parameter_types(&call.function_type.parameter);
        let indirect = self.backend.indirect_parameter_mask(&call.function_type);
        let moved =
            super::super::abi::mutation_parameter_mask(types.len(), &call.function_type.moves);
        let mut slots = vec![None; types.len()];
        let mut unplaced = Vec::new();
        let mut cleanups = Vec::new();
        let mut hidden = Vec::new();
        for step in &call.steps {
            match step {
                LoweredCallStep::Callee { .. } => {}
                LoweredCallStep::Resource { resource } => {
                    hidden.push(self.dynamic_resource_argument(
                        owner,
                        call,
                        *resource,
                        dynamic_shape,
                        environment,
                    )?);
                }
                LoweredCallStep::Argument { argument } => {
                    let record = &call.arguments[*argument];
                    let value =
                        self.emit_dynamic_argument(owner, id, call, *argument, environment)?;
                    let cleanup = record.drops_after_call
                        || (record.temporary
                            && record.slot.is_some_and(|slot| !moved[slot])
                            && crate::contains_type_parameter(&record.expected));
                    if cleanup {
                        cleanups.push((
                            self.dynamic_descriptor(&record.expected, environment, span)?,
                            value,
                        ));
                    }
                    match record.slot {
                        Some(slot) => slots[slot] = Some(value),
                        None => unplaced.push(value),
                    }
                }
                LoweredCallStep::ProductElement {
                    expression, slot, ..
                }
                | LoweredCallStep::Default {
                    expression, slot, ..
                } => {
                    let value = self.emit_dynamic_expression(owner, *expression, environment)?;
                    slots[*slot] = Some(value);
                }
                LoweredCallStep::Invoke => break,
                _ => {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "dynamic closure argument step is not yet implemented",
                    ));
                }
            }
        }
        if slots.iter().all(Option::is_none) && unplaced.len() == 1 {
            let descriptor =
                self.dynamic_descriptor(&call.function_type.parameter, environment, span)?;
            for (index, slot) in slots.iter_mut().enumerate() {
                *slot = Some(
                    self.dynamic_product_field(descriptor, unplaced[0], index as u32)?
                        .0,
                );
            }
        }
        let mut arguments = vec![closure_environment.into()];
        if dynamic_shape {
            // The dynamic shape has no descriptor parameters; the result goes
            // through a caller-provided out slot and every value by pointer.
            arguments.push(storage.into());
        }
        arguments.extend(hidden);
        for (index, slot) in slots.iter().enumerate() {
            let pointer = slot
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic closure argument"))?;
            if dynamic_shape || indirect[index] {
                arguments.push(pointer.into());
            } else {
                let ty = self.backend.compile_type(types[index])?;
                arguments.push(
                    self.load_dynamic_storage(ty, pointer, "closure.argument")
                        .map_err(compiler_diagnostic)?
                        .into(),
                );
            }
        }
        let signature = if dynamic_shape {
            self.backend
                .compile_dynamic_function_type(&call.function_type, 0)?
        } else {
            self.backend
                .compile_closure_function_type(&call.function_type)?
        };
        let value = self
            .backend
            .builder
            .build_indirect_call(signature, code, &arguments, "closure.call")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic();
        if let Some(value) = value {
            self.backend
                .builder
                .build_store(storage, value)
                .map_err(compiler_diagnostic)?;
        }
        for (descriptor, pointer) in cleanups.into_iter().rev() {
            self.backend.drop_dynamic_value(descriptor, pointer)?;
        }
        Ok(storage)
    }

    /// A concrete extern call inside a dynamic body. Arguments are concrete
    /// scalars or pointers; each flattened slot follows the concrete pass
    /// mode, and the result is stored back into the call's reserved slot.
    fn emit_dynamic_extern_call(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        call: &crate::LoweredCall,
        symbol: crate::SymbolId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let span = &call.origin.span;
        let storage = *environment
            .temporaries
            .get(&id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic extern storage"))?;
        let types = super::flattened_parameter_types(&call.function_type.parameter);
        let indirect = self.backend.indirect_parameter_mask(&call.function_type);
        let mut slots = vec![None; types.len()];
        let mut unplaced = Vec::new();
        for step in &call.steps {
            match step {
                LoweredCallStep::Argument { argument } => {
                    let record = &call.arguments[*argument];
                    let value =
                        self.emit_dynamic_argument(owner, id, call, *argument, environment)?;
                    match record.slot {
                        Some(slot) => slots[slot] = Some(value),
                        None => unplaced.push(value),
                    }
                }
                LoweredCallStep::ProductElement {
                    expression, slot, ..
                }
                | LoweredCallStep::Default {
                    expression, slot, ..
                } => {
                    let value = self.emit_dynamic_expression(owner, *expression, environment)?;
                    slots[*slot] = Some(value);
                }
                LoweredCallStep::Invoke => break,
                _ => {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "dynamic extern argument step is not yet implemented",
                    ));
                }
            }
        }
        if slots.iter().all(Option::is_none) && unplaced.len() == 1 {
            let descriptor =
                self.dynamic_descriptor(&call.function_type.parameter, environment, span)?;
            for (index, slot) in slots.iter_mut().enumerate() {
                *slot = Some(
                    self.dynamic_product_field(descriptor, unplaced[0], index as u32)?
                        .0,
                );
            }
        }
        let mut arguments = Vec::with_capacity(slots.len());
        for (index, slot) in slots.iter().enumerate() {
            let pointer = slot
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic extern argument"))?;
            if indirect[index] {
                arguments.push(pointer.into());
            } else {
                let ty = self.backend.compile_type(types[index])?;
                arguments.push(
                    self.load_dynamic_storage(ty, pointer, "extern.argument")
                        .map_err(compiler_diagnostic)?
                        .into(),
                );
            }
        }
        let function = self
            .externs
            .get(&symbol)
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing extern declaration"))?;
        let value = self
            .backend
            .builder
            .build_direct_call(function, &arguments, "extern.call")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .basic();
        if let Some(value) = value {
            self.backend
                .builder
                .build_store(storage, value)
                .map_err(compiler_diagnostic)?;
        }
        Ok(storage)
    }

    /// Scalar intrinsics evaluate their concrete arguments from storage and
    /// store the SSA result back into the call's reserved slot. The set
    /// mirrors Decision 9's supported scalar operations; anything needing the
    /// GC or run-time descriptors stays with the concrete emitter.
    fn emit_dynamic_intrinsic(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        call: &crate::LoweredCall,
        intrinsic: crate::IntrinsicFunction,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        use inkwell::values::BasicValue;
        let span = &call.origin.span;
        let storage = *environment
            .temporaries
            .get(&id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic intrinsic storage"))?;
        let types = super::flattened_parameter_types(&call.function_type.parameter);
        let mut slots = vec![None; types.len()];
        let mut unplaced = Vec::new();
        for step in &call.steps {
            match step {
                LoweredCallStep::Argument { argument } => {
                    let record = &call.arguments[*argument];
                    let value =
                        self.emit_dynamic_argument(owner, id, call, *argument, environment)?;
                    match record.slot {
                        Some(slot) => slots[slot] = Some(value),
                        None => unplaced.push(value),
                    }
                }
                LoweredCallStep::ProductElement {
                    expression, slot, ..
                }
                | LoweredCallStep::Default {
                    expression, slot, ..
                } => {
                    let value = self.emit_dynamic_expression(owner, *expression, environment)?;
                    slots[*slot] = Some(value);
                }
                LoweredCallStep::Invoke => break,
                _ => {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "dynamic intrinsic argument step is not yet implemented",
                    ));
                }
            }
        }
        if slots.iter().all(Option::is_none) && unplaced.len() == 1 {
            let descriptor =
                self.dynamic_descriptor(&call.function_type.parameter, environment, span)?;
            for (index, slot) in slots.iter_mut().enumerate() {
                *slot = Some(
                    self.dynamic_product_field(descriptor, unplaced[0], index as u32)?
                        .0,
                );
            }
        }
        let slot_pointer = |index: usize| -> CodeGenerationResult<PointerValue<'context>> {
            slots
                .get(index)
                .and_then(|slot| *slot)
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic intrinsic argument"))
        };
        // Descriptor-sized intrinsics operate on storage pointers, not on
        // loaded SSA values.
        match intrinsic {
            crate::IntrinsicFunction::SliceLength => {
                let slice = slot_pointer(0)?;
                let length = self
                    .backend
                    .builder
                    .build_struct_gep(self.backend.slice_type(), slice, 1, "slice.length.slot")
                    .map_err(compiler_diagnostic)?;
                let length = self
                    .load_dynamic_storage(self.backend.size_type, length, "slice.length")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(storage, length)
                    .map_err(compiler_diagnostic)?;
                return Ok(storage);
            }
            crate::IntrinsicFunction::SliceGetRef => {
                let slice = slot_pointer(0)?;
                let position = slot_pointer(1)?;
                let slice_type = self.backend.slice_type();
                let data = self
                    .backend
                    .builder
                    .build_struct_gep(slice_type, slice, 0, "slice.data")
                    .map_err(compiler_diagnostic)?;
                let data = self
                    .load_dynamic_storage(
                        self.backend.context.ptr_type(AddressSpace::default()),
                        data,
                        "slice.pointer",
                    )
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let length = self
                    .backend
                    .builder
                    .build_struct_gep(slice_type, slice, 1, "slice.length.slot")
                    .map_err(compiler_diagnostic)?;
                let length = self
                    .load_dynamic_storage(self.backend.size_type, length, "slice.length")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let position = self
                    .load_dynamic_storage(self.backend.size_type, position, "slice.position")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let out_of_bounds = self
                    .backend
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::UGE,
                        position,
                        length,
                        "slice.out_of_bounds",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend.build_trap_if(out_of_bounds, span.clone())?;
                let crate::CheckedType::Ref(payload) = &call.result_type else {
                    return Err(Diagnostic::new(span.clone(), "get_ref result is not a Ref"));
                };
                let descriptor = self.dynamic_descriptor(payload, environment, span)?;
                let layout = self.backend.load_descriptor_layout(descriptor)?;
                let offset = self
                    .backend
                    .builder
                    .build_int_mul(position, layout.stride, "slice.offset")
                    .map_err(compiler_diagnostic)?;
                let element = self.backend.dynamic_byte_offset(data, offset)?;
                self.backend
                    .builder
                    .build_store(storage, element)
                    .map_err(compiler_diagnostic)?;
                return Ok(storage);
            }
            crate::IntrinsicFunction::RefReplace => {
                let reference = slot_pointer(0)?;
                let replacement = slot_pointer(1)?;
                let payload = self
                    .load_dynamic_storage(
                        self.backend.context.ptr_type(AddressSpace::default()),
                        reference,
                        "ref.replace.reference",
                    )
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let descriptor = self.dynamic_descriptor(&call.result_type, environment, span)?;
                self.backend
                    .copy_dynamic_value(descriptor, payload, storage)?;
                self.backend
                    .copy_dynamic_value(descriptor, replacement, payload)?;
                return Ok(storage);
            }
            crate::IntrinsicFunction::BufferLength | crate::IntrinsicFunction::BufferCapacity => {
                let buffer = slot_pointer(0)?;
                let buffer = self
                    .load_dynamic_storage(
                        self.backend.context.ptr_type(AddressSpace::default()),
                        buffer,
                        "buffer.pointer",
                    )
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let field = if intrinsic == crate::IntrinsicFunction::BufferLength {
                    0
                } else {
                    1
                };
                let slot = self
                    .backend
                    .builder
                    .build_struct_gep(
                        self.backend
                            .buffer_header_type(self.backend.context.i8_type().into()),
                        buffer,
                        field,
                        "buffer.metadata.slot",
                    )
                    .map_err(compiler_diagnostic)?;
                let value = self
                    .load_dynamic_storage(self.backend.size_type, slot, "buffer.metadata")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(storage, value)
                    .map_err(compiler_diagnostic)?;
                return Ok(storage);
            }
            crate::IntrinsicFunction::BufferWithCapacity => {
                let capacity = slot_pointer(0)?;
                let capacity = self
                    .load_dynamic_storage(self.backend.size_type, capacity, "buffer.capacity")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let element = self.intrinsic_element_descriptor(call, environment, span)?;
                let layout = self.dynamic_buffer_layout(element)?;
                let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
                let bytes = self
                    .backend
                    .builder
                    .build_int_mul(capacity, layout.stride, "buffer.bytes")
                    .map_err(compiler_diagnostic)?;
                let concrete = self
                    .backend
                    .builder
                    .build_int_add(layout.header_offset, bytes, "buffer.concrete.size")
                    .map_err(compiler_diagnostic)?;
                let total = self
                    .backend
                    .builder
                    .build_int_add(
                        concrete,
                        self.backend.size_type.const_int(pointer_bytes, false),
                        "buffer.total.size",
                    )
                    .map_err(compiler_diagnostic)?;
                let buffer = self
                    .backend
                    .build_gc_allocation(total, "buffer", span.clone())?;
                self.backend
                    .builder
                    .build_store(buffer, self.backend.size_type.const_zero())
                    .map_err(compiler_diagnostic)?;
                let capacity_slot = self.backend.dynamic_byte_offset(
                    buffer,
                    self.backend.size_type.const_int(pointer_bytes, false),
                )?;
                self.backend
                    .builder
                    .build_store(capacity_slot, capacity)
                    .map_err(compiler_diagnostic)?;
                let frozen_slot = self.backend.dynamic_byte_offset(
                    buffer,
                    self.backend.size_type.const_int(2 * pointer_bytes, false),
                )?;
                self.backend
                    .builder
                    .build_store(frozen_slot, self.backend.context.i8_type().const_zero())
                    .map_err(compiler_diagnostic)?;
                let descriptor_slot = self.backend.dynamic_byte_offset(buffer, concrete)?;
                self.backend
                    .builder
                    .build_store(descriptor_slot, element)
                    .map_err(compiler_diagnostic)?;
                let finalizer = self.dynamic_buffer_finalizer_code(span)?;
                self.backend.set_gc_finalizer(buffer, finalizer)?;
                self.backend
                    .builder
                    .build_store(storage, buffer)
                    .map_err(compiler_diagnostic)?;
                return Ok(storage);
            }
            crate::IntrinsicFunction::BufferPush => {
                let buffer = self.load_buffer_pointer(slot_pointer(0)?)?;
                let value = slot_pointer(1)?;
                let element = self.intrinsic_element_descriptor(call, environment, span)?;
                let layout = self.dynamic_buffer_layout(element)?;
                self.trap_if_buffer_frozen(buffer, span)?;
                let length = self.load_buffer_metadata(buffer, 0)?;
                let capacity = self.load_buffer_metadata(buffer, 1)?;
                let full = self
                    .backend
                    .builder
                    .build_int_compare(inkwell::IntPredicate::UGE, length, capacity, "buffer.full")
                    .map_err(compiler_diagnostic)?;
                self.backend.build_trap_if(full, span.clone())?;
                let data = self
                    .backend
                    .dynamic_byte_offset(buffer, layout.header_offset)?;
                let offset = self
                    .backend
                    .builder
                    .build_int_mul(length, layout.stride, "buffer.push.offset")
                    .map_err(compiler_diagnostic)?;
                let field = self.backend.dynamic_byte_offset(data, offset)?;
                self.backend.copy_dynamic_value(element, value, field)?;
                let next = self
                    .backend
                    .builder
                    .build_int_add(
                        length,
                        self.backend.size_type.const_int(1, false),
                        "buffer.push.length",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(buffer, next)
                    .map_err(compiler_diagnostic)?;
                return Ok(storage);
            }
            crate::IntrinsicFunction::BufferGet => {
                let buffer = self.load_buffer_pointer(slot_pointer(0)?)?;
                let index = slot_pointer(1)?;
                let index = self
                    .load_dynamic_storage(self.backend.size_type, index, "buffer.index")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let element = self.intrinsic_element_descriptor(call, environment, span)?;
                let layout = self.dynamic_buffer_layout(element)?;
                let data = self
                    .backend
                    .dynamic_byte_offset(buffer, layout.header_offset)?;
                let offset = self
                    .backend
                    .builder
                    .build_int_mul(index, layout.stride, "buffer.get.offset")
                    .map_err(compiler_diagnostic)?;
                let field = self.backend.dynamic_byte_offset(data, offset)?;
                self.backend
                    .builder
                    .build_store(storage, field)
                    .map_err(compiler_diagnostic)?;
                return Ok(storage);
            }
            crate::IntrinsicFunction::BufferFreeze => {
                let buffer = self.load_buffer_pointer(slot_pointer(0)?)?;
                let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
                let frozen_slot = self.backend.dynamic_byte_offset(
                    buffer,
                    self.backend.size_type.const_int(2 * pointer_bytes, false),
                )?;
                self.backend
                    .builder
                    .build_store(
                        frozen_slot,
                        self.backend.context.i8_type().const_int(1, false),
                    )
                    .map_err(compiler_diagnostic)?;
                let length = self.load_buffer_metadata(buffer, 0)?;
                let element = self.intrinsic_element_descriptor(call, environment, span)?;
                let layout = self.dynamic_buffer_layout(element)?;
                let data = self
                    .backend
                    .dynamic_byte_offset(buffer, layout.header_offset)?;
                self.backend
                    .builder
                    .build_store(storage, data)
                    .map_err(compiler_diagnostic)?;
                let length_slot = self.backend.dynamic_byte_offset(
                    storage,
                    self.backend.size_type.const_int(pointer_bytes, false),
                )?;
                self.backend
                    .builder
                    .build_store(length_slot, length)
                    .map_err(compiler_diagnostic)?;
                return Ok(storage);
            }
            crate::IntrinsicFunction::BufferPop => {
                let buffer = self.load_buffer_pointer(slot_pointer(0)?)?;
                let Some(alternatives) = call.buffer_pop else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "buffer pop is missing its option alternatives",
                    ));
                };
                let element = self.intrinsic_element_descriptor(call, environment, span)?;
                let layout = self.dynamic_buffer_layout(element)?;
                self.trap_if_buffer_frozen(buffer, span)?;
                let length = self.load_buffer_metadata(buffer, 0)?;
                let descriptor = self.dynamic_descriptor(&call.result_type, environment, span)?;
                let result_layout = self.backend.load_descriptor_layout(descriptor)?;
                self.backend
                    .builder
                    .build_memset(
                        storage,
                        1,
                        self.backend.context.i8_type().const_zero(),
                        result_layout.store_size,
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(
                        storage,
                        self.backend
                            .context
                            .i32_type()
                            .const_int(alternatives.none as u64, false),
                    )
                    .map_err(compiler_diagnostic)?;
                let function = self.current_dynamic_function(span)?;
                let some = self
                    .backend
                    .context
                    .append_basic_block(function, "buffer.pop.some");
                let done = self
                    .backend
                    .context
                    .append_basic_block(function, "buffer.pop.done");
                let empty = self
                    .backend
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::EQ,
                        length,
                        self.backend.size_type.const_zero(),
                        "buffer.pop.empty",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_conditional_branch(empty, done, some)
                    .map_err(compiler_diagnostic)?;
                self.backend.builder.position_at_end(some);
                let next = self
                    .backend
                    .builder
                    .build_int_sub(
                        length,
                        self.backend.size_type.const_int(1, false),
                        "buffer.pop.length",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(buffer, next)
                    .map_err(compiler_diagnostic)?;
                let data = self
                    .backend
                    .dynamic_byte_offset(buffer, layout.header_offset)?;
                let offset = self
                    .backend
                    .builder
                    .build_int_mul(next, layout.stride, "buffer.pop.offset")
                    .map_err(compiler_diagnostic)?;
                let field = self.backend.dynamic_byte_offset(data, offset)?;
                let payload_count = alternatives.none.max(alternatives.some) + 1;
                let payload = self.dynamic_sum_payload(descriptor, storage, payload_count)?;
                self.backend.copy_dynamic_value(element, field, payload)?;
                self.backend
                    .builder
                    .build_store(
                        storage,
                        self.backend
                            .context
                            .i32_type()
                            .const_int(alternatives.some as u64, false),
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_unconditional_branch(done)
                    .map_err(compiler_diagnostic)?;
                self.backend.builder.position_at_end(done);
                return Ok(storage);
            }
            crate::IntrinsicFunction::BufferTransfer => {
                let source = self.load_buffer_pointer(slot_pointer(0)?)?;
                let destination = self.load_buffer_pointer(slot_pointer(1)?)?;
                let element = self.intrinsic_element_descriptor(call, environment, span)?;
                let layout = self.dynamic_buffer_layout(element)?;
                self.trap_if_buffer_frozen(source, span)?;
                self.trap_if_buffer_frozen(destination, span)?;
                let source_length = self.load_buffer_metadata(source, 0)?;
                let destination_length = self.load_buffer_metadata(destination, 0)?;
                let destination_capacity = self.load_buffer_metadata(destination, 1)?;
                let required = self
                    .backend
                    .builder
                    .build_int_add(
                        destination_length,
                        source_length,
                        "buffer.transfer.required",
                    )
                    .map_err(compiler_diagnostic)?;
                let overflow = self
                    .backend
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::UGT,
                        required,
                        destination_capacity,
                        "buffer.transfer.overflow",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend.build_trap_if(overflow, span.clone())?;
                let source_data = self
                    .backend
                    .dynamic_byte_offset(source, layout.header_offset)?;
                let destination_data = self
                    .backend
                    .dynamic_byte_offset(destination, layout.header_offset)?;
                let destination_offset = self
                    .backend
                    .builder
                    .build_int_mul(destination_length, layout.stride, "buffer.transfer.offset")
                    .map_err(compiler_diagnostic)?;
                let destination_field = self
                    .backend
                    .dynamic_byte_offset(destination_data, destination_offset)?;
                let bytes = self
                    .backend
                    .builder
                    .build_int_mul(source_length, layout.stride, "buffer.transfer.bytes")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_memcpy(destination_field, 1, source_data, 1, bytes)
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(destination, required)
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(source, self.backend.size_type.const_zero())
                    .map_err(compiler_diagnostic)?;
                return Ok(storage);
            }
            _ => {}
        }
        let mut values = Vec::with_capacity(slots.len());
        for index in 0..slots.len() {
            let pointer = slot_pointer(index)?;
            let ty = self.backend.compile_type(types[index])?;
            values.push(
                self.load_dynamic_storage(ty, pointer, "intrinsic.argument")
                    .map_err(compiler_diagnostic)?,
            );
        }
        let result = match intrinsic {
            crate::IntrinsicFunction::ParseNumber { .. }
            | crate::IntrinsicFunction::ValidateUtf8
            | crate::IntrinsicFunction::StringHasNul
            | crate::IntrinsicFunction::CStringBytes
            | crate::IntrinsicFunction::ToString { .. }
            | crate::IntrinsicFunction::StringFromCString
            | crate::IntrinsicFunction::StringToCString
            | crate::IntrinsicFunction::StringAdd => {
                let Some(LoweredExpressionKind::Call(call_id)) =
                    self.view.expression(owner, id).map(|record| &record.kind)
                else {
                    return Err(Diagnostic::new(span.clone(), "intrinsic is not a call"));
                };
                let arguments = values.iter().copied().map(Into::into).collect::<Vec<_>>();
                inkwell::values::BasicValueEnum::try_from(
                    self.emit_string_intrinsic(owner, call, *call_id, intrinsic, &arguments)?,
                )
                .map_err(|_| {
                    Diagnostic::new(span.clone(), "string intrinsic result is not a value")
                })?
            }
            crate::IntrinsicFunction::NumericConvert { from, to } => self
                .backend
                .build_numeric_conversion(from, to, values[0])?
                .as_basic_value_enum(),
            crate::IntrinsicFunction::PointerCast => values[0],
            crate::IntrinsicFunction::PointerAddress => self
                .backend
                .builder
                .build_ptr_to_int(
                    values[0].into_pointer_value(),
                    self.backend.size_type,
                    "pointer.address",
                )
                .map_err(compiler_diagnostic)?
                .as_basic_value_enum(),
            crate::IntrinsicFunction::AddressPointer => self
                .backend
                .builder
                .build_int_to_ptr(
                    values[0].into_int_value(),
                    self.backend.context.ptr_type(AddressSpace::default()),
                    "address.pointer",
                )
                .map_err(compiler_diagnostic)?
                .as_basic_value_enum(),
            crate::IntrinsicFunction::IntegerBinary { integer, operation } => self
                .backend
                .build_integer_binary(
                    integer,
                    operation,
                    values[0].into_int_value(),
                    values[1].into_int_value(),
                )?
                .as_basic_value_enum(),
            crate::IntrinsicFunction::IntegerCompare { integer, operation } => {
                let condition = self.backend.build_integer_compare(
                    integer,
                    operation,
                    values[0].into_int_value(),
                    values[1].into_int_value(),
                )?;
                self.dynamic_bool_value(condition, &call.result_type, span)?
            }
            crate::IntrinsicFunction::FloatBinary { float, operation } => self
                .backend
                .build_float_binary(
                    float,
                    operation,
                    values[0].into_float_value(),
                    values[1].into_float_value(),
                )?
                .as_basic_value_enum(),
            crate::IntrinsicFunction::FloatCompare { float, operation } => {
                let condition = self.backend.build_float_compare(
                    float,
                    operation,
                    values[0].into_float_value(),
                    values[1].into_float_value(),
                )?;
                self.dynamic_bool_value(condition, &call.result_type, span)?
            }
            _ => {
                return Err(Diagnostic::new(
                    span.clone(),
                    "this dynamic intrinsic is not yet implemented",
                ));
            }
        };
        self.backend
            .builder
            .build_store(storage, result)
            .map_err(compiler_diagnostic)?;
        Ok(storage)
    }

    fn dynamic_bool_value(
        &self,
        condition: inkwell::values::IntValue<'context>,
        result_type: &CheckedType,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<inkwell::values::BasicValueEnum<'context>> {
        let sum = crate::typecheck::sum_through_implicit_wrapper(result_type).ok_or_else(|| {
            Diagnostic::new(span.clone(), "dynamic comparison result must be Bool")
        })?;
        let sum_type = self.backend.compile_sum_type(sum)?;
        inkwell::values::BasicValueEnum::try_from(self.backend.build_bool_value(
            condition,
            sum_type,
            span.clone(),
        )?)
        .map_err(|_| Diagnostic::new(span.clone(), "dynamic comparison result is not first-class"))
    }

    /// The forwarding entry a dynamic-shape closure points at (R-C). Its
    /// parameters are the dynamic shape without descriptors; the body loads
    /// the instance's descriptors from the closure environment and calls the
    /// instance with them.
    pub(super) fn emit_descriptor_bound_entry(
        &mut self,
        ordinal: crate::specialization::ArtifactOrdinal,
        plan: &crate::DescriptorBoundEntryPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let function = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing descriptor-bound entry declaration")
            })?;
        let instance = self.instances.get(&plan.instance).copied().ok_or_else(|| {
            Diagnostic::new(span.clone(), "missing descriptor-bound entry instance")
        })?;
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let parameters = function.get_params();
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
        let environment = parameters[0].into_pointer_value();
        let mut arguments = vec![parameters[0].into()];
        for index in 0..plan.runtime.len() {
            let offset = self
                .backend
                .size_type
                .const_int(index as u64 * pointer_bytes, false);
            let slot = self.backend.dynamic_byte_offset(environment, offset)?;
            let descriptor = self
                .load_dynamic_storage(pointer, slot, "entry.descriptor")
                .map_err(compiler_diagnostic)?;
            arguments.push(descriptor.into());
        }
        for parameter in &parameters[1..] {
            arguments.push((*parameter).into());
        }
        self.backend
            .builder
            .build_direct_call(instance, &arguments, "")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    pub(super) fn emit_dynamic_instance_body(
        &mut self,
        id: FunctionInstanceId,
        function: FunctionValue<'context>,
        body: &LoweredInstanceBody,
        root: BlockId,
    ) -> CodeGenerationResult<()> {
        let unsupported = |detail| {
            Diagnostic::new(
                body.origin.span.clone(),
                format!("dynamic implementation does not yet support {detail}"),
            )
        };
        let runtime = self
            .view
            .instance_runtime_order(id)
            .ok_or_else(|| unsupported("missing key"))?;
        let parameters = function.get_params();
        let descriptors = runtime
            .iter()
            .enumerate()
            .map(|(index, parameter)| (*parameter, parameters[index + 1].into_pointer_value()))
            .collect::<HashMap<_, _>>();
        let resource_count = body.signature.effects.resources.len();
        let mut resources = HashMap::new();
        if body.function_providers.len() != resource_count {
            return Err(unsupported(
                "resource providers disagree with the effect row",
            ));
        }
        for (position, provider_id) in body.function_providers.iter().enumerate() {
            let provider = self
                .view
                .resource_provider(EmissionOwner::Instance(id), *provider_id)
                .ok_or_else(|| unsupported("missing function resource provider"))?;
            let pointer = parameters
                .get(1 + runtime.len() + 1 + position)
                .ok_or_else(|| unsupported("missing function resource parameter"))?
                .into_pointer_value();
            resources.insert(
                *provider_id,
                DynamicResource {
                    value_type: provider.resource.value_type.clone(),
                    pointer,
                },
            );
        }
        let mut environment = DynamicEnvironment {
            descriptors,
            compound_descriptors: Vec::new(),
            bindings: HashMap::new(),
            binding_order: Vec::new(),
            temporaries: HashMap::new(),
            packed_arguments: HashMap::new(),
            coercions: HashMap::new(),
            loop_results: HashMap::new(),
            cells: HashMap::new(),
            initialization_states: HashMap::new(),
            resources,
            loops: Vec::new(),
            closure: parameters[0].into_pointer_value(),
            result: parameters[1 + runtime.len()].into_pointer_value(),
            result_descriptor: self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null(),
            returned: false,
        };
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        // Seed capture bindings from the closure environment (R-D). The
        // descriptor region holds one descriptor per run-time parameter in
        // R-A order; captures follow at descriptor-computed offsets.
        if !body.captures.is_empty() {
            let accesses = body
                .captures
                .iter()
                .map(|capture| {
                    if capture.requires_initialization_state
                        || capture.mutable_storage
                        || capture.derived
                    {
                        crate::LoweredCaptureAccess::SharedCell
                    } else if capture.capture.borrowed {
                        crate::LoweredCaptureAccess::Borrowed
                    } else {
                        crate::LoweredCaptureAccess::ByValue
                    }
                })
                .collect::<Vec<_>>();
            let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
            let pointer_bytes = self.backend.target_data.get_pointer_byte_size(None) as u64;
            let capture_descriptors = body
                .captures
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    let offset = self
                        .backend
                        .size_type
                        .const_int((runtime.len() + index) as u64 * pointer_bytes, false);
                    let slot = self
                        .backend
                        .dynamic_byte_offset(environment.closure, offset)?;
                    Ok(Some(
                        self.load_dynamic_storage(pointer_type, slot, "capture.descriptor")
                            .map_err(compiler_diagnostic)?
                            .into_pointer_value(),
                    ))
                })
                .collect::<CodeGenerationResult<Vec<_>>>()?;
            let (offsets, _) = self.dynamic_capture_offsets(
                runtime.len() + body.captures.len(),
                &accesses,
                &capture_descriptors,
            )?;
            for (index, capture) in body.captures.iter().enumerate() {
                let field = self
                    .backend
                    .dynamic_byte_offset(environment.closure, offsets[index])?;
                let pointer = match accesses[index] {
                    crate::LoweredCaptureAccess::ByValue => field,
                    crate::LoweredCaptureAccess::Borrowed => self
                        .load_dynamic_storage(
                            self.backend.context.ptr_type(AddressSpace::default()),
                            field,
                            "capture.pointer",
                        )
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value(),
                    crate::LoweredCaptureAccess::SharedCell => self
                        .load_dynamic_storage(
                            self.backend.context.ptr_type(AddressSpace::default()),
                            field,
                            "capture.cell",
                        )
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value(),
                };
                let descriptor = capture_descriptors[index].ok_or_else(|| {
                    Diagnostic::new(body.origin.span.clone(), "missing capture descriptor")
                })?;
                environment.bindings.insert(
                    capture.capture.symbol,
                    DynamicBinding {
                        pointer,
                        descriptor,
                        owned: false,
                        live: true,
                    },
                );
                environment.binding_order.push(capture.capture.symbol);
            }
        }
        self.prepare_dynamic_descriptor(
            &body.signature.parameter,
            &mut environment,
            &body.origin.span,
        )?;
        environment.result_descriptor = self.prepare_dynamic_descriptor(
            &body.signature.result,
            &mut environment,
            &body.origin.span,
        )?;
        if let Some(coercion) = &body.body_coercion {
            self.prepare_dynamic_descriptor(&coercion.source, &mut environment, &body.origin.span)?;
            self.prepare_dynamic_descriptor(&coercion.target, &mut environment, &body.origin.span)?;
        }
        for (expression_id, record) in body.expressions() {
            if matches!(record.kind, LoweredExpressionKind::Deferred(_)) {
                continue;
            }
            self.prepare_dynamic_descriptor(
                &record.value_type,
                &mut environment,
                &record.origin.span,
            )?;
            if let Some(coercion) = &record.coercion {
                self.prepare_dynamic_descriptor(
                    &coercion.source,
                    &mut environment,
                    &record.origin.span,
                )?;
                let descriptor = self.prepare_dynamic_descriptor(
                    &coercion.target,
                    &mut environment,
                    &record.origin.span,
                )?;
                if !matches!(record.coercion_plan, Some(LoweredCoercionPlan::Identity)) {
                    environment.coercions.insert(
                        expression_id,
                        self.backend.allocate_dynamic_value(descriptor)?,
                    );
                }
            }
            if let LoweredExpressionKind::Call(call) = record.kind {
                let call = self
                    .view
                    .call(EmissionOwner::Instance(id), call)
                    .expect("owned call");
                self.prepare_dynamic_descriptor(
                    &call.function_type.parameter,
                    &mut environment,
                    &record.origin.span,
                )?;
                for entry in &call.substitutions.types {
                    self.prepare_dynamic_descriptor(
                        &entry.value_type,
                        &mut environment,
                        &record.origin.span,
                    )?;
                }
            }
            if let LoweredExpressionKind::Loop(loop_) = &record.kind {
                self.prepare_dynamic_descriptor(
                    &loop_.result_type,
                    &mut environment,
                    &record.origin.span,
                )?;
            }
            if let LoweredExpressionKind::CallableValue(value_id) = &record.kind
                && let Some(callable) = self
                    .view
                    .callable_value(EmissionOwner::Instance(id), *value_id)
            {
                for entry in &callable.substitutions.types {
                    self.prepare_dynamic_descriptor(
                        &entry.value_type,
                        &mut environment,
                        &record.origin.span,
                    )?;
                }
            }
        }
        // A mutable local's value lives in its own entry-block storage. The
        // cell is allocated before any body control flow so assignments in
        // loops never repeat stack allocation.
        let owner = EmissionOwner::Instance(id);
        for (_, item) in body.items() {
            let LoweredItemKind::Binding(binding) = &item.kind else {
                continue;
            };
            if !binding.cell
                || binding.signal
                || binding.derived
                || binding.generic
                || binding.compile_time_only
            {
                continue;
            }
            let Some(symbol) = binding.symbol else {
                continue;
            };
            let ty = self
                .view
                .owner_symbol_type(owner, symbol)
                .ok_or_else(|| Diagnostic::new(item.origin.span.clone(), "missing cell type"))?
                .clone();
            self.prepare_dynamic_descriptor(&ty, &mut environment, &item.origin.span)?;
            let descriptor = self.dynamic_descriptor(&ty, &environment, &item.origin.span)?;
            let captured = self.view.symbol(symbol).is_some_and(|record| {
                record.captured
                    && (record.mutable_storage
                        || record.derived
                        || record.requires_initialization_check)
            });
            let cell = self.allocate_dynamic_cell(descriptor, captured, &item.origin.span)?;
            environment.cells.insert(symbol, cell);
        }
        // Reserve call results before any body control flow. Descriptor-derived
        // sizes dominate every alloca and each slot is allocated once per call.
        for (expression, record) in body.expressions() {
            match &record.kind {
                LoweredExpressionKind::Call(_)
                | LoweredExpressionKind::Index(_)
                | LoweredExpressionKind::Product(_)
                | LoweredExpressionKind::RepeatedProduct(_)
                | LoweredExpressionKind::Integer(_)
                | LoweredExpressionKind::Float(_)
                | LoweredExpressionKind::String(_)
                | LoweredExpressionKind::CString(_)
                | LoweredExpressionKind::StringTemplate(_)
                | LoweredExpressionKind::Resource(_)
                | LoweredExpressionKind::CallableValue(_) => {
                    let descriptor = self.dynamic_descriptor(
                        record
                            .coercion
                            .as_ref()
                            .map_or(&record.value_type, |coercion| &coercion.source),
                        &environment,
                        &record.origin.span,
                    )?;
                    environment
                        .temporaries
                        .insert(expression, self.backend.allocate_dynamic_value(descriptor)?);
                }
                LoweredExpressionKind::Loop(loop_) => {
                    let descriptor = self.dynamic_descriptor(
                        &loop_.result_type,
                        &environment,
                        &record.origin.span,
                    )?;
                    environment
                        .loop_results
                        .insert(expression, self.backend.allocate_dynamic_value(descriptor)?);
                }
                LoweredExpressionKind::Name(name) if name.singleton.is_some() => {
                    let descriptor = self.dynamic_descriptor(
                        record
                            .coercion
                            .as_ref()
                            .map_or(&record.value_type, |coercion| &coercion.source),
                        &environment,
                        &record.origin.span,
                    )?;
                    environment
                        .temporaries
                        .insert(expression, self.backend.allocate_dynamic_value(descriptor)?);
                }
                _ => {}
            }
        }
        for (expression, record) in body.expressions() {
            let LoweredExpressionKind::Call(call_id) = record.kind else {
                continue;
            };
            let call = self
                .view
                .call(EmissionOwner::Instance(id), call_id)
                .expect("owned call");
            let Some(LoweredBoundTarget::Instance(callee)) = self.view.binding(
                EmissionOwner::Instance(id),
                LoweredBindingSite::Call(call_id),
            ) else {
                continue;
            };
            let signature = self
                .view
                .instance_signature(*callee)
                .expect("callee signature");
            if !matches!(signature.parameter.as_ref(), CheckedType::Product(_))
                && matches!(
                    call.function_type.parameter.as_ref(),
                    CheckedType::Product(_)
                )
            {
                let descriptor = self.dynamic_descriptor(
                    &call.function_type.parameter,
                    &environment,
                    &record.origin.span,
                )?;
                environment
                    .packed_arguments
                    .insert(expression, self.backend.allocate_dynamic_value(descriptor)?);
            }
        }
        let owner = EmissionOwner::Instance(id);
        let raw = &parameters[2 + runtime.len() + resource_count..];
        let whole = body
            .signature
            .mutations
            .contains(&crate::CheckedMutation::Whole);
        let types = super::flattened_parameter_types(&body.signature.parameter);
        let moves = super::super::abi::mutation_parameter_mask(types.len(), &body.signature.moves);
        let descriptor =
            self.dynamic_descriptor(&body.signature.parameter, &environment, &body.origin.span)?;
        let pattern = self
            .view
            .pattern(owner, body.parameter_pattern)
            .expect("parameter pattern");
        if whole || !matches!(body.signature.parameter.as_ref(), CheckedType::Product(_)) {
            if whole && moves.iter().any(|owned| *owned) && !moves.iter().all(|owned| *owned) {
                return Err(unsupported("partially moved whole-product parameters"));
            }
            if raw.len() != 1 {
                return Err(unsupported("parameter arity"));
            }
            self.bind_dynamic_pattern(
                owner,
                body.parameter_pattern,
                raw[0].into_pointer_value(),
                descriptor,
                moves.iter().all(|owned| *owned),
                &mut environment,
            )?;
        } else if let LoweredPatternKind::Product { elements, .. } = &pattern.kind {
            if raw.len() != elements.len() || raw.len() != types.len() {
                return Err(unsupported("parameter arity"));
            }
            for (index, pattern) in elements.iter().enumerate() {
                let descriptor =
                    self.dynamic_descriptor(types[index], &environment, &body.origin.span)?;
                self.bind_dynamic_pattern(
                    owner,
                    *pattern,
                    raw[index].into_pointer_value(),
                    descriptor,
                    moves[index],
                    &mut environment,
                )?;
            }
        } else {
            if raw.len() != types.len() {
                return Err(unsupported("parameter arity"));
            }
            if moves.iter().any(|owned| *owned) && !moves.iter().all(|owned| *owned) {
                return Err(unsupported("partially moved whole-product bindings"));
            }
            let storage = self.backend.allocate_dynamic_value(descriptor)?;
            for (index, argument) in raw.iter().enumerate() {
                let (field, component) =
                    self.dynamic_product_field(descriptor, storage, index as u32)?;
                self.backend
                    .copy_dynamic_value(component, argument.into_pointer_value(), field)?;
            }
            self.bind_dynamic_pattern(
                owner,
                body.parameter_pattern,
                storage,
                descriptor,
                moves.iter().all(|owned| *owned),
                &mut environment,
            )?;
        }
        let value = self.emit_dynamic_block(owner, root, &mut environment)?;
        for symbol in &body.body_moved_symbols {
            if let Some(binding) = environment.bindings.get_mut(symbol) {
                binding.live = false;
            }
        }
        if !environment.returned {
            // An empty block result is the unit value; its descriptor has zero
            // store size, so a null source makes the copy a no-op.
            let value = value.unwrap_or_else(|| {
                self.backend
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null()
            });
            if let (Some(coercion), Some(plan)) = (&body.body_coercion, &body.body_coercion_plan) {
                self.coerce_dynamic_value(
                    value,
                    &coercion.source,
                    &coercion.target,
                    plan,
                    environment.result,
                    &environment,
                    &body.origin.span,
                )?;
            } else {
                self.backend.copy_dynamic_value(
                    environment.result_descriptor,
                    value,
                    environment.result,
                )?;
            }
            self.drop_dynamic_bindings(&mut environment, 0)?;
            self.backend
                .builder
                .build_return(None)
                .map_err(compiler_diagnostic)?;
        }
        Ok(())
    }

    /// Build the finite descriptor graph in the entry block before value
    /// allocations or body control flow. Cache by the instance-local type.
    fn prepare_dynamic_descriptor(
        &self,
        ty: &CheckedType,
        environment: &mut DynamicEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        use crate::specialization::{CompoundDescriptorDropKey, CompoundDescriptorShape};
        if let Some((_, descriptor)) = environment
            .compound_descriptors
            .iter()
            .find(|(value_type, _)| value_type == ty)
        {
            return Ok(*descriptor);
        }
        if !crate::contains_type_parameter(ty) || matches!(ty, CheckedType::Parameter { .. }) {
            return self.dynamic_descriptor(ty, environment, span);
        }
        if is_static_layout_type(ty) {
            let layout = self.backend.descriptor_layout(ty)?;
            let runtime = self.backend.static_runtime_layout(layout);
            let descriptor = self.backend.build_runtime_descriptor(
                runtime,
                &[],
                self.backend
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null(),
            )?;
            environment
                .compound_descriptors
                .push((ty.clone(), descriptor));
            return Ok(descriptor);
        }
        let (shape, children): (CompoundDescriptorShape, Vec<&CheckedType>) = match ty {
            CheckedType::Product(product) => (
                CompoundDescriptorShape::Product,
                product
                    .elements
                    .iter()
                    .map(|field| &field.value_type)
                    .collect(),
            ),
            CheckedType::Sum(sum) => (
                CompoundDescriptorShape::Sum,
                sum.alternatives.iter().collect(),
            ),
            CheckedType::Wrapper { representation, .. } => {
                let descriptor =
                    self.prepare_dynamic_descriptor(representation, environment, span)?;
                environment
                    .compound_descriptors
                    .push((ty.clone(), descriptor));
                return Ok(descriptor);
            }
            _ => {
                return Err(Diagnostic::new(
                    span.clone(),
                    "this dynamic descriptor shape is not yet supported",
                ));
            }
        };
        let components = children
            .into_iter()
            .map(|child| self.prepare_dynamic_descriptor(child, environment, span))
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let layouts = components
            .iter()
            .map(|component| self.backend.load_descriptor_layout(*component))
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let layout = match shape {
            CompoundDescriptorShape::Product => self.backend.runtime_product_layout(&layouts)?,
            CompoundDescriptorShape::Sum => self.backend.runtime_sum_layout(&layouts)?.0,
        };
        let key = CompoundDescriptorDropKey {
            shape,
            count: components
                .len()
                .try_into()
                .map_err(|_| Diagnostic::new(span.clone(), "too many descriptor components"))?,
        };
        let ordinal = self
            .view
            .artifacts()
            .find_map(|(_, artifact)| match &artifact.plan {
                Some(crate::LoweredArtifactPlan::CompoundDescriptorDrop(plan)) if *plan == key => {
                    Some(artifact.ordinal)
                }
                _ => None,
            })
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing planned compound cleanup"))?;
        let drop = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing compound cleanup entry"))?
            .as_global_value()
            .as_pointer_value();
        let mut has_drop = self.backend.context.bool_type().const_zero();
        for component in &components {
            let pointer = self.backend.descriptor_drop_pointer(*component)?;
            let present = self
                .backend
                .builder
                .build_is_not_null(pointer, "component.has.drop")
                .map_err(compiler_diagnostic)?;
            has_drop = self
                .backend
                .builder
                .build_or(has_drop, present, "compound.has.drop")
                .map_err(compiler_diagnostic)?;
        }
        let drop = self
            .backend
            .builder
            .build_select(
                has_drop,
                drop,
                self.backend
                    .context
                    .ptr_type(AddressSpace::default())
                    .const_null(),
                "compound.drop.entry",
            )
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let descriptor = self
            .backend
            .build_runtime_descriptor(layout, &components, drop)?;
        environment
            .compound_descriptors
            .push((ty.clone(), descriptor));
        Ok(descriptor)
    }

    fn dynamic_descriptor(
        &self,
        value_type: &CheckedType,
        environment: &DynamicEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        if let Some((_, descriptor)) = environment
            .compound_descriptors
            .iter()
            .find(|(ty, _)| ty == value_type)
        {
            return Ok(*descriptor);
        }
        match value_type {
            CheckedType::Parameter { id, .. } => environment
                .descriptors
                .get(id)
                .copied()
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing runtime type descriptor")),
            CheckedType::Wrapper { representation, .. } => {
                self.dynamic_descriptor(representation, environment, span)
            }
            ty if !crate::contains_type_parameter(ty) => self.static_dynamic_descriptor(ty, span),
            ty if is_static_layout_type(ty) => {
                let layout = self.backend.descriptor_layout(ty)?;
                let runtime = self.backend.static_runtime_layout(layout);
                self.backend.build_runtime_descriptor(
                    runtime,
                    &[],
                    self.backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                )
            }
            _ => Err(Diagnostic::new(
                span.clone(),
                "dynamic compound descriptors are not yet connected to emission",
            )),
        }
    }

    pub(super) fn static_dynamic_descriptor(
        &self,
        ty: &CheckedType,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let ordinal = self
            .view
            .artifacts()
            .find_map(|(_, artifact)| match &artifact.plan {
                Some(crate::LoweredArtifactPlan::TypeDescriptor(plan))
                    if plan.value_type == *ty =>
                {
                    Some(artifact.ordinal)
                }
                _ => None,
            })
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "missing planned concrete type descriptor")
            })?;
        Ok(self.descriptor_global(ordinal, span)?.as_pointer_value())
    }

    fn dynamic_sum_payload(
        &self,
        descriptor: PointerValue<'context>,
        value: PointerValue<'context>,
        count: usize,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let layouts = (0..count)
            .map(|index| {
                let component = self
                    .backend
                    .load_descriptor_component(descriptor, index as u32)?;
                self.backend.load_descriptor_layout(component)
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let (_, offset) = self.backend.runtime_sum_layout(&layouts)?;
        self.backend.dynamic_byte_offset(value, offset)
    }

    fn coerce_dynamic_value(
        &self,
        value: PointerValue<'context>,
        source: &CheckedType,
        target: &CheckedType,
        plan: &LoweredCoercionPlan,
        destination: PointerValue<'context>,
        environment: &DynamicEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        use inkwell::values::BasicValue;
        let descriptor = self.dynamic_descriptor(target, environment, span)?;
        match plan {
            LoweredCoercionPlan::Identity => {
                self.backend
                    .copy_dynamic_value(descriptor, value, destination)
            }
            LoweredCoercionPlan::SumInject {
                alternative,
                payload,
            } => {
                let CheckedType::Sum(sum) = target else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid dynamic sum injection target",
                    ));
                };
                let layout = self.backend.load_descriptor_layout(descriptor)?;
                self.backend
                    .builder
                    .build_memset(
                        destination,
                        1,
                        self.backend.context.i8_type().const_zero(),
                        layout.store_size,
                    )
                    .map_err(compiler_diagnostic)?;
                let tag = self
                    .backend
                    .builder
                    .build_store(
                        destination,
                        self.backend
                            .context
                            .i32_type()
                            .const_int(*alternative as u64, false),
                    )
                    .map_err(compiler_diagnostic)?;
                tag.set_alignment(1)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                let storage =
                    self.dynamic_sum_payload(descriptor, destination, sum.alternatives.len())?;
                self.coerce_dynamic_value(
                    value,
                    source,
                    &sum.alternatives[*alternative],
                    payload,
                    storage,
                    environment,
                    span,
                )
            }
            LoweredCoercionPlan::Wrap { payload } => {
                let CheckedType::Wrapper { representation, .. } = target else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid dynamic wrapper coercion target",
                    ));
                };
                self.coerce_dynamic_value(
                    value,
                    source,
                    representation,
                    payload,
                    destination,
                    environment,
                    span,
                )
            }
            LoweredCoercionPlan::SumWiden { arms } => {
                let (CheckedType::Sum(source_sum), CheckedType::Sum(target_sum)) = (source, target)
                else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid dynamic sum widening types",
                    ));
                };
                let source_descriptor = self.dynamic_descriptor(source, environment, span)?;
                let input = self.dynamic_sum_payload(
                    source_descriptor,
                    value,
                    source_sum.alternatives.len(),
                )?;
                let output = self.dynamic_sum_payload(
                    descriptor,
                    destination,
                    target_sum.alternatives.len(),
                )?;
                let layout = self.backend.load_descriptor_layout(descriptor)?;
                self.backend
                    .builder
                    .build_memset(
                        destination,
                        1,
                        self.backend.context.i8_type().const_zero(),
                        layout.store_size,
                    )
                    .map_err(compiler_diagnostic)?;
                let tag = self
                    .load_dynamic_storage(self.backend.context.i32_type(), value, "sum.coerce.tag")
                    .map_err(compiler_diagnostic)?;
                tag.as_instruction_value()
                    .expect("sum tag load")
                    .set_alignment(1)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                let function = self
                    .backend
                    .builder
                    .get_insert_block()
                    .and_then(|block| block.get_parent())
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "sum widening outside a function")
                    })?;
                let done = self
                    .backend
                    .context
                    .append_basic_block(function, "sum.coerce.done");
                let invalid = self
                    .backend
                    .context
                    .append_basic_block(function, "sum.coerce.invalid");
                let cases = (0..source_sum.alternatives.len())
                    .map(|index| {
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
                    .build_switch(tag.into_int_value(), invalid, &cases)
                    .map_err(compiler_diagnostic)?;
                self.backend.builder.position_at_end(invalid);
                self.backend
                    .builder
                    .build_unreachable()
                    .map_err(compiler_diagnostic)?;
                for (index, (_, block)) in cases.into_iter().enumerate() {
                    self.backend.builder.position_at_end(block);
                    let Some(arm) = arms.get(index).and_then(Option::as_ref) else {
                        self.backend
                            .builder
                            .build_unreachable()
                            .map_err(compiler_diagnostic)?;
                        continue;
                    };
                    self.coerce_dynamic_value(
                        input,
                        &source_sum.alternatives[index],
                        &target_sum.alternatives[arm.target],
                        &arm.payload,
                        output,
                        environment,
                        span,
                    )?;
                    let tag = self
                        .backend
                        .builder
                        .build_store(
                            destination,
                            self.backend
                                .context
                                .i32_type()
                                .const_int(arm.target as u64, false),
                        )
                        .map_err(compiler_diagnostic)?;
                    tag.set_alignment(1)
                        .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                    self.backend
                        .builder
                        .build_unconditional_branch(done)
                        .map_err(compiler_diagnostic)?;
                }
                self.backend.builder.position_at_end(done);
                Ok(())
            }
        }
    }

    /// Short-circuit `&&`/`||` over dynamic `Bool` storage. Both arms produce
    /// the pointer to the operand's storage; the tag lives at offset zero of
    /// the sum representation.
    fn emit_dynamic_logical(
        &mut self,
        owner: EmissionOwner,
        span: &staple_syntax::Span,
        logical: &crate::LoweredLogical,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        use inkwell::values::BasicValue;
        let left = self.emit_dynamic_expression(owner, logical.left, environment)?;
        if environment.returned {
            return Ok(left);
        }
        let tag = self
            .load_dynamic_storage(self.backend.context.i32_type(), left, "logical.tag")
            .map_err(compiler_diagnostic)?;
        tag.as_instruction_value()
            .expect("logical tag load")
            .set_alignment(1)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        let is_true = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                tag.into_int_value(),
                self.backend
                    .context
                    .i32_type()
                    .const_int(logical.true_index as u64, false),
                "logical.is_true",
            )
            .map_err(compiler_diagnostic)?;
        let function = self.current_dynamic_function(span)?;
        let merge = self
            .backend
            .context
            .append_basic_block(function, "logical.merge");
        let right_block = self
            .backend
            .context
            .append_basic_block(function, "logical.right");
        let short_circuit = self
            .backend
            .context
            .append_basic_block(function, "logical.short_circuit");
        let (true_target, false_target) = match logical.operator {
            staple_syntax::LogicalOperator::And => (right_block, short_circuit),
            staple_syntax::LogicalOperator::Or => (short_circuit, right_block),
        };
        self.backend
            .builder
            .build_conditional_branch(is_true, true_target, false_target)
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(short_circuit);
        self.backend
            .builder
            .build_unconditional_branch(merge)
            .map_err(compiler_diagnostic)?;
        let mut incoming = vec![(left, short_circuit)];
        self.backend.builder.position_at_end(right_block);
        let base = environment.snapshot();
        environment.returned = false;
        let right = self.emit_dynamic_expression(owner, logical.right, environment)?;
        if !environment.returned {
            // Bindings introduced by the right operand are dropped by its own
            // block; only the conditionally set liveness of outer symbols is
            // restored at the merge.
            environment.restore(&base);
            self.backend
                .builder
                .build_unconditional_branch(merge)
                .map_err(compiler_diagnostic)?;
            let predecessor = self
                .backend
                .builder
                .get_insert_block()
                .ok_or_else(|| Diagnostic::new(span.clone(), "logical outside a function"))?;
            incoming.push((right, predecessor));
        } else {
            environment.restore(&base);
        }
        self.backend.builder.position_at_end(merge);
        environment.returned = false;
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        Ok(self
            .backend
            .builder
            .build_phi(pointer, "logical.value")
            .map_err(compiler_diagnostic)?)
        .map(|phi| {
            let values = incoming
                .iter()
                .map(|(value, block)| (value as &dyn inkwell::values::BasicValue<'context>, *block))
                .collect::<Vec<_>>();
            phi.add_incoming(&values);
            phi.as_basic_value().into_pointer_value()
        })
    }

    /// Evaluate the subject once and emit each arm's recorded pattern test.
    /// The arm result is the pointer to its storage; all arms share the
    /// pointer type, so the merge is a pointer phi.
    fn emit_dynamic_match(
        &mut self,
        owner: EmissionOwner,
        span: &staple_syntax::Span,
        match_: &crate::LoweredMatch,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let subject_expression = self
            .view
            .expression(owner, match_.subject)
            .cloned()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing match subject"))?;
        let subject_owned = match &subject_expression.kind {
            LoweredExpressionKind::Name(name) => {
                subject_expression.moved_symbols.contains(&name.symbol)
            }
            _ => true,
        };
        let subject = self.emit_dynamic_expression(owner, match_.subject, environment)?;
        if environment.returned {
            return Ok(subject);
        }
        let descriptor = self.dynamic_descriptor(&match_.source, environment, span)?;
        let function = self.current_dynamic_function(span)?;
        let merge = self
            .backend
            .context
            .append_basic_block(function, "match.merge");
        let base = environment.snapshot();
        let mut incoming = Vec::new();
        let mut continuing = None;
        for arm in &match_.arms {
            environment.restore(&base);
            environment.returned = false;
            let arm_block = self
                .backend
                .context
                .append_basic_block(function, "match.arm");
            let failure = self
                .backend
                .context
                .append_basic_block(function, "match.next");
            self.test_dynamic_pattern(
                owner,
                arm.pattern,
                subject,
                descriptor,
                subject_owned,
                arm_block,
                failure,
                environment,
            )?;
            self.backend.builder.position_at_end(arm_block);
            environment.returned = false;
            let mark = environment.binding_order.len();
            let value = self.emit_dynamic_expression(owner, arm.body, environment)?;
            if !environment.returned {
                self.drop_dynamic_bindings(environment, mark)?;
                self.backend
                    .builder
                    .build_unconditional_branch(merge)
                    .map_err(compiler_diagnostic)?;
                let predecessor = self
                    .backend
                    .builder
                    .get_insert_block()
                    .ok_or_else(|| Diagnostic::new(span.clone(), "match outside a function"))?;
                incoming.push((value, predecessor));
                continuing = Some(environment.snapshot());
            }
            self.backend.builder.position_at_end(failure);
        }
        self.backend
            .builder
            .build_unreachable()
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(merge);
        if incoming.is_empty() {
            self.backend
                .builder
                .build_unreachable()
                .map_err(compiler_diagnostic)?;
            environment.returned = true;
            if let Some(state) = continuing {
                environment.restore(&state);
            }
            return Ok(subject);
        }
        environment.returned = false;
        if let Some(state) = &continuing {
            environment.restore(state);
        }
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let phi = self
            .backend
            .builder
            .build_phi(pointer, "match.value")
            .map_err(compiler_diagnostic)?;
        let values = incoming
            .iter()
            .map(|(value, block)| (value as &dyn inkwell::values::BasicValue<'context>, *block))
            .collect::<Vec<_>>();
        phi.add_incoming(&values);
        Ok(phi.as_basic_value().into_pointer_value())
    }

    /// Emits one match arm's pattern test as conditional branches, binding the
    /// pattern's symbols on the success path. The builder is left at whichever
    /// block continues the test.
    #[allow(clippy::too_many_arguments)]
    fn test_dynamic_pattern(
        &mut self,
        owner: EmissionOwner,
        pattern_id: PatternId,
        pointer: PointerValue<'context>,
        descriptor: PointerValue<'context>,
        owned: bool,
        success: inkwell::basic_block::BasicBlock<'context>,
        failure: inkwell::basic_block::BasicBlock<'context>,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let pattern = self
            .view
            .pattern(owner, pattern_id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing dynamic pattern")
            })?;
        let span = &pattern.origin.span;
        match &pattern.kind {
            LoweredPatternKind::At {
                binding,
                pattern: nested,
            } => {
                self.bind_dynamic_pattern(
                    owner,
                    *binding,
                    pointer,
                    descriptor,
                    owned,
                    environment,
                )?;
                self.test_dynamic_pattern(
                    owner,
                    *nested,
                    pointer,
                    descriptor,
                    owned,
                    success,
                    failure,
                    environment,
                )
            }
            LoweredPatternKind::Binding { symbol, .. } => {
                if let Some(index) = pattern.test.sum_alternative {
                    let tag = self.dynamic_sum_tag(pointer, span)?;
                    let matches = self
                        .backend
                        .builder
                        .build_int_compare(
                            inkwell::IntPredicate::EQ,
                            tag,
                            self.backend
                                .context
                                .i32_type()
                                .const_int(index as u64, false),
                            "match.tag",
                        )
                        .map_err(compiler_diagnostic)?;
                    if symbol.is_some() {
                        let function = self.current_dynamic_function(span)?;
                        let selected = self
                            .backend
                            .context
                            .append_basic_block(function, "match.selected");
                        self.backend
                            .builder
                            .build_conditional_branch(matches, selected, failure)
                            .map_err(compiler_diagnostic)?;
                        self.backend.builder.position_at_end(selected);
                        let CheckedType::Sum(sum) = &pattern.test.subject else {
                            return Err(Diagnostic::new(
                                span.clone(),
                                "dynamic sum pattern has a non-sum subject",
                            ));
                        };
                        let payload_count = sum.alternatives.len();
                        let payload =
                            self.dynamic_sum_payload(descriptor, pointer, payload_count)?;
                        let component = self
                            .backend
                            .load_descriptor_component(descriptor, index as u32)?;
                        self.bind_dynamic_pattern(
                            owner,
                            pattern_id,
                            payload,
                            component,
                            owned,
                            environment,
                        )?;
                        self.backend
                            .builder
                            .build_unconditional_branch(success)
                            .map_err(compiler_diagnostic)?;
                    } else {
                        self.backend
                            .builder
                            .build_conditional_branch(matches, success, failure)
                            .map_err(compiler_diagnostic)?;
                    }
                    Ok(())
                } else {
                    self.bind_dynamic_pattern(
                        owner,
                        pattern_id,
                        pointer,
                        descriptor,
                        owned,
                        environment,
                    )?;
                    self.backend
                        .builder
                        .build_unconditional_branch(success)
                        .map_err(compiler_diagnostic)?;
                    Ok(())
                }
            }
            LoweredPatternKind::Wildcard => {
                if owned {
                    self.backend.drop_dynamic_value(descriptor, pointer)?;
                }
                self.backend
                    .builder
                    .build_unconditional_branch(success)
                    .map_err(compiler_diagnostic)?;
                Ok(())
            }
            LoweredPatternKind::Product { elements, .. } if elements.len() == 1 => self
                .test_dynamic_pattern(
                    owner,
                    elements[0],
                    pointer,
                    descriptor,
                    owned,
                    success,
                    failure,
                    environment,
                ),
            LoweredPatternKind::Product { elements, .. } => {
                if elements.is_empty() {
                    self.backend
                        .builder
                        .build_unconditional_branch(success)
                        .map_err(compiler_diagnostic)?;
                    return Ok(());
                }
                let function = self.current_dynamic_function(span)?;
                for (index, element) in elements.iter().enumerate() {
                    let (field, component) =
                        self.dynamic_product_field(descriptor, pointer, index as u32)?;
                    let next = if index + 1 == elements.len() {
                        success
                    } else {
                        self.backend
                            .context
                            .append_basic_block(function, "match.pattern")
                    };
                    self.test_dynamic_pattern(
                        owner,
                        *element,
                        field,
                        component,
                        owned,
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
            LoweredPatternKind::Nominal { argument, .. } => match &pattern.test.subject {
                CheckedType::String | CheckedType::StringLiteralSet(_)
                    if pattern.test.identity == crate::LoweredPatternIdentity::String =>
                {
                    let payload_type = &self.view.pattern(owner, *argument).unwrap().test.subject;
                    let payload_descriptor =
                        self.dynamic_descriptor(payload_type, environment, span)?;
                    self.test_dynamic_pattern(
                        owner,
                        *argument,
                        pointer,
                        payload_descriptor,
                        owned,
                        success,
                        failure,
                        environment,
                    )
                }

                CheckedType::Ref(_) => {
                    let payload = self
                        .load_dynamic_storage(
                            self.backend.context.ptr_type(AddressSpace::default()),
                            pointer,
                            "match.payload",
                        )
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value();
                    let payload_type = self
                        .view
                        .pattern(owner, *argument)
                        .map(|argument| argument.test.subject.clone())
                        .unwrap_or(CheckedType::Error);
                    let payload_descriptor =
                        self.dynamic_descriptor(&payload_type, environment, span)?;
                    self.test_dynamic_pattern(
                        owner,
                        *argument,
                        payload,
                        payload_descriptor,
                        owned,
                        success,
                        failure,
                        environment,
                    )
                }
                CheckedType::Wrapper { .. } => self.test_dynamic_pattern(
                    owner,
                    *argument,
                    pointer,
                    self.dynamic_descriptor(
                        &self.view.pattern(owner, *argument).unwrap().test.subject,
                        environment,
                        span,
                    )?,
                    owned,
                    success,
                    failure,
                    environment,
                ),
                CheckedType::Sum(sum) => {
                    let Some(index) = pattern.test.sum_alternative else {
                        return Err(Diagnostic::new(
                            span.clone(),
                            "dynamic sum pattern does not select an alternative",
                        ));
                    };
                    let tag = self.dynamic_sum_tag(pointer, span)?;
                    let matches = self
                        .backend
                        .builder
                        .build_int_compare(
                            inkwell::IntPredicate::EQ,
                            tag,
                            self.backend
                                .context
                                .i32_type()
                                .const_int(index as u64, false),
                            "match.tag",
                        )
                        .map_err(compiler_diagnostic)?;
                    let function = self.current_dynamic_function(span)?;
                    let selected = self
                        .backend
                        .context
                        .append_basic_block(function, "match.selected");
                    self.backend
                        .builder
                        .build_conditional_branch(matches, selected, failure)
                        .map_err(compiler_diagnostic)?;
                    self.backend.builder.position_at_end(selected);
                    let payload =
                        self.dynamic_sum_payload(descriptor, pointer, sum.alternatives.len())?;
                    let payload_type = &self.view.pattern(owner, *argument).unwrap().test.subject;
                    let component = self.dynamic_descriptor(payload_type, environment, span)?;
                    self.test_dynamic_pattern(
                        owner,
                        *argument,
                        payload,
                        component,
                        owned,
                        success,
                        failure,
                        environment,
                    )
                }
                _ => Err(Diagnostic::new(
                    span.clone(),
                    "dynamic nominal pattern subject is not yet supported",
                )),
            },
            LoweredPatternKind::Literal { .. } => {
                let bytes = pattern.test.literal.as_ref().ok_or_else(|| {
                    Diagnostic::new(span.clone(), "missing string pattern literal")
                })?;
                let literal = std::str::from_utf8(bytes)
                    .map_err(|_| Diagnostic::new(span.clone(), "invalid string pattern literal"))?;
                let pointer = if let Some(index) = pattern.test.sum_alternative {
                    let CheckedType::Sum(sum) = &pattern.test.subject else {
                        return Err(Diagnostic::new(
                            span.clone(),
                            "string alternative has no sum subject",
                        ));
                    };
                    let tag = self.dynamic_sum_tag(pointer, span)?;
                    let matches = self
                        .backend
                        .builder
                        .build_int_compare(
                            inkwell::IntPredicate::EQ,
                            tag,
                            self.backend
                                .context
                                .i32_type()
                                .const_int(index as u64, false),
                            "match.string.tag",
                        )
                        .map_err(compiler_diagnostic)?;
                    let selected = self.backend.context.append_basic_block(
                        self.current_dynamic_function(span)?,
                        "match.string.selected",
                    );
                    self.backend
                        .builder
                        .build_conditional_branch(matches, selected, failure)
                        .map_err(compiler_diagnostic)?;
                    self.backend.builder.position_at_end(selected);
                    self.dynamic_sum_payload(descriptor, pointer, sum.alternatives.len())?
                } else {
                    pointer
                };
                let string = self
                    .load_dynamic_storage(
                        self.backend.compile_type(&CheckedType::String)?,
                        pointer,
                        "match.string",
                    )
                    .map_err(compiler_diagnostic)?;
                string
                    .as_instruction_value()
                    .unwrap()
                    .set_alignment(1)
                    .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
                self.backend.build_string_literal_pattern_compare(
                    string.into_struct_value(),
                    literal,
                    success,
                    failure,
                    span.clone(),
                )
            }
        }
    }

    /// Cross one `Ref` payload per recorded dereference. Every pointer-like
    /// representation stores the payload pointer in its own storage.
    fn dereference_dynamic_pointer(
        &self,
        mut pointer: PointerValue<'context>,
        dereference: &[CheckedType],
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        for _ in dereference {
            pointer = self
                .load_dynamic_storage(
                    self.backend.context.ptr_type(AddressSpace::default()),
                    pointer,
                    "access.payload",
                )
                .map_err(compiler_diagnostic)?
                .into_pointer_value();
        }
        let _ = span;
        Ok(pointer)
    }

    fn dynamic_sum_tag(
        &self,
        pointer: PointerValue<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<inkwell::values::IntValue<'context>> {
        use inkwell::values::BasicValue;
        let tag = self
            .load_dynamic_storage(self.backend.context.i32_type(), pointer, "match.tag")
            .map_err(compiler_diagnostic)?;
        tag.as_instruction_value()
            .expect("sum tag load")
            .set_alignment(1)
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?;
        Ok(tag.into_int_value())
    }

    fn current_dynamic_function(
        &self,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        self.backend
            .builder
            .get_insert_block()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "dynamic expression outside a function"))
    }

    /// Emit a loop with a preallocated entry-block result slot. `break`
    /// copies into the slot and branches to the exit; the body repeats until a
    /// break reaches the exit.
    fn emit_dynamic_loop(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        span: &staple_syntax::Span,
        loop_: &crate::LoweredLoop,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let function = self.current_dynamic_function(span)?;
        let header = self
            .backend
            .context
            .append_basic_block(function, "loop.body");
        let exit = self
            .backend
            .context
            .append_basic_block(function, "loop.exit");
        let result = environment.loop_results.get(&id).copied();
        let result_descriptor = self.dynamic_descriptor(&loop_.result_type, environment, span)?;
        self.backend
            .builder
            .build_unconditional_branch(header)
            .map_err(compiler_diagnostic)?;
        self.backend.builder.position_at_end(header);
        environment.returned = false;
        environment.loops.push(DynamicLoopContext {
            depth: loop_.depth,
            header,
            exit,
            result,
            result_descriptor,
            binding_mark: environment.binding_order.len(),
            breaks: false,
        });
        let value = self.emit_dynamic_block(owner, loop_.body, environment)?;
        if !environment.returned {
            if loop_.drops_body_result {
                if let Some(value) = value {
                    let result_type = self
                        .view
                        .block(owner, loop_.body)
                        .and_then(|block| block.result)
                        .and_then(|result| self.view.expression(owner, result))
                        .map(|expression| {
                            expression
                                .coercion
                                .as_ref()
                                .map_or(&expression.value_type, |coercion| &coercion.target)
                                .clone()
                        })
                        .ok_or_else(|| {
                            Diagnostic::new(span.clone(), "loop body result is missing")
                        })?;
                    let descriptor = self.dynamic_descriptor(&result_type, environment, span)?;
                    self.backend.drop_dynamic_value(descriptor, value)?;
                }
            }
            self.backend
                .builder
                .build_unconditional_branch(header)
                .map_err(compiler_diagnostic)?;
        }
        let context = environment
            .loops
            .pop()
            .ok_or_else(|| Diagnostic::new(span.clone(), "loop context is missing"))?;
        self.backend.builder.position_at_end(exit);
        let Some(result) = context.result else {
            return Err(Diagnostic::new(span.clone(), "loop result slot is missing"));
        };
        if !context.breaks {
            self.backend
                .builder
                .build_unreachable()
                .map_err(compiler_diagnostic)?;
            environment.returned = true;
            return Ok(result);
        }
        environment.returned = false;
        Ok(result)
    }

    /// Resolve a mutation target to its storage pointer and descriptor.
    /// Concrete MutateIndex dispatch inside a dynamic body. Its lowering
    /// binding still selects the ordinary implementation or structural body.
    fn emit_dynamic_indexed_assignment(
        &mut self,
        owner: EmissionOwner,
        id: crate::ItemId,
        assignment: &crate::LoweredAssignmentItem,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let place = self
            .view
            .place(owner, assignment.target)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing indexed place")
            })?;
        let crate::LoweredPlaceKind::Indexed { base, index } = place.kind else {
            return Err(Diagnostic::new(
                place.origin.span.clone(),
                "indexed assignment has no indexed place",
            ));
        };
        let span = &place.origin.span;
        let (base_pointer, base_descriptor) = self.emit_dynamic_place(owner, base, environment)?;
        let position = self.emit_dynamic_expression(owner, index, environment)?;
        if environment.returned {
            return Ok(());
        }
        let position_type = self
            .view
            .expression(owner, index)
            .map(|expression| &expression.value_type)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing index expression"))?;
        let position = self
            .load_dynamic_storage(
                self.backend.compile_type(position_type)?,
                position,
                "mutation.index",
            )
            .map_err(compiler_diagnostic)?;
        let replacement = self.emit_dynamic_expression(owner, assignment.value, environment)?;
        if environment.returned {
            return Ok(());
        }
        let replacement = self
            .load_dynamic_storage(
                self.backend.compile_type(&place.value_type)?,
                replacement,
                "mutation.value",
            )
            .map_err(compiler_diagnostic)?;
        let function =
            self.dynamic_bound_function(owner, LoweredBindingSite::IndexedAssignment(id), span)?;
        let null = self
            .backend
            .context
            .ptr_type(AddressSpace::default())
            .const_null();
        self.backend
            .builder
            .build_direct_call(
                function,
                &[
                    null.into(),
                    base_pointer.into(),
                    position.into(),
                    replacement.into(),
                ],
                "mutate_index.call",
            )
            .map_err(compiler_diagnostic)?;
        if assignment.drops_base_temporary {
            self.backend
                .drop_dynamic_value(base_descriptor, base_pointer)?;
        }
        Ok(())
    }

    fn emit_dynamic_place(
        &mut self,
        owner: EmissionOwner,
        place_id: PlaceId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<(PointerValue<'context>, PointerValue<'context>)> {
        let place = self
            .view
            .place(owner, place_id)
            .cloned()
            .ok_or_else(|| Diagnostic::new(staple_syntax::Span::Compiler, "missing place"))?;
        match &place.kind {
            crate::LoweredPlaceKind::Symbol { symbol }
            | crate::LoweredPlaceKind::CapturedCell { symbol } => {
                let binding = environment.bindings.get(symbol).ok_or_else(|| {
                    Diagnostic::new(place.origin.span.clone(), "missing place root")
                })?;
                Ok((binding.pointer, binding.descriptor))
            }
            crate::LoweredPlaceKind::Temporary { expression } => {
                let pointer = self.emit_dynamic_expression(owner, *expression, environment)?;
                let descriptor =
                    self.dynamic_descriptor(&place.value_type, environment, &place.origin.span)?;
                Ok((pointer, descriptor))
            }
            crate::LoweredPlaceKind::Resource { use_ } => {
                let use_record = self.view.resource_use(owner, *use_).ok_or_else(|| {
                    Diagnostic::new(place.origin.span.clone(), "missing resource place use")
                })?;
                let provider = use_record.provider.ok_or_else(|| {
                    Diagnostic::new(
                        use_record.origin.span.clone(),
                        "resource place has no selected provider",
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
                let descriptor =
                    self.dynamic_descriptor(&place.value_type, environment, &place.origin.span)?;
                Ok((bound.pointer, descriptor))
            }
            crate::LoweredPlaceKind::Dereference {
                reference,
                dereference,
            } => {
                let mut pointer = self.emit_dynamic_expression(owner, *reference, environment)?;
                for _ in dereference {
                    pointer = self
                        .load_dynamic_storage(
                            self.backend.context.ptr_type(AddressSpace::default()),
                            pointer,
                            "place.payload",
                        )
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value();
                }
                let descriptor =
                    self.dynamic_descriptor(&place.value_type, environment, &place.origin.span)?;
                Ok((pointer, descriptor))
            }
            crate::LoweredPlaceKind::ProductElement {
                base,
                index,
                slice: false,
            } => {
                let (base_pointer, base_descriptor) =
                    self.emit_dynamic_place(owner, *base, environment)?;
                let base_type = self
                    .view
                    .place(owner, *base)
                    .map(|place| place.value_type.clone())
                    .unwrap_or(CheckedType::Error);
                if let CheckedType::Wrapper { representation, .. } = &base_type
                    && !matches!(representation.as_ref(), CheckedType::Product(_))
                {
                    return Ok((base_pointer, base_descriptor));
                }
                self.dynamic_product_field(base_descriptor, base_pointer, *index as u32)
            }
            crate::LoweredPlaceKind::ProductElement {
                base,
                index,
                slice: true,
            } => {
                let (slice, _) = self.emit_dynamic_place(owner, *base, environment)?;
                let slice_type = self.backend.slice_type();
                let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
                let data = self
                    .backend
                    .builder
                    .build_struct_gep(slice_type, slice, 0, "mutation.slice.data")
                    .map_err(compiler_diagnostic)?;
                let data = self
                    .load_dynamic_storage(pointer_type, data, "mutation.slice.pointer")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let length = self
                    .backend
                    .builder
                    .build_struct_gep(slice_type, slice, 1, "mutation.slice.length")
                    .map_err(compiler_diagnostic)?;
                let length = self
                    .load_dynamic_storage(self.backend.size_type, length, "mutation.slice.count")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                let position = self.backend.size_type.const_int(*index as u64, false);
                let out_of_bounds = self
                    .backend
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::UGE,
                        position,
                        length,
                        "mutation.slice.out_of_bounds",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .build_trap_if(out_of_bounds, place.origin.span.clone())?;
                let descriptor =
                    self.dynamic_descriptor(&place.value_type, environment, &place.origin.span)?;
                let layout = self.backend.load_descriptor_layout(descriptor)?;
                let offset = self
                    .backend
                    .builder
                    .build_int_mul(position, layout.stride, "mutation.slice.offset")
                    .map_err(compiler_diagnostic)?;
                Ok((self.backend.dynamic_byte_offset(data, offset)?, descriptor))
            }
            crate::LoweredPlaceKind::Representation { base } => {
                self.emit_dynamic_place(owner, *base, environment)
            }
            crate::LoweredPlaceKind::Indexed { .. } => Err(Diagnostic::new(
                place.origin.span.clone(),
                "dynamic mutation target is not yet implemented",
            )),
        }
    }

    fn dynamic_product_field(
        &self,
        descriptor: PointerValue<'context>,
        value: PointerValue<'context>,
        index: u32,
    ) -> CodeGenerationResult<(PointerValue<'context>, PointerValue<'context>)> {
        let mut offset = self.backend.size_type.const_zero();
        for field in 0..index {
            let component = self.backend.load_descriptor_component(descriptor, field)?;
            let layout = self.backend.load_descriptor_layout(component)?;
            offset = self
                .backend
                .builder
                .build_int_add(offset, layout.store_size, "product.field.offset")
                .map_err(compiler_diagnostic)?;
        }
        Ok((
            self.backend.dynamic_byte_offset(value, offset)?,
            self.backend.load_descriptor_component(descriptor, index)?,
        ))
    }

    fn bind_dynamic_pattern(
        &self,
        owner: EmissionOwner,
        pattern: PatternId,
        pointer: PointerValue<'context>,
        descriptor: PointerValue<'context>,
        owned: bool,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<()> {
        let pattern = self.view.pattern(owner, pattern).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing dynamic pattern")
        })?;
        match &pattern.kind {
            LoweredPatternKind::Product { elements, .. } => {
                if elements.len() == 1 {
                    return self.bind_dynamic_pattern(
                        owner,
                        elements[0],
                        pointer,
                        descriptor,
                        owned,
                        environment,
                    );
                }
                for (index, pattern) in elements.iter().enumerate() {
                    let (field, component) =
                        self.dynamic_product_field(descriptor, pointer, index as u32)?;
                    self.bind_dynamic_pattern(
                        owner,
                        *pattern,
                        field,
                        component,
                        owned,
                        environment,
                    )?;
                }
                Ok(())
            }
            LoweredPatternKind::Nominal { argument, .. } => {
                if pattern.test.identity == crate::LoweredPatternIdentity::Ref {
                    let payload = self
                        .load_dynamic_storage(
                            self.backend.context.ptr_type(AddressSpace::default()),
                            pointer,
                            "pattern.payload",
                        )
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value();
                    let payload_type = self
                        .view
                        .pattern(owner, *argument)
                        .map(|argument| argument.test.subject.clone())
                        .unwrap_or(CheckedType::Error);
                    let payload_descriptor =
                        self.dynamic_descriptor(&payload_type, environment, &pattern.origin.span)?;
                    self.bind_dynamic_pattern(
                        owner,
                        *argument,
                        payload,
                        payload_descriptor,
                        owned,
                        environment,
                    )
                } else {
                    self.bind_dynamic_pattern(
                        owner,
                        *argument,
                        pointer,
                        descriptor,
                        owned,
                        environment,
                    )
                }
            }
            LoweredPatternKind::At {
                binding,
                pattern: nested,
            } => {
                self.bind_dynamic_pattern(
                    owner,
                    *binding,
                    pointer,
                    descriptor,
                    owned,
                    environment,
                )?;
                self.bind_dynamic_pattern(owner, *nested, pointer, descriptor, owned, environment)
            }
            LoweredPatternKind::Binding {
                symbol: Some(symbol),
                ..
            } => {
                let owned = self.dynamic_symbol_owned(*symbol, owned);
                environment.bindings.insert(
                    *symbol,
                    DynamicBinding {
                        pointer,
                        descriptor,
                        owned,
                        live: true,
                    },
                );
                environment.binding_order.push(*symbol);
                Ok(())
            }
            LoweredPatternKind::Binding { symbol: None, .. }
            | LoweredPatternKind::Literal { .. } => Ok(()),
            LoweredPatternKind::Wildcard if !owned => Ok(()),
            LoweredPatternKind::Wildcard => self.backend.drop_dynamic_value(descriptor, pointer),
        }
    }

    /// A pattern symbol owns its value only when the subject is owned and the
    /// symbol itself is not a borrow.
    fn dynamic_symbol_owned(&self, symbol: SymbolId, subject_owned: bool) -> bool {
        subject_owned
            && self
                .view
                .symbol(symbol)
                .is_some_and(|record| !record.non_owning && !record.mutated_parameter)
    }

    fn drop_dynamic_bindings(
        &self,
        environment: &mut DynamicEnvironment<'context>,
        mark: usize,
    ) -> CodeGenerationResult<()> {
        for symbol in environment.binding_order[mark..].iter().rev() {
            let binding = environment
                .bindings
                .get_mut(symbol)
                .expect("recorded binding");
            if binding.owned && binding.live {
                self.backend
                    .drop_dynamic_value(binding.descriptor, binding.pointer)?;
                binding.live = false;
            }
        }
        Ok(())
    }

    fn emit_dynamic_propagating_binding(
        &mut self,
        owner: EmissionOwner,
        binding: &crate::LoweredPatternBindingItem,
        value: PointerValue<'context>,
        environment: &mut DynamicEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let propagation = binding
            .propagation
            .as_ref()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing dynamic propagation plan"))?;
        let CheckedType::Sum(sum) = &propagation.source else {
            return Err(Diagnostic::new(
                span.clone(),
                "propagation source is not a sum",
            ));
        };
        let source_descriptor = self.dynamic_descriptor(&propagation.source, environment, span)?;
        let function = self.current_dynamic_function(span)?;
        let success = self
            .backend
            .context
            .append_basic_block(function, "propagate.ok");
        let failure = self
            .backend
            .context
            .append_basic_block(function, "propagate.return");
        let tag = self.dynamic_sum_tag(value, span)?;
        let selected = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                tag,
                self.backend
                    .context
                    .i32_type()
                    .const_int(propagation.success_index as u64, false),
                "propagate.success",
            )
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_conditional_branch(selected, success, failure)
            .map_err(compiler_diagnostic)?;
        let state = environment.snapshot();
        self.backend.builder.position_at_end(failure);
        if let Some(message) = &binding.panic_message {
            self.backend.build_panic(message, span)?;
        } else {
            if propagation.source == propagation.result {
                self.backend.copy_dynamic_value(
                    environment.result_descriptor,
                    value,
                    environment.result,
                )?;
            } else {
                let (source, source_type) = if let Some(index) = binding.propagation_residual {
                    (
                        self.dynamic_sum_payload(source_descriptor, value, sum.alternatives.len())?,
                        &sum.alternatives[index],
                    )
                } else {
                    (value, &propagation.source)
                };
                if let Some(plan) = &binding.propagation_plan {
                    self.coerce_dynamic_value(
                        source,
                        source_type,
                        &propagation.result,
                        plan,
                        environment.result,
                        environment,
                        span,
                    )?;
                } else {
                    self.backend.copy_dynamic_value(
                        environment.result_descriptor,
                        source,
                        environment.result,
                    )?;
                }
            }
            self.drop_dynamic_bindings(environment, 0)?;
            self.backend
                .builder
                .build_return(None)
                .map_err(compiler_diagnostic)?;
        }
        environment.restore(&state);
        self.backend.builder.position_at_end(success);
        let payload = self.dynamic_sum_payload(source_descriptor, value, sum.alternatives.len())?;
        let mut root = binding.pattern;
        loop {
            let pattern = self
                .view
                .pattern(owner, root)
                .cloned()
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing propagation pattern"))?;
            match pattern.kind {
                LoweredPatternKind::At { binding, pattern } => {
                    self.bind_dynamic_pattern(
                        owner,
                        binding,
                        value,
                        source_descriptor,
                        true,
                        environment,
                    )?;
                    root = pattern;
                }
                LoweredPatternKind::Nominal { argument, .. } => {
                    let payload_type = &self.view.pattern(owner, argument).unwrap().test.subject;
                    let descriptor = self.dynamic_descriptor(payload_type, environment, span)?;
                    return self.bind_dynamic_pattern(
                        owner,
                        argument,
                        payload,
                        descriptor,
                        true,
                        environment,
                    );
                }
                _ => {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "propagation success pattern is not nominal",
                    ));
                }
            }
        }
    }

    fn emit_dynamic_block(
        &mut self,
        owner: EmissionOwner,
        id: BlockId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<Option<PointerValue<'context>>> {
        let block = self.view.block(owner, id).ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing dynamic block")
        })?;
        let mark = environment.binding_order.len();
        for id in &block.items {
            let Some(item) = self.view.item(owner, *id) else {
                continue;
            };
            let LoweredItemKind::Binding(binding) = &item.kind else {
                continue;
            };
            if binding.predeclare_state_only != Some(true) {
                continue;
            }
            let Some(symbol) = binding.symbol else {
                continue;
            };
            if !environment.initialization_states.contains_key(&symbol) {
                let state = if self
                    .view
                    .symbol(symbol)
                    .is_some_and(|record| record.captured)
                {
                    self.backend.build_gc_allocation(
                        self.backend.size_type.const_int(1, false),
                        "dynamic.binding.state",
                        item.origin.span.clone(),
                    )?
                } else {
                    self.backend
                        .entry_builder()
                        .build_alloca(self.backend.context.i8_type(), "dynamic.binding.state")
                        .map_err(compiler_diagnostic)?
                };
                environment.initialization_states.insert(symbol, state);
            }
            self.backend
                .builder
                .build_store(
                    environment.initialization_states[&symbol],
                    self.backend.context.i8_type().const_zero(),
                )
                .map_err(compiler_diagnostic)?;
        }
        for id in &block.items {
            if environment.returned {
                break;
            }
            let item = self.view.item(owner, *id).ok_or_else(|| {
                Diagnostic::new(block.origin.span.clone(), "missing dynamic item")
            })?;
            match &item.kind {
                LoweredItemKind::Binding(binding) if binding.generic => {
                    if binding.compile_time_only {
                        continue;
                    }
                    if let Some(symbol) = binding.symbol {
                        if let Some(state) = environment.initialization_states.get(&symbol) {
                            for value in [1, 2] {
                                self.backend
                                    .builder
                                    .build_store(
                                        *state,
                                        self.backend.context.i8_type().const_int(value, false),
                                    )
                                    .map_err(compiler_diagnostic)?;
                            }
                        }
                        self.store_initialization_state(symbol, 1)?;
                        self.store_initialization_state(symbol, 2)?;
                    }
                }
                LoweredItemKind::Binding(binding) if !binding.signal && !binding.derived => {
                    if binding.compile_time_only {
                        continue;
                    }
                    let Some(value) = binding.value else {
                        return Err(Diagnostic::new(
                            item.origin.span.clone(),
                            "uninitialized dynamic binding is not yet implemented",
                        ));
                    };
                    if let Some(symbol) = binding.symbol {
                        if binding.cell
                            && self.view.symbol(symbol).is_some_and(|record| {
                                record.captured
                                    && (record.mutable_storage
                                        || record.derived
                                        || record.requires_initialization_check)
                            })
                        {
                            let cell = environment.cells[&symbol];
                            let ty = self.view.owner_symbol_type(owner, symbol).unwrap();
                            let descriptor =
                                self.dynamic_descriptor(ty, environment, &item.origin.span)?;
                            let layout = self.backend.load_descriptor_layout(descriptor)?;
                            let state =
                                self.backend.dynamic_byte_offset(cell, layout.store_size)?;
                            self.backend
                                .builder
                                .build_store(
                                    state,
                                    self.backend.context.i8_type().const_int(1, false),
                                )
                                .map_err(compiler_diagnostic)?;
                        }
                    }
                    let pointer = self.emit_dynamic_expression(owner, value, environment)?;
                    if let Some(symbol) = binding.symbol {
                        let ty = self.view.owner_symbol_type(owner, symbol).ok_or_else(|| {
                            Diagnostic::new(
                                item.origin.span.clone(),
                                "missing dynamic binding type",
                            )
                        })?;
                        let descriptor =
                            self.dynamic_descriptor(ty, environment, &item.origin.span)?;
                        let pointer = if binding.cell {
                            let cell =
                                environment.cells.get(&symbol).copied().ok_or_else(|| {
                                    Diagnostic::new(
                                        item.origin.span.clone(),
                                        "missing dynamic binding cell",
                                    )
                                })?;
                            self.backend.copy_dynamic_value(descriptor, pointer, cell)?;
                            if self.view.symbol(symbol).is_some_and(|record| {
                                record.captured
                                    && (record.mutable_storage
                                        || record.derived
                                        || record.requires_initialization_check)
                            }) {
                                let layout = self.backend.load_descriptor_layout(descriptor)?;
                                let state =
                                    self.backend.dynamic_byte_offset(cell, layout.store_size)?;
                                self.backend
                                    .builder
                                    .build_store(
                                        state,
                                        self.backend.context.i8_type().const_int(2, false),
                                    )
                                    .map_err(compiler_diagnostic)?;
                            }
                            cell
                        } else {
                            pointer
                        };
                        let owned = self.dynamic_symbol_owned(symbol, true);
                        environment.bindings.insert(
                            symbol,
                            DynamicBinding {
                                pointer,
                                descriptor,
                                owned,
                                live: true,
                            },
                        );
                        environment.binding_order.push(symbol);
                    }
                }
                LoweredItemKind::PatternBinding(binding) => {
                    let pointer =
                        self.emit_dynamic_expression(owner, binding.value, environment)?;
                    let expression =
                        self.view.expression(owner, binding.value).ok_or_else(|| {
                            Diagnostic::new(
                                item.origin.span.clone(),
                                "missing dynamic pattern initializer",
                            )
                        })?;
                    let value_type = expression
                        .coercion
                        .as_ref()
                        .map_or(&expression.value_type, |coercion| &coercion.target);
                    let descriptor =
                        self.dynamic_descriptor(value_type, environment, &item.origin.span)?;
                    if binding.propagating || binding.asserting {
                        self.emit_dynamic_propagating_binding(
                            owner,
                            binding,
                            pointer,
                            environment,
                            &item.origin.span,
                        )?;
                    } else {
                        self.bind_dynamic_pattern(
                            owner,
                            binding.pattern,
                            pointer,
                            descriptor,
                            true,
                            environment,
                        )?;
                    }
                }
                LoweredItemKind::Expression(statement) => {
                    let value =
                        self.emit_dynamic_expression(owner, statement.expression, environment)?;
                    let expression = self
                        .view
                        .expression(owner, statement.expression)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                item.origin.span.clone(),
                                "missing discarded dynamic expression",
                            )
                        })?;
                    let value_type = expression
                        .coercion
                        .as_ref()
                        .map_or(&expression.value_type, |coercion| &coercion.target);
                    if statement.drop_result || crate::contains_type_parameter(value_type) {
                        let descriptor =
                            self.dynamic_descriptor(value_type, environment, &item.origin.span)?;
                        self.backend.drop_dynamic_value(descriptor, value)?;
                    }
                }
                LoweredItemKind::Return(item) => {
                    let value = self.emit_dynamic_expression(owner, item.value, environment)?;
                    self.backend.copy_dynamic_value(
                        environment.result_descriptor,
                        value,
                        environment.result,
                    )?;
                    self.drop_dynamic_bindings(environment, 0)?;
                    self.backend
                        .builder
                        .build_return(None)
                        .map_err(compiler_diagnostic)?;
                    environment.returned = true;
                }
                LoweredItemKind::Break(break_item) => {
                    let Some((exit, result, result_descriptor, binding_mark)) = environment
                        .loops
                        .iter()
                        .rev()
                        .find(|context| context.depth == break_item.loop_depth)
                        .map(|context| {
                            (
                                context.exit,
                                context.result,
                                context.result_descriptor,
                                context.binding_mark,
                            )
                        })
                    else {
                        return Err(Diagnostic::new(
                            item.origin.span.clone(),
                            "dynamic break has no loop target",
                        ));
                    };
                    if let Some(expression) = break_item.value {
                        let value = self.emit_dynamic_expression(owner, expression, environment)?;
                        if let Some(result) = result {
                            self.backend
                                .copy_dynamic_value(result_descriptor, value, result)?;
                        }
                    }
                    self.drop_dynamic_bindings(environment, binding_mark)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(exit)
                        .map_err(compiler_diagnostic)?;
                    if let Some(context) = environment
                        .loops
                        .iter_mut()
                        .rev()
                        .find(|context| context.depth == break_item.loop_depth)
                    {
                        context.breaks = true;
                    }
                    environment.returned = true;
                }
                LoweredItemKind::Assignment(assignment) => {
                    if assignment.mutate_index.is_some() {
                        self.emit_dynamic_indexed_assignment(owner, *id, assignment, environment)?;
                        continue;
                    }
                    let (target, descriptor) =
                        self.emit_dynamic_place(owner, assignment.target, environment)?;
                    let value =
                        self.emit_dynamic_expression(owner, assignment.value, environment)?;
                    if assignment.drop_previous {
                        self.backend.drop_dynamic_value(descriptor, target)?;
                    }
                    self.backend.copy_dynamic_value(descriptor, value, target)?;
                    if let Some(symbol) = assignment.initialization_symbol
                        && let Some(binding) = environment.bindings.get_mut(&symbol)
                    {
                        binding.live = true;
                    }
                }
                LoweredItemKind::Continue(continue_item) => {
                    let Some((header, binding_mark)) = environment
                        .loops
                        .iter()
                        .rev()
                        .find(|context| context.depth == continue_item.loop_depth)
                        .map(|context| (context.header, context.binding_mark))
                    else {
                        return Err(Diagnostic::new(
                            item.origin.span.clone(),
                            "dynamic continue has no loop target",
                        ));
                    };
                    self.drop_dynamic_bindings(environment, binding_mark)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(header)
                        .map_err(compiler_diagnostic)?;
                    environment.returned = true;
                }
                _ => {
                    return Err(Diagnostic::new(
                        item.origin.span.clone(),
                        "dynamic block item is not yet implemented",
                    ));
                }
            }
        }
        if environment.returned {
            return Ok(None);
        }
        let value = block
            .result
            .map(|id| self.emit_dynamic_expression(owner, id, environment))
            .transpose()?;
        self.drop_dynamic_bindings(environment, mark)?;
        Ok(value)
    }

    /// Apply recorded coercions and moves to every expression route, including
    /// constructors and intrinsics that return early from raw emission.
    fn emit_dynamic_expression(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let expression = self.view.expression(owner, id).cloned().ok_or_else(|| {
            Diagnostic::new(staple_syntax::Span::Compiler, "missing dynamic expression")
        })?;
        let value = self.emit_raw_dynamic_expression(owner, id, environment)?;
        if environment.returned {
            return Ok(value);
        }
        let value = if let Some(coercion) = &expression.coercion {
            let plan = expression.coercion_plan.as_ref().ok_or_else(|| {
                Diagnostic::new(
                    expression.origin.span.clone(),
                    "missing dynamic coercion plan",
                )
            })?;
            if matches!(plan, LoweredCoercionPlan::Identity) {
                value
            } else {
                let target = *environment.coercions.get(&id).ok_or_else(|| {
                    Diagnostic::new(
                        expression.origin.span.clone(),
                        "missing dynamic coercion storage",
                    )
                })?;
                self.coerce_dynamic_value(
                    value,
                    &coercion.source,
                    &coercion.target,
                    plan,
                    target,
                    environment,
                    &expression.origin.span,
                )?;
                target
            }
        } else {
            value
        };
        for symbol in expression.moved_symbols {
            if let Some(binding) = environment.bindings.get_mut(&symbol) {
                binding.live = false;
            }
        }
        Ok(value)
    }

    fn emit_raw_dynamic_expression(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let expression = self
            .view
            .expression(owner, id)
            .ok_or_else(|| {
                Diagnostic::new(
                    staple_syntax::Span::Compiler,
                    format!("missing dynamic expression {}", id.index()),
                )
            })?
            .clone();
        let raw_type = expression
            .coercion
            .as_ref()
            .map_or(&expression.value_type, |coercion| &coercion.source);
        let value = match expression.kind {
            LoweredExpressionKind::Product(product) => {
                let collapsed =
                    product.fields.len() == 1 && !matches!(raw_type, CheckedType::Product(_));
                {
                    let descriptor = self.dynamic_descriptor(
                        &expression.value_type,
                        environment,
                        &expression.origin.span,
                    )?;
                    let storage = *environment.temporaries.get(&id).ok_or_else(|| {
                        Diagnostic::new(expression.origin.span.clone(), "missing product storage")
                    })?;
                    for step in product.steps {
                        let (value_id, mappings) = match step {
                            crate::LoweredProductStep::Positional { expression, slot }
                            | crate::LoweredProductStep::Designated {
                                expression, slot, ..
                            }
                            | crate::LoweredProductStep::Default {
                                expression, slot, ..
                            } => (expression, vec![(None, slot)]),
                            crate::LoweredProductStep::PositionalSpread {
                                expression,
                                mappings,
                            } => (
                                expression,
                                mappings
                                    .into_iter()
                                    .map(|mapping| (Some(mapping.source), mapping.slot))
                                    .collect(),
                            ),
                            crate::LoweredProductStep::NamedSpread {
                                expression,
                                mappings,
                            } => (
                                expression,
                                mappings
                                    .into_iter()
                                    .map(|mapping| (Some(mapping.source), mapping.slot))
                                    .collect(),
                            ),
                        };
                        let value = self.emit_dynamic_expression(owner, value_id, environment)?;
                        if environment.returned {
                            return Ok(value);
                        }
                        let source_record = self.view.expression(owner, value_id).unwrap();
                        let source_type = source_record
                            .coercion
                            .as_ref()
                            .map_or(&source_record.value_type, |coercion| &coercion.target);
                        let source_descriptor = self.dynamic_descriptor(
                            source_type,
                            environment,
                            &expression.origin.span,
                        )?;
                        for (source, index) in mappings {
                            let value = if let Some(source) = source {
                                self.dynamic_product_field(source_descriptor, value, source as u32)?
                                    .0
                            } else {
                                value
                            };
                            let (field, component) = if collapsed {
                                (storage, descriptor)
                            } else {
                                self.dynamic_product_field(descriptor, storage, index as u32)?
                            };
                            self.backend.copy_dynamic_value(component, value, field)?;
                        }
                    }
                    storage
                }
            }
            LoweredExpressionKind::RepeatedProduct(repeated) => {
                let count = match repeated.count {
                    crate::LoweredRepeatCount::Fixed(count) => count,
                    crate::LoweredRepeatCount::Symbolic(_) => {
                        return Err(Diagnostic::new(
                            expression.origin.span.clone(),
                            "runtime repeat counts are not supported",
                        ));
                    }
                };
                let value =
                    self.emit_dynamic_expression(owner, repeated.expression, environment)?;
                if repeated.collapsed {
                    value
                } else {
                    let descriptor =
                        self.dynamic_descriptor(raw_type, environment, &expression.origin.span)?;
                    let storage = *environment.temporaries.get(&id).ok_or_else(|| {
                        Diagnostic::new(
                            expression.origin.span.clone(),
                            "missing repeated product storage",
                        )
                    })?;
                    for index in 0..count {
                        let (field, component) =
                            self.dynamic_product_field(descriptor, storage, index as u32)?;
                        self.backend.copy_dynamic_value(component, value, field)?;
                    }
                    storage
                }
            }
            LoweredExpressionKind::Access(access) => {
                let base_type = self
                    .view
                    .expression(owner, access.base)
                    .expect("access base")
                    .value_type
                    .clone();
                let dereference = match &access.kind {
                    crate::LoweredAccessKind::Product { dereference, .. }
                    | crate::LoweredAccessKind::Scalar { dereference }
                    | crate::LoweredAccessKind::Representation { dereference }
                    | crate::LoweredAccessKind::Slice { dereference, .. } => dereference,
                };
                let container_type = dereference
                    .last()
                    .cloned()
                    .unwrap_or_else(|| base_type.clone());
                let descriptor =
                    self.dynamic_descriptor(&container_type, environment, &expression.origin.span)?;
                let base = self.emit_dynamic_expression(owner, access.base, environment)?;
                let base =
                    self.dereference_dynamic_pointer(base, dereference, &expression.origin.span)?;
                match &access.kind {
                    crate::LoweredAccessKind::Product { index, .. } => {
                        self.dynamic_product_field(descriptor, base, *index as u32)?
                            .0
                    }
                    crate::LoweredAccessKind::Scalar { .. }
                    | crate::LoweredAccessKind::Representation { .. } => base,
                    crate::LoweredAccessKind::Slice { index, .. } => {
                        let slice = self.backend.slice_type();
                        let data = self
                            .backend
                            .builder
                            .build_struct_gep(slice, base, 0, "slice.data")
                            .map_err(compiler_diagnostic)?;
                        let data = self
                            .load_dynamic_storage(
                                self.backend.context.ptr_type(AddressSpace::default()),
                                data,
                                "slice.pointer",
                            )
                            .map_err(compiler_diagnostic)?
                            .into_pointer_value();
                        let length = self
                            .backend
                            .builder
                            .build_struct_gep(slice, base, 1, "slice.length.slot")
                            .map_err(compiler_diagnostic)?;
                        let length = self
                            .load_dynamic_storage(self.backend.size_type, length, "slice.length")
                            .map_err(compiler_diagnostic)?
                            .into_int_value();
                        let position = self.backend.size_type.const_int(*index as u64, false);
                        let out_of_bounds = self
                            .backend
                            .builder
                            .build_int_compare(
                                inkwell::IntPredicate::UGE,
                                position,
                                length,
                                "slice.out_of_bounds",
                            )
                            .map_err(compiler_diagnostic)?;
                        self.backend
                            .build_trap_if(out_of_bounds, expression.origin.span.clone())?;
                        let element_descriptor = self.dynamic_descriptor(
                            raw_type,
                            environment,
                            &expression.origin.span,
                        )?;
                        let layout = self.backend.load_descriptor_layout(element_descriptor)?;
                        let offset = self
                            .backend
                            .builder
                            .build_int_mul(position, layout.stride, "slice.offset")
                            .map_err(compiler_diagnostic)?;
                        self.backend.dynamic_byte_offset(data, offset)?
                    }
                }
            }
            LoweredExpressionKind::TypeAscription(ascription) => {
                self.emit_dynamic_expression(owner, ascription.value, environment)?
            }
            LoweredExpressionKind::Logical(logical) => {
                self.emit_dynamic_logical(owner, &expression.origin.span, &logical, environment)?
            }
            LoweredExpressionKind::Loop(loop_) => {
                self.emit_dynamic_loop(owner, id, &expression.origin.span, &loop_, environment)?
            }
            LoweredExpressionKind::Match(match_) => {
                self.emit_dynamic_match(owner, &expression.origin.span, &match_, environment)?
            }
            LoweredExpressionKind::String(string) => {
                let storage = *environment.temporaries.get(&id).ok_or_else(|| {
                    Diagnostic::new(expression.origin.span.clone(), "missing literal storage")
                })?;
                let value = self
                    .backend
                    .build_string_literal(&string.value, expression.origin.span.clone())?;
                self.backend
                    .builder
                    .build_store(storage, value)
                    .map_err(compiler_diagnostic)?;
                storage
            }
            LoweredExpressionKind::CString(string) => {
                let storage = *environment.temporaries.get(&id).ok_or_else(|| {
                    Diagnostic::new(expression.origin.span.clone(), "missing literal storage")
                })?;
                let text =
                    std::str::from_utf8(&string.bytes[..string.bytes.len() - 1]).map_err(|_| {
                        Diagnostic::new(expression.origin.span.clone(), "invalid C string payload")
                    })?;
                let value = inkwell::values::BasicValueEnum::try_from(
                    self.backend
                        .build_owned_c_string(text, expression.origin.span.clone())?,
                )
                .map_err(|_| {
                    Diagnostic::new(expression.origin.span.clone(), "invalid C string value")
                })?;
                self.backend
                    .builder
                    .build_store(storage, value)
                    .map_err(compiler_diagnostic)?;
                storage
            }
            LoweredExpressionKind::Integer(integer) => {
                let storage = *environment.temporaries.get(&id).ok_or_else(|| {
                    Diagnostic::new(expression.origin.span.clone(), "missing literal storage")
                })?;
                let value = self
                    .backend
                    .compile_integer_type(integer.integer_type)
                    .const_int(integer.value, false);
                self.backend
                    .builder
                    .build_store(storage, value)
                    .map_err(compiler_diagnostic)?;
                storage
            }
            LoweredExpressionKind::Float(float) => {
                let storage = *environment.temporaries.get(&id).ok_or_else(|| {
                    Diagnostic::new(expression.origin.span.clone(), "missing literal storage")
                })?;
                let value = self
                    .backend
                    .compile_float_type(float.float_type)
                    .const_float(float.value);
                self.backend
                    .builder
                    .build_store(storage, value)
                    .map_err(compiler_diagnostic)?;
                storage
            }
            LoweredExpressionKind::Name(name)
                if name.singleton.is_some() && name.reactive.is_none() =>
            {
                *environment.temporaries.get(&id).ok_or_else(|| {
                    Diagnostic::new(expression.origin.span.clone(), "missing singleton storage")
                })?
            }
            LoweredExpressionKind::Name(name) if name.reactive.is_none() => environment
                .bindings
                .get(&name.symbol)
                .filter(|binding| binding.live)
                .map(|binding| binding.pointer)
                .ok_or_else(|| {
                    Diagnostic::new(
                        expression.origin.span.clone(),
                        "missing or moved dynamic binding",
                    )
                })?,
            LoweredExpressionKind::Block(block) => {
                match self.emit_dynamic_block(owner, block, environment)? {
                    Some(value) => value,
                    // A divergent block already emitted its terminator; the
                    // caller stops on `returned` before using this value.
                    None => self
                        .backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                }
            }
            LoweredExpressionKind::Call(call_id) => {
                let call = self.view.call(owner, call_id).ok_or_else(|| {
                    Diagnostic::new(expression.origin.span.clone(), "missing dynamic call")
                })?;
                for symbol in &call.initialization_checks {
                    if let Some(state) = environment.initialization_states.get(symbol) {
                        self.backend
                            .build_initialization_check(*state, call.origin.span.clone())?;
                    } else if let Some(state) = self.initialization_states.get(symbol) {
                        self.backend.build_initialization_check(
                            state.as_pointer_value(),
                            call.origin.span.clone(),
                        )?;
                    }
                }
                if let LoweredCallableTarget::Constructor { recursive, .. } = &call.target {
                    if recursive.is_none() {
                        return self.emit_dynamic_constructor_call(owner, id, call, environment);
                    }
                    if matches!(
                        recursive,
                        Some(crate::RecursiveConstruction::ManagedReference)
                    ) {
                        let argument = call
                            .arguments
                            .first()
                            .and_then(|argument| argument.expression)
                            .ok_or_else(|| {
                                Diagnostic::new(call.origin.span.clone(), "missing Ref payload")
                            })?;
                        let value = self.emit_dynamic_expression(owner, argument, environment)?;
                        let CheckedType::Ref(payload) = &call.result_type else {
                            return Err(Diagnostic::new(
                                call.origin.span.clone(),
                                "Ref constructor has non-Ref result",
                            ));
                        };
                        let descriptor =
                            self.dynamic_descriptor(payload, environment, &call.origin.span)?;
                        let descriptor =
                            self.copy_descriptor_to_gc(descriptor, &call.origin.span)?;
                        let allocation =
                            self.allocate_dynamic_cell(descriptor, true, &call.origin.span)?;
                        self.backend
                            .copy_dynamic_value(descriptor, value, allocation)?;
                        let result = *environment.temporaries.get(&id).ok_or_else(|| {
                            Diagnostic::new(call.origin.span.clone(), "missing Ref result storage")
                        })?;
                        self.backend
                            .builder
                            .build_store(result, allocation)
                            .map_err(compiler_diagnostic)?;
                        return Ok(result);
                    }
                    return Err(Diagnostic::new(
                        call.origin.span.clone(),
                        "unsupported recursive constructor",
                    ));
                }
                if let LoweredCallableTarget::Intrinsic { intrinsic, .. } = &call.target {
                    return self.emit_dynamic_intrinsic(owner, id, call, *intrinsic, environment);
                }
                if let LoweredCallableTarget::ExternalFunction { symbol } = &call.target {
                    return self.emit_dynamic_extern_call(owner, id, call, *symbol, environment);
                }
                if let LoweredCallableTarget::IndirectClosure { callee } = &call.target {
                    // `clone_call` remaps `call.callee`; the target's copy may
                    // still name the template arena.
                    let callee = call.callee.unwrap_or(*callee);
                    return self.emit_dynamic_indirect_concrete_call(
                        owner,
                        id,
                        call,
                        callee,
                        environment,
                    );
                }
                let route = match &call.target {
                    LoweredCallableTarget::DirectFunction { environment, .. } => *environment,
                    LoweredCallableTarget::TraitImplementation { .. }
                    | LoweredCallableTarget::StructuralTraitMethod { .. } => {
                        LoweredCallEnvironment::None
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            call.origin.span.clone(),
                            "dynamic call route is not yet implemented",
                        ));
                    }
                };
                let (callee, runtime) =
                    match self.view.binding(owner, LoweredBindingSite::Call(call_id)) {
                        Some(LoweredBoundTarget::Instance(callee)) => (
                            Some(*callee),
                            self.view.instance_runtime_order(*callee).ok_or_else(|| {
                                Diagnostic::new(call.origin.span.clone(), "missing callee key")
                            })?,
                        ),
                        Some(LoweredBoundTarget::Artifact(_)) => (None, Vec::new()),
                        _ => {
                            return Err(Diagnostic::new(
                                call.origin.span.clone(),
                                "missing dynamic callee binding",
                            ));
                        }
                    };
                let closure = match route {
                    LoweredCallEnvironment::None => self
                        .backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    LoweredCallEnvironment::Current => environment.closure,
                };
                let mut arguments = vec![closure.into()];
                for parameter in &runtime {
                    let ty = call
                        .substitutions
                        .types
                        .iter()
                        .find(|entry| entry.parameter == *parameter)
                        .map(|entry| &entry.value_type)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                call.origin.span.clone(),
                                "missing runtime call substitution",
                            )
                        })?;
                    arguments.push(
                        self.dynamic_descriptor(ty, environment, &call.origin.span)?
                            .into(),
                    );
                }
                let result = *environment.temporaries.get(&id).ok_or_else(|| {
                    Diagnostic::new(call.origin.span.clone(), "missing dynamic result storage")
                })?;
                if !runtime.is_empty() {
                    arguments.push(result.into());
                }
                let mut slots = vec![
                    None;
                    super::flattened_parameter_types(&call.function_type.parameter)
                        .len()
                ];
                let moved = super::super::abi::mutation_parameter_mask(
                    slots.len(),
                    &call.function_type.moves,
                );
                let mut cleanups = Vec::new();
                let mut unplaced = Vec::new();
                let mut hidden = Vec::new();
                for step in &call.steps {
                    match step {
                        LoweredCallStep::Argument {
                            argument: argument_index,
                        } => {
                            let argument = &call.arguments[*argument_index];
                            let value = self.emit_dynamic_argument(
                                owner,
                                id,
                                call,
                                *argument_index,
                                environment,
                            )?;
                            if let Some(slot) = argument.slot {
                                if argument.drops_after_call
                                    || (argument.temporary
                                        && !moved[slot]
                                        && crate::contains_type_parameter(&argument.expected))
                                {
                                    cleanups.push((
                                        self.dynamic_descriptor(
                                            &argument.expected,
                                            environment,
                                            &call.origin.span,
                                        )?,
                                        value,
                                    ));
                                }
                                slots[slot] = Some(value);
                            } else {
                                if argument.drops_after_call
                                    || (argument.temporary
                                        && !moved.iter().all(|slot| *slot)
                                        && crate::contains_type_parameter(&argument.expected))
                                {
                                    cleanups.push((
                                        self.dynamic_descriptor(
                                            &argument.expected,
                                            environment,
                                            &call.origin.span,
                                        )?,
                                        value,
                                    ));
                                }
                                unplaced.push(value);
                            }
                        }
                        LoweredCallStep::ProductElement {
                            argument,
                            slot,
                            expression,
                        } => {
                            let record = &call.arguments[*argument];
                            let value =
                                self.emit_dynamic_expression(owner, *expression, environment)?;
                            if record.drops_after_call
                                || (record.temporary
                                    && !moved[*slot]
                                    && crate::contains_type_parameter(&record.expected))
                            {
                                cleanups.push((
                                    self.dynamic_descriptor(
                                        &record.expected,
                                        environment,
                                        &call.origin.span,
                                    )?,
                                    value,
                                ));
                            }
                            slots[*slot] = Some(value);
                        }
                        LoweredCallStep::Default {
                            argument,
                            slot,
                            expression,
                            ..
                        } => {
                            let value =
                                self.emit_dynamic_expression(owner, *expression, environment)?;
                            let record = &call.arguments[*argument];
                            if record.drops_after_call
                                || (record.temporary
                                    && !moved[*slot]
                                    && crate::contains_type_parameter(&record.expected))
                            {
                                cleanups.push((
                                    self.dynamic_descriptor(
                                        &record.expected,
                                        environment,
                                        &call.origin.span,
                                    )?,
                                    value,
                                ));
                            }
                            slots[*slot] = Some(value);
                        }
                        LoweredCallStep::Resource { resource } => {
                            hidden.push(self.dynamic_resource_argument(
                                owner,
                                call,
                                *resource,
                                !runtime.is_empty(),
                                environment,
                            )?);
                        }
                        LoweredCallStep::Invoke => break,
                        _ => {
                            return Err(Diagnostic::new(
                                call.origin.span.clone(),
                                "dynamic argument step is not yet implemented",
                            ));
                        }
                    }
                }
                arguments.extend(hidden);
                if slots.iter().all(Option::is_none) && unplaced.len() == 1 {
                    if slots.len() == 1
                        && !matches!(
                            call.function_type.parameter.as_ref(),
                            CheckedType::Product(_)
                        )
                    {
                        slots[0] = Some(unplaced[0]);
                    } else {
                        let descriptor = self.dynamic_descriptor(
                            &call.function_type.parameter,
                            environment,
                            &call.origin.span,
                        )?;
                        for (index, slot) in slots.iter_mut().enumerate() {
                            *slot = Some(
                                self.dynamic_product_field(descriptor, unplaced[0], index as u32)?
                                    .0,
                            );
                        }
                    }
                }
                let mut writebacks = Vec::new();
                if runtime.is_empty() {
                    if let Some(conversions) = self.view.conversions(owner) {
                        for ((site, argument), conversion) in conversions {
                            let Some(slot) = argument else {
                                continue;
                            };
                            if *site != call_id {
                                continue;
                            }
                            let source =
                                slots.get(*slot).and_then(|slot| *slot).ok_or_else(|| {
                                    Diagnostic::new(
                                        call.origin.span.clone(),
                                        "missing converted dynamic argument",
                                    )
                                })?;
                            let destination = self
                                .backend
                                .entry_builder()
                                .build_alloca(
                                    self.backend.compile_type(&conversion.concrete)?,
                                    "conversion.argument",
                                )
                                .map_err(compiler_diagnostic)?;
                            let function = self.reabstraction_thunk_code(
                                owner,
                                call_id,
                                Some(*slot),
                                &call.origin.span,
                            )?;
                            self.backend
                                .builder
                                .build_direct_call(
                                    function,
                                    &[destination.into(), source.into()],
                                    "",
                                )
                                .map_err(compiler_diagnostic)?;
                            if conversion.writeback {
                                let function = self.reabstraction_writeback_code(
                                    owner,
                                    call_id,
                                    *slot,
                                    &call.origin.span,
                                )?;
                                writebacks.push((function, source, destination));
                            }
                            slots[*slot] = Some(destination);
                        }
                    }
                }
                let signature = callee
                    .and_then(|callee| self.view.instance_signature(callee))
                    .unwrap_or(&call.function_type);
                let pack = !matches!(signature.parameter.as_ref(), CheckedType::Product(_))
                    && matches!(
                        call.function_type.parameter.as_ref(),
                        CheckedType::Product(_)
                    );
                if pack {
                    let storage = *environment.packed_arguments.get(&id).ok_or_else(|| {
                        Diagnostic::new(call.origin.span.clone(), "missing packed argument storage")
                    })?;
                    let descriptor = self.dynamic_descriptor(
                        &call.function_type.parameter,
                        environment,
                        &call.origin.span,
                    )?;
                    for (index, slot) in slots.iter().enumerate() {
                        let value = slot.ok_or_else(|| {
                            Diagnostic::new(
                                call.origin.span.clone(),
                                "missing packed argument field",
                            )
                        })?;
                        let (field, component) =
                            self.dynamic_product_field(descriptor, storage, index as u32)?;
                        self.backend.copy_dynamic_value(component, value, field)?;
                    }
                    slots = vec![Some(storage)];
                }
                let parameter_types =
                    super::flattened_parameter_types(&call.function_type.parameter);
                let indirect = self.backend.indirect_parameter_mask(&call.function_type);
                for (index, slot) in slots.into_iter().enumerate() {
                    let pointer = slot.ok_or_else(|| {
                        Diagnostic::new(call.origin.span.clone(), "missing dynamic argument")
                    })?;
                    if !runtime.is_empty() || indirect[index] {
                        arguments.push(pointer.into());
                    } else {
                        let ty = self.backend.compile_type(parameter_types[index])?;
                        let value = self
                            .load_dynamic_storage(ty, pointer, "concrete.argument")
                            .map_err(compiler_diagnostic)?;
                        arguments.push(value.into());
                    }
                }
                let function = self.dynamic_bound_function(
                    owner,
                    LoweredBindingSite::Call(call_id),
                    &call.origin.span,
                )?;
                let call_value = self
                    .backend
                    .builder
                    .build_direct_call(
                        function,
                        &arguments,
                        if runtime.is_empty() {
                            "concrete.call"
                        } else {
                            ""
                        },
                    )
                    .map_err(compiler_diagnostic)?;
                for (function, destination, source) in writebacks {
                    self.backend
                        .builder
                        .build_direct_call(function, &[destination.into(), source.into()], "")
                        .map_err(compiler_diagnostic)?;
                }
                if runtime.is_empty() {
                    let value = call_value.try_as_basic_value().basic().ok_or_else(|| {
                        Diagnostic::new(
                            call.origin.span.clone(),
                            "concrete callee has no return value",
                        )
                    })?;
                    let value = if self
                        .view
                        .conversions(owner)
                        .is_some_and(|conversions| conversions.contains_key(&(call_id, None)))
                    {
                        self.convert_boundary_value(value, owner, call_id, None, &call.origin.span)?
                    } else {
                        value
                    };
                    self.backend
                        .builder
                        .build_store(result, value)
                        .map_err(compiler_diagnostic)?;
                }
                for (descriptor, pointer) in cleanups.into_iter().rev() {
                    self.backend.drop_dynamic_value(descriptor, pointer)?;
                }
                result
            }
            // Explicit arms per unsupported kind: a new lowered expression
            // kind must decide its dynamic behavior here and in the support
            // scanner instead of falling into a catch-all (R-F).
            LoweredExpressionKind::Name(_) => {
                return Err(Diagnostic::new(
                    expression.origin.span.clone(),
                    "dynamic reactive name reads are not yet implemented",
                ));
            }
            LoweredExpressionKind::Deferred(_) => {
                return Err(Diagnostic::new(
                    expression.origin.span.clone(),
                    "deferred dynamic expression is not yet implemented",
                ));
            }
            LoweredExpressionKind::StringTemplate(template) => {
                return self.emit_dynamic_string_template(owner, id, &template, environment);
            }
            LoweredExpressionKind::Index(ref index) => {
                self.emit_dynamic_index(owner, id, &expression, index, environment)?
            }
            LoweredExpressionKind::CallableValue(value_id) => {
                self.emit_dynamic_callable_value(owner, id, value_id, environment)?
            }
            LoweredExpressionKind::Resource(use_id) => {
                return self.emit_dynamic_resource_read(owner, id, use_id, environment);
            }
            LoweredExpressionKind::With(with_id) => {
                return self.emit_dynamic_with(owner, with_id, environment);
            }
            LoweredExpressionKind::Coro(_) => {
                return Err(Diagnostic::new(
                    expression.origin.span.clone(),
                    "dynamic coroutine creation is not yet implemented",
                ));
            }
            LoweredExpressionKind::Await(_) => {
                return Err(Diagnostic::new(
                    expression.origin.span.clone(),
                    "dynamic awaits are not yet implemented",
                ));
            }
        };
        Ok(value)
    }
}

/// Whether a type's representation is the same whatever its type arguments.
/// Pointer-like values keep their static layout inside a dynamic body, so
/// their descriptor comes from the concrete layout rules rather than from
/// component descriptors.
fn is_static_layout_type(value_type: &CheckedType) -> bool {
    matches!(
        value_type,
        CheckedType::Ref(_)
            | CheckedType::Slice(_)
            | CheckedType::Buffer(_)
            | CheckedType::CPointer { .. }
            | CheckedType::COpaquePointer
            | CheckedType::Function(_)
    )
}
