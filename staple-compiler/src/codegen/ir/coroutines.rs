//! Stage 5.8 shared coroutine state-machine and reactive runner IR.
//! Inputs are compiled LLVM types/values; source and checked-type selection
//! remain with the caller.
use super::super::layout::*;
use super::super::{Backend, CodeGenerationResult, Diagnostic, Span, compiler_diagnostic};
use super::value_as_basic;
use inkwell::{
    AddressSpace,
    basic_block::BasicBlock,
    types::{BasicTypeEnum, FunctionType, StructType},
    values::{
        AnyValue, BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue,
        StructValue,
    },
};

pub(crate) struct CoroutineFrameType<'context> {
    pub ty: StructType<'context>,
    pub frame_size: u64,
    pub result_field: u32,
    pub pending_field: u32,
}
pub(crate) struct CoroutineResumeEntry<'context> {
    pub frame: PointerValue<'context>,
    pub bad_state: BasicBlock<'context>,
    pub dispatch: Vec<BasicBlock<'context>>,
}
impl<'program, 'context> Backend<'program, 'context> {
    pub(crate) fn build_coroutine_frame_type(
        &self,
        cells: &[BasicTypeEnum<'context>],
        result: BasicTypeEnum<'context>,
        pending_bytes: Option<u32>,
    ) -> CoroutineFrameType<'context> {
        let mut fields = self.coroutine_header_type().get_field_types();
        fields.extend_from_slice(cells);
        let result_field = fields.len() as u32;
        fields.push(result);
        let pending_field = fields.len() as u32;
        if let Some(bytes) = pending_bytes {
            fields.push(self.context.i8_type().array_type(bytes).into());
        }
        let ty = self.context.struct_type(&fields, false);
        CoroutineFrameType {
            ty,
            frame_size: self.target_data.get_store_size(&ty),
            result_field,
            pending_field,
        }
    }
    pub(crate) fn reaction_payload_type(
        &self,
        fields: &[BasicTypeEnum<'context>],
    ) -> StructType<'context> {
        self.context.struct_type(fields, false)
    }

    pub(crate) fn derived_payload_type(
        &self,
        callback: StructType<'context>,
    ) -> StructType<'context> {
        self.context.struct_type(
            &[
                callback.into(),
                self.context.ptr_type(AddressSpace::default()).into(),
            ],
            false,
        )
    }

    pub(crate) fn until_payload_type(&self) -> StructType<'context> {
        self.context.struct_type(
            &[self.context.ptr_type(AddressSpace::default()).into(); 4],
            false,
        )
    }
    pub(crate) fn begin_coroutine_resume(
        &self,
        function: FunctionValue<'context>,
        resume_points: usize,
    ) -> CodeGenerationResult<CoroutineResumeEntry<'context>> {
        let entry = self.context.append_basic_block(function, "entry");
        let bad_state = self.context.append_basic_block(function, "bad.state");
        let mut dispatch = Vec::new();
        for state in 0..=resume_points {
            dispatch.push(
                self.context
                    .append_basic_block(function, &format!("state.{state}")),
            );
        }

        self.builder.position_at_end(entry);
        let frame = function
            .get_first_param()
            .expect("resume frame parameter")
            .into_pointer_value();

        Ok(CoroutineResumeEntry {
            frame,
            bad_state,
            dispatch,
        })
    }

    pub(crate) fn build_coroutine_resume_dispatch(
        &self,
        function: FunctionValue<'context>,
        frame: PointerValue<'context>,
        state_slot: PointerValue<'context>,
        status_type: StructType<'context>,
        dispatch: &[BasicBlock<'context>],
        bad_state: BasicBlock<'context>,
        resume_points: usize,
    ) -> CodeGenerationResult<IntValue<'context>> {
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let i8_type = self.context.i8_type();
        let header_type = self.coroutine_header_type();
        let state = self
            .builder
            .build_load(i8_type, state_slot, "coro.state")
            .map_err(compiler_diagnostic)?
            .into_int_value();

        // Cancellation check: a `spawn`ed task whose record carries a
        // cancel request unwinds at this boundary instead of resuming.
        let cancel_check = self.context.append_basic_block(function, "cancel.check");
        let unwind = self.context.append_basic_block(function, "cancel.unwind");
        let do_switch = self.context.append_basic_block(function, "resume.dispatch");
        let already_done = self.context.append_basic_block(function, "resume.spent");

        let record_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_RECORD, "coro.record.slot")
            .map_err(compiler_diagnostic)?;
        let record = self
            .builder
            .build_load(ptr_type, record_slot, "coro.record")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let has_record = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                record,
                ptr_type.const_null(),
                "coro.has.record",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(has_record, cancel_check, do_switch)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(cancel_check);
        let record_header = self.task_record_header_type();
        let cancel_slot = self
            .builder
            .build_struct_gep(
                record_header,
                record,
                TASK_RECORD_CANCEL,
                "task.cancel.slot",
            )
            .map_err(compiler_diagnostic)?;
        let cancel = self
            .builder
            .build_load(i8_type, cancel_slot, "task.cancel")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let cancel_set = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                cancel,
                i8_type.const_zero(),
                "task.cancel.set",
            )
            .map_err(compiler_diagnostic)?;
        let state_live = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::ULE,
                state,
                i8_type.const_int(resume_points as u64, false),
                "coro.state.live",
            )
            .map_err(compiler_diagnostic)?;
        let want_unwind = self
            .builder
            .build_and(cancel_set, state_live, "coro.want.unwind")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(want_unwind, unwind, do_switch)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(do_switch);
        let mut cases = (0..=resume_points)
            .map(|k| (i8_type.const_int(k as u64, false), dispatch[k]))
            .collect::<Vec<_>>();
        cases.push((i8_type.const_int(CORO_STATE_DONE, false), already_done));
        cases.push((i8_type.const_int(CORO_STATE_FREED, false), already_done));
        self.builder
            .build_switch(state, bad_state, &cases)
            .map_err(compiler_diagnostic)?;

        // A frame that has already run to its end (or been freed) is driven
        // again only by a redundant cancel/enqueue; report "done, no value".
        self.builder.position_at_end(already_done);
        self.builder
            .build_return(Some(&status_type.const_zero()))
            .map_err(compiler_diagnostic)?;

        // The cancellation unwind: if the frame is parked on a `Wait`, run
        // that completion's cancellation callback and drop the registration;
        // then drop the frame's initialised locals (and, for a task
        // cancelled before it ever ran, its owned captures), mark the frame
        // spent, and report CANCELLED to the driver.
        self.builder.position_at_end(unwind);
        Ok(state)
    }

    pub(crate) fn build_coroutine_bad_state(
        &self,
        bad_state: BasicBlock<'context>,
    ) -> CodeGenerationResult<()> {
        self.builder.position_at_end(bad_state);
        let trap = self
            .llvm_module
            .get_function("llvm.trap")
            .unwrap_or_else(|| {
                self.llvm_module.add_function(
                    "llvm.trap",
                    self.context.void_type().fn_type(&[], false),
                    None,
                )
            });
        self.build_runtime_call(trap, &[], "")?;
        self.builder
            .build_unreachable()
            .map_err(compiler_diagnostic)?;

        Ok(())
    }

    pub(crate) fn build_coroutine_complete(
        &self,
        return_value: BasicValueEnum<'context>,
        result_ptr_slot: PointerValue<'context>,
        state_slot: PointerValue<'context>,
        status_type: StructType<'context>,
    ) -> CodeGenerationResult<()> {
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let i8_type = self.context.i8_type();
        let result_ptr = self
            .builder
            .build_load(ptr_type, result_ptr_slot, "coro.result.ptr")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        self.builder
            .build_store(result_ptr, return_value)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(state_slot, i8_type.const_int(CORO_STATE_DONE, false))
            .map_err(compiler_diagnostic)?;
        let status = status_type.const_zero();
        self.builder
            .build_return(Some(&status))
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    pub(crate) fn build_coroutine_cleanup(
        &self,
        function: FunctionValue<'context>,
        finalizer: Option<FunctionValue<'context>>,
    ) -> CodeGenerationResult<()> {
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let i8_type = self.context.i8_type();
        let header_type = self.coroutine_header_type();
        let entry = self.context.append_basic_block(function, "entry");
        let not_freed = self.context.append_basic_block(function, "not.freed");
        let drop_block = self.context.append_basic_block(function, "drop.captures");
        let finish = self.context.append_basic_block(function, "finish");
        let done = self.context.append_basic_block(function, "done");

        self.builder.position_at_end(entry);
        let frame = function
            .get_first_param()
            .expect("cleanup frame parameter")
            .into_pointer_value();
        let state_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_STATE, "coro.state.slot")
            .map_err(compiler_diagnostic)?;
        let state = self
            .builder
            .build_load(i8_type, state_slot, "coro.state")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let is_freed = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                state,
                i8_type.const_int(CORO_STATE_FREED, false),
                "coro.is.freed",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(is_freed, done, not_freed)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(not_freed);
        // `state == 0` (created, never resumed) is the only case whose
        // captures are still owned by the frame; a completed body already
        // dropped its owned locals, and a mid-body suspension leaves its
        // frame cells for the (Step 3d) unwind path.
        let never_resumed = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                state,
                i8_type.const_int(0, false),
                "coro.never.resumed",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(never_resumed, drop_block, finish)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(drop_block);
        if let Some(finalizer) = finalizer {
            let env_slot = self
                .builder
                .build_struct_gep(header_type, frame, CORO_CAPTURE_ENV, "coro.env.slot")
                .map_err(compiler_diagnostic)?;
            let env_ptr = self
                .builder
                .build_load(ptr_type, env_slot, "coro.env")
                .map_err(compiler_diagnostic)?
                .into_pointer_value();
            self.build_runtime_call(finalizer, &[env_ptr.into()], "")?;
        }
        self.builder
            .build_unconditional_branch(finish)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(finish);
        let unregister = self
            .llvm_module
            .get_function("__staple_gc_unregister_root")
            .expect("GC root unregistration function");
        self.build_runtime_call(unregister, &[frame.into()], "")?;
        self.builder
            .build_store(state_slot, i8_type.const_int(CORO_STATE_FREED, false))
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_unconditional_branch(done)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(done);
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    pub(crate) fn build_coroutine_creation(
        &self,
        frame_type: StructType<'context>,
        frame_size: u64,
        result_field: u32,
        resume_fn: FunctionValue<'context>,
        cleanup_fn: FunctionValue<'context>,
        env_ptr: PointerValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let header_type = self.coroutine_header_type();
        let i8_type = self.context.i8_type();
        let frame = self.build_gc_allocation(
            self.size_type.const_int(frame_size, false),
            "coro.frame",
            span.clone(),
        )?;
        self.builder
            .build_store(frame, frame_type.const_zero())
            .map_err(compiler_diagnostic)?;
        let store_header =
            |emitter: &Self, field: u32, value: inkwell::values::BasicValueEnum<'context>| {
                let slot = emitter
                    .builder
                    .build_struct_gep(header_type, frame, field, "coro.header.slot")
                    .map_err(compiler_diagnostic)?;
                emitter
                    .builder
                    .build_store(slot, value)
                    .map_err(compiler_diagnostic)?;
                Ok::<(), Diagnostic>(())
            };
        store_header(self, CORO_STATE, i8_type.const_int(0, false).into())?;
        store_header(
            self,
            CORO_RESUME_FN,
            resume_fn.as_global_value().as_pointer_value().into(),
        )?;
        store_header(
            self,
            CORO_CLEANUP_FN,
            cleanup_fn.as_global_value().as_pointer_value().into(),
        )?;
        store_header(self, CORO_CAPTURE_ENV, env_ptr.into())?;
        let own_result = self
            .builder
            .build_struct_gep(frame_type, frame, result_field, "coro.own.result")
            .map_err(compiler_diagnostic)?;
        store_header(self, CORO_RESULT_PTR, own_result.into())?;

        self.register_gc_root_region(frame, frame_size, span)?;
        Ok(frame)
    }

    pub(crate) fn build_coroutine_child_suspend(
        &self,
        frame: PointerValue<'context>,
        frame_type: StructType<'context>,
        pending_field: u32,
        state: usize,
        child: PointerValue<'context>,
        status_type: StructType<'context>,
    ) -> CodeGenerationResult<()> {
        let i8_type = self.context.i8_type();
        let header_type = self.coroutine_header_type();
        let child_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_CHILD, "coro.child.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(child_slot, child)
            .map_err(compiler_diagnostic)?;

        let pending = self
            .builder
            .build_struct_gep(frame_type, frame, pending_field, "coro.own.pending")
            .map_err(compiler_diagnostic)?;
        let pending_ptr_slot = self
            .builder
            .build_struct_gep(
                header_type,
                frame,
                CORO_PENDING_PTR,
                "coro.pending.ptr.slot",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(pending_ptr_slot, pending)
            .map_err(compiler_diagnostic)?;

        let state_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_STATE, "coro.state.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(state_slot, i8_type.const_int(state as u64, false))
            .map_err(compiler_diagnostic)?;

        let mut status = status_type.const_zero();
        status = self
            .builder
            .build_insert_value(
                status,
                i8_type.const_int(CORO_STATUS_RESUME_CHILD, false),
                0,
                "coro.status.kind",
            )
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        status = self
            .builder
            .build_insert_value(status, child, 1, "coro.status.child")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        self.builder
            .build_return(Some(&status))
            .map_err(compiler_diagnostic)?;

        Ok(())
    }

    pub(crate) fn build_coroutine_pending_result(
        &self,
        frame: PointerValue<'context>,
        frame_type: StructType<'context>,
        pending_field: u32,
        result_llvm: BasicTypeEnum<'context>,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let pending = self
            .builder
            .build_struct_gep(frame_type, frame, pending_field, "coro.own.pending")
            .map_err(compiler_diagnostic)?;
        let value = self
            .builder
            .build_load(result_llvm, pending, "await.result")
            .map_err(compiler_diagnostic)?;
        Ok(value)
    }

    pub(crate) fn build_until_runner(
        &self,
        runner: FunctionValue<'context>,
        bool_fn_type: FunctionType<'context>,
    ) -> CodeGenerationResult<()> {
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let i8_type = self.context.i8_type();
        let i32_type = self.context.i32_type();
        let size_type = self.size_type;
        let payload_type = self.until_payload_type();
        let entry = self.context.append_basic_block(runner, "entry");
        let eval = self.context.append_basic_block(runner, "eval");
        let resolve = self.context.append_basic_block(runner, "resolve");
        let done = self.context.append_basic_block(runner, "done");

        self.builder.position_at_end(entry);
        let payload = runner.get_first_param().unwrap().into_pointer_value();
        let load_field = |emitter: &Self, index: u32, name: &str| {
            let slot = emitter
                .builder
                .build_struct_gep(payload_type, payload, index, name)
                .map_err(compiler_diagnostic)?;
            emitter
                .builder
                .build_load(ptr_type, slot, name)
                .map_err(compiler_diagnostic)
                .map(|value| value.into_pointer_value())
        };
        let completion = load_field(self, 2, "until.completion")?;
        let completion_state = self
            .builder
            .build_load(i8_type, completion, "until.completion.state")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        let already = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                completion_state,
                i8_type.const_zero(),
                "until.already.resolved",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(already, done, eval)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(eval);
        let code = load_field(self, 0, "until.code")?;
        let env = load_field(self, 1, "until.env")?;
        let result = self
            .builder
            .build_indirect_call(bool_fn_type, code, &[env.into()], "until.predicate")
            .map_err(compiler_diagnostic)?
            .try_as_basic_value()
            .unwrap_basic()
            .into_struct_value();
        let tag = self
            .builder
            .build_extract_value(result, 0, "until.bool.tag")
            .map_err(compiler_diagnostic)?
            .into_int_value();
        // `Bool` is `True | False`; `True` is alternative 0.
        let is_true = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::EQ,
                tag,
                i32_type.const_zero(),
                "until.is.true",
            )
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_conditional_branch(is_true, resolve, done)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(resolve);
        let complete = self.declare_named_function(
            "__staple_completion_complete",
            i8_type.fn_type(&[ptr_type.into(), ptr_type.into(), size_type.into()], false),
        );
        self.build_runtime_call(
            complete,
            &[
                completion.into(),
                completion.into(),
                size_type.const_zero().into(),
            ],
            "until.resolve",
        )?;
        self.builder
            .build_unconditional_branch(done)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(done);
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;

        Ok(())
    }

    pub(crate) fn build_reaction_runner(
        &self,
        runner: FunctionValue<'context>,
        payload_type: StructType<'context>,
        callback_type: StructType<'context>,
        resource_types: &[BasicTypeEnum<'context>],
        closure_fn_type: FunctionType<'context>,
    ) -> CodeGenerationResult<()> {
        let entry = self.context.append_basic_block(runner, "entry");
        self.builder.position_at_end(entry);
        let payload_argument = runner.get_first_param().unwrap().into_pointer_value();
        let callback_slot = self
            .builder
            .build_struct_gep(payload_type, payload_argument, 0, "reaction.callback")
            .map_err(compiler_diagnostic)?;
        let loaded_callback = self
            .builder
            .build_load(callback_type, callback_slot, "reaction.callback")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        let code = self
            .builder
            .build_extract_value(loaded_callback, 0, "reaction.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let closure_environment = self
            .builder
            .build_extract_value(loaded_callback, 1, "reaction.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let mut arguments = vec![closure_environment.into()];
        for (index, resource) in resource_types.iter().enumerate() {
            let slot = self
                .builder
                .build_struct_gep(
                    payload_type,
                    payload_argument,
                    (index + 1) as u32,
                    "reaction.resource",
                )
                .map_err(compiler_diagnostic)?;
            arguments.push(
                self.builder
                    .build_load(*resource, slot, "reaction.resource")
                    .map_err(compiler_diagnostic)?
                    .into(),
            );
        }
        self.builder
            .build_indirect_call(closure_fn_type, code, &arguments, "reaction.call")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    pub(crate) fn build_derived_runner(
        &self,
        runner: FunctionValue<'context>,
        payload_type: StructType<'context>,
        callback_type: StructType<'context>,
        closure_fn_type: FunctionType<'context>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let pointer_type = self.context.ptr_type(AddressSpace::default());
        let entry = self.context.append_basic_block(runner, "entry");
        self.builder.position_at_end(entry);
        let payload_argument = runner.get_first_param().unwrap().into_pointer_value();
        let callback_slot = self
            .builder
            .build_struct_gep(payload_type, payload_argument, 0, "derived.callback")
            .map_err(compiler_diagnostic)?;
        let loaded_callback = self
            .builder
            .build_load(callback_type, callback_slot, "derived.callback")
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        let code = self
            .builder
            .build_extract_value(loaded_callback, 0, "derived.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let closure_environment = self
            .builder
            .build_extract_value(loaded_callback, 1, "derived.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let call = self
            .builder
            .build_indirect_call(
                closure_fn_type,
                code,
                &[closure_environment.into()],
                "derived.evaluate",
            )
            .map_err(compiler_diagnostic)?;
        let value = call.try_as_basic_value().basic().ok_or_else(|| {
            Diagnostic::new(span.clone(), "derived evaluator result is not storable")
        })?;
        let output_slot = self
            .builder
            .build_struct_gep(payload_type, payload_argument, 1, "derived.output")
            .map_err(compiler_diagnostic)?;
        let output = self
            .builder
            .build_load(pointer_type, output_slot, "derived.output")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        self.builder
            .build_store(output, value)
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_return(None)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }
}

impl<'program, 'context> Backend<'program, 'context> {
    pub(crate) fn build_coroutine_resource_bundle(
        &mut self,
        frame: PointerValue<'context>,
        bundle_type: StructType<'context>,
        arguments: Vec<BasicMetadataValueEnum<'context>>,
        span: Span,
    ) -> CodeGenerationResult<()> {
        let bundle = self.build_gc_allocation(
            self.size_type
                .const_int(self.target_data.get_store_size(&bundle_type), false),
            "coro.bundle",
            span.clone(),
        )?;
        let mut value = bundle_type.const_zero();
        for (index, argument) in arguments.into_iter().enumerate() {
            let basic = value_as_basic(argument.as_any_value_enum())
                .ok_or_else(|| Diagnostic::new(span.clone(), "resource is not first-class"))?;
            value = self
                .builder
                .build_insert_value(value, basic, index as u32, "coro.resource")
                .map_err(compiler_diagnostic)?
                .into_struct_value();
        }
        self.builder
            .build_store(bundle, value)
            .map_err(compiler_diagnostic)?;
        let header_type = self.coroutine_header_type();
        let slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_RESOURCES, "coro.resources.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(slot, bundle)
            .map_err(compiler_diagnostic)?;
        Ok(())
    }

    pub(crate) fn build_coroutine_drive(
        &mut self,
        frame: PointerValue<'context>,
        result_llvm: BasicTypeEnum<'context>,
        span: Span,
    ) -> CodeGenerationResult<BasicValueEnum<'context>> {
        let header_type = self.coroutine_header_type();
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let i8_type = self.context.i8_type();
        // `block_on`'s coroutine is a task root.
        let parent_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_PARENT, "coro.parent.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(parent_slot, ptr_type.const_null())
            .map_err(compiler_diagnostic)?;
        let leaf_out = self
            .builder
            .build_alloca(ptr_type, "coro.leaf")
            .map_err(compiler_diagnostic)?;

        let drive = self
            .llvm_module
            .get_function("__staple_coro_drive")
            .expect("coroutine driver");
        let status = self
            .build_runtime_call(drive, &[frame.into(), leaf_out.into()], "coro.drive")?
            .try_as_basic_value()
            .unwrap_basic()
            .into_int_value();
        let suspended = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                status,
                i8_type.const_int(CORO_STATUS_DONE, false),
                "coro.suspended",
            )
            .map_err(compiler_diagnostic)?;
        self.build_trap_if(suspended, span.clone())?;

        let result_ptr_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_RESULT_PTR, "coro.result.ptr.slot")
            .map_err(compiler_diagnostic)?;
        let result_ptr = self
            .builder
            .build_load(ptr_type, result_ptr_slot, "coro.result.ptr")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let result = self
            .builder
            .build_load(result_llvm, result_ptr, "coro.result")
            .map_err(compiler_diagnostic)?;

        let cleanup_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_CLEANUP_FN, "coro.cleanup.slot")
            .map_err(compiler_diagnostic)?;
        let cleanup_ptr = self
            .builder
            .build_load(ptr_type, cleanup_slot, "coro.cleanup.fn")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let cleanup_type = self.context.void_type().fn_type(&[ptr_type.into()], false);
        self.builder
            .build_indirect_call(cleanup_type, cleanup_ptr, &[frame.into()], "")
            .map_err(compiler_diagnostic)?;

        Ok(result)
    }

    pub(crate) fn build_yield_coroutine(
        &mut self,
        span: Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let header_type = self.coroutine_header_type();
        let header_size = self.target_data.get_store_size(&header_type);
        let frame = self.build_gc_allocation(
            self.size_type.const_int(header_size, false),
            "coro.yield.frame",
            span.clone(),
        )?;
        self.builder
            .build_store(frame, header_type.const_zero())
            .map_err(compiler_diagnostic)?;
        let resume = self.declare_named_function(
            "__staple_coro_yield_resume",
            self.coroutine_status_type()
                .fn_type(&[ptr_type.into()], false),
        );
        let cleanup = self.declare_named_function(
            "__staple_coro_yield_cleanup",
            self.context.void_type().fn_type(&[ptr_type.into()], false),
        );
        for (field, value) in [
            (CORO_RESUME_FN, resume.as_global_value().as_pointer_value()),
            (
                CORO_CLEANUP_FN,
                cleanup.as_global_value().as_pointer_value(),
            ),
        ] {
            let slot = self
                .builder
                .build_struct_gep(header_type, frame, field, "coro.yield.slot")
                .map_err(compiler_diagnostic)?;
            self.builder
                .build_store(slot, value)
                .map_err(compiler_diagnostic)?;
        }
        self.register_gc_root_region(frame, header_size, span)?;
        Ok(frame)
    }

    pub(crate) fn build_coroutine_cancelled(
        &mut self,
        frame: PointerValue<'context>,
        status_type: StructType<'context>,
    ) -> CodeGenerationResult<()> {
        let header_type = self.coroutine_header_type();
        let i8_type = self.context.i8_type();
        let state_slot_unwind = self
            .builder
            .build_struct_gep(header_type, frame, CORO_STATE, "coro.state.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(state_slot_unwind, i8_type.const_int(CORO_STATE_DONE, false))
            .map_err(compiler_diagnostic)?;
        let mut cancelled_status = status_type.const_zero();
        cancelled_status = self
            .builder
            .build_insert_value(
                cancelled_status,
                i8_type.const_int(CORO_STATUS_CANCELLED, false),
                0,
                "coro.status.cancelled",
            )
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        self.builder
            .build_return(Some(&cancelled_status))
            .map_err(compiler_diagnostic)?;
        Ok(())
    }
}

impl<'program, 'context> Backend<'program, 'context> {
    pub(crate) fn build_until_coroutine(
        &mut self,
        pred_code: PointerValue<'context>,
        pred_env: PointerValue<'context>,
        scope: PointerValue<'context>,
        runner: FunctionValue<'context>,
        span: Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let i8_type = self.context.i8_type();
        let resume = self.declare_named_function(
            "__staple_until_resume",
            self.coroutine_status_type()
                .fn_type(&[ptr_type.into()], false),
        );
        let cleanup = self.declare_named_function(
            "__staple_until_cleanup",
            self.context.void_type().fn_type(&[ptr_type.into()], false),
        );

        let frame_type = self.until_frame_type();
        let frame_size = self.target_data.get_store_size(&frame_type);
        let frame = self.build_gc_allocation(
            self.size_type.const_int(frame_size, false),
            "until.frame",
            span.clone(),
        )?;
        self.builder
            .build_store(frame, frame_type.const_zero())
            .map_err(compiler_diagnostic)?;
        let field = |emitter: &Self, index: u32, value: BasicValueEnum<'context>, name: &str| {
            let slot = emitter
                .builder
                .build_struct_gep(frame_type, frame, index, name)
                .map_err(compiler_diagnostic)?;
            emitter
                .builder
                .build_store(slot, value)
                .map_err(compiler_diagnostic)?;
            Ok::<(), Diagnostic>(())
        };
        field(self, CORO_STATE, i8_type.const_zero().into(), "until.state")?;
        field(
            self,
            CORO_RESUME_FN,
            resume.as_global_value().as_pointer_value().into(),
            "until.resume",
        )?;
        field(
            self,
            CORO_CLEANUP_FN,
            cleanup.as_global_value().as_pointer_value().into(),
            "until.cleanup",
        )?;
        field(self, 13, pred_code.into(), "until.code")?;
        field(self, 14, pred_env.into(), "until.env")?;
        field(self, 15, scope.into(), "until.scope")?;
        field(
            self,
            16,
            runner.as_global_value().as_pointer_value().into(),
            "until.runner",
        )?;

        self.register_gc_root_region(frame, frame_size, span)?;
        Ok(frame)
    }
}

/// Which external record an `await` parks on (`compile_external_await`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalAwaitKind {
    Task,
    Wait,
}

impl<'program, 'context> Backend<'program, 'context> {
    pub(crate) fn build_external_await_suspend(
        &mut self,
        function: FunctionValue<'context>,
        frame: PointerValue<'context>,
        record: PointerValue<'context>,
        state: usize,
        dispatch: BasicBlock<'context>,
        status_type: StructType<'context>,
        kind: ExternalAwaitKind,
    ) -> CodeGenerationResult<()> {
        let header_type = self.coroutine_header_type();
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let i8_type = self.context.i8_type();
        // Stash the record in the (otherwise unused for an external await) child
        // slot so both the suspend-then-resume path and the already-resolved
        // fast path can recover it in the dispatch block.
        let child_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_CHILD, "coro.child.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(child_slot, record)
            .map_err(compiler_diagnostic)?;

        // Register the waiter; the runtime returns 0 when the record is already
        // resolved — an already-completed wait continues in the same resume
        // without losing a wakeup.
        let should_suspend = match kind {
            ExternalAwaitKind::Task => {
                let register = self.declare_named_function(
                    "__staple_task_await_register",
                    i8_type.fn_type(&[ptr_type.into(), ptr_type.into()], false),
                );
                self.build_runtime_call(register, &[record.into(), frame.into()], "await.suspend")?
                    .try_as_basic_value()
                    .unwrap_basic()
                    .into_int_value()
            }
            ExternalAwaitKind::Wait => {
                // Current task's scheduler: `frame->record->scheduler`, or null
                // for a `block_on` root (the runtime rejects a cross-scheduler
                // await).
                let task_record_slot = self
                    .builder
                    .build_struct_gep(header_type, frame, CORO_RECORD, "coro.record.slot")
                    .map_err(compiler_diagnostic)?;
                let task_record = self
                    .builder
                    .build_load(ptr_type, task_record_slot, "coro.record")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                let has_record = self
                    .builder
                    .build_int_compare(
                        inkwell::IntPredicate::NE,
                        task_record,
                        ptr_type.const_null(),
                        "await.has.record",
                    )
                    .map_err(compiler_diagnostic)?;
                let sched_from_record = self
                    .context
                    .append_basic_block(function, "await.sched.load");
                let sched_join = self
                    .context
                    .append_basic_block(function, "await.sched.join");
                let entry_block = self.builder.get_insert_block().expect("await block");
                self.builder
                    .build_conditional_branch(has_record, sched_from_record, sched_join)
                    .map_err(compiler_diagnostic)?;
                self.builder.position_at_end(sched_from_record);
                let sched_slot = self
                    .builder
                    .build_struct_gep(
                        self.task_record_header_type(),
                        task_record,
                        TASK_RECORD_SCHEDULER,
                        "task.record.scheduler",
                    )
                    .map_err(compiler_diagnostic)?;
                let sched_value = self
                    .builder
                    .build_load(ptr_type, sched_slot, "await.scheduler")
                    .map_err(compiler_diagnostic)?
                    .into_pointer_value();
                self.builder
                    .build_unconditional_branch(sched_join)
                    .map_err(compiler_diagnostic)?;
                self.builder.position_at_end(sched_join);
                let scheduler = self
                    .builder
                    .build_phi(ptr_type, "await.scheduler")
                    .map_err(compiler_diagnostic)?;
                scheduler.add_incoming(&[
                    (&ptr_type.const_null(), entry_block),
                    (&sched_value, sched_from_record),
                ]);
                let register = self.declare_named_function(
                    "__staple_completion_register",
                    i8_type.fn_type(&[ptr_type.into(), ptr_type.into(), ptr_type.into()], false),
                );
                self.build_runtime_call(
                    register,
                    &[
                        record.into(),
                        frame.into(),
                        scheduler.as_basic_value().into_pointer_value().into(),
                    ],
                    "await.suspend",
                )?
                .try_as_basic_value()
                .unwrap_basic()
                .into_int_value()
            }
        };
        let want_suspend = self
            .builder
            .build_int_compare(
                inkwell::IntPredicate::NE,
                should_suspend,
                i8_type.const_zero(),
                "await.want.suspend",
            )
            .map_err(compiler_diagnostic)?;
        let suspend_block = self
            .context
            .append_basic_block(function, "await.external.suspend");
        self.builder
            .build_conditional_branch(want_suspend, suspend_block, dispatch)
            .map_err(compiler_diagnostic)?;

        self.builder.position_at_end(suspend_block);
        let state_slot = self
            .builder
            .build_struct_gep(header_type, frame, CORO_STATE, "coro.state.slot")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(state_slot, i8_type.const_int(state as u64, false))
            .map_err(compiler_diagnostic)?;
        let mut status = status_type.const_zero();
        status = self
            .builder
            .build_insert_value(
                status,
                i8_type.const_int(CORO_STATUS_WAIT_EXTERNAL, false),
                0,
                "coro.status.kind",
            )
            .map_err(compiler_diagnostic)?
            .into_struct_value();
        self.builder
            .build_return(Some(&status))
            .map_err(compiler_diagnostic)?;

        // ---- resume / fast path: the record is resolved ----
        self.builder.position_at_end(dispatch);
        Ok(())
    }

    pub(crate) fn build_reaction_payload(
        &mut self,
        payload_type: StructType<'context>,
        callback: StructValue<'context>,
        resources: &[BasicMetadataValueEnum<'context>],
        span: Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let payload = self
            .builder
            .build_malloc(payload_type, "reaction.payload")
            .map_err(compiler_diagnostic)?;
        let callback_slot = self
            .builder
            .build_struct_gep(payload_type, payload, 0, "reaction.callback")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(callback_slot, callback)
            .map_err(compiler_diagnostic)?;
        for (index, resource) in resources.iter().enumerate() {
            let slot = self
                .builder
                .build_struct_gep(
                    payload_type,
                    payload,
                    (index + 1) as u32,
                    "reaction.resource",
                )
                .map_err(compiler_diagnostic)?;
            let resource = BasicValueEnum::try_from(*resource).map_err(|_| {
                Diagnostic::new(span.clone(), "reaction resource is not first-class")
            })?;
            self.builder
                .build_store(slot, resource)
                .map_err(compiler_diagnostic)?;
        }

        Ok(payload)
    }

    pub(crate) fn build_derived_payload(
        &mut self,
        payload_type: StructType<'context>,
        callback: StructValue<'context>,
        value_slot: PointerValue<'context>,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let payload = self
            .builder
            .build_malloc(payload_type, "derived.payload")
            .map_err(compiler_diagnostic)?;
        let callback_slot = self
            .builder
            .build_struct_gep(payload_type, payload, 0, "derived.callback")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(callback_slot, callback)
            .map_err(compiler_diagnostic)?;
        let output_slot = self
            .builder
            .build_struct_gep(payload_type, payload, 1, "derived.output")
            .map_err(compiler_diagnostic)?;
        self.builder
            .build_store(output_slot, value_slot)
            .map_err(compiler_diagnostic)?;

        Ok(payload)
    }

    pub(crate) fn build_batch_callback(
        &mut self,
        callback: StructValue<'context>,
        callback_type: FunctionType<'context>,
        resources: Vec<BasicMetadataValueEnum<'context>>,
    ) -> CodeGenerationResult<()> {
        let code = self
            .builder
            .build_extract_value(callback, 0, "batch.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let closure_environment = self
            .builder
            .build_extract_value(callback, 1, "batch.environment")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let mut arguments = vec![closure_environment.into()];
        arguments.extend(resources);
        self.builder
            .build_indirect_call(callback_type, code, &arguments, "batch.call")
            .map_err(compiler_diagnostic)?;
        Ok(())
    }
}

impl<'program, 'context> Backend<'program, 'context> {
    pub(crate) fn build_coroutine_cancel_teardown(
        &mut self,
        resume_fn: FunctionValue<'context>,
        frame: PointerValue<'context>,
        state: IntValue<'context>,
        wait_states: &[usize],
        until_states: &[usize],
        env_ptr: PointerValue<'context>,
        finalizer: Option<FunctionValue<'context>>,
    ) -> CodeGenerationResult<()> {
        let header_type = self.coroutine_header_type();
        let ptr_type = self.context.ptr_type(AddressSpace::default());
        let i8_type = self.context.i8_type();
        if !wait_states.is_empty() || !until_states.is_empty() {
            let abandon = self
                .context
                .append_basic_block(resume_fn, "cancel.abandon.wait");
            let child_cleanup = self
                .context
                .append_basic_block(resume_fn, "cancel.cleanup.until");
            let unwind_cells = self
                .context
                .append_basic_block(resume_fn, "cancel.unwind.cells");
            let mut cases = wait_states
                .iter()
                .map(|k| (i8_type.const_int(*k as u64, false), abandon))
                .collect::<Vec<_>>();
            cases.extend(
                until_states
                    .iter()
                    .map(|k| (i8_type.const_int(*k as u64, false), child_cleanup)),
            );
            let child_slot = self
                .builder
                .build_struct_gep(header_type, frame, CORO_CHILD, "coro.child.slot")
                .map_err(compiler_diagnostic)?;
            self.builder
                .build_switch(state, unwind_cells, &cases)
                .map_err(compiler_diagnostic)?;

            // Parked on a `Wait`: run its completion's cancellation teardown.
            self.builder.position_at_end(abandon);
            let record = self
                .builder
                .build_load(ptr_type, child_slot, "cancel.wait.record")
                .map_err(compiler_diagnostic)?;
            let abandon_fn = self.declare_named_function(
                "__staple_completion_abandon",
                self.context.void_type().fn_type(&[ptr_type.into()], false),
            );
            self.builder
                .build_direct_call(abandon_fn, &[record.into()], "")
                .map_err(compiler_diagnostic)?;
            self.builder
                .build_unconditional_branch(unwind_cells)
                .map_err(compiler_diagnostic)?;

            // Parked on a child coroutine: run its `cleanup` so its own
            // suspended state (e.g. an `until` subscription) is torn down.
            self.builder.position_at_end(child_cleanup);
            let child = self
                .builder
                .build_load(ptr_type, child_slot, "cancel.child.frame")
                .map_err(compiler_diagnostic)?
                .into_pointer_value();
            let cleanup_slot = self
                .builder
                .build_struct_gep(header_type, child, CORO_CLEANUP_FN, "child.cleanup.slot")
                .map_err(compiler_diagnostic)?;
            let cleanup_ptr = self
                .builder
                .build_load(ptr_type, cleanup_slot, "child.cleanup.fn")
                .map_err(compiler_diagnostic)?
                .into_pointer_value();
            self.builder
                .build_indirect_call(
                    self.context.void_type().fn_type(&[ptr_type.into()], false),
                    cleanup_ptr,
                    &[child.into()],
                    "",
                )
                .map_err(compiler_diagnostic)?;
            self.builder
                .build_unconditional_branch(unwind_cells)
                .map_err(compiler_diagnostic)?;

            self.builder.position_at_end(unwind_cells);
        }
        if let Some(finalizer) = finalizer {
            let never_ran = self
                .builder
                .build_int_compare(
                    inkwell::IntPredicate::EQ,
                    state,
                    i8_type.const_zero(),
                    "coro.cancel.never.ran",
                )
                .map_err(compiler_diagnostic)?;
            let drop_caps = self
                .context
                .append_basic_block(resume_fn, "cancel.drop.captures");
            let after_caps = self
                .context
                .append_basic_block(resume_fn, "cancel.after.captures");
            self.builder
                .build_conditional_branch(never_ran, drop_caps, after_caps)
                .map_err(compiler_diagnostic)?;
            self.builder.position_at_end(drop_caps);
            self.builder
                .build_direct_call(finalizer, &[env_ptr.into()], "")
                .map_err(compiler_diagnostic)?;
            self.builder
                .build_unconditional_branch(after_caps)
                .map_err(compiler_diagnostic)?;
            self.builder.position_at_end(after_caps);
        }
        Ok(())
    }
}
