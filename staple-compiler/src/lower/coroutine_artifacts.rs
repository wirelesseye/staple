//! Stage 4.5: the coroutine-codes artifact expander and scanner.
//!
//! `expand_coroutine_codes` fills one `CoroutineCodes` plan from the body
//! thunk's **instance-local** plan and body, never from the template plan the
//! `LoweredCoro` indexes and never from `TypedModule`: the frame result and
//! await types, the resume-state count, the `Wait`/`until` cancellation
//! states, the deferred-resource bundle with its pass modes, the ordered
//! concrete captures, the frame-binding unwind drops, and the thunk's
//! environment finalizer.
//!
//! The scanner walks one owner in lowered evaluation order through the shared
//! Stage 4.4 owner walker and requests one pair per `coro` creation, recording
//! the `CoroCreation` use site. Instance owners take the body instance from
//! their own `Coro` binding; initializer owners resolve the thunk with the
//! Stage 3.3 recipe, and a key that was never interned is a diagnostic rather
//! than a silent skip.

use staple_syntax::Diagnostic;

use super::artifact_closure::{ArtifactUseSite, ClosureRequest, ExpansionResult, ScanResult};
use super::cleanup_artifacts::{LoweredOwnerVisitor, OwnerArenas, walk_owner};
use super::instance_resolution::{InstanceResolutionRequest, InstanceResolutionTarget};
use super::{
    ArenaId, CallSubstitutions, CoroutineCodesPlan, CoroutineFrameBinding, CoroutineFramePlan,
    CoroutineResourceSlot, FunctionInstanceId, GcFinalizerPlan, InitializerId,
    LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId,
    LoweredBindingSite, LoweredBoundTarget, LoweredCoroId, LoweredCoroutinePlan, LoweredProgram,
    Origin, PlannedArtifact,
};
use crate::specialization::{ArtifactRequestKey, CanonicalType, CoroutineCodesKey, GcFinalizerKey};

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
        let Some(value_type) = body.binding_symbol_type(*symbol).cloned() else {
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

/// Scans one materialized instance body for `coro` creations.
pub(super) fn scan_instance(program: &LoweredProgram, instance: FunctionInstanceId) -> ScanResult {
    let Some(record) = program.instances.get(instance) else {
        return Ok(Vec::new());
    };
    let Some(body) = record.body.as_ref() else {
        return Ok(Vec::new());
    };
    scan_owner(program, OwnerArenas::Instance(body))
}

/// Scans one module initializer for `coro` creations.
pub(super) fn scan_initializer(program: &LoweredProgram, initializer: InitializerId) -> ScanResult {
    if program.initializers.get(initializer).is_none() {
        return Ok(Vec::new());
    }
    scan_owner(program, OwnerArenas::Initializer(initializer))
}

fn scan_owner(program: &LoweredProgram, owner: OwnerArenas<'_>) -> ScanResult {
    let mut visitor = CoroutineScanVisitor {
        program,
        owner,
        requests: Vec::new(),
    };
    walk_owner(program, owner, &mut visitor)?;
    Ok(visitor.requests)
}

/// The scanning visitor: every `coro` creation becomes a `CoroutineCodes`
/// request with its exact use site.
struct CoroutineScanVisitor<'a> {
    program: &'a LoweredProgram,
    owner: OwnerArenas<'a>,
    requests: Vec<ClosureRequest>,
}

impl CoroutineScanVisitor<'_> {
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
        let Some(mut function_type) = self
            .program
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
        let resolved = self
            .program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: plan.thunk,
                origin: origin.clone(),
                function_type,
                substitutions: CallSubstitutions::default(),
                evidence: None,
                target: InstanceResolutionTarget::Root,
            })
            .map_err(|diagnostic| vec![diagnostic])?;
        let Some(ordinal) = self.program.specializations.instance_ordinal(&resolved.key) else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                "coro body thunk instance was never interned".to_string(),
            )]);
        };
        Ok(FunctionInstanceId::from_index(ordinal.index()))
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
}

impl LoweredOwnerVisitor for CoroutineScanVisitor<'_> {
    fn coro_creation(&mut self, id: LoweredCoroId, origin: &Origin) -> Result<(), Vec<Diagnostic>> {
        self.request_pair(id, origin)
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
}
