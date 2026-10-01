//! Structural artifact bodies read only the closed, validated plans.
use super::*;
use crate::{DebugDelegate, DebugStep, PlannedCallee, StructuralBody, StructuralMethodPlan};

/// The structural method's first recorded argument type (its target).
fn structural_argument<'a>(
    plan: &'a StructuralMethodPlan,
    span: &staple_syntax::Span,
) -> CodeGenerationResult<&'a CheckedType> {
    plan.arguments
        .first()
        .ok_or_else(|| Diagnostic::new(span.clone(), "structural plan has no target argument"))
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    fn structural_unit(
        &self,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        value_as_basic(self.backend.unit_value())
            .ok_or_else(|| Diagnostic::new(span.clone(), "invalid unit value"))
    }

    fn planned_function(
        &self,
        callee: &PlannedCallee,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let function = match callee {
            PlannedCallee::Instance(planned) => planned
                .instance
                .and_then(|id| self.instances.get(&id).copied()),
            PlannedCallee::Artifact(planned) => planned
                .artifact
                .and_then(|id| self.artifacts.get(&id))
                .and_then(|functions| functions.first())
                .copied(),
        };
        function.ok_or_else(|| Diagnostic::new(span.clone(), "missing planned structural callee"))
    }

    fn emit_debug_delegate(
        &self,
        delegate: &DebugDelegate,
        value: BasicValueEnum<'context>,
        formatter: BasicValueEnum<'context>,
        name: &str,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let function = self.planned_function(&delegate.callee, span)?;
        if function.get_type()
            != self
                .backend
                .compile_closure_function_type(&delegate.callee_type)?
        {
            return Err(Diagnostic::new(
                span.clone(),
                "planned Debug delegate ABI disagrees with its declaration",
            ));
        }
        self.backend
            .build_debug_delegate(function, value, formatter, name)
    }

    /// Inject through the alternative's recorded coercion plan; the emitter
    /// never plans the injection itself.
    fn emit_structural_alternative(
        &mut self,
        value: BasicValueEnum<'context>,
        alternative: &crate::SumAlternative,
        result: &CheckedType,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        value_as_basic(self.emit_coercion(
            value.as_any_value_enum(),
            &alternative.alternative,
            result,
            &alternative.coercion_plan,
            span,
        )?)
        .ok_or_else(|| Diagnostic::new(span.clone(), "iterator step is not first-class"))
    }

    pub(super) fn emit_structural_body(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &StructuralMethodPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let function = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing structural declaration"))?;
        let entry = self.backend.context.append_basic_block(function, "entry");
        self.backend.builder.position_at_end(entry);
        let values = self
            .backend
            .load_structural_parameters(function, &plan.callable_type)?;
        let result = match &plan.body {
            StructuralBody::ProductDebug { steps, write } => {
                let [BasicValueEnum::StructValue(value), formatter] = values.as_slice() else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural Debug arguments",
                    ));
                };
                let write = write
                    .instance
                    .and_then(|id| self.instances.get(&id))
                    .copied()
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "missing Formatter.write declaration")
                    })?;
                for step in steps {
                    match step {
                        DebugStep::Write(literal) => self.backend.build_formatter_write_literal(
                            write,
                            *formatter,
                            literal,
                            span.clone(),
                        )?,
                        DebugStep::Element { index, delegate } => {
                            let field = self
                                .backend
                                .builder
                                .build_extract_value(*value, *index as u32, "debug.element")
                                .map_err(compiler_diagnostic)?;
                            self.emit_debug_delegate(
                                delegate,
                                field,
                                *formatter,
                                "debug.fmt",
                                span,
                            )?;
                        }
                    }
                }
                self.structural_unit(span)?
            }
            StructuralBody::SumDebug { alternatives } => {
                let [BasicValueEnum::StructValue(value), formatter] = values.as_slice() else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural Debug arguments",
                    ));
                };
                let Some(CheckedType::Sum(sum)) = plan.arguments.first() else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural Debug sum",
                    ));
                };
                let (merge, cases) =
                    self.backend
                        .begin_debug_sum(*value, alternatives.len(), span.clone())?;
                for (index, delegate) in alternatives.iter().enumerate() {
                    self.backend.builder.position_at_end(cases[index]);
                    let payload =
                        self.backend
                            .extract_sum_alternative(*value, sum, index, span.clone())?;
                    self.emit_debug_delegate(delegate, payload, *formatter, "debug.sum.fmt", span)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(merge)
                        .map_err(compiler_diagnostic)?;
                }
                self.backend.builder.position_at_end(merge);
                self.structural_unit(span)?
            }
            StructuralBody::IndexSwitch { elements, output } => {
                let [
                    BasicValueEnum::StructValue(value),
                    BasicValueEnum::IntValue(position),
                ] = values.as_slice()
                else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural Index arguments",
                    ));
                };
                let crate::codegen::ir::StructuralIndexBlocks {
                    output_type,
                    slot,
                    merge,
                    cases,
                } = self.backend.begin_structural_index(
                    *position,
                    elements.len(),
                    output,
                    span.clone(),
                )?;
                for element in elements {
                    self.backend.builder.position_at_end(cases[element.index].1);
                    let field = self
                        .backend
                        .builder
                        .build_extract_value(*value, element.index as u32, "index.field")
                        .map_err(compiler_diagnostic)?;
                    let field = self.emit_coercion(
                        field.as_any_value_enum(),
                        &element.element,
                        output,
                        &element.coercion_plan,
                        span,
                    )?;
                    self.backend
                        .builder
                        .build_store(
                            slot,
                            value_as_basic(field).ok_or_else(|| {
                                Diagnostic::new(span.clone(), "indexed field is not first-class")
                            })?,
                        )
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(merge)
                        .map_err(compiler_diagnostic)?;
                }
                self.backend.builder.position_at_end(merge);
                self.backend
                    .builder
                    .build_load(output_type, slot, "index.value")
                    .map_err(compiler_diagnostic)?
            }
            StructuralBody::IndexLoad { length, output, .. } => {
                let [value, BasicValueEnum::IntValue(position)] = values.as_slice() else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural Index arguments",
                    ));
                };
                let pointer = self
                    .backend
                    .builder
                    .build_alloca(
                        self.backend
                            .compile_type(structural_argument(plan, span)?)?,
                        "index.product",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(pointer, *value)
                    .map_err(compiler_diagnostic)?;
                self.backend.build_index_load(
                    pointer,
                    *position,
                    self.backend.size_type.const_int(*length as u64, false),
                    output,
                    span.clone(),
                )?
            }
            StructuralBody::DerefIndexLoad { length, output, .. } => {
                let [
                    BasicValueEnum::PointerValue(pointer),
                    BasicValueEnum::IntValue(position),
                ] = values.as_slice()
                else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural DerefIndex arguments",
                    ));
                };
                self.backend.build_index_load(
                    *pointer,
                    *position,
                    self.backend.size_type.const_int(*length as u64, false),
                    output,
                    span.clone(),
                )?
            }
            StructuralBody::MutateReplace {
                element,
                length,
                drop_previous,
            } => {
                let [
                    BasicValueEnum::PointerValue(pointer),
                    BasicValueEnum::IntValue(position),
                    replacement,
                ] = values.as_slice()
                else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural MutateIndex arguments",
                    ));
                };
                let element_type = self.backend.compile_type(element)?;
                let slot = self.backend.build_index_pointer(
                    *pointer,
                    *position,
                    self.backend.size_type.const_int(*length as u64, false),
                    element_type,
                    span.clone(),
                )?;
                if let Some(glue) = drop_previous {
                    let old = self
                        .backend
                        .builder
                        .build_load(element_type, slot, "index.old")
                        .map_err(compiler_diagnostic)?;
                    self.emit_drop_glue(old, self.planned_drop_glue(glue, span)?, span)?;
                }
                self.backend
                    .builder
                    .build_store(slot, *replacement)
                    .map_err(compiler_diagnostic)?;
                self.structural_unit(span)?
            }
            StructuralBody::DerefDelegate { payload, delegate } => {
                let mut values = values;
                let Some(BasicValueEnum::PointerValue(target)) = values.first().copied() else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural dereference arguments",
                    ));
                };
                let (pointer, name) = match plan.structural {
                    crate::StructuralTraitMethod::DerefIndex => {
                        let pointer = target;
                        values[0] = self
                            .backend
                            .builder
                            .build_load(self.backend.compile_type(payload)?, pointer, "deref.value")
                            .map_err(compiler_diagnostic)?;
                        (None, "index.deref")
                    }
                    crate::StructuralTraitMethod::DerefMutateIndex => {
                        let reference = self
                            .backend
                            .builder
                            .build_load(
                                self.backend
                                    .compile_type(structural_argument(plan, span)?)?,
                                target,
                                "mutation.target",
                            )
                            .map_err(compiler_diagnostic)?;
                        (Some(reference), "mutate_index.deref")
                    }
                    other @ (crate::StructuralTraitMethod::Debug
                    | crate::StructuralTraitMethod::Index
                    | crate::StructuralTraitMethod::MutateIndex
                    | crate::StructuralTraitMethod::IntoIterator
                    | crate::StructuralTraitMethod::Iterator) => {
                        return Err(Diagnostic::new(
                            span.clone(),
                            format!("invalid dereference plan {other:?}"),
                        ));
                    }
                };
                if let Some(pointer) = pointer {
                    values[0] = pointer;
                }
                let arguments = self.backend.build_trait_arguments(
                    &delegate.callee_type,
                    &values,
                    span.clone(),
                )?;
                let function = self.planned_function(&delegate.callee, span)?;
                self.backend
                    .builder
                    .build_direct_call(function, &arguments, name)
                    .map_err(compiler_diagnostic)?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "trait method result is not first-class")
                    })?
            }
            StructuralBody::IntoIterator { .. } => self
                .backend
                .build_structural_iterator(&values, span.clone())?,
            StructuralBody::Next {
                elements,
                iterator: _,
                item,
                result,
                done,
                yield_,
                ..
            } => {
                let [
                    product @ BasicValueEnum::StructValue(product_value),
                    BasicValueEnum::IntValue(cursor),
                ] = values.as_slice()
                else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural Iterator arguments",
                    ));
                };
                let crate::codegen::ir::StructuralNextBlocks {
                    result_type,
                    slot,
                    done: done_block,
                    dispatch,
                    unreachable,
                    merge,
                } = self
                    .backend
                    .begin_structural_next(*cursor, elements.len(), result)?;
                self.backend.builder.position_at_end(done_block);
                let iter_value = self
                    .backend
                    .build_product_value(&[*product, (*cursor).into()], span.clone())?;
                let done_value =
                    self.emit_structural_alternative(iter_value, done, result, span)?;
                self.backend
                    .builder
                    .build_store(slot, done_value)
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_unconditional_branch(merge)
                    .map_err(compiler_diagnostic)?;
                self.backend.builder.position_at_end(dispatch);
                let cases =
                    self.backend
                        .begin_next_dispatch(*cursor, elements.len(), unreachable)?;
                for element in elements {
                    self.backend.builder.position_at_end(cases[element.index].1);
                    let field = self
                        .backend
                        .builder
                        .build_extract_value(*product_value, element.index as u32, "next.field")
                        .map_err(compiler_diagnostic)?;
                    let field = self.emit_coercion(
                        field.as_any_value_enum(),
                        &element.element,
                        item,
                        &element.coercion_plan,
                        span,
                    )?;
                    let field = value_as_basic(field).ok_or_else(|| {
                        Diagnostic::new(span.clone(), "iterated field is not first-class")
                    })?;
                    let next_index = self
                        .backend
                        .size_type
                        .const_int((element.index + 1) as u64, false);
                    let next_iter = self
                        .backend
                        .build_product_value(&[*product, next_index.into()], span.clone())?;
                    let yielded = self
                        .backend
                        .build_product_value(&[field, next_iter], span.clone())?;
                    let yielded =
                        self.emit_structural_alternative(yielded, yield_, result, span)?;
                    self.backend
                        .builder
                        .build_store(slot, yielded)
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_unconditional_branch(merge)
                        .map_err(compiler_diagnostic)?;
                }
                self.backend.builder.position_at_end(merge);
                self.backend
                    .builder
                    .build_load(result_type, slot, "next.value")
                    .map_err(compiler_diagnostic)?
            }
            StructuralBody::Unexpanded => {
                return Err(Diagnostic::new(
                    span.clone(),
                    "structural artifact plan is unexpanded",
                ));
            }
        };
        self.backend
            .builder
            .build_return(Some(&result))
            .map_err(compiler_diagnostic)?;
        Ok(())
    }
}
