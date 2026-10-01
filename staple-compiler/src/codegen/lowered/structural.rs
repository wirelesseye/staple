//! Structural artifact bodies read only the closed, validated plans.
use super::*;
use crate::{DebugDelegate, DebugStep, PlannedCallee, StructuralBody, StructuralMethodPlan};

impl<'program, 'context> LoweredEmitter<'program, 'context> {
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
        self.backend
            .build_debug_delegate(function, value, formatter, name)
    }

    pub(super) fn emit_structural_body(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &StructuralMethodPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        // Until the remaining bodies land, fail before creating any blocks.
        if !matches!(
            plan.body,
            StructuralBody::ProductDebug { .. } | StructuralBody::SumDebug { .. }
        ) {
            return Err(Diagnostic::new(
                span.clone(),
                "lowered emitter: structural method artifact is not implemented yet",
            ));
        }
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
                value_as_basic(self.backend.unit_value()).unwrap()
            }
            StructuralBody::SumDebug { alternatives } => {
                let [BasicValueEnum::StructValue(value), formatter] = values.as_slice() else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "invalid structural Debug arguments",
                    ));
                };
                let CheckedType::Sum(sum) = &plan.arguments[0] else {
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
                value_as_basic(self.backend.unit_value()).unwrap()
            }
            StructuralBody::Unexpanded
            | StructuralBody::IndexSwitch { .. }
            | StructuralBody::IndexLoad { .. }
            | StructuralBody::MutateReplace { .. }
            | StructuralBody::DerefIndexLoad { .. }
            | StructuralBody::DerefDelegate { .. }
            | StructuralBody::IntoIterator { .. }
            | StructuralBody::Next { .. } => unreachable!("guarded above"),
        };
        self.backend
            .builder
            .build_return(Some(&result))
            .map_err(compiler_diagnostic)?;
        Ok(())
    }
}
