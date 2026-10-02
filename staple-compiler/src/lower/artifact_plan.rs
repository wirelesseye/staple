//! Stage 4 generated-artifact plans.
//!
//! Every generated artifact carries an owned, typed `LoweredArtifactPlan`
//! recording the decisions the legacy backend makes while emitting it. Stage
//! 4.1 defines one placeholder variant per artifact family together with the
//! concrete inputs already available at request time; Stage 4.3 through 4.6
//! fill each variant's remaining fields (selected callees, ordered layout
//! facts, and cleanup facts) rather than adding an untyped fallback. The
//! exhaustive matches on `family_name` force every new artifact family to
//! declare its plan variant here.
//!
//! A plan built at request time (`ConstructorConstruction::Unexpanded`,
//! `StructuralBody::Unexpanded`) is the minimal form: it carries every
//! identity field `matches_key` rebuilds but no body decisions, because body
//! building belongs to the family expander. Expansion replaces the marker
//! with the owned plan; validation rejects a plan whose family expander ran
//! but left the marker in place.
//!
//! Callees whose catalog ids do not exist yet during expansion are named by
//! key (`PlannedInstance`/`PlannedArtifact`) with the id left empty. The
//! closure engine binds them after the catalog reaches a fixed point.
//!
//! Target-specific LLVM layout stays in the backend; a plan records lowered
//! identities and concrete checked values only.

use super::{
    ArenaId, FunctionInstanceId, LoweredArtifactDependencyKind, LoweredCallableAdapter,
    LoweredInstanceDependencyKind,
};
use crate::specialization::{
    ArtifactOrdinal, ArtifactRequestKey, ArtifactSite, ArtifactSiteOwner, CanonicalAdapterKind,
    CanonicalFunctionType, CanonicalType, GcFinalizerKey, InstanceKey,
};
use crate::{
    CheckedFunctionType, CheckedResource, CheckedType, Origin, StructuralTraitMethod, SymbolId,
    TraitId, TraitMethodId, TypeId,
};

/// The owned plan of one generated artifact, one variant per artifact family.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LoweredArtifactPlan {
    /// A constructor value's callable adapter. Stage 4.3 adds the recursive
    /// construction class and the parameter-to-representation mapping.
    ConstructorAdapter(ConstructorAdapterPlan),
    /// A compiler-generated structural method. Stage 4.3 adds the ordered
    /// labels, element types, selected callees, and formatting edges.
    StructuralMethod(StructuralMethodPlan),
    /// Drop glue for one concrete value type. Stage 4.4 adds the selected
    /// `Drop` method and the ordered representation cleanup steps.
    DropGlue(DropGluePlan),
    /// A garbage-collector finalizer. Stage 4.4 adds the referenced drop glue.
    GcFinalizer(GcFinalizerPlan),
    /// A coroutine `resume`/`cleanup` pair. Stage 4.5 adds the frame layout
    /// inputs, cleanup drop glue, and thunk-environment finalizer.
    CoroutineCodes(CoroutineCodesPlan),
    /// A reaction subscription runner. Stage 4.5 adds the callback route,
    /// closure type, ordered resources, and payload slot order.
    ReactionRunner(ReactiveRunnerPlan),
    /// An `until` predicate runner. Stage 4.5 adds the predicate closure type.
    UntilRunner(ReactiveRunnerPlan),
    /// A derived binding runner. Stage 4.5 adds the evaluator call shape.
    DerivedRunner(ReactiveRunnerPlan),
    /// An extern closure adapter. Stage 4.6 records the eager
    /// foreign-symbol declaration parity facts; the callable sites that use
    /// the adapter live on their owners' use records.
    ExternAdapter(ExternAdapterPlan),
}

/// One type a plan carries, as `LoweredArtifactPlan::visit_types` yields it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PlanType<'a> {
    Value(&'a CheckedType),
    Function(&'a CheckedFunctionType),
    Resource(&'a CheckedResource),
}

/// A plan-local callee reference: an instance or artifact key whose catalog id
/// is only known once the closure reaches a fixed point. Expansion fills the
/// key and leaves the id empty; `bind_artifact_plan_callees` writes the id
/// back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlannedCallee {
    Instance(PlannedInstance),
    Artifact(PlannedArtifact),
}

/// One source-function instance a plan calls.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedInstance {
    /// The instance identity. The expander resolves this before building the
    /// plan, so the matching `ClosureRequest::Instance` is always emitted.
    pub key: InstanceKey,
    /// The interned instance id, filled by
    /// `LoweredProgram::bind_artifact_plan_callees`.
    pub instance: Option<FunctionInstanceId>,
    /// The dependency kind the plan's instance edge records.
    pub kind: LoweredInstanceDependencyKind,
}

/// One generated artifact a plan requests.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedArtifact {
    /// The artifact identity. The expander emits the matching
    /// `ClosureRequest::Artifact` with the family's placeholder plan.
    pub key: ArtifactRequestKey,
    /// The interned artifact ordinal, filled by
    /// `LoweredProgram::bind_artifact_plan_callees`.
    pub artifact: Option<ArtifactOrdinal>,
    /// The dependency kind the plan's artifact edge records.
    pub kind: LoweredArtifactDependencyKind,
}

/// An immutable view of one planned callee.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PlannedCalleeRef<'a> {
    Instance(&'a PlannedInstance),
    Artifact(&'a PlannedArtifact),
}

/// A mutable view of one planned callee, used by the binding pass.
pub(crate) enum PlannedCalleeRefMut<'a> {
    Instance(&'a mut PlannedInstance),
    Artifact(&'a mut PlannedArtifact),
}

impl LoweredArtifactPlan {
    /// The stable family name, matching `ArtifactRequestKey::family_name` for
    /// the key the plan was built from.
    pub(crate) fn family_name(&self) -> &'static str {
        match self {
            LoweredArtifactPlan::ConstructorAdapter(_) => "constructor-adapter",
            LoweredArtifactPlan::StructuralMethod(_) => "structural-method",
            LoweredArtifactPlan::DropGlue(_) => "drop-glue",
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Payload { .. }) => {
                "gc-finalizer-payload"
            }
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Cell { .. }) => "gc-finalizer-cell",
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::ClosureEnvironment { .. }) => {
                "gc-finalizer-closure-environment"
            }
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Buffer { .. }) => {
                "gc-finalizer-buffer"
            }
            LoweredArtifactPlan::CoroutineCodes(_) => "coroutine-codes",
            LoweredArtifactPlan::ReactionRunner(_) => "reaction-runner",
            LoweredArtifactPlan::UntilRunner(_) => "until-runner",
            LoweredArtifactPlan::DerivedRunner(_) => "derived-runner",
            LoweredArtifactPlan::ExternAdapter(_) => "extern-adapter",
        }
    }

    /// Whether this plan belongs to the same artifact family as `key` and its
    /// identity inputs rebuild exactly that key: instance and owner positions
    /// must agree, every identity-carrying plan field must agree, and checked
    /// types must canonicalize to the key's values. A plan whose types cannot
    /// canonicalize concretely never agrees.
    pub(crate) fn matches_key(&self, key: &ArtifactRequestKey, origin: &Origin) -> bool {
        let same_type =
            |value: &CheckedType, expected: &CanonicalType| match CanonicalType::concrete(
                value, origin,
            ) {
                Ok(canonical) => &canonical == expected,
                Err(_) => false,
            };
        let same_function = |value: &CheckedFunctionType, expected: &CanonicalFunctionType| {
            match CanonicalFunctionType::concrete(value, origin) {
                Ok(canonical) => &canonical == expected,
                Err(_) => false,
            }
        };
        match (self, key) {
            (
                LoweredArtifactPlan::ConstructorAdapter(plan),
                ArtifactRequestKey::ConstructorAdapter(key),
            ) => {
                plan.symbol == key.symbol
                    && plan.type_id == key.type_id
                    && CanonicalAdapterKind::from(plan.adapter) == key.adapter
                    && same_function(&plan.callable_type, &key.callable_type)
            }
            (
                LoweredArtifactPlan::StructuralMethod(plan),
                ArtifactRequestKey::StructuralMethod(key),
            ) => {
                plan.structural == key.structural
                    && plan.trait_id == key.trait_id
                    && plan.method == key.method
                    && plan.arguments.len() == key.arguments.len()
                    && plan
                        .arguments
                        .iter()
                        .zip(&key.arguments)
                        .all(|(argument, expected)| same_type(argument, expected))
                    && same_function(&plan.callable_type, &key.callable_type)
            }
            (LoweredArtifactPlan::DropGlue(plan), ArtifactRequestKey::DropGlue(key)) => {
                same_type(&plan.value_type, key)
            }
            (LoweredArtifactPlan::GcFinalizer(plan), ArtifactRequestKey::GcFinalizer(key)) => {
                match (plan, key) {
                    (
                        GcFinalizerPlan::Payload { value_type, .. },
                        GcFinalizerKey::Payload(expected),
                    )
                    | (GcFinalizerPlan::Cell { value_type, .. }, GcFinalizerKey::Cell(expected))
                    | (
                        GcFinalizerPlan::Buffer {
                            element: value_type,
                            ..
                        },
                        GcFinalizerKey::Buffer(expected),
                    ) => same_type(value_type, expected),
                    (
                        GcFinalizerPlan::ClosureEnvironment {
                            closure, captures, ..
                        },
                        GcFinalizerKey::ClosureEnvironment {
                            closure: expected_closure,
                            captures: expected_captures,
                        },
                    ) => {
                        closure.index() == expected_closure.index()
                            && captures.len() == expected_captures.len()
                            && captures
                                .iter()
                                .zip(expected_captures)
                                .all(|(capture, expected)| same_type(capture, expected))
                    }
                    _ => false,
                }
            }
            (
                LoweredArtifactPlan::CoroutineCodes(plan),
                ArtifactRequestKey::CoroutineCodes(key),
            ) => plan.body.index() == key.body.index(),
            (
                LoweredArtifactPlan::ReactionRunner(plan),
                ArtifactRequestKey::ReactionRunner(key),
            )
            | (LoweredArtifactPlan::UntilRunner(plan), ArtifactRequestKey::UntilRunner(key))
            | (LoweredArtifactPlan::DerivedRunner(plan), ArtifactRequestKey::DerivedRunner(key)) => {
                plan.owner == key.owner && plan.site == key.site
            }
            (LoweredArtifactPlan::ExternAdapter(plan), ArtifactRequestKey::ExternAdapter(key)) => {
                plan.symbol == key.symbol && same_function(&plan.callable_type, &key.callable_type)
            }
            _ => false,
        }
    }

    /// Every callee this plan names, in request order. The expander emits the
    /// matching closure requests in the same order, so validation can match
    /// planned callees to artifact-owned edges one-to-one. An unexpanded plan
    /// names no callees.
    pub(crate) fn planned_callees(&self) -> Vec<PlannedCalleeRef<'_>> {
        let mut callees = Vec::new();
        self.visit_callees(&mut |callee| callees.push(callee));
        callees
    }

    /// Every callee this plan names, mutably, in request order. The binding
    /// pass and the fixed-point comparison use this form.
    pub(crate) fn planned_callees_mut(&mut self) -> Vec<PlannedCalleeRefMut<'_>> {
        let mut callees = Vec::new();
        self.visit_callees_mut(&mut |callee| callees.push(callee));
        callees
    }

    /// Whether this plan's schema records its callees as `PlannedCallee`s, so
    /// the closure validator can match them one-to-one with artifact-owned
    /// edges. The runner and extern-adapter families call indirectly or
    /// directly through fixed symbols, so their schemas have no callee slots
    /// and keep their Stage 4.2 request-based representation.
    pub(crate) fn supports_planned_callees(&self) -> bool {
        matches!(
            self,
            LoweredArtifactPlan::ConstructorAdapter(_)
                | LoweredArtifactPlan::StructuralMethod(_)
                | LoweredArtifactPlan::DropGlue(_)
                | LoweredArtifactPlan::GcFinalizer(_)
                | LoweredArtifactPlan::CoroutineCodes(_)
        )
    }

    /// Whether expansion replaced the request-time marker for this family.
    /// Families without a marker (the remaining Stage 4.1 placeholder) are
    /// always expanded.
    pub(crate) fn is_expanded(&self) -> bool {
        match self {
            LoweredArtifactPlan::ConstructorAdapter(plan) => {
                !matches!(plan.construction, ConstructorConstruction::Unexpanded)
            }
            LoweredArtifactPlan::StructuralMethod(plan) => {
                !matches!(plan.body, StructuralBody::Unexpanded)
            }
            LoweredArtifactPlan::DropGlue(plan) => !matches!(plan.body, DropGlueBody::Unexpanded),
            LoweredArtifactPlan::GcFinalizer(plan) => match plan {
                GcFinalizerPlan::Payload { glue, .. }
                | GcFinalizerPlan::Cell { glue, .. }
                | GcFinalizerPlan::Buffer { glue, .. } => glue.is_some(),
                GcFinalizerPlan::ClosureEnvironment { drops, .. } => drops.is_some(),
            },
            LoweredArtifactPlan::CoroutineCodes(plan) => plan.frame.is_some(),
            LoweredArtifactPlan::ReactionRunner(plan)
            | LoweredArtifactPlan::UntilRunner(plan)
            | LoweredArtifactPlan::DerivedRunner(plan) => {
                !matches!(plan.body, ReactiveRunnerBody::Unexpanded)
            }
            LoweredArtifactPlan::ExternAdapter(plan) => plan.declaration.is_some(),
        }
    }

    /// Compares two plans modulo the bound catalog ids. The closure re-check
    /// uses this: a re-expansion must rebuild the same plan shape even though
    /// only the stored plan has its callees bound.
    pub(crate) fn eq_ignoring_bindings(&self, other: &Self) -> bool {
        fn clear(plan: &mut LoweredArtifactPlan) {
            for callee in plan.planned_callees_mut() {
                match callee {
                    PlannedCalleeRefMut::Instance(instance) => instance.instance = None,
                    PlannedCalleeRefMut::Artifact(artifact) => artifact.artifact = None,
                }
            }
        }
        let mut left = self.clone();
        let mut right = other.clone();
        clear(&mut left);
        clear(&mut right);
        left == right
    }

    /// Every checked type, function type, and resource the plan carries, in
    /// field order. The closed-catalog validator requires each to be fully
    /// concrete. Every struct is destructured without `..`, so a new plan
    /// field fails to compile until it is visited (or explicitly skipped as a
    /// non-type field).
    pub(crate) fn visit_types<'a>(&'a self, visit: &mut impl FnMut(PlanType<'a>)) {
        use PlanType::{Function, Resource, Value};
        fn values<'a>(types: &'a [CheckedType], visit: &mut impl FnMut(PlanType<'a>)) {
            for value_type in types {
                visit(Value(value_type));
            }
        }
        fn indexed<'a>(elements: &'a [IndexedElement], visit: &mut impl FnMut(PlanType<'a>)) {
            for IndexedElement {
                index: _,
                element,
                coercion,
                coercion_plan: _,
            } in elements
            {
                visit(Value(element));
                if let Some((from, to)) = coercion {
                    visit(Value(from));
                    visit(Value(to));
                }
            }
        }
        fn debug<'a>(delegate: &'a DebugDelegate, visit: &mut impl FnMut(PlanType<'a>)) {
            let DebugDelegate {
                value_type,
                callee: _,
                callee_type,
            } = delegate;
            visit(Value(value_type));
            visit(Function(callee_type));
        }
        match self {
            LoweredArtifactPlan::ConstructorAdapter(ConstructorAdapterPlan {
                symbol: _,
                type_id: _,
                adapter: _,
                callable_type,
                construction,
            }) => {
                visit(Function(callable_type));
                match construction {
                    ConstructorConstruction::Unexpanded => {}
                    ConstructorConstruction::Value {
                        parameters,
                        product,
                    } => {
                        values(parameters, visit);
                        visit(Value(product));
                    }
                    ConstructorConstruction::ManagedRef {
                        parameters,
                        product,
                        payload,
                        finalizer: _,
                    } => {
                        values(parameters, visit);
                        visit(Value(product));
                        visit(Value(payload));
                    }
                }
            }
            LoweredArtifactPlan::StructuralMethod(StructuralMethodPlan {
                structural: _,
                trait_id: _,
                method: _,
                arguments,
                callable_type,
                body,
            }) => {
                values(arguments, visit);
                visit(Function(callable_type));
                match body {
                    StructuralBody::Unexpanded => {}
                    StructuralBody::ProductDebug { steps, write: _ } => {
                        for step in steps {
                            match step {
                                DebugStep::Write(_) => {}
                                DebugStep::Element { index: _, delegate } => debug(delegate, visit),
                            }
                        }
                    }
                    StructuralBody::SumDebug { alternatives } => {
                        for delegate in alternatives {
                            debug(delegate, visit);
                        }
                    }
                    StructuralBody::IndexSwitch { elements, output } => {
                        indexed(elements, visit);
                        visit(Value(output));
                    }
                    StructuralBody::IndexLoad {
                        element,
                        length: _,
                        output,
                    }
                    | StructuralBody::DerefIndexLoad {
                        element,
                        length: _,
                        output,
                    } => {
                        visit(Value(element));
                        visit(Value(output));
                    }
                    StructuralBody::MutateReplace {
                        element,
                        length: _,
                        drop_previous: _,
                    } => visit(Value(element)),
                    StructuralBody::DerefDelegate { payload, delegate } => {
                        visit(Value(payload));
                        let TraitDelegate {
                            trait_id: _,
                            method: _,
                            arguments,
                            callee: _,
                            callee_type,
                        } = delegate;
                        values(arguments, visit);
                        visit(Function(callee_type));
                    }
                    StructuralBody::IntoIterator { source, iterator } => {
                        visit(Value(source));
                        visit(Value(iterator));
                    }
                    StructuralBody::Next {
                        product,
                        iterator,
                        item,
                        elements,
                        result,
                        done,
                        yield_,
                    } => {
                        visit(Value(product));
                        visit(Value(iterator));
                        visit(Value(item));
                        indexed(elements, visit);
                        visit(Value(result));
                        for SumAlternative {
                            index: _,
                            alternative,
                            coercion_plan: _,
                        } in [done, yield_]
                        {
                            visit(Value(alternative));
                        }
                    }
                }
            }
            LoweredArtifactPlan::DropGlue(DropGluePlan { value_type, body }) => {
                visit(Value(value_type));
                match body {
                    DropGlueBody::Unexpanded
                    | DropGlueBody::UserDrop { .. }
                    | DropGlueBody::CoroutineCleanup
                    | DropGlueBody::RuntimeRelease(_)
                    | DropGlueBody::CStringFree
                    | DropGlueBody::Distinct { .. } => {}
                    DropGlueBody::Product { fields } => {
                        for DroppedElement {
                            index: _,
                            value_type,
                            glue: _,
                        } in fields
                        {
                            visit(Value(value_type));
                        }
                    }
                    DropGlueBody::Sum { alternatives } => {
                        for DroppedAlternative {
                            index: _,
                            value_type,
                            glue: _,
                        } in alternatives
                        {
                            visit(Value(value_type));
                        }
                    }
                }
            }
            LoweredArtifactPlan::GcFinalizer(plan) => match plan {
                GcFinalizerPlan::Payload {
                    value_type,
                    glue: _,
                }
                | GcFinalizerPlan::Cell {
                    value_type,
                    glue: _,
                }
                | GcFinalizerPlan::Buffer {
                    element: value_type,
                    glue: _,
                } => visit(Value(value_type)),
                GcFinalizerPlan::ClosureEnvironment {
                    closure: _,
                    captures,
                    drops,
                } => {
                    values(captures, visit);
                    for DroppedCapture {
                        index: _,
                        value_type,
                        glue: _,
                    } in drops.iter().flatten()
                    {
                        visit(Value(value_type));
                    }
                }
            },
            LoweredArtifactPlan::CoroutineCodes(CoroutineCodesPlan { body: _, frame }) => {
                let Some(CoroutineFramePlan {
                    result_type,
                    resume_points: _,
                    frame_bindings,
                    await_result_types,
                    wait_await_states: _,
                    until_await_states: _,
                    resources,
                    captures,
                    capture_finalizer: _,
                }) = frame
                else {
                    return;
                };
                visit(Value(result_type));
                for CoroutineFrameBinding {
                    symbol: _,
                    value_type,
                    unwind_drop: _,
                } in frame_bindings
                {
                    visit(Value(value_type));
                }
                values(await_result_types, visit);
                for CoroutineResourceSlot {
                    resource,
                    indirect: _,
                } in resources
                {
                    visit(Resource(resource));
                }
                values(captures, visit);
            }
            LoweredArtifactPlan::ReactionRunner(ReactiveRunnerPlan {
                owner: _,
                site: _,
                body,
            })
            | LoweredArtifactPlan::UntilRunner(ReactiveRunnerPlan {
                owner: _,
                site: _,
                body,
            })
            | LoweredArtifactPlan::DerivedRunner(ReactiveRunnerPlan {
                owner: _,
                site: _,
                body,
            }) => match body {
                ReactiveRunnerBody::Unexpanded => {}
                ReactiveRunnerBody::Reaction {
                    callback_type,
                    resources,
                } => {
                    visit(Function(callback_type));
                    for RunnerResourceSlot {
                        resource,
                        indirect: _,
                    } in resources
                    {
                        visit(Resource(resource));
                    }
                }
                ReactiveRunnerBody::Until { predicate_type } => visit(Function(predicate_type)),
                ReactiveRunnerBody::Derived {
                    evaluator_type,
                    output_type,
                } => {
                    visit(Function(evaluator_type));
                    visit(Value(output_type));
                }
            },
            LoweredArtifactPlan::ExternAdapter(ExternAdapterPlan {
                symbol: _,
                callable_type,
                indirect_parameters: _,
                declaration: _,
            }) => visit(Function(callable_type)),
        }
    }

    fn visit_callees<'a>(&'a self, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
        match self {
            LoweredArtifactPlan::ConstructorAdapter(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::StructuralMethod(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::DropGlue(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::GcFinalizer(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::CoroutineCodes(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::ReactionRunner(_)
            | LoweredArtifactPlan::UntilRunner(_)
            | LoweredArtifactPlan::DerivedRunner(_)
            | LoweredArtifactPlan::ExternAdapter(_) => {}
        }
    }

    fn visit_callees_mut<'a>(&'a mut self, visit: &mut impl FnMut(PlannedCalleeRefMut<'a>)) {
        match self {
            LoweredArtifactPlan::ConstructorAdapter(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::StructuralMethod(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::DropGlue(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::GcFinalizer(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::CoroutineCodes(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::ReactionRunner(_)
            | LoweredArtifactPlan::UntilRunner(_)
            | LoweredArtifactPlan::DerivedRunner(_)
            | LoweredArtifactPlan::ExternAdapter(_) => {}
        }
    }
}

/// The plan shape of a constructor value's adapter.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConstructorAdapterPlan {
    /// The constructor's semantic symbol; part of the artifact key.
    pub symbol: SymbolId,
    /// The constructor's nominal type; part of the artifact key.
    pub type_id: TypeId,
    /// The adapter kind; part of the artifact key.
    pub adapter: LoweredCallableAdapter,
    /// The concrete callable type the adapter exposes; part of the artifact
    /// key. The adapter takes the flattened parameter slots in order and
    /// returns the construction below.
    pub callable_type: CheckedFunctionType,
    /// The construction the adapter performs. `Unexpanded` at request time.
    pub construction: ConstructorConstruction,
}

/// How a constructor adapter turns its flattened parameters into its result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConstructorConstruction {
    /// Request-time only: the family expander replaces this marker.
    Unexpanded,
    /// Rebuild the product and return it unchanged (ordinary nominal
    /// wrapping).
    Value {
        /// The flattened parameter slot types, in slot order.
        parameters: Vec<CheckedType>,
        /// The product rebuilt from `parameters`.
        product: CheckedType,
    },
    /// GC-allocate the product as a `Ref` payload, optionally with a payload
    /// finalizer when the payload needs drop.
    ManagedRef {
        /// The flattened parameter slot types, in slot order.
        parameters: Vec<CheckedType>,
        /// The product rebuilt from `parameters`, which is the `Ref` payload.
        product: CheckedType,
        /// The `Ref` payload type (`product`).
        payload: CheckedType,
        /// The payload's GC finalizer, requested when the payload needs drop.
        finalizer: Option<PlannedArtifact>,
    },
}

impl ConstructorAdapterPlan {
    fn visit_callees<'a>(&'a self, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
        if let ConstructorConstruction::ManagedRef {
            finalizer: Some(finalizer),
            ..
        } = &self.construction
        {
            visit(PlannedCalleeRef::Artifact(finalizer));
        }
    }

    fn visit_callees_mut<'a>(&'a mut self, visit: &mut impl FnMut(PlannedCalleeRefMut<'a>)) {
        if let ConstructorConstruction::ManagedRef {
            finalizer: Some(finalizer),
            ..
        } = &mut self.construction
        {
            visit(PlannedCalleeRefMut::Artifact(finalizer));
        }
    }
}

/// The plan shape of a compiler-generated structural method.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StructuralMethodPlan {
    /// The structural kind; part of the artifact key.
    pub structural: StructuralTraitMethod,
    /// The selected trait; part of the artifact key.
    pub trait_id: TraitId,
    /// The selected trait method; part of the artifact key.
    pub method: TraitMethodId,
    /// The completed concrete trait arguments; part of the artifact key.
    pub arguments: Vec<CheckedType>,
    /// The concrete method type the body implements; part of the artifact key.
    pub callable_type: CheckedFunctionType,
    /// The owned body decisions. `Unexpanded` at request time.
    pub body: StructuralBody,
}

/// One structural method's owned body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StructuralBody {
    /// Request-time only: the family expander replaces this marker.
    Unexpanded,
    /// Product `Debug`: write the recorded literal steps and delegate each
    /// element to `Debug.fmt`.
    ProductDebug {
        /// The exact literal/element steps in emission order.
        steps: Vec<DebugStep>,
        /// The `Formatter.write` instance every literal step calls.
        write: PlannedInstance,
    },
    /// Sum `Debug`: one delegate per alternative in tag order; no literals.
    SumDebug {
        /// The per-alternative delegates in tag order.
        alternatives: Vec<DebugDelegate>,
    },
    /// Heterogeneous product `Index`: bounds trap, switch per element, coerce,
    /// merge.
    IndexSwitch {
        /// The per-index elements in element order.
        elements: Vec<IndexedElement>,
        /// The indexed output type every element coerces into.
        output: CheckedType,
    },
    /// Homogeneous product `Index`: direct element load over a stack copy.
    IndexLoad {
        /// The homogeneous element type.
        element: CheckedType,
        /// The product length.
        length: usize,
        /// The indexed output type.
        output: CheckedType,
    },
    /// `MutateIndex`: bounds trap, element slot, optional drop of the old
    /// element, then store.
    MutateReplace {
        /// The mutated element type.
        element: CheckedType,
        /// The product length.
        length: usize,
        /// The drop glue for the previous element when it needs drop.
        drop_previous: Option<PlannedArtifact>,
    },
    /// `DerefIndex` fast path: a non-variadic homogeneous `Copy` product read
    /// directly through the reference.
    DerefIndexLoad {
        /// The homogeneous element type.
        element: CheckedType,
        /// The product length.
        length: usize,
        /// The indexed output type.
        output: CheckedType,
    },
    /// `DerefIndex`/`DerefMutateIndex` delegation: load the payload and call
    /// the payload type's own `Index`/`MutateIndex`.
    DerefDelegate {
        /// The referenced payload type.
        payload: CheckedType,
        /// The delegated selection and its concrete method type.
        delegate: TraitDelegate,
    },
    /// `IntoIterator`: rebuild the source product and pair it with cursor `0`.
    IntoIterator {
        /// The source product type.
        source: CheckedType,
        /// The derived iterator type `(source, USize)`.
        iterator: CheckedType,
    },
    /// `Iterator.next`: `Done` when the cursor is out of range, otherwise a
    /// per-element switch yielding `(item, (product, cursor + 1))`.
    Next {
        /// The iterated inner product type.
        product: CheckedType,
        /// The iterator type `(product, USize)`.
        iterator: CheckedType,
        /// The yielded item type.
        item: CheckedType,
        /// The per-element coercions into `item`, in element order.
        elements: Vec<IndexedElement>,
        /// The result sum type.
        result: CheckedType,
        /// The `Done` alternative.
        done: SumAlternative,
        /// The `Yield` alternative.
        yield_: SumAlternative,
    },
}

fn visit_planned<'a>(callee: &'a PlannedCallee, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
    match callee {
        PlannedCallee::Instance(instance) => visit(PlannedCalleeRef::Instance(instance)),
        PlannedCallee::Artifact(artifact) => visit(PlannedCalleeRef::Artifact(artifact)),
    }
}

fn visit_planned_mut<'a>(
    callee: &'a mut PlannedCallee,
    visit: &mut impl FnMut(PlannedCalleeRefMut<'a>),
) {
    match callee {
        PlannedCallee::Instance(instance) => visit(PlannedCalleeRefMut::Instance(instance)),
        PlannedCallee::Artifact(artifact) => visit(PlannedCalleeRefMut::Artifact(artifact)),
    }
}

impl StructuralMethodPlan {
    fn visit_callees<'a>(&'a self, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
        match &self.body {
            StructuralBody::Unexpanded
            | StructuralBody::IndexSwitch { .. }
            | StructuralBody::IndexLoad { .. }
            | StructuralBody::DerefIndexLoad { .. }
            | StructuralBody::IntoIterator { .. }
            | StructuralBody::Next { .. } => {}
            StructuralBody::ProductDebug { steps, write } => {
                visit(PlannedCalleeRef::Instance(write));
                for step in steps {
                    if let DebugStep::Element { delegate, .. } = step {
                        visit_planned(&delegate.callee, visit);
                    }
                }
            }
            StructuralBody::SumDebug { alternatives } => {
                for delegate in alternatives {
                    visit_planned(&delegate.callee, visit);
                }
            }
            StructuralBody::MutateReplace { drop_previous, .. } => {
                if let Some(drop_previous) = drop_previous {
                    visit(PlannedCalleeRef::Artifact(drop_previous));
                }
            }
            StructuralBody::DerefDelegate { delegate, .. } => {
                visit_planned(&delegate.callee, visit);
            }
        }
    }

    fn visit_callees_mut<'a>(&'a mut self, visit: &mut impl FnMut(PlannedCalleeRefMut<'a>)) {
        match &mut self.body {
            StructuralBody::Unexpanded
            | StructuralBody::IndexSwitch { .. }
            | StructuralBody::IndexLoad { .. }
            | StructuralBody::DerefIndexLoad { .. }
            | StructuralBody::IntoIterator { .. }
            | StructuralBody::Next { .. } => {}
            StructuralBody::ProductDebug { steps, write } => {
                visit(PlannedCalleeRefMut::Instance(write));
                for step in steps {
                    if let DebugStep::Element { delegate, .. } = step {
                        visit_planned_mut(&mut delegate.callee, visit);
                    }
                }
            }
            StructuralBody::SumDebug { alternatives } => {
                for delegate in alternatives {
                    visit_planned_mut(&mut delegate.callee, visit);
                }
            }
            StructuralBody::MutateReplace { drop_previous, .. } => {
                if let Some(drop_previous) = drop_previous {
                    visit(PlannedCalleeRefMut::Artifact(drop_previous));
                }
            }
            StructuralBody::DerefDelegate { delegate, .. } => {
                visit_planned_mut(&mut delegate.callee, visit);
            }
        }
    }
}

/// One product-`Debug` step in emission order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DebugStep {
    /// A literal string written through the `Formatter.write` instance.
    Write(String),
    /// One element: `Debug.fmt(element)`.
    Element {
        /// The element's position in the product.
        index: usize,
        /// The selected delegate and its concrete method type.
        delegate: DebugDelegate,
    },
}

/// One product element's or sum alternative's selected `Debug` method.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DebugDelegate {
    /// The debugged value type (element or alternative payload type).
    pub value_type: CheckedType,
    /// The selected callee: an explicit `Debug` implementation instance or a
    /// nested structural `Debug` artifact.
    pub callee: PlannedCallee,
    /// The concrete method type the delegated call uses.
    pub callee_type: CheckedFunctionType,
}

/// One delegated trait-method selection.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TraitDelegate {
    /// The trait the selection belongs to (`Index` or `MutateIndex`).
    pub trait_id: TraitId,
    /// The selected method.
    pub method: TraitMethodId,
    /// The completed concrete trait arguments.
    pub arguments: Vec<CheckedType>,
    /// The selected callee.
    pub callee: PlannedCallee,
    /// The concrete method type the delegated call uses.
    pub callee_type: CheckedFunctionType,
}

/// One element of a switch or load body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexedElement {
    /// The element's position in the product.
    pub index: usize,
    /// The element type.
    pub element: CheckedType,
    /// The coercion into the output or item type, as `(from, to)`, recorded
    /// only when the two types differ.
    pub coercion: Option<(CheckedType, CheckedType)>,
    /// Concrete coercion decisions, computed and revalidated during expansion.
    pub coercion_plan: super::LoweredCoercionPlan,
}

/// One `IterStep` alternative.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SumAlternative {
    /// The alternative's position in the result sum.
    pub index: usize,
    /// The alternative type (a `Distinct` representation).
    pub alternative: CheckedType,
    /// The concrete injection of `alternative` into the result sum, computed
    /// and revalidated during expansion like `IndexedElement::coercion_plan`.
    pub coercion_plan: super::LoweredCoercionPlan,
}

/// The plan shape of one drop-glue body: the concrete value type and the
/// ordered cleanup decision mirroring `compile_drop_value` exactly.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DropGluePlan {
    pub value_type: CheckedType,
    /// The owned cleanup decision. `Unexpanded` at request time.
    pub body: DropGlueBody,
}

/// One drop-glue body, mirroring the legacy decision order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DropGlueBody {
    /// Request-time only: the family expander replaces this marker.
    Unexpanded,
    /// Call the selected user `Drop` implementation on a borrowed pointer,
    /// then drop the representation when it needs drop.
    UserDrop {
        /// The selected `Drop` implementation method instance.
        method: PlannedInstance,
        /// The `Distinct` representation's own drop glue, requested only when
        /// the representation needs drop.
        representation: Option<PlannedArtifact>,
    },
    /// Load the cleanup function from the coroutine frame header and call it
    /// indirectly; no planned callee.
    CoroutineCleanup,
    /// Call the runtime release for one opaque runtime type.
    RuntimeRelease(RuntimeRelease),
    /// Call `free` on the C-string pointer.
    CStringFree,
    /// Drop each listed field in the recorded order, which is reverse element
    /// order; only droppable fields are listed.
    Product { fields: Vec<DroppedElement> },
    /// Switch on the tag and drop the listed alternatives; only droppable
    /// alternatives are listed, in tag order.
    Sum {
        alternatives: Vec<DroppedAlternative>,
    },
    /// Drop the claimed `Distinct` representation.
    Distinct { representation: PlannedArtifact },
}

/// The runtime release one opaque-type drop performs. Stage 4.6 turns these
/// into `LoweredRuntimeRequirements`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeRelease {
    SchedulerDestroy,
    WaitDrop,
    ResolverDrop,
    CompletionTokenRelease,
}

/// One dropped product field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DroppedElement {
    /// The field's position in the product.
    pub index: usize,
    pub value_type: CheckedType,
    pub glue: PlannedArtifact,
}

/// One dropped sum alternative.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DroppedAlternative {
    /// The alternative's position in the sum.
    pub index: usize,
    pub value_type: CheckedType,
    pub glue: PlannedArtifact,
}

impl DropGluePlan {
    fn visit_callees<'a>(&'a self, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
        match &self.body {
            DropGlueBody::Unexpanded
            | DropGlueBody::CoroutineCleanup
            | DropGlueBody::RuntimeRelease(_)
            | DropGlueBody::CStringFree => {}
            DropGlueBody::UserDrop {
                method,
                representation,
            } => {
                visit(PlannedCalleeRef::Instance(method));
                if let Some(representation) = representation {
                    visit(PlannedCalleeRef::Artifact(representation));
                }
            }
            DropGlueBody::Product { fields } => {
                for field in fields {
                    visit(PlannedCalleeRef::Artifact(&field.glue));
                }
            }
            DropGlueBody::Sum { alternatives } => {
                for alternative in alternatives {
                    visit(PlannedCalleeRef::Artifact(&alternative.glue));
                }
            }
            DropGlueBody::Distinct { representation } => {
                visit(PlannedCalleeRef::Artifact(representation));
            }
        }
    }

    fn visit_callees_mut<'a>(&'a mut self, visit: &mut impl FnMut(PlannedCalleeRefMut<'a>)) {
        match &mut self.body {
            DropGlueBody::Unexpanded
            | DropGlueBody::CoroutineCleanup
            | DropGlueBody::RuntimeRelease(_)
            | DropGlueBody::CStringFree => {}
            DropGlueBody::UserDrop {
                method,
                representation,
            } => {
                visit(PlannedCalleeRefMut::Instance(method));
                if let Some(representation) = representation {
                    visit(PlannedCalleeRefMut::Artifact(representation));
                }
            }
            DropGlueBody::Product { fields } => {
                for field in fields {
                    visit(PlannedCalleeRefMut::Artifact(&mut field.glue));
                }
            }
            DropGlueBody::Sum { alternatives } => {
                for alternative in alternatives {
                    visit(PlannedCalleeRefMut::Artifact(&mut alternative.glue));
                }
            }
            DropGlueBody::Distinct { representation } => {
                visit(PlannedCalleeRefMut::Artifact(representation));
            }
        }
    }
}

/// The plan shape of one garbage-collector finalizer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GcFinalizerPlan {
    Payload {
        value_type: CheckedType,
        /// The payload's drop glue, absent only in the request-time marker.
        glue: Option<PlannedArtifact>,
    },
    Cell {
        value_type: CheckedType,
        /// The cell value's drop glue, absent only in the request-time marker.
        glue: Option<PlannedArtifact>,
    },
    ClosureEnvironment {
        /// The closure's own function instance; its position is the key's
        /// instance ordinal.
        closure: FunctionInstanceId,
        captures: Vec<CheckedType>,
        /// The captures the finalizer drops, in reverse capture order.
        /// `None` at request time.
        drops: Option<Vec<DroppedCapture>>,
    },
    Buffer {
        element: CheckedType,
        /// The element's drop glue, absent only in the request-time marker.
        glue: Option<PlannedArtifact>,
    },
}

/// One capture dropped by a closure-environment finalizer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DroppedCapture {
    /// The capture's position in environment order.
    pub index: usize,
    pub value_type: CheckedType,
    pub glue: PlannedArtifact,
}

impl GcFinalizerPlan {
    fn visit_callees<'a>(&'a self, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
        match self {
            GcFinalizerPlan::Payload { glue, .. }
            | GcFinalizerPlan::Cell { glue, .. }
            | GcFinalizerPlan::Buffer { glue, .. } => {
                if let Some(glue) = glue {
                    visit(PlannedCalleeRef::Artifact(glue));
                }
            }
            GcFinalizerPlan::ClosureEnvironment { drops, .. } => {
                for drop in drops.iter().flatten() {
                    visit(PlannedCalleeRef::Artifact(&drop.glue));
                }
            }
        }
    }

    fn visit_callees_mut<'a>(&'a mut self, visit: &mut impl FnMut(PlannedCalleeRefMut<'a>)) {
        match self {
            GcFinalizerPlan::Payload { glue, .. }
            | GcFinalizerPlan::Cell { glue, .. }
            | GcFinalizerPlan::Buffer { glue, .. } => {
                if let Some(glue) = glue {
                    visit(PlannedCalleeRefMut::Artifact(glue));
                }
            }
            GcFinalizerPlan::ClosureEnvironment { drops, .. } => {
                for drop in drops.iter_mut().flatten() {
                    visit(PlannedCalleeRefMut::Artifact(&mut drop.glue));
                }
            }
        }
    }
}

/// The plan shape of one coroutine body's resume/cleanup pair.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoroutineCodesPlan {
    /// The coroutine body thunk instance this pair belongs to; its position
    /// is the key's instance ordinal.
    pub body: FunctionInstanceId,
    /// The frame facts the pair mirrors. `None` at request time.
    pub frame: Option<CoroutineFramePlan>,
}

/// The frame facts one resume/cleanup pair mirrors: the frame cell order, the
/// result and await types, the cancellation states, the deferred-resource
/// bundle, and the thunk captures with their environment finalizer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoroutineFramePlan {
    pub result_type: CheckedType,
    pub resume_points: usize,
    /// Frame bindings in plan order, which is frame cell order.
    pub frame_bindings: Vec<CoroutineFrameBinding>,
    pub await_result_types: Vec<CheckedType>,
    /// One-based resume states whose `await` parks on a `Wait`.
    pub wait_await_states: Vec<usize>,
    /// One-based resume states whose `await` parks on an `until` child.
    pub until_await_states: Vec<usize>,
    /// Deferred-effect resource slots in effect-row order.
    pub resources: Vec<CoroutineResourceSlot>,
    /// The thunk's ordered concrete capture types, from its own instance body.
    pub captures: Vec<CheckedType>,
    /// The thunk's environment finalizer, requested exactly when the thunk has
    /// any captures (not on the closure install gate).
    pub capture_finalizer: Option<PlannedArtifact>,
}

/// One frame binding cell and the drop glue the cancel unwind calls on it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoroutineFrameBinding {
    pub symbol: SymbolId,
    pub value_type: CheckedType,
    /// The conditional cell drop's glue, present exactly when the substituted
    /// type needs drop.
    pub unwind_drop: Option<PlannedArtifact>,
}

/// One deferred-effect resource slot the resume entry unpacks.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CoroutineResourceSlot {
    pub resource: CheckedResource,
    /// The slot is loaded through a pointer rather than stored by value:
    /// `mutable || !concrete_is_copy`.
    pub indirect: bool,
}

impl CoroutineCodesPlan {
    fn visit_callees<'a>(&'a self, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
        let Some(frame) = &self.frame else {
            return;
        };
        if let Some(finalizer) = &frame.capture_finalizer {
            visit(PlannedCalleeRef::Artifact(finalizer));
        }
        for binding in &frame.frame_bindings {
            if let Some(unwind_drop) = &binding.unwind_drop {
                visit(PlannedCalleeRef::Artifact(unwind_drop));
            }
        }
    }

    fn visit_callees_mut<'a>(&'a mut self, visit: &mut impl FnMut(PlannedCalleeRefMut<'a>)) {
        let Some(frame) = &mut self.frame else {
            return;
        };
        if let Some(finalizer) = &mut frame.capture_finalizer {
            visit(PlannedCalleeRefMut::Artifact(finalizer));
        }
        for binding in &mut frame.frame_bindings {
            if let Some(unwind_drop) = &mut binding.unwind_drop {
                visit(PlannedCalleeRefMut::Artifact(unwind_drop));
            }
        }
    }
}

/// The plan shape of one reactive runner.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReactiveRunnerPlan {
    /// The record whose body arena `site` indexes.
    pub owner: ArtifactSiteOwner,
    /// The lowered reactive-operation or binding site the runner serves.
    pub site: ArtifactSite,
    /// The runner body decisions. `Unexpanded` at request time.
    pub body: ReactiveRunnerBody,
}

/// How one reactive runner invokes its callback. Every runner calls
/// indirectly through the closure value, so no variant names a callee.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ReactiveRunnerBody {
    /// Request-time only: the family expander replaces this marker.
    Unexpanded,
    /// Load the callback closure and each resource slot, then indirect-call
    /// the closure with `(environment, resources…)`.
    Reaction {
        callback_type: CheckedFunctionType,
        resources: Vec<RunnerResourceSlot>,
    },
    /// If the completion is unresolved, indirect-call the predicate and
    /// complete when its `Bool` result is alternative `0` (`True`).
    Until { predicate_type: CheckedFunctionType },
    /// Load the evaluator closure, indirect-call it with its environment, and
    /// store the result through the output pointer.
    Derived {
        evaluator_type: CheckedFunctionType,
        output_type: CheckedType,
    },
}

/// One reaction resource payload slot; a slot is a pointer when
/// `mutable || !concrete_is_copy`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunnerResourceSlot {
    pub resource: CheckedResource,
    pub indirect: bool,
}

/// The plan shape of one extern closure adapter.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExternAdapterPlan {
    pub symbol: SymbolId,
    pub callable_type: CheckedFunctionType,
    /// Whether each flattened value parameter passes by pointer in the
    /// adapter's closure signature, mirroring `indirect_parameter_mask`. The
    /// adapter loads each by-pointer argument before the native extern call.
    /// The vector is empty for a whole-mutation callable, which no extern
    /// binding can produce.
    pub indirect_parameters: Vec<bool>,
    /// The foreign-symbol declaration facts. `None` at request time; the
    /// family expander fills it.
    pub declaration: Option<ExternDeclaration>,
}

/// The declaration parity facts of one extern adapter, recorded so Stage 5 can
/// keep legacy's eager foreign-symbol declaration while emitting the adapter
/// body only for a reachable artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExternDeclaration {
    /// The C symbol's declared arity, which legacy embeds in the overloaded
    /// `name.arityN` spelling.
    pub arity: usize,
    /// Legacy declares the foreign symbol and creates this adapter eagerly for
    /// every non-variadic extern binding, used or not. The artifact records
    /// which adapters a callable-value site actually reaches.
    pub eagerly_declared: bool,
}
