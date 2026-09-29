//! Stage 4.5: the coroutine-codes and reactive-runner artifact expanders and
//! scanner.
//!
//! `expand_coroutine_codes` fills one `CoroutineCodes` plan from the body
//! thunk's **instance-local** plan and body, never from the template plan the
//! `LoweredCoro` indexes and never from `TypedModule`: the frame result and
//! await types, the resume-state count, the `Wait`/`until` cancellation
//! states, the deferred-resource bundle with its pass modes, the ordered
//! concrete captures, the frame-binding unwind drops, and the thunk's
//! environment finalizer.
//!
//! `expand_reactive_runner` fills one `ReactionRunner`/`UntilRunner`/
//! `DerivedRunner` plan from the owner's lowered operation and callback
//! records: the callback's concrete closure type, the ordered resource slots
//! with their pass modes, the `until` predicate type, and the derived
//! evaluator's signature and output type.
//!
//! The scanner walks one owner in lowered evaluation order through the shared
//! Stage 4.4 owner walker. It requests one pair per `coro` creation
//! (`CoroCreation`), the environment finalizer a thunk callback installs
//! (`ReactiveCallbackEnvironment`/`DerivedEvaluatorEnvironment`, gap 1), and
//! one runner per reaction, `until`, and derived operation (`ReactiveRunner`).
//! Instance owners take their thunk instances from their own bindings;
//! initializer owners resolve with the Stage 3.3 recipe, and a key that was
//! never interned is a diagnostic rather than a silent skip.

use staple_syntax::Diagnostic;

use super::artifact_closure::{ArtifactUseSite, ClosureRequest, ExpansionResult, ScanResult};
use super::cleanup_artifacts::{LoweredOwnerVisitor, OwnerArenas, walk_owner};
use super::instance_resolution::{InstanceResolutionRequest, InstanceResolutionTarget};
use super::{
    ArenaId, CallSubstitutions, CoroutineCodesPlan, CoroutineFrameBinding, CoroutineFramePlan,
    CoroutineResourceSlot, FunctionId, FunctionInstanceId, GcFinalizerPlan, InitializerId,
    LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId,
    LoweredBindingSite, LoweredBoundTarget, LoweredCoroId, LoweredCoroutinePlan, LoweredProgram,
    LoweredReactiveCallbackId, LoweredReactiveOperationId, LoweredReactiveOperationKind, Origin,
    PlannedArtifact, ReactiveRunnerBody, ReactiveRunnerPlan, RunnerResourceSlot,
};
use crate::specialization::{
    ArtifactRequestKey, ArtifactSite, ArtifactSiteOwner, CanonicalType, CoroutineCodesKey,
    GcFinalizerKey, ReactiveRunnerKey,
};
use crate::{CheckedFunctionType, CheckedResource, CheckedType};

/// Expands one coroutine pair: the frame facts and planned callees the
/// resume/cleanup pair mirrors, read from the body thunk's own instance.
pub(super) fn expand_coroutine_codes(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: CoroutineCodesPlan,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                staple_syntax::Span::Compiler,
                "coroutine-codes expansion received a missing artifact".to_string(),
            )]);
        }
    };
    let Some(instance) = program.instances.get(plan.body) else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "coroutine-codes plan names missing body instance {}",
                plan.body.index()
            ),
        )]);
    };
    let is_coroutine_body = program
        .functions
        .get(instance.template)
        .is_some_and(|function| function.class.coroutine_body);
    if !is_coroutine_body {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            "coroutine-codes plan names an instance that is not a coroutine body thunk".to_string(),
        )]);
    }
    let Some(body) = instance.body.as_ref() else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "coroutine body instance {} has no materialized body",
                plan.body.index()
            ),
        )]);
    };
    let Some(local_id) = body.plan_template else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "coroutine body instance {} owns no local plan",
                plan.body.index()
            ),
        )]);
    };
    let Some(local) = body.plan(local_id) else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "coroutine body instance {} has no plan at its template position",
                plan.body.index()
            ),
        )]);
    };
    let ordinal = instance.ordinal;
    let mut requests = Vec::new();

    // The thunk's captures come from its own body, not from the pair key.
    let captures = body
        .captures()
        .iter()
        .map(|capture| capture.value_type.clone())
        .collect::<Vec<_>>();

    // Legacy gates the environment finalizer on non-empty captures, not on the
    // closure install gate, so a legitimately requested finalizer may drop
    // nothing.
    let capture_finalizer = if captures.is_empty() {
        None
    } else {
        let canonical_captures = captures
            .iter()
            .map(|capture| CanonicalType::concrete(capture, &origin))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|diagnostic| vec![diagnostic])?;
        let key = ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
            closure: ordinal,
            captures: canonical_captures,
        });
        requests.push(ClosureRequest::Artifact {
            key: key.clone(),
            plan: LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::ClosureEnvironment {
                closure: plan.body,
                captures: captures.clone(),
                drops: None,
            }),
            kind: LoweredArtifactDependencyKind::GcFinalizer,
            origin: origin.clone(),
            use_site: None,
        });
        Some(PlannedArtifact {
            key,
            artifact: None,
            kind: LoweredArtifactDependencyKind::GcFinalizer,
        })
    };

    // Frame-binding unwind drops follow the capture finalizer in plan order,
    // which is frame cell order. Legacy iterates a `HashMap`, so plan order is
    // the deterministic choice the transition test compares as a set.
    let mut frame_bindings = Vec::new();
    for symbol in &local.frame_bindings {
        let Some(value_type) = frame_binding_type(program, body, *symbol) else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                format!(
                    "coroutine frame binding symbol {} has no concrete type in its body",
                    symbol.0
                ),
            )]);
        };
        let unwind_drop = if program.concrete_needs_drop(&value_type) {
            Some(super::cleanup_artifacts::request_drop_glue(
                program,
                &value_type,
                &origin,
                &mut requests,
            )?)
        } else {
            None
        };
        frame_bindings.push(CoroutineFrameBinding {
            symbol: *symbol,
            value_type,
            unwind_drop,
        });
    }

    let resources = local
        .deferred_effects
        .resources
        .iter()
        .map(|resource| CoroutineResourceSlot {
            resource: resource.clone(),
            indirect: resource.mutable || !program.concrete_is_copy(&resource.value_type),
        })
        .collect();

    let frame = CoroutineFramePlan {
        result_type: local.result_type.clone(),
        resume_points: local.resume_points,
        frame_bindings,
        await_result_types: local.await_result_types.clone(),
        wait_await_states: local.wait_await_states.clone(),
        until_await_states: local.until_await_states.clone(),
        resources,
        captures,
        capture_finalizer,
    };
    Ok((
        LoweredArtifactPlan::CoroutineCodes(CoroutineCodesPlan {
            body: plan.body,
            frame: Some(frame),
        }),
        requests,
    ))
}

/// The concrete type of one frame-binding symbol.
///
/// `coroutine_lower` collects frame bindings through call arguments, so a
/// binding inside an implicit-thunk argument (a reaction, batch, or `until`
/// block, a derived initializer, or a block argument) is a frame binding too:
/// legacy lays out a frame cell for it and conditionally drops that
/// never-initialized cell on the cancel unwind. The symbol is bound in the
/// nested thunk's own instance, not in the coroutine body, so the search
/// descends through the bound implicit-thunk instances, the same nesting
/// `coroutine_lower` recurses through, and stops at nested coroutine bodies.
fn frame_binding_type(
    program: &LoweredProgram,
    body: &super::LoweredInstanceBody,
    symbol: super::SymbolId,
) -> Option<CheckedType> {
    if let Some(value_type) = body.binding_symbol_type(symbol) {
        return Some(value_type.clone());
    }
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![body];
    while let Some(current) = pending.pop() {
        for target in current.bindings.values() {
            let LoweredBoundTarget::Instance(instance) = target else {
                continue;
            };
            if !seen.insert(*instance) {
                continue;
            }
            let Some(record) = program.instances.get(*instance) else {
                continue;
            };
            let nested_thunk = program
                .functions
                .get(record.template)
                .is_some_and(|function| {
                    function.class.implicit_thunk && !function.class.coroutine_body
                });
            if !nested_thunk {
                continue;
            }
            let Some(nested) = record.body.as_ref() else {
                continue;
            };
            // A symbol is bound by exactly one template, so the first nested
            // thunk that binds it is its owner.
            if let Some(value_type) = nested.binding_symbol_type(symbol) {
                return Some(value_type.clone());
            }
            pending.push(nested);
        }
    }
    None
}

/// The runner family an expansion or request belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ReactiveRunnerFamily {
    Reaction,
    Until,
    Derived,
}

/// Expands one reactive runner: the callback call shape the legacy runner
/// embeds, read from the owner's lowered operation and callback records.
pub(super) fn expand_reactive_runner(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: ReactiveRunnerPlan,
    family: ReactiveRunnerFamily,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                staple_syntax::Span::Compiler,
                "reactive-runner expansion received a missing artifact".to_string(),
            )]);
        }
    };
    let owner = match plan.owner {
        ArtifactSiteOwner::Initializer(initializer) => {
            if program.initializers.get(initializer).is_none() {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "reactive runner names missing initializer {}",
                        initializer.index()
                    ),
                )]);
            }
            OwnerArenas::Initializer(initializer)
        }
        ArtifactSiteOwner::Instance(ordinal) => {
            let Some((_, instance)) = program
                .instances
                .iter()
                .find(|(_, instance)| instance.ordinal == ordinal)
            else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "reactive runner names missing instance ordinal {}",
                        ordinal.index()
                    ),
                )]);
            };
            let Some(body) = instance.body.as_ref() else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "reactive runner names instance {} with no materialized body",
                        ordinal.index()
                    ),
                )]);
            };
            OwnerArenas::Instance(body)
        }
        ArtifactSiteOwner::Artifact(_) => {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                "reactive runners are never owned by generated artifacts".to_string(),
            )]);
        }
    };
    let body = match family {
        ReactiveRunnerFamily::Reaction => {
            let ArtifactSite::Callback(callback) = plan.site else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "a reaction runner site is not a callback".to_string(),
                )]);
            };
            if !operation_references(
                program,
                owner,
                |kind| matches!(kind, LoweredReactiveOperationKind::Reaction { callback: id, .. } if *id == callback),
            ) {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "a reaction runner site has no matching reaction operation in its owner"
                        .to_string(),
                )]);
            }
            let Some(record) = owner.reactive_callback(program, callback) else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "a reaction runner names a missing callback".to_string(),
                )]);
            };
            let callback_type = record.function_type.clone();
            let resources = callback_type
                .effects
                .resources
                .iter()
                .map(|resource| runner_resource_slot(program, resource))
                .collect();
            ReactiveRunnerBody::Reaction {
                callback_type,
                resources,
            }
        }
        ReactiveRunnerFamily::Until => {
            let ArtifactSite::Callback(predicate) = plan.site else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "an `until` runner site is not a callback".to_string(),
                )]);
            };
            if !operation_references(
                program,
                owner,
                |kind| matches!(kind, LoweredReactiveOperationKind::Until { predicate: id, .. } if *id == predicate),
            ) {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "an `until` runner site has no matching `until` operation in its owner"
                        .to_string(),
                )]);
            }
            let Some(record) = owner.reactive_callback(program, predicate) else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "an `until` runner names a missing predicate".to_string(),
                )]);
            };
            let predicate_type = record.function_type.clone();
            // `Bool` is the sum carrying the `True` alternative; the runner
            // branches on the result's tag, so a non-`Bool` predicate is a
            // requester error. Names may be qualified in a module scope, so
            // the check matches the last name component like `lower_logical`.
            if !is_bool_type(&predicate_type.result) {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "an `until` predicate must return `Bool`, found `{}`",
                        predicate_type.result
                    ),
                )]);
            }
            ReactiveRunnerBody::Until { predicate_type }
        }
        ReactiveRunnerFamily::Derived => {
            let ArtifactSite::Operation(operation) = plan.site else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "a derived runner site is not an operation".to_string(),
                )]);
            };
            let Some(record) = owner.reactive_operation(program, operation) else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "a derived runner names a missing operation".to_string(),
                )]);
            };
            let LoweredReactiveOperationKind::DerivedCreate {
                evaluator,
                function_type,
                ..
            } = &record.kind
            else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "a derived runner site is not a derived creation".to_string(),
                )]);
            };
            let instance = resolve_thunk_instance(
                program,
                owner,
                LoweredBindingSite::DerivedEvaluator(operation),
                *evaluator,
                function_type.clone(),
                &origin,
            )?;
            let Some(evaluator_body) = program
                .instances
                .get(instance)
                .and_then(|record| record.body.as_ref())
            else {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "the derived evaluator instance {} has no materialized body",
                        instance.index()
                    ),
                )]);
            };
            let evaluator_type = evaluator_body.signature().clone();
            // The legacy proof rejects an evaluator that captures resources:
            // the runner passes only the environment to the indirect call.
            if !evaluator_type.effects.resources.is_empty() {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    "derived evaluators cannot capture resources".to_string(),
                )]);
            }
            let output_type = evaluator_type.result.as_ref().clone();
            ReactiveRunnerBody::Derived {
                evaluator_type,
                output_type,
            }
        }
    };
    let plan = match (family, plan) {
        (ReactiveRunnerFamily::Reaction, ReactiveRunnerPlan { owner, site, .. }) => {
            LoweredArtifactPlan::ReactionRunner(ReactiveRunnerPlan { owner, site, body })
        }
        (ReactiveRunnerFamily::Until, ReactiveRunnerPlan { owner, site, .. }) => {
            LoweredArtifactPlan::UntilRunner(ReactiveRunnerPlan { owner, site, body })
        }
        (ReactiveRunnerFamily::Derived, ReactiveRunnerPlan { owner, site, .. }) => {
            LoweredArtifactPlan::DerivedRunner(ReactiveRunnerPlan { owner, site, body })
        }
    };
    Ok((plan, Vec::new()))
}

/// Whether `value_type` is the `Bool` sum, directly or through its distinct
/// representation. Alternative names may be qualified, so the last component
/// is compared.
fn is_bool_type(value_type: &CheckedType) -> bool {
    fn alternative_is_true(alternative: &CheckedType) -> bool {
        let name = match alternative {
            CheckedType::Distinct { name, .. }
            | CheckedType::Opaque { name, .. }
            | CheckedType::TypeConstructor { name, .. } => name,
            _ => return false,
        };
        name == "True" || name.ends_with(".True")
    }
    match value_type {
        CheckedType::Sum(sum) => sum.alternatives.iter().any(alternative_is_true),
        CheckedType::Distinct { representation, .. } => is_bool_type(representation),
        _ => false,
    }
}

/// One reaction payload slot: a pointer when `mutable || !concrete_is_copy`.
fn runner_resource_slot(
    program: &LoweredProgram,
    resource: &CheckedResource,
) -> RunnerResourceSlot {
    RunnerResourceSlot {
        resource: resource.clone(),
        indirect: resource.mutable || !program.concrete_is_copy(&resource.value_type),
    }
}

/// Whether one operation in the owner's arenas satisfies `matches`.
fn operation_references(
    program: &LoweredProgram,
    owner: OwnerArenas<'_>,
    matches: impl Fn(&LoweredReactiveOperationKind) -> bool,
) -> bool {
    match owner {
        OwnerArenas::Instance(body) => body
            .reactive_operations
            .iter()
            .any(|(_, operation)| matches(&operation.kind)),
        OwnerArenas::Initializer(_) => program
            .reactive_operations
            .iter()
            .any(|(_, operation)| matches(&operation.kind)),
    }
}

/// The body instance of an initializer-owned `coro` creation: the Stage 3.3
/// recipe with the plan's deferred effects. A key that was never interned is a
/// diagnostic rather than a silent skip.
fn initializer_body_instance(
    program: &LoweredProgram,
    plan: &LoweredCoroutinePlan,
    origin: &Origin,
) -> Result<FunctionInstanceId, Vec<Diagnostic>> {
    let Some(mut function_type) = program
        .functions
        .get(plan.thunk)
        .map(|function| function.signature.clone())
    else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            "coroutine body thunk has no lowered template".to_string(),
        )]);
    };
    function_type.effects = plan.deferred_effects.clone();
    let resolved = program
        .resolve_instance_request(&InstanceResolutionRequest {
            function: plan.thunk,
            origin: origin.clone(),
            function_type,
            substitutions: CallSubstitutions::default(),
            evidence: None,
            target: InstanceResolutionTarget::Root,
        })
        .map_err(|diagnostic| vec![diagnostic])?;
    let Some(ordinal) = program.specializations.instance_ordinal(&resolved.key) else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            "coro body thunk instance was never interned".to_string(),
        )]);
    };
    Ok(FunctionInstanceId::from_index(ordinal.index()))
}

/// The thunk instance for one callback or evaluator. An instance owner already
/// binds it at the operation site; an initializer owner has no binding table,
/// so the thunk is resolved with the Stage 3.3 recipe, and a key that was
/// never interned is a diagnostic rather than a silent skip.
fn resolve_thunk_instance(
    program: &LoweredProgram,
    owner: OwnerArenas<'_>,
    binding: LoweredBindingSite,
    function: FunctionId,
    function_type: CheckedFunctionType,
    origin: &Origin,
) -> Result<FunctionInstanceId, Vec<Diagnostic>> {
    if let OwnerArenas::Instance(body) = owner
        && let Some(LoweredBoundTarget::Instance(instance)) = body.binding(binding)
    {
        return Ok(*instance);
    }
    let resolved = program
        .resolve_instance_request(&InstanceResolutionRequest {
            function,
            origin: origin.clone(),
            function_type,
            substitutions: CallSubstitutions::default(),
            evidence: None,
            target: InstanceResolutionTarget::Root,
        })
        .map_err(|diagnostic| vec![diagnostic])?;
    let Some(ordinal) = program.specializations.instance_ordinal(&resolved.key) else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "reactive thunk function {} was never interned for this owner",
                function.0
            ),
        )]);
    };
    Ok(FunctionInstanceId::from_index(ordinal.index()))
}

/// Validates the Stage 4.5 plans and creation uses:
///
/// - every expanded pair re-expands to itself from its body instance, so a
///   plan whose body is not a coroutine body thunk or whose frame facts
///   disagree with the body's local plan is rejected;
/// - every expanded runner re-expands to itself from its owner's operation
///   and callback records, so a site that does not resolve to a matching
///   reactive operation kind is rejected;
/// - every `CoroCreation` use names the same body instance its
///   `LoweredBindingSite::Coro` binding (or initializer recipe) resolves to.
pub(super) fn check_stage_4_5(program: &LoweredProgram, diagnostics: &mut Vec<Diagnostic>) {
    for (id, artifact) in program.artifacts.iter() {
        let Some(plan) = artifact.plan.clone() else {
            continue;
        };
        match plan {
            LoweredArtifactPlan::CoroutineCodes(plan) => {
                if plan.frame.is_none() {
                    continue; // A registered expander rejects markers itself.
                }
                match expand_coroutine_codes(program, id, plan.clone()) {
                    Ok((rebuilt, _)) => {
                        if !rebuilt.eq_ignoring_bindings(&LoweredArtifactPlan::CoroutineCodes(plan))
                        {
                            diagnostics.push(Diagnostic::new(
                                artifact.origin.span.clone(),
                                format!(
                                    "coroutine-codes artifact {} disagrees with its body instance's local plan",
                                    id.index()
                                ),
                            ));
                        }
                    }
                    Err(mut problems) => diagnostics.append(&mut problems),
                }
            }
            LoweredArtifactPlan::ReactionRunner(plan) => {
                if matches!(plan.body, ReactiveRunnerBody::Unexpanded) {
                    continue;
                }
                match expand_reactive_runner(program, id, plan, ReactiveRunnerFamily::Reaction) {
                    Ok((rebuilt, _)) => {
                        if !rebuilt.eq_ignoring_bindings(&plan_original(program, id)) {
                            diagnostics.push(Diagnostic::new(
                                artifact.origin.span.clone(),
                                format!(
                                    "reaction-runner artifact {} disagrees with its owner's operation",
                                    id.index()
                                ),
                            ));
                        }
                    }
                    Err(mut problems) => diagnostics.append(&mut problems),
                }
            }
            LoweredArtifactPlan::UntilRunner(plan) => {
                if matches!(plan.body, ReactiveRunnerBody::Unexpanded) {
                    continue;
                }
                match expand_reactive_runner(program, id, plan, ReactiveRunnerFamily::Until) {
                    Ok((rebuilt, _)) => {
                        if !rebuilt.eq_ignoring_bindings(&plan_original(program, id)) {
                            diagnostics.push(Diagnostic::new(
                                artifact.origin.span.clone(),
                                format!(
                                    "until-runner artifact {} disagrees with its owner's operation",
                                    id.index()
                                ),
                            ));
                        }
                    }
                    Err(mut problems) => diagnostics.append(&mut problems),
                }
            }
            LoweredArtifactPlan::DerivedRunner(plan) => {
                if matches!(plan.body, ReactiveRunnerBody::Unexpanded) {
                    continue;
                }
                match expand_reactive_runner(program, id, plan, ReactiveRunnerFamily::Derived) {
                    Ok((rebuilt, _)) => {
                        if !rebuilt.eq_ignoring_bindings(&plan_original(program, id)) {
                            diagnostics.push(Diagnostic::new(
                                artifact.origin.span.clone(),
                                format!(
                                    "derived-runner artifact {} disagrees with its owner's operation",
                                    id.index()
                                ),
                            ));
                        }
                    }
                    Err(mut problems) => diagnostics.append(&mut problems),
                }
            }
            _ => {}
        }
    }
    check_creation_uses(program, diagnostics);
}

/// The stored plan of one artifact, for the re-expansion comparison.
fn plan_original(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
) -> LoweredArtifactPlan {
    program
        .artifacts
        .get(artifact)
        .and_then(|record| record.plan.clone())
        .unwrap_or_else(|| panic!("artifact {} has a plan while validating", artifact.index()))
}

/// Every `CoroCreation` use names the body instance its site resolves to.
fn check_creation_uses(program: &LoweredProgram, diagnostics: &mut Vec<Diagnostic>) {
    for (_, instance) in program.instances.iter() {
        let Some(body) = instance.body.as_ref() else {
            continue;
        };
        for use_ in &body.artifact_uses {
            let ArtifactUseSite::CoroCreation(coro) = use_.site else {
                continue;
            };
            let Some(ArtifactRequestKey::CoroutineCodes(key)) =
                program.specializations.artifact(use_.artifact)
            else {
                continue; // The key agreement check reports a mismatched key.
            };
            let resolved = match body.binding(LoweredBindingSite::Coro(coro)) {
                Some(LoweredBoundTarget::Instance(instance)) => Some(*instance),
                _ => None,
            };
            let Some(resolved) = resolved else {
                diagnostics.push(Diagnostic::new(
                    use_.origin.span.clone(),
                    "a coro creation use has no bound body instance".to_string(),
                ));
                continue;
            };
            let ordinal = program.instances.get(resolved).map(|record| record.ordinal);
            if ordinal != Some(key.body) {
                diagnostics.push(Diagnostic::new(
                    use_.origin.span.clone(),
                    "a coro creation use names a different body instance than its binding"
                        .to_string(),
                ));
            }
        }
    }

    for (index, _) in program.initializers.iter() {
        let Some(uses) = program.initializer_artifact_uses.get(index.index()) else {
            continue;
        };
        for use_ in uses {
            let ArtifactUseSite::CoroCreation(coro) = use_.site else {
                continue;
            };
            let Some(ArtifactRequestKey::CoroutineCodes(key)) =
                program.specializations.artifact(use_.artifact)
            else {
                continue;
            };
            let Some(coro) = program.coros.get(coro) else {
                continue;
            };
            let Some(plan) = program.coroutine_plans.get(coro.plan) else {
                continue;
            };
            match initializer_body_instance(program, plan, &use_.origin) {
                Ok(resolved) => {
                    let ordinal = program.instances.get(resolved).map(|record| record.ordinal);
                    if ordinal != Some(key.body) {
                        diagnostics.push(Diagnostic::new(
                            use_.origin.span.clone(),
                            "a coro creation use names a different body instance than its thunk"
                                .to_string(),
                        ));
                    }
                }
                Err(mut problems) => diagnostics.append(&mut problems),
            }
        }
    }
}

/// Scans one materialized instance body for `coro` creations and reactive
/// operations.
pub(super) fn scan_instance(program: &LoweredProgram, instance: FunctionInstanceId) -> ScanResult {
    let Some(record) = program.instances.get(instance) else {
        return Ok(Vec::new());
    };
    let Some(body) = record.body.as_ref() else {
        return Ok(Vec::new());
    };
    scan_owner(
        program,
        OwnerArenas::Instance(body),
        ArtifactSiteOwner::Instance(record.ordinal),
    )
}

/// Scans one module initializer for `coro` creations and reactive operations.
pub(super) fn scan_initializer(program: &LoweredProgram, initializer: InitializerId) -> ScanResult {
    if program.initializers.get(initializer).is_none() {
        return Ok(Vec::new());
    }
    scan_owner(
        program,
        OwnerArenas::Initializer(initializer),
        ArtifactSiteOwner::Initializer(initializer),
    )
}

fn scan_owner(
    program: &LoweredProgram,
    owner: OwnerArenas<'_>,
    site_owner: ArtifactSiteOwner,
) -> ScanResult {
    let mut visitor = Stage45ScanVisitor {
        program,
        owner,
        site_owner,
        requests: Vec::new(),
    };
    walk_owner(program, owner, &mut visitor)?;
    Ok(visitor.requests)
}

/// The scanning visitor: every `coro` creation, installed callback
/// environment, and reactive runner becomes a closure request with its exact
/// use site.
struct Stage45ScanVisitor<'a> {
    program: &'a LoweredProgram,
    owner: OwnerArenas<'a>,
    site_owner: ArtifactSiteOwner,
    requests: Vec<ClosureRequest>,
}

impl Stage45ScanVisitor<'_> {
    /// The pair's body instance. An instance owner already binds it at the
    /// creation site; an initializer owner has no binding table, so the thunk
    /// is resolved with the Stage 3.3 recipe, and a key that was never
    /// interned is a diagnostic.
    fn body_instance(
        &self,
        id: LoweredCoroId,
        plan: &LoweredCoroutinePlan,
        origin: &Origin,
    ) -> Result<FunctionInstanceId, Vec<Diagnostic>> {
        if let OwnerArenas::Instance(body) = self.owner
            && let Some(LoweredBoundTarget::Instance(instance)) =
                body.binding(LoweredBindingSite::Coro(id))
        {
            return Ok(*instance);
        }
        initializer_body_instance(self.program, plan, origin)
    }

    fn request_pair(&mut self, id: LoweredCoroId, origin: &Origin) -> Result<(), Vec<Diagnostic>> {
        let Some(coro) = self.owner.coro(self.program, id) else {
            return Ok(());
        };
        let Some(plan) = self.program.coroutine_plans.get(coro.plan) else {
            return Ok(());
        };
        let body = self.body_instance(id, plan, origin)?;
        let Some(instance) = self.program.instances.get(body) else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                format!("coro creation binds missing body instance {}", body.index()),
            )]);
        };
        self.requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::CoroutineCodes(CoroutineCodesKey {
                body: instance.ordinal,
            }),
            plan: LoweredArtifactPlan::CoroutineCodes(CoroutineCodesPlan { body, frame: None }),
            kind: LoweredArtifactDependencyKind::CoroutineCodes,
            origin: origin.clone(),
            use_site: Some(ArtifactUseSite::CoroCreation(id)),
        });
        Ok(())
    }

    /// Requests the closure-environment finalizer a thunk callback or evaluator
    /// installs, gated exactly as the 4.4 closure scanner: some capture that
    /// neither requires initialization state nor is borrowed has a droppable
    /// concrete type.
    fn request_environment(
        &mut self,
        instance: FunctionInstanceId,
        site: ArtifactUseSite,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        let Some(record) = self.program.instances.get(instance) else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                format!("reactive thunk instance {} is missing", instance.index()),
            )]);
        };
        let Some(body) = record.body.as_ref() else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                format!(
                    "reactive thunk instance {} has no materialized body",
                    instance.index()
                ),
            )]);
        };
        let captures = body.captures();
        let gate = captures.iter().any(|capture| {
            !capture.requires_initialization_state
                && !capture.capture.borrowed
                && self.program.concrete_needs_drop(&capture.value_type)
        });
        if !gate {
            return Ok(());
        }
        let concrete = captures
            .iter()
            .map(|capture| capture.value_type.clone())
            .collect::<Vec<_>>();
        let canonical = concrete
            .iter()
            .map(|capture| CanonicalType::concrete(capture, origin))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|diagnostic| vec![diagnostic])?;
        self.requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
                closure: record.ordinal,
                captures: canonical,
            }),
            plan: LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::ClosureEnvironment {
                closure: instance,
                captures: concrete,
                drops: None,
            }),
            kind: LoweredArtifactDependencyKind::GcFinalizer,
            origin: origin.clone(),
            use_site: Some(site),
        });
        Ok(())
    }

    fn request_callback_environment(
        &mut self,
        callback: LoweredReactiveCallbackId,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        let Some(record) = self.owner.reactive_callback(self.program, callback) else {
            return Ok(());
        };
        // An explicit callback's own closure construction carries its 4.4
        // `ClosureEnvironment` use; only thunk callbacks install theirs here.
        let Some(thunk) = record.thunk else {
            return Ok(());
        };
        let function_type = record.function_type.clone();
        let instance = resolve_thunk_instance(
            self.program,
            self.owner,
            LoweredBindingSite::ReactiveCallback(callback),
            thunk,
            function_type,
            origin,
        )?;
        self.request_environment(
            instance,
            ArtifactUseSite::ReactiveCallbackEnvironment(callback),
            origin,
        )
    }

    fn request_runner(
        &mut self,
        family: ReactiveRunnerFamily,
        site: ArtifactSite,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        let key = match family {
            ReactiveRunnerFamily::Reaction => {
                ArtifactRequestKey::ReactionRunner(ReactiveRunnerKey {
                    owner: self.site_owner,
                    site,
                })
            }
            ReactiveRunnerFamily::Until => ArtifactRequestKey::UntilRunner(ReactiveRunnerKey {
                owner: self.site_owner,
                site,
            }),
            ReactiveRunnerFamily::Derived => ArtifactRequestKey::DerivedRunner(ReactiveRunnerKey {
                owner: self.site_owner,
                site,
            }),
        };
        let plan = ReactiveRunnerPlan {
            owner: self.site_owner,
            site,
            body: ReactiveRunnerBody::Unexpanded,
        };
        let plan = match family {
            ReactiveRunnerFamily::Reaction => LoweredArtifactPlan::ReactionRunner(plan),
            ReactiveRunnerFamily::Until => LoweredArtifactPlan::UntilRunner(plan),
            ReactiveRunnerFamily::Derived => LoweredArtifactPlan::DerivedRunner(plan),
        };
        let kind = match family {
            ReactiveRunnerFamily::Reaction => LoweredArtifactDependencyKind::ReactionRunner,
            ReactiveRunnerFamily::Until => LoweredArtifactDependencyKind::UntilRunner,
            ReactiveRunnerFamily::Derived => LoweredArtifactDependencyKind::DerivedRunner,
        };
        self.requests.push(ClosureRequest::Artifact {
            key,
            plan,
            kind,
            origin: origin.clone(),
            use_site: Some(ArtifactUseSite::ReactiveRunner(match site {
                ArtifactSite::Operation(operation) => operation,
                ArtifactSite::Callback(callback) => {
                    // The runner use site is keyed by the operation whose
                    // callback this is; call sites supply the operation.
                    let Some(operation) = self.operation_for_callback(callback) else {
                        return Err(vec![Diagnostic::new(
                            origin.span.clone(),
                            "a runner callback has no owning operation".to_string(),
                        )]);
                    };
                    operation
                }
                ArtifactSite::PlanLocal(_) => {
                    return Err(vec![Diagnostic::new(
                        origin.span.clone(),
                        "plan-local sites never request runners".to_string(),
                    )]);
                }
            })),
        });
        Ok(())
    }

    /// The operation whose callback or predicate is `callback`.
    fn operation_for_callback(
        &self,
        callback: LoweredReactiveCallbackId,
    ) -> Option<LoweredReactiveOperationId> {
        let matches = |operation: &super::LoweredReactiveOperation| match &operation.kind {
            LoweredReactiveOperationKind::Reaction {
                callback: candidate,
                ..
            }
            | LoweredReactiveOperationKind::Batch {
                callback: candidate,
            } => *candidate == callback,
            LoweredReactiveOperationKind::Until {
                predicate: candidate,
                ..
            } => *candidate == callback,
            _ => false,
        };
        match self.owner {
            OwnerArenas::Instance(body) => body
                .reactive_operations
                .iter()
                .find_map(|(id, operation)| matches(operation).then_some(id)),
            OwnerArenas::Initializer(_) => self
                .program
                .reactive_operations
                .iter()
                .find_map(|(id, operation)| matches(operation).then_some(id)),
        }
    }
}

impl LoweredOwnerVisitor for Stage45ScanVisitor<'_> {
    fn coro_creation(&mut self, id: LoweredCoroId, origin: &Origin) -> Result<(), Vec<Diagnostic>> {
        self.request_pair(id, origin)
    }

    fn reactive_operation(
        &mut self,
        id: LoweredReactiveOperationId,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        let Some(operation) = self.owner.reactive_operation(self.program, id) else {
            return Ok(());
        };
        match &operation.kind {
            LoweredReactiveOperationKind::Reaction { callback, .. } => {
                let callback = *callback;
                self.request_callback_environment(callback, origin)?;
                self.request_runner(
                    ReactiveRunnerFamily::Reaction,
                    ArtifactSite::Callback(callback),
                    origin,
                )
            }
            LoweredReactiveOperationKind::Until { predicate, .. } => {
                let predicate = *predicate;
                self.request_callback_environment(predicate, origin)?;
                self.request_runner(
                    ReactiveRunnerFamily::Until,
                    ArtifactSite::Callback(predicate),
                    origin,
                )
            }
            LoweredReactiveOperationKind::Batch { callback } => {
                self.request_callback_environment(*callback, origin)
            }
            LoweredReactiveOperationKind::DerivedCreate {
                evaluator,
                function_type,
                ..
            } => {
                let instance = resolve_thunk_instance(
                    self.program,
                    self.owner,
                    LoweredBindingSite::DerivedEvaluator(id),
                    *evaluator,
                    function_type.clone(),
                    origin,
                )?;
                self.request_environment(
                    instance,
                    ArtifactUseSite::DerivedEvaluatorEnvironment(id),
                    origin,
                )?;
                self.request_runner(
                    ReactiveRunnerFamily::Derived,
                    ArtifactSite::Operation(id),
                    origin,
                )
            }
            LoweredReactiveOperationKind::SignalCreate { .. }
            | LoweredReactiveOperationKind::SignalRead { .. }
            | LoweredReactiveOperationKind::SignalNotify { .. }
            | LoweredReactiveOperationKind::DerivedRead { .. }
            | LoweredReactiveOperationKind::Scope
            | LoweredReactiveOperationKind::Snapshot => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::specialization::{ArtifactRequestKey, CoroutineCodesKey, GcFinalizerKey};
    use crate::{
        ArenaId, CheckedType, DropGluePlan, FunctionInstanceId, GcFinalizerPlan,
        LoweredArtifactPlan, LoweredBindingSite, LoweredBoundTarget, LoweredInstanceBody,
        LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker, TypedModule,
    };

    use super::*;

    fn standard_library_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent")
            .join("stdlib")
    }

    fn checked_program(source: &str) -> TypedModule {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent");
        let program = ProgramLoader::new()
            .with_standard_library_root(standard_library_root())
            .load_source(source, root)
            .expect("test source should load");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("test source should resolve");
        TypeChecker::new()
            .check(resolved)
            .expect("test source should type check")
    }

    /// The production closure: lowered, materialized, closed, and validated.
    fn lower(source: &str) -> (TypedModule, LoweredModule) {
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .unwrap_or_else(|diagnostics| panic!("source should lower: {diagnostics:?}\n{source}"));
        (module, lowered)
    }

    fn function_template(program: &LoweredProgram, name: &str) -> crate::FunctionId {
        program
            .functions
            .iter()
            .find(|(_, _, function)| {
                function.name == name || function.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"))
    }

    fn instance_body<'a>(program: &'a LoweredProgram, name: &str) -> &'a LoweredInstanceBody {
        let template = function_template(program, name);
        program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == template)
            .and_then(|(_, instance)| instance.body.as_ref())
            .unwrap_or_else(|| panic!("no materialized body for {name}"))
    }

    fn pair_plan(program: &LoweredProgram, body: FunctionInstanceId) -> &CoroutineCodesPlan {
        let ordinal = program.instances.get(body).expect("body instance").ordinal;
        let key = ArtifactRequestKey::CoroutineCodes(CoroutineCodesKey { body: ordinal });
        let artifact_ordinal = program
            .specializations
            .artifact_ordinal(&key)
            .unwrap_or_else(|| panic!("the pair key is interned: {key:?}"));
        program
            .artifacts
            .iter()
            .find(|(_, artifact)| artifact.ordinal == artifact_ordinal)
            .and_then(|(_, artifact)| artifact.plan.as_ref())
            .map(|plan| match plan {
                LoweredArtifactPlan::CoroutineCodes(plan) => plan,
                other => panic!("expected a pair plan, got {other:?}"),
            })
            .expect("the pair artifact exists")
    }

    /// The body instance the sole `coro` creation in the named function binds.
    fn created_body_instance(program: &LoweredProgram, name: &str) -> FunctionInstanceId {
        let body = instance_body(program, name);
        let (coro_id, _) = body
            .coros
            .iter()
            .next()
            .unwrap_or_else(|| panic!("{name} contains a `coro` creation"));
        match body.binding(LoweredBindingSite::Coro(coro_id)) {
            Some(LoweredBoundTarget::Instance(instance)) => *instance,
            other => panic!("the creation binds its body instance, got {other:?}"),
        }
    }

    /// The pair serving the sole `coro` creation in the named function.
    fn created_pair<'a>(program: &'a LoweredProgram, name: &str) -> &'a CoroutineCodesPlan {
        pair_plan(program, created_body_instance(program, name))
    }

    fn canonical(value_type: &CheckedType) -> CanonicalType {
        CanonicalType::concrete(value_type, &Origin::compiler()).expect("a concrete type")
    }

    fn finalizer_plan<'a>(
        program: &'a LoweredProgram,
        key: &ArtifactRequestKey,
    ) -> &'a GcFinalizerPlan {
        let ordinal = program
            .specializations
            .artifact_ordinal(key)
            .unwrap_or_else(|| panic!("the finalizer key is interned: {key:?}"));
        program
            .artifacts
            .iter()
            .find(|(_, artifact)| artifact.ordinal == ordinal)
            .and_then(|(_, artifact)| artifact.plan.as_ref())
            .map(|plan| match plan {
                LoweredArtifactPlan::GcFinalizer(plan) => plan,
                other => panic!("expected a finalizer plan, got {other:?}"),
            })
            .expect("the finalizer artifact exists")
    }

    fn frame(plan: &CoroutineCodesPlan) -> &CoroutineFramePlan {
        plan.frame
            .as_ref()
            .unwrap_or_else(|| panic!("the pair is expanded: {plan:?}"))
    }

    /// Records the production closure's round and growth maxima for one
    /// fixture and requires it to converge quickly.
    fn record_stats(program: &LoweredProgram, fixture: &str) -> crate::ClosureStats {
        let stats = program
            .closure_stats
            .expect("the production closure records its stats");
        eprintln!(
            "stage 4.5 {fixture}: {} rounds, growth {}",
            stats.rounds, stats.growth
        );
        assert!(
            stats.rounds <= 4,
            "{fixture} converges quickly: {} rounds",
            stats.rounds
        );
        stats
    }

    #[test]
    fn coroutine_codes_plans_mirror_their_body_instances() {
        let source = concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "// No captures at all.\n",
            "def plain: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "// Droppable and `Copy` frame bindings.\n",
            "def framed: () -> Coroutine{} I32 = () => coro {\n",
            "  let owned = c_string \"a\"\n",
            "  let plain_value = 1\n",
            "  inspect owned + plain_value\n",
            "}\n",
            "// A droppable owned capture.\n",
            "def owning: move CString -> Coroutine{} I32 = move value => coro { inspect value; 1 }\n",
            "// A `Copy` capture installs a finalizer that drops nothing.\n",
            "def copying: I32 -> Coroutine{} I32 = value => coro { value }\n",
            "// A mutable-storage capture fires the gate but the body skips the cell.\n",
            "def mutable_capture: () -> Coroutine{state.read} I32 = () => {\n",
            "  let mut cell = c_string \"c\"\n",
            "  cell = c_string \"e\"\n",
            "  let task = coro { inspect cell; 1 }\n",
            "  task\n",
            "}\n",
            "let a = plain ()\n",
            "let framed_task = framed ()\n",
            "let b = owning (c_string \"b\")\n",
            "let c = copying 2\n",
            "let d = mutable_capture ()\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "frame facts");

        // No captures: no environment finalizer.
        let plan = created_pair(program, "plain");
        let plain = frame(plan);
        assert!(plain.captures.is_empty());
        assert!(plain.capture_finalizer.is_none());
        assert_eq!(plain.result_type, CheckedType::I32);
        assert_eq!(plain.resume_points, 0);
        assert!(plain.resources.is_empty());
        assert!(plain.frame_bindings.is_empty());

        // Frame bindings: droppable and `Copy` cells.
        let plan = created_pair(program, "framed");
        let framed = frame(plan);
        let owned = framed
            .frame_bindings
            .iter()
            .find(|binding| binding.value_type == CheckedType::CString)
            .expect("the CString frame binding");
        assert!(
            owned.unwind_drop.is_some(),
            "a droppable frame binding has unwind drop glue"
        );
        let copy = framed
            .frame_bindings
            .iter()
            .find(|binding| binding.value_type == CheckedType::I32)
            .expect("the I32 frame binding");
        assert!(
            copy.unwind_drop.is_none(),
            "a Copy frame binding has no unwind drop"
        );

        // A droppable capture: the finalizer plan drops it.
        let plan = created_pair(program, "owning");
        let owning = frame(plan);
        assert!(
            owning.captures.contains(&CheckedType::CString),
            "the droppable capture is recorded: {:?}",
            owning.captures
        );
        let finalizer = owning
            .capture_finalizer
            .as_ref()
            .expect("a capture requests the finalizer");
        assert!(finalizer.artifact.is_some(), "the finalizer is bound");
        let owning_body = program
            .instances
            .get(created_body_instance(program, "owning"))
            .and_then(|instance| instance.body.as_ref())
            .expect("body instance");
        let owning_captures = owning_body
            .captures()
            .iter()
            .map(|capture| canonical(&capture.value_type))
            .collect::<Vec<_>>();
        let expected = ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(created_body_instance(program, "owning"))
                .expect("body instance")
                .ordinal,
            captures: owning_captures.clone(),
        });
        assert_eq!(finalizer.key, expected);
        match finalizer_plan(program, &expected) {
            GcFinalizerPlan::ClosureEnvironment { drops, .. } => {
                let drops = drops.as_ref().expect("expanded");
                let dropped = drops
                    .iter()
                    .find(|drop| drop.value_type == CheckedType::CString)
                    .expect("the CString capture is dropped");
                assert_eq!(
                    owning_captures[dropped.index],
                    canonical(&CheckedType::CString),
                    "the dropped capture index names the CString capture"
                );
            }
            other => panic!("expected a closure-environment finalizer, got {other:?}"),
        }

        // A `Copy` capture: the finalizer is installed but drops nothing.
        let plan = created_pair(program, "copying");
        let copying = frame(plan);
        assert_eq!(copying.captures, vec![CheckedType::I32]);
        let finalizer = copying
            .capture_finalizer
            .as_ref()
            .expect("any capture requests the finalizer");
        match finalizer_plan(program, &finalizer.key) {
            GcFinalizerPlan::ClosureEnvironment { drops, .. } => {
                assert!(
                    drops.as_ref().expect("expanded").is_empty(),
                    "a Copy capture drops nothing"
                );
            }
            other => panic!("expected a closure-environment finalizer, got {other:?}"),
        }

        // A mutable-storage capture: the ownership checker never marks a
        // coroutine capture borrowed ("a coroutine cannot capture a borrowed
        // view"), so the mutable cell is the reachable gate-fires/body-skips
        // case. The finalizer is installed but drops nothing.
        let plan = created_pair(program, "mutable_capture");
        let mutable = frame(plan);
        let mutable_body = program
            .instances
            .get(created_body_instance(program, "mutable_capture"))
            .and_then(|instance| instance.body.as_ref())
            .expect("the body thunk's instance");
        let cell_capture = mutable_body
            .captures()
            .iter()
            .find(|capture| capture.value_type == CheckedType::CString)
            .expect("the CString capture");
        assert!(
            cell_capture.mutable_storage,
            "the CString capture is a mutable cell"
        );
        assert!(
            mutable.captures.contains(&CheckedType::CString),
            "the cell capture is recorded: {:?}",
            mutable.captures
        );
        let finalizer = mutable
            .capture_finalizer
            .as_ref()
            .expect("the gate fires for the droppable cell");
        match finalizer_plan(program, &finalizer.key) {
            GcFinalizerPlan::ClosureEnvironment { drops, .. } => {
                assert!(
                    drops.as_ref().expect("expanded").is_empty(),
                    "a mutable-storage capture is excluded by the finalizer body"
                );
            }
            other => panic!("expected a closure-environment finalizer, got {other:?}"),
        }
    }

    #[test]
    fn coroutine_codes_record_resume_states_and_await_types() {
        let source = concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "let signal flag = 0\n",
            "// Several resume points with mixed await result types.\n",
            "def chained: () -> Coroutine{} (I32, CString) = () => coro {\n",
            "  let number = await (coro { 1 })\n",
            "  let text = await (coro { c_string \"x\" })\n",
            "  (number, text)\n",
            "}\n",
            "// An external `Wait` parks on a wait state.\n",
            "def waiter: move Wait I32 -> Coroutine{} I32 = move pending => coro {\n",
            "  let _ = await pending\n",
            "  0\n",
            "}\n",
            "def make_completion: () -> (wait: Wait I32, resolver: Resolver I32) = () => completion (scheduler ())\n",
            "def drive_wait: () -> Coroutine{} I32 = () => {\n",
            "  let (wait, resolver) = make_completion ()\n",
            "  let _ = resolver\n",
            "  waiter wait\n",
            "}\n",
            "// An `until` child parks on an until state.\n",
            "def waiting: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { flag >= 1 })\n",
            "  ()\n",
            "}\n",
            "let a = chained ()\n",
            "let b = drive_wait ()\n",
            "with Reactive = reactive_scope () { waiting () }\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "resume states");

        let plan = created_pair(program, "chained");
        let chained = frame(plan);
        assert_eq!(chained.resume_points, 2);
        assert_eq!(
            chained.await_result_types,
            vec![CheckedType::I32, CheckedType::CString]
        );
        assert!(chained.wait_await_states.is_empty());
        assert!(chained.until_await_states.is_empty());

        let plan = created_pair(program, "waiter");
        let waiter = frame(plan);
        assert_eq!(waiter.resume_points, 1);
        assert_eq!(waiter.wait_await_states, vec![1]);
        assert!(waiter.until_await_states.is_empty());
        assert_eq!(waiter.await_result_types.len(), 1);

        let plan = created_pair(program, "waiting");
        let waiting = frame(plan);
        assert_eq!(waiting.resume_points, 1);
        assert_eq!(waiting.until_await_states, vec![1]);
        assert!(waiting.wait_await_states.is_empty());
    }

    #[test]
    fn coroutine_codes_record_resource_pass_modes() {
        let source = concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "// A `Copy` distinct resource is passed by value.\n",
            "type Counter = ctor (value: I32)\n",
            "def read_counter: () ->{Counter} I32 = () => (resource Counter).value\n",
            "def use_counter: () -> Coroutine{Counter} I32 = () => coro { read_counter () }\n",
            "// A mutable resource slot is indirect.\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "def use_mut: () -> Coroutine{mut Counter} I32 = () => coro { increment (); 0 }\n",
            "// A non-`Copy` droppable resource slot is indirect.\n",
            "type Token = ctor (id: I32, payload: CString)\n",
            "def observe_token: () ->{Token} I32 = () => (resource Token).id\n",
            "def use_token: () -> Coroutine{Token} I32 = () => coro { observe_token () }\n",
            "let a = with Counter = Counter (value: 1) { use_counter () }\n",
            "let b = with mut Counter = Counter (value: 2) { use_mut () }\n",
            "let c = with Token = Token (id: 1, payload: c_string \"t\") { use_token () }\n",
        );
        let (module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "resource pass modes");

        let plan = created_pair(program, "use_counter");
        let counter = frame(plan);
        assert_eq!(counter.resources.len(), 1);
        assert!(
            module.is_copy_in_function(&counter.resources[0].resource.value_type, None),
            "the value slot's resource is `Copy`"
        );
        assert!(
            !counter.resources[0].indirect,
            "a plain `Copy` resource is a value slot"
        );

        let plan = created_pair(program, "use_mut");
        let mutable = frame(plan);
        assert_eq!(mutable.resources.len(), 1);
        assert!(mutable.resources[0].resource.mutable);
        assert!(
            mutable.resources[0].indirect,
            "a mutable resource is indirect"
        );

        let plan = created_pair(program, "use_token");
        let token = frame(plan);
        assert_eq!(token.resources.len(), 1);
        assert!(!token.resources[0].resource.mutable);
        assert!(
            !module.is_copy_in_function(&token.resources[0].resource.value_type, None),
            "the indirect slot's resource is not `Copy`"
        );
        assert!(
            token.resources[0].indirect,
            "a non-`Copy` resource is indirect"
        );

        // Every planned slot agrees with legacy's pass predicate on the
        // substituted type: `mutable || !is_copy_in_function(.., None)`.
        for (_, artifact) in program.artifacts.iter() {
            let Some(LoweredArtifactPlan::CoroutineCodes(plan)) = &artifact.plan else {
                continue;
            };
            let Some(frame) = &plan.frame else {
                panic!("every requested pair is expanded");
            };
            for slot in &frame.resources {
                let legacy = slot.resource.mutable
                    || !module.is_copy_in_function(&slot.resource.value_type, None);
                assert_eq!(
                    slot.indirect, legacy,
                    "the planned pass mode agrees with `is_copy_in_function` for `{}`",
                    slot.resource.value_type
                );
            }
        }
    }

    #[test]
    fn coroutine_creation_sites_record_uses_in_initializers_and_instances() {
        let source = concat!(
            "use std.coroutine.*\n",
            "// A creation directly inside the module initializer.\n",
            "let created = coro { 1 }\n",
            "// A creation inside an instance body.\n",
            "def task: () -> Coroutine{} I32 = () => coro { 2 }\n",
            "let started = task ()\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "creation sites");

        // The initializer records its creation use with a matching pair.
        let mut initializer_uses = 0;
        for uses in &program.initializer_artifact_uses {
            for use_ in uses {
                if let ArtifactUseSite::CoroCreation(id) = use_.site {
                    initializer_uses += 1;
                    assert!(
                        program.coros.get(id).is_some(),
                        "the use site resolves in the initializer arenas"
                    );
                    assert_eq!(
                        program
                            .specializations
                            .artifact(use_.artifact)
                            .map(|key| key.family_name()),
                        Some("coroutine-codes")
                    );
                }
            }
        }
        assert_eq!(
            initializer_uses, 1,
            "the initializer's creation is recorded"
        );

        // The instance records its creation use the same way.
        let body = instance_body(program, "task");
        let uses = body
            .artifact_uses
            .iter()
            .filter(|use_| matches!(use_.site, ArtifactUseSite::CoroCreation(_)))
            .count();
        assert_eq!(uses, 1, "the instance's creation is recorded");
    }

    #[test]
    fn nested_and_generic_creations_get_distinct_pairs() {
        let source = concat!(
            "use std.coroutine.*\n",
            "// A nested `coro` inside a coroutine body: two pairs.\n",
            "def outer: () -> Coroutine{} I32 = () => coro {\n",
            "  let inner = coro { 2 }\n",
            "  await inner\n",
            "}\n",
            "// A generic enclosing function instantiated at two types.\n",
            "def generic: <T where Copy T> T -> Coroutine{} T = value => coro { value }\n",
            "let a: Coroutine{} I32 = generic 1\n",
            "let b: Coroutine{} U8 = generic (1 satisfies U8)\n",
            "let c = outer ()\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "nested and generic");

        let mut plans = Vec::new();
        for (_, artifact) in program.artifacts.iter() {
            if let Some(LoweredArtifactPlan::CoroutineCodes(plan)) = &artifact.plan {
                plans.push(plan);
            }
        }
        assert_eq!(
            plans.len(),
            4,
            "nested (2) plus two generic instantiations give four pairs"
        );
        let mut ordinals = plans
            .iter()
            .map(|plan| plan.body.index())
            .collect::<Vec<_>>();
        ordinals.sort_unstable();
        ordinals.dedup();
        assert_eq!(ordinals.len(), 4, "each pair is keyed by its body instance");

        // The two generic instantiations differ in their frame capture types.
        let mut generic_captures = plans
            .iter()
            .filter_map(|plan| plan.frame.as_ref())
            .filter(|frame| frame.captures.len() == 1)
            .map(|frame| frame.captures[0].clone())
            .collect::<Vec<_>>();
        generic_captures.sort_by_key(|capture| format!("{capture:?}"));
        assert!(
            generic_captures.contains(&CheckedType::I32)
                && generic_captures.contains(&CheckedType::U8),
            "the generic instantiations capture their own types: {generic_captures:?}"
        );
    }

    #[test]
    fn expanded_pairs_bind_every_planned_callee() {
        let source = concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "def task: move CString -> Coroutine{} I32 = move value => coro {\n",
            "  let owned = c_string \"a\"\n",
            "  inspect value + inspect owned\n",
            "}\n",
            "let created = task (c_string \"b\")\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        let stats = record_stats(program, "callee binding");
        let mut pairs = 0;
        let mut drops = 0;
        for (_, artifact) in program.artifacts.iter() {
            let Some(LoweredArtifactPlan::CoroutineCodes(plan)) = &artifact.plan else {
                continue;
            };
            pairs += 1;
            let frame = frame(plan);
            if let Some(finalizer) = &frame.capture_finalizer {
                assert!(finalizer.artifact.is_some(), "the finalizer is bound");
                let key = program
                    .specializations
                    .artifact(finalizer.artifact.expect("bound"))
                    .expect("the finalizer key");
                assert!(
                    matches!(
                        key,
                        ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment { .. })
                    ),
                    "the planned finalizer is a closure environment: {key:?}"
                );
            }
            for binding in &frame.frame_bindings {
                if let Some(unwind_drop) = &binding.unwind_drop {
                    drops += 1;
                    let key = program
                        .specializations
                        .artifact(unwind_drop.artifact.expect("bound"))
                        .expect("the drop key");
                    assert!(
                        matches!(key, ArtifactRequestKey::DropGlue(value) if value == &canonical(&binding.value_type)),
                        "the unwind drop is the binding's own glue: {key:?}"
                    );
                    let plan = program
                        .artifacts
                        .iter()
                        .find(|(_, record)| {
                            program
                                .specializations
                                .artifact(record.ordinal)
                                .is_some_and(|key| {
                                    key == &ArtifactRequestKey::DropGlue(canonical(
                                        &binding.value_type,
                                    ))
                                })
                        })
                        .and_then(|(_, record)| record.plan.as_ref());
                    assert!(
                        matches!(plan, Some(LoweredArtifactPlan::DropGlue(DropGluePlan { body, .. })) if !matches!(body, crate::DropGlueBody::Unexpanded)),
                        "the unwind drop plan is expanded"
                    );
                }
            }
        }
        assert_eq!(pairs, 1);
        assert!(drops >= 1, "the droppable frame binding plans its drop");
        assert!(
            stats.rounds <= 4,
            "coroutine plans converge quickly: {} rounds",
            stats.rounds
        );
    }

    // ------------------------------------------------------------------
    // Stage 4.5 reactive runners.
    // ------------------------------------------------------------------

    /// The human-readable owner of one runner plan: the initializer or the
    /// owning instance's template name.
    fn owner_name(program: &LoweredProgram, owner: ArtifactSiteOwner) -> String {
        match owner {
            ArtifactSiteOwner::Initializer(initializer) => {
                format!("<initializer {}>", initializer.index())
            }
            ArtifactSiteOwner::Instance(ordinal) => program
                .instances
                .iter()
                .find(|(_, instance)| instance.ordinal == ordinal)
                .and_then(|(_, instance)| program.functions.get(instance.template))
                .map(|function| function.name.clone())
                .unwrap_or_else(|| format!("<instance {}>", ordinal.index())),
            ArtifactSiteOwner::Artifact(ordinal) => format!("<artifact {}>", ordinal.index()),
        }
    }

    /// Every expanded runner plan of one family, with its owner name.
    fn runner_plans<'a>(
        program: &'a LoweredProgram,
        family: ReactiveRunnerFamily,
    ) -> Vec<(String, &'a ReactiveRunnerPlan)> {
        let mut plans = Vec::new();
        for (_, artifact) in program.artifacts.iter() {
            let Some(plan) = artifact.plan.as_ref() else {
                continue;
            };
            let plan = match (family, plan) {
                (ReactiveRunnerFamily::Reaction, LoweredArtifactPlan::ReactionRunner(plan))
                | (ReactiveRunnerFamily::Until, LoweredArtifactPlan::UntilRunner(plan))
                | (ReactiveRunnerFamily::Derived, LoweredArtifactPlan::DerivedRunner(plan)) => plan,
                _ => continue,
            };
            let owner = owner_name(program, plan.owner);
            plans.push((owner, plan));
        }
        plans
    }

    fn runner_body(plan: &ReactiveRunnerPlan) -> &ReactiveRunnerBody {
        assert!(
            !matches!(plan.body, ReactiveRunnerBody::Unexpanded),
            "the runner is expanded: {plan:?}"
        );
        &plan.body
    }

    /// The `GcFinalizer::ClosureEnvironment` plans requested from reactive
    /// callback/evaluator sites, as `(site, captures)`.
    fn reactive_environment_plans(
        program: &LoweredProgram,
    ) -> Vec<(ArtifactUseSite, Vec<CheckedType>)> {
        let mut found = Vec::new();
        let mut collect = |uses: &[super::super::LoweredArtifactUse]| {
            for use_ in uses {
                match use_.site {
                    ArtifactUseSite::ReactiveCallbackEnvironment(_)
                    | ArtifactUseSite::DerivedEvaluatorEnvironment(_) => {
                        let plan = program
                            .artifacts
                            .iter()
                            .find(|(_, artifact)| artifact.ordinal == use_.artifact)
                            .and_then(|(_, artifact)| artifact.plan.as_ref());
                        let Some(LoweredArtifactPlan::GcFinalizer(
                            GcFinalizerPlan::ClosureEnvironment { captures, .. },
                        )) = plan
                        else {
                            panic!(
                                "a reactive environment use names a closure finalizer: {use_:?}"
                            );
                        };
                        found.push((use_.site, captures.clone()));
                    }
                    _ => {}
                }
            }
        };
        for (_, instance) in program.instances.iter() {
            if let Some(body) = &instance.body {
                collect(&body.artifact_uses);
            }
        }
        for uses in &program.initializer_artifact_uses {
            collect(uses);
        }
        found
    }

    #[test]
    fn reaction_runners_record_callback_types_and_resource_slots() {
        let source = concat!(
            "use std.cinterop.(CString, c_string)\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "type Counter = ctor (value: I32)\n",
            "def read_counter: () ->{Counter} I32 = () => (resource Counter).value\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "def subscribe_plain: () ->{Reactive} () = () => reaction { () }\n",
            "def subscribe_value: () ->{Reactive, Counter} () = () => reaction { read_counter (); () }\n",
            "def subscribe_mut: () ->{Reactive, mut Counter} () = () => reaction { increment (); () }\n",
            "def poke: () -> () = () => ()\n",
            "def subscribe_explicit: () ->{Reactive} () = () => reaction poke\n",
            "def subscribe_owned: move CString ->{Reactive} () = move value => reaction { inspect value; () }\n",
            "let a = with Reactive = reactive_scope () { subscribe_plain () }\n",
            "let b = with Counter = Counter (value: 1) { with Reactive = reactive_scope () { subscribe_value () } }\n",
            "let c = with mut Counter = Counter (value: 2) { with Reactive = reactive_scope () { subscribe_mut () } }\n",
            "let d = with Reactive = reactive_scope () { subscribe_explicit () }\n",
            "let e = with Reactive = reactive_scope () { subscribe_owned (c_string \"x\") }\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "reaction runners");

        // Borrowed captures are unreachable for reactive callbacks: a thunk
        // cannot capture a borrowed view, and a locally built borrowed closure
        // cannot be passed as an argument. The droppable and explicit cases
        // below cover the reachable shapes.
        let plans = runner_plans(program, ReactiveRunnerFamily::Reaction);
        assert_eq!(plans.len(), 5, "one runner per reaction: {plans:?}");

        let body_for = |name: &str| {
            plans
                .iter()
                .find(|(owner, _)| owner.ends_with(name))
                .map(|(_, plan)| runner_body(plan))
                .unwrap_or_else(|| panic!("no reaction runner owned by {name}"))
        };

        let ReactiveRunnerBody::Reaction {
            callback_type,
            resources,
        } = body_for("subscribe_plain")
        else {
            panic!("a reaction runner carries a reaction body")
        };
        assert!(
            callback_type.effects.resources.is_empty(),
            "the plain callback needs no resources"
        );
        assert!(resources.is_empty());

        let ReactiveRunnerBody::Reaction { resources, .. } = body_for("subscribe_value") else {
            panic!("a reaction runner carries a reaction body")
        };
        assert_eq!(resources.len(), 1);
        assert!(!resources[0].resource.mutable);
        assert!(!resources[0].indirect, "a `Copy` resource slot is a value");

        let ReactiveRunnerBody::Reaction { resources, .. } = body_for("subscribe_mut") else {
            panic!("a reaction runner carries a reaction body")
        };
        assert_eq!(resources.len(), 1);
        assert!(resources[0].resource.mutable);
        assert!(resources[0].indirect, "a mutable resource slot is indirect");

        // The explicit callback has a runner but no thunk environment use.
        let ReactiveRunnerBody::Reaction { callback_type, .. } = body_for("subscribe_explicit")
        else {
            panic!("a reaction runner carries a reaction body")
        };
        assert!(
            callback_type.effects.resources.is_empty(),
            "the explicit callback needs no resources"
        );

        // A droppable thunk capture installs a finalizer; a borrowed capture
        // does not fire the install gate.
        let environments = reactive_environment_plans(program);
        let owned = environments
            .iter()
            .find(|(site, captures)| {
                matches!(site, ArtifactUseSite::ReactiveCallbackEnvironment(_))
                    && captures.contains(&CheckedType::CString)
            })
            .unwrap_or_else(|| panic!("the owned capture installs a finalizer: {environments:?}"));
        assert!(
            owned.1.len() >= 1,
            "the owned callback's concrete captures are listed"
        );
        assert!(
            environments
                .iter()
                .all(|(_, captures)| captures.contains(&CheckedType::CString)),
            "only the droppable capture installs a finalizer: {environments:?}"
        );
    }

    #[test]
    fn until_and_derived_runners_record_their_call_shapes() {
        let source = concat!(
            "use std.coroutine.*\n",
            "let signal flag = 0\n",
            "let signal count = 0\n",
            "def waiting: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { flag >= 1 })\n",
            "  ()\n",
            "}\n",
            "let doubled = count + count\n",
            "def make: () ->{state.read} I32 = () => {\n",
            "  let local = count + count\n",
            "  local\n",
            "}\n",
            "let waiting_task = with Reactive = reactive_scope () { waiting () }\n",
            "let made = make ()\n",
            "let observed = doubled\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "until and derived runners");

        let until = runner_plans(program, ReactiveRunnerFamily::Until);
        assert_eq!(until.len(), 1, "one runner per `until`: {until:?}");
        let ReactiveRunnerBody::Until { predicate_type } = runner_body(until[0].1) else {
            panic!("an `until` runner carries an until body")
        };
        assert!(
            matches!(
                predicate_type.result.as_ref(),
                CheckedType::Sum(sum)
                    if sum.alternatives.iter().any(|alternative| {
                        matches!(alternative, CheckedType::Distinct { name, .. } if name == "True")
                    })
            ),
            "the predicate returns `Bool`: {predicate_type:?}"
        );

        // `doubled` is an initializer derived binding; `observed = doubled` is
        // a second derived binding whose evaluator captures the first
        // (capturing a derived value); `make`'s local is instance-owned.
        let derived = runner_plans(program, ReactiveRunnerFamily::Derived);
        assert_eq!(
            derived.len(),
            3,
            "one runner per derived binding: {derived:?}"
        );
        let mut reads_state = 0;
        let mut pure = 0;
        for (owner, plan) in &derived {
            let ReactiveRunnerBody::Derived {
                evaluator_type,
                output_type,
            } = runner_body(plan)
            else {
                panic!("a derived runner carries a derived body")
            };
            assert!(
                evaluator_type.effects.resources.is_empty(),
                "a derived evaluator has no resources"
            );
            assert_eq!(
                canonical(output_type),
                canonical(&evaluator_type.result),
                "the output type is the evaluator's result"
            );
            if evaluator_type.effects.state.is_some() {
                reads_state += 1;
            } else {
                pure += 1;
            }
            assert!(
                owner.contains("make") || owner.contains("initializer"),
                "the derived runners belong to the initializer and the instance: {owner}"
            );
        }
        assert!(
            derived.iter().any(|(owner, _)| owner.contains("make")),
            "an instance-owned derived runner exists"
        );
        assert!(
            reads_state >= 1 && pure >= 1,
            "both a signal-reading evaluator and a derived-capturing evaluator are covered: {derived:?}"
        );
    }

    #[test]
    fn runner_sites_match_reactive_operations_and_environments() {
        let source = concat!(
            "use std.coroutine.*\n",
            "let signal count = 0\n",
            "def subscribe: () ->{Reactive} () = () => reaction { () }\n",
            "def wait: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { count >= 5 })\n",
            "  ()\n",
            "}\n",
            "def evaluate: () ->{state.read} I32 = () => {\n",
            "  let local = count + 1\n",
            "  local\n",
            "}\n",
            "let a = with Reactive = reactive_scope () { subscribe () }\n",
            "let b = with Reactive = reactive_scope () { wait () }\n",
            "let c = evaluate ()\n",
            "let d = batch { count = 1 }\n",
            "let e = snapshot count\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "runner site agreement");

        // Every runner plan is expanded; every family appears.
        let mut families = std::collections::HashSet::new();
        for (_, artifact) in program.artifacts.iter() {
            let Some(plan) = artifact.plan.as_ref() else {
                continue;
            };
            let family = match plan {
                LoweredArtifactPlan::ReactionRunner(_) => Some(ReactiveRunnerFamily::Reaction),
                LoweredArtifactPlan::UntilRunner(_) => Some(ReactiveRunnerFamily::Until),
                LoweredArtifactPlan::DerivedRunner(_) => Some(ReactiveRunnerFamily::Derived),
                _ => None,
            };
            if let Some(family) = family {
                assert!(plan.is_expanded(), "every runner is expanded: {plan:?}");
                families.insert(family);
            }
        }
        assert_eq!(
            families.len(),
            3,
            "the fixture covers all three runner families: {families:?}"
        );

        // Per owner: the runner uses equal the runner-bearing operations, and
        // each use resolves to an operation of the matching family kind.
        let mut owners: Vec<(String, usize, Vec<super::super::LoweredArtifactUse>)> = Vec::new();
        for (_, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            let expected = body
                .reactive_operations
                .iter()
                .filter(|(_, operation)| operation_has_runner(&operation.kind))
                .count();
            owners.push((instance.name.clone(), expected, body.artifact_uses.clone()));
        }
        for (index, _) in program.initializers.iter() {
            // The initializer owner's operations share the program arenas with
            // every declared function's template operations, so the expected
            // count is not derivable from the arena alone; the fixture's
            // initializer only has scope/batch/snapshot operations, which
            // request no runner.
            owners.push((
                format!("<initializer {}>", index.index()),
                0,
                program.initializer_artifact_uses[index.index()].clone(),
            ));
        }
        let mut runner_uses = 0;
        let mut environment_uses = 0;
        for (owner, expected, uses) in &owners {
            let found = uses
                .iter()
                .filter(|use_| matches!(use_.site, ArtifactUseSite::ReactiveRunner(_)))
                .count();
            assert_eq!(
                found, *expected,
                "{owner} has one runner use per runner-bearing operation"
            );
            runner_uses += found;
            for use_ in uses {
                match use_.site {
                    ArtifactUseSite::ReactiveRunner(operation) => {
                        let Some(record) = program
                            .instances
                            .iter()
                            .find(|(_, instance)| instance.name == *owner)
                            .and_then(|(_, instance)| instance.body.as_ref())
                            .and_then(|body| body.reactive_operation(operation))
                            .or_else(|| program.reactive_operations.get(operation))
                        else {
                            panic!("{owner} runner use site resolves to an operation");
                        };
                        let expected_family = match &record.kind {
                            LoweredReactiveOperationKind::Reaction { .. } => {
                                ReactiveRunnerFamily::Reaction
                            }
                            LoweredReactiveOperationKind::Until { .. } => {
                                ReactiveRunnerFamily::Until
                            }
                            LoweredReactiveOperationKind::DerivedCreate { .. } => {
                                ReactiveRunnerFamily::Derived
                            }
                            other => {
                                panic!("a runner use names a runner-bearing operation: {other:?}")
                            }
                        };
                        let actual_family = match program.specializations.artifact(use_.artifact) {
                            Some(ArtifactRequestKey::ReactionRunner(_)) => {
                                ReactiveRunnerFamily::Reaction
                            }
                            Some(ArtifactRequestKey::UntilRunner(_)) => ReactiveRunnerFamily::Until,
                            Some(ArtifactRequestKey::DerivedRunner(_)) => {
                                ReactiveRunnerFamily::Derived
                            }
                            other => panic!("a runner use names a runner key: {other:?}"),
                        };
                        assert_eq!(
                            expected_family, actual_family,
                            "{owner} runner family matches its operation"
                        );
                    }
                    ArtifactUseSite::ReactiveCallbackEnvironment(_)
                    | ArtifactUseSite::DerivedEvaluatorEnvironment(_) => {
                        environment_uses += 1;
                        let plan = program
                            .artifacts
                            .iter()
                            .find(|(_, artifact)| artifact.ordinal == use_.artifact)
                            .and_then(|(_, artifact)| artifact.plan.as_ref());
                        assert!(
                            matches!(
                                plan,
                                Some(LoweredArtifactPlan::GcFinalizer(
                                    GcFinalizerPlan::ClosureEnvironment { .. }
                                ))
                            ),
                            "every installed callback environment has a closure finalizer plan: {plan:?}"
                        );
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(runner_uses, 3, "one reaction, one until, one derived");
        // The fixture's callbacks capture nothing droppable, so no environment
        // finalizer is installed; the droppable case is asserted in
        // `reaction_runners_record_callback_types_and_resource_slots`.
        assert_eq!(environment_uses, 0);
    }

    fn operation_has_runner(kind: &LoweredReactiveOperationKind) -> bool {
        matches!(
            kind,
            LoweredReactiveOperationKind::Reaction { .. }
                | LoweredReactiveOperationKind::Until { .. }
                | LoweredReactiveOperationKind::DerivedCreate { .. }
        )
    }

    /// Validates a deliberately corrupted program with the production hooks
    /// and returns the diagnostics' messages.
    fn validation_messages(program: &LoweredProgram) -> Vec<String> {
        let hooks = super::super::artifact_closure::ProductionHooks;
        program
            .validate_artifact_closure(&hooks)
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect()
    }

    fn assert_message(messages: &[String], needle: &str) {
        assert!(
            messages.iter().any(|message| message.contains(needle)),
            "expected a `{needle}` diagnostic, got {messages:?}"
        );
    }

    fn coroutine_codes_artifact(program: &LoweredProgram) -> LoweredArtifactRequestId {
        program
            .artifacts
            .iter()
            .find(|(_, artifact)| {
                matches!(artifact.plan, Some(LoweredArtifactPlan::CoroutineCodes(_)))
            })
            .map(|(id, _)| id)
            .expect("the fixture requests a coroutine pair")
    }

    #[test]
    fn a_coroutine_pair_over_a_non_thunk_body_is_diagnosed() {
        let source = concat!(
            "use std.coroutine.*\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def plain: () -> I32 = () => 1\n",
            "let a = task ()\n",
            "let b = plain ()\n",
        );
        let (_module, mut lowered) = lower(source);
        let program = &mut lowered.program;
        let plain = program
            .instances
            .iter()
            .find(|(_, instance)| {
                program
                    .functions
                    .get(instance.template)
                    .is_some_and(|function| function.name.ends_with("plain"))
            })
            .map(|(id, _)| id)
            .expect("the plain instance");
        let artifact = coroutine_codes_artifact(program);
        let Some(LoweredArtifactPlan::CoroutineCodes(plan)) = program
            .artifacts
            .get_mut(artifact)
            .expect("artifact")
            .plan
            .as_mut()
        else {
            panic!("the pair plan")
        };
        plan.body = plain;
        assert_message(
            &validation_messages(program),
            "is not a coroutine body thunk",
        );
    }

    #[test]
    fn a_coroutine_pair_disagreeing_with_its_local_plan_is_diagnosed() {
        let source = concat!(
            "use std.coroutine.*\n",
            "let signal flag = 0\n",
            "def waiting: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { flag >= 1 })\n",
            "  ()\n",
            "}\n",
            "with Reactive = reactive_scope () { waiting () }\n",
        );
        let (_module, mut lowered) = lower(source);
        let program = &mut lowered.program;
        let artifact = coroutine_codes_artifact(program);
        let Some(LoweredArtifactPlan::CoroutineCodes(plan)) = program
            .artifacts
            .get_mut(artifact)
            .expect("artifact")
            .plan
            .as_mut()
        else {
            panic!("the pair plan")
        };
        let frame = plan.frame.as_mut().expect("expanded pair");
        frame.resume_points += 1;
        assert_message(
            &validation_messages(program),
            "disagrees with its body instance's local plan",
        );
    }

    #[test]
    fn a_coro_creation_use_naming_another_body_is_diagnosed() {
        let source = concat!(
            "use std.coroutine.*\n",
            "def first: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def second: () -> Coroutine{} I32 = () => coro { 2 }\n",
            "let a = first ()\n",
            "let b = second ()\n",
        );
        let (_module, mut lowered) = lower(source);
        let program = &mut lowered.program;
        let first = program
            .instances
            .iter()
            .find(|(_, instance)| {
                program
                    .functions
                    .get(instance.template)
                    .is_some_and(|function| function.name.ends_with("first"))
            })
            .map(|(id, _)| id)
            .expect("the first instance");
        let (second, second_body) = program
            .instances
            .iter()
            .find(|(_, instance)| {
                program
                    .functions
                    .get(instance.template)
                    .is_some_and(|function| function.name.ends_with("second"))
            })
            .map(|(id, instance)| (id, instance.body.as_ref().expect("second body")))
            .expect("the second instance");
        let second_bound = second_body
            .coros
            .iter()
            .next()
            .and_then(
                |(id, _)| match second_body.binding(LoweredBindingSite::Coro(id)) {
                    Some(LoweredBoundTarget::Instance(instance)) => Some(*instance),
                    _ => None,
                },
            )
            .expect("the second creation's body instance");
        // Point the first creation's binding at the second body thunk.
        let first_body = program
            .instances
            .get_mut(first)
            .and_then(|instance| instance.body.as_mut())
            .expect("the first body");
        let coro = first_body
            .coros
            .iter()
            .next()
            .map(|(id, _)| id)
            .expect("the first creation");
        first_body.bindings.insert(
            LoweredBindingSite::Coro(coro),
            LoweredBoundTarget::Instance(second_bound),
        );
        let _ = second;
        assert_message(
            &validation_messages(program),
            "names a different body instance than its binding",
        );
    }

    #[test]
    fn a_runner_site_without_a_matching_operation_is_diagnosed() {
        let source = concat!(
            "use std.coroutine.*\n",
            "let signal count = 0\n",
            "def subscribe: () ->{Reactive} () = () => reaction { () }\n",
            "def wait: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { count >= 1 })\n",
            "  ()\n",
            "}\n",
            "let a = with Reactive = reactive_scope () { subscribe () }\n",
            "let b = with Reactive = reactive_scope () { wait () }\n",
        );
        let (_module, mut lowered) = lower(source);
        let program = &mut lowered.program;
        // The `until` predicate callback, which no reaction references.
        let predicate = program
            .reactive_operations
            .iter()
            .find_map(|(_, operation)| match &operation.kind {
                LoweredReactiveOperationKind::Until { predicate, .. } => Some(*predicate),
                _ => None,
            })
            .expect("the fixture's `until` predicate");
        let artifact = program
            .artifacts
            .iter()
            .find(|(_, artifact)| {
                matches!(artifact.plan, Some(LoweredArtifactPlan::ReactionRunner(_)))
            })
            .map(|(id, _)| id)
            .expect("the reaction runner");
        let Some(LoweredArtifactPlan::ReactionRunner(plan)) = program
            .artifacts
            .get_mut(artifact)
            .expect("artifact")
            .plan
            .as_mut()
        else {
            panic!("the runner plan")
        };
        plan.site = ArtifactSite::Callback(predicate);
        let messages = validation_messages(program);
        assert!(
            messages
                .iter()
                .any(|message| message.contains("has no matching reaction operation in its owner")),
            "{messages:?}"
        );
    }

    #[test]
    fn generic_reactive_sites_get_one_runner_per_instantiation() {
        let source = concat!(
            "use std.coroutine.*\n",
            "let signal count = 0\n",
            "def peek: <T> T -> I32 = _ => 0\n",
            "def generic_reaction: <T where Copy T> T ->{Reactive} () = value => reaction { peek value; () }\n",
            "def generic_until: <T where Copy T> T -> Coroutine{Reactive} () = value => coro {\n",
            "  let _ = await (until { count + peek value > 0 })\n",
            "  ()\n",
            "}\n",
            "def generic_derived: <T where Copy T> T ->{state.read} I32 = value => {\n",
            "  let local = count + peek value\n",
            "  local\n",
            "}\n",
            "let r1 = with Reactive = reactive_scope () { generic_reaction 1 }\n",
            "let r2 = with Reactive = reactive_scope () { generic_reaction (1 satisfies U8) }\n",
            "let u1 = with Reactive = reactive_scope () { generic_until 1 }\n",
            "let u2 = with Reactive = reactive_scope () { generic_until (1 satisfies U8) }\n",
            "let d1 = generic_derived 1\n",
            "let d2 = generic_derived (1 satisfies U8)\n",
        );
        let (_module, lowered) = lower(source);
        let program = &lowered.program;
        record_stats(program, "generic reactive sites");

        for (family, name) in [
            (ReactiveRunnerFamily::Reaction, "reaction"),
            (ReactiveRunnerFamily::Until, "until"),
            (ReactiveRunnerFamily::Derived, "derived"),
        ] {
            let plans = runner_plans(program, family);
            assert_eq!(
                plans.len(),
                2,
                "two instantiations give two {name} runners: {plans:?}"
            );
            let ordinals = plans
                .iter()
                .map(|(_, plan)| match plan.owner {
                    ArtifactSiteOwner::Instance(ordinal) => ordinal.index(),
                    other => panic!("a generic runner is instance-owned: {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_ne!(
                ordinals[0], ordinals[1],
                "the two {name} runners have distinct owners"
            );
        }
    }
}
