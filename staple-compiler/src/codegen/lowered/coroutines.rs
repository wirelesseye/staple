//! Coroutine pair emission from the concrete artifact and body records.
use super::*;
use crate::codegen::ir::{CoroutineResumeEntry, ExternalAwaitKind};
use crate::codegen::layout::{
    COMPLETION_STATE_COMPLETED, COMPLETION_VALUE, CORO_CAPTURE_ENV, CORO_CHILD, CORO_HEADER_FIELDS,
    CORO_RESOURCES, CORO_RESULT_PTR, CORO_STATE, TASK_RECORD_RESULT,
};
use crate::{
    CoroutineCodesPlan, CoroutineFramePlan, LoweredAwaitId, LoweredAwaitKind, LoweredCoercionPlan,
    LoweredCoroId,
};
use inkwell::types::StructType;

#[derive(Clone)]
pub(super) struct CoroutineContext<'context> {
    pub frame: PointerValue<'context>,
    pub frame_type: StructType<'context>,
    pub status_type: StructType<'context>,
    pub dispatch: Vec<BasicBlock<'context>>,
    pub pending_field: u32,
}

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    fn coroutine_frame_type(
        &self,
        owner: EmissionOwner,
        frame: &CoroutineFramePlan,
    ) -> CodeGenerationResult<crate::codegen::ir::CoroutineFrameType<'context>> {
        let cells = frame
            .frame_bindings
            .iter()
            .map(|binding| {
                self.binding_cell_type(owner, binding.symbol)
                    .map(Into::into)
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
        let layout = self.coroutine_frame_type(owner, frame_plan)?;
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
            self.backend
                .build_coroutine_complete(result, result_slot, state_slot, status)?;
        }
        self.backend.build_coroutine_cleanup(cleanup, finalizer)?;
        if let Some(previous) = previous {
            self.backend.builder.position_at_end(previous);
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
        let layout = self.coroutine_frame_type(EmissionOwner::Instance(plan.body), frame_plan)?;
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

    /// Stage 5.8 Step 5: one `await` suspension inside a coroutine body.
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
                let child = value_as_basic(child)
                    .ok_or_else(|| {
                        Diagnostic::new(span.clone(), "`await` operand is not a coroutine")
                    })?
                    .into_pointer_value();
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

    /// Legacy `compile_external_await`: register the frame as the record's
    /// waiter, park, and on resume read `Completed payload | Cancelled` from
    /// the record's state.
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
        let completed_type = outcome_sum.alternatives[0].clone();
        let cancelled_type = outcome_sum.alternatives[1].clone();
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
            .ok_or_else(|| Diagnostic::new(span.clone(), "`await` operand is not a wait handle"))?
            .into_pointer_value();
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
        let completed_plan = LoweredCoercionPlan::SumInject {
            alternative: 0,
            payload: Box::new(LoweredCoercionPlan::Identity),
        };
        let cancelled_plan = LoweredCoercionPlan::SumInject {
            alternative: 1,
            payload: Box::new(LoweredCoercionPlan::Identity),
        };

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
            &completed_type,
            outcome_type,
            &completed_plan,
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
            &cancelled_type,
            outcome_type,
            &cancelled_plan,
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

    /// Legacy `store_coroutine_resources`: pack the recorded deferred
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

    /// Stage 5.8 Step 5: `block_on`'s synchronous drive (`compile_coroutine_drive`).
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
        let activation = call.runtime.coroutine.as_ref().ok_or_else(|| {
            Diagnostic::new(
                span.clone(),
                "`block_on` is missing its recorded activation",
            )
        })?;
        let uses = activation
            .deferred_resources
            .iter()
            .map(|index| {
                call.resource_bindings.get(*index).copied().ok_or_else(|| {
                    Diagnostic::new(
                        span.clone(),
                        "`block_on` activation names a missing resource binding",
                    )
                })
            })
            .collect::<CodeGenerationResult<Vec<_>>>()?;
        self.store_coroutine_resources(owner, &uses, frame, environment, span)?;
        let result_llvm = self.backend.compile_type(&activation.result_type)?;
        self.backend
            .build_coroutine_drive(frame, result_llvm, span.clone())
            .map(|value| value.as_any_value_enum())
    }
}
