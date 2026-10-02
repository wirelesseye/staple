//! Stage 5.8 Step 7: signal, derived, and reactive-call emission.
use super::*;
use crate::{
    LoweredReactiveCallbackId, LoweredReactiveOperationId, LoweredReactiveOperationKind,
    ReactiveRunnerBody, ReactiveRunnerPlan,
};
use inkwell::{types::BasicTypeEnum, values::StructValue};

impl<'program, 'context> LoweredEmitter<'program, 'context> {
    /// Legacy `signal_metadata_value`: the `%Signal*` a signal symbol's
    /// storage cell or module metadata global holds.
    pub(super) fn signal_metadata_value(
        &self,
        owner: EmissionOwner,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Option<PointerValue<'context>>> {
        if !self.view.symbol(symbol).is_some_and(|record| record.signal) {
            return Ok(None);
        }
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        let slot = if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            let cell_type = self.binding_cell_type(owner, symbol)?;
            self.backend
                .builder
                .build_struct_gep(cell_type, cell, 2, "signal.metadata")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
        } else if let Some(global) = self.signal_metadata.get(&symbol).copied() {
            global.as_pointer_value()
        } else {
            return Err(Diagnostic::new(
                span.clone(),
                "signal metadata is unavailable",
            ));
        };
        self.backend
            .builder
            .build_load(pointer_type, slot, "signal")
            .map(|value| Some(value.into_pointer_value()))
            .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))
    }

    /// Legacy `force_derived_read`: the value cell/global's metadata slot.
    fn derived_metadata_value(
        &self,
        owner: EmissionOwner,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Option<PointerValue<'context>>> {
        if !self
            .view
            .symbol(symbol)
            .is_some_and(|record| record.derived)
        {
            return Ok(None);
        }
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        let slot = if let Some(cell) = environment.binding_cells.get(&symbol).copied() {
            let cell_type = self.binding_cell_type(owner, symbol)?;
            self.backend
                .builder
                .build_struct_gep(cell_type, cell, 2, "derived.metadata")
                .map_err(|error| Diagnostic::new(span.clone(), error.to_string()))?
        } else if let Some(global) = self.derived_metadata.get(&symbol).copied() {
            global.as_pointer_value()
        } else {
            return Err(Diagnostic::new(
                span.clone(),
                "derived metadata is unavailable",
            ));
        };
        self.backend
            .builder
            .build_load(pointer_type, slot, "derived")
            .map(|value| Some(value.into_pointer_value()))
            .map_err(compiler_diagnostic)
    }

    /// Legacy `track_signal_read`: subscribe the current reaction to a signal
    /// read. A non-signal symbol tracks nothing.
    pub(super) fn track_signal_read(
        &self,
        owner: EmissionOwner,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let Some(signal) = self.signal_metadata_value(owner, environment, symbol, span)? else {
            return Ok(());
        };
        self.backend.build_reactive_runtime_call(
            "__staple_signal_track",
            &[signal.into()],
            None,
            "signal.track",
            span.clone(),
        )?;
        Ok(())
    }

    /// Legacy `force_derived_read`: recompute a stale derived binding before
    /// its value cell is read.
    pub(super) fn force_derived_read(
        &self,
        owner: EmissionOwner,
        environment: &FunctionEnvironment<'context>,
        symbol: SymbolId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let Some(derived) = self.derived_metadata_value(owner, environment, symbol, span)? else {
            return Ok(());
        };
        self.backend.build_reactive_runtime_call(
            "__staple_derived_read",
            &[derived.into()],
            None,
            "derived.read",
            staple_syntax::Span::Compiler,
        )?;
        Ok(())
    }

    /// Legacy `__staple_signal_create`: allocate a fresh signal.
    pub(super) fn emit_signal_create(
        &self,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        self.backend
            .build_reactive_runtime_call(
                "__staple_signal_create",
                &[],
                Some(
                    self.backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .into(),
                ),
                "signal.create",
                span.clone(),
            )?
            .map(|value| value.into_pointer_value())
            .ok_or_else(|| Diagnostic::new(span.clone(), "signal creation returned no value"))
    }

    /// The ambient `Reactive` scope pointer one provider supplies, loaded
    /// through its storage when the provider is indirect (legacy's
    /// `compile_reaction`/`compile_until` scope search, resolved by record).
    fn reactive_scope(
        &self,
        provider: Option<LoweredResourceProviderId>,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
        load_name: &str,
    ) -> CodeGenerationResult<PointerValue<'context>> {
        let provider = provider
            .ok_or_else(|| Diagnostic::new(span.clone(), "resource `Reactive` is not available"))?;
        let bound = environment
            .resources
            .get(&provider)
            .ok_or_else(|| Diagnostic::new(span.clone(), "resource `Reactive` is not available"))?;
        let value = value_as_basic(bound.value)
            .ok_or_else(|| Diagnostic::new(span.clone(), "reactive resource is not first-class"))?;
        let pointer = pointer_operand(value, "`Reactive` resource", span)?;
        if bound.indirect {
            let llvm_type = self.backend.compile_type(&bound.resource.value_type)?;
            let scope = self
                .backend
                .builder
                .build_load(llvm_type, pointer, load_name)
                .map_err(compiler_diagnostic)?;
            pointer_operand(scope, "`Reactive` scope", span)
        } else {
            Ok(pointer)
        }
    }

    /// One reactive callback's closure value: an explicit callable occurrence
    /// is evaluated; an implicit thunk builds its fresh capture environment
    /// and installs the recorded environment finalizer.
    fn reactive_callback_value(
        &mut self,
        owner: EmissionOwner,
        callback_id: LoweredReactiveCallbackId,
        environment: &mut FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<StructValue<'context>> {
        let callback = self
            .view
            .reactive_callback(owner, callback_id)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reactive callback record"))?
            .clone();
        if let Some(callable) = callback.callable {
            let value = self.emit_expression(owner, callable, environment)?;
            let AnyValueEnum::StructValue(closure) = value else {
                return Err(Diagnostic::new(span.clone(), "callback is not a closure"));
            };
            return Ok(closure);
        }
        if callback.thunk.is_none() {
            return Err(Diagnostic::new(
                span.clone(),
                "reactive callback has no thunk",
            ));
        }
        let binding = self
            .view
            .binding(owner, LoweredBindingSite::ReactiveCallback(callback_id))
            .ok_or_else(|| Diagnostic::new(span.clone(), "callback thunk is not bound"))?;
        let LoweredBoundTarget::Instance(instance) = binding else {
            return Err(Diagnostic::new(
                span.clone(),
                "callback thunk is not bound to an instance",
            ));
        };
        let instance = *instance;
        let function =
            self.instances.get(&instance).copied().ok_or_else(|| {
                Diagnostic::new(span.clone(), "callback instance is not declared")
            })?;
        let body = self
            .view
            .instance(instance)
            .and_then(|record| record.body.as_ref())
            .ok_or_else(|| Diagnostic::new(span.clone(), "callback instance has no body"))?;
        let pointer = self.build_capture_environment_value(owner, body, environment, span)?;
        if let Some(finalizer) = self.site_finalizer(
            owner,
            crate::ArtifactUseSite::ReactiveCallbackEnvironment(callback_id),
        ) {
            self.backend.set_gc_finalizer(pointer, finalizer)?;
        }
        self.backend.build_closure_value(function, pointer)
    }

    /// The declared runner function one reactive operation's `ReactiveRunner`
    /// use names.
    fn reactive_runner_function(
        &self,
        owner: EmissionOwner,
        operation: LoweredReactiveOperationId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<FunctionValue<'context>> {
        let ordinal = self
            .view
            .artifact_uses(owner)
            .and_then(|uses| {
                uses.iter().find_map(|use_| {
                    (use_.site == crate::ArtifactUseSite::ReactiveRunner(operation))
                        .then_some(use_.artifact)
                })
            })
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reactive runner use"))?;
        self.artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reactive runner declaration"))
    }

    /// The runner plan one reactive operation's `ReactiveRunner` use names.
    fn reactive_runner_plan(
        &self,
        owner: EmissionOwner,
        operation: LoweredReactiveOperationId,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<&'program ReactiveRunnerPlan> {
        let ordinal = self
            .view
            .artifact_uses(owner)
            .and_then(|uses| {
                uses.iter().find_map(|use_| {
                    (use_.site == crate::ArtifactUseSite::ReactiveRunner(operation))
                        .then_some(use_.artifact)
                })
            })
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reactive runner use"))?;
        let plan = self
            .view
            .artifact(ordinal)
            .and_then(|artifact| artifact.plan.as_ref())
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reactive runner plan"))?;
        match plan {
            LoweredArtifactPlan::ReactionRunner(plan)
            | LoweredArtifactPlan::UntilRunner(plan)
            | LoweredArtifactPlan::DerivedRunner(plan) => Ok(plan),
            _ => Err(Diagnostic::new(
                span.clone(),
                "reactive runner use does not name a runner",
            )),
        }
    }

    /// The ordered gap-free resource values one callback passes.
    fn callback_resources(
        &self,
        owner: EmissionOwner,
        callback: &crate::LoweredReactiveCallback,
        environment: &FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<Vec<BasicMetadataValueEnum<'context>>> {
        callback
            .resources
            .iter()
            .map(|use_id| {
                let record = self.view.resource_use(owner, *use_id).ok_or_else(|| {
                    Diagnostic::new(span.clone(), "missing callback resource use")
                })?;
                let value = self.bound_resource_value(environment, record)?;
                value_as_basic(value)
                    .map(Into::into)
                    .ok_or_else(|| Diagnostic::new(span.clone(), "resource is not first-class"))
            })
            .collect()
    }

    /// Legacy `compile_reaction`: closure, resources, ambient scope, payload
    /// with recorded slot pass modes, runner, and `__staple_reaction_create`.
    fn emit_reaction(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        operation: LoweredReactiveOperationId,
        callback: LoweredReactiveCallbackId,
        provider: Option<LoweredResourceProviderId>,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = call.origin.span.clone();
        let callback_record = self
            .view
            .reactive_callback(owner, callback)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing reaction callback"))?
            .clone();
        let closure = self.reactive_callback_value(owner, callback, environment, &span)?;
        let resources = self.callback_resources(owner, &callback_record, environment, &span)?;
        let scope = self.reactive_scope(provider, environment, &span, "reactive.resource")?;

        let plan = self.reactive_runner_plan(owner, operation, &span)?.clone();
        let ReactiveRunnerBody::Reaction {
            resources: slots, ..
        } = &plan.body
        else {
            return Err(Diagnostic::new(
                span.clone(),
                "reaction use is not a reaction runner",
            ));
        };
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        let mut fields: Vec<BasicTypeEnum<'context>> = vec![closure.get_type().into()];
        for slot in slots {
            if slot.indirect {
                fields.push(pointer_type.into());
            } else {
                fields.push(self.backend.compile_type(&slot.resource.value_type)?);
            }
        }
        let payload_type = self.backend.reaction_payload_type(&fields);
        let payload =
            self.backend
                .build_reaction_payload(payload_type, closure, &resources, span.clone())?;
        let runner = self.reactive_runner_function(owner, operation, &span)?;
        let payload_size = self.backend.size_type.const_int(
            self.backend.target_data.get_store_size(&payload_type),
            false,
        );
        self.backend.build_reactive_runtime_call(
            "__staple_reaction_create",
            &[
                scope.into(),
                runner.as_global_value().as_pointer_value().into(),
                payload.into(),
                payload_size.into(),
            ],
            Some(pointer_type.into()),
            "reaction.create",
            span,
        )?;
        Ok(self.backend.unit_value())
    }

    /// Legacy `compile_batch`: begin, indirect callback call, end.
    fn emit_batch(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        callback: LoweredReactiveCallbackId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = call.origin.span.clone();
        let callback_record = self
            .view
            .reactive_callback(owner, callback)
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing batch callback"))?
            .clone();
        let closure = self.reactive_callback_value(owner, callback, environment, &span)?;
        let resources = self.callback_resources(owner, &callback_record, environment, &span)?;
        self.backend.build_reactive_runtime_call(
            "__staple_batch_begin",
            &[],
            None,
            "batch.begin",
            span.clone(),
        )?;
        let callback_llvm = self
            .backend
            .compile_closure_function_type(&callback_record.function_type)?;
        self.backend
            .build_batch_callback(closure, callback_llvm, resources)?;
        self.backend.build_reactive_runtime_call(
            "__staple_batch_end",
            &[],
            None,
            "batch.end",
            span,
        )?;
        Ok(self.backend.unit_value())
    }

    /// Legacy `compile_until`: predicate closure, ambient scope, until runner,
    /// and the hand-written until coroutine frame.
    fn emit_until(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        operation: LoweredReactiveOperationId,
        callback: LoweredReactiveCallbackId,
        provider: Option<LoweredResourceProviderId>,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = call.origin.span.clone();
        let closure = self.reactive_callback_value(owner, callback, environment, &span)?;
        let code = self
            .backend
            .builder
            .build_extract_value(closure, 0, "until.pred.code")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let predicate_environment = self
            .backend
            .builder
            .build_extract_value(closure, 1, "until.pred.env")
            .map_err(compiler_diagnostic)?
            .into_pointer_value();
        let scope = self.reactive_scope(provider, environment, &span, "until.reactive")?;
        let runner = self.reactive_runner_function(owner, operation, &span)?;
        Ok(self
            .backend
            .build_until_coroutine(code, predicate_environment, scope, runner, span)?
            .as_any_value_enum())
    }

    /// Legacy `compile_intrinsic_call`'s `Snapshot`: evaluate the operand
    /// between a tracking suspend and restore.
    pub(super) fn emit_snapshot(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let span = call.origin.span.clone();
        let expression = call
            .arguments
            .first()
            .and_then(|argument| argument.expression)
            .ok_or_else(|| Diagnostic::new(span.clone(), "`snapshot` has no operand"))?;
        let previous = self
            .backend
            .build_reactive_runtime_call(
                "__staple_tracking_suspend",
                &[],
                Some(
                    self.backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .into(),
                ),
                "tracking.suspend",
                span.clone(),
            )?
            .ok_or_else(|| Diagnostic::new(span.clone(), "tracking suspend returned no value"))?
            .into_pointer_value();
        let value = self.emit_expression(owner, expression, environment)?;
        self.backend.build_reactive_runtime_call(
            "__staple_tracking_restore",
            &[previous.into()],
            None,
            "tracking.restore",
            span,
        )?;
        Ok(value)
    }

    /// One reactive intrinsic call's emission, dispatched by the recorded
    /// operation kind.
    pub(super) fn emit_reactive_call(
        &mut self,
        owner: EmissionOwner,
        call: &crate::LoweredCall,
        operation: LoweredReactiveOperationId,
        environment: &mut FunctionEnvironment<'context>,
    ) -> CodeGenerationResult<AnyValueEnum<'context>> {
        let kind = self
            .view
            .reactive_operation(owner, operation)
            .map(|record| record.kind.clone())
            .ok_or_else(|| {
                Diagnostic::new(
                    call.origin.span.clone(),
                    "missing reactive operation record",
                )
            })?;
        match kind {
            LoweredReactiveOperationKind::Reaction {
                callback,
                reactive_provider,
            } => self.emit_reaction(
                owner,
                call,
                operation,
                callback,
                reactive_provider,
                environment,
            ),
            LoweredReactiveOperationKind::Batch { callback } => {
                self.emit_batch(owner, call, callback, environment)
            }
            LoweredReactiveOperationKind::Until {
                predicate,
                reactive_provider,
            } => self.emit_until(
                owner,
                call,
                operation,
                predicate,
                reactive_provider,
                environment,
            ),
            LoweredReactiveOperationKind::Snapshot => self.emit_snapshot(owner, call, environment),
            other => Err(Diagnostic::new(
                call.origin.span.clone(),
                format!("internal invariant: {other:?} is not a reactive call"),
            )),
        }
    }

    /// Stage 5.8 Step 7: legacy `compile_derived_create`. The evaluator
    /// closure carries its recorded environment finalizer; the runner body is
    /// emitted with the artifact.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_derived_create(
        &mut self,
        owner: EmissionOwner,
        operation: LoweredReactiveOperationId,
        value_slot: PointerValue<'context>,
        metadata_slot: PointerValue<'context>,
        environment: &mut FunctionEnvironment<'context>,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let function_type = self
            .view
            .reactive_operation(owner, operation)
            .and_then(|record| match &record.kind {
                LoweredReactiveOperationKind::DerivedCreate { function_type, .. } => {
                    Some(function_type.clone())
                }
                _ => None,
            })
            .ok_or_else(|| Diagnostic::new(span.clone(), "derived evaluator is unavailable"))?;
        if !function_type.effects.resources.is_empty() {
            return Err(Diagnostic::new(
                span.clone(),
                "derived evaluators cannot capture resources",
            ));
        }
        let binding = self
            .view
            .binding(owner, LoweredBindingSite::DerivedEvaluator(operation))
            .ok_or_else(|| Diagnostic::new(span.clone(), "derived evaluator is not bound"))?;
        let LoweredBoundTarget::Instance(instance) = binding else {
            return Err(Diagnostic::new(
                span.clone(),
                "derived evaluator is not bound to an instance",
            ));
        };
        let instance = *instance;
        let function =
            self.instances.get(&instance).copied().ok_or_else(|| {
                Diagnostic::new(span.clone(), "derived evaluator is not declared")
            })?;
        let body = self
            .view
            .instance(instance)
            .and_then(|record| record.body.as_ref())
            .ok_or_else(|| Diagnostic::new(span.clone(), "derived evaluator has no body"))?;
        let pointer = self.build_capture_environment_value(owner, body, environment, span)?;
        if let Some(finalizer) = self.site_finalizer(
            owner,
            crate::ArtifactUseSite::DerivedEvaluatorEnvironment(operation),
        ) {
            self.backend.set_gc_finalizer(pointer, finalizer)?;
        }
        let callback = self.backend.build_closure_value(function, pointer)?;
        let payload_type = self
            .backend
            .derived_payload_type(self.backend.closure_type());
        let payload = self
            .backend
            .build_derived_payload(payload_type, callback, value_slot)?;
        let runner = self.reactive_runner_function(owner, operation, span)?;
        let payload_size = self.backend.size_type.const_int(
            self.backend.target_data.get_store_size(&payload_type),
            false,
        );
        let derived = self
            .backend
            .build_reactive_runtime_call(
                "__staple_derived_create",
                &[
                    runner.as_global_value().as_pointer_value().into(),
                    payload.into(),
                    payload_size.into(),
                ],
                Some(
                    self.backend
                        .context
                        .ptr_type(AddressSpace::default())
                        .into(),
                ),
                "derived.create",
                span.clone(),
            )?
            .ok_or_else(|| Diagnostic::new(span.clone(), "derived creation returned no value"))?
            .into_pointer_value();
        self.backend
            .builder
            .build_store(metadata_slot, derived)
            .map(|_| ())
            .map_err(compiler_diagnostic)
    }

    /// Stage 5.8 Step 7: one runner artifact body from its plan.
    pub(super) fn emit_runner_body(
        &mut self,
        ordinal: ArtifactOrdinal,
        plan: &ReactiveRunnerPlan,
        span: &staple_syntax::Span,
    ) -> CodeGenerationResult<()> {
        let runner = self
            .artifacts
            .get(&ordinal)
            .and_then(|functions| functions.first())
            .copied()
            .ok_or_else(|| Diagnostic::new(span.clone(), "missing runner declaration"))?;
        let pointer_type = self.backend.context.ptr_type(AddressSpace::default());
        match &plan.body {
            ReactiveRunnerBody::Reaction {
                callback_type,
                resources,
            } => {
                let callback = self.backend.closure_type();
                let mut fields: Vec<BasicTypeEnum<'context>> = vec![callback.into()];
                for slot in resources {
                    if slot.indirect {
                        fields.push(pointer_type.into());
                    } else {
                        fields.push(self.backend.compile_type(&slot.resource.value_type)?);
                    }
                }
                let payload_type = self.backend.reaction_payload_type(&fields);
                let closure_type = self.backend.compile_closure_function_type(callback_type)?;
                self.backend.build_reaction_runner(
                    runner,
                    payload_type,
                    callback,
                    &fields[1..],
                    closure_type,
                )
            }
            ReactiveRunnerBody::Until { predicate_type } => {
                let predicate = self.backend.compile_closure_function_type(predicate_type)?;
                self.backend.build_until_runner(runner, predicate)
            }
            ReactiveRunnerBody::Derived { evaluator_type, .. } => {
                let callback = self.backend.closure_type();
                let payload_type = self.backend.derived_payload_type(callback);
                let evaluator = self.backend.compile_closure_function_type(evaluator_type)?;
                self.backend.build_derived_runner(
                    runner,
                    payload_type,
                    callback,
                    evaluator,
                    span.clone(),
                )
            }
            ReactiveRunnerBody::Unexpanded => Err(Diagnostic::new(
                span.clone(),
                "reactive runner plan was never expanded",
            )),
        }
    }
}
