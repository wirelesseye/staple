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
    CallSubstitutions, DropGlueBody, DropGluePlan, DroppedAlternative, DroppedElement,
    LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId,
    LoweredInstanceDependencyKind, LoweredProgram, Origin, PlannedArtifact, PlannedInstance,
    RuntimeRelease,
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
    use std::path::{Path, PathBuf};

    use crate::specialization::{ArtifactRequestKey, CanonicalType};
    use crate::{
        ArenaId, CheckedType, DropGlueBody, DropGluePlan, FunctionInstanceId, LoweredArtifactPlan,
        LoweredArtifactRequestId, LoweredInstanceDependencyKind, LoweredProgram, Lowerer,
        NameResolver, Origin, ProgramLoader, RuntimeRelease, TypeChecker, TypedModule,
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
}
