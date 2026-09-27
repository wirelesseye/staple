//! Stage 4.4: the ownership-cleanup artifact expanders.
//!
//! `expand_drop_glue` mirrors `compile_drop_value` exactly: the user `Drop`
//! selection with its representation drop, the coroutine/runtime opaque
//! cleanup routes, the C-string free, and the structural product/sum/distinct
//! recursion. Nested glues are requested as `DropGlue` artifacts, so a
//! recursive nominal type terminates through the 4.2 key deduplication.
//!
//! `expand_gc_finalizer` (Step 3) mirrors the four `ensure_*_finalizer`
//! builders, and the scanner/owned-binding collector (Step 4) reads the drop
//! facts Stage 3.4 already computed on materialized bodies.

use staple_syntax::{Diagnostic, Span};

use super::artifact_closure::{ClosureRequest, ExpansionResult};
use super::instance_resolution::{
    InstanceResolutionRequest, InstanceResolutionTarget, RuntimeOpaqueKind,
};
use super::{
    ArenaId, CallSubstitutions, DropGlueBody, DropGluePlan, DroppedAlternative, DroppedCapture,
    DroppedElement, GcFinalizerPlan, LoweredArtifactDependencyKind, LoweredArtifactPlan,
    LoweredArtifactRequestId, LoweredInstanceDependencyKind, LoweredProgram, Origin,
    PlannedArtifact, PlannedInstance, RuntimeRelease,
};
use crate::specialization::{ArtifactRequestKey, CanonicalType};
use crate::{CheckedType, FunctionId};

/// Expands one drop-glue artifact: the selected cleanup body for the plan's
/// concrete value type, mirroring the legacy decision order, plus the nested
/// glue requests the body needs.
pub(super) fn expand_drop_glue(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: DropGluePlan,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "drop-glue expansion received a missing artifact".to_string(),
            )]);
        }
    };
    let value_type = plan.value_type.clone();
    if !program.concrete_needs_drop(&value_type) {
        // A `DropGlue` key is only ever requested for a type that needs drop;
        // requesting one for a type legacy would no-op means the requester's
        // `needs_drop` predicate and this expander disagree.
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!("drop glue for `{value_type}` does not need drop"),
        )]);
    }
    let mut requests = Vec::new();
    let body = drop_glue_body(program, &value_type, &origin, &mut requests)?;
    Ok((
        LoweredArtifactPlan::DropGlue(DropGluePlan { value_type, body }),
        requests,
    ))
}

/// Expands one finalizer artifact: the referenced drop glue for the payload,
/// cell value, or buffer element, or the ordered capture drops for a closure
/// environment. The closure environment reads its capture metadata from the
/// closure instance's own materialized body, never from the requesting site.
pub(super) fn expand_gc_finalizer(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: GcFinalizerPlan,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "gc-finalizer expansion received a missing artifact".to_string(),
            )]);
        }
    };
    let mut requests = Vec::new();
    let plan = match plan {
        GcFinalizerPlan::Payload { value_type, .. } => {
            let glue = request_finalizer_glue(program, &value_type, &origin, &mut requests)?;
            GcFinalizerPlan::Payload {
                value_type,
                glue: Some(glue),
            }
        }
        GcFinalizerPlan::Cell { value_type, .. } => {
            let glue = request_finalizer_glue(program, &value_type, &origin, &mut requests)?;
            GcFinalizerPlan::Cell {
                value_type,
                glue: Some(glue),
            }
        }
        GcFinalizerPlan::Buffer { element, .. } => {
            let glue = request_finalizer_glue(program, &element, &origin, &mut requests)?;
            GcFinalizerPlan::Buffer {
                element,
                glue: Some(glue),
            }
        }
        GcFinalizerPlan::ClosureEnvironment {
            closure,
            captures,
            drops: _,
        } => {
            let drops =
                closure_environment_drops(program, closure, &captures, &origin, &mut requests)?;
            GcFinalizerPlan::ClosureEnvironment {
                closure,
                captures,
                drops: Some(drops),
            }
        }
    };
    Ok((LoweredArtifactPlan::GcFinalizer(plan), requests))
}

/// Requests the drop glue a payload/cell/element finalizer calls. The
/// finalizer only exists when its value needs drop, so a non-droppable type is
/// a requester/expander disagreement.
fn request_finalizer_glue(
    program: &LoweredProgram,
    value_type: &CheckedType,
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<PlannedArtifact, Vec<Diagnostic>> {
    if !program.concrete_needs_drop(value_type) {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!("gc finalizer value `{value_type}` does not need drop"),
        )]);
    }
    request_drop_glue(program, value_type, origin, requests)
}

/// The captures a closure-environment finalizer drops, in reverse capture
/// order, mirroring `ensure_closure_finalizer`: skip captures that require
/// initialization state, have mutable storage, are derived, or are borrowed,
/// then drop the rest that need drop.
fn closure_environment_drops(
    program: &LoweredProgram,
    closure: crate::FunctionInstanceId,
    captures: &[CheckedType],
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<Vec<DroppedCapture>, Vec<Diagnostic>> {
    let Some(instance) = program.instances.get(closure) else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "closure finalizer names missing function instance {}",
                closure.index()
            ),
        )]);
    };
    let Some(body) = instance.body.as_ref() else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "closure finalizer names function instance {} with no materialized body",
                closure.index()
            ),
        )]);
    };
    let body_captures = body.captures();
    if body_captures.len() != captures.len() {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "closure finalizer for instance {} lists {} captures but the closure has {}",
                closure.index(),
                captures.len(),
                body_captures.len()
            ),
        )]);
    }
    for (capture, expected) in body_captures.iter().zip(captures) {
        let actual = CanonicalType::concrete(&capture.value_type, origin)
            .map_err(|diagnostic| vec![diagnostic])?;
        let expected =
            CanonicalType::concrete(expected, origin).map_err(|diagnostic| vec![diagnostic])?;
        if actual != expected {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                format!(
                    "closure finalizer for instance {} lists a capture type that disagrees with the closure's own capture metadata",
                    closure.index()
                ),
            )]);
        }
    }
    let mut drops = Vec::new();
    for (index, capture) in body_captures.iter().enumerate().rev() {
        if capture.requires_initialization_state
            || capture.mutable_storage
            || capture.derived
            || capture.capture.borrowed
        {
            continue;
        }
        if !program.concrete_needs_drop(&capture.value_type) {
            continue;
        }
        let glue = request_finalizer_glue(program, &capture.value_type, origin, requests)?;
        drops.push(DroppedCapture {
            index,
            value_type: capture.value_type.clone(),
            glue,
        });
    }
    Ok(drops)
}

/// Builds one drop-glue body in the legacy decision order.
fn drop_glue_body(
    program: &LoweredProgram,
    value_type: &CheckedType,
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<DropGlueBody, Vec<Diagnostic>> {
    if let Some(method) = user_drop_method(program, value_type, origin, requests)? {
        let representation = match value_type {
            CheckedType::Distinct { representation, .. }
                if program.concrete_needs_drop(representation) =>
            {
                Some(request_drop_glue(
                    program,
                    representation,
                    origin,
                    requests,
                )?)
            }
            _ => None,
        };
        return Ok(DropGlueBody::UserDrop {
            method,
            representation,
        });
    }
    if let Some(kind) = program.runtime_opaque_kind(value_type) {
        return Ok(match kind {
            RuntimeOpaqueKind::Coroutine => DropGlueBody::CoroutineCleanup,
            RuntimeOpaqueKind::Scheduler => {
                DropGlueBody::RuntimeRelease(RuntimeRelease::SchedulerDestroy)
            }
            RuntimeOpaqueKind::Wait => DropGlueBody::RuntimeRelease(RuntimeRelease::WaitDrop),
            RuntimeOpaqueKind::Resolver => {
                DropGlueBody::RuntimeRelease(RuntimeRelease::ResolverDrop)
            }
            RuntimeOpaqueKind::CompletionToken => {
                DropGlueBody::RuntimeRelease(RuntimeRelease::CompletionTokenRelease)
            }
        });
    }
    match value_type {
        CheckedType::CString => Ok(DropGlueBody::CStringFree),
        CheckedType::Product(product) => {
            // Legacy iterates and drops fields in reverse element order,
            // skipping every field that does not need drop.
            let mut fields = Vec::new();
            for (index, element) in product.elements.iter().enumerate().rev() {
                if !program.concrete_needs_drop(&element.value_type) {
                    continue;
                }
                let glue = request_drop_glue(program, &element.value_type, origin, requests)?;
                fields.push(DroppedElement {
                    index,
                    value_type: element.value_type.clone(),
                    glue,
                });
            }
            Ok(DropGlueBody::Product { fields })
        }
        CheckedType::Sum(sum) => {
            // Legacy switches on the tag in alternative order and drops only
            // alternatives that need drop.
            let mut alternatives = Vec::new();
            for (index, alternative) in sum.alternatives.iter().enumerate() {
                if !program.concrete_needs_drop(alternative) {
                    continue;
                }
                let glue = request_drop_glue(program, alternative, origin, requests)?;
                alternatives.push(DroppedAlternative {
                    index,
                    value_type: alternative.clone(),
                    glue,
                });
            }
            Ok(DropGlueBody::Sum { alternatives })
        }
        CheckedType::Distinct { representation, .. } => {
            if !program.concrete_needs_drop(representation) {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "drop glue for `{value_type}` needs drop but no legacy branch applies to its representation"
                    ),
                )]);
            }
            let glue = request_drop_glue(program, representation, origin, requests)?;
            Ok(DropGlueBody::Distinct {
                representation: glue,
            })
        }
        other => Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!("drop glue for `{other}` needs drop but no legacy branch applies"),
        )]),
    }
}

/// Selects the user `Drop` method for a type and requests its instance, or
/// returns `None` when legacy would fall through to the opaque/structural
/// branches.
fn user_drop_method(
    program: &LoweredProgram,
    value_type: &CheckedType,
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<Option<PlannedInstance>, Vec<Diagnostic>> {
    let Some(function) = program.drop_method_for_concrete(value_type) else {
        return Ok(None);
    };
    // A matching implementation is non-generic by the exact-argument rule, so
    // the template signature is already concrete and needs no substitutions.
    let signature = function_signature(program, function, origin)?;
    let resolved = program
        .resolve_instance_request(&InstanceResolutionRequest {
            function,
            origin: origin.clone(),
            function_type: signature,
            substitutions: CallSubstitutions::default(),
            evidence: None,
            target: InstanceResolutionTarget::Root,
        })
        .map_err(|diagnostic| vec![diagnostic])?;
    requests.push(ClosureRequest::Instance {
        resolved: resolved.clone(),
        kind: LoweredInstanceDependencyKind::DropMethod,
        origin: origin.clone(),
        use_site: None,
    });
    Ok(Some(PlannedInstance {
        key: resolved.key,
        instance: None,
        kind: LoweredInstanceDependencyKind::DropMethod,
    }))
}

/// The template signature of one selected method function.
fn function_signature(
    program: &LoweredProgram,
    function: FunctionId,
    origin: &Origin,
) -> Result<crate::CheckedFunctionType, Vec<Diagnostic>> {
    program
        .functions
        .get(function)
        .map(|template| template.signature.clone())
        .ok_or_else(|| {
            vec![Diagnostic::new(
                origin.span.clone(),
                format!(
                    "selected drop method function {} has no lowered template",
                    function.0
                ),
            )]
        })
}

/// Requests one nested `DropGlue` artifact and returns its planned callee.
fn request_drop_glue(
    program: &LoweredProgram,
    value_type: &CheckedType,
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<PlannedArtifact, Vec<Diagnostic>> {
    debug_assert!(program.concrete_needs_drop(value_type));
    let canonical =
        CanonicalType::concrete(value_type, origin).map_err(|diagnostic| vec![diagnostic])?;
    let key = ArtifactRequestKey::DropGlue(canonical);
    requests.push(ClosureRequest::Artifact {
        key: key.clone(),
        plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
            value_type: value_type.clone(),
            body: DropGlueBody::Unexpanded,
        }),
        kind: LoweredArtifactDependencyKind::DropGlue,
        origin: origin.clone(),
        use_site: None,
    });
    Ok(PlannedArtifact {
        key,
        artifact: None,
        kind: LoweredArtifactDependencyKind::DropGlue,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use crate::specialization::{ArtifactRequestKey, CanonicalType};
    use crate::{
        ArenaId, CheckedType, DropGlueBody, DropGluePlan, FunctionInstanceId, LoweredArtifactPlan,
        LoweredArtifactRequestId, LoweredClosureCapture, LoweredInstanceDependencyKind,
        LoweredProgram, Lowerer, NameResolver, Origin, ProgramLoader, RuntimeRelease, TypeChecker,
        TypedModule,
    };

    use super::super::artifact_closure::{
        ArtifactFamilyHooks, ArtifactUseSite, ClosureRequest, ExpansionResult, ScanResult,
    };

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

    /// The Stage 3 program: graph built and materialized, closure not run.
    fn stage_three(source: &str) -> LoweredProgram {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        assert!(program.build_specialization_worklist().is_empty());
        assert!(program.materialize_instance_bodies().is_empty());
        program
    }

    /// The concrete checked type of one declared function's parameter, taken
    /// from its materialized instance body.
    fn parameter_type(program: &LoweredProgram, name: &str, index: usize) -> CheckedType {
        let template = program
            .functions
            .iter()
            .find(|(_, _, function)| {
                function.name == name || function.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"));
        program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == template)
            .and_then(|(_, instance)| instance.body.as_ref())
            .and_then(|body| body.parameters.get(index))
            .map(|parameter| parameter.value_type.clone())
            .unwrap_or_else(|| panic!("no parameter {index} on {name}"))
    }

    /// A hook set that scripts drop-glue requests onto one seed instance and
    /// delegates every `DropGlue` artifact to the production expander.
    struct CleanupHooks {
        seed: FunctionInstanceId,
        origin: Origin,
        types: Vec<(CheckedType, u32)>,
    }

    impl ArtifactFamilyHooks for CleanupHooks {
        fn scan_initializer(
            &self,
            _program: &LoweredProgram,
            _initializer: crate::InitializerId,
        ) -> ScanResult {
            Ok(Vec::new())
        }

        fn scan_instance(
            &self,
            _program: &LoweredProgram,
            instance: FunctionInstanceId,
        ) -> ScanResult {
            if instance != self.seed {
                return Ok(Vec::new());
            }
            let mut requests = Vec::new();
            for (value_type, site) in &self.types {
                let canonical = CanonicalType::concrete(value_type, &self.origin).unwrap_or_else(
                    |diagnostic| panic!("type {value_type:?} is not concrete: {diagnostic:?}"),
                );
                requests.push(ClosureRequest::Artifact {
                    key: ArtifactRequestKey::DropGlue(canonical),
                    plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
                        value_type: value_type.clone(),
                        body: DropGlueBody::Unexpanded,
                    }),
                    kind: super::super::LoweredArtifactDependencyKind::DropGlue,
                    origin: self.origin.clone(),
                    use_site: Some(ArtifactUseSite::Test(*site)),
                });
            }
            Ok(requests)
        }

        fn expand(
            &self,
            program: &LoweredProgram,
            artifact: LoweredArtifactRequestId,
        ) -> ExpansionResult {
            let record = program.artifacts.get(artifact).expect("artifact");
            let plan = record.plan.clone().expect("plan");
            match program.specializations.artifact(record.ordinal) {
                Some(ArtifactRequestKey::DropGlue(_)) => {
                    let LoweredArtifactPlan::DropGlue(plan) = plan else {
                        unreachable!("a drop-glue key carries a drop-glue plan")
                    };
                    super::expand_drop_glue(program, artifact, plan)
                }
                _ => Ok((plan, Vec::new())),
            }
        }

        fn expands_body(&self, key: &ArtifactRequestKey) -> bool {
            matches!(key, ArtifactRequestKey::DropGlue(_))
        }
    }

    fn drop_glue_plan<'a>(
        program: &'a LoweredProgram,
        value_type: &CheckedType,
    ) -> &'a DropGluePlan {
        let origin = Origin::compiler();
        let canonical = CanonicalType::concrete(value_type, &origin).expect("concrete type");
        let ordinal = program
            .specializations
            .artifact_ordinal(&ArtifactRequestKey::DropGlue(canonical))
            .expect("the drop-glue key is interned");
        let artifact = program
            .artifacts
            .iter()
            .find(|(_, record)| record.ordinal == ordinal)
            .map(|(_, record)| record)
            .expect("the artifact record exists");
        match artifact.plan.as_ref().expect("expanded plan") {
            LoweredArtifactPlan::DropGlue(plan) => plan,
            other => panic!("expected a drop-glue plan, got {other:?}"),
        }
    }

    const CLEANUP_FIXTURE: &str = concat!(
        "use std.cinterop.(CString, c_string)\n",
        "use std.coroutine.*\n",
        "type Resource = ctor I32\n",
        "impl Drop Resource { def drop = Resource value => () }\n",
        "type Handle = ctor CString\n",
        "impl Drop Handle { def drop = Handle value => () }\n",
        "type Wrapped = ctor CString\n",
        "type Box T = ctor (T)\n",
        "impl<T where Copy T> Drop (Box T) { def drop = Box value => () }\n",
        "type Chain = ctor ((CString) | (Ref Chain))\n",
        "def expose_resource: Resource -> I32 = value => 0\n",
        "def expose_handle: Handle -> I32 = value => 0\n",
        "def expose_wrapped: Wrapped -> I32 = value => 0\n",
        "def expose_box: (Box CString) -> I32 = value => 0\n",
        "def expose_box_handle: (Box Handle) -> I32 = value => 0\n",
        "def expose_product: (I32, CString) -> I32 = value => 0\n",
        "def expose_nested: ((I32, CString), I32) -> I32 = value => 0\n",
        "def expose_sum: (CString | I32) -> I32 = value => 0\n",
        "def expose_chain: Chain -> I32 = value => 0\n",
        "def expose_coroutine: (Coroutine{} I32) -> I32 = value => 0\n",
        "def expose_scheduler: Scheduler -> I32 = value => 0\n",
        "def expose_wait: (Wait I32) -> I32 = value => 0\n",
        "def expose_resolver: (Resolver I32) -> I32 = value => 0\n",
        "def expose_token: CompletionToken -> I32 = value => 0\n",
        "let kept = 1\n",
    );

    #[test]
    fn drop_glue_bodies_mirror_the_legacy_decision_order() {
        let module = checked_program(CLEANUP_FIXTURE);
        let mut program = stage_three(CLEANUP_FIXTURE);
        let seed = FunctionInstanceId::from_index(0);
        let origin = program
            .instances
            .get(seed)
            .expect("seed instance")
            .origin
            .clone();
        let resource = parameter_type(&program, "expose_resource", 0);
        let handle = parameter_type(&program, "expose_handle", 0);
        let wrapped = parameter_type(&program, "expose_wrapped", 0);
        let box_c_string = parameter_type(&program, "expose_box", 0);
        let box_handle = parameter_type(&program, "expose_box_handle", 0);
        let product = parameter_type(&program, "expose_product", 0);
        let nested = parameter_type(&program, "expose_nested", 0);
        let sum = parameter_type(&program, "expose_sum", 0);
        let chain = parameter_type(&program, "expose_chain", 0);
        let coroutine = parameter_type(&program, "expose_coroutine", 0);
        let scheduler = parameter_type(&program, "expose_scheduler", 0);
        let wait = parameter_type(&program, "expose_wait", 0);
        let resolver = parameter_type(&program, "expose_resolver", 0);
        let token = parameter_type(&program, "expose_token", 0);

        let mut types = vec![(CheckedType::CString, 0)];
        for (site, value_type) in [
            resource.clone(),
            handle.clone(),
            wrapped.clone(),
            box_c_string.clone(),
            box_handle.clone(),
            product.clone(),
            nested.clone(),
            sum.clone(),
            chain.clone(),
            coroutine.clone(),
            scheduler.clone(),
            wait.clone(),
            resolver.clone(),
            token.clone(),
        ]
        .into_iter()
        .enumerate()
        {
            types.push((value_type, site as u32 + 1));
        }
        let hooks = CleanupHooks {
            seed,
            origin,
            types,
        };
        let diagnostics = program.close_artifact_catalog(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        // CString: the `free` branch.
        assert_eq!(
            drop_glue_plan(&program, &CheckedType::CString).body,
            DropGlueBody::CStringFree
        );

        // User `Drop` on a non-represented distinct: no representation glue.
        let plan = drop_glue_plan(&program, &resource);
        let DropGlueBody::UserDrop {
            method,
            representation,
        } = &plan.body
        else {
            panic!("Resource selects a user drop: {:?}", plan.body);
        };
        assert!(representation.is_none(), "I32 does not need drop");
        let selected = module
            .drop_method_for(&resource)
            .expect("the typed module selects the Resource drop method");
        let bound = program
            .instances
            .get(method.instance.expect("bound after closure"))
            .expect("method instance");
        assert_eq!(bound.template, selected);
        assert_eq!(
            method.kind,
            LoweredInstanceDependencyKind::DropMethod,
            "the selected method edge is a drop-method edge"
        );

        // User `Drop` on a represented distinct: the representation glue is the
        // nested CString glue and is only requested because CString needs drop.
        let plan = drop_glue_plan(&program, &handle);
        let DropGlueBody::UserDrop {
            method,
            representation,
        } = &plan.body
        else {
            panic!("Handle selects a user drop: {:?}", plan.body);
        };
        assert!(method.instance.is_some());
        let representation = representation
            .as_ref()
            .expect("the CString representation needs drop");
        let representation_ordinal = drop_glue_ordinal(&program, representation);
        assert_eq!(
            program.specializations.artifact(representation_ordinal),
            Some(&ArtifactRequestKey::DropGlue(
                CanonicalType::concrete(&CheckedType::CString, &Origin::compiler())
                    .expect("concrete")
            ))
        );

        // A represented distinct with no user `Drop`: the distinct branch.
        let plan = drop_glue_plan(&program, &wrapped);
        let DropGlueBody::Distinct { representation } = &plan.body else {
            panic!("Wrapped selects the distinct branch: {:?}", plan.body);
        };
        assert_eq!(
            program
                .specializations
                .artifact(drop_glue_ordinal(&program, representation)),
            Some(&ArtifactRequestKey::DropGlue(
                CanonicalType::concrete(&CheckedType::CString, &Origin::compiler())
                    .expect("concrete")
            ))
        );

        // Product: only droppable fields, in reverse element order.
        let plan = drop_glue_plan(&program, &product);
        let DropGlueBody::Product { fields } = &plan.body else {
            panic!("a product selects the product branch: {:?}", plan.body);
        };
        assert_eq!(fields.len(), 1, "I32 fields are skipped");
        assert_eq!(fields[0].index, 1, "the CString field is index 1");
        assert_eq!(fields[0].value_type, CheckedType::CString);

        // Nested products: the outer field glue is the inner product's glue.
        let plan = drop_glue_plan(&program, &nested);
        let DropGlueBody::Product { fields } = &plan.body else {
            panic!(
                "a nested product selects the product branch: {:?}",
                plan.body
            );
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].index, 0);
        assert_eq!(fields[0].value_type, product);
        let nested_ordinal = drop_glue_ordinal(&program, &fields[0].glue);
        let nested_plan = match program
            .artifacts
            .iter()
            .find(|(_, record)| record.ordinal == nested_ordinal)
            .and_then(|(_, record)| record.plan.as_ref())
        {
            Some(LoweredArtifactPlan::DropGlue(plan)) => plan,
            other => panic!("expected a nested drop-glue plan, got {other:?}"),
        };
        assert!(matches!(nested_plan.body, DropGlueBody::Product { .. }));

        // Sum: only droppable alternatives, in tag order.
        let plan = drop_glue_plan(&program, &sum);
        let DropGlueBody::Sum { alternatives } = &plan.body else {
            panic!("a sum selects the sum branch: {:?}", plan.body);
        };
        assert_eq!(alternatives.len(), 1, "I32 alternatives are skipped");
        assert_eq!(alternatives[0].index, 0);
        assert_eq!(alternatives[0].value_type, CheckedType::CString);

        // A recursive nominal type through a reference: the reference field is
        // skipped, so the glue body terminates on the CString.
        let plan = drop_glue_plan(&program, &chain);
        let DropGlueBody::Distinct { representation } = &plan.body else {
            panic!("Chain selects the distinct branch: {:?}", plan.body);
        };
        let chain_representation = drop_glue_ordinal(&program, representation);
        let chain_representation_plan = match program
            .artifacts
            .iter()
            .find(|(_, record)| record.ordinal == chain_representation)
            .and_then(|(_, record)| record.plan.as_ref())
        {
            Some(LoweredArtifactPlan::DropGlue(plan)) => plan,
            other => panic!("expected the chain representation glue, got {other:?}"),
        };
        let DropGlueBody::Sum { alternatives } = &chain_representation_plan.body else {
            panic!(
                "Chain's representation is a sum: {:?}",
                chain_representation_plan.body
            );
        };
        assert_eq!(alternatives.len(), 1);
        assert_eq!(alternatives[0].value_type, CheckedType::CString);

        // Runtime opaques: the dedicated cleanup routes.
        assert_eq!(
            drop_glue_plan(&program, &coroutine).body,
            DropGlueBody::CoroutineCleanup
        );
        assert_eq!(
            drop_glue_plan(&program, &scheduler).body,
            DropGlueBody::RuntimeRelease(RuntimeRelease::SchedulerDestroy)
        );
        assert_eq!(
            drop_glue_plan(&program, &wait).body,
            DropGlueBody::RuntimeRelease(RuntimeRelease::WaitDrop)
        );
        assert_eq!(
            drop_glue_plan(&program, &resolver).body,
            DropGlueBody::RuntimeRelease(RuntimeRelease::ResolverDrop)
        );
        assert_eq!(
            drop_glue_plan(&program, &token).body,
            DropGlueBody::RuntimeRelease(RuntimeRelease::CompletionTokenRelease)
        );

        // Two generic instantiations produce two keys, and a generic `Drop`
        // implementation is never selected for a concrete type.
        assert_ne!(
            CanonicalType::concrete(&box_c_string, &Origin::compiler()).expect("concrete"),
            CanonicalType::concrete(&box_handle, &Origin::compiler()).expect("concrete")
        );
        for box_type in [&box_c_string, &box_handle] {
            assert!(
                module.drop_method_for(box_type).is_none(),
                "a generic `Drop` implementation never matches a concrete type"
            );
            let plan = drop_glue_plan(&program, box_type);
            assert!(
                matches!(plan.body, DropGlueBody::Distinct { .. }),
                "Box CString selects the distinct branch: {:?}",
                plan.body
            );
        }

        // Every expanded key passes the transition gate against the typed
        // module: needs-drop gating and user-drop selection agree exactly.
        for (_, artifact) in program.artifacts.iter() {
            let Some(LoweredArtifactPlan::DropGlue(plan)) = &artifact.plan else {
                continue;
            };
            assert!(
                module.type_needs_drop(&plan.value_type),
                "a drop-glue key is only requested for a droppable type"
            );
            let legacy = module.drop_method_for(&plan.value_type);
            match &plan.body {
                DropGlueBody::UserDrop { method, .. } => {
                    let expected = legacy.expect("a planned user drop is the legacy selection");
                    let bound = program
                        .instances
                        .get(method.instance.expect("bound after closure"))
                        .expect("method instance");
                    assert_eq!(bound.template, expected);
                }
                DropGlueBody::Unexpanded => panic!("drop glue was never expanded"),
                _ => {
                    assert!(
                        legacy.is_none(),
                        "a non-user body is only planned when no user drop matches"
                    );
                }
            }
        }
    }

    /// Resolves the artifact ordinal of a bound planned glue callee.
    fn drop_glue_ordinal(
        _program: &LoweredProgram,
        glue: &super::PlannedArtifact,
    ) -> crate::specialization::ArtifactOrdinal {
        glue.artifact.expect("bound after closure")
    }

    #[test]
    fn drop_glue_converges_and_reports_closure_stats() {
        let source = concat!(
            "type Resource = ctor I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
            "def mutate_resource: move (Resource, Resource) -> (Resource, Resource) = move pair => {\n",
            "  let mut copy = pair\n",
            "  copy[0] = Resource 3\n",
            "  copy\n",
            "}\n",
            "let replaced = mutate_resource (Resource 1, Resource 2)\n",
        );
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("source should lower through the production closure");
        let stats = lowered
            .program
            .closure_stats
            .expect("the production closure records its stats");
        assert!(stats.rounds >= 1, "{stats:?}");
        assert!(
            stats.rounds <= 4,
            "drop glue converges in a few rounds: {stats:?}"
        );
        assert!(
            stats.growth <= 16,
            "drop glue growth stays small: {stats:?}"
        );
        // Every stage-4.3-requested drop glue is expanded.
        for (_, artifact) in lowered.program.artifacts.iter() {
            if let Some(LoweredArtifactPlan::DropGlue(plan)) = &artifact.plan {
                assert!(
                    !matches!(plan.body, DropGlueBody::Unexpanded),
                    "drop glue {} was expanded",
                    artifact.ordinal.index()
                );
            }
        }
        eprintln!("stage 4.4 drop-glue closure stats: {stats:?}");
    }

    // ------------------------------------------------------------------
    // Stage 4.4 gc-finalizer fixtures.
    // ------------------------------------------------------------------

    use crate::GcFinalizerPlan;
    use crate::specialization::GcFinalizerKey;

    /// A hook set that scripts exact finalizer requests onto one seed instance
    /// and delegates both cleanup families to the production expanders.
    struct FinalizerHooks {
        seed: FunctionInstanceId,
        origin: Origin,
        requests: Vec<ClosureRequest>,
    }

    impl ArtifactFamilyHooks for FinalizerHooks {
        fn scan_initializer(
            &self,
            _program: &LoweredProgram,
            _initializer: crate::InitializerId,
        ) -> ScanResult {
            Ok(Vec::new())
        }

        fn scan_instance(
            &self,
            _program: &LoweredProgram,
            instance: FunctionInstanceId,
        ) -> ScanResult {
            if instance != self.seed {
                return Ok(Vec::new());
            }
            Ok(self.requests.clone())
        }

        fn expand(
            &self,
            program: &LoweredProgram,
            artifact: LoweredArtifactRequestId,
        ) -> ExpansionResult {
            let record = program.artifacts.get(artifact).expect("artifact");
            let plan = record.plan.clone().expect("plan");
            match (
                program.specializations.artifact(record.ordinal),
                plan.clone(),
            ) {
                (Some(ArtifactRequestKey::DropGlue(_)), LoweredArtifactPlan::DropGlue(plan)) => {
                    super::expand_drop_glue(program, artifact, plan)
                }
                (
                    Some(ArtifactRequestKey::GcFinalizer(_)),
                    LoweredArtifactPlan::GcFinalizer(plan),
                ) => super::expand_gc_finalizer(program, artifact, plan),
                _ => Ok((plan, Vec::new())),
            }
        }

        fn expands_body(&self, key: &ArtifactRequestKey) -> bool {
            matches!(
                key,
                ArtifactRequestKey::DropGlue(_) | ArtifactRequestKey::GcFinalizer(_)
            )
        }
    }

    const FINALIZER_FIXTURE: &str = concat!(
        "use std.cinterop.*\n",
        "extern \"c\" { inspect: CString -> I32 }\n",
        "type Owned = ctor CString\n",
        "type CellValue = ctor CString\n",
        "type BorrowedValue = ctor CString\n",
        "type Wrapped = ctor CString\n",
        "type DerivedValue = ctor CString\n",
        "def make_owned = (move value: Owned) => { let callback = () => inspect (value.*); callback }\n",
        "def make_mutable = () => {\n",
        "  let mut cell = CellValue (c_string \"a\")\n",
        "  cell = CellValue (c_string \"b\")\n",
        "  let callback = () => inspect (cell.*)\n",
        "  callback\n",
        "}\n",
        "def use_borrowed = (value: BorrowedValue) => { let callback = () => inspect (value.*); callback () }\n",
        "def peek: <T> T -> I32 = _ => 0\n",
        "def make_generic: <T> move T -> (() -> I32) = move value => () => peek value\n",
        "def use_derived = () => {\n",
        "  let signal count = 1\n",
        "  let derived_value = when { count > 0 => DerivedValue (c_string \"x\"), else => DerivedValue (c_string \"y\") }\n",
        "  let callback = () => peek derived_value\n",
        "  callback ()\n",
        "}\n",
        "let owned = make_owned (Owned (c_string \"owned\"))\n",
        "let mutable = make_mutable ()\n",
        "let borrowed = use_borrowed (BorrowedValue (c_string \"borrowed\"))\n",
        "let generic_string = make_generic (c_string \"generic\")\n",
        "let generic_wrapped = make_generic (Wrapped (c_string \"wrapped\"))\n",
    );

    /// The first closure instance whose own captures include `capture`.
    fn closure_instance_capturing(
        program: &LoweredProgram,
        capture: &CheckedType,
    ) -> FunctionInstanceId {
        closure_instance_capturing_filtered(program, capture, |_| true)
    }

    /// The first closure instance with a capture of `capture`'s type that also
    /// satisfies `filter`; disambiguates closures sharing one capture type.
    fn closure_instance_capturing_filtered(
        program: &LoweredProgram,
        capture: &CheckedType,
        filter: impl Fn(&crate::LoweredInstanceCapture) -> bool,
    ) -> FunctionInstanceId {
        program
            .instances
            .iter()
            .find_map(|(id, instance)| {
                let body = instance.body.as_ref()?;
                body.captures()
                    .iter()
                    .any(|candidate| &candidate.value_type == capture && filter(candidate))
                    .then_some(id)
            })
            .unwrap_or_else(|| panic!("no closure instance captures {capture:?}"))
    }

    fn finalizer_request(
        key: GcFinalizerKey,
        plan: GcFinalizerPlan,
        origin: &Origin,
        site: u32,
    ) -> ClosureRequest {
        ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(key),
            plan: LoweredArtifactPlan::GcFinalizer(plan),
            kind: super::super::LoweredArtifactDependencyKind::GcFinalizer,
            origin: origin.clone(),
            use_site: Some(ArtifactUseSite::Test(site)),
        }
    }

    fn finalizer_plan<'a>(
        program: &'a LoweredProgram,
        key: &GcFinalizerKey,
    ) -> &'a GcFinalizerPlan {
        let ordinal = program
            .specializations
            .artifact_ordinal(&ArtifactRequestKey::GcFinalizer(key.clone()))
            .expect("the finalizer key is interned");
        let artifact = program
            .artifacts
            .iter()
            .find(|(_, record)| record.ordinal == ordinal)
            .map(|(_, record)| record)
            .expect("the artifact record exists");
        match artifact.plan.as_ref().expect("expanded plan") {
            LoweredArtifactPlan::GcFinalizer(plan) => plan,
            other => panic!("expected a gc-finalizer plan, got {other:?}"),
        }
    }

    fn assert_finalizer_glue(
        program: &LoweredProgram,
        glue: &super::PlannedArtifact,
        expected: &CheckedType,
    ) {
        let ordinal = drop_glue_ordinal(program, glue);
        assert_eq!(
            program.specializations.artifact(ordinal),
            Some(&ArtifactRequestKey::DropGlue(
                CanonicalType::concrete(expected, &Origin::compiler()).expect("concrete")
            )),
            "the finalizer's glue is the expected drop-glue key"
        );
    }

    #[test]
    fn finalizer_bodies_reference_their_drop_glue_and_capture_drops() {
        let mut program = stage_three(FINALIZER_FIXTURE);
        let seed = FunctionInstanceId::from_index(0);
        let origin = program
            .instances
            .get(seed)
            .expect("seed instance")
            .origin
            .clone();

        let owned_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "Owned"),
            name: "Owned".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };
        let cell_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "CellValue"),
            name: "CellValue".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };
        let borrowed_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "BorrowedValue"),
            name: "BorrowedValue".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };
        let derived_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "DerivedValue"),
            name: "DerivedValue".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };
        let wrapped_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "Wrapped"),
            name: "Wrapped".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };

        let owned_closure = closure_instance_capturing(&program, &owned_capture);
        let mutable_closure = closure_instance_capturing(&program, &cell_capture);
        let borrowed_closure = closure_instance_capturing(&program, &borrowed_capture);
        let generic_string_closure = closure_instance_capturing(&program, &CheckedType::CString);
        let generic_wrapped_closure = closure_instance_capturing(&program, &wrapped_capture);
        let derived_closure =
            closure_instance_capturing_filtered(&program, &derived_capture, |capture| {
                capture.derived
            });
        assert_ne!(
            generic_string_closure, generic_wrapped_closure,
            "two generic instantiations produce two closure instances"
        );

        let request_for = |closure: FunctionInstanceId,
                           capture: &CheckedType,
                           plan: GcFinalizerPlan,
                           site: u32| {
            let ordinal = program
                .instances
                .get(closure)
                .expect("closure instance")
                .ordinal;
            let canonical = CanonicalType::concrete(capture, &origin).expect("concrete capture");
            finalizer_request(
                GcFinalizerKey::ClosureEnvironment {
                    closure: ordinal,
                    captures: vec![canonical],
                },
                plan,
                &origin,
                site,
            )
        };
        let requests = vec![
            finalizer_request(
                GcFinalizerKey::Payload(
                    CanonicalType::concrete(&CheckedType::CString, &origin).expect("concrete"),
                ),
                GcFinalizerPlan::Payload {
                    value_type: CheckedType::CString,
                    glue: None,
                },
                &origin,
                0,
            ),
            finalizer_request(
                GcFinalizerKey::Cell(
                    CanonicalType::concrete(&owned_capture, &origin).expect("concrete"),
                ),
                GcFinalizerPlan::Cell {
                    value_type: owned_capture.clone(),
                    glue: None,
                },
                &origin,
                1,
            ),
            finalizer_request(
                GcFinalizerKey::Buffer(
                    CanonicalType::concrete(&cell_capture, &origin).expect("concrete"),
                ),
                GcFinalizerPlan::Buffer {
                    element: cell_capture.clone(),
                    glue: None,
                },
                &origin,
                2,
            ),
            request_for(
                owned_closure,
                &owned_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: owned_closure,
                    captures: vec![owned_capture.clone()],
                    drops: None,
                },
                3,
            ),
            request_for(
                mutable_closure,
                &cell_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: mutable_closure,
                    captures: vec![cell_capture.clone()],
                    drops: None,
                },
                4,
            ),
            request_for(
                borrowed_closure,
                &borrowed_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: borrowed_closure,
                    captures: vec![borrowed_capture.clone()],
                    drops: None,
                },
                5,
            ),
            request_for(
                generic_string_closure,
                &CheckedType::CString,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: generic_string_closure,
                    captures: vec![CheckedType::CString],
                    drops: None,
                },
                6,
            ),
            request_for(
                generic_wrapped_closure,
                &wrapped_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: generic_wrapped_closure,
                    captures: vec![wrapped_capture.clone()],
                    drops: None,
                },
                7,
            ),
            request_for(
                derived_closure,
                &derived_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: derived_closure,
                    captures: vec![derived_capture.clone()],
                    drops: None,
                },
                8,
            ),
        ];
        let hooks = FinalizerHooks {
            seed,
            origin,
            requests,
        };
        let diagnostics = program.close_artifact_catalog(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        // Payload/Cell/Buffer: every expanded finalizer has bound glue.
        for (key, expected) in [
            (
                GcFinalizerKey::Payload(canonical(&CheckedType::CString)),
                CheckedType::CString,
            ),
            (
                GcFinalizerKey::Cell(canonical(&owned_capture)),
                owned_capture.clone(),
            ),
            (
                GcFinalizerKey::Buffer(canonical(&cell_capture)),
                cell_capture.clone(),
            ),
        ] {
            match finalizer_plan(&program, &key) {
                GcFinalizerPlan::Payload { glue, .. }
                | GcFinalizerPlan::Cell { glue, .. }
                | GcFinalizerPlan::Buffer { glue, .. } => {
                    let glue = glue.as_ref().expect("the finalizer is expanded");
                    assert_finalizer_glue(&program, glue, &expected);
                }
                other => panic!("unexpected finalizer plan {other:?}"),
            }
        }

        // The by-value droppable capture is dropped by its closure finalizer.
        let owned_key = GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(owned_closure)
                .expect("instance")
                .ordinal,
            captures: vec![canonical(&owned_capture)],
        };
        let GcFinalizerPlan::ClosureEnvironment { drops, .. } =
            finalizer_plan(&program, &owned_key)
        else {
            panic!("expected a closure-environment plan")
        };
        let drops = drops.as_ref().expect("expanded");
        assert_eq!(drops.len(), 1, "Owned is dropped: {drops:?}");
        assert_eq!(drops[0].index, 0);
        assert_finalizer_glue(&program, &drops[0].glue, &owned_capture);

        // The mutable capture fires the install gate but the body drops
        // nothing (legacy `has_mutable_storage`).
        let mutable_key = GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(mutable_closure)
                .expect("instance")
                .ordinal,
            captures: vec![canonical(&cell_capture)],
        };
        let GcFinalizerPlan::ClosureEnvironment { drops, .. } =
            finalizer_plan(&program, &mutable_key)
        else {
            panic!("expected a closure-environment plan")
        };
        assert!(
            drops.as_ref().expect("expanded").is_empty(),
            "a mutable capture is skipped by the finalizer body"
        );
        let mutable_site = closure_site_capture(&program, mutable_closure, &cell_capture);
        assert!(
            mutable_site.mutable_storage,
            "the capture has mutable storage"
        );
        assert!(
            !mutable_site.requires_initialization_state && !mutable_site.capture.borrowed,
            "the install gate is not excluded by initialization or borrowing"
        );

        // A borrowed capture is skipped by the body and excluded by the gate.
        let borrowed_key = GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(borrowed_closure)
                .expect("instance")
                .ordinal,
            captures: vec![canonical(&borrowed_capture)],
        };
        let GcFinalizerPlan::ClosureEnvironment { drops, .. } =
            finalizer_plan(&program, &borrowed_key)
        else {
            panic!("expected a closure-environment plan")
        };
        assert!(
            drops.as_ref().expect("expanded").is_empty(),
            "a borrowed capture is skipped"
        );
        let borrowed_site = closure_site_capture(&program, borrowed_closure, &borrowed_capture);
        assert!(borrowed_site.capture.borrowed, "the capture is borrowed");

        // A derived capture fires the gate but the body skips it.
        let derived_key = GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(derived_closure)
                .expect("instance")
                .ordinal,
            captures: vec![canonical(&derived_capture)],
        };
        let GcFinalizerPlan::ClosureEnvironment { drops, .. } =
            finalizer_plan(&program, &derived_key)
        else {
            panic!("expected a closure-environment plan")
        };
        assert!(
            drops.as_ref().expect("expanded").is_empty(),
            "a derived capture is skipped by the finalizer body"
        );
        let derived_site = closure_site_capture(&program, derived_closure, &derived_capture);
        assert!(derived_site.derived, "the capture is a derived binding");

        // Two generic instantiations produce two plans that each drop their own
        // capture.
        for (closure, capture) in [
            (generic_string_closure, CheckedType::CString),
            (generic_wrapped_closure, wrapped_capture.clone()),
        ] {
            let key = GcFinalizerKey::ClosureEnvironment {
                closure: program.instances.get(closure).expect("instance").ordinal,
                captures: vec![canonical(&capture)],
            };
            let GcFinalizerPlan::ClosureEnvironment { drops, .. } = finalizer_plan(&program, &key)
            else {
                panic!("expected a closure-environment plan")
            };
            let drops = drops.as_ref().expect("expanded");
            assert_eq!(drops.len(), 1, "{capture:?}: {drops:?}");
            assert_finalizer_glue(&program, &drops[0].glue, &capture);
        }

        // Every planned dropped capture agrees with the construction site's
        // `drops_value`, and the install gate matches for every Fresh closure
        // construction.
        for (id, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for (_, value) in body.callable_values.iter() {
                let Some(closure) = &value.closure else {
                    continue;
                };
                if closure.environment != crate::LoweredClosureEnvironment::Fresh
                    || closure.captures.is_empty()
                {
                    continue;
                }
                let closure_instance = closure_instance_capturing_types(
                    &program,
                    closure.function,
                    closure.captures.iter().map(|capture| &capture.value_type),
                )
                .unwrap_or_else(|| {
                    panic!(
                        "closure in instance {} has no interned instance",
                        id.index()
                    )
                });
                let key = GcFinalizerKey::ClosureEnvironment {
                    closure: program
                        .instances
                        .get(closure_instance)
                        .expect("closure instance")
                        .ordinal,
                    captures: closure
                        .captures
                        .iter()
                        .map(|capture| canonical(&capture.value_type))
                        .collect(),
                };
                let plan = program
                    .specializations
                    .artifact_ordinal(&ArtifactRequestKey::GcFinalizer(key))
                    .map(|ordinal| {
                        program
                            .artifacts
                            .iter()
                            .find(|(_, record)| record.ordinal == ordinal)
                            .map(|(_, record)| record)
                            .expect("artifact")
                    })
                    .and_then(|artifact| match artifact.plan.as_ref() {
                        Some(LoweredArtifactPlan::GcFinalizer(
                            GcFinalizerPlan::ClosureEnvironment { drops, .. },
                        )) => drops.clone(),
                        _ => None,
                    });
                let gate = closure.captures.iter().any(|capture| {
                    !capture.requires_initialization_state
                        && !capture.capture.borrowed
                        && program.concrete_needs_drop(&capture.value_type)
                });
                if gate {
                    let drops = plan.unwrap_or_else(|| {
                        panic!(
                            "the install gate fired for a closure in instance {} but no finalizer was requested",
                            id.index()
                        )
                    });
                    let dropped = drops.iter().map(|drop| drop.index).collect::<HashSet<_>>();
                    let expected = closure
                        .captures
                        .iter()
                        .enumerate()
                        .filter(|(_, capture)| capture.drops_value)
                        .map(|(index, _)| index)
                        .collect::<HashSet<_>>();
                    assert_eq!(
                        dropped,
                        expected,
                        "the finalizer's drops agree with the construction site's drops_value in instance {}",
                        id.index()
                    );
                } else if let Some(drops) = plan {
                    // The gate excludes this construction, so legacy never
                    // installs the finalizer; a plan requested by a
                    // transition fixture must still drop nothing.
                    assert!(
                        drops.is_empty(),
                        "a finalizer for a gate-excluded closure drops nothing in instance {}",
                        id.index()
                    );
                }
            }
        }
        eprintln!(
            "stage 4.4 finalizer closure stats: {:?}",
            program.closure_stats
        );
    }

    fn nominal_type_id(program: &LoweredProgram, name: &str) -> crate::TypeId {
        program
            .types
            .iter()
            .find(|(_, _, metadata)| metadata.name == name)
            .map(|(_, _, metadata)| metadata.semantic_id)
            .unwrap_or_else(|| panic!("no lowered type named {name}"))
    }

    fn canonical(value_type: &CheckedType) -> CanonicalType {
        CanonicalType::concrete(value_type, &Origin::compiler()).expect("a concrete type")
    }

    fn closure_site_capture<'a>(
        program: &'a LoweredProgram,
        closure: FunctionInstanceId,
        capture: &CheckedType,
    ) -> &'a LoweredClosureCapture {
        let template = program
            .instances
            .get(closure)
            .expect("closure instance")
            .template;
        for (_, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for (_, value) in body.callable_values.iter() {
                let Some(construction) = &value.closure else {
                    continue;
                };
                if construction.function != template {
                    continue;
                }
                if let Some(found) = construction
                    .captures
                    .iter()
                    .find(|candidate| &candidate.value_type == capture)
                {
                    return found;
                }
            }
        }
        panic!("no construction site captures {capture:?}")
    }

    fn closure_instance_capturing_types<'a>(
        program: &'a LoweredProgram,
        function: crate::FunctionId,
        capture_types: impl Iterator<Item = &'a CheckedType>,
    ) -> Option<FunctionInstanceId> {
        let expected = capture_types.cloned().collect::<Vec<_>>();
        program.instances.iter().find_map(|(id, instance)| {
            if instance.template != function {
                return None;
            }
            let body = instance.body.as_ref()?;
            let captures = body
                .captures()
                .iter()
                .map(|capture| capture.value_type.clone())
                .collect::<Vec<_>>();
            (captures == expected).then_some(id)
        })
    }
}
