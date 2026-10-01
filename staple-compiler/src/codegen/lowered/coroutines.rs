//! Coroutine pair emission from the concrete artifact and body records.
use super::*;
use crate::codegen::ir::CoroutineResumeEntry;
use crate::codegen::layout::{
    CORO_CAPTURE_ENV, CORO_HEADER_FIELDS, CORO_RESOURCES, CORO_RESULT_PTR, CORO_STATE,
};
use crate::{CoroutineCodesPlan, CoroutineFramePlan, LoweredCoroId};
use inkwell::types::StructType;

// Step 5 consumes the suspension context fields.
#[allow(dead_code)]
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
}
