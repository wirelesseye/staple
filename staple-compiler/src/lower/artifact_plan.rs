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

use crate::specialization::{ArtifactRequestKey, ArtifactSite};
use crate::{CheckedFunctionType, CheckedType, FunctionId, StructuralTraitMethod, SymbolId};

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

    /// Whether this plan belongs to the same artifact family as `key`.
    pub(crate) fn matches_key(&self, key: &ArtifactRequestKey) -> bool {
        self.family_name() == key.family_name()
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
        closure: FunctionId,
        captures: Vec<CheckedType>,
    },
    Buffer {
        element: CheckedType,
    },
}

/// The plan shape of one coroutine body's resume/cleanup pair.
#[derive(Debug, Clone)]
pub(crate) struct CoroutineCodesPlan {
    /// The coroutine body thunk template this pair belongs to.
    pub body: FunctionId,
}

/// The plan shape of one reactive runner.
#[derive(Debug, Clone)]
pub(crate) struct ReactiveRunnerPlan {
    /// The lowered reactive-operation or binding site the runner serves.
    pub site: ArtifactSite,
}

/// The plan shape of one extern closure adapter.
#[derive(Debug, Clone)]
pub(crate) struct ExternAdapterPlan {
    pub symbol: SymbolId,
    pub callable_type: CheckedFunctionType,
}
