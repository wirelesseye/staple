//! Coroutine emission from concrete frame and await plans.
//!
//! Uses recorded frame order, resume states, resource slots, cancellation cleanup,
//! and bound coroutine pair names. The emitter translates these facts to LLVM
//! control flow and runtime calls without reclassifying source awaits.

use super::*;
use crate::codegen::ir::{CoroutineResumeEntry, ExternalAwaitKind};
use crate::codegen::layout::{
    COMPLETION_CANCEL_ENV, COMPLETION_CANCEL_FN, COMPLETION_FLAG_CANCEL_ARMED, COMPLETION_FLAGS,
    COMPLETION_SCHEDULER, COMPLETION_STATE_COMPLETED, COMPLETION_VALUE, CORO_CAPTURE_ENV,
    CORO_CHILD, CORO_HEADER_FIELDS, CORO_PARENT, CORO_RECORD, CORO_RESOURCES, CORO_RESULT_PTR,
    CORO_STATE, TASK_RECORD_FRAME, TASK_RECORD_RESULT, TASK_RECORD_SCHEDULER,
};
use crate::{
    CoroutineCodesPlan, CoroutineFramePlan, LoweredAwaitId, LoweredAwaitKind, LoweredCoroId,
};
use inkwell::types::StructType;

#[derive(Clone)]
pub(super) struct CoroutineContext<'context> {
    pub frame: PointerValue<'context>,
    pub frame_type: StructType<'context>,
    pub status_type: StructType<'context>,
    pub dispatch: Vec<BasicBlock<'context>>,
    pub pending_field: u32,
    /// Every frame binding symbol, including bindings inside
    /// nested thunks. A move out of one clears its frame cell state so the
    /// completion and cancel drops skip it.
    pub frame_bindings: Vec<SymbolId>,
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    fn coroutine_frame_type(
        &self,
        frame: &CoroutineFramePlan,
    ) -> CodeGenerationResult<crate::codegen::ir::CoroutineFrameType<'context>> {
        let cells = frame
            .frame_bindings
            .iter()
            .map(|binding| {
                // The plan's recorded concrete type, not an owner-local
                // lookup: a binding inside a nested thunk belongs to that
                // thunk's instance, not the coroutine body's.
                let value = self.backend.compile_type(&binding.value_type)?;
                let mut fields = vec![value, self.backend.context.i8_type().into()];
                if self
                    .view
                    .symbol(binding.symbol)
                    .is_some_and(|record| record.signal || record.derived)
                {
                    fields.push(
                        self.backend
                            .context
                            .ptr_type(AddressSpace::default())
                            .into(),
                    );
                }
                Ok(self.backend.context.struct_type(&fields, false).into())
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        let result = self.backend.compile_type(&frame.result_type)?;
        let pending = if frame.resume_points > 0 {
            let mut bytes = 1;
            for result in &frame.await_result_types {
                bytes = bytes.max(
                    self.backend
                        .target_data
                        .get_store_size(&self.backend.compile_type(result)?),
                );
            }
            Some(bytes as u32)
        } else {
            None
        };
        Ok(self
            .backend
            .build_coroutine_frame_type(&cells, result, pending))
    }

    fn coroutine_bundle_type(
        &self,
        frame: &CoroutineFramePlan,
    ) -> CodeGenerationResult<StructType<'context>> {
        let fields = frame
            .resources
            .iter()
            .map(|slot| {
                if slot.indirect {
                    Ok(self
                        .backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .into())
                } else {
                    self.backend.compile_type(&slot.resource.value_type)
                }
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        Ok(self.backend.context.struct_type(&fields, false))
    }

    pub(super) fn emit_coroutine_pair(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &CoroutineCodesPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let frame_plan = plan
            .frame
            .as_ref()
            .ok_or_else(|| Diagnostic::new(span.clone(), "unexpanded coroutine frame plan"))?;
        let body = self
            .view
            .instance(plan.body)
            .and_then(|instance| instance.body.as_ref())
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing coroutine body instance"))?;
        let root = body
            .root
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing coroutine root block"))?;
        let owner = EmissionOwner::Instance(plan.body);
        let functions = self
            .artifacts
            .get(&ordinal)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing coroutine pair declaration"))?;
        let [resume, cleanup] = functions.as_slice() else {
            return Err(Diagnostic::new(
                span.clone(),
                "coroutine pair must declare two functions",
            ));
        };
        let (resume, cleanup) = (*resume, *cleanup);
        let layout = self.coroutine_frame_type(frame_plan)?;
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let header = self.backend.coroutine_header_type();
        let status = self.backend.coroutine_status_type();
        let finalizer = frame_plan
            .capture_finalizer
            .as_ref()
            .map(|planned| {
                planned
                    .artifact
                    .and_then(|ordinal| self.artifacts.get(&ordinal))
                    .and_then(|functions| functions.first())
                    .copied()
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "missing coroutine capture finalizer")
                    })
            })
            .transpose()?;
        let previous = self.backend.builder.get_insert_block();
        let CoroutineResumeEntry {
            frame,
            bad_state,
            dispatch,
        } = self
            .backend
            .begin_coroutine_resume(resume, frame_plan.resume_points)?;
        let mut environment = FunctionEnvironment::default();
        if !frame_plan.resources.is_empty() {
            if body.function_providers.len() != frame_plan.resources.len() {
                return Err(Diagnostic::new(
                    span.clone(),
                    "coroutine resource providers disagree with frame slots",
                ));
            }
            let slot = self
                .backend
                .builder
                .build_struct_gep(header, frame, CORO_RESOURCES, "coro.resources.slot")
                .map_err(compiler_diagnostic)?;
            let bundle = self
                .backend
                .builder
                .build_load(pointer, slot, "coro.resources")
                .map_err(compiler_diagnostic)?
                .into_pointer_value();
            let bundle_type = self.coroutine_bundle_type(frame_plan)?;
            for (index, resource) in frame_plan.resources.iter().enumerate() {
                let field = self
                    .backend
                    .builder
                    .build_struct_gep(bundle_type, bundle, index as u32, "coro.resource")
                    .map_err(compiler_diagnostic)?;
                let value = if resource.indirect {
                    self.backend
                        .builder
                        .build_load(pointer, field, "coro.resource.ptr")
                        .map_err(compiler_diagnostic)?
                        .into_pointer_value()
                } else {
                    field
                };
                environment.resources.insert(
                    body.function_providers[index],
                    BoundResource {
                        resource: resource.resource.clone(),
                        value: value.as_any_value_enum(),
                        indirect: true,
                    },
                );
            }
        }
        let env_slot = self
            .backend
            .builder
            .build_struct_gep(header, frame, CORO_CAPTURE_ENV, "coro.env.slot")
            .map_err(compiler_diagnostic)?;
        let env_ptr = self
            .backend
            .builder
            .build_load(pointer, env_slot, "coro.env")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        environment.closure_environment = Some(env_ptr);
        self.bind_instance_captures(body, env_ptr, &mut environment)?;
        for (index, binding) in frame_plan.frame_bindings.iter().enumerate() {
            let cell = self
                .backend
                .builder
                .build_struct_gep(
                    layout.ty,
                    frame,
                    CORO_HEADER_FIELDS + index as u32,
                    "coro.cell",
                )
                .map_err(compiler_diagnostic)?;
            environment.binding_cells.insert(binding.symbol, cell);
        }
        let state_slot = self
            .backend
            .builder
            .build_struct_gep(header, frame, CORO_STATE, "coro.state.slot")
            .map_err(compiler_diagnostic)?;
        let result_slot = self
            .backend
            .builder
            .build_struct_gep(header, frame, CORO_RESULT_PTR, "coro.result.ptr.slot")
            .map_err(compiler_diagnostic)?;
        environment.coroutine = Some(CoroutineContext {
            frame,
            frame_type: layout.ty,
            status_type: status,
            dispatch: dispatch.clone(),
            pending_field: layout.pending_field,
            frame_bindings: frame_plan
                .frame_bindings
                .iter()
                .map(|binding| binding.symbol)
                .collect(),
        });
        let state = self.backend.build_coroutine_resume_dispatch(
            resume,
            frame,
            state_slot,
            status,
            &dispatch,
            bad_state,
            frame_plan.resume_points,
        )?;
        self.backend.build_coroutine_cancel_teardown(
            resume,
            frame,
            state,
            &frame_plan.wait_await_states,
            &frame_plan.until_await_states,
            env_ptr,
            finalizer,
        )?;
        for (index, binding) in frame_plan.frame_bindings.iter().enumerate() {
            if let Some(glue) = &binding.unwind_drop {
                let ordinal = glue.artifact.ok_or_else(|| {
                    Diagnostic::new(span.clone(), "unbound coroutine unwind drop")
                })?;
                let plan = self.drop_glue_plan(ordinal, span)?;
                let cell = self
                    .backend
                    .builder
                    .build_struct_gep(
                        layout.ty,
                        frame,
                        CORO_HEADER_FIELDS + index as u32,
                        "coro.cancel.cell",
                    )
                    .map_err(compiler_diagnostic)?;
                let llvm_type = self.backend.compile_type(&binding.value_type)?;
                let blocks =
                    self.backend
                        .begin_conditional_cell_drop(cell, llvm_type, span.clone())?;
                self.emit_drop_glue(blocks.value, plan, span)?;
                self.backend.end_conditional_cell_drop(&blocks)?;
            }
        }
        self.backend.build_coroutine_cancelled(frame, status)?;
        self.backend.build_coroutine_bad_state(bad_state)?;
        self.backend.builder.position_at_end(dispatch[0]);
        let result = self.emit_instance_root(owner, body, root, &mut environment)?;
        if !environment.returned {
            let result = value_as_basic(result).ok_or_else(|| {
                Diagnostic::new(span.clone(), "coroutine result is not first-class")
            })?;
            self.drop_all_owned(&environment, span)?;
            // A coroutine that completes normally drops its live frame
            // bindings, in plan order, before the result is published. The
            // recorded `unwind_drop` glue is the same type drop the cancel
            // unwind uses; the cell state skips a moved-out or
            // never-initialized binding, and the unwind and completion paths
            // are mutually exclusive.
            self.emit_frame_binding_drops(frame, layout.ty, frame_plan, span)?;
            self.backend
                .build_coroutine_complete(result, result_slot, state_slot, status)?;
        }
        self.backend.build_coroutine_cleanup(cleanup, finalizer)?;
        if let Some(previous) = previous {
            self.backend.builder.position_at_end(previous);
        }
        Ok(())
    }

    /// One conditional cell drop per droppable frame binding, in plan order.
    /// The cancel unwind and the normal completion path share the recorded
    /// `unwind_drop` glue (the drop is type-based, so a separate completion
    /// plan field would be redundant); the cell state makes the drop
    /// conditional, so a moved-out or never-initialized binding is skipped.
    fn emit_frame_binding_drops(
        &mut self,
        frame: PointerValue<'context>,
        frame_type: StructType<'context>,
        frame_plan: &CoroutineFramePlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        for (index, binding) in frame_plan.frame_bindings.iter().enumerate() {
            let Some(glue) = &binding.unwind_drop else {
                continue;
            };
            let ordinal = glue
                .artifact
                .ok_or_else(|| Diagnostic::new(span.clone(), "unbound coroutine frame drop"))?;
            let plan = self.drop_glue_plan(ordinal, span)?;
            let cell = self
                .backend
                .builder
                .build_struct_gep(
                    frame_type,
                    frame,
                    CORO_HEADER_FIELDS + index as u32,
                    "coro.complete.cell",
                )
                .map_err(compiler_diagnostic)?;
            let llvm_type = self.backend.compile_type(&binding.value_type)?;
            let blocks = self
                .backend
                .begin_conditional_cell_drop(cell, llvm_type, span.clone())?;
            self.emit_drop_glue(blocks.value, plan, span)?;
            self.backend.end_conditional_cell_drop(&blocks)?;
        }
        Ok(())
    }

    pub(super) fn emit_coro(
        &mut self,
        owner: EmissionOwner,
        id: LoweredCoroId,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let coro = self
            .view
            .coro(owner, id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing coroutine creation record"))?;
        let use_ = self
            .view
            .artifact_uses(owner)
            .and_then(|uses| {
                uses.iter()
                    .find(|use_| use_.site == crate::ArtifactUseSite::CoroCreation(id))
            })
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing coroutine creation use"))?;
        let Some(LoweredArtifactPlan::CoroutineCodes(plan)) = self
            .view
            .artifact(use_.artifact)
            .and_then(|artifact| artifact.plan.as_ref())
        else {
            return Err(Diagnostic::new(
                span.clone(),
                "coroutine creation use does not name a pair",
            ));
        };
        let body = self
            .view
            .instance(plan.body)
            .and_then(|instance| instance.body.as_ref())
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing coroutine capture body"))?;
        let functions = self
            .artifacts
            .get(&use_.artifact)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing coroutine creation pair"))?;
        let [resume, cleanup] = functions.as_slice() else {
            return Err(Diagnostic::new(
                span.clone(),
                "coroutine creation requires a pair",
            ));
        };
        let (resume, cleanup) = (*resume, *cleanup);
        let env_ptr = match coro.environment {
            LoweredClosureEnvironment::Fresh => {
                self.build_capture_environment_value(owner, body, environment, span)?
            }
            LoweredClosureEnvironment::None => self
                .backend
                .context
                .ptr_type(AddressSpace::default())
                .const_null(),
            LoweredClosureEnvironment::Current => {
                environment.closure_environment.ok_or_else(|| {
                    Diagnostic::new(span.clone(), "missing enclosing coroutine environment")
                })?
            }
            LoweredClosureEnvironment::Stored => {
                return Err(Diagnostic::new(
                    span.clone(),
                    "coroutine creation cannot use a stored closure",
                ));
            }
        };
        let frame_plan = plan.frame.as_ref().ok_or_else(|| {
            Diagnostic::new(span.clone(), "missing coroutine creation frame plan")
        })?;
        let layout = self.coroutine_frame_type(frame_plan)?;
        self.backend
            .build_coroutine_creation(
                layout.ty,
                layout.frame_size,
                layout.result_field,
                resume,
                cleanup,
                env_ptr,
                span.clone(),
            )
            .map(|frame| frame.as_any_value_enum())
    }

    /// One `await` suspension inside a coroutine body.
    /// A child coroutine parks `RESUME_CHILD` after stashing the child frame
    /// and its deferred-resource bundle; on resume it loads the pending
    /// result. An external `Task`/`Wait` handle parks `WAIT_EXTERNAL` and
    /// returns the `Completed T | Cancelled` outcome sum.
    pub(super) fn emit_await(
        &mut self,
        owner: EmissionOwner,
        id: LoweredAwaitId,
        environment: &mut FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let await_ = self
            .view
            .await_record(owner, id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing lowered await record"))?
            .clone();
        let context = environment
            .coroutine
            .clone()
            .ok_or_else(|| Diagnostic::new(span.clone(), "`await` outside a coroutine body"))?;
        let dispatch = context
            .dispatch
            .get(await_.resume_state)
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "await resume state has no dispatch block")
            })?;
        match &await_.kind {
            LoweredAwaitKind::ChildCoroutine {
                child_result,
                deferred_resources,
                ..
            } => {
                let child = self.emit_expression(owner, await_.operand, environment)?;
                let child = value_as_basic(child).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "`await` operand is not a coroutine")
                })?;
                let child = pointer_operand(child, "`await` coroutine operand", span)?;
                self.store_coroutine_resources(
                    owner,
                    deferred_resources,
                    child,
                    environment,
                    span,
                )?;
                let frame = context.frame;
                self.backend.build_coroutine_child_suspend(
                    frame,
                    context.frame_type,
                    context.pending_field,
                    await_.resume_state,
                    child,
                    context.status_type,
                )?;
                self.backend.builder.position_at_end(dispatch);
                let result_llvm = self.backend.compile_type(child_result)?;
                let value = self.backend.build_coroutine_pending_result(
                    frame,
                    context.frame_type,
                    context.pending_field,
                    result_llvm,
                )?;
                Ok(value.as_any_value_enum())
            }
            LoweredAwaitKind::Task { result } | LoweredAwaitKind::Wait { result } => {
                self.emit_external_await(owner, &await_, result, context, environment, span)
            }
        }
    }

    /// Register the frame as the record's waiter, park, and on resume read
    /// `Completed payload | Cancelled` from the record's state.
    fn emit_external_await(
        &mut self,
        owner: EmissionOwner,
        await_: &crate::LoweredAwait,
        result: &CheckedType,
        context: CoroutineContext<'context>,
        environment: &mut FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let ptr_type = self.backend.context.ptr_type(AddressSpace::default());
        let i8_type = self.backend.context.i8_type();
        let header_type = self.backend.coroutine_header_type();
        let frame = context.frame;
        let CheckedType::Sum(outcome_sum) = &await_.result_type else {
            return Err(Diagnostic::new(
                span.clone(),
                "`await` result is not a `Completed | Cancelled` sum",
            ));
        };
        let [completed_type, cancelled_type, ..] = outcome_sum.alternatives.as_slice() else {
            return Err(Diagnostic::new(
                span.clone(),
                "`await` outcome sum has fewer than two alternatives",
            ));
        };
        let outcome_plans = await_.outcome.as_ref().ok_or_else(|| {
            Diagnostic::new(span.clone(), "external await has no recorded outcome plans")
        })?;
        let payload_llvm = self.backend.compile_type(result)?;
        let (record_type, result_field, kind) = match &await_.kind {
            LoweredAwaitKind::Task { .. } => (
                self.backend.task_record_type(payload_llvm),
                TASK_RECORD_RESULT,
                ExternalAwaitKind::Task,
            ),
            LoweredAwaitKind::Wait { .. } => (
                self.backend.completion_record_type(payload_llvm),
                COMPLETION_VALUE,
                ExternalAwaitKind::Wait,
            ),
            LoweredAwaitKind::ChildCoroutine { .. } => {
                return Err(Diagnostic::new(
                    span.clone(),
                    "child coroutine await reached external await emission",
                ));
            }
        };
        let dispatch = context
            .dispatch
            .get(await_.resume_state)
            .copied()
            .ok_or_else(|| {
                Diagnostic::new(span.clone(), "await resume state has no dispatch block")
            })?;
        let function = context
            .dispatch
            .first()
            .and_then(|block| block.get_parent())
            .ok_or_else(|| Diagnostic::new(span.clone(), "await dispatch has no function"))?;
        let record = self.emit_expression(owner, await_.operand, environment)?;
        let record = value_as_basic(record)
            .ok_or_else(|| Diagnostic::new(span.clone(), "`await` operand is not a wait handle"))?;
        let record = pointer_operand(record, "`await` handle operand", span)?;
        self.backend.build_external_await_suspend(
            function,
            frame,
            record,
            await_.resume_state,
            dispatch,
            context.status_type,
            kind,
        )?;
        let record_slot = self
            .backend
            .builder
            .build_struct_gep(header_type, frame, CORO_CHILD, "coro.child.slot")
            .map_err(compiler_diagnostic)?;
        let record = self
            .backend
            .builder
            .build_load(ptr_type, record_slot, "external.record")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let record_state = self
            .backend
            .builder
            .build_load(i8_type, record, "external.record.state")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let is_completed = self
            .backend
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                record_state,
                i8_type.const_int(COMPLETION_STATE_COMPLETED, false),
                "external.completed",
            )
            .map_err(compiler_diagnostic)?;

        let completed_block = self
            .backend
            .context
            .append_basic_block(function, "await.ext.completed");
        let cancelled_block = self
            .backend
            .context
            .append_basic_block(function, "await.ext.cancelled");
        let merge_block = self
            .backend
            .context
            .append_basic_block(function, "await.ext.merge");
        self.backend
            .builder
            .build_conditional_branch(is_completed, completed_block, cancelled_block)
            .map_err(compiler_diagnostic)?;

        let outcome_type = &await_.result_type;
        let outcome_llvm = self.backend.compile_type(outcome_type)?;
        // Lowering records both injections (`LoweredAwait::outcome`).
        let completed_plan = &outcome_plans.completed;
        let cancelled_plan = &outcome_plans.cancelled;

        self.backend.builder.position_at_end(completed_block);
        let result_slot = self
            .backend
            .builder
            .build_struct_gep(record_type, record, result_field, "external.record.result")
            .map_err(compiler_diagnostic)?;
        let payload = self
            .backend
            .builder
            .build_load(payload_llvm, result_slot, "external.result")
            .map_err(compiler_diagnostic)?;
        let completed_value = self.emit_coercion(
            payload.as_any_value_enum(),
            completed_type,
            outcome_type,
            completed_plan,
            span,
        )?;
        let completed_value = value_as_basic(completed_value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "`Completed` value is not first-class"))?;
        let completed_end = self
            .backend
            .builder
            .get_insert_block()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing completed block"))?;
        self.backend
            .builder
            .build_unconditional_branch(merge_block)
            .map_err(compiler_diagnostic)?;

        self.backend.builder.position_at_end(cancelled_block);
        let cancelled_value = self.emit_coercion(
            self.backend.unit_value(),
            cancelled_type,
            outcome_type,
            cancelled_plan,
            span,
        )?;
        let cancelled_value = value_as_basic(cancelled_value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "`Cancelled` value is not first-class"))?;
        let cancelled_end = self
            .backend
            .builder
            .get_insert_block()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing cancelled block"))?;
        self.backend
            .builder
            .build_unconditional_branch(merge_block)
            .map_err(compiler_diagnostic)?;

        self.backend.builder.position_at_end(merge_block);
        let outcome = self
            .backend
            .builder
            .build_phi(outcome_llvm, "await.ext.outcome")
            .map_err(compiler_diagnostic)?;
        outcome.add_incoming(&[
            (&completed_value, completed_end),
            (&cancelled_value, cancelled_end),
        ]);
        Ok(outcome.as_basic_value().as_any_value_enum())
    }

    /// Pack the recorded deferred
    /// resources into a fresh GC bundle and store its pointer in
    /// `frame->resources`. Each use's recorded pass mode decides whether the
    /// bundle slot is the borrowed pointer or the value.
    pub(super) fn store_coroutine_resources(
        &mut self,
        owner: EmissionOwner,
        uses: &[crate::LoweredResourceUseId],
        frame: PointerValue<'context>,
        environment: &mut FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        if uses.is_empty() {
            return Ok(());
        }
        let pointer = self.backend.context.ptr_type(AddressSpace::default());
        let mut fields = Vec::with_capacity(uses.len());
        let mut arguments = Vec::with_capacity(uses.len());
        for use_id in uses {
            let record = self
                .view
                .resource_use(owner, *use_id)
                .ok_or_else(|| Diagnostic::new(span.clone(), "missing coroutine resource use"))?;
            let value = self.bound_resource_value(environment, record)?;
            if record.pass_mode == crate::LoweredArgumentPassMode::Value {
                fields.push(self.backend.compile_type(&record.resource.value_type)?);
            } else {
                fields.push(pointer.into());
            }
            arguments.push(
                value_as_basic(value)
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "coroutine resource is not first-class")
                    })?
                    .into(),
            );
        }
        let bundle_type = self.backend.context.struct_type(&fields, false);
        self.backend
            .build_coroutine_resource_bundle(frame, bundle_type, arguments, span.clone())
    }

    /// `block_on`'s synchronous drive (compile coroutine drive).
    /// The call's recorded activation names the concrete result type and the
    /// deferred-resource slots in its own `resource_bindings`.
    pub(super) fn emit_coroutine_drive(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        frame: PointerValue<'context>,
        environment: &mut FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let uses = self.activation_resource_uses(call, span)?;
        self.store_coroutine_resources(owner, &uses, frame, environment, span)?;
        let activation = call.runtime.coroutine.as_ref().ok_or_else(|| {
            Diagnostic::new(
                span.clone(),
                "`block_on` has no recorded coroutine activation",
            )
        })?;
        let result_llvm = self.backend.compile_type(&activation.result_type)?;
        self.backend
            .build_coroutine_drive(frame, result_llvm, span.clone())
            .map(|value| value.as_any_value_enum())
    }

    /// The recorded activation's deferred-resource uses, in row order.
    fn activation_resource_uses(
        &self,
        call: &crate::LoweredCall,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Vec<crate::LoweredResourceUseId>> {
        let activation = call.runtime.coroutine.as_ref().ok_or_else(|| {
            Diagnostic::new(span.clone(), "coroutine activation is missing its record")
        })?;
        activation
            .deferred_resources
            .iter()
            .map(|index| {
                call.resource_bindings.get(*index).copied().ok_or_else(|| {
                    Diagnostic::new(
                        span.clone(),
                        "coroutine activation names a missing resource binding",
                    )
                })
            })
            .collect()
    }

    /// The `Tasks` scope pointer the
    /// call's recorded binding names. `spawn` is the only reader, and
    /// records the index into the call's own `resource_bindings`.
    fn current_task_scope(
        &self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let activation = call.runtime.coroutine.as_ref().ok_or_else(|| {
            Diagnostic::new(span.clone(), "`spawn` is missing its recorded activation")
        })?;
        let index = activation.tasks_resource.ok_or_else(|| {
            Diagnostic::new(span.clone(), "no `Tasks` scope is in scope for `spawn`")
        })?;
        let use_id = call.resource_bindings.get(index).copied().ok_or_else(|| {
            Diagnostic::new(
                span.clone(),
                "`spawn` is missing its `Tasks` resource binding",
            )
        })?;
        let record = self
            .view
            .resource_use(owner, use_id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing `Tasks` resource use"))?;
        let provider = record.provider.ok_or_else(|| {
            Diagnostic::new(span.clone(), "`Tasks` resource has no selected provider")
        })?;
        let bound = environment
            .resources
            .get(&provider)
            .ok_or_else(|| Diagnostic::new(span.clone(), "resource `Tasks` is not available"))?;
        let pointer = value_as_basic(bound.value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "`Tasks` resource is not first-class"))?;
        let pointer = pointer_operand(pointer, "`Tasks` resource", span)?;
        if bound.indirect {
            let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
            self.backend
                .builder
                .build_load(pointer_type, pointer, "tasks.scope")
                .map_err(compiler_diagnostic)
                .map(|value| value.into_pointer_value())
        } else {
            Ok(pointer)
        }
    }

    /// Load `%TaskScope.scheduler`.
    fn current_scheduler(
        &self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let scope = self.current_task_scope(owner, call, environment, span)?;
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        // `%TaskScope { ptr scheduler, ptr tasks_head }`.
        let scope_type = self
            .backend
            .context
            .struct_type(&[pointer_type.into(), pointer_type.into()], false);
        let scheduler_slot = self
            .backend
            .builder
            .build_struct_gep(scope_type, scope, 0, "tasks.scheduler.slot")
            .map_err(compiler_diagnostic)?;
        self.backend
            .builder
            .build_load(pointer_type, scheduler_slot, "tasks.scheduler")
            .map_err(compiler_diagnostic)
            .map(|value| value.into_pointer_value())
    }

    /// Compile scheduler intrinsic.
    pub(super) fn emit_scheduler_intrinsic(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        intrinsic: IntrinsicFunction,
        arguments: &[BasicMetadataValueEnum<'context>],
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = call.origin.span.clone();
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        let i8_type = self.backend.context.i8_type();
        match intrinsic {
            IntrinsicFunction::SchedulerCreate => {
                let create = self.backend.declare_named_function(
                    "__staple_sched_create",
                    pointer_type.fn_type(&[], false),
                );
                let scheduler = self
                    .backend
                    .build_runtime_call(create, &[], "scheduler")?
                    .try_as_basic_value()
                    .unwrap_basic();
                Ok(scheduler.as_any_value_enum())
            }
            IntrinsicFunction::TaskScope => {
                let [BasicMetadataValueEnum::PointerValue(scheduler)] = arguments else {
                    return Err(Diagnostic::new(span, "scheduler is not first-class"));
                };
                let open = self.backend.declare_named_function(
                    "__staple_task_scope_open",
                    pointer_type.fn_type(&[pointer_type.into()], false),
                );
                let scope = self
                    .backend
                    .build_runtime_call(open, &[(*scheduler).into()], "task.scope")?
                    .try_as_basic_value()
                    .unwrap_basic();
                Ok(scope.as_any_value_enum())
            }
            IntrinsicFunction::YieldNow => Ok(self
                .backend
                .build_yield_coroutine(span)?
                .as_any_value_enum()),
            IntrinsicFunction::Spawn => {
                let result_type = call
                    .runtime
                    .coroutine
                    .as_ref()
                    .map(|activation| activation.result_type.clone())
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "`spawn` is missing its recorded activation")
                    })?;
                let [BasicMetadataValueEnum::PointerValue(frame)] = arguments else {
                    return Err(Diagnostic::new(span, "coroutine is not first-class"));
                };
                let header_type = self.backend.coroutine_header_type();
                let result_llvm = self.backend.compile_type(&result_type)?;
                let record_type = self.backend.task_record_type(result_llvm);
                let record = self.backend.build_gc_allocation(
                    self.backend
                        .size_type
                        .const_int(self.backend.target_data.get_store_size(&record_type), false),
                    "task.record",
                    span.clone(),
                )?;
                self.backend
                    .builder
                    .build_store(record, record_type.const_zero())
                    .map_err(compiler_diagnostic)?;
                let record_result = self
                    .backend
                    .builder
                    .build_struct_gep(
                        record_type,
                        record,
                        TASK_RECORD_RESULT,
                        "task.record.result",
                    )
                    .map_err(compiler_diagnostic)?;
                let result_slot = self
                    .backend
                    .builder
                    .build_struct_gep(header_type, *frame, CORO_RESULT_PTR, "coro.header.slot")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(result_slot, record_result)
                    .map_err(compiler_diagnostic)?;
                let record_slot = self
                    .backend
                    .builder
                    .build_struct_gep(header_type, *frame, CORO_RECORD, "coro.header.slot")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(record_slot, record)
                    .map_err(compiler_diagnostic)?;
                let parent_slot = self
                    .backend
                    .builder
                    .build_struct_gep(header_type, *frame, CORO_PARENT, "coro.header.slot")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(parent_slot, pointer_type.const_null())
                    .map_err(compiler_diagnostic)?;
                let frame_slot = self
                    .backend
                    .builder
                    .build_struct_gep(record_type, record, TASK_RECORD_FRAME, "task.record.frame")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(frame_slot, *frame)
                    .map_err(compiler_diagnostic)?;

                let uses = self.activation_resource_uses(call, &span)?;
                self.store_coroutine_resources(owner, &uses, *frame, environment, &span)?;

                let scope = self.current_task_scope(owner, call, environment, &span)?;
                let scheduler = self.current_scheduler(owner, call, environment, &span)?;
                let scheduler_slot = self
                    .backend
                    .builder
                    .build_struct_gep(
                        record_type,
                        record,
                        TASK_RECORD_SCHEDULER,
                        "task.record.scheduler",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(scheduler_slot, scheduler)
                    .map_err(compiler_diagnostic)?;

                let track = self.backend.declare_named_function(
                    "__staple_task_scope_track",
                    self.backend
                        .context
                        .void_type()
                        .fn_type(&[pointer_type.into(), pointer_type.into()], false),
                );
                self.backend
                    .build_runtime_call(track, &[scope.into(), record.into()], "")?;
                let enqueue = self.backend.declare_named_function(
                    "__staple_sched_enqueue",
                    self.backend
                        .context
                        .void_type()
                        .fn_type(&[pointer_type.into(), pointer_type.into()], false),
                );
                self.backend.build_runtime_call(
                    enqueue,
                    &[scheduler.into(), (*frame).into()],
                    "",
                )?;
                Ok(record.as_any_value_enum())
            }
            IntrinsicFunction::Pump => {
                let values = self.intrinsic_product(call, arguments, 2, &span)?;
                let [scheduler, limit] = values.as_slice() else {
                    return Err(Diagnostic::new(span.clone(), "`pump` takes two arguments"));
                };
                let (scheduler, limit) = (*scheduler, *limit);
                let counts_type = self.backend.context.struct_type(
                    &[self.backend.size_type.into(), self.backend.size_type.into()],
                    false,
                );
                let pump = self.backend.declare_named_function(
                    "__staple_sched_pump",
                    counts_type
                        .fn_type(&[pointer_type.into(), self.backend.size_type.into()], false),
                );
                let result = self
                    .backend
                    .build_runtime_call(pump, &[scheduler.into(), limit.into()], "pump")?
                    .try_as_basic_value()
                    .unwrap_basic();
                Ok(result.as_any_value_enum())
            }
            IntrinsicFunction::TaskIsFinished => {
                let [BasicMetadataValueEnum::PointerValue(record)] = arguments else {
                    return Err(Diagnostic::new(span, "task handle is not first-class"));
                };
                let state = self
                    .backend
                    .builder
                    .build_load(i8_type, *record, "task.state")
                    .map_err(compiler_diagnostic)?
                    .into_int_value();
                // `record.state`: 0 pending, 1 completed, 2 cancelled — finished
                // is anything past pending.
                let finished = self
                    .backend
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::NE,
                        state,
                        i8_type.const_int(0, false),
                        "task.finished",
                    )
                    .map_err(compiler_diagnostic)?;
                self.build_intrinsic_bool(finished, &call.result_type, span)
            }
            IntrinsicFunction::TaskCancel => {
                let [BasicMetadataValueEnum::PointerValue(record)] = arguments else {
                    return Err(Diagnostic::new(span, "task handle is not first-class"));
                };
                let cancel = self.backend.declare_named_function(
                    "__staple_task_cancel",
                    self.backend
                        .context
                        .void_type()
                        .fn_type(&[pointer_type.into()], false),
                );
                self.backend
                    .build_runtime_call(cancel, &[(*record).into()], "")?;
                Ok(self.backend.unit_value())
            }
            other => Err(Diagnostic::new(
                span,
                format!("internal invariant: {other:?} is not a scheduler intrinsic"),
            )),
        }
    }

    /// Compile completion intrinsic.
    pub(super) fn emit_completion_intrinsic(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        call_id: LoweredCallId,
        intrinsic: IntrinsicFunction,
        arguments: &[BasicMetadataValueEnum<'context>],
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = call.origin.span.clone();
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        let i8_type = self.backend.context.i8_type();
        match intrinsic {
            IntrinsicFunction::Completion
            | IntrinsicFunction::CompletionWithCancel
            | IntrinsicFunction::CompletionToken => {
                let value_type = call.runtime.completion_value_type.clone().ok_or_else(|| {
                    Diagnostic::new(span.clone(), "completion result is missing its payload")
                })?;
                let (scheduler, cancel_closure) =
                    if intrinsic == IntrinsicFunction::CompletionWithCancel {
                        let fields = self.intrinsic_product(call, arguments, 2, &span)?;
                        let [scheduler, closure] = fields.as_slice() else {
                            return Err(Diagnostic::new(
                                span.clone(),
                                "`completion_with_cancel` takes two arguments",
                            ));
                        };
                        let closure = struct_operand(*closure, "cancel callback", &span)?;
                        let scheduler = *scheduler;
                        (scheduler, Some(closure))
                    } else {
                        let [scheduler] = arguments else {
                            return Err(Diagnostic::new(span, "argument is not first-class"));
                        };
                        (
                            BasicValueEnum::try_from(*scheduler).map_err(|_| {
                                Diagnostic::new(span.clone(), "argument is not first-class")
                            })?,
                            None,
                        )
                    };
                let value_llvm = self.backend.compile_type(&value_type)?;
                let record_type = self.backend.completion_record_type(value_llvm);
                let record = self.backend.build_gc_allocation(
                    self.backend
                        .size_type
                        .const_int(self.backend.target_data.get_store_size(&record_type), false),
                    "completion.record",
                    span.clone(),
                )?;
                self.backend
                    .builder
                    .build_store(record, record_type.const_zero())
                    .map_err(compiler_diagnostic)?;
                let scheduler_slot = self
                    .backend
                    .builder
                    .build_struct_gep(
                        record_type,
                        record,
                        COMPLETION_SCHEDULER,
                        "completion.scheduler",
                    )
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(scheduler_slot, scheduler)
                    .map_err(compiler_diagnostic)?;
                if let Some(closure) = cancel_closure {
                    // `{ code, environment }` — store both halves and arm the
                    // callback (flags bit1).
                    let code = self
                        .backend
                        .builder
                        .build_extract_value(closure, 0, "on_cancel.code")
                        .map_err(compiler_diagnostic)?;
                    let environment = self
                        .backend
                        .builder
                        .build_extract_value(closure, 1, "on_cancel.env")
                        .map_err(compiler_diagnostic)?;
                    let cancel_env = self
                        .backend
                        .builder
                        .build_struct_gep(
                            record_type,
                            record,
                            COMPLETION_CANCEL_ENV,
                            "completion.cancel.env",
                        )
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_store(cancel_env, environment)
                        .map_err(compiler_diagnostic)?;
                    let cancel_fn = self
                        .backend
                        .builder
                        .build_struct_gep(
                            record_type,
                            record,
                            COMPLETION_CANCEL_FN,
                            "completion.cancel.fn",
                        )
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_store(cancel_fn, code)
                        .map_err(compiler_diagnostic)?;
                    let flags = self
                        .backend
                        .builder
                        .build_struct_gep(record_type, record, COMPLETION_FLAGS, "completion.flags")
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_store(
                            flags,
                            i8_type.const_int(COMPLETION_FLAG_CANCEL_ARMED as u64, false),
                        )
                        .map_err(compiler_diagnostic)?;
                }
                // `(wait, resolver)` — both handles are the same record pointer.
                let handles_type = self
                    .backend
                    .context
                    .struct_type(&[pointer_type.into(), pointer_type.into()], true);
                let mut value = handles_type.const_zero();
                value = self
                    .backend
                    .builder
                    .build_insert_value(value, record, 0, "completion.wait")
                    .map_err(compiler_diagnostic)?
                    .into_struct_value();
                value = self
                    .backend
                    .builder
                    .build_insert_value(value, record, 1, "completion.resolver")
                    .map_err(compiler_diagnostic)?
                    .into_struct_value();
                Ok(value.as_any_value_enum())
            }
            IntrinsicFunction::ResolverComplete => {
                let value_type = call.runtime.completion_value_type.clone().ok_or_else(|| {
                    Diagnostic::new(span.clone(), "`complete` value has no concrete type")
                })?;
                let fields = self.intrinsic_product(call, arguments, 2, &span)?;
                let [record, value] = fields.as_slice() else {
                    return Err(Diagnostic::new(
                        span.clone(),
                        "`Resolver.complete` takes two arguments",
                    ));
                };
                let value = *value;
                let record = pointer_operand(*record, "resolver record", &span)?;
                let value_llvm = self.backend.compile_type(&value_type)?;
                let slot = self
                    .backend
                    .entry_builder()
                    .build_alloca(value_llvm, "resolver.value.slot")
                    .map_err(compiler_diagnostic)?;
                self.backend
                    .builder
                    .build_store(slot, value)
                    .map_err(compiler_diagnostic)?;
                let size = self
                    .backend
                    .size_type
                    .const_int(self.backend.target_data.get_store_size(&value_llvm), false);
                let complete = self.backend.declare_named_function(
                    "__staple_completion_complete",
                    i8_type.fn_type(
                        &[
                            pointer_type.into(),
                            pointer_type.into(),
                            self.backend.size_type.into(),
                        ],
                        false,
                    ),
                );
                let gone = self
                    .backend
                    .build_runtime_call(
                        complete,
                        &[record.into(), slot.into(), size.into()],
                        "completion.gone",
                    )?
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value();
                // If the consumer had already abandoned the wait, the runtime
                // did not take the value — drop the copy we still own. The
                // recorded `CompletionOrphan` use is the existence fact (Step
                // 2), never a type query.
                let orphan = self.view.artifact_uses(owner).is_some_and(|uses| {
                    uses.iter()
                        .any(|use_| use_.site == crate::ArtifactUseSite::CompletionOrphan(call_id))
                });
                if orphan {
                    let function = self
                        .backend
                        .builder
                        .get_insert_block()
                        .and_then(|block| block.get_parent())
                        .ok_or_else(|| {
                            Diagnostic::new(span.clone(), "`complete` is not in a function")
                        })?;
                    let drop_block = self
                        .backend
                        .context
                        .append_basic_block(function, "complete.drop");
                    let done_block = self
                        .backend
                        .context
                        .append_basic_block(function, "complete.done");
                    let is_gone = self
                        .backend
                        .builder
                        .build_int_compare(
                            inkwell::IntPredicate::NE,
                            gone,
                            i8_type.const_zero(),
                            "complete.consumer.gone",
                        )
                        .map_err(compiler_diagnostic)?;
                    self.backend
                        .builder
                        .build_conditional_branch(is_gone, drop_block, done_block)
                        .map_err(compiler_diagnostic)?;
                    self.backend.builder.position_at_end(drop_block);
                    let owned = self
                        .backend
                        .builder
                        .build_load(value_llvm, slot, "complete.orphan")
                        .map_err(compiler_diagnostic)?;
                    self.emit_drop_site(
                        owner,
                        crate::ArtifactUseSite::CompletionOrphan(call_id),
                        DropSource::Value(owned),
                        &span,
                    )?;
                    self.backend
                        .builder
                        .build_unconditional_branch(done_block)
                        .map_err(compiler_diagnostic)?;
                    self.backend.builder.position_at_end(done_block);
                }
                Ok(self.backend.unit_value())
            }
            IntrinsicFunction::ResolverCancel
            | IntrinsicFunction::CompletionTokenResolve
            | IntrinsicFunction::CompletionTokenCancel => {
                let [BasicMetadataValueEnum::PointerValue(record)] = arguments else {
                    return Err(Diagnostic::new(span, "handle is not first-class"));
                };
                let runtime = match intrinsic {
                    IntrinsicFunction::CompletionTokenResolve => {
                        "__staple_completion_token_resolve"
                    }
                    IntrinsicFunction::CompletionTokenCancel => "__staple_completion_token_cancel",
                    _ => "__staple_completion_cancel",
                };
                let function = self.backend.declare_named_function(
                    runtime,
                    self.backend
                        .context
                        .void_type()
                        .fn_type(&[pointer_type.into()], false),
                );
                self.backend
                    .build_runtime_call(function, &[(*record).into()], "")?;
                Ok(self.backend.unit_value())
            }
            other => Err(Diagnostic::new(
                span,
                format!("internal invariant: {other:?} is not a completion intrinsic"),
            )),
        }
    }

    /// Compile product expression's packed struct build followed by
    /// the field extractions the emitter's intrinsic branches perform. A product
    /// *literal* is lowered element by element, so its product expression is
    /// rebuilt here; a whole product value reaches `emit_intrinsic` already
    /// unpacked into its fields by `emit_call`.
    fn intrinsic_product(
        &self,
        call: &crate::LoweredCall,
        arguments: &[BasicMetadataValueEnum<'context>],
        fields: usize,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Vec<BasicValueEnum<'context>>> {
        let values = arguments
            .iter()
            .map(|argument| {
                BasicValueEnum::try_from(*argument)
                    .map_err(|_| Diagnostic::new(span.clone(), "argument is not first-class"))
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        if values.len() != fields {
            return Err(Diagnostic::new(
                span.clone(),
                "intrinsic argument count does not match its parameter list",
            ));
        }
        let product_literal = fields > 1
            && call.arguments.len() > 1
            && call
                .arguments
                .iter()
                .all(|argument| argument.slot.is_some());
        if !product_literal {
            return Ok(values);
        }
        let product = self.backend.build_product_value(&values, span.clone())?;
        let BasicValueEnum::StructValue(product) = product else {
            return Err(Diagnostic::new(
                span.clone(),
                "intrinsic product argument is not a product value",
            ));
        };
        (0..fields)
            .map(|index| {
                self.backend
                    .builder
                    .build_extract_value(product, index as u32, "argument.element")
                    .map_err(compiler_diagnostic)
            })
            .collect()
    }
}
