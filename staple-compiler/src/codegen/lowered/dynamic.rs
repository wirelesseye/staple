//! Descriptor ABI emission. Unsupported dynamic constructs fail explicitly;
//! they cannot accidentally fall through to the concrete SSA emitter.
use std::collections::HashMap;

use inkwell::{
    AddressSpace,
    values::{FunctionValue, PointerValue},
};

use super::{CodeGenerationResult, Diagnostic, LoweredEmitter, compiler_diagnostic};
use crate::lower::EmissionOwner;
use crate::{
    BlockId, CheckedType, ExpressionId, FunctionInstanceId, LoweredBindingSite, LoweredBoundTarget,
    LoweredCallEnvironment, LoweredCallStep, LoweredCallableTarget, LoweredCoercionPlan,
    LoweredExpressionKind, LoweredInstanceBody, LoweredItemKind, LoweredPatternKind, PatternId,
    SymbolId, TypeParameterId,
};

#[derive(Clone, Copy)]
struct DynamicBinding<'context> {
    pointer: PointerValue<'context>,
    descriptor: PointerValue<'context>,
    owned: bool,
    live: bool,
}

struct DynamicEnvironment<'context> {
    descriptors: HashMap<TypeParameterId, PointerValue<'context>>,
    bindings: HashMap<SymbolId, DynamicBinding<'context>>,
    binding_order: Vec<SymbolId>,
    temporaries: HashMap<ExpressionId, PointerValue<'context>>,
    closure: PointerValue<'context>,
    result: PointerValue<'context>,
    result_descriptor: PointerValue<'context>,
    returned: bool,
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
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
        if !body.captures.is_empty() || !body.signature.effects.resources.is_empty() {
            return Err(unsupported("captures or resources"));
        }
        if body
            .body_coercion_plan
            .as_ref()
            .is_some_and(|plan| !matches!(plan, LoweredCoercionPlan::Identity))
        {
            return Err(unsupported("body coercion"));
        }
        let runtime = self
            .view
            .instance_runtime_parameters(id)
            .ok_or_else(|| unsupported("missing key"))?;
        let parameters = function.get_params();
        let descriptors = runtime
            .iter()
            .enumerate()
            .map(|(index, parameter)| (*parameter, parameters[index + 1].into_pointer_value()))
            .collect::<HashMap<_, _>>();
        let mut environment = DynamicEnvironment {
            descriptors,
            bindings: HashMap::new(),
            binding_order: Vec::new(),
            temporaries: HashMap::new(),
            closure: parameters[0].into_pointer_value(),
            result: parameters[1 + runtime.len()].into_pointer_value(),
            result_descriptor: self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null(),
            returned: false,
        };
        environment.result_descriptor =
            self.dynamic_descriptor(&body.signature.result, &environment, &body.origin.span)?;
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        // Reserve call results before any body control flow. Descriptor-derived
        // sizes dominate every alloca and each slot is allocated once per call.
        for (expression, record) in body.expressions() {
            if matches!(
                record.kind,
                LoweredExpressionKind::Call(_)
                    | LoweredExpressionKind::Integer(_)
                    | LoweredExpressionKind::Float(_)
            ) {
                let descriptor =
                    self.dynamic_descriptor(&record.value_type, &environment, &record.origin.span)?;
                environment
                    .temporaries
                    .insert(expression, self.backend.allocate_dynamic_value(descriptor)?);
            }
        }
        let owner = EmissionOwner::Instance(id);
        let raw = &parameters[2 + runtime.len()..];
        if raw.len() != 1 {
            return Err(unsupported("flattened product parameters"));
        }
        let descriptor =
            self.dynamic_descriptor(&body.signature.parameter, &environment, &body.origin.span)?;
        let owned = body
            .signature
            .moves
            .contains(&crate::CheckedMutation::Whole);
        self.bind_dynamic_pattern(
            owner,
            body.parameter_pattern,
            raw[0].into_pointer_value(),
            descriptor,
            owned,
            &mut environment,
        )?;
        let value = self.emit_dynamic_block(owner, root, &mut environment)?;
        for symbol in &body.body_moved_symbols {
            if let Some(binding) = environment.bindings.get_mut(symbol) {
                binding.live = false;
            }
        }
        if !environment.returned {
            let value = value.ok_or_else(|| unsupported("empty result"))?;
            self.backend.copy_dynamic_value(
                environment.result_descriptor,
                value,
                environment.result,
            )?;
            self.drop_dynamic_bindings(&mut environment, 0)?;
            self.backend
                .builder
                .build_return(None)
                .map_err(compiler_diagnostic)?;
        }
        Ok(())
    }

    fn dynamic_descriptor(
        &self,
        value_type: &CheckedType,
        environment: &DynamicEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        match value_type {
            CheckedType::Parameter { id, .. } => environment
                .descriptors
                .get(id)
                .copied()
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing runtime type descriptor")),
            ty if !crate::contains_type_parameter(ty) => self.static_dynamic_descriptor(ty, span),
            _ => Err(Diagnostic::new(
                span.clone(),
                "dynamic compound descriptors are not yet connected to emission",
            )),
        }
    }

    fn static_dynamic_descriptor(
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
        match pattern.kind {
            LoweredPatternKind::Binding {
                symbol: Some(symbol),
                ..
            } => {
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
                Ok(())
            }
            LoweredPatternKind::Wildcard if !owned => Ok(()),
            LoweredPatternKind::Wildcard => self.backend.drop_dynamic_value(descriptor, pointer),
            _ => Err(Diagnostic::new(
                pattern.origin.span.clone(),
                "dynamic destructuring is not yet implemented",
            )),
        }
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
            if environment.returned {
                break;
            }
            let item = self.view.item(owner, *id).ok_or_else(|| {
                Diagnostic::new(block.origin.span.clone(), "missing dynamic item")
            })?;
            match &item.kind {
                LoweredItemKind::Binding(binding)
                    if !binding.cell && !binding.signal && !binding.derived && !binding.generic =>
                {
                    if binding.compile_time_only {
                        continue;
                    }
                    let Some(value) = binding.value else {
                        return Err(Diagnostic::new(
                            item.origin.span.clone(),
                            "uninitialized dynamic binding is not yet implemented",
                        ));
                    };
                    let pointer = self.emit_dynamic_expression(owner, value, environment)?;
                    if let Some(symbol) = binding.symbol {
                        let ty = &self
                            .view
                            .expression(owner, value)
                            .expect("emitted expression")
                            .value_type;
                        let descriptor =
                            self.dynamic_descriptor(ty, environment, &item.origin.span)?;
                        environment.bindings.insert(
                            symbol,
                            DynamicBinding {
                                pointer,
                                descriptor,
                                owned: true,
                                live: true,
                            },
                        );
                        environment.binding_order.push(symbol);
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

    fn emit_dynamic_expression(
        &mut self,
        owner: EmissionOwner,
        id: ExpressionId,
        environment: &mut DynamicEnvironment<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let expression = self
            .view
            .expression(owner, id)
            .ok_or_else(|| {
                Diagnostic::new(staple_syntax::Span::Compiler, "missing dynamic expression")
            })?
            .clone();
        if expression
            .coercion_plan
            .as_ref()
            .is_some_and(|plan| !matches!(plan, LoweredCoercionPlan::Identity))
        {
            return Err(Diagnostic::new(
                expression.origin.span,
                "dynamic coercion is not yet implemented",
            ));
        }
        let value = match expression.kind {
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
            LoweredExpressionKind::Name(name) if !name.mutable && name.reactive.is_none() => {
                environment
                    .bindings
                    .get(&name.symbol)
                    .filter(|binding| binding.live)
                    .map(|binding| binding.pointer)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            expression.origin.span.clone(),
                            "missing or moved dynamic binding",
                        )
                    })?
            }
            LoweredExpressionKind::Block(block) => self
                .emit_dynamic_block(owner, block, environment)?
                .ok_or_else(|| {
                    Diagnostic::new(
                        expression.origin.span.clone(),
                        "divergent dynamic expression is not yet implemented",
                    )
                })?,
            LoweredExpressionKind::Call(call_id) => {
                let call = self.view.call(owner, call_id).ok_or_else(|| {
                    Diagnostic::new(expression.origin.span.clone(), "missing dynamic call")
                })?;
                let LoweredCallableTarget::DirectFunction {
                    environment: route, ..
                } = call.target
                else {
                    return Err(Diagnostic::new(
                        call.origin.span.clone(),
                        "dynamic call route is not yet implemented",
                    ));
                };
                let Some(LoweredBoundTarget::Instance(callee)) =
                    self.view.binding(owner, LoweredBindingSite::Call(call_id))
                else {
                    return Err(Diagnostic::new(
                        call.origin.span.clone(),
                        "missing dynamic callee binding",
                    ));
                };
                let runtime = self
                    .view
                    .instance_runtime_parameters(*callee)
                    .ok_or_else(|| {
                        Diagnostic::new(call.origin.span.clone(), "missing callee key")
                    })?;
                if !call.resource_bindings.is_empty() {
                    return Err(Diagnostic::new(
                        call.origin.span.clone(),
                        "resource calls in dynamic bodies are not yet implemented",
                    ));
                }
                let closure = match route {
                    LoweredCallEnvironment::None => self
                        .backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .const_null(),
                    LoweredCallEnvironment::Current => environment.closure,
                };
                let mut arguments = vec![closure.into()];
                for parameter in runtime {
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
                for step in &call.steps {
                    match step {
                        LoweredCallStep::Argument { argument } => {
                            let argument = &call.arguments[*argument];
                            let value = argument.expression.ok_or_else(|| {
                                Diagnostic::new(
                                    call.origin.span.clone(),
                                    "dynamic thunk argument is not yet implemented",
                                )
                            })?;
                            let value = self.emit_dynamic_expression(owner, value, environment)?;
                            let slot = argument
                                .slot
                                .or_else(|| (slots.len() == 1).then_some(0))
                                .ok_or_else(|| {
                                    Diagnostic::new(
                                        call.origin.span.clone(),
                                        "dynamic product argument is not yet implemented",
                                    )
                                })?;
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
                        }
                        LoweredCallStep::Invoke => {}
                        _ => {
                            return Err(Diagnostic::new(
                                call.origin.span.clone(),
                                "dynamic argument step is not yet implemented",
                            ));
                        }
                    }
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
                            .backend
                            .builder
                            .build_load(ty, pointer, "concrete.argument")
                            .map_err(compiler_diagnostic)?;
                        arguments.push(value.into());
                    }
                }
                let function = self.instances.get(callee).copied().ok_or_else(|| {
                    Diagnostic::new(call.origin.span.clone(), "missing dynamic declaration")
                })?;
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
                if runtime.is_empty() {
                    let value = call_value.try_as_basic_value().basic().ok_or_else(|| {
                        Diagnostic::new(
                            call.origin.span.clone(),
                            "concrete callee has no return value",
                        )
                    })?;
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
            _ => {
                return Err(Diagnostic::new(
                    expression.origin.span.clone(),
                    "dynamic expression is not yet implemented",
                ));
            }
        };
        for symbol in expression.moved_symbols {
            if let Some(binding) = environment.bindings.get_mut(&symbol) {
                binding.live = false;
            }
        }
        Ok(value)
    }
}
