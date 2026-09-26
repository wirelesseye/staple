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
//! Target-specific LLVM layout stays in the backend; a plan records lowered
//! identities and concrete checked values only.

#![allow(dead_code)] // Stage 4.3-4.6 fill the placeholder fields.

use super::{ArenaId, FunctionInstanceId};
use crate::specialization::{
    ArtifactRequestKey, ArtifactSite, ArtifactSiteOwner, CanonicalFunctionType, CanonicalType,
    GcFinalizerKey,
};
use crate::{CheckedFunctionType, CheckedType, Origin, StructuralTraitMethod, SymbolId};

/// The owned plan of one generated artifact, one variant per artifact family.
#[derive(Debug, Clone)]
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
    /// An extern closure adapter. Stage 4.6 adds the callable sites that use
    /// it and the eager-declaration parity notes.
    ExternAdapter(ExternAdapterPlan),
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
    /// must agree, and checked types must canonicalize to the key's values.
    /// A plan whose types cannot canonicalize concretely never agrees.
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
            ) => same_function(&plan.callable_type, &key.callable_type),
            (
                LoweredArtifactPlan::StructuralMethod(plan),
                ArtifactRequestKey::StructuralMethod(key),
            ) => plan.structural == key.structural,
            (LoweredArtifactPlan::DropGlue(plan), ArtifactRequestKey::DropGlue(key)) => {
                same_type(&plan.value_type, key)
            }
            (LoweredArtifactPlan::GcFinalizer(plan), ArtifactRequestKey::GcFinalizer(key)) => {
                match (plan, key) {
                    (
                        GcFinalizerPlan::Payload { value_type },
                        GcFinalizerKey::Payload(expected),
                    )
                    | (GcFinalizerPlan::Cell { value_type }, GcFinalizerKey::Cell(expected))
                    | (
                        GcFinalizerPlan::Buffer {
                            element: value_type,
                        },
                        GcFinalizerKey::Buffer(expected),
                    ) => same_type(value_type, expected),
                    (
                        GcFinalizerPlan::ClosureEnvironment { closure, captures },
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
}

/// The plan shape of a constructor value's adapter.
#[derive(Debug, Clone)]
pub(crate) struct ConstructorAdapterPlan {
    /// The concrete callable type the adapter exposes.
    pub callable_type: CheckedFunctionType,
}

/// The plan shape of a compiler-generated structural method.
#[derive(Debug, Clone)]
pub(crate) struct StructuralMethodPlan {
    pub structural: StructuralTraitMethod,
}

/// The plan shape of one drop-glue body.
#[derive(Debug, Clone)]
pub(crate) struct DropGluePlan {
    pub value_type: CheckedType,
}

/// The plan shape of one garbage-collector finalizer.
#[derive(Debug, Clone)]
pub(crate) enum GcFinalizerPlan {
    Payload {
        value_type: CheckedType,
    },
    Cell {
        value_type: CheckedType,
    },
    ClosureEnvironment {
        /// The closure's own function instance; its position is the key's
        /// instance ordinal.
        closure: FunctionInstanceId,
        captures: Vec<CheckedType>,
    },
    Buffer {
        element: CheckedType,
    },
}

/// The plan shape of one coroutine body's resume/cleanup pair.
#[derive(Debug, Clone)]
pub(crate) struct CoroutineCodesPlan {
    /// The coroutine body thunk instance this pair belongs to; its position
    /// is the key's instance ordinal.
    pub body: FunctionInstanceId,
}

/// The plan shape of one reactive runner.
#[derive(Debug, Clone)]
pub(crate) struct ReactiveRunnerPlan {
    /// The record whose body arena `site` indexes.
    pub owner: ArtifactSiteOwner,
    /// The lowered reactive-operation or binding site the runner serves.
    pub site: ArtifactSite,
}

/// The plan shape of one extern closure adapter.
#[derive(Debug, Clone)]
pub(crate) struct ExternAdapterPlan {
    pub symbol: SymbolId,
    pub callable_type: CheckedFunctionType,
}
