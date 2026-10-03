//! The owned boundary between checking and LLVM emission.
//!
//! [`Lowerer`] reads a successfully checked [`TypedModule`] and snapshots its
//! semantic catalogs and runtime constructs into typed arenas. Arena handles
//! are owner-relative: initializers use program arenas, while concrete function
//! instances own separate arenas. Source origins remain available for diagnostics.
//!
//! Canonical substitutions and resolved evidence identify concrete instances.
//! The worklist reserves stable ordinals before visiting bodies, and generated
//! artifact closure repeatedly scans owners, expands plans, and materializes new
//! instances until no requests remain. The catalog is closed before emission.
//!
//! Validators check source coverage, arena references, concrete instance bodies,
//! dependency/use agreement, bound callees, planned names, and complete artifact
//! plans. Cleanup, coroutine, reactive, and runtime requirements are recorded here.
//! The resulting [`LoweredProgram`] owns all semantic facts its consumer needs;
//! codegen selects neither trait implementations nor ownership behavior and
//! never queries the checker. The read-only emission view exposes owner-local
//! records without permitting mutations across this boundary.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;

use staple_syntax::{Diagnostic, Expression, Item, Pattern, Span, SyntaxId};

use crate::specialization::SpecializationCatalog;
use crate::{
    BuiltinType, CheckedAccess, CheckedCoercion, CheckedEffectSet, CheckedFunctionType,
    CheckedMutation, CheckedProductType, CheckedPropagation, CheckedResource, CheckedTraitBound,
    CheckedTraitDispatch, CheckedType, DefinitionId, FloatType, FunctionId, IntegerType,
    IntrinsicFunction, ModuleId, RecursiveConstruction, ResolvedFunction, ResolvedModule,
    SourceModule, StructuralTraitMethod, SymbolId, TraitId, TraitMethodId, TypeId, TypeParameterId,
    TypedModule, contains_type_parameter, infer_type_parameters, select_sum_alternative,
    slice_ref_length,
};

mod artifact_closure;
mod artifact_plan;
mod artifact_validation;
#[cfg(test)]
pub(crate) mod census;
mod cleanup_artifacts;
mod coroutine_artifacts;
mod emission;
mod extern_artifacts;
pub(crate) mod graph_validation;
mod initializer_bindings;
mod instance_body;
mod instance_resolution;
mod runtime_call_facts;
mod runtime_requirements;
mod structural_artifacts;
mod worklist;

pub(crate) use runtime_call_facts::LoweredRuntimeCallFacts;

// Read-only emission view. `LoweredModule::program` returns this
// type; `codegen::layout::LayoutContext` stores it so the shared
// layout layer can read semantic IDs and the concrete `Copy` decision without
// naming the lowered arenas.
pub(crate) use emission::EmissionOwner;
pub(crate) use emission::EmissionView;

// Concrete specialization resolver. The implementation stays in a
// lowering child module so it can read the owned `LoweredProgram` directly;
// these re-exports name the handoff types for the rest of the crate.
#[allow(unused_imports)]
// Re-exported handoff types are also used within the lowering subtree.
pub(crate) use instance_resolution::{
    InstanceResolutionRequest, InstanceResolutionTarget, RelevantParameters,
    ResolvedInstanceRequest, SubstitutionEntry, SubstitutionEnvironment, SubstitutionSource,
    SubstitutionValue,
};

// Specialization worklist. The graph records identity, roots, dependency edges,
// and artifact requests; specialization materializes instance bodies.
#[allow(unused_imports)] // Worklist API types are also used within the lowering subtree.
pub(crate) use worklist::{
    LoweredArtifactDependency, LoweredArtifactDependencyKind, LoweredArtifactRequest,
    LoweredArtifactRequestRoot, LoweredFunctionInstance, LoweredInstanceDependency,
    LoweredInstanceDependencyKind, LoweredInstanceRequest, LoweredScanOwner,
};

// Concrete instance bodies. Bodies are instance-owned and instance-local;
// shared semantic catalogs stay at the program level.
#[allow(unused_imports)] // Instance API types are also used within the lowering subtree.
pub(crate) use instance_body::{
    LoweredBindingSite, LoweredBoundTarget, LoweredInstanceBody, LoweredInstanceCapture,
    LoweredInstanceParameter, LoweredOwnedBinding, OwnedStorage,
};

// Artifact closure. The engine scans owners and expands artifacts through
// family scanners and expanders.
#[allow(unused_imports)] // Hook types are shared with lowering tests.
use artifact_closure::{ArtifactFamilyHooks, ClosureRequest, ProductionHooks};
#[allow(unused_imports)] // Use records are shared with validators and tests.
pub(crate) use artifact_closure::{ArtifactUseSite, LoweredArtifactUse, LoweredInstanceUse};

// Generated artifact plans. Plans are attached to artifact requests and
// filled by each artifact family's expander.
#[allow(unused_imports)]
// Family-specific plan types are also used within the lowering subtree.
pub(crate) use artifact_plan::{
    ConstructorAdapterPlan, ConstructorConstruction, CoroutineCodesPlan, CoroutineFrameBinding,
    CoroutineFramePlan, CoroutineResourceSlot, DebugDelegate, DebugStep, DropGlueBody,
    DropGluePlan, DroppedAlternative, DroppedCapture, DroppedElement, ExternAdapterPlan,
    ExternDeclaration, GcFinalizerPlan, IndexedElement, LoweredArtifactPlan, PlanType,
    PlannedArtifact, PlannedCallee, PlannedCalleeRef, PlannedCalleeRefMut, PlannedInstance,
    ReactiveRunnerBody, ReactiveRunnerPlan, RunnerResourceSlot, RuntimeRelease, StructuralBody,
    StructuralMethodPlan, SumAlternative, TraitDelegate,
};

// Runtime requirements. Fixed-named runtime surfaces are not an
// artifact family; each program carries the ordered set its operations need.
#[allow(unused_imports)] // Requirement types are shared with emission and tests.
pub(crate) use runtime_requirements::{LoweredRuntimeRequirements, RuntimeRequirement};

macro_rules! arena_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $name(usize);

        impl ArenaId for $name {
            fn from_index(index: usize) -> Self {
                Self(index)
            }

            fn index(self) -> usize {
                self.0
            }
        }

        // Generated for every arena ID type; tests build only some of them.
        #[cfg(test)]
        #[allow(dead_code)]
        impl $name {
            pub(crate) fn for_test(index: usize) -> Self {
                Self(index)
            }
        }
    };
}

/// Dense-position access for lowered arena handles. Per-site artifact keys
/// encode these positions rather than source syntax IDs.
pub(crate) trait ArenaId: Copy {
    fn from_index(index: usize) -> Self;
    fn index(self) -> usize;
}

arena_id!(ExpressionId);
arena_id!(PatternId);
arena_id!(PlaceId);
arena_id!(BlockId);
arena_id!(ItemId);
arena_id!(LoweredFunctionId);
arena_id!(InitializerId);
arena_id!(LoweredModuleId);
arena_id!(LoweredSymbolId);
arena_id!(LoweredTypeId);
arena_id!(LoweredTraitId);
arena_id!(LoweredTraitMethodId);
arena_id!(LoweredTraitImplementationId);
arena_id!(LoweredCallId);
arena_id!(LoweredCallableValueId);
arena_id!(LoweredResourceProviderId);
arena_id!(LoweredResourceUseId);
arena_id!(LoweredWithId);
arena_id!(LoweredReactiveOperationId);
arena_id!(LoweredReactiveCallbackId);
arena_id!(LoweredCoroutinePlanId);
arena_id!(LoweredCoroId);
arena_id!(LoweredAwaitId);
arena_id!(FunctionInstanceId);
arena_id!(LoweredArtifactRequestId);

/// Deterministic, append-only storage whose handles cannot be mixed with
/// handles from another lowered-node family.
#[derive(Debug, Clone)]
struct Arena<T, I> {
    values: Vec<T>,
    id: PhantomData<fn() -> I>,
}

impl<T, I> Default for Arena<T, I> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            id: PhantomData,
        }
    }
}

impl<T, I: ArenaId> Arena<T, I> {
    fn push(&mut self, value: T) -> I {
        let id = I::from_index(self.values.len());
        self.values.push(value);
        id
    }

    fn contains(&self, id: I) -> bool {
        id.index() < self.values.len()
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn get(&self, id: I) -> Option<&T> {
        self.values.get(id.index())
    }

    fn get_mut(&mut self, id: I) -> Option<&mut T> {
        self.values.get_mut(id.index())
    }

    fn iter(&self) -> impl Iterator<Item = (I, &T)> {
        self.values
            .iter()
            .enumerate()
            .map(|(index, value)| (I::from_index(index), value))
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (I, &mut T)> {
        self.values
            .iter_mut()
            .enumerate()
            .map(|(index, value)| (I::from_index(index), value))
    }
}

#[derive(Debug, Clone)]
struct CatalogEntry<K, T> {
    key: K,
    origin: Origin,
    value: T,
}

/// An insertion-ordered semantic catalog with a separate lookup index.
///
/// The ordered arena is authoritative for traversal. The map is deliberately
/// validated in both directions so later phases cannot observe a stale or
/// overwritten semantic-ID lookup.
#[derive(Debug, Clone)]
struct Catalog<K, T, I> {
    entries: Arena<CatalogEntry<K, T>, I>,
    by_key: HashMap<K, I>,
}

impl<K, T, I> Default for Catalog<K, T, I> {
    fn default() -> Self {
        Self {
            entries: Arena::default(),
            by_key: HashMap::new(),
        }
    }
}

impl<K, T, I> Catalog<K, T, I>
where
    K: Copy + Debug + Eq + Hash,
    I: ArenaId + Eq,
{
    fn insert(&mut self, kind: &str, key: K, origin: Origin, value: T) -> Result<I, Diagnostic> {
        if self.by_key.contains_key(&key) {
            return Err(Diagnostic::new(
                origin.span,
                format!("duplicate lowered {kind} semantic id {key:?}"),
            ));
        }
        let id = self.entries.push(CatalogEntry { key, origin, value });
        self.by_key.insert(key, id);
        Ok(id)
    }

    fn get(&self, key: K) -> Option<&T> {
        self.by_key
            .get(&key)
            .and_then(|id| self.entries.get(*id))
            .map(|entry| &entry.value)
    }

    fn get_mut(&mut self, key: K) -> Option<&mut T> {
        let id = *self.by_key.get(&key)?;
        self.entries.get_mut(id).map(|entry| &mut entry.value)
    }

    fn iter(&self) -> impl Iterator<Item = (I, K, &T)> {
        self.entries
            .iter()
            .map(|(id, entry)| (id, entry.key, &entry.value))
    }

    fn validate(&self, kind: &str) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut keys = HashSet::new();
        for (id, entry) in self.entries.iter() {
            if !keys.insert(entry.key) {
                diagnostics.push(Diagnostic::new(
                    entry.origin.span.clone(),
                    format!("lowered {kind} catalog repeats semantic id {:?}", entry.key),
                ));
            }
            if self.by_key.get(&entry.key) != Some(&id) {
                diagnostics.push(Diagnostic::new(
                    entry.origin.span.clone(),
                    format!(
                        "lowered {kind} catalog lookup disagrees for semantic id {:?}",
                        entry.key
                    ),
                ));
            }
        }
        for (key, id) in &self.by_key {
            let Some(entry) = self.entries.get(*id) else {
                diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!("lowered {kind} catalog has dangling lookup for semantic id {key:?}"),
                ));
                continue;
            };
            if entry.key != *key {
                diagnostics.push(Diagnostic::new(
                    entry.origin.span.clone(),
                    format!(
                        "lowered {kind} catalog lookup points to semantic id {:?} instead of {key:?}",
                        entry.key
                    ),
                ));
            }
        }
        diagnostics
    }
}

/// Source identity retained on every lowered node for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Origin {
    pub syntax: SyntaxId,
    pub span: Span,
}

impl Origin {
    fn compiler() -> Self {
        Self {
            syntax: SyntaxId::COMPILER,
            span: Span::Compiler,
        }
    }
}

/// The runtime owner of a lowered expression occurrence. Every reachable
/// runtime expression belongs to exactly one function template or module
/// initializer, and the owner participates in the expression memo key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ExpressionOwner {
    Module(ModuleId),
    Function(FunctionId),
}

/// Additional occurrence identity for contextual expressions. A product
/// type's declared default expression is one shared AST node that may be
/// evaluated at many construction sites with different checked types, so the
/// consuming product expression and destination slot must distinguish those
/// occurrences. Ordinary source occurrences are `Primary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ExpressionContext {
    Primary,
    ContextualDefault { consumer: SyntaxId, slot: usize },
}

/// Occurrence-aware expression memo key. Ordinary occurrences deduplicate by
/// source syntax and owner; contextual defaults additionally carry the
/// consuming product and destination slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ExpressionKey {
    pub syntax: SyntaxId,
    pub owner: ExpressionOwner,
    pub context: ExpressionContext,
}

/// An owned expression family. Every ordinary syntax variant maps to exactly
/// one family, and every family has a concrete lowered payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrdinaryExpressionFamily {
    Block,
    Satisfies,
    Match,
    Loop,
    Product,
    RepeatedProduct,
    Access,
    Index,
    Logical,
    Name,
    String,
    StringTemplate,
    CString,
    Integer,
    Float,
    Function,
    Call,
}

/// A callable expression family.
/// Callable lowering owns the complete payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferredExpressionFamily {
    /// Lowering owns function values and calls.
    Callable,
}

/// The concrete lowering route for a resource or coroutine expression. Every
/// deferred syntax family maps to exactly one route, and each route names the
/// record family that will own its populated payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ResourceCoroutineRoute {
    /// A `resource` value read or resource place.
    ResourceUse,
    /// A `with` provider and its scoped body.
    ResourceProvider,
    /// A `coro { ... }` creation.
    CoroutineCreation,
    /// An `await` on a child coroutine frame.
    AwaitChildCoroutine,
    /// An `await` on an external `Task` handle.
    AwaitTask,
    /// An `await` on an external `Wait` handle.
    AwaitWait,
}

impl ResourceCoroutineRoute {
    #[cfg(test)]
    /// Every route, checked by the route-table test.
    pub(crate) const ALL: [ResourceCoroutineRoute; 6] = [
        ResourceCoroutineRoute::ResourceUse,
        ResourceCoroutineRoute::ResourceProvider,
        ResourceCoroutineRoute::CoroutineCreation,
        ResourceCoroutineRoute::AwaitChildCoroutine,
        ResourceCoroutineRoute::AwaitTask,
        ResourceCoroutineRoute::AwaitWait,
    ];

    #[cfg(test)]
    /// The expression kind name a populated route produces, used by the route
    /// table to prove every route has exactly one owned record family.
    pub(crate) fn record_family(self) -> &'static str {
        match self {
            ResourceCoroutineRoute::ResourceUse => "ResourceUse",
            ResourceCoroutineRoute::ResourceProvider => "With",
            ResourceCoroutineRoute::CoroutineCreation => "Coro",
            ResourceCoroutineRoute::AwaitChildCoroutine
            | ResourceCoroutineRoute::AwaitTask
            | ResourceCoroutineRoute::AwaitWait => "Await",
        }
    }
}

/// A reactive intrinsic's explicit lowering route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReactiveIntrinsicRoute {
    Scope,
    Reaction,
    Batch,
    Until,
    Snapshot,
}

impl ReactiveIntrinsicRoute {
    #[cfg(test)]
    pub(crate) const ALL: [ReactiveIntrinsicRoute; 5] = [
        ReactiveIntrinsicRoute::Scope,
        ReactiveIntrinsicRoute::Reaction,
        ReactiveIntrinsicRoute::Batch,
        ReactiveIntrinsicRoute::Until,
        ReactiveIntrinsicRoute::Snapshot,
    ];
}

/// A coroutine-related intrinsic's explicit lowering route. These intrinsics
/// share the scheduler/completion runtime surface but do not lower to
/// resource or coroutine plan records themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CoroutineIntrinsicRoute {
    BlockOn,
    SchedulerCreate,
    TaskScope,
    Spawn,
    Pump,
    YieldNow,
    TaskIsFinished,
    TaskCancel,
    Completion,
    CompletionWithCancel,
    CompletionToken,
    CompletionTokenResolve,
    CompletionTokenCancel,
    ResolverComplete,
    ResolverCancel,
}

impl CoroutineIntrinsicRoute {
    #[cfg(test)]
    pub(crate) const ALL: [CoroutineIntrinsicRoute; 15] = [
        CoroutineIntrinsicRoute::BlockOn,
        CoroutineIntrinsicRoute::SchedulerCreate,
        CoroutineIntrinsicRoute::TaskScope,
        CoroutineIntrinsicRoute::Spawn,
        CoroutineIntrinsicRoute::Pump,
        CoroutineIntrinsicRoute::YieldNow,
        CoroutineIntrinsicRoute::TaskIsFinished,
        CoroutineIntrinsicRoute::TaskCancel,
        CoroutineIntrinsicRoute::Completion,
        CoroutineIntrinsicRoute::CompletionWithCancel,
        CoroutineIntrinsicRoute::CompletionToken,
        CoroutineIntrinsicRoute::CompletionTokenResolve,
        CoroutineIntrinsicRoute::CompletionTokenCancel,
        CoroutineIntrinsicRoute::ResolverComplete,
        CoroutineIntrinsicRoute::ResolverCancel,
    ];
}

/// The reactive or coroutine route of one compiler intrinsic. Ordinary
/// intrinsics (`None`) stay ordinary calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum IntrinsicRoute {
    Reactive(ReactiveIntrinsicRoute),
    Coroutine(CoroutineIntrinsicRoute),
}

/// Classifies each runtime expression into a concrete lowering family.
/// Every supported family has a payload; compile-time expressions are excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpressionDisposition {
    Ordinary(OrdinaryExpressionFamily),
    /// A lowering resource or coroutine expression with its concrete route.
    ResourceCoroutine(ResourceCoroutineRoute),
    /// Compile-time-only survivors that earlier phases must eliminate.
    Rejected,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredExpression {
    pub key: ExpressionKey,
    pub origin: Origin,
    pub value_type: CheckedType,
    pub effects: CheckedEffectSet,
    pub coercion: Option<CheckedCoercion>,
    /// The recursive plan emitted for
    /// `coercion`. `None` exactly when `coercion` is `None`; a template whose
    /// types still contain declared parameters leaves the plan absent, and
    /// materialization recomputes it from the substituted types.
    pub coercion_plan: Option<LoweredCoercionPlan>,
    pub moved_symbols: Vec<SymbolId>,
    pub kind: LoweredExpressionKind,
}

/// The checked emission plan for one expression coercion, computed during
/// lowering with the checker's `select_sum_alternative` rule so emission never
/// re-selects an alternative. Unsupported concrete source/target pairs produce
/// lowering diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LoweredCoercionPlan {
    /// The representation is unchanged: `source == target`, a string literal
    /// set widened to `String` or another set, or `NumberLiteral` to `USize`.
    Identity,
    /// `Ref (T; N)` to `Slice T`: build the slice from the pointer and the
    /// checked element count `N`.
    SliceRef { length: usize },
    /// Inject a non-sum value into one alternative of the target sum.
    SumInject {
        alternative: usize,
        payload: Box<LoweredCoercionPlan>,
    },
    /// Widen every alternative of a source sum into the target sum, one arm
    /// per source alternative in source order. `None` is a source alternative
    /// with no target; the emitter reaches it only through a propagating binding
    /// that narrowed the success tag first and emits `unreachable`.
    SumWiden {
        arms: Vec<Option<LoweredSumWidenArm>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoweredSumWidenArm {
    /// The target sum alternative the source alternative widens into.
    pub target: usize,
    pub payload: Box<LoweredCoercionPlan>,
}

impl LoweredCoercionPlan {
    /// Computes a coercion plan from source to target. Unsupported concrete pairs
    /// produce a diagnostic; unresolved templates defer the plan to materialization.
    pub(crate) fn plan(source: &CheckedType, target: &CheckedType) -> Result<Self, String> {
        // Emission ends the block with `unreachable` for a `Never` source
        // before any coercion runs, so the plan is never executed.
        if source == &CheckedType::Never {
            return Ok(LoweredCoercionPlan::Identity);
        }
        if source == target
            || matches!(
                (source, target),
                (CheckedType::StringLiteralSet(_), CheckedType::String)
                    | (
                        CheckedType::StringLiteralSet(_),
                        CheckedType::StringLiteralSet(_)
                    )
                    | (CheckedType::NumberLiteral(_), CheckedType::USize)
            )
        {
            return Ok(LoweredCoercionPlan::Identity);
        }
        if matches!(
            (source, target),
            (CheckedType::Ref(_), CheckedType::Slice(_))
        ) {
            let length = slice_ref_length(source, target)
                .ok_or_else(|| format!("invalid slice coercion from `{source}` to `{target}`"))?;
            return Ok(LoweredCoercionPlan::SliceRef { length });
        }
        let CheckedType::Sum(target_sum) = target else {
            return Err(format!(
                "unsupported runtime coercion from `{source}` to `{target}`"
            ));
        };
        match source {
            CheckedType::Sum(source_sum) => {
                let mut arms = Vec::with_capacity(source_sum.alternatives.len());
                for alternative in &source_sum.alternatives {
                    let index =
                        select_sum_alternative(alternative, &target_sum.alternatives).map_err(
                            |()| {
                                format!(
                                    "coercion alternative `{alternative}` matches more than one alternative of `{target}`"
                                )
                            },
                        )?;
                    match index {
                        Some(index) => {
                            let target_alternative = &target_sum.alternatives[index];
                            arms.push(Some(LoweredSumWidenArm {
                                target: index,
                                payload: Box::new(Self::plan(alternative, target_alternative)?),
                            }));
                        }
                        None => arms.push(None),
                    }
                }
                Ok(LoweredCoercionPlan::SumWiden { arms })
            }
            _ => {
                let index = select_sum_alternative(source, &target_sum.alternatives)
                    .map_err(|()| {
                        format!(
                            "coercion source `{source}` matches more than one alternative of `{target}`"
                        )
                    })?
                    .ok_or_else(|| {
                        format!(
                            "sum injection target is missing a unique source alternative for `{source}`"
                        )
                    })?;
                let target_alternative = &target_sum.alternatives[index];
                Ok(LoweredCoercionPlan::SumInject {
                    alternative: index,
                    payload: Box::new(Self::plan(source, target_alternative)?),
                })
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredExpressionKind {
    /// A callable expression dispatched to its specialized lowerer.
    Deferred(DeferredExpressionFamily),
    Block(BlockId),
    /// An ordinary value read: a local, parameter, global, mutable cell,
    /// captured cell, or singleton. Callable-valued names and constructors are
    /// deferred instead of appearing here.
    Name(LoweredName),
    Integer(LoweredInteger),
    Float(LoweredFloat),
    String(LoweredString),
    CString(LoweredCString),
    /// A structural representation, product, slice, or scalar access.
    Access(LoweredAccess),
    Product(LoweredProduct),
    RepeatedProduct(LoweredRepeatedProduct),
    /// `value satisfies Type`: a transparent wrapper. The parent expression
    /// header remains authoritative for the checked coercion.
    Satisfies(LoweredSatisfies),
    Logical(LoweredLogical),
    Loop(LoweredLoop),
    Match(LoweredMatch),
    Index(LoweredIndex),
    StringTemplate(LoweredStringTemplate),
    /// An owned call with its explicit target and argument plan.
    Call(LoweredCallId),
    /// An owned first-class callable value with its construction plan.
    CallableValue(LoweredCallableValueId),
    /// An owned resource read bound to its selected lexical provider.
    Resource(LoweredResourceUseId),
    /// An owned `with` provider and scope body.
    With(LoweredWithId),
    /// An owned `coro` creation linked to its body plan.
    Coro(LoweredCoroId),
    /// An owned `await` suspension site in its owning coroutine plan.
    Await(LoweredAwaitId),
}

/// A string template with its ordered parts and checked formatting
/// selections. Literal text is retained exactly after source decoding, and
/// interpolations keep the selected formatting trait/method and value type.
/// Helper instantiation and artifact deduplication remain lowering work.
#[derive(Debug, Clone)]
pub(crate) struct LoweredStringTemplate {
    pub parts: Vec<LoweredStringTemplatePart>,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredStringTemplatePart {
    Literal(String),
    Interpolation(LoweredInterpolation),
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredInterpolation {
    pub expression: ExpressionId,
    /// Tests pick the expected `Display` or `Debug` body by the format.
    #[cfg(test)]
    pub format: staple_syntax::StringInterpolationFormat,
    pub value_type: CheckedType,
    pub trait_id: TraitId,
    pub method: TraitMethodId,
    /// The single validated evidence recipe for the formatting selection.
    pub evidence: TraitEvidence,
}

/// Standard formatter helper selections copied from checked metadata so later
/// emission never discovers them by name.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoweredStringFormatting {
    pub constructor: Option<FunctionId>,
    pub write: Option<FunctionId>,
    pub finish: Option<FunctionId>,
}

/// The eight explicit callable categories. Every call and callable value has
/// exactly one category; there is no unknown/fallback variant. Variants carry
/// semantic IDs only, never backend symbol strings or debug-formatted keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LoweredCallableCategory {
    DirectKnownFunction,
    IndirectClosure,
    ExternalFunction,
    Intrinsic,
    Constructor,
    TraitImplementation,
    StructuralTraitMethod,
}

impl LoweredCallableCategory {
    #[cfg(test)]
    /// Every category, used by the decision-table test to prove that each one
    /// has at least one explicit route and target representation.
    pub(crate) const ALL: [LoweredCallableCategory; 7] = [
        LoweredCallableCategory::DirectKnownFunction,
        LoweredCallableCategory::IndirectClosure,
        LoweredCallableCategory::ExternalFunction,
        LoweredCallableCategory::Intrinsic,
        LoweredCallableCategory::Constructor,
        LoweredCallableCategory::TraitImplementation,
        LoweredCallableCategory::StructuralTraitMethod,
    ];
}

/// A typed callable target. This is the semantic identity a call or callable
/// value invokes; concrete instances and generated adapters stay in specialization.
#[derive(Debug, Clone)]
pub(crate) enum LoweredCallableTarget {
    /// A known function invoked directly. The environment is `None` for a
    /// null environment and `Current` for same-function recursion, so the
    /// category alone does not imply a null environment.
    DirectFunction {
        function: FunctionId,
        environment: LoweredCallEnvironment,
    },
    /// A callee value evaluated at the call site and invoked through its
    /// closure representation.
    IndirectClosure { callee: ExpressionId },
    /// An `extern` binding.
    ExternalFunction { symbol: SymbolId },
    /// A compiler intrinsic selected by the checker.
    Intrinsic {
        symbol: SymbolId,
        intrinsic: IntrinsicFunction,
    },
    /// A nominal constructor. `recursive` distinguishes managed-reference
    /// construction from ordinary nominal wrapping.
    Constructor {
        symbol: SymbolId,
        type_id: TypeId,
        recursive: Option<RecursiveConstruction>,
    },
    /// An explicit trait implementation method. `function` is absent while
    /// the selection depends on specialization substitution; the call's evidence
    /// recipe retains the declared obligation.
    TraitImplementation {
        trait_id: TraitId,
        method: TraitMethodId,
        function: Option<FunctionId>,
    },
    /// A structural trait method generated by the compiler.
    StructuralTraitMethod {
        trait_id: TraitId,
        method: TraitMethodId,
        structural: StructuralTraitMethod,
    },
}

impl LoweredCallableTarget {
    pub(crate) fn category(&self) -> LoweredCallableCategory {
        match self {
            LoweredCallableTarget::DirectFunction { .. } => {
                LoweredCallableCategory::DirectKnownFunction
            }
            LoweredCallableTarget::IndirectClosure { .. } => {
                LoweredCallableCategory::IndirectClosure
            }
            LoweredCallableTarget::ExternalFunction { .. } => {
                LoweredCallableCategory::ExternalFunction
            }
            LoweredCallableTarget::Intrinsic { .. } => LoweredCallableCategory::Intrinsic,
            LoweredCallableTarget::Constructor { .. } => LoweredCallableCategory::Constructor,
            LoweredCallableTarget::TraitImplementation { .. } => {
                LoweredCallableCategory::TraitImplementation
            }
            LoweredCallableTarget::StructuralTraitMethod { .. } => {
                LoweredCallableCategory::StructuralTraitMethod
            }
        }
    }
}

/// How a direct call obtains its closure environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredCallEnvironment {
    /// The target takes a null environment.
    None,
    /// The call reuses the enclosing closure environment (same-function
    /// recursion).
    Current,
}

/// The adapter a first-class callable value needs before it can be invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredCallableAdapter {
    None,
    Constructor,
    External,
    NestedClosure,
}

/// How a closure environment holds one capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredCaptureAccess {
    ByValue,
    Borrowed,
    SharedCell,
}

/// The environment a callable value's closure plan uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredClosureEnvironment {
    /// Construction allocates a fresh environment from the catalog captures.
    Fresh,
    /// The value needs no environment (null).
    None,
    /// Construction reuses the enclosing environment (recursion).
    Current,
    /// The value is an existing stored closure; no environment is
    /// constructed at this site.
    Stored,
}

/// One capture of a closure construction, copied from the function catalog in
/// capture order with its ownership and access facts.
#[derive(Debug, Clone)]
pub(crate) struct LoweredClosureCapture {
    /// The catalog capture record (symbol, borrowing, non-owning, cell facts).
    pub capture: LoweredCapture,
    pub value_type: CheckedType,
    pub access: LoweredCaptureAccess,
    /// The constructed environment owns the capture value.
    pub owns_value: bool,
    /// The captured symbol requires initialization state.
    pub requires_initialization_state: bool,
}

/// A closure construction plan: the target function, its ordered captures,
/// environment reuse, and adapter requirement. Target-specific environment
/// layout stays in LLVM.
#[derive(Debug, Clone)]
pub(crate) struct LoweredClosureConstruction {
    pub function: FunctionId,
    pub captures: Vec<LoweredClosureCapture>,
    pub environment: LoweredClosureEnvironment,
    pub adapter: LoweredCallableAdapter,
    pub substitutions: CallSubstitutions,
}

/// Compile-time substitutions recorded at a call or closure use site. The
/// mapping retains unresolved declared template parameters instead of
/// inventing a concrete instance; specialization owns instance interning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CallSubstitutions {
    pub types: Vec<CallTypeSubstitution>,
    pub effects: Vec<CallEffectSubstitution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallTypeSubstitution {
    pub parameter: TypeParameterId,
    pub value_type: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallEffectSubstitution {
    pub parameter: TypeParameterId,
    pub effects: CheckedEffectSet,
}

/// Trait selection evidence for a call, index read, indexed mutation, or
/// formatting interpolation. Negative implementations stay rejection data and
/// never become callable targets.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TraitEvidence {
    /// A selected explicit implementation and its method function.
    ExplicitImplementation {
        trait_id: TraitId,
        implementation: LoweredTraitImplementationId,
        method: TraitMethodId,
        function: FunctionId,
        arguments: Vec<CheckedType>,
    },
    /// A selected structural method with its completed trait arguments.
    Structural {
        trait_id: TraitId,
        method: TraitMethodId,
        structural: StructuralTraitMethod,
        arguments: Vec<CheckedType>,
    },
    /// A declared bound or implementation prerequisite that specialization must
    /// realize after substitution; no implementation is chosen yet.
    DeclaredBound {
        trait_id: TraitId,
        method: Option<TraitMethodId>,
        arguments: Vec<CheckedType>,
        prerequisites: Vec<CheckedTraitBound>,
    },
}

/// How one visible call argument is passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredArgumentPassMode {
    /// Passed by value in its ABI slot.
    Value,
    /// Passed as a borrowed pointer to the caller's value.
    BorrowedPointer,
    /// Passed as a mutable place pointer.
    MutablePlace,
    /// Materialized into a temporary and passed by pointer.
    MaterializedTemporary,
}

/// One visible call argument with its final ABI slot and temporary
/// facts. Final slot mapping is stored here, separately from evaluation order.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCallArgument {
    /// The argument occurrence, absent when the argument is an implicit
    /// thunk whose body is owned by the thunk function.
    pub expression: Option<ExpressionId>,
    /// The implicit thunk closure this argument adapts to, when present.
    pub thunk: Option<FunctionId>,
    /// The final ABI slot, absent when the argument is materialized.
    pub slot: Option<usize>,
    pub pass_mode: LoweredArgumentPassMode,
    /// The checked type expected for this argument.
    pub expected: CheckedType,
    /// The argument's source place, when it has one.
    pub place: Option<PlaceId>,
    /// The value is materialized into a temporary for the pass.
    pub temporary: bool,
    /// The temporary must be dropped after the call.
    pub drops_after_call: bool,
}

/// An ordered call step. Explicit source arguments execute in source order;
/// defaults execute in the order the checked plan selected.
#[derive(Debug, Clone)]
pub(crate) enum LoweredCallStep {
    /// Evaluate the callee occurrence of an indirect call.
    Callee { expression: ExpressionId },
    /// Evaluate a visible argument in source order.
    Argument { argument: usize },
    /// Place one element of a product argument into a final slot.
    ProductElement {
        argument: usize,
        slot: usize,
        expression: ExpressionId,
    },
    /// Expand a positional spread into an argument's final slots.
    ProductSpread {
        argument: usize,
        expression: ExpressionId,
        mappings: Vec<LoweredSpreadMapping>,
    },
    /// Expand a named spread into an argument's final slots.
    NamedProductSpread {
        argument: usize,
        expression: ExpressionId,
        mappings: Vec<LoweredNamedSpreadMapping>,
    },
    /// Evaluate a contextual default for one argument slot.
    Default {
        argument: usize,
        slot: usize,
        expression: ExpressionId,
        expected: CheckedType,
    },
    /// Look up the provider for a hidden resource requirement.
    Resource { resource: usize },
    /// Perform the invocation.
    Invoke,
}

/// A complete call: explicit target, ordered argument plan, hidden resource
/// requirements, and the site-specific substitution/evidence recipe.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCall {
    pub origin: Origin,
    pub target: LoweredCallableTarget,
    /// The callee occurrence for an indirect call; direct targets reference
    /// their semantic ID instead.
    pub callee: Option<ExpressionId>,
    /// The checked function type after call-site inference.
    pub function_type: CheckedFunctionType,
    /// Ordered visible arguments with their final ABI slots.
    pub arguments: Vec<LoweredCallArgument>,
    /// Ordered hidden resource requirements from the checked effect row,
    /// resolved to their selected lexical providers in effect-row order.
    pub resource_bindings: Vec<LoweredResourceUseId>,
    /// Symbols whose initialization state the call must check.
    pub initialization_checks: Vec<SymbolId>,
    /// Ordered evaluation steps.
    pub steps: Vec<LoweredCallStep>,
    /// The call's checked result type.
    pub result_type: CheckedType,
    /// Compile-time substitutions known at the call site.
    pub substitutions: CallSubstitutions,
    /// Trait evidence for trait-dispatched calls.
    pub evidence: Option<TraitEvidence>,
    /// The reactive operation this intrinsic call performs (`reactive_scope`,
    /// `reaction`, `batch`, `until`, `snapshot`).
    pub reactive: Option<LoweredReactiveOperationId>,
    /// `Buffer.pop`'s `Option` result alternatives. Recomputed per instance
    /// from the concrete result type and validated there, so the emitter
    /// never searches the sum for them.
    pub buffer_pop: Option<LoweredOptionAlternatives>,
    /// Concrete coroutine/task/completion inputs, recomputed per instance.
    pub runtime: LoweredRuntimeCallFacts,
}

/// The `None` and `Some` alternative indices of an `Option` result sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoweredOptionAlternatives {
    pub none: usize,
    pub some: usize,
}

impl LoweredOptionAlternatives {
    /// The alternatives of `Buffer.pop`'s result when `target` is that
    /// intrinsic (the `Distinct` alternatives named `None` and `Some`); `None`
    /// for any other target.
    pub(crate) fn for_call(
        target: &LoweredCallableTarget,
        result: &CheckedType,
    ) -> Result<Option<Self>, String> {
        if !matches!(
            target,
            LoweredCallableTarget::Intrinsic {
                intrinsic: IntrinsicFunction::BufferPop,
                ..
            }
        ) {
            return Ok(None);
        }
        let CheckedType::Sum(option) = result else {
            return Err("Buffer.pop must return Option T".to_string());
        };
        let find = |suffix: &str| {
            option
                .alternatives
                .iter()
                .position(|alternative| {
                    matches!(alternative, CheckedType::Distinct { name, .. } if name.ends_with(suffix))
                })
                .ok_or_else(|| format!("Option is missing {suffix}"))
        };
        Ok(Some(Self {
            none: find("None")?,
            some: find("Some")?,
        }))
    }
}

/// A first-class callable value with its explicit target and construction
/// plan.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCallableValue {
    pub origin: Origin,
    pub target: LoweredCallableTarget,
    /// The checked function type of the value after call-site inference.
    pub function_type: CheckedFunctionType,
    pub adapter: LoweredCallableAdapter,
    /// Closure construction for values that build an environment.
    pub closure: Option<LoweredClosureConstruction>,
    /// Compile-time substitutions known at the use site.
    pub substitutions: CallSubstitutions,
    /// Trait evidence for trait-method values.
    pub evidence: Option<TraitEvidence>,
    /// The value's symbol must have its initialization state checked before
    /// the closure is used.
    pub requires_initialization_check: bool,
}

/// How a resource provider entered scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredProviderOriginKind {
    /// The provider value expression of a `with`.
    Source,
    /// A function parameter seeded from the checked effect row.
    FunctionParameter,
    /// The executable entry's IO/reactive resource parameter.
    EntryParameter,
}

/// What the provider identity refers to: the `with` value expression, an
/// implicit function effect parameter, or an executable-entry resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredProviderTarget {
    /// The provider value expression of a `with`.
    Expression(ExpressionId),
    /// A function effect parameter at its position in the checked row.
    EffectParameter { position: usize },
    /// An executable-entry resource installed before initialization.
    Entry,
}

/// How a provider's storage is established. `Place` reuses a source place's
/// address; `Materialized` evaluates the provider value into fresh storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredProviderStorage {
    Place,
    Materialized,
}

/// The scope-exit obligation a provider carries when its lexical scope ends.
/// Exits are recorded as classifications rather than cleanup blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredScopeExit {
    /// A `Reactive` scope disposes its subscriptions.
    Reactive,
    /// A `Tasks` scope closes queued children.
    Tasks,
    /// An ordinary resource needs no scope-exit cleanup.
    Ordinary,
}

/// One lexical resource provider: a function/entry scope root or a `with`
/// provider. Stable identity is the arena ID; `parent` records lexical nesting.
#[derive(Debug, Clone)]
pub(crate) struct LoweredResourceProvider {
    pub origin: Origin,
    pub resource: CheckedResource,
    pub kind: LoweredProviderOriginKind,
    pub target: LoweredProviderTarget,
    /// The enclosing provider when this one nests inside another.
    pub parent: Option<LoweredResourceProviderId>,
    /// The runtime owner whose block contains the provider.
    pub owner: ExpressionOwner,
    /// The provider value is held indirectly and reads pass through a pointer.
    pub indirect: bool,
    /// Passing the provider requires a borrow pointer (mutable or non-`Copy`).
    pub borrow: bool,
    /// Whether the provider pointer is a source place or materialized storage.
    pub storage: LoweredProviderStorage,
    pub scope_exit: LoweredScopeExit,
}

/// How one resource use consumes its selected provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredResourceUseKind {
    /// A `resource` value read.
    Read,
    /// The addressable place of a resource assignment target.
    MutablePlace,
    /// A hidden call/effect argument in checked effect-row order.
    HiddenArgument,
}

/// A resolved resource requirement: the provider selected at the occurrence
/// and how the value is read or passed. A generic template requirement keeps
/// `provider` absent until specialization substitutes it.
#[derive(Debug, Clone)]
pub(crate) struct LoweredResourceUse {
    pub origin: Origin,
    pub resource: CheckedResource,
    pub provider: Option<LoweredResourceProviderId>,
    pub kind: LoweredResourceUseKind,
    pub pass_mode: LoweredArgumentPassMode,
    /// The provider value must be loaded through its pointer.
    pub indirect: bool,
}

/// A lowered `with`: provider value evaluated once before scope entry, the
/// nested body block, and the ordered enter/exit obligations.
#[derive(Debug, Clone)]
pub(crate) struct LoweredWith {
    pub origin: Origin,
    pub provider: LoweredResourceProviderId,
    /// The provider value expression, evaluated before the body.
    pub value: ExpressionId,
    /// The source place the provider storage reuses
    /// (`LoweredProviderStorage::Place`), recorded so the
    /// emitter does not re-derive the place decision.
    pub place: Option<PlaceId>,
    pub body: BlockId,
    /// The scope-exit obligation, if any.
    pub scope_exit: LoweredScopeExit,
}

/// One reactive callback: an implicit thunk or an explicit callable occurrence
/// with its checked function type, captures, and resource requirements.
#[derive(Debug, Clone)]
pub(crate) struct LoweredReactiveCallback {
    pub origin: Origin,
    /// The implicit thunk that owns a block callback.
    pub thunk: Option<FunctionId>,
    /// The callable occurrence for an explicit callback.
    pub callable: Option<ExpressionId>,
    pub function_type: CheckedFunctionType,
    /// Ordered captures for a thunk callback.
    pub captures: Vec<LoweredCapture>,
    /// Ordered hidden resource uses the callback needs.
    pub resources: Vec<LoweredResourceUseId>,
}

/// Where a signal's storage is created: module global storage or a local
/// binding cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredSignalStorage {
    Global,
    LocalCell,
}

/// A signal, derived, or reactive operation attached to its checking site.
#[derive(Debug, Clone)]
pub(crate) enum LoweredReactiveOperationKind {
    /// Signal storage creation for a symbol.
    SignalCreate {
        symbol: SymbolId,
        storage: LoweredSignalStorage,
    },
    /// A tracked read of a signal symbol.
    SignalRead { symbol: SymbolId },
    /// A write notification for a signal symbol.
    SignalNotify { symbol: SymbolId },
    /// A read that forces a derived binding to recompute if stale.
    DerivedRead { symbol: SymbolId },
    /// Derived binding creation: evaluator thunk, captures, callback type.
    DerivedCreate {
        symbol: SymbolId,
        evaluator: FunctionId,
        function_type: CheckedFunctionType,
        /// Ordered captures copied from the evaluator thunk.
        captures: Vec<LoweredCapture>,
    },
    /// A new ambient `Reactive` scope.
    Scope,
    /// A `reaction` subscription.
    Reaction {
        callback: LoweredReactiveCallbackId,
        /// The ambient `Reactive` provider the subscription uses.
        reactive_provider: Option<LoweredResourceProviderId>,
    },
    /// A `batch` boundary.
    Batch { callback: LoweredReactiveCallbackId },
    /// An `until` predicate subscription.
    Until {
        predicate: LoweredReactiveCallbackId,
        reactive_provider: Option<LoweredResourceProviderId>,
    },
    /// A `snapshot` tracking suspension around its operand.
    Snapshot,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredReactiveOperation {
    pub origin: Origin,
    pub kind: LoweredReactiveOperationKind,
}

/// One owned coroutine plan linked to its body function, captures, deferred
/// effects, resume states, and cancellation classifications.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCoroutinePlan {
    pub origin: Origin,
    /// The `coro` body block syntax that keys the plan.
    pub body_syntax: SyntaxId,
    /// The lowered coroutine body block, linked once its thunk body lowers.
    pub body: Option<BlockId>,
    /// The implicit thunk that owns the body.
    pub thunk: FunctionId,
    /// Ordered captures copied from the thunk catalog.
    pub captures: Vec<LoweredCapture>,
    pub result_type: CheckedType,
    pub deferred_effects: CheckedEffectSet,
    pub resume_points: usize,
    /// Ordered body-local frame-binding symbols.
    pub frame_bindings: Vec<SymbolId>,
    /// Ordered awaited-result types.
    pub await_result_types: Vec<CheckedType>,
    /// One-based resume states whose `await` parks on a `Wait`.
    pub wait_await_states: Vec<usize>,
    /// One-based resume states whose `await` parks on an `until` child.
    pub until_await_states: Vec<usize>,
    /// Ordered awaits owned by this plan.
    pub awaits: Vec<LoweredAwaitId>,
}

/// A `coro { ... }` creation linked to its body plan.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCoro {
    pub origin: Origin,
    pub plan: LoweredCoroutinePlanId,
    /// The capture environment construction at the creation site.
    pub environment: LoweredClosureEnvironment,
}

/// Which suspension an `await` performs.
#[derive(Debug, Clone)]
pub(crate) enum LoweredAwaitKind {
    /// A child coroutine frame: run it to completion and take its result.
    ChildCoroutine {
        /// The child's plan when the operand is an identifiable coroutine
        /// body; absent when only the checked effect/result parts are known.
        plan: Option<LoweredCoroutinePlanId>,
        child_result: CheckedType,
        /// Ordered resource uses acquired for the child's deferred effects at
        /// activation time.
        deferred_resources: Vec<LoweredResourceUseId>,
        /// The operand is a call to the `until` intrinsic, which parks
        /// off-queue and can be safely cleaned up during cancellation.
        until: bool,
    },
    /// An external `Task` handle.
    Task { result: CheckedType },
    /// An external `Wait` handle.
    Wait { result: CheckedType },
}

/// One `await` suspension site inside its owning coroutine plan.
#[derive(Debug, Clone)]
pub(crate) struct LoweredAwait {
    pub origin: Origin,
    pub operand: ExpressionId,
    /// The await expression's checked result type: the child result, or the
    /// `Completed T | Cancelled` outcome sum for external awaits.
    pub result_type: CheckedType,
    pub owning_plan: LoweredCoroutinePlanId,
    /// One-based resume state.
    pub resume_state: usize,
    pub kind: LoweredAwaitKind,
    /// An external (`Task`/`Wait`) await's injections into its outcome sum.
    /// Recomputed per instance from the concrete result type and validated,
    /// so the emitter never plans them.
    pub outcome: Option<LoweredAwaitOutcome>,
}

/// The `Completed payload | Cancelled` injections of one external await, at the
/// result sum's fixed positions (0 and 1).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoweredAwaitOutcome {
    pub completed: LoweredCoercionPlan,
    pub cancelled: LoweredCoercionPlan,
}

impl LoweredAwaitOutcome {
    /// The outcome plans of an external await, `None` for a child-coroutine
    /// await or a template result type that still names a type parameter.
    pub(crate) fn for_await(
        kind: &LoweredAwaitKind,
        result_type: &CheckedType,
    ) -> Result<Option<Self>, String> {
        if matches!(kind, LoweredAwaitKind::ChildCoroutine { .. })
            || contains_type_parameter(result_type)
        {
            return Ok(None);
        }
        let CheckedType::Sum(outcome) = result_type else {
            return Err("`await` result is not a `Completed | Cancelled` sum".to_string());
        };
        let [completed, cancelled, ..] = outcome.alternatives.as_slice() else {
            return Err("`await` outcome sum has fewer than two alternatives".to_string());
        };
        Ok(Some(Self {
            completed: LoweredCoercionPlan::plan(completed, result_type)?,
            cancelled: LoweredCoercionPlan::plan(cancelled, result_type)?,
        }))
    }
}

/// The runtime call route a checked call follows, mirroring the backend's
/// decision order. Every call has exactly one route; the indirect closure
/// route is the fallback, never an unknown-callable case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CallRoute {
    /// A completed juxtaposed call chain through a closure value.
    Juxtaposed,
    /// A juxtaposed chain whose callee is a compiler intrinsic.
    JuxtaposedIntrinsic,
    /// A curried call with checked defaults or a `_` partial application.
    CurriedDefault,
    /// A trait-dispatched call with a selected explicit implementation.
    TraitImplementation,
    /// A trait-dispatched call whose implementation depends on specialization
    /// substitution.
    DeclaredTraitBound,
    /// A trait-dispatched call resolved structurally.
    StructuralTraitMethod,
    /// A compiler intrinsic.
    Intrinsic,
    /// A direct call to a captureless generic function template.
    GenericDirect,
    /// An `extern` binding.
    External,
    /// A call through a first-class closure value.
    Indirect,
    /// A nominal constructor.
    Constructor,
    /// A primitive macro call (`c_string`).
    PrimitiveMacro,
}

impl CallRoute {
    #[cfg(test)]
    /// Every route, checked by the decision-table test against the exhaustive
    /// category mapping.
    pub(crate) const ALL: [CallRoute; 12] = [
        CallRoute::Juxtaposed,
        CallRoute::JuxtaposedIntrinsic,
        CallRoute::CurriedDefault,
        CallRoute::TraitImplementation,
        CallRoute::DeclaredTraitBound,
        CallRoute::StructuralTraitMethod,
        CallRoute::Intrinsic,
        CallRoute::GenericDirect,
        CallRoute::External,
        CallRoute::Indirect,
        CallRoute::Constructor,
        CallRoute::PrimitiveMacro,
    ];

    #[cfg(test)]
    /// The single explicit callable category this route produces. The
    /// exhaustive match is the compile-time half of the decision table.
    pub(crate) fn category(self) -> LoweredCallableCategory {
        use LoweredCallableCategory::*;
        match self {
            CallRoute::Juxtaposed | CallRoute::CurriedDefault | CallRoute::Indirect => {
                IndirectClosure
            }
            CallRoute::JuxtaposedIntrinsic | CallRoute::Intrinsic | CallRoute::PrimitiveMacro => {
                Intrinsic
            }
            CallRoute::TraitImplementation | CallRoute::DeclaredTraitBound => TraitImplementation,
            CallRoute::StructuralTraitMethod => StructuralTraitMethod,
            CallRoute::GenericDirect => DirectKnownFunction,
            CallRoute::External => ExternalFunction,
            CallRoute::Constructor => Constructor,
        }
    }
}

/// The construction route of a first-class callable value, mirroring the
/// backend's value routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CallableValueRoute {
    /// A trait method selector value.
    TraitMethod,
    /// A constructor value.
    Constructor,
    /// An `extern` binding value.
    External,
    /// A compiler intrinsic value.
    Intrinsic,
    /// A generic function template value.
    GenericFunction,
    /// A declared non-generic function value.
    DeclaredFunction,
    /// An anonymous function expression.
    AnonymousFunction,
    /// An ordinary function-typed value read; the value's construction
    /// happened at its binding site.
    OrdinaryRead,
}

impl CallableValueRoute {
    #[cfg(test)]
    /// Every construction route, checked by the decision-table test.
    pub(crate) const ALL: [CallableValueRoute; 8] = [
        CallableValueRoute::TraitMethod,
        CallableValueRoute::Constructor,
        CallableValueRoute::External,
        CallableValueRoute::Intrinsic,
        CallableValueRoute::GenericFunction,
        CallableValueRoute::DeclaredFunction,
        CallableValueRoute::AnonymousFunction,
        CallableValueRoute::OrdinaryRead,
    ];
}

/// A checked index read: `base[index]`. The complete checked `Index` dispatch
/// recipe is copied here; lowering converts it into explicit callable
/// evidence without consulting the type checker. The operands are lowered in
/// source evaluation order (base before index).
#[derive(Debug, Clone)]
pub(crate) struct LoweredIndex {
    pub base: ExpressionId,
    pub index: ExpressionId,
    /// The checked `Index` dispatch as recorded by the type checker.
    pub dispatch: CheckedTraitDispatch,
    /// Owning trait of the dispatched method.
    pub trait_id: TraitId,
    /// Functional-dependency-completed trait arguments.
    pub arguments: Vec<CheckedType>,
    /// Instantiated method type, carrying mutation/move masks, effects,
    /// resources, and the result type.
    pub method_type: Option<CheckedFunctionType>,
    /// The whole `(base, index)` argument is materialized into a temporary so
    /// a whole-argument mutation/move can be passed by address.
    pub whole_temporary: bool,
    /// The base operand is materialized into a temporary for a mutation.
    pub base_temporary: bool,
    /// The index operand is materialized into a temporary for a mutation.
    pub index_temporary: bool,
    /// The base operand's place, reused for mutated or borrowed operands.
    /// An operand without a place uses a recorded temporary.
    pub base_place: Option<PlaceId>,
    /// The index operand's place when it has one.
    pub index_place: Option<PlaceId>,
    /// How the `Index` call passes its operands and which operand temporaries
    /// it drops afterwards, so the emitter never derives a pass mode or a
    /// cleanup from the operand types.
    pub operands: LoweredIndexOperands,
    /// The single validated evidence recipe for the `Index` dispatch.
    pub evidence: TraitEvidence,
}

/// Operand passing and cleanup facts for an Index call, indexed by the
/// method's flattened parameters. Mutation temporaries drop after the call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LoweredIndexOperands {
    /// The parameter is passed by address (a place pointer or a temporary).
    pub indirect: Vec<bool>,
    /// A mutation operand without a place is materialized into a temporary
    /// that is dropped after the call.
    pub drops_after_call: Vec<bool>,
    /// The whole-argument mutation temporary is dropped after the call.
    pub whole_drops_after_call: bool,
}

impl LoweredIndexOperands {
    /// The emitter's decisions for one `Index` call: a parameter is indirect when
    /// it is mutated or, unless moved, not `Copy` (for the two-operand shape,
    /// judged on the actual operand type); a mutation operand with no place
    /// gets a temporary dropped when its parameter type needs drop; a whole
    /// mutation materializes the product, dropped when the product needs drop.
    pub(crate) fn compute(
        method_type: &CheckedFunctionType,
        operand_types: [&CheckedType; 2],
        operand_places: [bool; 2],
        whole_temporary: bool,
        is_copy: impl Fn(&CheckedType) -> bool,
        needs_drop: impl Fn(&CheckedType) -> bool,
    ) -> Self {
        let types = match method_type.parameter.as_ref() {
            CheckedType::Product(product) => product
                .elements
                .iter()
                .map(|element| &element.value_type)
                .collect::<Vec<_>>(),
            other => vec![other],
        };
        let mask = |mutations: &[CheckedMutation]| {
            let whole = mutations.contains(&CheckedMutation::Whole);
            (0..types.len())
                .map(|index| whole || mutations.contains(&CheckedMutation::Element(index)))
                .collect::<Vec<_>>()
        };
        let mutation = mask(&method_type.mutations);
        let moves = mask(&method_type.moves);
        let mut indirect = types
            .iter()
            .enumerate()
            .map(|(index, value_type)| mutation[index] || (!moves[index] && !is_copy(value_type)))
            .collect::<Vec<_>>();
        if types.len() == 2 {
            for (element, actual) in operand_types.into_iter().enumerate() {
                if !mutation[element] && !moves[element] {
                    indirect[element] = !is_copy(actual);
                }
            }
        }
        let any_indirect = indirect.iter().any(|indirect| *indirect);
        let whole_drops_after_call =
            any_indirect && whole_temporary && needs_drop(&method_type.parameter);
        let drops_after_call = (0..types.len())
            .map(|element| {
                any_indirect
                    && !whole_temporary
                    && element < 2
                    && indirect[element]
                    && mutation[element]
                    && !operand_places[element]
                    && needs_drop(types[element])
            })
            .collect();
        Self {
            indirect,
            drops_after_call,
            whole_drops_after_call,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredSatisfies {
    pub value: ExpressionId,
}

/// A short-circuiting logical operator with its checked `Bool` selection.
/// Operands are always evaluated left-to-right and `right` only when the
/// short-circuit does not determine the result.
#[derive(Debug, Clone)]
pub(crate) struct LoweredLogical {
    pub operator: staple_syntax::LogicalOperator,
    pub left: ExpressionId,
    pub right: ExpressionId,
    pub bool_type: CheckedType,
    /// Resolved `True` alternative of the checked `Bool` sum.
    pub true_index: usize,
}

/// A loop with its lowered body block and cleanup metadata. `break`/`continue`
/// items reference the enclosing loop through the recorded nesting depth.
#[derive(Debug, Clone)]
pub(crate) struct LoweredLoop {
    pub body: BlockId,
    pub result_type: CheckedType,
    /// The body result must be dropped before the back edge.
    pub drops_body_result: bool,
    /// One-based nesting depth of this loop, matching the `loop_depth`
    /// recorded on the break/continue items it owns.
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredMatch {
    pub subject: ExpressionId,
    /// Checked subject type copied from `CheckedMatch`.
    pub source: CheckedType,
    pub arms: Vec<LoweredMatchArm>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredMatchArm {
    pub origin: Origin,
    pub pattern: PatternId,
    pub body: ExpressionId,
    /// Symbols the pattern binds, in source order. The arm's cleanup boundary
    /// is the enclosing ownership state plus these bindings; a divergent arm
    /// (a `Never` body) never contributes a runtime value.
    pub bound_symbols: Vec<SymbolId>,
}

/// A product construction plan. `steps` preserve source evaluation order
/// (including spread expansion and default evaluation); `fields` is the final
/// checked layout, where later steps override earlier values for a slot.
#[derive(Debug, Clone)]
pub(crate) struct LoweredProduct {
    /// The final checked product shape, defaults cleared.
    pub final_type: CheckedProductType,
    /// Ordered source evaluation steps.
    pub steps: Vec<LoweredProductStep>,
    /// Final layout: exactly one expression per slot.
    pub fields: Vec<ExpressionId>,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredProductStep {
    /// A positional element placed at the next positional slot.
    Positional {
        expression: ExpressionId,
        slot: usize,
    },
    /// A named element (designator or named-spread entry).
    Designated {
        name: String,
        expression: ExpressionId,
        slot: usize,
    },
    /// A positional spread expanded against the operand's checked product.
    PositionalSpread {
        expression: ExpressionId,
        mappings: Vec<LoweredSpreadMapping>,
    },
    /// A named spread expanded against the operand's checked product.
    NamedSpread {
        expression: ExpressionId,
        mappings: Vec<LoweredNamedSpreadMapping>,
    },
    /// A missing contextual default evaluated in final slot order.
    Default {
        slot: usize,
        expression: ExpressionId,
        expected: CheckedType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoweredSpreadMapping {
    /// Element position inside the spread operand.
    pub source: usize,
    /// Destination slot in the final product layout.
    pub slot: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoweredNamedSpreadMapping {
    pub name: String,
    /// Element position inside the spread operand.
    pub source: usize,
    /// Destination slot in the final product layout.
    pub slot: usize,
}

/// A repeated product `(value; count)`. The element is evaluated exactly once;
/// `collapsed` marks the fixed `count == 1` representation that is the element
/// itself rather than a single-field product.
#[derive(Debug, Clone)]
pub(crate) struct LoweredRepeatedProduct {
    pub expression: ExpressionId,
    pub count: LoweredRepeatCount,
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoweredRepeatCount {
    Fixed(usize),
    Symbolic(CheckedType),
}

impl LoweredRepeatCount {
    fn for_type(value_type: &CheckedType) -> Self {
        match value_type {
            CheckedType::Product(product) if !product.variadic => {
                Self::Fixed(product.elements.len())
            }
            CheckedType::Array { count, .. } => Self::Symbolic(count.as_ref().clone()),
            _ => Self::Fixed(1),
        }
    }
}

impl LoweredRepeatedProduct {
    fn validate_shape(
        &self,
        value_type: &CheckedType,
        origin: &Origin,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        if self.collapsed != (self.count == LoweredRepeatCount::Fixed(1)) {
            diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                "repeated product collapse marker disagrees with its count",
            ));
        }
        if self.count != LoweredRepeatCount::for_type(value_type) {
            let message = match (&self.count, value_type) {
                (LoweredRepeatCount::Fixed(_), CheckedType::Array { .. }) => {
                    "repeated product fixed count disagrees with its symbolic array result type"
                }
                _ => "repeated product count disagrees with its result type",
            };
            diagnostics.push(Diagnostic::new(origin.span.clone(), message));
        }
    }
}

/// An ordinary value read. The symbol catalog supplies the storage
/// classification; initialization checking, movement, and singleton identity
/// are copied from the checked occurrence.
#[derive(Debug, Clone)]
pub(crate) struct LoweredName {
    pub symbol: SymbolId,
    pub requires_initialization_check: bool,
    /// The symbol's storage is mutable and reads must go through its cell.
    pub mutable: bool,
    /// Singleton type identity when the name denotes a singleton value.
    pub singleton: Option<TypeId>,
    /// The tracked signal read or derived read this occurrence performs, when
    /// the symbol carries reactive storage.
    pub reactive: Option<LoweredReactiveOperationId>,
}

/// A checked integer literal. The magnitude is parsed once and validated
/// against the selected scalar type at lowering time.
#[derive(Debug, Clone)]
pub(crate) struct LoweredInteger {
    pub value: u64,
    pub integer_type: IntegerType,
}

/// A checked float literal with the exact finite value selected by checking.
/// `F32` literals are stored widened to `f64` exactly as the backend does.
#[derive(Debug, Clone)]
pub(crate) struct LoweredFloat {
    pub value: f64,
    pub float_type: FloatType,
}

/// A string literal decoded to UTF-8 once during lowering.
#[derive(Debug, Clone)]
pub(crate) struct LoweredString {
    pub value: String,
}

/// A C string literal decoded once. `bytes` includes the required trailing
/// NUL and never contains an interior NUL.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCString {
    pub bytes: Vec<u8>,
}

/// A structural access with its base expression and checked dereference path.
#[derive(Debug, Clone)]
pub(crate) struct LoweredAccess {
    pub base: ExpressionId,
    pub kind: LoweredAccessKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoweredAccessKind {
    /// Distinct representation or single-element distinct access.
    Representation { dereference: Vec<CheckedType> },
    /// Element `index` of a fixed product, optionally behind `Ref` payloads.
    Product {
        index: usize,
        dereference: Vec<CheckedType>,
    },
    /// Element `index` of a `Slice`, bounds-checked at runtime.
    Slice {
        index: usize,
        dereference: Vec<CheckedType>,
    },
    /// The single-element shortcut, read directly after any dereference.
    Scalar { dereference: Vec<CheckedType> },
}

/// A source pattern with its checked type and lowered children. Patterns are
/// shared by function parameter templates, runtime pattern bindings, and
/// match arms.
#[derive(Debug, Clone)]
pub(crate) struct LoweredPattern {
    pub origin: Origin,
    pub value_type: CheckedType,
    pub kind: LoweredPatternKind,
    /// The checked pattern test plan, consumed directly by emission.
    pub test: LoweredPatternTestPlan,
}

/// The checked test decisions for one pattern. A template whose
/// subject or patterns still contain declared parameters leaves the decisions
/// undecided (`None` / `LoweredPatternIdentity::None`); materialization
/// recomputes the plan from the substituted types like the other specialization
/// derived facts, and the instance-body validator requires it to agree.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoweredPatternTestPlan {
    /// The checked type of the value the pattern is tested against. For a
    /// top-level match arm this is `LoweredMatch::source`; nested patterns get
    /// their element, payload, or representation type from the parent plan.
    pub subject: CheckedType,
    /// The sum alternative a tag compare selects, when the subject is a sum
    /// and this pattern selects exactly one alternative.
    pub sum_alternative: Option<usize>,
    /// The nominal identity this pattern tests; `None` when the pattern is
    /// structural or the plan is undecided.
    pub identity: LoweredPatternIdentity,
    /// The decoded bytes of a string-literal pattern.
    pub literal: Option<Vec<u8>>,
}

impl LoweredPatternTestPlan {
    /// An undecided plan for a template whose types still contain declared
    /// parameters. Materialization replaces it with the concrete decisions.
    pub(crate) fn undecided(subject: CheckedType) -> Self {
        LoweredPatternTestPlan {
            subject,
            sum_alternative: None,
            identity: LoweredPatternIdentity::None,
            literal: None,
        }
    }
}

/// The nominal identity a pattern test uses. Lowering asks the resolver whether
/// each nominal branch names a builtin or a singleton and records the answer
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredPatternIdentity {
    /// No identity test at this level: a structural pattern, a plain binding,
    /// or a sum-alternative test whose tag compare is already recorded.
    None,
    /// A `String` nominal destructure over a subject of builtin `String`.
    String,
    /// A `Ref` nominal destructure; the emitter loads the payload.
    Ref,
    /// A distinct representation destructure where the subject is the distinct
    /// type itself.
    Representation,
    /// A singleton name pattern (`True`, `False`, or a declared singleton).
    Singleton,
}

/// The plan-relevant shape of a syntax pattern, captured before its children
/// are lowered so the plan can be computed without the arena records.
#[derive(Debug, Clone)]
enum PatternPlanShape {
    Structural,
    At,
    Binding { singleton: Option<TypeId> },
    Product { elements: usize },
    Nominal { target: Option<TypeId> },
    Literal { literal: String },
}

/// Computes a pattern test against its use site's subject type. Child subjects
/// follow syntax order: at bindings before nested patterns, product elements
/// in order, and nominal arguments as one child.
fn pattern_test_plan(
    subject: &CheckedType,
    value_type: &CheckedType,
    shape: &PatternPlanShape,
    builtin_of: &dyn Fn(TypeId) -> Option<BuiltinType>,
    string_representation: Option<&CheckedType>,
) -> Result<(LoweredPatternTestPlan, Vec<CheckedType>), String> {
    let mut plan = LoweredPatternTestPlan {
        subject: subject.clone(),
        sum_alternative: None,
        identity: LoweredPatternIdentity::None,
        literal: None,
    };
    let mut children = Vec::new();
    match shape {
        PatternPlanShape::Structural => {}
        PatternPlanShape::At => {
            children.push(subject.clone());
            children.push(subject.clone());
        }
        PatternPlanShape::Binding {
            singleton: Some(singleton),
        } => {
            plan.identity = LoweredPatternIdentity::Singleton;
            match subject {
                CheckedType::Sum(sum) => {
                    plan.sum_alternative = Some(
                        sum.alternatives
                            .iter()
                            .position(|alternative| {
                                matches!(
                                    alternative,
                                    CheckedType::Distinct { id, .. } if id == singleton
                                )
                            })
                            .ok_or_else(|| {
                                "singleton pattern does not select a sum alternative".to_owned()
                            })?,
                    );
                }
                CheckedType::Distinct { id, .. } if id == singleton => {}
                _ => {
                    return Err("checked singleton pattern has an incompatible value".to_owned());
                }
            }
        }
        PatternPlanShape::Binding { singleton: None } => {
            if let CheckedType::Sum(sum) = subject
                && value_type != subject
            {
                plan.sum_alternative = Some(
                    sum.alternatives
                        .iter()
                        .position(|alternative| alternative == value_type)
                        .ok_or_else(|| {
                            "typed match pattern does not select a sum alternative".to_owned()
                        })?,
                );
            }
        }
        PatternPlanShape::Product { elements } => {
            if *elements == 1 {
                children.push(subject.clone());
            } else if *elements > 1 {
                let CheckedType::Product(product) = subject else {
                    return Err("checked product pattern has a non-product value".to_owned());
                };
                if product.elements.len() != *elements {
                    return Err(
                        "checked product pattern does not match its value layout".to_owned()
                    );
                }
                children.extend(
                    product
                        .elements
                        .iter()
                        .map(|element| element.value_type.clone()),
                );
            }
        }
        PatternPlanShape::Nominal { target } => match subject {
            CheckedType::String
                if target
                    .and_then(|target| builtin_of(target))
                    .is_some_and(|builtin| builtin == BuiltinType::String) =>
            {
                plan.identity = LoweredPatternIdentity::String;
                let representation = string_representation.ok_or_else(|| {
                    "standard library String representation was not checked".to_owned()
                })?;
                children.push(representation.clone());
            }
            CheckedType::Ref(payload)
                if target
                    .and_then(|target| builtin_of(target))
                    .is_some_and(|builtin| builtin == BuiltinType::Ref) =>
            {
                plan.identity = LoweredPatternIdentity::Ref;
                children.push(payload.as_ref().clone());
            }
            CheckedType::Sum(sum) => {
                let expected = target.ok_or_else(|| "unresolved match pattern".to_owned())?;
                let index = sum
                    .alternatives
                    .iter()
                    .position(|alternative| {
                        matches!(alternative, CheckedType::Distinct { id, .. } if *id == expected)
                    })
                    .ok_or_else(|| "match pattern does not select a sum alternative".to_owned())?;
                plan.sum_alternative = Some(index);
                let CheckedType::Distinct { representation, .. } = &sum.alternatives[index] else {
                    return Err("checked sum alternative is not a distinct type".to_owned());
                };
                children.push(representation.as_ref().clone());
            }
            CheckedType::Distinct {
                id, representation, ..
            } if *target == Some(*id) => {
                plan.identity = LoweredPatternIdentity::Representation;
                children.push(representation.as_ref().clone());
            }
            _ => {
                return Err("checked nominal pattern has an incompatible value".to_owned());
            }
        },
        PatternPlanShape::Literal { literal } => {
            let text = staple_syntax::string_literal::decode(literal)
                .map_err(|message| message.to_owned())?;
            plan.literal = Some(text.clone().into_bytes());
            match subject {
                CheckedType::String | CheckedType::StringLiteralSet(_) => {}
                CheckedType::Sum(sum) => {
                    plan.sum_alternative = Some(
                        sum.alternatives
                            .iter()
                            .position(|alternative| match alternative {
                                CheckedType::String => true,
                                CheckedType::StringLiteralSet(values) => values.contains(&text),
                                _ => false,
                            })
                            .ok_or_else(|| {
                                "checked sum has no string literal alternative".to_owned()
                            })?,
                    );
                }
                _ => {
                    return Err("checked string pattern has an incompatible value".to_owned());
                }
            }
        }
    }
    Ok((plan, children))
}

/// The plan-relevant shape of a lowered pattern, used when materialization
/// recomputes a concrete plan from the instance-local kind.
fn pattern_plan_shape_from_kind(kind: &LoweredPatternKind) -> PatternPlanShape {
    match kind {
        LoweredPatternKind::Wildcard => PatternPlanShape::Structural,
        LoweredPatternKind::Binding { singleton, .. } => PatternPlanShape::Binding {
            singleton: *singleton,
        },
        LoweredPatternKind::Product { elements, .. } => PatternPlanShape::Product {
            elements: elements.len(),
        },
        LoweredPatternKind::Nominal { target, .. } => PatternPlanShape::Nominal { target: *target },
        LoweredPatternKind::Literal { literal } => PatternPlanShape::Literal {
            literal: literal.clone(),
        },
        LoweredPatternKind::At { .. } => PatternPlanShape::At,
    }
}

/// The plan-relevant shape of a syntax pattern, or a diagnostic when the
/// resolver has no fact the shape needs (a singleton/target type).
fn pattern_plan_shape(
    resolved: &ResolvedModule,
    pattern: &Pattern,
) -> Result<PatternPlanShape, Diagnostic> {
    Ok(match pattern {
        Pattern::Wildcard(_) => PatternPlanShape::Structural,
        Pattern::Binding(binding) => PatternPlanShape::Binding {
            singleton: resolved.type_for_pattern(binding.syntax.id),
        },
        Pattern::Product(product) => PatternPlanShape::Product {
            elements: product.elements.len(),
        },
        Pattern::Nominal(nominal) => PatternPlanShape::Nominal {
            target: resolved.type_for_pattern(nominal.syntax.id),
        },
        Pattern::StringLiteral(literal) => PatternPlanShape::Literal {
            literal: literal.literal.clone(),
        },
        Pattern::At(_) => PatternPlanShape::At,
        Pattern::Splice(splice) => {
            return Err(Diagnostic::new(
                splice.syntax.span.clone(),
                "unexpanded pattern splice reached lowering",
            ));
        }
    })
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredPatternKind {
    Wildcard,
    Binding {
        initialization_state_only: bool,
        /// Source spelling retained for LLVM value names.
        name: String,
        /// The symbol this pattern binds. Absent for singleton patterns such
        /// as `True`, which name an existing value instead of binding one.
        symbol: Option<SymbolId>,
        /// The singleton type a name-like pattern selects, when it is not a
        /// binding.
        singleton: Option<TypeId>,
        mutable: bool,
        moved: bool,
    },
    Product {
        elements: Vec<PatternId>,
        mutable: bool,
        moved: bool,
    },
    /// A nominal destructuring pattern (`Ref inner`, `Some value`, ...) with
    /// the named type it selects and its argument pattern.
    Nominal {
        target: Option<TypeId>,
        name: String,
        /// Whether the whole nominal destructure transfers ownership.
        moved: bool,
        argument: PatternId,
    },
    /// A string literal pattern; the literal is retained exactly as written.
    Literal {
        literal: String,
    },
    At {
        binding: PatternId,
        pattern: PatternId,
    },
}

/// A normalized assignment target. Places exist only as mutation destinations;
/// code generation turns them back into storage pointers without consulting
/// the source AST.
#[derive(Debug, Clone)]
pub(crate) struct LoweredPlace {
    pub origin: Origin,
    pub value_type: CheckedType,
    pub kind: LoweredPlaceKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredPlaceKind {
    /// Direct storage of a local, parameter, or global symbol.
    Symbol { symbol: SymbolId },
    /// Symbol storage reached through a shared capture cell.
    CapturedCell { symbol: SymbolId },
    /// A non-place base materialized into temporary storage so it can be
    /// mutated (`x[i] = v` where `x` is not itself a place root).
    Temporary { expression: ExpressionId },
    /// An ambient resource value in scope, bound to its selected provider.
    Resource { use_: LoweredResourceUseId },
    /// A pointer into a reference value: `reference` evaluates the `Ref`
    /// container and `dereference` records the crossed payloads
    /// outermost-first, matching `CheckedAccess`.
    Dereference {
        reference: ExpressionId,
        dereference: Vec<CheckedType>,
    },
    /// Element `index` of a product place. `slice` marks a `Slice` base,
    /// whose pointer is loaded from the slice representation and bounds
    /// checked rather than projected directly.
    ProductElement {
        base: PlaceId,
        index: usize,
        slice: bool,
    },
    /// The representation pointer of a distinct value, covering both `.*`
    /// and single-element distinct access.
    Representation { base: PlaceId },
    /// `base[index]` mutation, dispatched through the `MutateIndex` trait.
    Indexed { base: PlaceId, index: ExpressionId },
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredBlock {
    pub origin: Origin,
    pub items: Vec<ItemId>,
    pub result: Option<ExpressionId>,
}

/// One runtime item inside a module initializer or a runtime block.
#[derive(Debug, Clone)]
pub(crate) struct LoweredItem {
    pub origin: Origin,
    pub kind: LoweredItemKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredItemKind {
    Binding(LoweredBindingItem),
    PatternBinding(LoweredPatternBindingItem),
    Assignment(LoweredAssignmentItem),
    Return(LoweredReturnItem),
    Break(LoweredBreakItem),
    Continue(LoweredContinueItem),
    Expression(LoweredExpressionStatementItem),
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredBindingItem {
    /// Checked def storage allocated before the block items: true is a state-only cell.
    pub predeclare_state_only: Option<bool>,
    /// State access uses the cell directly rather than its value/state struct.
    pub initialization_state_only: bool,
    /// The bound runtime symbol. Absent for compile-time-only `const`
    /// bindings, which stay outside the runtime symbol catalog.
    pub symbol: Option<SymbolId>,
    pub value: Option<ExpressionId>,
    pub compile_time_only: bool,
    /// The binding declares compile-time parameters, so code generation only
    /// records its initialization state.
    pub generic: bool,
    pub derived: bool,
    pub signal: bool,
    /// The signal creation or derived creation this binding performs, when the
    /// symbol carries reactive storage.
    pub reactive: Option<LoweredReactiveOperationId>,
    /// The binding's value lives in a local binding cell rather than an SSA
    /// value or module global storage.
    pub cell: bool,
    pub requires_initialization_check: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredPatternBindingItem {
    pub pattern: PatternId,
    pub value: ExpressionId,
    /// `true` for `let pattern? = value` propagation.
    pub propagating: bool,
    /// Checked propagation metadata, present exactly for propagating
    /// bindings.
    pub propagation: Option<CheckedPropagation>,
    /// The failure value's coercion plan when the propagated result is itself a
    /// sum. `None` when the source is returned unchanged or extracted as a
    /// residual variant.
    pub propagation_plan: Option<LoweredCoercionPlan>,
    /// The source alternative returned as the failure value when the propagated
    /// result is a single, non-sum residual variant (extracted with
    /// `extract_sum_alternative`).
    pub propagation_residual: Option<usize>,
}

impl LoweredPatternBindingItem {
    /// The residual failure alternative of one propagation: the source sum's
    /// alternative equal to a non-sum result. `None` when the source is
    /// returned unchanged or widened through a coercion plan.
    pub(crate) fn residual_alternative(propagation: &CheckedPropagation) -> Option<usize> {
        if propagation.source == propagation.result
            || matches!(propagation.result, CheckedType::Sum(_))
        {
            return None;
        }
        let CheckedType::Sum(source) = &propagation.source else {
            return None;
        };
        source
            .alternatives
            .iter()
            .position(|alternative| alternative == &propagation.result)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredAssignmentItem {
    pub target: PlaceId,
    pub value: ExpressionId,
    /// Selected `MutateIndex` dispatch for an indexed target.
    pub mutate_index: Option<CheckedTraitDispatch>,
    /// The single validated evidence recipe for the `MutateIndex` dispatch.
    pub evidence: Option<TraitEvidence>,
    /// The place's root symbol, whose initialization state is written back.
    pub initialization_symbol: Option<SymbolId>,
    /// Whether the place's previous value must be dropped before the store.
    pub drop_previous: bool,
    /// An indexed (`MutateIndex`) assignment whose base has no place
    /// materializes it into a temporary, dropped after the call when it needs
    /// drop.
    pub drops_base_temporary: bool,
    /// The signal write notification this assignment performs, when the place
    /// is rooted at a signal symbol.
    pub signal_notify: Option<LoweredReactiveOperationId>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredReturnItem {
    pub value: ExpressionId,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredBreakItem {
    pub value: Option<ExpressionId>,
    /// One-based nesting depth of the loop this break targets.
    pub loop_depth: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredContinueItem {
    /// One-based nesting depth of the loop this continue targets.
    pub loop_depth: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredExpressionStatementItem {
    pub expression: ExpressionId,
    /// Whether the discarded result needs a drop after evaluation.
    pub drop_result: bool,
}

/// One capture of a function template, carrying the ownership facts code
/// generation currently reads from the type checker.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCapture {
    pub symbol: SymbolId,
    pub borrowed: bool,
    pub non_owning: bool,
    /// The capture must be reached through a shared cell (mutable storage,
    /// derived binding, or initialization state) rather than by value.
    pub requires_cell: bool,
}

/// Orthogonal function-template classifications. A thunk can be a derived
/// evaluator and a resource helper at the same time, so these are explicit
/// flags rather than a single enum.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LoweredFunctionClass {
    pub declared: bool,
    pub implicit_thunk: bool,
    pub derived_evaluator: bool,
    pub coroutine_body: bool,
    pub resource_helper: bool,
    pub external: bool,
    pub intrinsic: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredFunction {
    pub origin: Origin,
    pub semantic_id: FunctionId,
    pub name: String,
    pub module: ModuleId,
    pub binding_symbol: Option<SymbolId>,
    pub signature: CheckedFunctionType,
    pub bounds: Vec<CheckedTraitBound>,
    pub parameter_style: staple_syntax::FunctionParameterStyle,
    /// The lowered parameter pattern, with bound symbols in source order.
    pub parameter_pattern: PatternId,
    pub parameters: Vec<SymbolId>,
    pub captures: Vec<LoweredCapture>,
    pub body_origin: Origin,
    pub body_syntax: SyntaxId,
    pub body: Option<BlockId>,
    /// The body block's own header facts. When the function body lowers to
    /// `LoweredExpressionKind::Block`, the block expression's coercion (and
    /// moved symbols) would otherwise be lost by unwrapping it to its root
    /// block; emission applies them to the body result.
    pub body_coercion: Option<CheckedCoercion>,
    pub body_coercion_plan: Option<LoweredCoercionPlan>,
    pub body_moved_symbols: Vec<SymbolId>,
    pub class: LoweredFunctionClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredEntryResourceKind {
    Io,
    Reactive,
}

/// An IO or reactive resource the executable entry installs before it runs
/// its initializer items. Recorded as initializer metadata rather than as
/// synthetic runtime AST nodes.
#[derive(Debug, Clone)]
pub(crate) struct LoweredEntryResource {
    pub kind: LoweredEntryResourceKind,
    pub resource: CheckedResource,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredInitializer {
    /// Collision-free symbol assigned after catalog closure.
    pub name: String,
    pub origin: Origin,
    pub module: ModuleId,
    pub resources: Vec<LoweredEntryResource>,
    /// The module's ordered runtime items, lowered into `body`'s item list.
    pub body: BlockId,
    /// Tests locate the executable entry's initializer by this flag.
    #[cfg(test)]
    pub executable_entry: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredModuleInfo {
    pub origin: Origin,
    pub semantic_id: ModuleId,
    /// The stable, load-order-independent prefix used to mangle the module's
    /// symbols (`__staple_m{prefix}.{name}`, `__staple_init_m{prefix}`).
    pub symbol_prefix: String,
    pub parent: Option<ModuleId>,
    pub initialization_index: usize,
    pub initializer: InitializerId,
    /// Tests identify modules by their qualified name.
    #[cfg(test)]
    pub qualified_name: String,
    /// Tests locate a companion module by this flag.
    #[cfg(test)]
    pub companion: bool,
    /// Tests locate the executable entry module by this flag.
    #[cfg(test)]
    pub executable_entry: bool,
}

/// The primary storage category of a symbol. Independent facts (mutation,
/// moves, initialization checking, capture-cell use) stay in explicit
/// `LoweredSymbol` flags rather than overloading this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolStorage {
    ImmutableValue,
    MutableCell,
    GlobalStorage,
    FunctionBinding,
    DerivedBinding,
    Signal,
    CapturedCell,
    ExternalSymbol,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredSymbol {
    /// Checked symbol type used for initialization-state layout specialization.
    pub initialization_state_type: CheckedType,
    pub initialization_state_only: bool,
    pub origin: Origin,
    pub semantic_id: SymbolId,
    pub module: ModuleId,
    /// The declared name of a module-level binding, pattern binding, or
    /// `extern` binding. Empty for function-local symbols, whose LLVM names
    /// are backend-local.
    pub name: String,
    pub owner: Option<FunctionId>,
    pub value_type: CheckedType,
    pub storage: SymbolStorage,
    pub requires_initialization_check: bool,
    /// Mirrors `TypedModule::has_mutable_storage`: an ordinary `mut` binding,
    /// a signal, or a parameter with an explicit mutation effect.
    pub mutable_storage: bool,
    /// The symbol is captured by some function.
    pub captured: bool,
    /// The symbol is non-owning (a value frozen by borrowing through `Ref`).
    pub non_owning: bool,
    pub derived: bool,
    pub signal: bool,
    pub mutated_parameter: bool,
    pub captured_cell: bool,
    pub function: Option<FunctionId>,
    pub constructor: Option<TypeId>,
    pub singleton: Option<TypeId>,
    pub intrinsic: Option<crate::IntrinsicFunction>,
    pub external: bool,
    /// The resolver assigned the symbol to an arity-overload set, so the
    /// backend disambiguates its emitted name (external names get an arity
    /// suffix, module globals get an overload suffix).
    pub overloaded: bool,
    /// The symbol is declared at module scope, so a signal or derived symbol
    /// lives in a module global rather than a binding cell.
    pub module_symbol: bool,
    /// The backend declares module-level storage for this symbol: a
    /// non-generic module binding or a top-level pattern binding that is not
    /// an already-declared external symbol.
    pub has_global: bool,
    /// The harness registers a GC root region for this symbol's module global
    /// (`has_global` and the value type contains a managed reference).
    pub global_root: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredTypeMetadata {
    pub origin: Origin,
    pub semantic_id: TypeId,
    /// Tests locate a type's metadata by its declared name.
    #[cfg(test)]
    pub name: String,
    pub module: ModuleId,
    pub builtin: Option<BuiltinType>,
    pub recursive_construction: Option<RecursiveConstruction>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredTraitMetadata {
    pub origin: Origin,
    pub semantic_id: TraitId,
    pub name: String,
    pub module: ModuleId,
    pub parameters: Vec<CheckedType>,
    pub prerequisites: Vec<CheckedTraitBound>,
    /// Declared method order.
    pub methods: Vec<TraitMethodId>,
    /// Default implementations, in declared method order.
    pub default_methods: Vec<(TraitMethodId, FunctionId)>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredTraitMethodMetadata {
    pub origin: Origin,
    pub semantic_id: TraitMethodId,
    pub name: String,
    pub trait_id: TraitId,
    pub value_type: CheckedType,
    pub default_function: Option<FunctionId>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredTraitImplementationMetadata {
    pub origin: Origin,
    pub trait_id: TraitId,
    /// Implementation-declared type parameters, ascending.
    pub parameters: Vec<TypeParameterId>,
    pub arguments: Vec<CheckedType>,
    pub bounds: Vec<CheckedTraitBound>,
    pub negative: bool,
    /// Selected method functions, ordered by semantic method ID.
    pub methods: Vec<(TraitMethodId, FunctionId)>,
}

/// Type-checker-owned standard and runtime subsystem identities. Optional
/// slots are absent in library-only or `no_prelude` programs, and every
/// present ID must resolve to the matching catalog family.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoweredSemanticIds {
    pub natural_trait: Option<TraitId>,
    pub sized_trait: Option<TraitId>,
    pub copy_trait: Option<TraitId>,
    /// The checker-selected `Clone` trait, used by buffer-clone
    /// artifact and evidence recording.
    pub clone_trait: Option<TraitId>,
    pub drop_trait: Option<TraitId>,
    pub default_trait: Option<TraitId>,
    pub debug_trait: Option<TraitId>,
    pub display_trait: Option<TraitId>,
    pub index_trait: Option<TraitId>,
    pub mutate_index_trait: Option<TraitId>,
    pub into_iterator_trait: Option<TraitId>,
    pub iterator_trait: Option<TraitId>,
    pub io_type: Option<TypeId>,
    pub reactive_type: Option<TypeId>,
    pub coroutine_type: Option<TypeId>,
    pub task_type: Option<TypeId>,
    pub completed_type: Option<TypeId>,
    pub cancelled_type: Option<TypeId>,
    pub tasks_type: Option<TypeId>,
    pub scheduler_type: Option<TypeId>,
    pub wait_type: Option<TypeId>,
    pub resolver_type: Option<TypeId>,
    pub completion_token_type: Option<TypeId>,
    pub io_resource: Option<CheckedResource>,
    pub reactive_resource: Option<CheckedResource>,
    pub string_representation: Option<CheckedType>,
}

/// The complete owned lowering representation. Arena order is insertion order,
/// which lowering defines to be deterministic program/source order.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoweredProgram {
    modules: Catalog<ModuleId, LoweredModuleInfo, LoweredModuleId>,
    expressions: Arena<LoweredExpression, ExpressionId>,
    /// Lookup only; traversal always uses the expression arena. Repeated
    /// lowering of the same occurrence key returns the first allocated ID.
    /// Contextual defaults use distinct keys so shared default syntax does not
    /// alias incompatible node types.
    expression_lookup: HashMap<ExpressionKey, ExpressionId>,
    patterns: Arena<LoweredPattern, PatternId>,
    places: Arena<LoweredPlace, PlaceId>,
    blocks: Arena<LoweredBlock, BlockId>,
    /// Lookup only; traversal always uses the block arena. Repeated lowering
    /// of the same block occurrence returns the first allocated ID.
    block_lookup: HashMap<ExpressionKey, BlockId>,
    items: Arena<LoweredItem, ItemId>,
    functions: Catalog<FunctionId, LoweredFunction, LoweredFunctionId>,
    symbols: Catalog<SymbolId, LoweredSymbol, LoweredSymbolId>,
    types: Catalog<TypeId, LoweredTypeMetadata, LoweredTypeId>,
    traits: Catalog<TraitId, LoweredTraitMetadata, LoweredTraitId>,
    trait_methods: Catalog<TraitMethodId, LoweredTraitMethodMetadata, LoweredTraitMethodId>,
    trait_implementations: Arena<LoweredTraitImplementationMetadata, LoweredTraitImplementationId>,
    calls: Arena<LoweredCall, LoweredCallId>,
    callable_values: Arena<LoweredCallableValue, LoweredCallableValueId>,
    resource_providers: Arena<LoweredResourceProvider, LoweredResourceProviderId>,
    resource_uses: Arena<LoweredResourceUse, LoweredResourceUseId>,
    withs: Arena<LoweredWith, LoweredWithId>,
    reactive_operations: Arena<LoweredReactiveOperation, LoweredReactiveOperationId>,
    reactive_callbacks: Arena<LoweredReactiveCallback, LoweredReactiveCallbackId>,
    coroutine_plans: Arena<LoweredCoroutinePlan, LoweredCoroutinePlanId>,
    /// Lookup only; traversal always uses the plan arena.
    coroutine_plan_lookup: HashMap<SyntaxId, LoweredCoroutinePlanId>,
    /// Lookup only: body-thunk function to its plan.
    coroutine_plan_by_thunk: HashMap<FunctionId, LoweredCoroutinePlanId>,
    coros: Arena<LoweredCoro, LoweredCoroId>,
    awaits: Arena<LoweredAwait, LoweredAwaitId>,
    initializers: Arena<LoweredInitializer, InitializerId>,
    /// The reachable function instances in first-discovery order; the matching
    /// key of each instance is interned in `specializations` at the instance's
    /// own ordinal.
    instances: Arena<LoweredFunctionInstance, FunctionInstanceId>,
    /// Constructor-adapter and structural-method requests in first-discovery
    /// order; the matching key is interned in `specializations`.
    artifacts: Arena<LoweredArtifactRequest, LoweredArtifactRequestId>,
    /// Artifact uses recorded on module initializers by closure scanners,
    /// indexed by `InitializerId` in scan order. Initializer requests from
    /// specialization stay request-root-only, so this starts empty and only
    /// closure-phase scanners add entries.
    initializer_artifact_uses: Vec<Vec<LoweredArtifactUse>>,
    /// Artifact edges owned by module initializers, indexed by `InitializerId`
    /// in request order.
    initializer_artifacts: Vec<Vec<LoweredArtifactDependency>>,
    /// Instance uses recorded on module initializers by closure scanners,
    /// indexed by `InitializerId` in scan order.
    initializer_instance_uses: Vec<Vec<LoweredInstanceUse>>,
    /// Instance edges owned by module initializers, indexed by `InitializerId`
    /// in request order. Initializer instance requests from specialization stay
    /// request-root-only; only closure-phase scans add entries.
    initializer_instances: Vec<Vec<LoweredInstanceDependency>>,
    /// Owned bindings of nested initializer block locals, indexed by
    /// `InitializerId` in registration order. Module globals are never owned.
    initializer_owned_bindings: Vec<Vec<LoweredOwnedBinding>>,
    /// Concrete bindings for every dispatch/construction site in each module
    /// initializer, keyed by program-arena `LoweredBindingSite` and indexed by
    /// `InitializerId`. Built at the closure fixed point; instance bodies carry
    /// the same table per body.
    initializer_bindings: Vec<std::collections::BTreeMap<LoweredBindingSite, LoweredBoundTarget>>,
    /// Resolved trait evidence for the initializer sites that need it, indexed
    /// by `InitializerId`.
    initializer_evidence: Vec<std::collections::BTreeMap<LoweredBindingSite, TraitEvidence>>,
    /// Append-only instance/artifact key catalog. Only the specialization
    /// worklist reserves ordinals, before visiting a body, so recursion
    /// converges.
    specializations: SpecializationCatalog,
    semantic_ids: LoweredSemanticIds,
    string_formatting: LoweredStringFormatting,
    /// The ordered, deduplicated runtime surfaces the closed catalog needs.
    /// Recorded after the closure fixed point; emission installs each surface
    /// only when present.
    pub(crate) runtime_requirements: LoweredRuntimeRequirements,
    /// Transient lowering state: the number of currently enclosing loops,
    /// recorded on break/continue items and loop nodes so validation can tie
    /// exits to the loop that owns them. Not part of the lowered program.
    loop_depth: usize,
    /// Transient lowering state: inner call syntaxes consumed by an outer
    /// juxtaposed call's checked plan. They are never lowered on their own.
    consumed_calls: HashSet<SyntaxId>,
    /// Transient lowering state: the active lexical provider stack while one
    /// function body or module initializer lowers. The last matching provider
    /// by checked value type agrees with the recorded cleanup selection.
    active_resource_providers: Vec<LoweredResourceProviderId>,
    /// Test-only: the rounds and total growth the last closed artifact catalog
    /// observed, so the closure statistics can record the observed maxima rather
    /// than assert the defensive bounds blindly.
    #[cfg(test)]
    pub(crate) closure_stats: Option<ClosureStats>,
}

/// Test-only closure-run statistics.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClosureStats {
    pub rounds: usize,
    pub growth: usize,
}

impl LoweredProgram {
    /// Copies deterministic declaration metadata out of checked compiler state.
    /// Symbols are snapshotted first so expression lowering can read storage
    /// facts from the catalog instead of the resolver.
    fn snapshot(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        // Declaration catalogs are populated before any expression lowers:
        // module initializers and function bodies reference functions, types,
        // traits, implementations, and symbols by semantic ID, so every
        // catalog must already be complete when a callable value is lowered.
        let mut diagnostics = self.snapshot_symbols(module);
        diagnostics.extend(self.snapshot_types(module));
        diagnostics.extend(self.snapshot_traits(module));
        diagnostics.extend(self.snapshot_semantic_ids(module));
        diagnostics.extend(self.snapshot_functions(module));
        diagnostics.extend(self.snapshot_modules(module));
        diagnostics.extend(self.validate_runtime_metadata(module));
        diagnostics
    }

    /// Copies type-checker-owned standard and runtime subsystem identities.
    /// No name lookup happens here; absent prelude or library-only programs
    /// keep whichever IDs the checker selected.
    fn snapshot_semantic_ids(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let ids = module.semantic_ids();
        self.semantic_ids = LoweredSemanticIds {
            natural_trait: ids.natural_trait,
            sized_trait: ids.sized_trait,
            copy_trait: ids.copy_trait,
            clone_trait: ids.clone_trait,
            drop_trait: ids.drop_trait,
            default_trait: ids.default_trait,
            debug_trait: ids.debug_trait,
            display_trait: ids.display_trait,
            index_trait: ids.index_trait,
            mutate_index_trait: ids.mutate_index_trait,
            into_iterator_trait: ids.into_iterator_trait,
            iterator_trait: ids.iterator_trait,
            io_type: ids.io_type,
            reactive_type: ids.reactive_type,
            coroutine_type: ids.coroutine_type,
            task_type: ids.task_type,
            completed_type: ids.completed_type,
            cancelled_type: ids.cancelled_type,
            tasks_type: ids.tasks_type,
            scheduler_type: ids.scheduler_type,
            wait_type: ids.wait_type,
            resolver_type: ids.resolver_type,
            completion_token_type: ids.completion_token_type,
            io_resource: module.io_resource(),
            reactive_resource: module.reactive_resource(),
            string_representation: module.string_representation().cloned(),
        };
        let formatting = module.string_formatting();
        self.string_formatting = LoweredStringFormatting {
            constructor: formatting.formatter_new,
            write: formatting.formatter_write,
            finish: formatting.formatter_finish,
        };
        Vec::new()
    }

    /// Inserts type metadata in ascending `TypeId`, retaining origin, module,
    /// declaration kind, builtin/recursive classification, checked parameter
    /// templates, and the compact representation template.
    fn snapshot_types(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let resolved = module.resolved();
        let mut diagnostics = Vec::new();
        for (id, declaration) in resolved.types_in_id_order() {
            let origin = Origin {
                syntax: declaration.syntax.id,
                span: declaration.syntax.span.clone(),
            };
            let Some(module_id) = resolved.definition_module(DefinitionId::Type(id)) else {
                diagnostics.push(Diagnostic::new(
                    origin.span,
                    format!(
                        "cannot lower type `{}` (type id {}) without an owning module",
                        declaration.name, id.0
                    ),
                ));
                continue;
            };
            let value = LoweredTypeMetadata {
                origin: origin.clone(),
                semantic_id: id,
                #[cfg(test)]
                name: declaration.name.clone(),
                module: module_id,
                builtin: resolved.builtin_type(id),
                recursive_construction: resolved.recursive_construction(id),
            };
            if let Err(diagnostic) = self.types.insert("type", id, origin, value) {
                diagnostics.push(diagnostic);
            }
        }
        diagnostics
    }

    /// Inserts traits and their methods by semantic ID, preserving declared
    /// method order inside each trait, then records checked implementations in
    /// resolver declaration order.
    fn snapshot_traits(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let resolved = module.resolved();
        let mut diagnostics = Vec::new();
        let parameter_arguments = module
            .trait_parameter_arguments_in_id_order()
            .into_iter()
            .map(|(id, arguments)| (id, arguments.to_vec()))
            .collect::<HashMap<_, _>>();
        let method_types = module
            .trait_method_types_in_id_order()
            .into_iter()
            .map(|(id, value_type)| (id, value_type.clone()))
            .collect::<HashMap<_, _>>();
        let method_origins = resolved
            .trait_methods_in_id_order()
            .into_iter()
            .map(|(id, member)| {
                (
                    id,
                    (
                        Origin {
                            syntax: member.syntax.id,
                            span: member.syntax.span.clone(),
                        },
                        member.name.clone(),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        for (trait_id, trait_) in resolved.traits_in_id_order() {
            let origin = Origin {
                syntax: trait_.declaration.syntax.id,
                span: trait_.declaration.syntax.span.clone(),
            };
            let Some(module_id) = resolved.definition_module(DefinitionId::Trait(trait_id)) else {
                diagnostics.push(Diagnostic::new(
                    origin.span,
                    format!(
                        "cannot lower trait `{}` (trait id {}) without an owning module",
                        trait_.declaration.name, trait_id.0
                    ),
                ));
                continue;
            };
            let default_methods = trait_
                .methods
                .iter()
                .filter_map(|method| {
                    trait_
                        .default_methods
                        .get(method)
                        .map(|function| (*method, *function))
                })
                .collect();
            let value = LoweredTraitMetadata {
                origin: origin.clone(),
                semantic_id: trait_id,
                name: trait_.declaration.name.clone(),
                module: module_id,
                parameters: parameter_arguments
                    .get(&trait_id)
                    .cloned()
                    .unwrap_or_default(),
                prerequisites: module.trait_prerequisites(trait_id).to_vec(),
                methods: trait_.methods.clone(),
                default_methods,
            };
            if let Err(diagnostic) = self.traits.insert("trait", trait_id, origin, value) {
                diagnostics.push(diagnostic);
            }
            for method in &trait_.methods {
                let Some((origin, name)) = method_origins.get(method).cloned() else {
                    diagnostics.push(Diagnostic::new(
                        Span::Compiler,
                        format!(
                            "cannot lower trait method {method:?} without a declaration origin"
                        ),
                    ));
                    continue;
                };
                let Some(value_type) = method_types.get(method).cloned() else {
                    diagnostics.push(Diagnostic::new(
                        origin.span,
                        format!("cannot lower trait method {method:?} without a checked type"),
                    ));
                    continue;
                };
                let value = LoweredTraitMethodMetadata {
                    origin: origin.clone(),
                    semantic_id: *method,
                    name,
                    trait_id,
                    value_type,
                    default_function: trait_.default_methods.get(method).copied(),
                };
                if let Err(diagnostic) =
                    self.trait_methods
                        .insert("trait method", *method, origin, value)
                {
                    diagnostics.push(diagnostic);
                }
            }
        }

        let resolved_implementations = resolved.trait_implementations();
        let checked_implementations = module.checked_trait_implementations();
        if resolved_implementations.len() != checked_implementations.len() {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "lowered trait implementation count {} does not match resolver count {}",
                    checked_implementations.len(),
                    resolved_implementations.len()
                ),
            ));
        }
        for (declared, checked) in resolved_implementations.iter().zip(checked_implementations) {
            let mut parameters = checked.parameters.iter().copied().collect::<Vec<_>>();
            parameters.sort_by_key(|parameter| parameter.0);
            let mut methods = checked
                .methods
                .iter()
                .map(|(method, function)| (*method, *function))
                .collect::<Vec<_>>();
            methods.sort_by_key(|(method, _)| method.0);
            self.trait_implementations
                .push(LoweredTraitImplementationMetadata {
                    origin: Origin {
                        syntax: declared.syntax,
                        span: declared.span.clone(),
                    },
                    trait_id: checked.trait_id,
                    parameters,
                    arguments: checked.arguments.clone(),
                    bounds: checked.bounds.clone(),
                    negative: checked.negative,
                    methods,
                });
        }
        diagnostics
    }

    /// Enumerates resolver-declared runtime symbols in ascending `SymbolId`,
    /// supplementing any referenced but undeclared symbol with a compiler
    /// origin. Compile-time-only consts and macro quote placeholders stay out
    /// of the catalog.
    fn snapshot_symbols(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let resolved = module.resolved();
        let mut diagnostics = Vec::new();
        let facts = SymbolDeclarationFacts::collect(module);
        let declared = resolved.symbols_in_id_order();
        let origins = declared
            .iter()
            .map(|info| {
                (
                    info.id,
                    Origin {
                        syntax: info.declaration,
                        span: info.span.clone(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let mut captured = HashSet::new();
        let mut referenced = HashSet::new();
        for function in module
            .functions()
            .iter()
            .chain(module.implicit_thunks_in_id_order())
        {
            captured.extend(function.captures.iter().copied());
            referenced.extend(pattern_symbols(resolved, &function.pattern));
            referenced.extend(function.captures.iter().copied());
        }
        for info in &declared {
            if compile_time_only_symbol(module, info.id)
                || (!info.module_symbol && info.owner.is_none())
            {
                continue;
            }
            referenced.remove(&info.id);
            let origin = origins[&info.id].clone();
            if let Some(diagnostic) = self.snapshot_symbol(
                module,
                info.id,
                origin,
                info.module,
                info.owner,
                &captured,
                &facts,
            ) {
                diagnostics.push(diagnostic);
            }
        }
        for symbol in referenced {
            if self.symbols.get(symbol).is_some() || compile_time_only_symbol(module, symbol) {
                continue;
            }
            let Some(module_id) = resolved.symbol_module(symbol) else {
                diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!("referenced lowered symbol {symbol:?} has no owning module"),
                ));
                continue;
            };
            let origin = origins
                .get(&symbol)
                .cloned()
                .unwrap_or_else(Origin::compiler);
            if let Some(diagnostic) =
                self.snapshot_symbol(module, symbol, origin, module_id, None, &captured, &facts)
            {
                diagnostics.push(diagnostic);
            }
        }
        diagnostics.extend(initializer_symbol_diagnostics(
            module,
            &origins,
            &self.symbols,
        ));
        diagnostics
    }

    #[allow(clippy::too_many_arguments)]
    fn snapshot_symbol(
        &mut self,
        module: &TypedModule,
        symbol: SymbolId,
        origin: Origin,
        module_id: ModuleId,
        owner: Option<FunctionId>,
        captured: &HashSet<SymbolId>,
        facts: &SymbolDeclarationFacts,
    ) -> Option<Diagnostic> {
        let resolved = module.resolved();
        let Some(value_type) = module.declared_type_of_symbol(symbol) else {
            return Some(Diagnostic::new(
                origin.span,
                format!("cannot lower symbol {symbol:?} without a declared checked type"),
            ));
        };
        let function = module.function_for_symbol(symbol);
        let constructor = resolved.constructor_type(symbol);
        let singleton = resolved.singleton_type(symbol);
        let intrinsic = resolved.intrinsic_function(symbol);
        let external = resolved.is_external_symbol(symbol);
        let derived = module.is_derived_symbol(symbol);
        let signal = resolved.is_signal_symbol(symbol);
        let mutable = module.has_mutable_storage(symbol);
        let module_symbol = resolved.is_module_symbol(symbol);
        let storage = symbol_storage(
            external,
            function.is_some() || constructor.is_some() || singleton.is_some(),
            derived,
            signal,
            captured.contains(&symbol) && mutable,
            module_symbol,
            mutable,
        );
        let has_global = facts.globals.contains(&symbol);
        let global_root = has_global && checked_type_contains_ref(&value_type);
        let value = LoweredSymbol {
            initialization_state_type: module
                .type_of_symbol(symbol)
                .cloned()
                .unwrap_or_else(|| value_type.clone()),
            initialization_state_only: module
                .type_of_symbol(symbol)
                .is_some_and(contains_type_parameter),
            origin: origin.clone(),
            semantic_id: symbol,
            module: module_id,
            name: facts.names.get(&symbol).cloned().unwrap_or_default(),
            owner,
            value_type,
            storage,
            requires_initialization_check: resolved.requires_initialization_state(symbol),
            mutable_storage: mutable,
            captured: captured.contains(&symbol),
            non_owning: module.is_non_owning_symbol(symbol),
            derived,
            signal,
            mutated_parameter: module.is_mutated_parameter(symbol),
            captured_cell: capture_requires_cell(module, symbol),
            function,
            constructor,
            singleton,
            intrinsic,
            external,
            overloaded: resolved.symbol_is_overloaded(symbol),
            module_symbol,
            has_global,
            global_root,
        };
        self.symbols.insert("symbol", symbol, origin, value).err()
    }

    /// Inserts declared functions in resolver order, then implicit thunks in
    /// stable semantic-ID order.
    /// Populates the function catalog in two passes so forward and recursive
    /// references resolve: every function's metadata is inserted first, then
    /// every body is lowered with the complete catalog available.
    fn snapshot_functions(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let derived_evaluators = module
            .derived_evaluators_in_symbol_order()
            .into_iter()
            .map(|(_, function)| function)
            .collect::<HashSet<_>>();
        let mut diagnostics = Vec::new();
        let mut pending = Vec::new();
        for function in module.functions() {
            pending.push((function, false));
        }
        for function in module.implicit_thunks_in_id_order() {
            pending.push((function, true));
        }
        for (function, implicit_thunk) in &pending {
            self.snapshot_function_metadata(
                module,
                function,
                *implicit_thunk,
                &derived_evaluators,
                &mut diagnostics,
            );
        }
        // Plans exist before any body lowers: a `coro` creation site and every
        // `await` inside the body must find its plan while lowering.
        for (function, implicit_thunk) in &pending {
            if *implicit_thunk {
                self.snapshot_coroutine_plan(module, function, &mut diagnostics);
            }
        }
        for (function, _) in &pending {
            match self.lower_function_body(module, function) {
                Ok((body, coercion, coercion_plan, moved_symbols)) => {
                    if let Some(entry) = self.functions.get_mut(function.id) {
                        entry.body = Some(body);
                        entry.body_coercion = coercion;
                        entry.body_coercion_plan = coercion_plan;
                        entry.body_moved_symbols = moved_symbols;
                    }
                    if let Some(plan) = self.coroutine_plan_by_thunk.get(&function.id).copied()
                        && let Some(plan) = self.coroutine_plans.get_mut(plan)
                    {
                        plan.body = Some(body);
                    }
                }
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        diagnostics
    }

    /// Copies the checker's coroutine plan for one coroutine body thunk into an
    /// owned lowered record keyed by its body syntax. The plan links the
    /// owning implicit thunk, ordered captures, checked result and deferred
    /// effects, resume count, frame bindings, awaited result types, and
    /// wait/`until` cancellation classifications; its body block links after
    /// the thunk body lowers and await sites append as they lower.
    fn snapshot_coroutine_plan(
        &mut self,
        module: &TypedModule,
        function: &ResolvedFunction,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let body_syntax = function.body.syntax().id;
        let Some(plan) = module.coroutine_plan(body_syntax) else {
            return;
        };
        let origin = Origin {
            syntax: body_syntax,
            span: function.body.syntax().span.clone(),
        };
        let Some(catalog) = self.functions.get(function.id) else {
            diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                format!(
                    "cannot lower coroutine plan for function {} missing from the function catalog",
                    function.id.0
                ),
            ));
            return;
        };
        // The checked thunk result is `Coroutine{E} T`; the plan's result and
        // deferred effects must be exactly its parts.
        if let Some((effects, result)) = module.coroutine_parts(&catalog.signature.result)
            && (effects != &plan.deferred_effects || result != &plan.result_type)
        {
            diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                "coroutine plan result/deferred effects disagree with its thunk signature",
            ));
            return;
        }
        let captures = catalog.captures.clone();
        let result_type = plan.result_type.clone();
        let deferred_effects = plan.deferred_effects.clone();
        let plan_id = self.coroutine_plans.push(LoweredCoroutinePlan {
            origin,
            body_syntax: plan.body_syntax,
            body: None,
            thunk: function.id,
            captures,
            result_type,
            deferred_effects,
            resume_points: plan.resume_points,
            frame_bindings: plan.frame_bindings.clone(),
            await_result_types: plan.await_result_types.clone(),
            wait_await_states: plan.wait_await_states.clone(),
            until_await_states: plan.until_await_states.clone(),
            awaits: Vec::new(),
        });
        self.coroutine_plan_lookup.insert(plan.body_syntax, plan_id);
        self.coroutine_plan_by_thunk.insert(function.id, plan_id);
    }

    fn snapshot_function_metadata(
        &mut self,
        module: &TypedModule,
        function: &ResolvedFunction,
        implicit_thunk: bool,
        derived_evaluators: &HashSet<FunctionId>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let resolved = module.resolved();
        let body_syntax = function.body.syntax();
        let origin = Origin {
            syntax: body_syntax.id,
            span: body_syntax.span.clone(),
        };
        let Some(signature) = module.type_of_function(function.id).cloned() else {
            diagnostics.push(Diagnostic::new(
                origin.span,
                format!(
                    "cannot lower function `{}` (function id {}) without a checked function type",
                    function.name, function.id.0
                ),
            ));
            return;
        };
        let Some(module_id) = resolved.module_for_syntax(body_syntax.id) else {
            diagnostics.push(Diagnostic::new(
                origin.span,
                format!(
                    "cannot lower function `{}` (function id {}) without an owning module",
                    function.name, function.id.0
                ),
            ));
            return;
        };
        let binding_symbol = function
            .binding_syntax
            .and_then(|syntax| resolved.symbol_for(syntax));
        let derived_evaluator = derived_evaluators.contains(&function.id);
        let coroutine_body = implicit_thunk && module.coroutine_plan(body_syntax.id).is_some();
        let effectful = !signature.effects.resources.is_empty()
            || signature.effects.state.is_some()
            || signature.effects.variable.is_some();
        let resource_helper = implicit_thunk && !derived_evaluator && !coroutine_body && effectful;
        let class = LoweredFunctionClass {
            declared: !implicit_thunk,
            implicit_thunk,
            derived_evaluator,
            coroutine_body,
            resource_helper,
            external: binding_symbol.is_some_and(|symbol| resolved.is_external_symbol(symbol)),
            intrinsic: binding_symbol
                .is_some_and(|symbol| resolved.intrinsic_function(symbol).is_some()),
        };
        let parameter_pattern = match self.lower_function_pattern(module, function, &signature) {
            Ok(pattern) => pattern,
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                return;
            }
        };
        let captures = function
            .captures
            .iter()
            .map(|symbol| LoweredCapture {
                symbol: *symbol,
                borrowed: module.is_borrowed_capture(function.id, *symbol),
                non_owning: module.is_non_owning_symbol(*symbol),
                requires_cell: capture_requires_cell(module, *symbol),
            })
            .collect();
        let value = LoweredFunction {
            origin: origin.clone(),
            semantic_id: function.id,
            name: function.name.clone(),
            module: module_id,
            binding_symbol,
            signature,
            bounds: module.bounds_of_function(function.id).to_vec(),
            parameter_style: function.parameter_style,
            parameter_pattern,
            parameters: pattern_symbols(resolved, &function.pattern),
            body_coercion: None,
            body_coercion_plan: None,
            body_moved_symbols: Vec::new(),
            captures,
            body_origin: origin.clone(),
            body_syntax: body_syntax.id,
            body: None,
            class,
        };
        if let Err(diagnostic) = self
            .functions
            .insert("function", function.id, origin, value)
        {
            diagnostics.push(diagnostic);
        }
    }

    fn snapshot_modules(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let resolved = module.resolved();
        let program = resolved.program();
        let sources = program.modules();
        let executable_entry = program.executable_entry();
        let mut diagnostics =
            validate_initialization_order(sources, program.initialization_order());
        let mut seen = vec![false; sources.len()];
        for (index, module_id) in program.initialization_order().iter().copied().enumerate() {
            if module_id.0 >= sources.len() {
                continue;
            }
            let source = &sources[module_id.0];
            let origin = module_origin(source);
            if std::mem::replace(&mut seen[module_id.0], true) {
                continue;
            }
            let is_entry = executable_entry == Some(module_id);
            let resources = if is_entry {
                entry_resources(module)
            } else {
                Vec::new()
            };
            let provider_base = self.active_resource_providers.len();
            if is_entry {
                self.seed_entry_providers(
                    ExpressionOwner::Module(module_id),
                    origin.clone(),
                    &resources,
                );
            }
            let items = self.lower_items(
                module,
                ExpressionOwner::Module(module_id),
                &source.syntax.items,
                &mut diagnostics,
            );
            self.active_resource_providers.truncate(provider_base);
            let body = self.blocks.push(LoweredBlock {
                origin: origin.clone(),
                items,
                result: None,
            });
            let initializer = self.initializers.push(LoweredInitializer {
                name: String::new(),
                origin: origin.clone(),
                module: module_id,
                resources,
                body,
                #[cfg(test)]
                executable_entry: is_entry,
            });
            let info = LoweredModuleInfo {
                origin: origin.clone(),
                semantic_id: module_id,
                symbol_prefix: program.mangled_module_prefix(module_id),
                parent: source.parent,
                initialization_index: index,
                initializer,
                #[cfg(test)]
                qualified_name: source.qualified_name.clone(),
                #[cfg(test)]
                companion: source.companion,
                #[cfg(test)]
                executable_entry: is_entry,
            };
            if let Err(diagnostic) = self.modules.insert("module", module_id, origin, info) {
                diagnostics.push(diagnostic);
            }
        }
        diagnostics
    }

    /// Lowers one runtime block's item sequence, preserving source order. The
    /// trailing expression becomes the block result rather than an item, so a
    /// block's value is never also a statement. The block occurrence is
    /// memoized so loop bodies and function bodies reached again later reuse
    /// the same arena node.
    fn lower_block(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        block: &staple_syntax::BlockExpression,
    ) -> Result<BlockId, Diagnostic> {
        let key = ExpressionKey {
            syntax: block.syntax.id,
            owner,
            context,
        };
        if let Some(existing) = self.block_lookup.get(&key) {
            return Ok(*existing);
        }
        let origin = Origin {
            syntax: block.syntax.id,
            span: block.syntax.span.clone(),
        };
        let mut items = Vec::new();
        let mut result = None;
        let last = block.items.len().checked_sub(1);
        for (index, item) in block.items.iter().enumerate() {
            if Some(index) == last
                && let Item::Expression(expression) = item
            {
                result = Some(self.lower_expression(module, owner, context, expression)?);
                continue;
            }
            if let Some(item) = self.lower_item(module, owner, context, item)? {
                items.push(item);
            }
        }
        let id = self.blocks.push(LoweredBlock {
            origin,
            items,
            result,
        });
        self.block_lookup.insert(key, id);
        Ok(id)
    }

    /// Lowers a function template's body into exactly one block. A non-block
    /// body expression becomes the result of a synthetic single-result block,
    /// matching how code generation returns the body expression directly.
    fn lower_function_body(
        &mut self,
        module: &TypedModule,
        function: &ResolvedFunction,
    ) -> Result<
        (
            BlockId,
            Option<CheckedCoercion>,
            Option<LoweredCoercionPlan>,
            Vec<SymbolId>,
        ),
        Diagnostic,
    > {
        let owner = ExpressionOwner::Function(function.id);
        let provider_base = self.active_resource_providers.len();
        self.seed_function_providers(module, function);
        let body = self.lower_expression(module, owner, ExpressionContext::Primary, &function.body);
        self.active_resource_providers.truncate(provider_base);
        let body = body?;
        // When the body lowers to a block expression, its header coercion and
        // moved symbols would be lost by unwrapping it to its root block; they
        // ride back to the caller with the root. A non-block body keeps its
        // header on the wrapped result expression, where emission applies it.
        if let Some(LoweredExpressionKind::Block(block)) = self
            .expressions
            .get(body)
            .map(|expression| &expression.kind)
        {
            let expression = self.expressions.get(body).ok_or_else(|| {
                internal_invariant(
                    staple_syntax::Span::Compiler,
                    "body ID refers to an allocated expression",
                )
            })?;
            return Ok((
                *block,
                expression.coercion.clone(),
                expression.coercion_plan.clone(),
                expression.moved_symbols.clone(),
            ));
        }
        let syntax = function.body.syntax();
        let root = self.blocks.push(LoweredBlock {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            items: Vec::new(),
            result: Some(body),
        });
        Ok((root, None, None, Vec::new()))
    }

    /// Lowers every runtime item of a module initializer in source order.
    /// Declaration-only and other compile-time-only source items are omitted,
    /// matching how resolution and code generation treat them.
    fn lower_items(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        items: &[Item],
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Vec<ItemId> {
        let mut lowered = Vec::new();
        for item in items {
            match self.lower_item(module, owner, ExpressionContext::Primary, item) {
                Ok(Some(item)) => lowered.push(item),
                Ok(None) => {}
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        lowered
    }

    /// Lowers one runtime item. Returns `None` for compile-time-only source
    /// items that resolution and code generation omit. Unexpanded macro,
    /// splice, and operator nodes are lowering diagnostics instead of being
    /// silently dropped.
    fn lower_item(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        item: &Item,
    ) -> Result<Option<ItemId>, Diagnostic> {
        let syntax = item.syntax();
        let origin = Origin {
            syntax: syntax.id,
            span: syntax.span.clone(),
        };
        let kind = match item {
            Item::Binding(binding) => {
                LoweredItemKind::Binding(self.lower_binding_item(module, owner, context, binding)?)
            }
            Item::PatternBinding(binding) => LoweredItemKind::PatternBinding(
                self.lower_pattern_binding_item(module, owner, context, binding)?,
            ),
            Item::Assignment(assignment) => LoweredItemKind::Assignment(
                self.lower_assignment_item(module, owner, context, assignment)?,
            ),
            Item::Return(item) => LoweredItemKind::Return(LoweredReturnItem {
                value: self.lower_expression(module, owner, context, &item.value)?,
            }),
            Item::Break(item) => LoweredItemKind::Break(LoweredBreakItem {
                value: match &item.value {
                    Some(value) => Some(self.lower_expression(module, owner, context, value)?),
                    None => None,
                },
                loop_depth: self.loop_depth,
            }),
            Item::Continue(_) => LoweredItemKind::Continue(LoweredContinueItem {
                loop_depth: self.loop_depth,
            }),
            Item::Expression(expression) => {
                let expression = self.lower_expression(module, owner, context, expression)?;
                let drop_result = self.expression_needs_drop(module, expression);
                LoweredItemKind::Expression(LoweredExpressionStatementItem {
                    expression,
                    drop_result,
                })
            }
            Item::VisibilitySplice(splice) => {
                return Err(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "unexpanded visibility splice reached lowering",
                ));
            }
            Item::RepeatedItemSplice(splice) => {
                return Err(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "unexpanded repeated item splice reached lowering",
                ));
            }
            // A surviving visibility-macro invocation is the marker left by a
            // successfully expanded item-producing macro; its generated items
            // are lowered separately. Resolution and code generation both
            // treat it as a compile-time-only no-op.
            Item::VisibilityMacroInvocation(_)
            | Item::Modified(_)
            | Item::UseDeclaration(_)
            | Item::Submodule(_)
            | Item::ExternBlock(_)
            | Item::TypeDeclaration(_)
            | Item::MacroDeclaration(_)
            | Item::TraitDeclaration(_)
            | Item::TraitImplementation(_) => return Ok(None),
        };
        Ok(Some(self.items.push(LoweredItem { origin, kind })))
    }

    /// The binding-cell rule: a mutable (or initialization-checked) symbol
    /// without module storage gets a binding cell; a mutated parameter arrives
    /// as a caller-provided pointer instead.
    fn symbol_requires_cell(&self, module: &TypedModule, symbol: SymbolId) -> bool {
        if module.is_mutated_parameter(symbol) {
            return false;
        }
        let has_global = self
            .symbols
            .get(symbol)
            .is_some_and(|symbol| symbol.has_global);
        !has_global && capture_requires_cell(module, symbol)
    }

    fn lower_binding_item(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        binding: &staple_syntax::Binding,
    ) -> Result<LoweredBindingItem, Diagnostic> {
        let resolved = module.resolved();
        let Some(symbol) = module.symbol_for(binding.syntax.id) else {
            return Err(Diagnostic::new(
                binding.syntax.span.clone(),
                format!(
                    "cannot lower binding `{}` without a resolved symbol",
                    binding.name
                ),
            ));
        };
        let value = match &binding.value {
            Some(value) => Some(self.lower_expression(module, owner, context, value)?),
            None => None,
        };
        let reactive = self.lower_binding_reactive_operation(module, binding, symbol)?;
        Ok(LoweredBindingItem {
            predeclare_state_only: (binding.kind == staple_syntax::BindingKind::Def
                && resolved.requires_initialization_state(symbol))
            .then(|| {
                module
                    .type_of_symbol(symbol)
                    .is_some_and(contains_type_parameter)
            }),
            initialization_state_only: module
                .type_of_symbol(symbol)
                .is_some_and(contains_type_parameter),
            symbol: Some(symbol),
            value,
            compile_time_only: compile_time_only_symbol(module, symbol),
            generic: !binding.type_parameters.is_empty(),
            derived: module.is_derived_symbol(symbol),
            signal: resolved.is_signal_symbol(symbol),
            reactive,
            cell: self.symbol_requires_cell(module, symbol),
            requires_initialization_check: resolved.requires_initialization_state(symbol),
        })
    }

    fn lower_pattern_binding_item(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        binding: &staple_syntax::PatternBinding,
    ) -> Result<LoweredPatternBindingItem, Diagnostic> {
        let value = self.lower_expression(module, owner, context, &binding.value)?;
        let value_type = self
            .expressions
            .get(value)
            .map(|value| value.value_type.clone())
            .unwrap_or(CheckedType::Never);
        let pattern = self.lower_pattern(module, &binding.pattern, &value_type)?;
        let propagating = binding.kind == staple_syntax::PatternBindingKind::Propagating;
        let propagation = module.propagation_for(binding.syntax.id).cloned();
        if propagating && propagation.is_none() {
            return Err(Diagnostic::new(
                binding.syntax.span.clone(),
                "cannot lower a propagating binding without checked propagation metadata",
            ));
        }
        // The failure path's sum coercion plan.
        let propagation_plan = propagation.as_ref().and_then(|propagation| {
            (propagation.source != propagation.result
                && matches!(propagation.result, CheckedType::Sum(_)))
            .then(|| LoweredCoercionPlan::plan(&propagation.source, &propagation.result).ok())
            .flatten()
        });
        let propagation_residual = propagation
            .as_ref()
            .and_then(LoweredPatternBindingItem::residual_alternative);
        Ok(LoweredPatternBindingItem {
            pattern,
            value,
            propagating,
            propagation,
            propagation_plan,
            propagation_residual,
        })
    }

    fn lower_assignment_item(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        assignment: &staple_syntax::Assignment,
    ) -> Result<LoweredAssignmentItem, Diagnostic> {
        if let Expression::Index(index) = &assignment.target {
            let target = self.lower_indexed_place(module, owner, context, index)?;
            let value = self.lower_expression(module, owner, context, &assignment.value)?;
            let mutate_index = module.trait_dispatch_for(assignment.syntax.id).cloned();
            let Some(dispatch) = &mutate_index else {
                return Err(Diagnostic::new(
                    assignment.syntax.span.clone(),
                    "cannot lower an indexed assignment without a checked MutateIndex dispatch",
                ));
            };
            let origin = Origin {
                syntax: assignment.syntax.id,
                span: assignment.syntax.span.clone(),
            };
            let evidence = Some(self.evidence_for_dispatch(module, owner, &origin, dispatch)?);
            let drops_base_temporary = self
                .indexed_base_temporary_type(target)
                .is_some_and(|base_type| module.type_needs_drop(&base_type));
            return Ok(LoweredAssignmentItem {
                target,
                value,
                mutate_index,
                evidence,
                initialization_symbol: None,
                drop_previous: false,
                drops_base_temporary,
                signal_notify: None,
            });
        }
        let target = self.lower_place(module, owner, context, &assignment.target)?;
        let value = self.lower_expression(module, owner, context, &assignment.value)?;
        let target_type = self
            .places
            .get(target)
            .map(|place| place.value_type.clone())
            .unwrap_or(CheckedType::Error);
        let initialization_symbol = self.place_root_symbol(target);
        let signal_notify = initialization_symbol
            .filter(|symbol| module.resolved().is_signal_symbol(*symbol))
            .map(|symbol| {
                self.push_reactive_operation(
                    Origin {
                        syntax: assignment.syntax.id,
                        span: assignment.syntax.span.clone(),
                    },
                    LoweredReactiveOperationKind::SignalNotify { symbol },
                )
            });
        Ok(LoweredAssignmentItem {
            target,
            value,
            mutate_index: None,
            evidence: None,
            initialization_symbol,
            drop_previous: module.type_needs_drop(&target_type),
            drops_base_temporary: false,
            signal_notify,
        })
    }

    /// The value type of an indexed assignment target's base when the base is
    /// a materialized temporary (no source place), else `None`.
    pub(crate) fn indexed_base_temporary_type(&self, target: PlaceId) -> Option<CheckedType> {
        let LoweredPlaceKind::Indexed { base, .. } = &self.places.get(target)?.kind else {
            return None;
        };
        let base = self.places.get(*base)?;
        matches!(base.kind, LoweredPlaceKind::Temporary { .. }).then(|| base.value_type.clone())
    }

    /// Builds the evidence recipe for a checked dispatch whose owning trait is
    /// looked up from the dispatch method.
    fn evidence_for_dispatch(
        &self,
        module: &TypedModule,
        owner: ExpressionOwner,
        origin: &Origin,
        dispatch: &CheckedTraitDispatch,
    ) -> Result<TraitEvidence, Diagnostic> {
        let Some(trait_id) = module.resolved().trait_for_method(dispatch.method) else {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!("trait method {} has no owning trait", dispatch.method.0),
            ));
        };
        self.trait_evidence_for(
            module,
            owner,
            origin,
            trait_id,
            dispatch.method,
            &dispatch.arguments,
        )
    }

    /// The scope-exit obligation a provider of `value_type` carries: a
    /// `Reactive` provider disposes its scope, a `Tasks` provider closes its
    /// queued children, and every other provider is ordinary.
    fn scope_exit_for(module: &TypedModule, value_type: &CheckedType) -> LoweredScopeExit {
        if module.is_reactive_type(value_type) {
            LoweredScopeExit::Reactive
        } else if module.is_tasks_type(value_type) {
            LoweredScopeExit::Tasks
        } else {
            LoweredScopeExit::Ordinary
        }
    }

    /// The nearest active provider with the same checked value type, in
    /// reverse lexical order — the exact rule code generation uses.
    fn active_provider_for(&self, value_type: &CheckedType) -> Option<LoweredResourceProviderId> {
        self.active_resource_providers
            .iter()
            .rev()
            .copied()
            .find(|provider| {
                self.resource_providers
                    .get(*provider)
                    .is_some_and(|provider| provider.resource.value_type == *value_type)
            })
    }

    /// Pushes one provider record for a function effect row or executable
    /// entry, returning its stable identity.
    fn push_provider(
        &mut self,
        origin: Origin,
        resource: CheckedResource,
        kind: LoweredProviderOriginKind,
        target: LoweredProviderTarget,
        owner: ExpressionOwner,
        indirect: bool,
        borrow: bool,
        storage: LoweredProviderStorage,
        scope_exit: LoweredScopeExit,
    ) -> LoweredResourceProviderId {
        let parent = self.active_resource_providers.last().copied();
        let provider = self.resource_providers.push(LoweredResourceProvider {
            origin,
            resource,
            kind,
            target,
            parent,
            owner,
            indirect,
            borrow,
            storage,
            scope_exit,
        });
        self.active_resource_providers.push(provider);
        provider
    }

    /// Seeds the provider stack with a function's checked effect-row resources
    /// at their row positions. Effect resources are implicit ABI parameters
    /// and own no source symbol.
    fn seed_function_providers(&mut self, module: &TypedModule, function: &ResolvedFunction) {
        let Some(signature) = module.type_of_function(function.id).cloned() else {
            return;
        };
        for (position, resource) in signature.effects.resources.iter().cloned().enumerate() {
            let indirect = resource.mutable
                || !module.is_copy_in_function(&resource.value_type, Some(function.id));
            let origin = Origin {
                syntax: function.body.syntax().id,
                span: function.body.syntax().span.clone(),
            };
            self.push_provider(
                origin,
                resource,
                LoweredProviderOriginKind::FunctionParameter,
                LoweredProviderTarget::EffectParameter { position },
                ExpressionOwner::Function(function.id),
                indirect,
                indirect,
                LoweredProviderStorage::Materialized,
                // Effect parameters borrow the caller's scope; only a `with`
                // provider or an entry-created scope owns its cleanup.
                LoweredScopeExit::Ordinary,
            );
        }
    }

    /// Seeds the provider stack with the executable entry's IO and reactive
    /// resources, in the order the entry installs them.
    fn seed_entry_providers(
        &mut self,
        owner: ExpressionOwner,
        origin: Origin,
        resources: &[LoweredEntryResource],
    ) {
        for entry in resources {
            let (indirect, scope_exit) = match entry.kind {
                LoweredEntryResourceKind::Io => (true, LoweredScopeExit::Ordinary),
                LoweredEntryResourceKind::Reactive => (false, LoweredScopeExit::Reactive),
            };
            self.push_provider(
                origin.clone(),
                entry.resource.clone(),
                LoweredProviderOriginKind::EntryParameter,
                LoweredProviderTarget::Entry,
                owner,
                indirect,
                false,
                LoweredProviderStorage::Materialized,
                scope_exit,
            );
        }
    }

    /// Binds an ambient resource occurrence to its nearest active provider.
    /// A missing provider produces a source diagnostic.
    fn lower_resource_use(
        &mut self,
        module: &TypedModule,
        syntax: SyntaxId,
        span: Span,
        kind: LoweredResourceUseKind,
    ) -> Result<LoweredResourceUseId, Diagnostic> {
        let Some(resource) = module.resource_for_expression(syntax).cloned() else {
            return Err(Diagnostic::new(
                span,
                "cannot lower a resource occurrence without checked resource metadata",
            ));
        };
        self.bind_resource_requirement(module, syntax, span, kind, resource, None)
    }

    /// Binds one checked resource requirement to its selected provider and
    /// records how the value passes. `hidden_mutable` is the requirement's
    /// checked mutability for a hidden call/effect argument; `None` derives it
    /// from the requirement itself for reads and places.
    fn bind_resource_requirement(
        &mut self,
        module: &TypedModule,
        syntax: SyntaxId,
        span: Span,
        kind: LoweredResourceUseKind,
        resource: CheckedResource,
        function: Option<FunctionId>,
    ) -> Result<LoweredResourceUseId, Diagnostic> {
        let Some(provider) = self.active_provider_for(&resource.value_type) else {
            return Err(Diagnostic::new(
                span,
                format!("resource `{}` is not available", resource.value_type),
            ));
        };
        let indirect = self
            .resource_providers
            .get(provider)
            .is_some_and(|provider| provider.indirect);
        let pass_mode = match kind {
            LoweredResourceUseKind::MutablePlace => {
                if !indirect {
                    return Err(Diagnostic::new(
                        span,
                        format!("resource `{}` is not mutable", resource.value_type),
                    ));
                }
                LoweredArgumentPassMode::MutablePlace
            }
            LoweredResourceUseKind::HiddenArgument => {
                let borrow =
                    resource.mutable || !module.is_copy_in_function(&resource.value_type, function);
                if borrow {
                    if !indirect {
                        return Err(Diagnostic::new(
                            span,
                            format!("resource `{}` is not borrowable", resource.value_type),
                        ));
                    }
                    LoweredArgumentPassMode::BorrowedPointer
                } else {
                    LoweredArgumentPassMode::Value
                }
            }
            LoweredResourceUseKind::Read => LoweredArgumentPassMode::Value,
        };
        Ok(self.resource_uses.push(LoweredResourceUse {
            origin: Origin { syntax, span },
            resource,
            provider: Some(provider),
            kind,
            pass_mode,
            indirect,
        }))
    }

    /// Pushes one reactive operation record.
    fn push_reactive_operation(
        &mut self,
        origin: Origin,
        kind: LoweredReactiveOperationKind,
    ) -> LoweredReactiveOperationId {
        self.reactive_operations
            .push(LoweredReactiveOperation { origin, kind })
    }

    /// Where a signal's storage is created: module symbols live in globals,
    /// everything else in a binding cell.
    fn signal_storage(&self, module: &TypedModule, symbol: SymbolId) -> LoweredSignalStorage {
        if module.resolved().is_module_symbol(symbol) {
            LoweredSignalStorage::Global
        } else {
            LoweredSignalStorage::LocalCell
        }
    }

    /// Records the signal creation or derived creation performed by a binding.
    /// Generic bindings only record initialization state in the backend, so
    /// they create no reactive storage.
    fn lower_binding_reactive_operation(
        &mut self,
        module: &TypedModule,
        binding: &staple_syntax::Binding,
        symbol: SymbolId,
    ) -> Result<Option<LoweredReactiveOperationId>, Diagnostic> {
        if !binding.type_parameters.is_empty() {
            return Ok(None);
        }
        let origin = Origin {
            syntax: binding.syntax.id,
            span: binding.syntax.span.clone(),
        };
        if module.is_derived_symbol(symbol) {
            let Some(evaluator) = module.derived_evaluator(symbol) else {
                return Err(Diagnostic::new(
                    origin.span,
                    "derived evaluator is unavailable",
                ));
            };
            let evaluator = evaluator.id;
            let Some(function_type) = module.type_of_function(evaluator).cloned() else {
                return Err(Diagnostic::new(
                    origin.span,
                    "derived evaluator has no function type",
                ));
            };
            if !function_type.effects.resources.is_empty() {
                return Err(Diagnostic::new(
                    origin.span,
                    "derived evaluators cannot capture resources",
                ));
            }
            let captures = self
                .functions
                .get(evaluator)
                .map(|function| function.captures.clone())
                .unwrap_or_default();
            return Ok(Some(self.push_reactive_operation(
                origin,
                LoweredReactiveOperationKind::DerivedCreate {
                    symbol,
                    evaluator,
                    function_type,
                    captures,
                },
            )));
        }
        if module.resolved().is_signal_symbol(symbol) {
            let storage = self.signal_storage(module, symbol);
            return Ok(Some(self.push_reactive_operation(
                origin,
                LoweredReactiveOperationKind::SignalCreate { symbol, storage },
            )));
        }
        Ok(None)
    }

    /// The tracked read a name occurrence performs on reactive storage.
    fn lower_name_reactive_operation(
        &mut self,
        module: &TypedModule,
        origin: Origin,
        symbol: SymbolId,
    ) -> Option<LoweredReactiveOperationId> {
        if module.resolved().is_signal_symbol(symbol) {
            Some(self.push_reactive_operation(
                origin,
                LoweredReactiveOperationKind::SignalRead { symbol },
            ))
        } else if module.is_derived_symbol(symbol) {
            Some(self.push_reactive_operation(
                origin,
                LoweredReactiveOperationKind::DerivedRead { symbol },
            ))
        } else {
            None
        }
    }

    /// The nearest active `Reactive` provider, matching the backend's ambient
    /// scope search.
    fn active_reactive_provider(&self, module: &TypedModule) -> Option<LoweredResourceProviderId> {
        self.active_resource_providers
            .iter()
            .rev()
            .copied()
            .find(|provider| {
                self.resource_providers
                    .get(*provider)
                    .is_some_and(|provider| module.is_reactive_type(&provider.resource.value_type))
            })
    }

    /// Captures already lowered on a callable-value occurrence. An explicit
    /// callback can carry a stored closure or a fresh environment; the capture
    /// catalog is copied here so the reactive record owns it.
    fn callable_value_captures(&self, expression: ExpressionId) -> Vec<LoweredCapture> {
        self.expressions
            .get(expression)
            .and_then(|expression| match &expression.kind {
                LoweredExpressionKind::CallableValue(value) => self.callable_values.get(*value),
                _ => None,
            })
            .and_then(|value| value.closure.as_ref())
            .map(|closure| {
                closure
                    .captures
                    .iter()
                    .map(|capture| capture.capture.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Builds the callback record for a reactive intrinsic: the implicit thunk
    /// that owns a block callback or the explicit callable occurrence, its
    /// checked function type, ordered captures, and ordered hidden resource
    /// requirements.
    fn lower_reactive_callback(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        call: &staple_syntax::CallExpression,
    ) -> Result<LoweredReactiveCallbackId, Diagnostic> {
        let argument = &call.argument;
        let span = argument.syntax().span.clone();
        let thunk = module
            .implicit_thunk_for(argument.syntax().id)
            .map(|f| f.id);
        let (callable, function_type) = match thunk {
            Some(thunk) => {
                let Some(function_type) = module.type_of_function(thunk).cloned() else {
                    return Err(Diagnostic::new(span, "callback thunk has no function type"));
                };
                (None, function_type)
            }
            None => {
                let callable =
                    self.lower_expression(module, owner, ExpressionContext::Primary, argument)?;
                let function_type = match module.type_of_expression(argument.syntax().id) {
                    Some(CheckedType::Function(function_type)) => function_type.clone(),
                    _ => {
                        return Err(Diagnostic::new(span, "callback has no function type"));
                    }
                };
                (Some(callable), function_type)
            }
        };
        let captures = match (thunk, callable) {
            (Some(thunk), _) => self
                .functions
                .get(thunk)
                .map(|function| function.captures.clone())
                .unwrap_or_default(),
            (None, Some(callable)) => self.callable_value_captures(callable),
            (None, None) => Vec::new(),
        };
        let mut resources = Vec::with_capacity(function_type.effects.resources.len());
        for resource in &function_type.effects.resources {
            resources.push(self.bind_resource_requirement(
                module,
                argument.syntax().id,
                argument.syntax().span.clone(),
                LoweredResourceUseKind::HiddenArgument,
                resource.clone(),
                owner_function(owner),
            )?);
        }
        Ok(self.reactive_callbacks.push(LoweredReactiveCallback {
            origin: Origin {
                syntax: argument.syntax().id,
                span: argument.syntax().span.clone(),
            },
            thunk,
            callable,
            function_type,
            captures,
            resources,
        }))
    }

    /// The reactive operation an intrinsic call performs, when it is one of
    /// the five reactive intrinsics.
    fn lower_reactive_intrinsic_operation(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        origin: &Origin,
        call: &staple_syntax::CallExpression,
        intrinsic: crate::IntrinsicFunction,
    ) -> Result<Option<LoweredReactiveOperationId>, Diagnostic> {
        let Some(IntrinsicRoute::Reactive(route)) = intrinsic_route(intrinsic) else {
            return Ok(None);
        };
        let kind = match route {
            ReactiveIntrinsicRoute::Scope => LoweredReactiveOperationKind::Scope,
            ReactiveIntrinsicRoute::Snapshot => LoweredReactiveOperationKind::Snapshot,
            ReactiveIntrinsicRoute::Reaction => {
                let callback = self.lower_reactive_callback(module, owner, call)?;
                LoweredReactiveOperationKind::Reaction {
                    callback,
                    reactive_provider: self.active_reactive_provider(module),
                }
            }
            ReactiveIntrinsicRoute::Batch => {
                let callback = self.lower_reactive_callback(module, owner, call)?;
                LoweredReactiveOperationKind::Batch { callback }
            }
            ReactiveIntrinsicRoute::Until => {
                let predicate = self.lower_reactive_callback(module, owner, call)?;
                let predicate_record = self.reactive_callbacks.get(predicate).ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "the predicate callback was just recorded",
                    )
                })?;
                // Pure apart from reading signals.
                if !predicate_record.function_type.effects.resources.is_empty()
                    || matches!(
                        predicate_record.function_type.effects.state,
                        Some(
                            crate::CheckedStateEffect::Write | crate::CheckedStateEffect::ReadWrite
                        )
                    )
                {
                    return Err(Diagnostic::new(
                        call.argument.syntax().span.clone(),
                        "an `until` predicate must be pure apart from reading signals",
                    ));
                }
                LoweredReactiveOperationKind::Until {
                    predicate,
                    reactive_provider: self.active_reactive_provider(module),
                }
            }
        };
        Ok(Some(self.push_reactive_operation(origin.clone(), kind)))
    }

    /// Resolves a call's hidden effect-row requirements in checked order,
    /// after its visible arguments have already been lowered. Every binding
    /// references a provider visible at the call site; a missing provider is a
    /// source diagnostic.
    fn lower_call_resource_bindings(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        origin: &Origin,
        effects: &CheckedEffectSet,
    ) -> Result<Vec<LoweredResourceUseId>, Diagnostic> {
        let mut bindings = Vec::with_capacity(effects.resources.len());
        for resource in &effects.resources {
            let binding = self.bind_resource_requirement(
                module,
                origin.syntax,
                origin.span.clone(),
                LoweredResourceUseKind::HiddenArgument,
                resource.clone(),
                owner_function(owner),
            )?;
            bindings.push(binding);
        }
        Ok(bindings)
    }

    /// Lowers a `with`: the provider value evaluates once, the provider is
    /// active only while the body lowers, and the scope-exit obligation is
    /// recorded for both normal and early exits.
    fn lower_with(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        with: &staple_syntax::WithResourceExpression,
    ) -> Result<LoweredWithId, Diagnostic> {
        let origin = Origin {
            syntax: with.syntax.id,
            span: with.syntax.span.clone(),
        };
        let Some(resource) = module.resource_for_expression(with.syntax.id).cloned() else {
            return Err(Diagnostic::new(
                with.syntax.span.clone(),
                "cannot lower a `with` without checked resource metadata",
            ));
        };
        let value = self.lower_expression(module, owner, context, &with.value)?;
        let copy = module.is_copy_in_function(&resource.value_type, owner_function(owner));
        let borrow = with.mutable || !copy;
        let storage = if borrow && self.provider_value_has_place(module, &with.value) {
            LoweredProviderStorage::Place
        } else {
            LoweredProviderStorage::Materialized
        };
        // Record the reused source place once, so emission never re-derives the
        // place decision.
        let place = match storage {
            LoweredProviderStorage::Place => {
                Some(self.lower_place(module, owner, context, &with.value)?)
            }
            LoweredProviderStorage::Materialized => None,
        };
        let scope_exit = Self::scope_exit_for(module, &resource.value_type);
        let provider = self.push_provider(
            origin.clone(),
            resource,
            LoweredProviderOriginKind::Source,
            LoweredProviderTarget::Expression(value),
            owner,
            true,
            borrow,
            storage,
            scope_exit,
        );
        let body = self.lower_block(module, owner, context, &with.body);
        self.active_resource_providers.pop();
        let body = body?;
        Ok(self.withs.push(LoweredWith {
            origin,
            provider,
            value,
            place,
            body,
            scope_exit,
        }))
    }

    /// Whether code generation can reuse an existing pointer for a `with`
    /// value. This is emission's place-pointer rule, including resource places
    /// and transparent single-value wrappers, rather than the narrower
    /// symbol-root test used for call argument mutation.
    fn provider_value_has_place(&self, module: &TypedModule, value: &Expression) -> bool {
        if module.resolved().symbol_for(value.syntax().id).is_some() {
            return true;
        }
        match value {
            Expression::Product(product) if product.elements.len() == 1 => {
                self.provider_value_has_place(module, &product.elements[0].value)
            }
            Expression::Satisfies(satisfies) => {
                self.provider_value_has_place(module, &satisfies.value)
            }
            Expression::Resource(resource) => module
                .resource_for_expression(resource.syntax.id)
                .and_then(|required| self.active_provider_for(&required.value_type))
                .and_then(|provider| self.resource_providers.get(provider))
                .is_some_and(|provider| provider.indirect),
            Expression::Access(access) => match module.access_for(access.syntax.id) {
                Some(crate::CheckedAccess::Representation { dereference }) => {
                    !dereference.is_empty() || self.provider_value_has_place(module, &access.value)
                }
                Some(crate::CheckedAccess::Product {
                    dereference, slice, ..
                }) => {
                    !dereference.is_empty()
                        || *slice
                        || self.provider_value_has_place(module, &access.value)
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The root symbol of an assignment target's place, whose initialization
    /// state the assignment writes back: a direct symbol or captured cell, or a
    /// representation/product projection of one. Slice and dereference places
    /// have none.
    ///
    /// A field write resolves to its base's root symbol for notification, so a
    /// reaction over a signal product field re-runs. The emitter never writes
    /// the base's initialization state for a field projection; a projection
    /// only executes on an already-initialized base (its own check traps
    /// otherwise).
    fn place_root_symbol(&self, place: PlaceId) -> Option<SymbolId> {
        match &self.places.get(place)?.kind {
            LoweredPlaceKind::Symbol { symbol } | LoweredPlaceKind::CapturedCell { symbol } => {
                Some(*symbol)
            }
            LoweredPlaceKind::Representation { base }
            | LoweredPlaceKind::ProductElement { base, .. } => self.place_root_symbol(*base),
            LoweredPlaceKind::Temporary { .. }
            | LoweredPlaceKind::Resource { .. }
            | LoweredPlaceKind::Dereference { .. }
            | LoweredPlaceKind::Indexed { .. } => None,
        }
    }

    fn lower_indexed_place(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        index: &staple_syntax::IndexExpression,
    ) -> Result<PlaceId, Diagnostic> {
        let syntax = &index.syntax;
        let Some(value_type) = module.type_of_expression(syntax.id).cloned() else {
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "cannot lower an indexed place without a checked element type",
            ));
        };
        let base = if expression_has_place_root(module.resolved(), &index.value) {
            self.lower_place(module, owner, context, &index.value)?
        } else {
            let value_syntax = index.value.syntax();
            let Some(base_type) = module.type_of_expression(value_syntax.id).cloned() else {
                return Err(Diagnostic::new(
                    value_syntax.span.clone(),
                    "cannot lower an indexed base without a checked type",
                ));
            };
            let expression = self.lower_expression(module, owner, context, &index.value)?;
            self.places.push(LoweredPlace {
                origin: Origin {
                    syntax: value_syntax.id,
                    span: value_syntax.span.clone(),
                },
                value_type: base_type,
                kind: LoweredPlaceKind::Temporary { expression },
            })
        };
        let position = self.lower_expression(module, owner, context, &index.index)?;
        Ok(self.places.push(LoweredPlace {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type,
            kind: LoweredPlaceKind::Indexed {
                base,
                index: position,
            },
        }))
    }

    /// Lowers an assignment target into an explicit place tree.
    fn lower_place(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        expression: &Expression,
    ) -> Result<PlaceId, Diagnostic> {
        let syntax = expression.syntax();
        let origin = Origin {
            syntax: syntax.id,
            span: syntax.span.clone(),
        };
        let Some(value_type) = module.type_of_expression(syntax.id).cloned() else {
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "cannot lower a place without a checked type",
            ));
        };
        if let Some(symbol) = module.symbol_for(syntax.id) {
            let kind = if self.symbol_requires_cell(module, symbol) {
                LoweredPlaceKind::CapturedCell { symbol }
            } else {
                LoweredPlaceKind::Symbol { symbol }
            };
            return Ok(self.places.push(LoweredPlace {
                origin,
                value_type,
                kind,
            }));
        }
        let kind = match expression {
            Expression::Product(product) if product.elements.len() == 1 => {
                return self.lower_place(module, owner, context, &product.elements[0].value);
            }
            Expression::Satisfies(satisfies) => {
                return self.lower_place(module, owner, context, &satisfies.value);
            }
            Expression::Resource(resource) => {
                let use_ = self.lower_resource_use(
                    module,
                    resource.syntax.id,
                    resource.syntax.span.clone(),
                    LoweredResourceUseKind::MutablePlace,
                )?;
                LoweredPlaceKind::Resource { use_ }
            }
            Expression::Access(access) => {
                self.lower_access_place(module, owner, context, access)?
            }
            Expression::Index(index) => {
                return self.lower_indexed_place(module, owner, context, index);
            }
            other => {
                return Err(Diagnostic::new(
                    other.syntax().span.clone(),
                    "assignment target is not a lowerable place",
                ));
            }
        };
        Ok(self.places.push(LoweredPlace {
            origin,
            value_type,
            kind,
        }))
    }

    fn lower_access_place(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        access: &staple_syntax::AccessExpression,
    ) -> Result<LoweredPlaceKind, Diagnostic> {
        let Some(checked) = module.access_for(access.syntax.id).cloned() else {
            return Err(Diagnostic::new(
                access.syntax.span.clone(),
                "cannot lower an access place without checked access metadata",
            ));
        };
        match checked {
            CheckedAccess::Representation { dereference } => {
                let base =
                    self.lower_access_base(module, owner, context, &access.value, dereference)?;
                Ok(LoweredPlaceKind::Representation { base })
            }
            CheckedAccess::Product {
                index,
                dereference,
                slice,
                scalar,
            } => {
                let base =
                    self.lower_access_base(module, owner, context, &access.value, dereference)?;
                if scalar {
                    Ok(LoweredPlaceKind::Representation { base })
                } else {
                    Ok(LoweredPlaceKind::ProductElement { base, index, slice })
                }
            }
        }
    }

    /// The base of an access place: a recursive place when no `Ref` payloads
    /// are crossed, or a dereference of the evaluated value expression.
    fn lower_access_base(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        value: &Expression,
        dereference: Vec<CheckedType>,
    ) -> Result<PlaceId, Diagnostic> {
        if dereference.is_empty() {
            return self.lower_place(module, owner, context, value);
        }
        let syntax = value.syntax();
        let Some(value_type) = dereference.last().cloned() else {
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "cannot lower an empty dereference chain",
            ));
        };
        let reference = self.lower_expression(module, owner, context, value)?;
        Ok(self.places.push(LoweredPlace {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type,
            kind: LoweredPlaceKind::Dereference {
                reference,
                dereference,
            },
        }))
    }

    /// Lowers a function template's parameter pattern. Compiler-synthesized
    /// implicit-thunk parameters carry no source pattern type, so their
    /// checked signature parameter and body origin stand in.
    fn lower_function_pattern(
        &mut self,
        module: &TypedModule,
        function: &ResolvedFunction,
        signature: &CheckedFunctionType,
    ) -> Result<PatternId, Diagnostic> {
        if module
            .type_of_pattern(function.pattern.syntax().id)
            .is_some()
        {
            return self.lower_pattern(module, &function.pattern, signature.parameter.as_ref());
        }
        let Pattern::Product(product) = &function.pattern else {
            return Err(Diagnostic::new(
                Span::Compiler,
                "cannot lower a compiler-synthesized function parameter pattern",
            ));
        };
        if !product.elements.is_empty() {
            return Err(Diagnostic::new(
                Span::Compiler,
                "compiler-synthesized function parameter patterns must be empty products",
            ));
        }
        let syntax = function.body.syntax();
        Ok(self.patterns.push(LoweredPattern {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type: signature.parameter.as_ref().clone(),
            kind: LoweredPatternKind::Product {
                elements: Vec::new(),
                mutable: product.mutable,
                moved: product.moved,
            },
            test: LoweredPatternTestPlan::undecided(signature.parameter.as_ref().clone()),
        }))
    }

    /// Lowers a checked pattern recursively, recording bound symbols,
    /// singleton targets, and the emission test plan against the subject
    /// type the use site supplies.
    fn lower_pattern(
        &mut self,
        module: &TypedModule,
        pattern: &Pattern,
        subject: &CheckedType,
    ) -> Result<PatternId, Diagnostic> {
        let syntax = pattern.syntax();
        let Some(value_type) = module.type_of_pattern(syntax.id).cloned() else {
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "cannot lower a pattern without a checked type",
            ));
        };
        let resolved = module.resolved();
        let shape = pattern_plan_shape(resolved, pattern)?;
        let (test, child_subjects) = match pattern_test_plan(
            subject,
            &value_type,
            &shape,
            &|id| resolved.builtin_type(id),
            module.string_representation(),
        ) {
            Ok(result) => result,
            Err(message) => {
                // A template whose subject still contains declared parameters
                // cannot decide the plan; materialization recomputes it once
                // the types are concrete. A concrete subject that the emitter
                // rejects is a lowering diagnostic.
                if instance_resolution::unresolved_type_problem(subject).is_some()
                    || instance_resolution::unresolved_type_problem(&value_type).is_some()
                {
                    (
                        LoweredPatternTestPlan::undecided(subject.clone()),
                        Vec::new(),
                    )
                } else {
                    return Err(Diagnostic::new(syntax.span.clone(), message));
                }
            }
        };
        let child_subject = |index: usize, child: &Pattern| {
            child_subjects
                .get(index)
                .cloned()
                .or_else(|| module.type_of_pattern(child.syntax().id).cloned())
                .unwrap_or(CheckedType::Error)
        };
        let kind = match pattern {
            Pattern::Wildcard(_) => LoweredPatternKind::Wildcard,
            Pattern::Binding(binding) => LoweredPatternKind::Binding {
                initialization_state_only: resolved
                    .symbol_for(binding.syntax.id)
                    .and_then(|symbol| module.type_of_symbol(symbol))
                    .is_some_and(contains_type_parameter),
                name: binding.name.clone(),
                symbol: resolved.symbol_for(binding.syntax.id),
                singleton: resolved.type_for_pattern(binding.syntax.id),
                mutable: binding.mutable,
                moved: binding.moved,
            },
            Pattern::Product(product) => {
                let mut elements = Vec::with_capacity(product.elements.len());
                for (index, element) in product.elements.iter().enumerate() {
                    let element_subject = child_subject(index, element);
                    elements.push(self.lower_pattern(module, element, &element_subject)?);
                }
                LoweredPatternKind::Product {
                    elements,
                    mutable: product.mutable,
                    moved: product.moved,
                }
            }
            Pattern::Nominal(nominal) => LoweredPatternKind::Nominal {
                target: resolved.type_for_pattern(syntax.id),
                name: nominal.name.clone(),
                moved: nominal.moved,
                argument: {
                    let argument_subject = child_subject(0, &nominal.argument);
                    self.lower_pattern(module, &nominal.argument, &argument_subject)?
                },
            },
            Pattern::StringLiteral(literal) => LoweredPatternKind::Literal {
                literal: literal.literal.clone(),
            },
            Pattern::At(at) => {
                let binding_pattern = Pattern::Binding(at.binding.as_ref().clone());
                let binding_subject = child_subject(0, &binding_pattern);
                let binding = self.lower_pattern(module, &binding_pattern, &binding_subject)?;
                let inner_subject = child_subject(1, &at.pattern);
                let pattern = self.lower_pattern(module, &at.pattern, &inner_subject)?;
                LoweredPatternKind::At { binding, pattern }
            }
            Pattern::Splice(splice) => {
                return Err(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "unexpanded pattern splice reached lowering",
                ));
            }
        };
        Ok(self.patterns.push(LoweredPattern {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type,
            kind,
            test,
        }))
    }

    /// Lowers one runtime expression occurrence. The occurrence key memoizes
    /// repeated visits; children are lowered before the node is allocated so a
    /// diagnostic never leaves a partially initialized arena node.
    fn lower_expression(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        expression: &Expression,
    ) -> Result<ExpressionId, Diagnostic> {
        self.lower_expression_occurrence(module, owner, context, expression, None)
    }

    /// Lowers one expression occurrence with an optional root type override.
    /// Contextual product defaults use it because the shared default syntax
    /// node may have been checked last for a different instantiation; the
    /// consumer's checked slot type is authoritative for the root.
    fn lower_expression_occurrence(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        expression: &Expression,
        type_override: Option<CheckedType>,
    ) -> Result<ExpressionId, Diagnostic> {
        let syntax = expression.syntax();
        let key = ExpressionKey {
            syntax: syntax.id,
            owner,
            context,
        };
        if let Some(existing) = self.expression_lookup.get(&key) {
            return Ok(*existing);
        }
        let disposition = classify_expression(module, expression);
        if disposition == ExpressionDisposition::Rejected {
            reject_compile_time_expression(expression)?;
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "compile-time-only expression reached lowering",
            ));
        }
        let value_type = match type_override {
            Some(value_type) => value_type,
            None => match module.type_of_expression(syntax.id).cloned() {
                Some(value_type) => value_type,
                // The checker stops recording a type once control flow
                // diverges (`return`, `break`, `continue`, or a `Never`
                // sub-expression), so an expression with no recorded type is
                // unreachable and its value is `Never`.
                None => CheckedType::Never,
            },
        };
        let effects = module
            .effects_of_expression(syntax.id)
            .cloned()
            .unwrap_or_default();
        let coercion = module.coercion_for(syntax.id).cloned();
        let coercion_plan = match &coercion {
            Some(coercion) => match LoweredCoercionPlan::plan(&coercion.source, &coercion.target) {
                Ok(plan) => Some(plan),
                Err(message) => {
                    if instance_resolution::unresolved_type_problem(&coercion.source).is_some()
                        || instance_resolution::unresolved_type_problem(&coercion.target).is_some()
                    {
                        None
                    } else {
                        return Err(Diagnostic::new(syntax.span.clone(), message));
                    }
                }
            },
            None => None,
        };
        let mut moved_symbols = module.moved_symbols(syntax.id).collect::<Vec<_>>();
        moved_symbols.sort_by_key(|symbol| symbol.0);
        let kind = match disposition {
            ExpressionDisposition::Ordinary(family) => {
                self.lower_ordinary_expression(module, owner, context, family, expression)?
            }
            ExpressionDisposition::ResourceCoroutine(route) => {
                self.lower_resource_expression(module, owner, context, route, expression)?
            }
            ExpressionDisposition::Rejected => {
                return Err(internal_invariant(
                    expression.syntax().span.clone(),
                    "rejected expressions exit before dispatch",
                ));
            }
        };
        let id = self.expressions.push(LoweredExpression {
            key,
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type,
            effects,
            coercion,
            coercion_plan,
            moved_symbols,
            kind,
        });
        self.expression_lookup.insert(key, id);
        Ok(id)
    }

    /// Lowers the children and payload of one ordinary expression
    /// family. Every family has a concrete payload; a family/expression
    /// mismatch is a defensive diagnostic.
    fn lower_ordinary_expression(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        family: OrdinaryExpressionFamily,
        expression: &Expression,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        match (family, expression) {
            (OrdinaryExpressionFamily::Block, Expression::Block(block)) => Ok(
                LoweredExpressionKind::Block(self.lower_block(module, owner, context, block)?),
            ),
            (OrdinaryExpressionFamily::Satisfies, Expression::Satisfies(satisfies)) => {
                let value = self.lower_expression(module, owner, context, &satisfies.value)?;
                Ok(LoweredExpressionKind::Satisfies(LoweredSatisfies { value }))
            }
            (OrdinaryExpressionFamily::Match, Expression::Match(match_)) => self
                .lower_match(module, owner, context, match_)
                .map(LoweredExpressionKind::Match),
            (OrdinaryExpressionFamily::Loop, Expression::Loop(loop_)) => self
                .lower_loop(module, owner, context, loop_)
                .map(LoweredExpressionKind::Loop),
            (OrdinaryExpressionFamily::Product, Expression::Product(product)) => self
                .lower_product(module, owner, context, product)
                .map(LoweredExpressionKind::Product),
            (OrdinaryExpressionFamily::RepeatedProduct, Expression::RepeatedProduct(repeated)) => {
                self.lower_repeated_product(module, owner, context, repeated)
                    .map(LoweredExpressionKind::RepeatedProduct)
            }
            (OrdinaryExpressionFamily::Access, Expression::Access(access)) => {
                self.lower_access(module, owner, context, access)
            }
            (OrdinaryExpressionFamily::Name, Expression::Name(name)) => self.lower_name(
                module,
                owner,
                name.syntax.id,
                name.syntax.span.clone(),
                module.symbol_for(name.syntax.id),
            ),
            (OrdinaryExpressionFamily::Function, Expression::Function(function)) => {
                let value = self.lower_callable_value(
                    module,
                    owner,
                    function.syntax.id,
                    function.syntax.span.clone(),
                    None,
                )?;
                Ok(LoweredExpressionKind::CallableValue(value))
            }
            (OrdinaryExpressionFamily::Call, Expression::Call(call)) => {
                self.lower_call(module, owner, context, call)
            }
            (OrdinaryExpressionFamily::Integer, Expression::Integer(integer)) => self
                .lower_integer(module, integer)
                .map(LoweredExpressionKind::Integer),
            (OrdinaryExpressionFamily::Float, Expression::Float(float)) => self
                .lower_float(module, float)
                .map(LoweredExpressionKind::Float),
            (OrdinaryExpressionFamily::String, Expression::String(string)) => {
                let value = staple_syntax::string_literal::decode(&string.literal)
                    .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
                Ok(LoweredExpressionKind::String(LoweredString { value }))
            }
            (OrdinaryExpressionFamily::CString, Expression::CString(string)) => self
                .lower_c_string(string)
                .map(LoweredExpressionKind::CString),
            (OrdinaryExpressionFamily::Index, Expression::Index(index)) => self
                .lower_index(module, owner, context, index)
                .map(LoweredExpressionKind::Index),
            (OrdinaryExpressionFamily::Logical, Expression::Logical(logical)) => self
                .lower_logical(module, owner, context, logical)
                .map(LoweredExpressionKind::Logical),
            (OrdinaryExpressionFamily::StringTemplate, Expression::StringTemplate(template)) => {
                self.lower_string_template(module, owner, context, template)
                    .map(LoweredExpressionKind::StringTemplate)
            }
            _ => Err(Diagnostic::new(
                expression.syntax().span.clone(),
                format!(
                    "lowered expression family {} does not match its syntax variant",
                    family_name(family)
                ),
            )),
        }
    }

    /// Lowers one lowering-owned family. Resource reads, providers, and places
    /// have concrete payloads, as do coroutine creation and `await`.
    fn lower_resource_expression(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        route: ResourceCoroutineRoute,
        expression: &Expression,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        match (route, expression) {
            (ResourceCoroutineRoute::ResourceUse, Expression::Resource(resource)) => {
                let use_ = self.lower_resource_use(
                    module,
                    resource.syntax.id,
                    resource.syntax.span.clone(),
                    LoweredResourceUseKind::Read,
                )?;
                Ok(LoweredExpressionKind::Resource(use_))
            }
            (ResourceCoroutineRoute::ResourceProvider, Expression::With(with)) => self
                .lower_with(module, owner, context, with)
                .map(LoweredExpressionKind::With),
            (ResourceCoroutineRoute::CoroutineCreation, Expression::Coro(coro)) => {
                self.lower_coro(coro).map(LoweredExpressionKind::Coro)
            }
            (_, Expression::Await(await_)) => self
                .lower_await(module, owner, route, await_)
                .map(LoweredExpressionKind::Await),
            (_, _) => Err(Diagnostic::new(
                expression.syntax().span.clone(),
                format!("lowering route {route:?} does not match its syntax variant"),
            )),
        }
    }

    /// Lowers a `coro { ... }` creation: the body plan plus the capture
    /// environment construction. Frame layout, rooting, and deferred-effect
    /// ownership stay classifications, not target offsets.
    fn lower_coro(
        &mut self,
        coro: &staple_syntax::CoroExpression,
    ) -> Result<LoweredCoroId, Diagnostic> {
        let body_syntax = coro.body.syntax.id;
        let Some(plan) = self.coroutine_plan_lookup.get(&body_syntax).copied() else {
            return Err(Diagnostic::new(
                coro.syntax.span.clone(),
                "cannot lower a coroutine without its body plan",
            ));
        };
        let environment = match self.coroutine_plans.get(plan) {
            Some(plan) if !plan.captures.is_empty() => LoweredClosureEnvironment::Fresh,
            Some(_) => LoweredClosureEnvironment::None,
            None => {
                return Err(Diagnostic::new(
                    coro.syntax.span.clone(),
                    "coroutine body plan is missing from the plan arena",
                ));
            }
        };
        Ok(self.coros.push(LoweredCoro {
            origin: Origin {
                syntax: coro.syntax.id,
                span: coro.syntax.span.clone(),
            },
            plan,
            environment,
        }))
    }

    /// Lowers one `await` site inside its owning coroutine plan. The site
    /// records its one-based resume state, checked operand kind, child
    /// deferred-resource bindings, and outcome/result type.
    fn lower_await(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        route: ResourceCoroutineRoute,
        await_: &staple_syntax::AwaitExpression,
    ) -> Result<LoweredAwaitId, Diagnostic> {
        let span = await_.syntax.span.clone();
        let ExpressionOwner::Function(function) = owner else {
            return Err(Diagnostic::new(span, "`await` outside a coroutine body"));
        };
        let Some(plan_id) = self.coroutine_plan_by_thunk.get(&function).copied() else {
            return Err(Diagnostic::new(span, "`await` outside a coroutine body"));
        };
        let operand =
            self.lower_expression(module, owner, ExpressionContext::Primary, &await_.operand)?;
        let operand_type = module
            .type_of_expression(await_.operand.syntax().id)
            .cloned();
        let kind = match route {
            ResourceCoroutineRoute::AwaitTask => {
                let result = operand_type
                    .as_ref()
                    .and_then(|ty| module.task_result(ty))
                    .cloned()
                    .unwrap_or(CheckedType::Error);
                LoweredAwaitKind::Task { result }
            }
            ResourceCoroutineRoute::AwaitWait => {
                let result = operand_type
                    .as_ref()
                    .and_then(|ty| module.wait_result(ty))
                    .cloned()
                    .unwrap_or(CheckedType::Error);
                LoweredAwaitKind::Wait { result }
            }
            ResourceCoroutineRoute::AwaitChildCoroutine => {
                let Some((child_deferred, child_result)) = operand_type
                    .as_ref()
                    .and_then(|ty| module.coroutine_parts(ty))
                    .map(|(effects, result)| (effects.clone(), result.clone()))
                else {
                    return Err(Diagnostic::new(span, "`await` requires a coroutine"));
                };
                let mut deferred_resources = Vec::with_capacity(child_deferred.resources.len());
                for resource in &child_deferred.resources {
                    deferred_resources.push(self.bind_resource_requirement(
                        module,
                        await_.operand.syntax().id,
                        await_.operand.syntax().span.clone(),
                        LoweredResourceUseKind::HiddenArgument,
                        resource.clone(),
                        Some(function),
                    )?);
                }
                let plan = self
                    .expressions
                    .get(operand)
                    .and_then(|expression| match &expression.kind {
                        LoweredExpressionKind::Coro(coro) => self.coros.get(*coro),
                        _ => None,
                    })
                    .map(|coro| coro.plan);
                LoweredAwaitKind::ChildCoroutine {
                    plan,
                    child_result,
                    deferred_resources,
                    until: await_operand_is_until(module, &await_.operand),
                }
            }
            other => {
                return Err(Diagnostic::new(
                    span,
                    format!("`await` route {other:?} is not an await route"),
                ));
            }
        };
        let state = self
            .coroutine_plans
            .get(plan_id)
            .map(|plan| plan.awaits.len() + 1)
            .unwrap_or(0);
        let resume_points = self
            .coroutine_plans
            .get(plan_id)
            .map(|plan| plan.resume_points)
            .unwrap_or(0);
        if state == 0 || state > resume_points {
            return Err(Diagnostic::new(
                span,
                format!(
                    "await resume state {state} is outside the coroutine's 1..={resume_points} resume states"
                ),
            ));
        }
        let result_type = module
            .type_of_expression(await_.syntax.id)
            .cloned()
            .unwrap_or(CheckedType::Never);
        let outcome = LoweredAwaitOutcome::for_await(&kind, &result_type)
            .map_err(|message| Diagnostic::new(span.clone(), message))?;
        let await_id = self.awaits.push(LoweredAwait {
            origin: Origin {
                syntax: await_.syntax.id,
                span: await_.syntax.span.clone(),
            },
            operand,
            result_type,
            owning_plan: plan_id,
            resume_state: state,
            kind,
            outcome,
        });
        if let Some(plan) = self.coroutine_plans.get_mut(plan_id) {
            plan.awaits.push(await_id);
        }
        Ok(await_id)
    }

    /// Lowers a symbol-selected name occurrence. Functions, constructors,
    /// trait-method selectors, and other callable values get explicit
    /// construction plans; singleton values record their identity and remain
    /// ordinary reads.
    fn lower_name(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        syntax: SyntaxId,
        span: Span,
        symbol: Option<SymbolId>,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        let resolved = module.resolved();
        if let Some(symbol) = symbol
            && compile_time_only_symbol(module, symbol)
        {
            return Err(Diagnostic::new(
                span,
                format!("compile-time-only symbol {symbol:?} reached lowering as a runtime value"),
            ));
        }
        // A name or companion selector that the checker resolved to a trait
        // method is a first-class callable value, as are constructors and
        // function bindings. Their explicit targets and closure construction
        // plans are owned here.
        if module.trait_dispatch_for(syntax).is_some()
            || symbol.is_some_and(|symbol| {
                resolved.constructor_type(symbol).is_some()
                    || module.function_for_symbol(symbol).is_some()
                    || resolved.is_external_symbol(symbol)
                    || resolved.intrinsic_function(symbol).is_some()
            })
        {
            let value = self.lower_callable_value(module, owner, syntax, span, symbol)?;
            return Ok(LoweredExpressionKind::CallableValue(value));
        }
        let Some(symbol) = symbol else {
            return Err(Diagnostic::new(
                span,
                "cannot lower a name without a resolved symbol",
            ));
        };
        if self.symbols.get(symbol).is_none() {
            return Err(Diagnostic::new(
                span,
                format!("symbol {symbol:?} is missing from the lowered symbol catalog"),
            ));
        }
        let requires_initialization_check = resolved.requires_initialization_check(syntax);
        let mutable = module.has_mutable_storage(symbol);
        let singleton = resolved.singleton_type(symbol);
        let reactive = self.lower_name_reactive_operation(
            module,
            Origin {
                syntax,
                span: span.clone(),
            },
            symbol,
        );
        Ok(LoweredExpressionKind::Name(LoweredName {
            symbol,
            requires_initialization_check,
            mutable,
            singleton,
            reactive,
        }))
    }

    /// Lowers a structural access. A symbol-selected access (a singleton or a
    /// companion/function value) lowers like a name instead of reading a base
    /// expression, matching the checked access metadata.
    fn lower_access(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        access: &staple_syntax::AccessExpression,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        if module.trait_dispatch_for(access.syntax.id).is_some() {
            let value = self.lower_callable_value(
                module,
                owner,
                access.syntax.id,
                access.syntax.span.clone(),
                module.symbol_for(access.syntax.id),
            )?;
            return Ok(LoweredExpressionKind::CallableValue(value));
        }
        if let Some(symbol) = module.symbol_for(access.syntax.id) {
            return self.lower_name(
                module,
                owner,
                access.syntax.id,
                access.syntax.span.clone(),
                Some(symbol),
            );
        }
        let Some(checked) = module.access_for(access.syntax.id).cloned() else {
            return Err(Diagnostic::new(
                access.syntax.span.clone(),
                "cannot lower an access without checked access metadata",
            ));
        };
        let base = self.lower_expression(module, owner, context, &access.value)?;
        let kind = match checked {
            CheckedAccess::Representation { dereference } => {
                LoweredAccessKind::Representation { dereference }
            }
            CheckedAccess::Product {
                index,
                dereference,
                slice,
                scalar,
            } => {
                if scalar {
                    LoweredAccessKind::Scalar { dereference }
                } else if slice {
                    LoweredAccessKind::Slice { index, dereference }
                } else {
                    LoweredAccessKind::Product { index, dereference }
                }
            }
        };
        Ok(LoweredExpressionKind::Access(LoweredAccess { base, kind }))
    }

    fn lower_integer(
        &self,
        module: &TypedModule,
        integer: &staple_syntax::IntegerExpression,
    ) -> Result<LoweredInteger, Diagnostic> {
        let value = integer.literal.parse::<u64>().map_err(|_| {
            Diagnostic::new(
                integer.syntax.span.clone(),
                format!("integer literal `{}` is too large", integer.literal),
            )
        })?;
        let integer_type = module
            .type_of_expression(integer.syntax.id)
            .and_then(CheckedType::integer_type)
            .unwrap_or(IntegerType::I32);
        let width = integer_literal_bit_width(integer_type);
        let value_bits = if integer_type.is_signed() {
            width - 1
        } else {
            width
        };
        if value_bits < 64 && value > ((1_u64 << value_bits) - 1) {
            return Err(Diagnostic::new(
                integer.syntax.span.clone(),
                format!(
                    "integer literal `{}` does not fit in `{}`",
                    integer.literal,
                    integer_type.name()
                ),
            ));
        }
        Ok(LoweredInteger {
            value,
            integer_type,
        })
    }

    fn lower_float(
        &self,
        module: &TypedModule,
        float: &staple_syntax::FloatExpression,
    ) -> Result<LoweredFloat, Diagnostic> {
        let float_type = module
            .type_of_expression(float.syntax.id)
            .and_then(CheckedType::float_type)
            .unwrap_or(FloatType::F64);
        let value = match float_type {
            FloatType::F32 => float.literal.parse::<f32>().map(f64::from),
            FloatType::F64 => float.literal.parse::<f64>(),
        }
        .map_err(|_| Diagnostic::new(float.syntax.span.clone(), "invalid float literal"))?;
        if !value.is_finite() {
            return Err(Diagnostic::new(
                float.syntax.span.clone(),
                format!(
                    "float literal `{}` does not fit in `{}`",
                    float.literal,
                    float_type.name()
                ),
            ));
        }
        Ok(LoweredFloat { value, float_type })
    }

    fn lower_c_string(
        &self,
        string: &staple_syntax::CStringExpression,
    ) -> Result<LoweredCString, Diagnostic> {
        self.lower_c_string_literal(&string.literal, string.syntax.span.clone())
    }

    /// Decodes a C-string literal once, validating the interior-NUL rule.
    /// Shared by `Expression::CString` and any surviving `c_string` primitive
    /// call.
    fn lower_c_string_literal(
        &self,
        literal: &str,
        span: Span,
    ) -> Result<LoweredCString, Diagnostic> {
        let value = staple_syntax::string_literal::decode(literal)
            .map_err(|message| Diagnostic::new(span.clone(), message))?;
        if value.as_bytes().contains(&0) {
            return Err(Diagnostic::new(
                span,
                "C string literals cannot contain an interior NUL byte",
            ));
        }
        let mut bytes = value.into_bytes();
        bytes.push(0);
        Ok(LoweredCString { bytes })
    }

    /// Lowers a product construction into ordered evaluation steps plus a
    /// final layout. Designated fields resolve to checked slots, spreads
    /// expand to explicit source/destination mappings, and contextual defaults
    /// fill the remaining slots in final order. Explicit values override
    /// earlier spread values exactly as the checked plan permits.
    fn lower_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        product: &staple_syntax::ProductExpression,
    ) -> Result<LoweredProduct, Diagnostic> {
        // The checker normalizes a plain one-element product `(e)` to its
        // element: the expression's type is `e`'s type, which may itself be a
        // product (`()` or a pair). Lower it as that element in a one-element
        // shape, which emission collapses to the value itself, rather than
        // reading the element's own product type as this product's layout.
        if let [element] = product.elements.as_slice()
            && !element.spread
            && !element.designated
            && !element.named_spread
        {
            let expression = self.lower_expression(module, owner, context, &element.value)?;
            let value_type = self
                .expressions
                .get(expression)
                .map(|expression| expression.value_type.clone())
                .unwrap_or(CheckedType::Never);
            return Ok(LoweredProduct {
                final_type: CheckedProductType {
                    elements: vec![crate::CheckedTypeElement {
                        name: element.name.clone(),
                        value_type,
                        default: None,
                    }],
                    variadic: false,
                },
                fields: vec![expression],
                steps: vec![LoweredProductStep::Positional {
                    expression,
                    slot: 0,
                }],
            });
        }
        let checked = module.type_of_expression(product.syntax.id).cloned();
        let plan = module.product_default_plan(product.syntax.id).cloned();
        let final_type = match &plan {
            Some(plan) => Some(plan.final_type.clone()),
            None => match &checked {
                Some(CheckedType::Product(product_type)) if !product_type.variadic => {
                    Some(product_type.clone())
                }
                _ => None,
            },
        };
        let Some(final_type) = final_type else {
            // The checker diverged before recording a product shape, so the
            // product is unreachable. Lower every element in source order and
            // record it positionally; subsequent consumers never emit this value.
            let mut steps = Vec::new();
            let mut elements = Vec::new();
            for element in &product.elements {
                let expression = self.lower_expression(module, owner, context, &element.value)?;
                let value_type = self
                    .expressions
                    .get(expression)
                    .map(|expression| expression.value_type.clone())
                    .unwrap_or(CheckedType::Never);
                elements.push(crate::CheckedTypeElement {
                    name: element.name.clone(),
                    value_type,
                    default: None,
                });
                steps.push(LoweredProductStep::Positional {
                    expression,
                    slot: elements.len() - 1,
                });
            }
            return Ok(LoweredProduct {
                final_type: CheckedProductType {
                    elements,
                    variadic: false,
                },
                fields: steps
                    .iter()
                    .map(|step| match step {
                        LoweredProductStep::Positional { expression, .. } => Ok(*expression),
                        _ => Err(internal_invariant(
                            product.syntax.span.clone(),
                            "fallback products contain only positional steps",
                        )),
                    })
                    .collect::<Result<Vec<_>, Diagnostic>>()?,
                steps,
            });
        };

        if product.elements.iter().any(|element| element.designated) {
            self.lower_designated_product(module, owner, context, product, &final_type)
        } else if product.elements.iter().any(|element| element.named_spread) {
            self.lower_named_spread_product(module, owner, context, product, &final_type)
        } else {
            self.lower_positional_product(module, owner, context, product, &final_type)
        }
    }

    fn lower_positional_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
    ) -> Result<LoweredProduct, Diagnostic> {
        let mut steps = Vec::new();
        let mut fields = vec![None; final_type.elements.len()];
        let mut positioned = 0usize;
        for element in &product.elements {
            let expression = self.lower_expression(module, owner, context, &element.value)?;
            if element.spread {
                let mappings =
                    self.expand_positional_spread(element, expression, positioned, final_type)?;
                for mapping in &mappings {
                    fields[mapping.slot] = Some(expression);
                }
                positioned += mappings.len();
                steps.push(LoweredProductStep::PositionalSpread {
                    expression,
                    mappings,
                });
            } else {
                if positioned >= final_type.elements.len() {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "too many positional elements in product",
                    ));
                }
                fields[positioned] = Some(expression);
                steps.push(LoweredProductStep::Positional {
                    expression,
                    slot: positioned,
                });
                positioned += 1;
            }
        }
        self.fill_product_defaults(module, owner, product, final_type, &mut steps, &mut fields)?;
        self.finish_product(product, final_type, steps, fields)
    }

    fn lower_designated_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
    ) -> Result<LoweredProduct, Diagnostic> {
        let mut steps = Vec::new();
        let mut fields = vec![None; final_type.elements.len()];
        let mut positioned = 0usize;
        for element in &product.elements {
            let expression = self.lower_expression(module, owner, context, &element.value)?;
            if element.designated {
                let name = element.name.clone().ok_or_else(|| {
                    internal_invariant(
                        element.value.syntax().span.clone(),
                        "designators always have a name",
                    )
                })?;
                let Some(slot) = final_type
                    .elements
                    .iter()
                    .position(|field| field.name.as_deref() == Some(name.as_str()))
                else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        format!("unknown designated product field `{name}`"),
                    ));
                };
                fields[slot] = Some(expression);
                steps.push(LoweredProductStep::Designated {
                    name,
                    expression,
                    slot,
                });
                continue;
            }
            if element.spread {
                let mappings =
                    self.expand_positional_spread(element, expression, positioned, final_type)?;
                for mapping in &mappings {
                    fields[mapping.slot] = Some(expression);
                }
                positioned += mappings.len();
                steps.push(LoweredProductStep::PositionalSpread {
                    expression,
                    mappings,
                });
                continue;
            }
            if positioned >= final_type.elements.len() {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    "too many positional elements in designated product initializer",
                ));
            }
            fields[positioned] = Some(expression);
            steps.push(LoweredProductStep::Positional {
                expression,
                slot: positioned,
            });
            positioned += 1;
        }
        self.fill_product_defaults(module, owner, product, final_type, &mut steps, &mut fields)?;
        self.finish_product(product, final_type, steps, fields)
    }

    fn lower_named_spread_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
    ) -> Result<LoweredProduct, Diagnostic> {
        let mut steps = Vec::new();
        let mut fields = vec![None; final_type.elements.len()];
        for element in &product.elements {
            let expression = self.lower_expression(module, owner, context, &element.value)?;
            if element.spread {
                let operand = self
                    .expressions
                    .get(expression)
                    .map(|expression| expression.value_type.clone())
                    .unwrap_or(CheckedType::Never);
                let CheckedType::Product(operand_type) = operand else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand does not have a fixed product type",
                    ));
                };
                let mut mappings = Vec::new();
                for (source, field) in operand_type.elements.iter().enumerate() {
                    let Some(name) = field.name.clone() else {
                        return Err(Diagnostic::new(
                            element.syntax.span.clone(),
                            "a named spread operand must have every element named",
                        ));
                    };
                    let Some(slot) = final_type
                        .elements
                        .iter()
                        .position(|final_field| final_field.name.as_deref() == Some(name.as_str()))
                    else {
                        return Err(Diagnostic::new(
                            element.syntax.span.clone(),
                            format!("unknown field `{name}` in named product spread"),
                        ));
                    };
                    fields[slot] = Some(expression);
                    mappings.push(LoweredNamedSpreadMapping { name, source, slot });
                }
                steps.push(LoweredProductStep::NamedSpread {
                    expression,
                    mappings,
                });
                continue;
            }
            let Some(name) = element.name.clone() else {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    "every element must be named when the product contains a named spread",
                ));
            };
            let Some(slot) = final_type
                .elements
                .iter()
                .position(|field| field.name.as_deref() == Some(name.as_str()))
            else {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    format!("unknown field `{name}` in named product spread"),
                ));
            };
            fields[slot] = Some(expression);
            steps.push(LoweredProductStep::Designated {
                name,
                expression,
                slot,
            });
        }
        // Named spreads are checked complete: every final field must be
        // provided, and defaults are not part of this construction path.
        for (slot, field) in fields.iter().enumerate() {
            if field.is_none() {
                return Err(missing_product_slot_error(
                    final_type,
                    slot,
                    &product.syntax.span,
                ));
            }
        }
        self.finish_product(product, final_type, steps, fields)
    }

    /// Expands a positional spread operand into explicit source-index to
    /// destination-slot mappings, rejecting operands the checker should have
    /// rejected and destinations outside the final shape.
    fn expand_positional_spread(
        &self,
        element: &staple_syntax::ProductElement,
        expression: ExpressionId,
        positioned: usize,
        final_type: &CheckedProductType,
    ) -> Result<Vec<LoweredSpreadMapping>, Diagnostic> {
        let operand = self
            .expressions
            .get(expression)
            .map(|expression| expression.value_type.clone())
            .unwrap_or(CheckedType::Never);
        let CheckedType::Product(operand_type) = operand else {
            return Err(Diagnostic::new(
                element.syntax.span.clone(),
                "product spread operand does not have a fixed product type",
            ));
        };
        if operand_type.variadic {
            return Err(Diagnostic::new(
                element.syntax.span.clone(),
                "cannot spread a variadic product",
            ));
        }
        let mut mappings = Vec::new();
        for source in 0..operand_type.elements.len() {
            let Some(_) = final_type.elements.get(positioned + source) else {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    "too many positional elements in product",
                ));
            };
            mappings.push(LoweredSpreadMapping {
                source,
                slot: positioned + source,
            });
        }
        Ok(mappings)
    }

    /// Evaluates missing contextual defaults in final slot order using
    /// occurrence-aware keys and the plan's contextual element type.
    fn fill_product_defaults(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
        steps: &mut Vec<LoweredProductStep>,
        fields: &mut [Option<ExpressionId>],
    ) -> Result<(), Diagnostic> {
        let plan = module.product_default_plan(product.syntax.id).cloned();
        for slot in 0..final_type.elements.len() {
            if fields[slot].is_some() {
                continue;
            }
            let default = plan
                .as_ref()
                .and_then(|plan| plan.defaults.get(slot))
                .and_then(|default| default.as_ref());
            let Some(default) = default else {
                return Err(missing_product_slot_error(
                    final_type,
                    slot,
                    &product.syntax.span,
                ));
            };
            let expected = final_type.elements[slot].value_type.clone();
            let default_context = ExpressionContext::ContextualDefault {
                consumer: product.syntax.id,
                slot,
            };
            let expression = self.lower_expression_occurrence(
                module,
                owner,
                default_context,
                default,
                Some(expected.clone()),
            )?;
            fields[slot] = Some(expression);
            steps.push(LoweredProductStep::Default {
                slot,
                expression,
                expected,
            });
        }
        Ok(())
    }

    fn finish_product(
        &self,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
        steps: Vec<LoweredProductStep>,
        fields: Vec<Option<ExpressionId>>,
    ) -> Result<LoweredProduct, Diagnostic> {
        let mut final_fields = Vec::with_capacity(fields.len());
        for (slot, field) in fields.into_iter().enumerate() {
            match field {
                Some(field) => final_fields.push(field),
                None => {
                    return Err(missing_product_slot_error(
                        final_type,
                        slot,
                        &product.syntax.span,
                    ));
                }
            }
        }
        Ok(LoweredProduct {
            final_type: final_type.clone(),
            steps,
            fields: final_fields,
        })
    }

    /// Lowers `&&`/`||` operands left-to-right and copies the checked `Bool`
    /// selection, resolving the `True` alternative index the backend needs.
    fn lower_logical(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        logical: &staple_syntax::LogicalExpression,
    ) -> Result<LoweredLogical, Diagnostic> {
        let left = self.lower_expression(module, owner, context, &logical.left)?;
        let right = self.lower_expression(module, owner, context, &logical.right)?;
        let checked = module.logical_for(logical.syntax.id).cloned();
        // Missing metadata means checking diverged before the logical was
        // recorded, so the expression is unreachable and never emitted.
        let bool_type = match checked {
            Some(checked) => checked.bool_type,
            None => module
                .type_of_expression(logical.syntax.id)
                .cloned()
                .unwrap_or(CheckedType::Never),
        };
        let true_index = match &bool_type {
            CheckedType::Sum(sum) => sum
                .alternatives
                .iter()
                .position(|alternative| {
                    matches!(alternative, CheckedType::Distinct { name, .. } if name == "True")
                })
                .ok_or_else(|| {
                    Diagnostic::new(
                        logical.syntax.span.clone(),
                        "`Bool` has no `True` alternative",
                    )
                })?,
            CheckedType::Never => 0,
            _ => {
                return Err(Diagnostic::new(
                    logical.syntax.span.clone(),
                    "`&&`/`||` require `Bool` to be a sum type",
                ));
            }
        };
        Ok(LoweredLogical {
            operator: logical.operator,
            left,
            right,
            bool_type,
            true_index,
        })
    }

    /// Lowers a string template: parts keep their order and decoded literal
    /// text, interpolations lower left-to-right, and each interpolation keeps
    /// its checked value type plus the selected formatting trait and method.
    /// Missing formatter helpers or trait selections diagnose here instead of
    /// being rediscovered by name during emission.
    fn lower_string_template(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        template: &staple_syntax::StringTemplateExpression,
    ) -> Result<LoweredStringTemplate, Diagnostic> {
        let formatting = module.string_formatting();
        if formatting.formatter_new.is_none()
            || formatting.formatter_write.is_none()
            || formatting.formatter_finish.is_none()
        {
            return Err(Diagnostic::new(
                template.syntax.span.clone(),
                "string formatting helpers are unavailable",
            ));
        }
        let mut parts = Vec::with_capacity(template.parts.len());
        for part in &template.parts {
            match part {
                staple_syntax::StringTemplatePart::Literal(literal) => {
                    parts.push(LoweredStringTemplatePart::Literal(literal.clone()));
                }
                staple_syntax::StringTemplatePart::Interpolation(interpolation) => {
                    let expression =
                        self.lower_expression(module, owner, context, &interpolation.expression)?;
                    let value_type = self
                        .expressions
                        .get(expression)
                        .map(|expression| expression.value_type.clone())
                        .unwrap_or(CheckedType::Never);
                    let checked = formatting
                        .interpolations
                        .get(&interpolation.expression.syntax().id)
                        .cloned();
                    let (trait_id, method, value_type) = match checked {
                        Some(checked) => (checked.trait_id, checked.method, checked.value_type),
                        None => {
                            // Checking diverged before recording this
                            // interpolation; the template is unreachable. Use
                            // the checker-selected formatting trait, never a
                            // name lookup.
                            let trait_id = match interpolation.format {
                                staple_syntax::StringInterpolationFormat::Display => {
                                    module.semantic_ids().display_trait
                                }
                                staple_syntax::StringInterpolationFormat::Debug => {
                                    module.semantic_ids().debug_trait
                                }
                            }
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    interpolation.expression.syntax().span.clone(),
                                    "standard formatting trait is unavailable",
                                )
                            })?;
                            let method = self
                                .traits
                                .get(trait_id)
                                .and_then(|trait_| trait_.methods.first().copied())
                                .ok_or_else(|| {
                                    Diagnostic::new(
                                        interpolation.expression.syntax().span.clone(),
                                        "formatting trait has no method",
                                    )
                                })?;
                            (trait_id, method, value_type)
                        }
                    };
                    let evidence = self.trait_evidence_for(
                        module,
                        owner,
                        &Origin {
                            syntax: interpolation.expression.syntax().id,
                            span: interpolation.expression.syntax().span.clone(),
                        },
                        trait_id,
                        method,
                        std::slice::from_ref(&value_type),
                    )?;
                    parts.push(LoweredStringTemplatePart::Interpolation(
                        LoweredInterpolation {
                            expression,
                            #[cfg(test)]
                            format: interpolation.format,
                            value_type,
                            trait_id,
                            method,
                            evidence,
                        },
                    ));
                }
            }
        }
        Ok(LoweredStringTemplate { parts })
    }

    /// Lowers `base[index]` in evaluation order and copies the checked `Index`
    /// dispatch recipe: owning trait, completed argument types, instantiated
    /// method type (mutation/move masks, effects, resources), and the
    /// temporary-cleanup facts for mutation operands that are not places.
    fn lower_index(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        index: &staple_syntax::IndexExpression,
    ) -> Result<LoweredIndex, Diagnostic> {
        let base = self.lower_expression(module, owner, context, &index.value)?;
        let position = self.lower_expression(module, owner, context, &index.index)?;
        let dispatch = module
            .trait_dispatch_for(index.syntax.id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(index.syntax.span.clone(), "missing `Index` dispatch")
            })?;
        let Some(trait_id) = module.resolved().trait_for_method(dispatch.method) else {
            return Err(Diagnostic::new(
                index.syntax.span.clone(),
                "`Index` dispatch method has no owning trait",
            ));
        };
        let arguments = module
            .complete_trait_arguments(trait_id, &dispatch.arguments)
            .ok_or_else(|| {
                Diagnostic::new(index.syntax.span.clone(), "incomplete `Index` dispatch")
            })?;
        let method_type =
            module.instantiated_trait_method_type(trait_id, &arguments, dispatch.method);
        let base_is_place = expression_has_place_root(module.resolved(), &index.value);
        let index_is_place = expression_has_place_root(module.resolved(), &index.index);
        let mut whole_temporary = false;
        let mut base_temporary = false;
        let mut index_temporary = false;
        if let Some(method_type) = &method_type {
            for target in method_type.mutations.iter().chain(&method_type.moves) {
                match target {
                    CheckedMutation::Whole => whole_temporary = true,
                    CheckedMutation::Element(0) => base_temporary |= !base_is_place,
                    CheckedMutation::Element(1) => index_temporary |= !index_is_place,
                    CheckedMutation::Element(_) => {}
                }
            }
        }
        // Record the operand places so emission can reuse them without
        // re-deriving `expression_has_place_root`.
        let base_place = if base_is_place {
            self.lower_place(module, owner, context, &index.value).ok()
        } else {
            None
        };
        let index_place = if index_is_place {
            self.lower_place(module, owner, context, &index.index).ok()
        } else {
            None
        };
        let evidence = self.evidence_for_dispatch(
            module,
            owner,
            &Origin {
                syntax: index.syntax.id,
                span: index.syntax.span.clone(),
            },
            &dispatch,
        )?;
        let operands = match &method_type {
            Some(method_type) => {
                let base_type = self
                    .expressions
                    .get(base)
                    .map(|expression| expression.value_type.clone())
                    .unwrap_or(CheckedType::Error);
                let position_type = self
                    .expressions
                    .get(position)
                    .map(|expression| expression.value_type.clone())
                    .unwrap_or(CheckedType::Error);
                LoweredIndexOperands::compute(
                    method_type,
                    [&base_type, &position_type],
                    [base_place.is_some(), index_place.is_some()],
                    whole_temporary,
                    |value_type| module.is_copy_in_function(value_type, None),
                    |value_type| module.type_needs_drop(value_type),
                )
            }
            None => LoweredIndexOperands::default(),
        };
        Ok(LoweredIndex {
            base,
            index: position,
            dispatch,
            trait_id,
            arguments,
            method_type,
            whole_temporary,
            base_temporary,
            index_temporary,
            base_place,
            index_place,
            operands,
            evidence,
        })
    }

    /// Lowers a loop body into the existing block/item arenas, tracks loop
    /// nesting for break/continue validation, and records the body-result drop
    /// requirement and fall-through fact.
    fn lower_loop(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        loop_: &staple_syntax::LoopExpression,
    ) -> Result<LoweredLoop, Diagnostic> {
        let body_type = module.type_of_expression(loop_.body.syntax.id).cloned();
        let result_type = module
            .type_of_expression(loop_.syntax.id)
            .cloned()
            .unwrap_or(CheckedType::Never);
        self.loop_depth += 1;
        let body = self.lower_block(module, owner, context, &loop_.body);
        self.loop_depth -= 1;
        let body = body?;
        let drops_body_result = body_type
            .as_ref()
            .is_some_and(|body_type| module.type_needs_drop(body_type));
        Ok(LoweredLoop {
            body,
            result_type,
            drops_body_result,
            depth: self.loop_depth + 1,
        })
    }

    /// Lowers a match subject first, then arms in source order, reusing the
    /// lowering pattern lowering and copying the checked subject type.
    fn lower_match(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        match_: &staple_syntax::MatchExpression,
    ) -> Result<LoweredMatch, Diagnostic> {
        let subject = self.lower_expression(module, owner, context, &match_.subject)?;
        let Some(checked) = module.match_for(match_.syntax.id).cloned() else {
            // The checker diverged before recording the match (for example
            // when the subject returns), so the whole match is unreachable and
            // code generation never emits its arms.
            let source = self
                .expressions
                .get(subject)
                .map(|subject| subject.value_type.clone())
                .unwrap_or(CheckedType::Never);
            return Ok(LoweredMatch {
                subject,
                source,
                arms: Vec::new(),
            });
        };
        let mut arms = Vec::with_capacity(match_.arms.len());
        for arm in &match_.arms {
            let pattern = self.lower_pattern(module, &arm.pattern, &checked.source)?;
            let body = self.lower_expression(module, owner, context, &arm.body)?;
            arms.push(LoweredMatchArm {
                origin: Origin {
                    syntax: arm.syntax.id,
                    span: arm.syntax.span.clone(),
                },
                pattern,
                body,
                bound_symbols: pattern_symbols(module.resolved(), &arm.pattern),
            });
        }
        Ok(LoweredMatch {
            subject,
            source: checked.source,
            arms,
        })
    }

    /// Lowers `(value; count)`: the checked result type gives the repetition
    /// count and whether the single-element representation collapses.
    fn lower_repeated_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        repeated: &staple_syntax::RepeatedProductExpression,
    ) -> Result<LoweredRepeatedProduct, Diagnostic> {
        let expression = self.lower_expression(module, owner, context, &repeated.value)?;
        let source_type = module
            .coercion_for(repeated.syntax.id)
            .map(|coercion| &coercion.source)
            .or_else(|| module.type_of_expression(repeated.syntax.id))
            .unwrap_or(&CheckedType::Error);
        let count = LoweredRepeatCount::for_type(source_type);
        let collapsed = count == LoweredRepeatCount::Fixed(1);
        Ok(LoweredRepeatedProduct {
            expression,
            count,
            collapsed,
        })
    }

    fn expression_needs_drop(&self, module: &TypedModule, expression: ExpressionId) -> bool {
        self.expressions
            .get(expression)
            .is_some_and(|expression| module.type_needs_drop(&expression.value_type))
    }

    /// Classifies a checked call: juxtaposition, curried defaults, trait dispatch,
    /// intrinsic, generic direct, external, then indirect closure. Every call has
    /// an explicit route.
    fn classify_call_route(
        &self,
        module: &TypedModule,
        owner: ExpressionOwner,
        call: &staple_syntax::CallExpression,
    ) -> Result<CallRoute, Diagnostic> {
        let resolved = module.resolved();
        if resolved.primitive_macro_for(call.syntax.id).is_some() {
            return Ok(CallRoute::PrimitiveMacro);
        }
        if let Some(symbol) = module.symbol_for(call.callee.syntax().id)
            && resolved.constructor_type(symbol).is_some()
        {
            return Ok(CallRoute::Constructor);
        }
        if let Some(plan) = module.juxtaposed_call_plan(call.syntax.id) {
            let expected = match plan.function.parameter.as_ref() {
                CheckedType::Product(product) => product.elements.len(),
                _ => 0,
            };
            if plan.arguments.len() == expected {
                let mut callee = call.callee.as_ref();
                for _ in 1..plan.consumed_calls {
                    let Expression::Call(previous) = callee else {
                        break;
                    };
                    callee = previous.callee.as_ref();
                }
                let intrinsic = module
                    .symbol_for(callee.syntax().id)
                    .and_then(|symbol| resolved.intrinsic_function(symbol));
                return Ok(match intrinsic {
                    Some(_) => CallRoute::JuxtaposedIntrinsic,
                    None => CallRoute::Juxtaposed,
                });
            }
        }
        if module.curried_default_plan(call.syntax.id).is_some() {
            return Ok(CallRoute::CurriedDefault);
        }
        if let Some(dispatch) = module.trait_dispatch_for(call.callee.syntax().id) {
            return self.classify_trait_call_route(module, dispatch);
        }
        if let Some(symbol) = module.symbol_for(call.callee.syntax().id) {
            if resolved.intrinsic_function(symbol).is_some() {
                return Ok(CallRoute::Intrinsic);
            }
            if !self.call_callee_is_local(owner, symbol)
                && let Some(function) = module.function_for_symbol(symbol)
                && self
                    .functions
                    .get(function)
                    .is_some_and(|function| function.captures.is_empty())
                && module
                    .type_of_function(function)
                    .is_some_and(|function_type| {
                        contains_type_parameter(&CheckedType::Function(function_type.clone()))
                    })
            {
                return Ok(CallRoute::GenericDirect);
            }
            if !self.call_callee_is_local(owner, symbol) && resolved.is_external_symbol(symbol) {
                return Ok(CallRoute::External);
            }
        }
        Ok(CallRoute::Indirect)
    }

    /// Resolves the trait-dispatch route into explicit evidence: a selected
    /// implementation, a structural method, or a declared bound whose
    /// selection waits for specialization substitution.
    fn classify_trait_call_route(
        &self,
        module: &TypedModule,
        dispatch: &CheckedTraitDispatch,
    ) -> Result<CallRoute, Diagnostic> {
        let Some(trait_id) = module.resolved().trait_for_method(dispatch.method) else {
            return Err(Diagnostic::new(
                Span::Compiler,
                format!("trait method {} has no owning trait", dispatch.method.0),
            ));
        };
        let Some(arguments) = module.complete_trait_arguments(trait_id, &dispatch.arguments) else {
            return Ok(CallRoute::DeclaredTraitBound);
        };
        if module
            .trait_impl_method(trait_id, &arguments, dispatch.method)
            .is_some()
        {
            return Ok(CallRoute::TraitImplementation);
        }
        if module
            .structural_trait_method(trait_id, &arguments)
            .is_some()
        {
            return Ok(CallRoute::StructuralTraitMethod);
        }
        // A generic argument's implementation may only be selectable after
        // specialization has canonical substitutions.
        Ok(CallRoute::DeclaredTraitBound)
    }

    /// Classifies a callable construction as a trait method, constructor, intrinsic,
    /// extern, declared or generic function, anonymous function, or stored read.
    fn classify_callable_value_route(
        &self,
        module: &TypedModule,
        syntax: SyntaxId,
        symbol: Option<SymbolId>,
    ) -> CallableValueRoute {
        if module.trait_dispatch_for(syntax).is_some() {
            return CallableValueRoute::TraitMethod;
        }
        let Some(symbol) = symbol else {
            return CallableValueRoute::AnonymousFunction;
        };
        let resolved = module.resolved();
        if resolved.constructor_type(symbol).is_some() {
            return CallableValueRoute::Constructor;
        }
        // Extern bindings are function candidates too, but their first-class
        // values go through the generated extern adapter, so they must be
        // classified before plain function bindings.
        if resolved.is_external_symbol(symbol) {
            return CallableValueRoute::External;
        }
        if let Some(function) = module.function_for_symbol(symbol) {
            let generic = module
                .type_of_function(function)
                .is_some_and(|function_type| {
                    contains_type_parameter(&CheckedType::Function(function_type.clone()))
                });
            return if generic {
                CallableValueRoute::GenericFunction
            } else {
                CallableValueRoute::DeclaredFunction
            };
        }
        // An intrinsic without a function binding has no first-class value
        // route in the backend; the route stays explicit and defensive.
        if resolved.intrinsic_function(symbol).is_some() {
            return CallableValueRoute::Intrinsic;
        }
        CallableValueRoute::OrdinaryRead
    }

    /// Lowers a first-class callable value into its explicit target,
    /// construction plan, adapter, substitutions, and trait evidence.
    fn lower_callable_value(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        syntax: SyntaxId,
        span: Span,
        symbol: Option<SymbolId>,
    ) -> Result<LoweredCallableValueId, Diagnostic> {
        let origin = Origin { syntax, span };
        let route = self.classify_callable_value_route(module, syntax, symbol);
        // A callable value can be checked as a sum alternative or another
        // coercible type; its underlying callable type comes from the
        // semantic target in that case.
        let checked_function_type = match module.type_of_expression(syntax) {
            Some(CheckedType::Function(function_type)) => Some(function_type.clone()),
            _ => None,
        };
        let resolved = module.resolved();
        let mut substitutions = CallSubstitutions::default();
        let (target, function_type, adapter, closure, evidence) = match route {
            CallableValueRoute::TraitMethod => {
                let dispatch = module.trait_dispatch_for(syntax).cloned().ok_or_else(|| {
                    Diagnostic::new(
                        origin.span.clone(),
                        "trait-method value has no checked dispatch",
                    )
                })?;
                let (target, adapter, closure, evidence) =
                    self.lower_trait_method_value(module, owner, &origin, &dispatch)?;
                let function_type = checked_function_type
                    .or_else(|| {
                        match self
                            .trait_methods
                            .get(dispatch.method)
                            .map(|method| &method.value_type)
                        {
                            Some(CheckedType::Function(function_type)) => {
                                Some(function_type.clone())
                            }
                            _ => None,
                        }
                    })
                    .ok_or_else(|| {
                        Diagnostic::new(
                            origin.span.clone(),
                            "trait-method value has no checked function type",
                        )
                    })?;
                (target, function_type, adapter, closure, evidence)
            }
            CallableValueRoute::Constructor => {
                let symbol = symbol.ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "constructor values are symbol-selected",
                    )
                })?;
                let type_id = resolved.constructor_type(symbol).ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "selected constructor symbol has a registered type",
                    )
                })?;
                let function_type = checked_function_type
                    .or_else(|| symbol_function_type(module, symbol))
                    .ok_or_else(|| {
                        Diagnostic::new(
                            origin.span.clone(),
                            "constructor value has no checked function type",
                        )
                    })?;
                (
                    LoweredCallableTarget::Constructor {
                        symbol,
                        type_id,
                        recursive: resolved.recursive_construction(type_id),
                    },
                    function_type,
                    LoweredCallableAdapter::Constructor,
                    None,
                    None,
                )
            }
            CallableValueRoute::External => {
                let symbol = symbol.ok_or_else(|| {
                    internal_invariant(origin.span.clone(), "extern values are symbol-selected")
                })?;
                let function_type = checked_function_type
                    .or_else(|| symbol_function_type(module, symbol))
                    .ok_or_else(|| {
                        Diagnostic::new(
                            origin.span.clone(),
                            "extern value has no checked function type",
                        )
                    })?;
                (
                    LoweredCallableTarget::ExternalFunction { symbol },
                    function_type,
                    LoweredCallableAdapter::External,
                    None,
                    None,
                )
            }
            CallableValueRoute::Intrinsic => {
                let symbol = symbol.ok_or_else(|| {
                    internal_invariant(origin.span.clone(), "intrinsic values are symbol-selected")
                })?;
                let intrinsic = resolved.intrinsic_function(symbol).ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "selected intrinsic symbol has a registered intrinsic",
                    )
                })?;
                let function_type = checked_function_type
                    .or_else(|| symbol_function_type(module, symbol))
                    .ok_or_else(|| {
                        Diagnostic::new(
                            origin.span.clone(),
                            "intrinsic value has no checked function type",
                        )
                    })?;
                (
                    LoweredCallableTarget::Intrinsic { symbol, intrinsic },
                    function_type,
                    LoweredCallableAdapter::None,
                    None,
                    None,
                )
            }
            CallableValueRoute::GenericFunction
            | CallableValueRoute::DeclaredFunction
            | CallableValueRoute::AnonymousFunction => {
                let function = match route {
                    CallableValueRoute::AnonymousFunction => module.function_for(syntax),
                    _ => symbol.and_then(|symbol| module.function_for_symbol(symbol)),
                }
                .ok_or_else(|| {
                    Diagnostic::new(
                        origin.span.clone(),
                        "callable value has no resolved function",
                    )
                })?;
                let template = module.type_of_function(function).cloned().ok_or_else(|| {
                    Diagnostic::new(
                        origin.span.clone(),
                        format!("function {} has no checked type", function.0),
                    )
                })?;
                let function_type = checked_function_type.unwrap_or_else(|| template.clone());
                substitutions = call_substitutions(&template, &function_type);
                let environment = if route == CallableValueRoute::DeclaredFunction {
                    LoweredClosureEnvironment::Stored
                } else {
                    LoweredClosureEnvironment::Fresh
                };
                let mut closure = self.closure_construction(
                    module,
                    &origin,
                    function,
                    environment,
                    substitutions.clone(),
                )?;
                let adapter = if route == CallableValueRoute::AnonymousFunction
                    && !closure.captures.is_empty()
                {
                    LoweredCallableAdapter::NestedClosure
                } else {
                    LoweredCallableAdapter::None
                };
                closure.adapter = adapter;
                (
                    LoweredCallableTarget::DirectFunction {
                        function,
                        environment: LoweredCallEnvironment::None,
                    },
                    function_type,
                    adapter,
                    Some(closure),
                    None,
                )
            }
            CallableValueRoute::OrdinaryRead => {
                return Err(Diagnostic::new(
                    origin.span.clone(),
                    "an ordinary function-typed value is not a callable construction",
                ));
            }
        };
        let requires_initialization_check = resolved.requires_initialization_check(syntax);
        Ok(self.callable_values.push(LoweredCallableValue {
            origin,
            target,
            function_type,
            adapter,
            closure,
            substitutions,
            evidence,
            requires_initialization_check,
        }))
    }

    /// Lowers a trait-method selector value into explicit evidence. Generic
    /// arguments whose implementation depends on later substitution keep a
    /// declared bound instead of a premature concrete choice.
    #[allow(clippy::type_complexity)]
    fn lower_trait_method_value(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        origin: &Origin,
        dispatch: &CheckedTraitDispatch,
    ) -> Result<
        (
            LoweredCallableTarget,
            LoweredCallableAdapter,
            Option<LoweredClosureConstruction>,
            Option<TraitEvidence>,
        ),
        Diagnostic,
    > {
        let Some(trait_id) = module.resolved().trait_for_method(dispatch.method) else {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!("trait method {} has no owning trait", dispatch.method.0),
            ));
        };
        let evidence = self.trait_evidence_for(
            module,
            owner,
            origin,
            trait_id,
            dispatch.method,
            &dispatch.arguments,
        )?;
        match &evidence {
            TraitEvidence::ExplicitImplementation { function, .. } => {
                let closure = self.closure_construction(
                    module,
                    origin,
                    *function,
                    LoweredClosureEnvironment::Fresh,
                    CallSubstitutions::default(),
                )?;
                Ok((
                    LoweredCallableTarget::TraitImplementation {
                        trait_id,
                        method: dispatch.method,
                        function: Some(*function),
                    },
                    LoweredCallableAdapter::None,
                    Some(closure),
                    Some(evidence),
                ))
            }
            TraitEvidence::Structural { structural, .. } => Ok((
                LoweredCallableTarget::StructuralTraitMethod {
                    trait_id,
                    method: dispatch.method,
                    structural: *structural,
                },
                LoweredCallableAdapter::None,
                None,
                Some(evidence),
            )),
            TraitEvidence::DeclaredBound { .. } => Ok((
                LoweredCallableTarget::TraitImplementation {
                    trait_id,
                    method: dispatch.method,
                    function: None,
                },
                LoweredCallableAdapter::None,
                None,
                Some(evidence),
            )),
        }
    }

    /// Builds the single evidence recipe for a checked trait dispatch:
    /// a selected explicit implementation, a structural method, or the
    /// declared bound specialization must realize after substitution. Selection never
    /// uses display names.
    fn trait_evidence_for(
        &self,
        module: &TypedModule,
        owner: ExpressionOwner,
        origin: &Origin,
        trait_id: TraitId,
        method: TraitMethodId,
        arguments: &[CheckedType],
    ) -> Result<TraitEvidence, Diagnostic> {
        let Some(completed) = module.complete_trait_arguments(trait_id, arguments) else {
            return Ok(TraitEvidence::DeclaredBound {
                trait_id,
                method: Some(method),
                arguments: arguments.to_vec(),
                prerequisites: self.declared_prerequisites(module, owner, trait_id),
            });
        };
        if let Some(function) = module.trait_impl_method(trait_id, &completed, method) {
            let implementation = self
                .trait_implementation_id(trait_id, method, function, &completed)
                .ok_or_else(|| {
                    Diagnostic::new(
                        origin.span.clone(),
                        format!(
                            "trait implementation for method {} and function {} is missing from the lowered catalog",
                            method.0, function.0
                        ),
                    )
                })?;
            return Ok(TraitEvidence::ExplicitImplementation {
                trait_id,
                implementation,
                method,
                function,
                arguments: completed,
            });
        }
        if let Some(structural) = module.structural_trait_method(trait_id, &completed) {
            return Ok(TraitEvidence::Structural {
                trait_id,
                method,
                structural,
                arguments: completed,
            });
        }
        Ok(TraitEvidence::DeclaredBound {
            trait_id,
            method: Some(method),
            arguments: completed,
            prerequisites: self.declared_prerequisites(module, owner, trait_id),
        })
    }

    /// The enclosing function's declared bounds for one trait, retained on a
    /// deferred evidence recipe so specialization can realize the obligation without
    /// a resolver lookup.
    fn declared_prerequisites(
        &self,
        module: &TypedModule,
        owner: ExpressionOwner,
        trait_id: TraitId,
    ) -> Vec<CheckedTraitBound> {
        match owner {
            ExpressionOwner::Function(function) => module
                .bounds_of_function(function)
                .iter()
                .filter(|bound| bound.trait_id == trait_id)
                .cloned()
                .collect(),
            ExpressionOwner::Module(_) => Vec::new(),
        }
    }

    /// Finds the lowered trait implementation that provides `method` for the
    /// completed trait arguments.
    ///
    /// A default method function is shared by every implementation that does
    /// not override it, so the implementation header must match the completed
    /// arguments; matching only the method function would record the first
    /// implementation that happens to share the default.
    fn trait_implementation_id(
        &self,
        trait_id: TraitId,
        method: TraitMethodId,
        function: FunctionId,
        arguments: &[CheckedType],
    ) -> Option<LoweredTraitImplementationId> {
        self.trait_implementations
            .iter()
            .find(|(_, metadata)| {
                metadata.trait_id == trait_id
                    && !metadata.negative
                    && metadata.arguments.len() == arguments.len()
                    && {
                        let mut substitutions = HashMap::new();
                        metadata
                            .arguments
                            .iter()
                            .zip(arguments)
                            .all(|(template, argument)| {
                                infer_type_parameters(template, argument, &mut substitutions)
                            })
                    }
                    && metadata
                        .methods
                        .iter()
                        .any(|(candidate, selected)| *candidate == method && *selected == function)
            })
            .map(|(id, _)| id)
    }

    /// Copies a function's catalog captures in order into a closure plan,
    /// classifying each capture's access and ownership/drop responsibility.
    fn closure_construction(
        &self,
        module: &TypedModule,
        origin: &Origin,
        function: FunctionId,
        environment: LoweredClosureEnvironment,
        substitutions: CallSubstitutions,
    ) -> Result<LoweredClosureConstruction, Diagnostic> {
        let catalog = self.functions.get(function).ok_or_else(|| {
            Diagnostic::new(
                origin.span.clone(),
                format!(
                    "closure function {} is missing from the lowered function catalog",
                    function.0
                ),
            )
        })?;
        let mut captures = Vec::with_capacity(catalog.captures.len());
        for capture in &catalog.captures {
            let value_type = module
                .type_of_symbol(capture.symbol)
                .cloned()
                .or_else(|| module.declared_type_of_symbol(capture.symbol))
                .unwrap_or(CheckedType::Error);
            let access = if capture.requires_cell {
                LoweredCaptureAccess::SharedCell
            } else if capture.borrowed {
                LoweredCaptureAccess::Borrowed
            } else {
                LoweredCaptureAccess::ByValue
            };
            let owns_value = access == LoweredCaptureAccess::ByValue && !capture.non_owning;
            let requires_initialization_state = module
                .resolved()
                .requires_initialization_state(capture.symbol);
            captures.push(LoweredClosureCapture {
                capture: capture.clone(),
                value_type,
                access,
                owns_value,
                requires_initialization_state,
            });
        }
        Ok(LoweredClosureConstruction {
            function,
            captures,
            environment,
            adapter: LoweredCallableAdapter::None,
            substitutions,
        })
    }

    /// Whether a callee symbol is local at this call site: owned by the
    /// enclosing function or captured by it. Local bindings stay indirect.
    fn call_callee_is_local(&self, owner: ExpressionOwner, symbol: SymbolId) -> bool {
        let ExpressionOwner::Function(function) = owner else {
            return false;
        };
        let owned = self
            .symbols
            .get(symbol)
            .is_some_and(|symbol| symbol.owner == Some(function));
        let captured = self.functions.get(function).is_some_and(|function| {
            function
                .captures
                .iter()
                .any(|capture| capture.symbol == symbol)
        });
        owned || captured
    }

    /// Lowers an ordinary, direct, indirect, external, or intrinsic call.
    /// Juxtaposed chains, curried defaults, trait dispatch, constructors, and
    /// primitive macros stay explicitly deferred to their owning steps.
    fn lower_call(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        call: &staple_syntax::CallExpression,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        let route = self.classify_call_route(module, owner, call)?;
        if self.consumed_calls.contains(&call.syntax.id) {
            return Err(Diagnostic::new(
                call.syntax.span.clone(),
                "call expression is consumed by an outer juxtaposed call",
            ));
        }
        if matches!(
            route,
            CallRoute::Juxtaposed | CallRoute::JuxtaposedIntrinsic
        ) {
            return self.lower_juxtaposed_call(module, owner, context, call);
        }
        if route == CallRoute::PrimitiveMacro {
            // Macro expansion normally rewrites `c_string "..."` into a
            // decoded `Expression::CString`; normalize any surviving primitive
            // call to the same owned payload with its interior-NUL check.
            let Expression::String(string) = call.argument.as_ref() else {
                return Err(Diagnostic::new(
                    call.syntax.span.clone(),
                    "`c_string` requires a string literal",
                ));
            };
            let c_string =
                self.lower_c_string_literal(&string.literal, string.syntax.span.clone())?;
            return Ok(LoweredExpressionKind::CString(c_string));
        }
        if !matches!(
            route,
            CallRoute::GenericDirect
                | CallRoute::External
                | CallRoute::Indirect
                | CallRoute::Intrinsic
                | CallRoute::Constructor
                | CallRoute::TraitImplementation
                | CallRoute::DeclaredTraitBound
                | CallRoute::StructuralTraitMethod
        ) {
            return Ok(LoweredExpressionKind::Deferred(
                DeferredExpressionFamily::Callable,
            ));
        }
        let origin = Origin {
            syntax: call.syntax.id,
            span: call.syntax.span.clone(),
        };
        let resolved = module.resolved();
        let callee_syntax = call.callee.syntax().id;
        let symbol = module.symbol_for(callee_syntax);
        let mut substitutions = CallSubstitutions::default();
        let mut initialization_checks = Vec::new();
        let mut evidence = None;
        let (target, callee, function_type) = match route {
            CallRoute::GenericDirect => {
                let symbol = symbol.ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "generic direct calls are symbol-selected",
                    )
                })?;
                let function = module.function_for_symbol(symbol).ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "generic call symbol has a registered function",
                    )
                })?;
                let function_type = checked_call_function_type(module, call, &origin)?;
                let template = module.type_of_function(function).cloned().ok_or_else(|| {
                    Diagnostic::new(
                        origin.span.clone(),
                        format!("function {} has no checked type", function.0),
                    )
                })?;
                substitutions = call_substitutions(&template, &function_type);
                if resolved.requires_initialization_check(callee_syntax) {
                    initialization_checks.push(symbol);
                }
                let environment = match owner {
                    ExpressionOwner::Function(owner_function) if owner_function == function => {
                        LoweredCallEnvironment::Current
                    }
                    _ => LoweredCallEnvironment::None,
                };
                (
                    LoweredCallableTarget::DirectFunction {
                        function,
                        environment,
                    },
                    None,
                    function_type,
                )
            }
            CallRoute::External => {
                let symbol = symbol.ok_or_else(|| {
                    internal_invariant(origin.span.clone(), "extern calls are symbol-selected")
                })?;
                let function_type = checked_call_function_type(module, call, &origin)?;
                (
                    LoweredCallableTarget::ExternalFunction { symbol },
                    None,
                    function_type,
                )
            }
            CallRoute::Intrinsic => {
                let symbol = symbol.ok_or_else(|| {
                    internal_invariant(origin.span.clone(), "intrinsic calls are symbol-selected")
                })?;
                let intrinsic = resolved.intrinsic_function(symbol).ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "selected intrinsic symbol has a registered intrinsic",
                    )
                })?;
                let function_type = checked_call_function_type(module, call, &origin)?;
                (
                    LoweredCallableTarget::Intrinsic { symbol, intrinsic },
                    None,
                    function_type,
                )
            }
            CallRoute::Indirect => {
                let callee = self.lower_expression(module, owner, context, &call.callee)?;
                let function_type = match module.type_of_expression(callee_syntax) {
                    Some(CheckedType::Function(function_type)) => function_type.clone(),
                    _ => self
                        .expressions
                        .get(callee)
                        .and_then(|expression| match &expression.value_type {
                            CheckedType::Function(function_type) => Some(function_type.clone()),
                            _ => None,
                        })
                        .ok_or_else(|| {
                            Diagnostic::new(
                                origin.span.clone(),
                                "indirect call has no checked function type",
                            )
                        })?,
                };
                (
                    LoweredCallableTarget::IndirectClosure { callee },
                    Some(callee),
                    function_type,
                )
            }
            CallRoute::Constructor => {
                let symbol = symbol.ok_or_else(|| {
                    internal_invariant(origin.span.clone(), "constructor calls are symbol-selected")
                })?;
                let type_id = resolved.constructor_type(symbol).ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "selected constructor symbol has a registered type",
                    )
                })?;
                let function_type = checked_call_function_type(module, call, &origin)?;
                (
                    LoweredCallableTarget::Constructor {
                        symbol,
                        type_id,
                        recursive: resolved.recursive_construction(type_id),
                    },
                    None,
                    function_type,
                )
            }
            CallRoute::TraitImplementation
            | CallRoute::DeclaredTraitBound
            | CallRoute::StructuralTraitMethod => {
                let dispatch = module
                    .trait_dispatch_for(callee_syntax)
                    .cloned()
                    .ok_or_else(|| {
                        Diagnostic::new(origin.span.clone(), "trait call has no checked dispatch")
                    })?;
                let trait_id = resolved.trait_for_method(dispatch.method).ok_or_else(|| {
                    Diagnostic::new(
                        origin.span.clone(),
                        format!("trait method {} has no owning trait", dispatch.method.0),
                    )
                })?;
                let recipe = self.trait_evidence_for(
                    module,
                    owner,
                    &origin,
                    trait_id,
                    dispatch.method,
                    &dispatch.arguments,
                )?;
                let completed = match &recipe {
                    TraitEvidence::ExplicitImplementation { arguments, .. }
                    | TraitEvidence::Structural { arguments, .. }
                    | TraitEvidence::DeclaredBound { arguments, .. } => arguments.clone(),
                };
                let function_type = module
                    .instantiated_trait_method_type(trait_id, &completed, dispatch.method)
                    .or_else(|| {
                        match self
                            .trait_methods
                            .get(dispatch.method)
                            .map(|method| &method.value_type)
                        {
                            Some(CheckedType::Function(function_type)) => {
                                Some(function_type.clone())
                            }
                            _ => None,
                        }
                    })
                    .ok_or_else(|| {
                        Diagnostic::new(
                            origin.span.clone(),
                            "trait call has no checked function type",
                        )
                    })?;
                let target = match &recipe {
                    TraitEvidence::Structural { structural, .. } => {
                        LoweredCallableTarget::StructuralTraitMethod {
                            trait_id,
                            method: dispatch.method,
                            structural: *structural,
                        }
                    }
                    _ => LoweredCallableTarget::TraitImplementation {
                        trait_id,
                        method: dispatch.method,
                        function: match &recipe {
                            TraitEvidence::ExplicitImplementation { function, .. } => {
                                Some(*function)
                            }
                            _ => None,
                        },
                    },
                };
                let parameters = self
                    .traits
                    .get(trait_id)
                    .map(|trait_| trait_.parameters.clone())
                    .unwrap_or_default();
                substitutions = trait_call_substitutions(&parameters, &completed);
                evidence = Some(recipe);
                (target, None, function_type)
            }
            _ => {
                return Err(internal_invariant(
                    origin.span.clone(),
                    "non-concrete call routes exit before concrete dispatch",
                ));
            }
        };
        let (arguments, mut steps) = if matches!(
            route,
            CallRoute::External | CallRoute::Intrinsic | CallRoute::Constructor
        ) {
            self.lower_plain_call_arguments(module, owner, context, &call.argument, &function_type)?
        } else {
            self.lower_effect_call_arguments(
                module,
                owner,
                context,
                &call.argument,
                &function_type,
            )?
        };
        if let Some(callee) = callee {
            steps.insert(0, LoweredCallStep::Callee { expression: callee });
        }
        let resource_bindings = if target.records_resources() {
            self.lower_call_resource_bindings(module, owner, &origin, &function_type.effects)?
        } else {
            // External, intrinsic, and constructor calls have no hidden
            // resource ABI arguments; reactive intrinsics resolve their ambient
            // provider inside their reactive operation record.
            Vec::new()
        };
        for index in 0..resource_bindings.len() {
            steps.push(LoweredCallStep::Resource { resource: index });
        }
        steps.push(LoweredCallStep::Invoke);
        let reactive = match &target {
            LoweredCallableTarget::Intrinsic { intrinsic, .. } => {
                self.lower_reactive_intrinsic_operation(module, owner, &origin, call, *intrinsic)?
            }
            _ => None,
        };
        let buffer_pop =
            LoweredOptionAlternatives::for_call(&target, function_type.result.as_ref())
                .map_err(|message| Diagnostic::new(origin.span.clone(), message))?;
        let runtime = LoweredRuntimeCallFacts::for_call(
            &target,
            &arguments,
            &function_type,
            &self.semantic_ids,
        )
        .map_err(|message| Diagnostic::new(origin.span.clone(), message))?;
        let mut arguments = arguments;
        mark_call_temporary_drops(
            &target,
            &function_type,
            &mut arguments,
            |id| {
                self.expressions
                    .get(id)
                    .map(|expression| expression.value_type.clone())
            },
            |value_type| module.type_needs_drop(value_type),
        );
        let call_id = self.calls.push(LoweredCall {
            origin,
            target,
            callee,
            function_type: function_type.clone(),
            arguments,
            resource_bindings,
            initialization_checks,
            steps,
            result_type: function_type.result.as_ref().clone(),
            substitutions,
            evidence,
            reactive,
            buffer_pop,
            runtime,
        });
        Ok(LoweredExpressionKind::Call(call_id))
    }

    /// Lowers a complete juxtaposed call chain from its checked plan. The
    /// outer call owns the whole chain: the plan's consumed inner calls are
    /// marked consumed, the chain root is evaluated once as the callee, and
    /// the plan's ordered arguments fill the flattened parameter slots.
    fn lower_juxtaposed_call(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        call: &staple_syntax::CallExpression,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        let origin = Origin {
            syntax: call.syntax.id,
            span: call.syntax.span.clone(),
        };
        let plan = module
            .juxtaposed_call_plan(call.syntax.id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(origin.span.clone(), "juxtaposed call has no checked plan")
            })?;
        let mut callee = call.callee.as_ref();
        for _ in 1..plan.consumed_calls {
            let Expression::Call(previous) = callee else {
                break;
            };
            self.consumed_calls.insert(previous.syntax.id);
            callee = previous.callee.as_ref();
        }
        let symbol = module.symbol_for(callee.syntax().id);
        let intrinsic = symbol.and_then(|symbol| module.resolved().intrinsic_function(symbol));
        let (target, callee_field, intrinsic_call) = match (intrinsic, symbol) {
            (Some(intrinsic), Some(symbol)) => (
                LoweredCallableTarget::Intrinsic { symbol, intrinsic },
                None,
                true,
            ),
            _ => {
                let callee_id = self.lower_expression(module, owner, context, callee)?;
                (
                    LoweredCallableTarget::IndirectClosure { callee: callee_id },
                    Some(callee_id),
                    false,
                )
            }
        };
        let function_type = plan.function.clone();
        let types = flattened_parameter_types(&function_type.parameter);
        let count = types.len();
        if plan.arguments.len() != count {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!(
                    "juxtaposed call has {} arguments for {count} parameter slots",
                    plan.arguments.len()
                ),
            ));
        }
        let mutation_mask = mutation_slot_mask(count, &function_type.mutations);
        let move_mask = mutation_slot_mask(count, &function_type.moves);
        let indirect = (0..count)
            .map(|index| {
                !intrinsic_call
                    && (mutation_mask[index]
                        || (!move_mask[index] && !module.is_copy_in_function(&types[index], None)))
            })
            .collect::<Vec<_>>();
        let mut arguments = Vec::new();
        let mut steps = Vec::new();
        if let Some(callee) = callee_field {
            steps.push(LoweredCallStep::Callee { expression: callee });
        }
        for (index, argument) in plan.arguments.iter().enumerate() {
            let entry = self.lower_call_argument(
                module,
                owner,
                context,
                argument,
                Some(index),
                &types[index],
                indirect[index],
                mutation_mask[index],
            )?;
            steps.push(call_argument_step(index, index, entry.expression));
            arguments.push(entry);
        }
        let resource_bindings = if !target.records_resources() {
            Vec::new()
        } else {
            self.lower_call_resource_bindings(module, owner, &origin, &function_type.effects)?
        };
        for index in 0..resource_bindings.len() {
            steps.push(LoweredCallStep::Resource { resource: index });
        }
        steps.push(LoweredCallStep::Invoke);
        let reactive = match &target {
            LoweredCallableTarget::Intrinsic { intrinsic, .. } => {
                self.lower_reactive_intrinsic_operation(module, owner, &origin, call, *intrinsic)?
            }
            _ => None,
        };
        let buffer_pop =
            LoweredOptionAlternatives::for_call(&target, function_type.result.as_ref())
                .map_err(|message| Diagnostic::new(origin.span.clone(), message))?;
        let runtime = LoweredRuntimeCallFacts::for_call(
            &target,
            &arguments,
            &function_type,
            &self.semantic_ids,
        )
        .map_err(|message| Diagnostic::new(origin.span.clone(), message))?;
        let mut arguments = arguments;
        mark_call_temporary_drops(
            &target,
            &function_type,
            &mut arguments,
            |id| {
                self.expressions
                    .get(id)
                    .map(|expression| expression.value_type.clone())
            },
            |value_type| module.type_needs_drop(value_type),
        );
        let call_id = self.calls.push(LoweredCall {
            origin,
            target,
            callee: callee_field,
            function_type: function_type.clone(),
            arguments,
            resource_bindings,
            initialization_checks: Vec::new(),
            steps,
            result_type: function_type.result.as_ref().clone(),
            substitutions: CallSubstitutions::default(),
            evidence: None,
            reactive,
            buffer_pop,
            runtime,
        });
        Ok(LoweredExpressionKind::Call(call_id))
    }

    /// Lowers call arguments that the backend passes by value only: external,
    /// intrinsic, and constructor calls.
    fn lower_plain_call_arguments(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        argument: &Expression,
        function_type: &CheckedFunctionType,
    ) -> Result<(Vec<LoweredCallArgument>, Vec<LoweredCallStep>), Diagnostic> {
        let types = flattened_parameter_types(&function_type.parameter);
        self.lower_value_call_arguments(module, owner, context, argument, function_type, &types)
    }

    /// Lowers call arguments with the backend's effect-aware pass modes:
    /// mutation and move markers become mutable place pointers, non-`Copy`
    /// slots become borrowed or materialized pointers, and the rest pass by
    /// value.
    fn lower_effect_call_arguments(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        argument: &Expression,
        function_type: &CheckedFunctionType,
    ) -> Result<(Vec<LoweredCallArgument>, Vec<LoweredCallStep>), Diagnostic> {
        let types = flattened_parameter_types(&function_type.parameter);
        let count = types.len();
        let mutation_mask = mutation_slot_mask(count, &function_type.mutations);
        let move_mask = mutation_slot_mask(count, &function_type.moves);
        let mut indirect = (0..count)
            .map(|index| {
                mutation_mask[index]
                    || (!move_mask[index] && !module.is_copy_in_function(&types[index], None))
            })
            .collect::<Vec<_>>();
        if let Some(actual) = module.type_of_expression(argument.syntax().id) {
            let actual_types = flattened_parameter_types(actual);
            if actual_types.len() == count {
                for (index, actual_type) in actual_types.iter().enumerate() {
                    if !mutation_mask[index] && !move_mask[index] {
                        indirect[index] = !module.is_copy_in_function(actual_type, None);
                    }
                }
            }
        }
        if !indirect.iter().any(|flag| *flag) {
            return self.lower_value_call_arguments(
                module,
                owner,
                context,
                argument,
                function_type,
                &types,
            );
        }
        if function_type.mutations.contains(&CheckedMutation::Whole) {
            let entry = self.lower_call_argument(
                module, owner, context, argument, None, &types[0], true, true,
            )?;
            return Ok((vec![entry], vec![LoweredCallStep::Argument { argument: 0 }]));
        }
        if count == 1 && indirect[0] {
            let entry = self.lower_call_argument(
                module,
                owner,
                context,
                argument,
                Some(0),
                &types[0],
                indirect[0],
                mutation_mask[0],
            )?;
            return Ok((vec![entry], vec![LoweredCallStep::Argument { argument: 0 }]));
        }
        if let Some((placements, steps)) =
            self.place_call_arguments(module, owner, context, argument)?
        {
            if placements.len() != count && !variadic_argument(function_type, placements.len()) {
                return Err(Diagnostic::new(
                    argument.syntax().span.clone(),
                    format!(
                        "call argument has {} slots for {count} parameter slots",
                        placements.len()
                    ),
                ));
            }
            let arguments = placements
                .iter()
                .enumerate()
                .map(|(slot, placement)| {
                    let place = placement.place;
                    let mutation = mutation_mask.get(slot).copied().unwrap_or(false);
                    let indirect_slot = indirect.get(slot).copied().unwrap_or(false);
                    let expected = types.get(slot).cloned().unwrap_or(CheckedType::Error);
                    let pass_mode = if mutation {
                        LoweredArgumentPassMode::MutablePlace
                    } else if indirect_slot {
                        if place.is_some() {
                            LoweredArgumentPassMode::BorrowedPointer
                        } else {
                            LoweredArgumentPassMode::MaterializedTemporary
                        }
                    } else {
                        LoweredArgumentPassMode::Value
                    };
                    LoweredCallArgument {
                        expression: placement.expression,
                        thunk: placement.thunk,
                        slot: Some(slot),
                        pass_mode,
                        expected: expected.clone(),
                        place,
                        temporary: (mutation || indirect_slot) && place.is_none(),
                        drops_after_call: false,
                    }
                })
                .collect();
            return Ok((arguments, steps));
        }
        // A product-valued place passed without literal destructuring: the
        // backend takes addresses of mutated fields and loads the rest.
        if let CheckedType::Product(_) = function_type.parameter.as_ref()
            && expression_has_place_root(module.resolved(), argument)
        {
            let expression = self.lower_expression(module, owner, context, argument)?;
            let place = self.lower_place(module, owner, context, argument).ok();
            let mut arguments = Vec::new();
            for index in 0..count {
                let pass_mode = if mutation_mask[index] {
                    LoweredArgumentPassMode::MutablePlace
                } else if indirect[index] {
                    LoweredArgumentPassMode::BorrowedPointer
                } else {
                    LoweredArgumentPassMode::Value
                };
                arguments.push(LoweredCallArgument {
                    expression: Some(expression),
                    thunk: None,
                    slot: Some(index),
                    pass_mode,
                    expected: types[index].clone(),
                    place,
                    temporary: false,
                    drops_after_call: false,
                });
            }
            return Ok((arguments, vec![LoweredCallStep::Argument { argument: 0 }]));
        }
        Err(Diagnostic::new(
            argument.syntax().span.clone(),
            "mutation-affected product argument cannot be addressed",
        ))
    }

    /// Lowers arguments for a call with no indirect slots: every argument
    /// passes by value, with implicit thunks recorded explicitly.
    fn lower_value_call_arguments(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        argument: &Expression,
        function_type: &CheckedFunctionType,
        types: &[CheckedType],
    ) -> Result<(Vec<LoweredCallArgument>, Vec<LoweredCallStep>), Diagnostic> {
        let count = types.len();
        if let Some((placements, steps)) =
            self.place_call_arguments(module, owner, context, argument)?
        {
            if placements.len() != count && !variadic_argument(function_type, placements.len()) {
                return Err(Diagnostic::new(
                    argument.syntax().span.clone(),
                    format!(
                        "call argument has {} slots for {count} parameter slots",
                        placements.len()
                    ),
                ));
            }
            let argument_types = match module.type_of_expression(argument.syntax().id) {
                Some(CheckedType::Product(product)) => product
                    .elements
                    .iter()
                    .map(|element| element.value_type.clone())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let arguments = placements
                .iter()
                .enumerate()
                .map(|(slot, placement)| LoweredCallArgument {
                    expression: placement.expression,
                    thunk: placement.thunk,
                    slot: Some(slot),
                    pass_mode: LoweredArgumentPassMode::Value,
                    expected: types
                        .get(slot)
                        .cloned()
                        .or_else(|| argument_types.get(slot).cloned())
                        .unwrap_or(CheckedType::Error),
                    place: placement.place,
                    temporary: false,
                    drops_after_call: false,
                })
                .collect();
            return Ok((arguments, steps));
        }
        let entry = self.lower_call_argument(
            module,
            owner,
            context,
            argument,
            (count == 1).then_some(0),
            types.first().unwrap_or(&function_type.parameter),
            false,
            false,
        )?;
        Ok((vec![entry], vec![LoweredCallStep::Argument { argument: 0 }]))
    }

    /// Evaluates source product arguments in order, expands spread slot mappings,
    /// and fills checked defaults in final slot order. Returns None for non-products.
    fn place_call_arguments(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        argument: &Expression,
    ) -> Result<Option<(Vec<CallArgumentPlacement>, Vec<LoweredCallStep>)>, Diagnostic> {
        let plan = module.product_default_plan(argument.syntax().id).cloned();
        let Expression::Product(product) = argument else {
            // A non-product argument checked against a defaulted product
            // parameter initializes its first slot; defaults fill the rest.
            let Some(plan) = plan else {
                return Ok(None);
            };
            let final_type = plan.final_type.clone();
            let mut placements = vec![None; final_type.elements.len()];
            let mut steps = Vec::new();
            let placement = self.lower_argument_value(module, owner, context, argument)?;
            steps.push(call_argument_step(0, 0, placement.expression));
            placements[0] = Some(placement);
            self.fill_call_defaults(
                module,
                owner,
                argument,
                &plan,
                &final_type,
                &mut placements,
                &mut steps,
            )?;
            return Ok(Some((
                placements.into_iter().map(Option::unwrap).collect(),
                steps,
            )));
        };
        let final_type = match module.type_of_expression(argument.syntax().id) {
            Some(CheckedType::Product(product_type)) if !product_type.variadic => {
                product_type.clone()
            }
            _ => match &plan {
                Some(plan) => plan.final_type.clone(),
                None => return Ok(None),
            },
        };
        let mut placements = vec![None; final_type.elements.len()];
        let mut steps = Vec::new();
        let mut positional = 0usize;
        for element in &product.elements {
            if element.designated {
                let name = element.name.clone().ok_or_else(|| {
                    internal_invariant(
                        element.value.syntax().span.clone(),
                        "designated elements always have a name",
                    )
                })?;
                let Some(slot) = final_type
                    .elements
                    .iter()
                    .position(|field| field.name.as_deref() == Some(name.as_str()))
                else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        format!("unknown designated product field `{name}`"),
                    ));
                };
                let placement =
                    self.lower_argument_value(module, owner, context, &element.value)?;
                steps.push(call_argument_step(slot, slot, placement.expression));
                placements[slot] = Some(placement);
                continue;
            }
            if element.spread {
                let expression = self.lower_expression(module, owner, context, &element.value)?;
                let Some(CheckedType::Product(operand)) =
                    module.type_of_expression(element.value.syntax().id)
                else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand does not have a fixed product type",
                    ));
                };
                let operand = operand.clone();
                if element.named_spread {
                    let mut mappings = Vec::new();
                    for (source, field) in operand.elements.iter().enumerate() {
                        let Some(name) = field.name.clone() else {
                            return Err(Diagnostic::new(
                                element.syntax.span.clone(),
                                "a named spread operand must have every element named",
                            ));
                        };
                        let Some(slot) = final_type.elements.iter().position(|final_field| {
                            final_field.name.as_deref() == Some(name.as_str())
                        }) else {
                            return Err(Diagnostic::new(
                                element.syntax.span.clone(),
                                format!("unknown field `{name}` in named product spread"),
                            ));
                        };
                        mappings.push(LoweredNamedSpreadMapping { name, source, slot });
                        placements[slot] = Some(CallArgumentPlacement {
                            expression: Some(expression),
                            thunk: None,
                            place: None,
                        });
                    }
                    if let Some(first) = mappings.first() {
                        steps.push(LoweredCallStep::NamedProductSpread {
                            argument: first.slot,
                            expression,
                            mappings,
                        });
                    }
                } else {
                    let mut mappings = Vec::new();
                    for source in 0..operand.elements.len() {
                        if positional >= final_type.elements.len() {
                            return Err(Diagnostic::new(
                                element.syntax.span.clone(),
                                "too many positional elements in product argument",
                            ));
                        }
                        let slot = positional;
                        positional += 1;
                        mappings.push(LoweredSpreadMapping { source, slot });
                        placements[slot] = Some(CallArgumentPlacement {
                            expression: Some(expression),
                            thunk: None,
                            place: None,
                        });
                    }
                    if let Some(first) = mappings.first() {
                        steps.push(LoweredCallStep::ProductSpread {
                            argument: first.slot,
                            expression,
                            mappings,
                        });
                    }
                }
                continue;
            }
            if positional >= final_type.elements.len() {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    "too many positional elements in product argument",
                ));
            }
            let slot = positional;
            positional += 1;
            let placement = self.lower_argument_value(module, owner, context, &element.value)?;
            steps.push(call_argument_step(slot, slot, placement.expression));
            placements[slot] = Some(placement);
        }
        if let Some(plan) = &plan {
            self.fill_call_defaults(
                module,
                owner,
                argument,
                plan,
                &final_type,
                &mut placements,
                &mut steps,
            )?;
        }
        for (slot, placement) in placements.iter().enumerate() {
            if placement.is_none() {
                return Err(missing_product_slot_error(
                    &final_type,
                    slot,
                    &argument.syntax().span,
                ));
            }
        }
        Ok(Some((
            placements.into_iter().map(Option::unwrap).collect(),
            steps,
        )))
    }

    /// Evaluates a checked product default for every still-empty argument
    /// slot in final order, giving each default its own occurrence key.
    #[allow(clippy::too_many_arguments)]
    fn fill_call_defaults(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        argument: &Expression,
        plan: &crate::CheckedProductDefaultPlan,
        final_type: &CheckedProductType,
        placements: &mut [Option<CallArgumentPlacement>],
        steps: &mut Vec<LoweredCallStep>,
    ) -> Result<(), Diagnostic> {
        for (slot, default) in plan.defaults.iter().enumerate() {
            if placements[slot].is_some() {
                continue;
            }
            let Some(default) = default else {
                return Err(missing_product_slot_error(
                    final_type,
                    slot,
                    &argument.syntax().span,
                ));
            };
            let expected = plan.final_type.elements[slot].value_type.clone();
            let expression = self.lower_expression_occurrence(
                module,
                owner,
                ExpressionContext::ContextualDefault {
                    consumer: argument.syntax().id,
                    slot,
                },
                default,
                Some(expected.clone()),
            )?;
            steps.push(LoweredCallStep::Default {
                argument: slot,
                slot,
                expression,
                expected,
            });
            placements[slot] = Some(CallArgumentPlacement {
                expression: Some(expression),
                thunk: None,
                place: None,
            });
        }
        Ok(())
    }

    /// Lowers one argument element: an implicit thunk keeps its function and
    /// no occurrence; anything else lowers to an expression and an optional
    /// source place.
    fn lower_argument_value(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        expression: &Expression,
    ) -> Result<CallArgumentPlacement, Diagnostic> {
        if let Some(thunk) = module.implicit_thunk_for(expression.syntax().id) {
            return Ok(CallArgumentPlacement {
                expression: None,
                thunk: Some(thunk.id),
                place: None,
            });
        }
        let expression_id = self.lower_expression(module, owner, context, expression)?;
        let place = if expression_has_place_root(module.resolved(), expression) {
            self.lower_place(module, owner, context, expression).ok()
        } else {
            None
        };
        Ok(CallArgumentPlacement {
            expression: Some(expression_id),
            thunk: None,
            place,
        })
    }

    /// Lowers one argument occurrence with its final ABI slot and pass mode.
    #[allow(clippy::too_many_arguments)]
    fn lower_call_argument(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        expression: &Expression,
        slot: Option<usize>,
        expected: &CheckedType,
        indirect: bool,
        mutation: bool,
    ) -> Result<LoweredCallArgument, Diagnostic> {
        let placement = self.lower_argument_value(module, owner, context, expression)?;
        let pass_mode = if mutation {
            LoweredArgumentPassMode::MutablePlace
        } else if indirect {
            if placement.place.is_some() {
                LoweredArgumentPassMode::BorrowedPointer
            } else {
                LoweredArgumentPassMode::MaterializedTemporary
            }
        } else {
            LoweredArgumentPassMode::Value
        };
        let temporary = (mutation || indirect) && placement.place.is_none();
        Ok(LoweredCallArgument {
            expression: placement.expression,
            thunk: placement.thunk,
            slot,
            pass_mode,
            expected: expected.clone(),
            place: placement.place,
            temporary,
            // Decided for the whole call by `mark_call_temporary_drops`.
            drops_after_call: false,
        })
    }

    fn validate(&self) -> Vec<Diagnostic> {
        let mut diagnostics = self.modules.validate("module");
        diagnostics.extend(self.functions.validate("function"));
        diagnostics.extend(self.symbols.validate("symbol"));
        diagnostics.extend(self.types.validate("type"));
        diagnostics.extend(self.traits.validate("trait"));
        diagnostics.extend(self.trait_methods.validate("trait method"));
        diagnostics.extend(self.validate_occurrence_lookups());
        diagnostics.extend(self.validate_arena_identity());
        diagnostics.extend(self.validate_function_body_ownership());
        diagnostics.extend(self.validate_concrete_metadata());
        diagnostics.extend(self.validate_capture_consistency());
        diagnostics.extend(self.validate_arena_references());
        diagnostics.extend(self.validate_ownership());
        diagnostics.extend(self.validate_modules_and_initializers());
        diagnostics.extend(self.validate_functions());
        diagnostics.extend(self.validate_symbols());
        diagnostics.extend(self.validate_types());
        diagnostics.extend(self.validate_traits());
        diagnostics.extend(validate_semantic_ids(
            &self.semantic_ids,
            &self.types,
            &self.traits,
        ));
        diagnostics.extend(self.validate_string_formatting());
        diagnostics.extend(self.validate_calls_and_callable_values());
        diagnostics.extend(self.validate_resource_and_coroutine_records());
        diagnostics
    }

    /// Proves every runtime source construct has exactly one lowered
    /// counterpart by walking the checked program's functions, module
    /// initializers, items, expressions, patterns, and assignment places. A
    /// construct that survives only in the checked `TypedModule` payload
    /// diagnoses here instead of surfacing during emission.
    fn validate_source_coverage(&self, module: &TypedModule) -> Vec<Diagnostic> {
        SourceCoverage::run(self, module)
    }

    /// Checks the lowering resource provider/use, `with`, reactive, and
    /// coroutine arenas against the catalogs and each other, rejecting dangling
    /// records and inconsistent provider or callback facts.
    fn validate_resource_and_coroutine_records(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, provider) in self.resource_providers.iter() {
            if let Some(parent) = provider.parent
                && !self.resource_providers.contains(parent)
            {
                diagnostics.push(invalid_reference(
                    &provider.origin,
                    "resource provider",
                    "provider",
                    parent.index(),
                ));
            }
            let target_kind = match provider.target {
                LoweredProviderTarget::Expression(expression) => {
                    if !self.expressions.contains(expression) {
                        diagnostics.push(invalid_reference(
                            &provider.origin,
                            "resource provider",
                            "expression",
                            expression.index(),
                        ));
                    }
                    LoweredProviderOriginKind::Source
                }
                LoweredProviderTarget::EffectParameter { .. } => {
                    LoweredProviderOriginKind::FunctionParameter
                }
                LoweredProviderTarget::Entry => LoweredProviderOriginKind::EntryParameter,
            };
            if provider.kind != target_kind {
                diagnostics.push(Diagnostic::new(
                    provider.origin.span.clone(),
                    "resource provider origin and target disagree",
                ));
            }
            if provider.kind == LoweredProviderOriginKind::FunctionParameter
                && provider.scope_exit != LoweredScopeExit::Ordinary
            {
                diagnostics.push(Diagnostic::new(
                    provider.origin.span.clone(),
                    "function effect parameter cannot own resource scope cleanup",
                ));
            }
            if let Some(parent) = provider.parent {
                match self.resource_providers.get(parent) {
                    Some(parent) if parent.owner == provider.owner => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        provider.origin.span.clone(),
                        "resource provider nests across two lexical owners",
                    )),
                    None => {}
                }
            }
        }
        for (_, use_) in self.resource_uses.iter() {
            match use_.provider {
                Some(provider) => match self.resource_providers.get(provider) {
                    Some(record) => {
                        if record.resource.value_type != use_.resource.value_type {
                            diagnostics.push(Diagnostic::new(
                                use_.origin.span.clone(),
                                format!(
                                    "resource use `{}` selected a provider for `{}`",
                                    use_.resource.value_type, record.resource.value_type
                                ),
                            ));
                        }
                        if use_.indirect != record.indirect {
                            diagnostics.push(Diagnostic::new(
                                use_.origin.span.clone(),
                                "resource use indirectness disagrees with its provider",
                            ));
                        }
                    }
                    None => diagnostics.push(invalid_reference(
                        &use_.origin,
                        "resource use",
                        "provider",
                        provider.index(),
                    )),
                },
                None => {}
            }
        }
        for (_, with) in self.withs.iter() {
            if !self.resource_providers.contains(with.provider) {
                diagnostics.push(invalid_reference(
                    &with.origin,
                    "with",
                    "provider",
                    with.provider.index(),
                ));
            }
            if !self.expressions.contains(with.value) {
                diagnostics.push(invalid_reference(
                    &with.origin,
                    "with",
                    "expression",
                    with.value.index(),
                ));
            }
            if !self.blocks.contains(with.body) {
                diagnostics.push(invalid_reference(
                    &with.origin,
                    "with",
                    "block",
                    with.body.index(),
                ));
            }
            match (
                self.resource_providers.get(with.provider),
                self.expressions.get(with.value),
            ) {
                (Some(provider), Some(value)) => {
                    if provider.kind != LoweredProviderOriginKind::Source {
                        diagnostics.push(Diagnostic::new(
                            with.origin.span.clone(),
                            "lowered `with` references a non-source provider",
                        ));
                    }
                    if !types_agree(&provider.resource.value_type, &value.value_type) {
                        diagnostics.push(Diagnostic::new(
                            with.origin.span.clone(),
                            format!(
                                "lowered `with` provider is `{}` but its value is `{}`",
                                provider.resource.value_type, value.value_type
                            ),
                        ));
                    }
                    if provider.scope_exit != with.scope_exit {
                        diagnostics.push(Diagnostic::new(
                            with.origin.span.clone(),
                            "lowered `with` scope-exit classification disagrees with its provider",
                        ));
                    }
                }
                _ => {}
            }
        }
        for (_, callback) in self.reactive_callbacks.iter() {
            if callback.thunk.is_some() == callback.callable.is_some() {
                diagnostics.push(Diagnostic::new(
                    callback.origin.span.clone(),
                    "reactive callback must be exactly one of an implicit thunk or a callable occurrence",
                ));
            }
            if let Some(thunk) = callback.thunk {
                match self.functions.get(thunk) {
                    Some(function) => {
                        if function.signature != callback.function_type {
                            diagnostics.push(Diagnostic::new(
                                callback.origin.span.clone(),
                                "reactive callback type disagrees with its thunk signature",
                            ));
                        }
                        if function.captures.len() != callback.captures.len()
                            || function
                                .captures
                                .iter()
                                .zip(&callback.captures)
                                .any(|(catalog, capture)| catalog.symbol != capture.symbol)
                        {
                            diagnostics.push(Diagnostic::new(
                                callback.origin.span.clone(),
                                "reactive callback captures disagree with its thunk",
                            ));
                        }
                    }
                    None => diagnostics.push(invalid_reference(
                        &callback.origin,
                        "reactive callback",
                        "function",
                        thunk.0,
                    )),
                }
            }
            if let Some(callable) = callback.callable
                && !self.expressions.contains(callable)
            {
                diagnostics.push(invalid_reference(
                    &callback.origin,
                    "reactive callback",
                    "expression",
                    callable.index(),
                ));
            }
            for use_ in &callback.resources {
                match self.resource_uses.get(*use_) {
                    Some(record) if record.kind == LoweredResourceUseKind::HiddenArgument => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        callback.origin.span.clone(),
                        "reactive callback resource is not a hidden argument",
                    )),
                    None => diagnostics.push(invalid_reference(
                        &callback.origin,
                        "reactive callback",
                        "resource use",
                        use_.index(),
                    )),
                }
            }
            for capture in &callback.captures {
                if self.symbols.get(capture.symbol).is_none() {
                    diagnostics.push(invalid_reference(
                        &callback.origin,
                        "reactive callback",
                        "symbol",
                        capture.symbol.0,
                    ));
                }
            }
        }
        diagnostics.extend(self.validate_reactive_attachments());
        for (_, operation) in self.reactive_operations.iter() {
            match &operation.kind {
                LoweredReactiveOperationKind::SignalCreate { symbol, storage } => {
                    match self.symbols.get(*symbol) {
                        Some(record) if record.signal => {
                            // Module-level signals own their global; every
                            // other signal lives in a binding cell.
                            let expected = if record.owner.is_none() {
                                LoweredSignalStorage::Global
                            } else {
                                LoweredSignalStorage::LocalCell
                            };
                            if *storage != expected {
                                diagnostics.push(Diagnostic::new(
                                    operation.origin.span.clone(),
                                    "signal creation storage disagrees with the symbol's storage",
                                ));
                            }
                        }
                        Some(_) => diagnostics.push(Diagnostic::new(
                            operation.origin.span.clone(),
                            "signal creation names a non-signal symbol",
                        )),
                        None => diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "symbol",
                            symbol.0,
                        )),
                    }
                }
                LoweredReactiveOperationKind::SignalRead { symbol }
                | LoweredReactiveOperationKind::SignalNotify { symbol } => {
                    match self.symbols.get(*symbol) {
                        Some(record) if record.signal => {}
                        Some(_) => diagnostics.push(Diagnostic::new(
                            operation.origin.span.clone(),
                            "signal read/notify names a non-signal symbol",
                        )),
                        None => diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "symbol",
                            symbol.0,
                        )),
                    }
                }
                LoweredReactiveOperationKind::DerivedRead { symbol } => {
                    match self.symbols.get(*symbol) {
                        Some(record) if record.derived => {}
                        Some(_) => diagnostics.push(Diagnostic::new(
                            operation.origin.span.clone(),
                            "derived read names a non-derived symbol",
                        )),
                        None => diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "symbol",
                            symbol.0,
                        )),
                    }
                }
                LoweredReactiveOperationKind::DerivedCreate {
                    symbol,
                    evaluator,
                    function_type,
                    captures,
                } => {
                    match self.symbols.get(*symbol) {
                        Some(record) if record.derived => {}
                        Some(_) => diagnostics.push(Diagnostic::new(
                            operation.origin.span.clone(),
                            "derived creation names a non-derived symbol",
                        )),
                        None => diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "symbol",
                            symbol.0,
                        )),
                    }
                    match self.functions.get(*evaluator) {
                        Some(function) => {
                            if function.signature != *function_type {
                                diagnostics.push(Diagnostic::new(
                                    operation.origin.span.clone(),
                                    "derived evaluator type disagrees with its function catalog entry",
                                ));
                            }
                            if function.captures.len() != captures.len()
                                || function
                                    .captures
                                    .iter()
                                    .zip(captures)
                                    .any(|(catalog, capture)| catalog.symbol != capture.symbol)
                            {
                                diagnostics.push(Diagnostic::new(
                                    operation.origin.span.clone(),
                                    "derived captures disagree with the evaluator function",
                                ));
                            }
                        }
                        None => diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "function",
                            evaluator.0,
                        )),
                    }
                    if !function_type.effects.resources.is_empty() {
                        diagnostics.push(Diagnostic::new(
                            operation.origin.span.clone(),
                            "derived evaluators cannot capture resources",
                        ));
                    }
                    for capture in captures {
                        if self.symbols.get(capture.symbol).is_none() {
                            diagnostics.push(invalid_reference(
                                &operation.origin,
                                "reactive operation",
                                "symbol",
                                capture.symbol.0,
                            ));
                        }
                    }
                }
                LoweredReactiveOperationKind::Scope | LoweredReactiveOperationKind::Snapshot => {}
                LoweredReactiveOperationKind::Reaction {
                    callback,
                    reactive_provider,
                } => {
                    if !self.reactive_callbacks.contains(*callback) {
                        diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "callback",
                            callback.index(),
                        ));
                    }
                    if let Some(provider) = reactive_provider
                        && !self.resource_providers.contains(*provider)
                    {
                        diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "provider",
                            provider.index(),
                        ));
                    }
                }
                LoweredReactiveOperationKind::Until {
                    predicate,
                    reactive_provider,
                } => {
                    match self.reactive_callbacks.get(*predicate) {
                        Some(callback) => {
                            // Pure apart from reading signals.
                            if !callback.function_type.effects.resources.is_empty()
                                || matches!(
                                    callback.function_type.effects.state,
                                    Some(
                                        crate::CheckedStateEffect::Write
                                            | crate::CheckedStateEffect::ReadWrite
                                    )
                                )
                            {
                                diagnostics.push(Diagnostic::new(
                                    operation.origin.span.clone(),
                                    "an `until` predicate must be pure apart from reading signals",
                                ));
                            }
                        }
                        None => diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "callback",
                            predicate.index(),
                        )),
                    }
                    if let Some(provider) = reactive_provider
                        && !self.resource_providers.contains(*provider)
                    {
                        diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "provider",
                            provider.index(),
                        ));
                    }
                }
                LoweredReactiveOperationKind::Batch { callback } => {
                    if !self.reactive_callbacks.contains(*callback) {
                        diagnostics.push(invalid_reference(
                            &operation.origin,
                            "reactive operation",
                            "callback",
                            callback.index(),
                        ));
                    }
                }
            }
        }
        for (plan_id, plan) in self.coroutine_plans.iter() {
            match plan.body {
                Some(body) if !self.blocks.contains(body) => diagnostics.push(invalid_reference(
                    &plan.origin,
                    "coroutine plan",
                    "block",
                    body.index(),
                )),
                Some(_) => {}
                None => diagnostics.push(Diagnostic::new(
                    plan.origin.span.clone(),
                    "coroutine plan has no linked body block",
                )),
            }
            let mut thunk_captures: Option<&[LoweredCapture]> = None;
            match self.functions.get(plan.thunk) {
                Some(thunk) => {
                    thunk_captures = Some(&thunk.captures);
                    if thunk.captures.len() != plan.captures.len() {
                        diagnostics.push(Diagnostic::new(
                            plan.origin.span.clone(),
                            format!(
                                "coroutine plan has {} captures for a body thunk with {}",
                                plan.captures.len(),
                                thunk.captures.len()
                            ),
                        ));
                    }
                    for (catalog, capture) in thunk.captures.iter().zip(&plan.captures) {
                        if catalog.symbol != capture.symbol {
                            diagnostics.push(Diagnostic::new(
                                plan.origin.span.clone(),
                                format!(
                                    "coroutine capture {} disagrees with body thunk capture {}",
                                    capture.symbol.0, catalog.symbol.0
                                ),
                            ));
                        }
                    }
                    if plan.body != thunk.body || plan.body.is_none() {
                        diagnostics.push(Diagnostic::new(
                            plan.origin.span.clone(),
                            "coroutine plan body disagrees with its thunk body block",
                        ));
                    }
                    if thunk.body_syntax != plan.body_syntax {
                        diagnostics.push(Diagnostic::new(
                            plan.origin.span.clone(),
                            "coroutine plan body syntax disagrees with its thunk",
                        ));
                    }
                }
                None => diagnostics.push(invalid_reference(
                    &plan.origin,
                    "coroutine plan",
                    "function",
                    plan.thunk.0,
                )),
            }
            let mut frame_owner = None;
            for symbol in &plan.frame_bindings {
                match self.symbols.get(*symbol) {
                    Some(record) => {
                        if thunk_captures
                            .is_some_and(|captures| captures.iter().any(|c| c.symbol == *symbol))
                        {
                            diagnostics.push(Diagnostic::new(
                                plan.origin.span.clone(),
                                format!(
                                    "coroutine frame binding {} is also a thunk capture",
                                    symbol.0
                                ),
                            ));
                        }
                        match frame_owner {
                            None => frame_owner = Some(record.owner),
                            Some(owner) if owner != record.owner => {
                                diagnostics.push(Diagnostic::new(
                                    plan.origin.span.clone(),
                                    "coroutine frame bindings span multiple owners",
                                ))
                            }
                            Some(_) => {}
                        }
                    }
                    None => diagnostics.push(invalid_reference(
                        &plan.origin,
                        "coroutine plan",
                        "symbol",
                        symbol.0,
                    )),
                }
            }
            if plan.await_result_types.len() != plan.resume_points {
                diagnostics.push(Diagnostic::new(
                    plan.origin.span.clone(),
                    format!(
                        "coroutine plan has {} resume points but {} awaited result types",
                        plan.resume_points,
                        plan.await_result_types.len()
                    ),
                ));
            }
            if plan.awaits.len() != plan.resume_points {
                diagnostics.push(Diagnostic::new(
                    plan.origin.span.clone(),
                    format!(
                        "coroutine plan has {} awaits for {} resume points",
                        plan.awaits.len(),
                        plan.resume_points
                    ),
                ));
            }
            for (index, await_) in plan.awaits.iter().enumerate() {
                match self.awaits.get(*await_) {
                    Some(record) if record.owning_plan == plan_id => {
                        if record.resume_state != index + 1 {
                            diagnostics.push(Diagnostic::new(
                                record.origin.span.clone(),
                                format!(
                                    "await at plan position {index} records resume state {}",
                                    record.resume_state
                                ),
                            ));
                        }
                    }
                    Some(record) => diagnostics.push(Diagnostic::new(
                        record.origin.span.clone(),
                        "await site is listed in a plan it does not own",
                    )),
                    None => diagnostics.push(invalid_reference(
                        &plan.origin,
                        "coroutine plan",
                        "await",
                        await_.index(),
                    )),
                }
            }
            for state in plan
                .wait_await_states
                .iter()
                .chain(&plan.until_await_states)
            {
                if *state == 0 || *state > plan.resume_points {
                    diagnostics.push(Diagnostic::new(
                        plan.origin.span.clone(),
                        format!(
                            "coroutine plan cancellation state {state} is outside 1..={}",
                            plan.resume_points
                        ),
                    ));
                }
            }
            for await_ in &plan.awaits {
                if !self.awaits.contains(*await_) {
                    diagnostics.push(invalid_reference(
                        &plan.origin,
                        "coroutine plan",
                        "await",
                        await_.index(),
                    ));
                }
            }
        }
        for (_, coro) in self.coros.iter() {
            if !self.coroutine_plans.contains(coro.plan) {
                diagnostics.push(invalid_reference(
                    &coro.origin,
                    "coro",
                    "plan",
                    coro.plan.index(),
                ));
            }
        }
        for (_, await_) in self.awaits.iter() {
            if !self.expressions.contains(await_.operand) {
                diagnostics.push(invalid_reference(
                    &await_.origin,
                    "await",
                    "expression",
                    await_.operand.index(),
                ));
            }
            match self.coroutine_plans.get(await_.owning_plan) {
                Some(plan) => {
                    if await_.resume_state == 0 || await_.resume_state > plan.resume_points {
                        diagnostics.push(Diagnostic::new(
                            await_.origin.span.clone(),
                            format!(
                                "await resume state {} is outside 1..={}",
                                await_.resume_state, plan.resume_points
                            ),
                        ));
                        continue;
                    }
                    let state = await_.resume_state;
                    let wait_state = plan.wait_await_states.contains(&state);
                    let until_state = plan.until_await_states.contains(&state);
                    let expected = match &await_.kind {
                        LoweredAwaitKind::Task { .. } => {
                            (!wait_state && !until_state, "an external task")
                        }
                        LoweredAwaitKind::Wait { .. } => {
                            (wait_state && !until_state, "an external wait")
                        }
                        LoweredAwaitKind::ChildCoroutine { until, .. } => {
                            ((*until) == until_state && !wait_state, "a child coroutine")
                        }
                    };
                    if !expected.0 {
                        diagnostics.push(Diagnostic::new(
                            await_.origin.span.clone(),
                            format!(
                                "await at resume state {state} is not classified as {}",
                                expected.1
                            ),
                        ));
                    }
                    if let Some(expected_type) = plan.await_result_types.get(state - 1)
                        && !types_agree(expected_type, &await_.result_type)
                    {
                        diagnostics.push(Diagnostic::new(
                            await_.origin.span.clone(),
                            format!(
                                "await result `{}` disagrees with the plan's awaited type `{expected_type}`",
                                await_.result_type
                            ),
                        ));
                    }
                }
                None => diagnostics.push(invalid_reference(
                    &await_.origin,
                    "await",
                    "plan",
                    await_.owning_plan.index(),
                )),
            }
            if let LoweredAwaitKind::ChildCoroutine {
                plan,
                deferred_resources,
                ..
            } = &await_.kind
            {
                if let Some(plan) = plan
                    && !self.coroutine_plans.contains(*plan)
                {
                    diagnostics.push(invalid_reference(
                        &await_.origin,
                        "await",
                        "plan",
                        plan.index(),
                    ));
                }
                for use_ in deferred_resources {
                    if !self.resource_uses.contains(*use_) {
                        diagnostics.push(invalid_reference(
                            &await_.origin,
                            "await",
                            "resource use",
                            use_.index(),
                        ));
                    }
                }
            }
        }
        diagnostics
    }

    /// Checks that every signal/derived site and every reactive intrinsic call
    /// carries an operation of the matching kind for its symbol or intrinsic.
    fn validate_reactive_attachments(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, item) in self.items.iter() {
            match &item.kind {
                LoweredItemKind::Binding(binding) => match (binding.reactive, binding.symbol) {
                    (Some(operation), Some(symbol)) => {
                        match self.reactive_operations.get(operation).map(|o| &o.kind) {
                            Some(LoweredReactiveOperationKind::SignalCreate {
                                symbol: target,
                                ..
                            }) if *target == symbol && binding.signal => {}
                            Some(LoweredReactiveOperationKind::DerivedCreate {
                                symbol: target,
                                ..
                            }) if *target == symbol && binding.derived => {}
                            Some(_) => diagnostics.push(Diagnostic::new(
                                item.origin.span.clone(),
                                "binding reactive operation disagrees with its symbol",
                            )),
                            None => diagnostics.push(invalid_reference(
                                &item.origin,
                                "binding",
                                "reactive operation",
                                operation.index(),
                            )),
                        }
                    }
                    (Some(_), None) => diagnostics.push(Diagnostic::new(
                        item.origin.span.clone(),
                        "compile-time-only binding owns a reactive operation",
                    )),
                    (None, Some(_)) if binding.signal || binding.derived => {
                        diagnostics.push(Diagnostic::new(
                            item.origin.span.clone(),
                            "signal/derived binding has no reactive creation operation",
                        ));
                    }
                    (None, _) => {}
                },
                LoweredItemKind::Assignment(assignment) => {
                    match (assignment.signal_notify, assignment.initialization_symbol) {
                        (Some(operation), Some(symbol)) => {
                            match self.reactive_operations.get(operation).map(|o| &o.kind) {
                                Some(LoweredReactiveOperationKind::SignalNotify {
                                    symbol: target,
                                }) if *target == symbol => {}
                                Some(_) => diagnostics.push(Diagnostic::new(
                                    item.origin.span.clone(),
                                    "assignment notification disagrees with its signal symbol",
                                )),
                                None => diagnostics.push(invalid_reference(
                                    &item.origin,
                                    "assignment",
                                    "reactive operation",
                                    operation.index(),
                                )),
                            }
                        }
                        (Some(_), None) => diagnostics.push(Diagnostic::new(
                            item.origin.span.clone(),
                            "assignment notification has no signal root",
                        )),
                        (None, _) => {}
                    }
                }
                _ => {}
            }
        }
        for (_, expression) in self.expressions.iter() {
            let LoweredExpressionKind::Name(name) = &expression.kind else {
                continue;
            };
            let derived = self
                .symbols
                .get(name.symbol)
                .is_some_and(|symbol| symbol.derived);
            match name.reactive {
                Some(operation) => match self.reactive_operations.get(operation).map(|o| &o.kind) {
                    Some(LoweredReactiveOperationKind::SignalRead { symbol })
                        if *symbol == name.symbol && !derived => {}
                    Some(LoweredReactiveOperationKind::DerivedRead { symbol })
                        if *symbol == name.symbol && derived => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        expression.origin.span.clone(),
                        "name reactive operation disagrees with its symbol",
                    )),
                    None => diagnostics.push(invalid_reference(
                        &expression.origin,
                        "name",
                        "reactive operation",
                        operation.index(),
                    )),
                },
                None => {}
            }
        }
        for (_, call) in self.calls.iter() {
            let expected = match &call.target {
                LoweredCallableTarget::Intrinsic { intrinsic, .. } => intrinsic_route(*intrinsic)
                    .and_then(|route| match route {
                        IntrinsicRoute::Reactive(route) => Some(route),
                        IntrinsicRoute::Coroutine(_) => None,
                    }),
                _ => None,
            };
            match (call.reactive, expected) {
                (Some(operation), Some(expected)) => {
                    match self.reactive_operations.get(operation).map(|o| &o.kind) {
                        Some(LoweredReactiveOperationKind::Scope)
                            if expected == ReactiveIntrinsicRoute::Scope => {}
                        Some(LoweredReactiveOperationKind::Snapshot)
                            if expected == ReactiveIntrinsicRoute::Snapshot => {}
                        Some(LoweredReactiveOperationKind::Reaction { .. })
                            if expected == ReactiveIntrinsicRoute::Reaction => {}
                        Some(LoweredReactiveOperationKind::Batch { .. })
                            if expected == ReactiveIntrinsicRoute::Batch => {}
                        Some(LoweredReactiveOperationKind::Until { .. })
                            if expected == ReactiveIntrinsicRoute::Until => {}
                        Some(_) => diagnostics.push(Diagnostic::new(
                            call.origin.span.clone(),
                            "call reactive operation disagrees with its intrinsic",
                        )),
                        None => diagnostics.push(invalid_reference(
                            &call.origin,
                            "call",
                            "reactive operation",
                            operation.index(),
                        )),
                    }
                }
                (Some(_), None) => diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    "non-reactive call owns a reactive operation",
                )),
                (None, Some(_)) => diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    "reactive intrinsic call has no reactive operation",
                )),
                (None, None) => {}
            }
        }
        diagnostics
    }

    /// Checks every call and callable value against the semantic catalogs and
    /// the arena children they own: targets, callee/argument occurrences,
    /// ABI slots, ordered steps, closure captures, and trait evidence.
    fn validate_calls_and_callable_values(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, call) in self.calls.iter() {
            self.validate_callable_target(&call.origin, &call.target, &mut diagnostics);
            if let Some(callee) = call.callee
                && !self.expressions.contains(callee)
            {
                diagnostics.push(invalid_reference(
                    &call.origin,
                    "call",
                    "expression",
                    callee.index(),
                ));
            }
            let mut slots = HashSet::new();
            for argument in &call.arguments {
                if let Some(expression) = argument.expression
                    && !self.expressions.contains(expression)
                {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call argument",
                        "expression",
                        expression.index(),
                    ));
                }
                if let Some(thunk) = argument.thunk
                    && self.functions.get(thunk).is_none()
                {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call argument",
                        "function",
                        thunk.0,
                    ));
                }
                if let Some(place) = argument.place
                    && !self.places.contains(place)
                {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call argument",
                        "place",
                        place.index(),
                    ));
                }
                if let Some(slot) = argument.slot
                    && !slots.insert(slot)
                {
                    diagnostics.push(Diagnostic::new(
                        call.origin.span.clone(),
                        format!("call argument slot {slot} is filled more than once"),
                    ));
                }
            }
            for step in &call.steps {
                let expression = match step {
                    LoweredCallStep::Callee { expression } => Some(*expression),
                    LoweredCallStep::Argument { argument } => {
                        self.check_call_step_argument(
                            &call.origin,
                            *argument,
                            call.arguments.len(),
                            &mut diagnostics,
                        );
                        None
                    }
                    LoweredCallStep::ProductElement {
                        argument,
                        expression,
                        ..
                    }
                    | LoweredCallStep::ProductSpread {
                        argument,
                        expression,
                        ..
                    }
                    | LoweredCallStep::NamedProductSpread {
                        argument,
                        expression,
                        ..
                    }
                    | LoweredCallStep::Default {
                        argument,
                        expression,
                        ..
                    } => {
                        self.check_call_step_argument(
                            &call.origin,
                            *argument,
                            call.arguments.len(),
                            &mut diagnostics,
                        );
                        Some(*expression)
                    }
                    LoweredCallStep::Resource { resource } => {
                        if *resource >= call.resource_bindings.len() {
                            diagnostics.push(Diagnostic::new(
                                call.origin.span.clone(),
                                format!(
                                    "call step targets out-of-range resource {resource} of {}",
                                    call.resource_bindings.len()
                                ),
                            ));
                        }
                        None
                    }
                    LoweredCallStep::Invoke => None,
                };
                if let Some(expression) = expression
                    && !self.expressions.contains(expression)
                {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call step",
                        "expression",
                        expression.index(),
                    ));
                }
            }
            self.validate_call_step_sequence(call, &mut diagnostics);
            for symbol in &call.initialization_checks {
                if self.symbols.get(*symbol).is_none() {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call initialization check",
                        "symbol",
                        symbol.0,
                    ));
                }
            }
            if let Some(evidence) = &call.evidence {
                self.validate_trait_evidence(&call.origin, evidence, &mut diagnostics);
            }
            self.validate_call_target_agreement(call, &mut diagnostics);
        }
        for (_, value) in self.callable_values.iter() {
            self.validate_callable_target(&value.origin, &value.target, &mut diagnostics);
            if let Some(closure) = &value.closure {
                match self.functions.get(closure.function) {
                    Some(function) => {
                        if function.captures.len() != closure.captures.len() {
                            diagnostics.push(Diagnostic::new(
                                value.origin.span.clone(),
                                format!(
                                    "closure construction has {} captures for a function with {}",
                                    closure.captures.len(),
                                    function.captures.len()
                                ),
                            ));
                        }
                        for (catalog, capture) in function.captures.iter().zip(&closure.captures) {
                            if catalog.symbol != capture.capture.symbol {
                                diagnostics.push(Diagnostic::new(
                                    value.origin.span.clone(),
                                    format!(
                                        "closure capture {} disagrees with catalog capture {}",
                                        capture.capture.symbol.0, catalog.symbol.0
                                    ),
                                ));
                            }
                            if self.symbols.get(capture.capture.symbol).is_none() {
                                diagnostics.push(invalid_reference(
                                    &value.origin,
                                    "closure capture",
                                    "symbol",
                                    capture.capture.symbol.0,
                                ));
                            }
                        }
                    }
                    None => diagnostics.push(invalid_reference(
                        &value.origin,
                        "closure construction",
                        "function",
                        closure.function.0,
                    )),
                }
            }
            if let Some(evidence) = &value.evidence {
                self.validate_trait_evidence(&value.origin, evidence, &mut diagnostics);
            }
        }
        diagnostics
    }

    /// Checks that a call's callee occurrence, target category, evidence
    /// recipe, resource order, argument slot layout, and substitutions are
    /// mutually consistent.
    fn validate_call_target_agreement(
        &self,
        call: &LoweredCall,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match (&call.target, call.callee) {
            (LoweredCallableTarget::IndirectClosure { callee }, Some(field)) => {
                if *callee != field {
                    diagnostics.push(Diagnostic::new(
                        call.origin.span.clone(),
                        "indirect call target disagrees with its callee occurrence",
                    ));
                }
            }
            (LoweredCallableTarget::IndirectClosure { .. }, None) => {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    "indirect call target has no callee occurrence",
                ));
            }
            (_, Some(_)) => {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    "direct call target owns a callee occurrence",
                ));
            }
            (_, None) => {}
        }

        match (&call.target, &call.evidence) {
            (
                LoweredCallableTarget::TraitImplementation {
                    trait_id,
                    method,
                    function,
                },
                Some(TraitEvidence::ExplicitImplementation {
                    trait_id: evidence_trait,
                    method: evidence_method,
                    function: evidence_function,
                    ..
                }),
            ) if trait_id == evidence_trait
                && method == evidence_method
                && function == &Some(*evidence_function) => {}
            (
                LoweredCallableTarget::StructuralTraitMethod {
                    trait_id,
                    method,
                    structural,
                },
                Some(TraitEvidence::Structural {
                    trait_id: evidence_trait,
                    method: evidence_method,
                    structural: evidence_structural,
                    ..
                }),
            ) if trait_id == evidence_trait
                && method == evidence_method
                && structural == evidence_structural => {}
            (
                LoweredCallableTarget::TraitImplementation {
                    trait_id, method, ..
                },
                Some(TraitEvidence::DeclaredBound {
                    trait_id: evidence_trait,
                    method: evidence_method,
                    ..
                }),
            ) if trait_id == evidence_trait && Some(*method) == *evidence_method => {}
            (LoweredCallableTarget::TraitImplementation { .. }, _) => {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    "trait implementation call has no matching evidence recipe",
                ));
            }
            (LoweredCallableTarget::StructuralTraitMethod { .. }, _) => {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    "structural trait call has no matching evidence recipe",
                ));
            }
            (_, Some(_)) => {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    "non-trait call owns a trait evidence recipe",
                ));
            }
            (_, None) => {}
        }

        if LoweredRuntimeCallFacts::for_call(
            &call.target,
            &call.arguments,
            &call.function_type,
            &self.semantic_ids,
        )
        .ok()
        .as_ref()
            != Some(&call.runtime)
        {
            diagnostics.push(Diagnostic::new(
                call.origin.span.clone(),
                "call runtime facts disagree with its checked types",
            ));
        }

        let expected_resources = &call.function_type.effects.resources;
        let mut bound_resources = Vec::with_capacity(call.resource_bindings.len());
        for binding in &call.resource_bindings {
            match self.resource_uses.get(*binding) {
                Some(use_) => bound_resources.push(&use_.resource),
                None => diagnostics.push(invalid_reference(
                    &call.origin,
                    "call",
                    "resource use",
                    binding.index(),
                )),
            }
        }
        let passes_hidden_resources = call.target.records_resources();
        let agrees = if passes_hidden_resources {
            bound_resources.len() == expected_resources.len()
                && bound_resources
                    .iter()
                    .zip(expected_resources)
                    .all(|(bound, expected)| **bound == *expected)
        } else {
            call.resource_bindings.is_empty()
        };
        if !agrees {
            diagnostics.push(Diagnostic::new(
                call.origin.span.clone(),
                "call resources disagree with its checked effect row",
            ));
        }

        let parameter_slots = flattened_parameter_types(&call.function_type.parameter).len();
        let variadic = matches!(
            call.function_type.parameter.as_ref(),
            CheckedType::Product(product) if product.variadic
        );
        let mut slotted = call
            .arguments
            .iter()
            .filter_map(|argument| argument.slot)
            .collect::<Vec<_>>();
        if slotted.len() == call.arguments.len() {
            slotted.sort_unstable();
            if variadic {
                if slotted.len() < parameter_slots {
                    diagnostics.push(Diagnostic::new(
                        call.origin.span.clone(),
                        format!(
                            "variadic call fills {} slots for a {parameter_slots}-slot fixed prefix",
                            slotted.len()
                        ),
                    ));
                }
            } else if slotted != (0..parameter_slots).collect::<Vec<_>>() {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    format!(
                        "call argument slots do not fill the {parameter_slots} parameter slots exactly once"
                    ),
                ));
            }
        }

        let mut substituted = HashSet::new();
        for parameter in call
            .substitutions
            .types
            .iter()
            .map(|substitution| substitution.parameter)
            .chain(
                call.substitutions
                    .effects
                    .iter()
                    .map(|substitution| substitution.parameter),
            )
        {
            if !substituted.insert(parameter) {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    format!("call substitution repeats type parameter {}", parameter.0),
                ));
            }
        }
    }

    /// Check that the ordered program actually evaluates every input before
    /// its single invocation, rather than merely containing valid references.
    fn validate_call_step_sequence(&self, call: &LoweredCall, diagnostics: &mut Vec<Diagnostic>) {
        let mut covered = vec![0usize; call.arguments.len()];
        let mut resources = Vec::new();
        let mut callee_count = 0usize;
        let mut invoked = false;
        let mut resources_started = false;
        for (index, step) in call.steps.iter().enumerate() {
            match step {
                LoweredCallStep::Callee { expression } => {
                    callee_count += 1;
                    if index != 0 || call.callee != Some(*expression) {
                        diagnostics.push(Diagnostic::new(
                            call.origin.span.clone(),
                            "call callee step is missing, misplaced, or disagrees with its callee",
                        ));
                    }
                }
                LoweredCallStep::Argument { argument } => {
                    if let Some(count) = covered.get_mut(*argument) {
                        *count += 1;
                    }
                    // A product-valued place is evaluated once, then projected
                    // into all ABI slots by the backend.
                    if *argument == 0 && call.arguments.len() > 1 {
                        let first = &call.arguments[0];
                        if first.place.is_some()
                            && call.arguments.iter().all(|entry| {
                                entry.expression == first.expression && entry.place == first.place
                            })
                        {
                            for count in covered.iter_mut().skip(1) {
                                *count += 1;
                            }
                        }
                    }
                }
                LoweredCallStep::ProductElement { argument, .. }
                | LoweredCallStep::Default { argument, .. } => {
                    if let Some(count) = covered.get_mut(*argument) {
                        *count += 1;
                    }
                }
                LoweredCallStep::ProductSpread { mappings, .. } => {
                    for mapping in mappings {
                        if let Some(count) = covered.get_mut(mapping.slot) {
                            *count += 1;
                        }
                    }
                }
                LoweredCallStep::NamedProductSpread { mappings, .. } => {
                    for mapping in mappings {
                        if let Some(count) = covered.get_mut(mapping.slot) {
                            *count += 1;
                        }
                    }
                }
                LoweredCallStep::Resource { resource } => {
                    resources_started = true;
                    resources.push(*resource);
                }
                LoweredCallStep::Invoke => {
                    if invoked || index + 1 != call.steps.len() {
                        diagnostics.push(Diagnostic::new(
                            call.origin.span.clone(),
                            "call invocation must occur exactly once as the final step",
                        ));
                    }
                    invoked = true;
                }
            }
            if !matches!(
                step,
                LoweredCallStep::Callee { .. }
                    | LoweredCallStep::Resource { .. }
                    | LoweredCallStep::Invoke
            ) && resources_started
            {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    "call argument step occurs after a resource step",
                ));
            }
        }
        if callee_count != usize::from(call.callee.is_some()) {
            diagnostics.push(Diagnostic::new(
                call.origin.span.clone(),
                "call has an incorrect number of callee steps",
            ));
        }
        if !invoked {
            diagnostics.push(Diagnostic::new(
                call.origin.span.clone(),
                "call has no invocation step",
            ));
        }
        if resources != (0..call.resource_bindings.len()).collect::<Vec<_>>() {
            diagnostics.push(Diagnostic::new(
                call.origin.span.clone(),
                "call resource steps are missing, duplicated, or out of order",
            ));
        }
        for (argument, count) in covered.into_iter().enumerate() {
            if count != 1 {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    format!("call argument {argument} is evaluated {count} times"),
                ));
            }
        }
    }

    fn check_call_step_argument(
        &self,
        origin: &Origin,
        argument: usize,
        arguments: usize,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        if argument >= arguments {
            diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                format!("call step targets out-of-range argument {argument} of {arguments}"),
            ));
        }
    }

    fn validate_callable_target(
        &self,
        origin: &Origin,
        target: &LoweredCallableTarget,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match target {
            LoweredCallableTarget::DirectFunction { function, .. } => {
                if self.functions.get(*function).is_none() {
                    diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "function",
                        function.0,
                    ));
                }
            }
            LoweredCallableTarget::IndirectClosure { callee } => {
                if !self.expressions.contains(*callee) {
                    diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "expression",
                        callee.index(),
                    ));
                }
            }
            LoweredCallableTarget::ExternalFunction { symbol } => match self.symbols.get(*symbol) {
                Some(symbol) if symbol.external => {}
                Some(_) => diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    "external callable target is not an external symbol",
                )),
                None => diagnostics.push(invalid_reference(
                    origin,
                    "callable target",
                    "symbol",
                    symbol.0,
                )),
            },
            LoweredCallableTarget::Intrinsic { symbol, intrinsic } => {
                match self.symbols.get(*symbol) {
                    Some(symbol) if symbol.intrinsic == Some(*intrinsic) => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "intrinsic callable target disagrees with its symbol",
                    )),
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "symbol",
                        symbol.0,
                    )),
                }
            }
            LoweredCallableTarget::Constructor {
                symbol,
                type_id,
                recursive,
            } => {
                match self.symbols.get(*symbol) {
                    Some(symbol) if symbol.constructor == Some(*type_id) => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "constructor callable target disagrees with its symbol",
                    )),
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "symbol",
                        symbol.0,
                    )),
                }
                match self.types.get(*type_id) {
                    Some(metadata) if metadata.recursive_construction == *recursive => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "constructor callable target recursive classification disagrees with its type",
                    )),
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "type",
                        type_id.0,
                    )),
                }
            }
            LoweredCallableTarget::TraitImplementation {
                trait_id,
                method,
                function,
            } => {
                self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
                if let Some(function) = function
                    && self.functions.get(*function).is_none()
                {
                    diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "function",
                        function.0,
                    ));
                }
            }
            LoweredCallableTarget::StructuralTraitMethod {
                trait_id, method, ..
            } => {
                self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
            }
        }
    }

    fn validate_trait_method_reference(
        &self,
        origin: &Origin,
        trait_id: TraitId,
        method: TraitMethodId,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        if self.traits.get(trait_id).is_none() {
            diagnostics.push(invalid_reference(
                origin,
                "trait evidence",
                "trait",
                trait_id.0,
            ));
        }
        match self.trait_methods.get(method) {
            Some(metadata) if metadata.trait_id == trait_id => {}
            Some(_) => diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                "trait evidence method does not belong to its recorded trait",
            )),
            None => diagnostics.push(invalid_reference(
                origin,
                "trait evidence",
                "trait method",
                method.0,
            )),
        }
    }

    fn validate_trait_evidence(
        &self,
        origin: &Origin,
        evidence: &TraitEvidence,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match evidence {
            TraitEvidence::ExplicitImplementation {
                trait_id,
                implementation,
                method,
                function,
                ..
            } => {
                self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
                if self.functions.get(*function).is_none() {
                    diagnostics.push(invalid_reference(
                        origin,
                        "trait evidence",
                        "function",
                        function.0,
                    ));
                }
                match self.trait_implementations.get(*implementation) {
                    Some(metadata)
                        if metadata.trait_id == *trait_id
                            && metadata.methods.iter().any(|(candidate, selected)| {
                                candidate == method && selected == function
                            }) => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "trait evidence implementation does not provide its selected method",
                    )),
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "trait evidence",
                        "trait implementation",
                        implementation.index(),
                    )),
                }
            }
            TraitEvidence::Structural {
                trait_id, method, ..
            } => {
                self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
            }
            TraitEvidence::DeclaredBound {
                trait_id, method, ..
            } => {
                if self.traits.get(*trait_id).is_none() {
                    diagnostics.push(invalid_reference(
                        origin,
                        "trait evidence",
                        "trait",
                        trait_id.0,
                    ));
                }
                if let Some(method) = method {
                    self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
                }
            }
        }
    }

    /// Checks that checked formatter helper selections resolve to the
    /// function catalog. Absent helpers are valid for `no_prelude` programs
    /// that never construct a template.
    fn validate_string_formatting(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (role, function) in [
            ("formatter constructor", self.string_formatting.constructor),
            ("formatter write", self.string_formatting.write),
            ("formatter finish", self.string_formatting.finish),
        ] {
            let Some(function) = function else {
                continue;
            };
            if self.functions.get(function).is_none() {
                diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!(
                        "lowered {role} function reference {} is missing from the function catalog",
                        function.0
                    ),
                ));
            }
        }
        diagnostics
    }

    fn validate_arena_references(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (id, expression) in self.expressions.iter() {
            if self.expression_lookup.get(&expression.key) != Some(&id) {
                diagnostics.push(Diagnostic::new(
                    expression.origin.span.clone(),
                    format!(
                        "expression occurrence {:?} disagrees with its lookup entry",
                        expression.key
                    ),
                ));
            }
            match &expression.kind {
                LoweredExpressionKind::Deferred(DeferredExpressionFamily::Callable) => {
                    diagnostics.push(Diagnostic::new(
                        expression.origin.span.clone(),
                        "callable expression was not lowered",
                    ));
                }
                LoweredExpressionKind::String(_) => {}
                LoweredExpressionKind::Integer(integer) => {
                    let width = integer_literal_bit_width(integer.integer_type);
                    let value_bits = if integer.integer_type.is_signed() {
                        width - 1
                    } else {
                        width
                    };
                    if value_bits < 64 && integer.value > ((1_u64 << value_bits) - 1) {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            format!(
                                "lowered integer payload {} does not fit in `{}`",
                                integer.value,
                                integer.integer_type.name()
                            ),
                        ));
                    }
                }
                LoweredExpressionKind::Float(float) => {
                    if !float.value.is_finite() {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "lowered float payload is not finite",
                        ));
                    }
                    if float.float_type == FloatType::F32
                        && f64::from(float.value as f32) != float.value
                    {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "lowered F32 payload is not exactly representable",
                        ));
                    }
                }
                LoweredExpressionKind::CString(c_string) => {
                    let trailing_nul = c_string.bytes.last() == Some(&0);
                    if !trailing_nul {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "lowered C string payload has no trailing NUL",
                        ));
                    }
                    if c_string.bytes[..c_string.bytes.len().saturating_sub(1)].contains(&0) {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "lowered C string payload contains an interior NUL",
                        ));
                    }
                }
                LoweredExpressionKind::Block(id) if !self.blocks.contains(*id) => diagnostics.push(
                    invalid_reference(&expression.origin, "expression", "block", id.index()),
                ),
                LoweredExpressionKind::Block(_) => {}
                LoweredExpressionKind::Name(name) => {
                    if self.symbols.get(name.symbol).is_none() {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "name",
                            "symbol",
                            name.symbol.0,
                        ));
                    }
                    if let Some(singleton) = name.singleton
                        && self.types.get(singleton).is_none()
                    {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "name",
                            "type",
                            singleton.0,
                        ));
                    }
                }
                LoweredExpressionKind::Access(access) => {
                    if !self.expressions.contains(access.base) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "access",
                            "expression",
                            access.base.index(),
                        ));
                    }
                }
                LoweredExpressionKind::Product(product) => {
                    for step in &product.steps {
                        let child = match step {
                            LoweredProductStep::Positional { expression, .. }
                            | LoweredProductStep::Designated { expression, .. }
                            | LoweredProductStep::PositionalSpread { expression, .. }
                            | LoweredProductStep::NamedSpread { expression, .. }
                            | LoweredProductStep::Default { expression, .. } => *expression,
                        };
                        if !self.expressions.contains(child) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "product step",
                                "expression",
                                child.index(),
                            ));
                        }
                    }
                    for field in &product.fields {
                        if !self.expressions.contains(*field) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "product field",
                                "expression",
                                field.index(),
                            ));
                        }
                    }
                    self.validate_product_plan(expression, product, &mut diagnostics);
                }
                LoweredExpressionKind::RepeatedProduct(repeated) => {
                    if !self.expressions.contains(repeated.expression) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "repeated product",
                            "expression",
                            repeated.expression.index(),
                        ));
                    }
                    let source_type = expression
                        .coercion
                        .as_ref()
                        .map(|coercion| &coercion.source)
                        .unwrap_or(&expression.value_type);
                    repeated.validate_shape(source_type, &expression.origin, &mut diagnostics);
                }
                LoweredExpressionKind::Satisfies(satisfies) => {
                    if !self.expressions.contains(satisfies.value) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "satisfies",
                            "expression",
                            satisfies.value.index(),
                        ));
                    }
                }
                LoweredExpressionKind::Logical(logical) => {
                    for child in [logical.left, logical.right] {
                        if !self.expressions.contains(child) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "logical",
                                "expression",
                                child.index(),
                            ));
                        }
                    }
                    if let CheckedType::Sum(sum) = &logical.bool_type
                        && logical.true_index >= sum.alternatives.len()
                    {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            format!(
                                "logical true alternative index {} is out of range for {} alternatives",
                                logical.true_index,
                                sum.alternatives.len()
                            ),
                        ));
                    }
                }
                LoweredExpressionKind::StringTemplate(template) => {
                    for part in &template.parts {
                        let LoweredStringTemplatePart::Interpolation(interpolation) = part else {
                            continue;
                        };
                        if !self.expressions.contains(interpolation.expression) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "string template",
                                "expression",
                                interpolation.expression.index(),
                            ));
                        }
                        if self.traits.get(interpolation.trait_id).is_none() {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "string template",
                                "trait",
                                interpolation.trait_id.0,
                            ));
                        }
                        match self.trait_methods.get(interpolation.method) {
                            Some(method) if method.trait_id == interpolation.trait_id => {}
                            Some(_) => diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                "interpolation formatting method does not belong to its trait",
                            )),
                            None => diagnostics.push(invalid_reference(
                                &expression.origin,
                                "string template",
                                "trait method",
                                interpolation.method.0,
                            )),
                        }
                        self.validate_trait_evidence(
                            &expression.origin,
                            &interpolation.evidence,
                            &mut diagnostics,
                        );
                        if !evidence_matches(
                            &interpolation.evidence,
                            interpolation.trait_id,
                            Some(interpolation.method),
                        ) {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                "interpolation evidence does not match its formatting selection",
                            ));
                        }
                    }
                }
                LoweredExpressionKind::Index(index) => {
                    for child in [index.base, index.index] {
                        if !self.expressions.contains(child) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "index",
                                "expression",
                                child.index(),
                            ));
                        }
                    }
                    for place in [index.base_place, index.index_place].into_iter().flatten() {
                        if !self.places.contains(place) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "index",
                                "place",
                                place.index(),
                            ));
                        }
                    }
                    // The operand temporary facts must agree with the recorded
                    // places and the method's mutation/move marks.
                    if let Some(method_type) = &index.method_type {
                        let place_marked = |element: usize| {
                            method_type
                                .mutations
                                .iter()
                                .chain(&method_type.moves)
                                .any(|mutation| *mutation == CheckedMutation::Element(element))
                        };
                        for (element, temporary, place) in [
                            (0, index.base_temporary, index.base_place),
                            (1, index.index_temporary, index.index_place),
                        ] {
                            if temporary != (place_marked(element) && place.is_none()) {
                                diagnostics.push(Diagnostic::new(
                                    expression.origin.span.clone(),
                                    "index operand temporary fact disagrees with its place",
                                ));
                            }
                        }
                        // The recorded operand facts cover exactly the
                        // method's flattened parameters.
                        let parameters = match method_type.parameter.as_ref() {
                            CheckedType::Product(product) => product.elements.len(),
                            _ => 1,
                        };
                        if index.operands.indirect.len() != parameters
                            || index.operands.drops_after_call.len() != parameters
                        {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                "index operand facts do not cover the method's parameters",
                            ));
                        }
                    }
                    if self
                        .trait_methods
                        .get(index.dispatch.method)
                        .map(|method| method.trait_id)
                        != Some(index.trait_id)
                    {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "index dispatch method does not belong to its recorded trait",
                        ));
                    }
                    if index.method_type.is_none() {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "index dispatch has no instantiated method type for lowering",
                        ));
                    }
                    self.validate_trait_evidence(
                        &expression.origin,
                        &index.evidence,
                        &mut diagnostics,
                    );
                    if !evidence_matches(
                        &index.evidence,
                        index.trait_id,
                        Some(index.dispatch.method),
                    ) {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "index evidence does not match its checked dispatch",
                        ));
                    }
                    let base_type = self
                        .expressions
                        .get(index.base)
                        .map(|base| &base.value_type);
                    let position_type = self
                        .expressions
                        .get(index.index)
                        .map(|position| &position.value_type);
                    for (position, actual) in [
                        (0usize, base_type),
                        (1, position_type),
                        (2, Some(&expression.value_type)),
                    ] {
                        if let (Some(expected), Some(actual)) =
                            (index.arguments.get(position), actual)
                            && !types_agree(expected, actual)
                        {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                format!(
                                    "index dispatch argument {position} is `{expected}` but the lowered operand is `{actual}`"
                                ),
                            ));
                        }
                    }
                }
                LoweredExpressionKind::Loop(loop_) => {
                    if !self.blocks.contains(loop_.body) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "loop",
                            "block",
                            loop_.body.index(),
                        ));
                    }
                }
                LoweredExpressionKind::Match(match_) => {
                    if !self.expressions.contains(match_.subject) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "match",
                            "expression",
                            match_.subject.index(),
                        ));
                    }
                    for arm in &match_.arms {
                        if !self.patterns.contains(arm.pattern) {
                            diagnostics.push(invalid_reference(
                                &arm.origin,
                                "match arm",
                                "pattern",
                                arm.pattern.index(),
                            ));
                        }
                        if !self.expressions.contains(arm.body) {
                            diagnostics.push(invalid_reference(
                                &arm.origin,
                                "match arm",
                                "expression",
                                arm.body.index(),
                            ));
                        }
                        for symbol in &arm.bound_symbols {
                            if self.symbols.get(*symbol).is_none() {
                                diagnostics.push(invalid_reference(
                                    &arm.origin,
                                    "match arm",
                                    "symbol",
                                    symbol.0,
                                ));
                            }
                        }
                    }
                }
                LoweredExpressionKind::Call(call) => {
                    if !self.calls.contains(*call) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "expression",
                            "call",
                            call.index(),
                        ));
                    }
                }
                LoweredExpressionKind::CallableValue(value) => {
                    if !self.callable_values.contains(*value) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "expression",
                            "callable value",
                            value.index(),
                        ));
                    }
                }
                LoweredExpressionKind::Resource(use_) => {
                    if !self.resource_uses.contains(*use_) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "expression",
                            "resource use",
                            use_.index(),
                        ));
                    }
                }
                LoweredExpressionKind::With(with) => {
                    if !self.withs.contains(*with) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "expression",
                            "with",
                            with.index(),
                        ));
                    }
                }
                LoweredExpressionKind::Coro(coro) => {
                    if !self.coros.contains(*coro) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "expression",
                            "coro",
                            coro.index(),
                        ));
                    }
                }
                LoweredExpressionKind::Await(await_) => {
                    if !self.awaits.contains(*await_) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "expression",
                            "await",
                            await_.index(),
                        ));
                    }
                }
            }
            self.validate_expression_coercion(expression, &mut diagnostics);
        }
        diagnostics.extend(self.validate_loop_exits());
        diagnostics.extend(self.validate_mutation_dispatch_arguments());
        for (_, pattern) in self.patterns.iter() {
            match &pattern.kind {
                LoweredPatternKind::Wildcard | LoweredPatternKind::Literal { .. } => {}
                LoweredPatternKind::Binding {
                    symbol, singleton, ..
                } => {
                    if let Some(symbol) = symbol
                        && self.symbols.get(*symbol).is_none()
                    {
                        diagnostics.push(invalid_reference(
                            &pattern.origin,
                            "pattern",
                            "symbol",
                            symbol.0,
                        ));
                    }
                    if let Some(singleton) = singleton
                        && self.types.get(*singleton).is_none()
                    {
                        diagnostics.push(invalid_reference(
                            &pattern.origin,
                            "pattern",
                            "type",
                            singleton.0,
                        ));
                    }
                }
                LoweredPatternKind::Product { elements, .. } => {
                    for element in elements {
                        if !self.patterns.contains(*element) {
                            diagnostics.push(invalid_reference(
                                &pattern.origin,
                                "pattern",
                                "pattern",
                                element.index(),
                            ));
                        }
                    }
                }
                LoweredPatternKind::Nominal {
                    target, argument, ..
                } => {
                    if let Some(target) = target
                        && self.types.get(*target).is_none()
                    {
                        diagnostics.push(invalid_reference(
                            &pattern.origin,
                            "pattern",
                            "type",
                            target.0,
                        ));
                    }
                    if !self.patterns.contains(*argument) {
                        diagnostics.push(invalid_reference(
                            &pattern.origin,
                            "pattern",
                            "pattern",
                            argument.index(),
                        ));
                    }
                }
                LoweredPatternKind::At {
                    binding,
                    pattern: nested,
                } => {
                    for child in [*binding, *nested] {
                        if !self.patterns.contains(child) {
                            diagnostics.push(invalid_reference(
                                &pattern.origin,
                                "at pattern",
                                "pattern",
                                child.index(),
                            ));
                        }
                    }
                }
            }
            self.validate_pattern_test_plan(pattern, &mut diagnostics);
        }
        for (_, place) in self.places.iter() {
            match &place.kind {
                LoweredPlaceKind::Symbol { symbol } | LoweredPlaceKind::CapturedCell { symbol } => {
                    if self.symbols.get(*symbol).is_none() {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "symbol",
                            symbol.0,
                        ));
                    }
                }
                LoweredPlaceKind::Temporary { expression } => {
                    if !self.expressions.contains(*expression) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "expression",
                            expression.index(),
                        ));
                    }
                }
                LoweredPlaceKind::Resource { use_ } => match self.resource_uses.get(*use_) {
                    Some(use_) if use_.kind == LoweredResourceUseKind::MutablePlace => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        place.origin.span.clone(),
                        "resource place references a resource read",
                    )),
                    None => diagnostics.push(invalid_reference(
                        &place.origin,
                        "place",
                        "resource use",
                        use_.index(),
                    )),
                },
                LoweredPlaceKind::Dereference { reference, .. } => {
                    if !self.expressions.contains(*reference) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "expression",
                            reference.index(),
                        ));
                    }
                }
                LoweredPlaceKind::ProductElement { base, .. }
                | LoweredPlaceKind::Representation { base } => {
                    if !self.places.contains(*base) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "place",
                            base.index(),
                        ));
                    }
                }
                LoweredPlaceKind::Indexed { base, index } => {
                    if !self.places.contains(*base) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "place",
                            base.index(),
                        ));
                    }
                    if !self.expressions.contains(*index) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "expression",
                            index.index(),
                        ));
                    }
                }
            }
        }
        for (_, block) in self.blocks.iter() {
            for item in &block.items {
                if !self.items.contains(*item) {
                    diagnostics.push(invalid_reference(
                        &block.origin,
                        "block",
                        "item",
                        item.index(),
                    ));
                }
            }
            if let Some(result) = block.result
                && !self.expressions.contains(result)
            {
                diagnostics.push(invalid_reference(
                    &block.origin,
                    "block",
                    "expression",
                    result.index(),
                ));
            }
        }
        for (_, item) in self.items.iter() {
            let mut check = |target: &str, index: usize, valid: bool| {
                if !valid {
                    diagnostics.push(invalid_reference(&item.origin, "item", target, index));
                }
            };
            match &item.kind {
                LoweredItemKind::Binding(binding) => {
                    if let Some(symbol) = binding.symbol
                        && !binding.compile_time_only
                    {
                        check("symbol", symbol.0, self.symbols.get(symbol).is_some());
                    }
                    if let Some(value) = binding.value {
                        check(
                            "expression",
                            value.index(),
                            self.expressions.contains(value),
                        );
                    }
                }
                LoweredItemKind::PatternBinding(binding) => {
                    check(
                        "pattern",
                        binding.pattern.index(),
                        self.patterns.contains(binding.pattern),
                    );
                    check(
                        "expression",
                        binding.value.index(),
                        self.expressions.contains(binding.value),
                    );
                    // A propagation whose residual result is a sum needs the
                    // failure coercion plan.
                    if let Some(propagation) = &binding.propagation {
                        let needs_plan = propagation.source != propagation.result
                            && matches!(propagation.result, CheckedType::Sum(_));
                        let unresolved =
                            instance_resolution::unresolved_type_problem(&propagation.source)
                                .is_some()
                                || instance_resolution::unresolved_type_problem(
                                    &propagation.result,
                                )
                                .is_some();
                        match (&binding.propagation_plan, needs_plan) {
                            (Some(plan), true) => {
                                match LoweredCoercionPlan::plan(
                                    &propagation.source,
                                    &propagation.result,
                                ) {
                                    Ok(expected) if *plan == expected => {}
                                    _ => diagnostics.push(Diagnostic::new(
                                        item.origin.span.clone(),
                                        "propagation coercion plan disagrees with its checked types",
                                    )),
                                }
                            }
                            (None, true) if !unresolved => diagnostics.push(Diagnostic::new(
                                item.origin.span.clone(),
                                "propagation coercion has no emission plan",
                            )),
                            _ => {}
                        }
                        if !unresolved
                            && binding.propagation_residual
                                != LoweredPatternBindingItem::residual_alternative(propagation)
                        {
                            diagnostics.push(Diagnostic::new(
                                item.origin.span.clone(),
                                "propagation residual alternative disagrees with its checked types",
                            ));
                        }
                    }
                }
                LoweredItemKind::Assignment(assignment) => {
                    check(
                        "place",
                        assignment.target.index(),
                        self.places.contains(assignment.target),
                    );
                    check(
                        "expression",
                        assignment.value.index(),
                        self.expressions.contains(assignment.value),
                    );
                    if let Some(symbol) = assignment.initialization_symbol {
                        check("symbol", symbol.0, self.symbols.get(symbol).is_some());
                    }
                    if let Some(evidence) = &assignment.evidence {
                        self.validate_trait_evidence(&item.origin, evidence, &mut diagnostics);
                    }
                }
                LoweredItemKind::Return(item) => {
                    check(
                        "expression",
                        item.value.index(),
                        self.expressions.contains(item.value),
                    );
                }
                LoweredItemKind::Break(item) => {
                    if let Some(value) = item.value {
                        check(
                            "expression",
                            value.index(),
                            self.expressions.contains(value),
                        );
                    }
                }
                LoweredItemKind::Continue(_) => {}
                LoweredItemKind::Expression(item) => {
                    check(
                        "expression",
                        item.expression.index(),
                        self.expressions.contains(item.expression),
                    );
                }
            }
        }
        diagnostics
    }

    /// Checks that the occurrence lookup has exactly one entry per expression
    /// node, so no two nodes alias the same occurrence key.
    fn validate_occurrence_lookups(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let nodes = self.expressions.iter().count();
        if self.expression_lookup.len() != nodes {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "expression occurrence lookup has {} entries for {nodes} nodes",
                    self.expression_lookup.len()
                ),
            ));
        }
        diagnostics
    }

    /// Checks that every arena handle addresses its own insertion slot and
    /// that each auxiliary lookup index has exactly one entry per indexed
    /// node. Arenas are append-only, so this is a defensive backstop for
    /// directly mutated fixtures rather than a runtime invariant.
    fn validate_arena_identity(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        macro_rules! check_arena {
            ($arena:expr, $kind:literal) => {
                for (position, (id, node)) in $arena.iter().enumerate() {
                    if id.index() != position {
                        diagnostics.push(Diagnostic::new(
                            node.origin.span.clone(),
                            format!(
                                "lowered {} id {} is out of insertion position {position}",
                                $kind,
                                id.index()
                            ),
                        ));
                    }
                }
            };
        }
        check_arena!(self.expressions, "expression");
        check_arena!(self.patterns, "pattern");
        check_arena!(self.places, "place");
        check_arena!(self.blocks, "block");
        check_arena!(self.items, "item");
        check_arena!(self.trait_implementations, "trait implementation");
        check_arena!(self.calls, "call");
        check_arena!(self.callable_values, "callable value");
        check_arena!(self.resource_providers, "resource provider");
        check_arena!(self.resource_uses, "resource use");
        check_arena!(self.withs, "with");
        check_arena!(self.reactive_operations, "reactive operation");
        check_arena!(self.reactive_callbacks, "reactive callback");
        check_arena!(self.coroutine_plans, "coroutine plan");
        check_arena!(self.coros, "coro");
        check_arena!(self.awaits, "await");
        check_arena!(self.initializers, "initializer");
        macro_rules! check_catalog {
            ($catalog:expr, $kind:literal) => {
                for (position, (id, _, node)) in $catalog.iter().enumerate() {
                    if id.index() != position {
                        diagnostics.push(Diagnostic::new(
                            node.origin.span.clone(),
                            format!(
                                "lowered {} catalog id {} is out of insertion position {position}",
                                $kind,
                                id.index()
                            ),
                        ));
                    }
                }
            };
        }
        check_catalog!(self.modules, "module");
        check_catalog!(self.functions, "function");
        check_catalog!(self.symbols, "symbol");
        check_catalog!(self.types, "type");
        check_catalog!(self.traits, "trait");
        check_catalog!(self.trait_methods, "trait method");
        for (key, id) in &self.block_lookup {
            match self.blocks.get(*id) {
                Some(block) if block.origin.syntax == key.syntax => {}
                Some(_) => diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    "block occurrence lookup points at a different block syntax".to_owned(),
                )),
                None => diagnostics.push(invalid_reference(
                    &Origin::compiler(),
                    "block lookup",
                    "block",
                    id.index(),
                )),
            }
        }
        for (key, id) in &self.coroutine_plan_lookup {
            match self.coroutine_plans.get(*id) {
                Some(plan) if plan.body_syntax == *key => {}
                Some(_) => diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    "coroutine plan lookup points at a different body syntax".to_owned(),
                )),
                None => diagnostics.push(invalid_reference(
                    &Origin::compiler(),
                    "coroutine plan lookup",
                    "coroutine plan",
                    id.index(),
                )),
            }
        }
        for (thunk, id) in &self.coroutine_plan_by_thunk {
            match self.coroutine_plans.get(*id) {
                Some(plan) if plan.thunk == *thunk => {}
                Some(_) => diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    "coroutine plan thunk lookup points at a different thunk".to_owned(),
                )),
                None => diagnostics.push(invalid_reference(
                    &Origin::compiler(),
                    "coroutine plan thunk lookup",
                    "coroutine plan",
                    id.index(),
                )),
            }
        }
        diagnostics
    }

    /// Checks that a function body or module initializer block belongs to
    /// exactly one owner and that a template's recorded body origin matches the
    /// block it points at. Parameter patterns are likewise owned once.
    fn validate_function_body_ownership(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut bodies = HashMap::<BlockId, FunctionId>::new();
        let mut patterns = HashMap::<PatternId, FunctionId>::new();
        for (_, key, function) in self.functions.iter() {
            if let Some(body) = function.body {
                if let Some(previous) = bodies.insert(body, key) {
                    diagnostics.push(Diagnostic::new(
                        function.origin.span.clone(),
                        format!(
                            "function {key:?} shares its body block with function {previous:?}"
                        ),
                    ));
                }
                if let Some(block) = self.blocks.get(body)
                    && block.origin.syntax != function.body_syntax
                {
                    diagnostics.push(Diagnostic::new(
                        function.origin.span.clone(),
                        format!(
                            "function {key:?} body block origin does not match its body syntax id"
                        ),
                    ));
                }
            } else {
                diagnostics.push(Diagnostic::new(
                    function.origin.span.clone(),
                    format!("function {key:?} has no lowered body block"),
                ));
            }
            if let Some(previous) = patterns.insert(function.parameter_pattern, key) {
                diagnostics.push(Diagnostic::new(
                    function.origin.span.clone(),
                    format!(
                        "function {key:?} shares its parameter pattern with function {previous:?}"
                    ),
                ));
            }
        }
        let mut initializers = HashMap::<BlockId, ModuleId>::new();
        for (_, initializer) in self.initializers.iter() {
            if let Some(previous) = initializers.insert(initializer.body, initializer.module) {
                diagnostics.push(Diagnostic::new(
                    initializer.origin.span.clone(),
                    format!(
                        "module {:?} shares its initializer block with module {previous:?}",
                        initializer.module
                    ),
                ));
            }
        }
        for (block, function) in bodies {
            if let Some(module) = initializers.get(&block) {
                diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!(
                        "block {} is both function {function:?}'s body and module {module:?}'s initializer",
                        block.index()
                    ),
                ));
            }
        }
        diagnostics
    }

    /// Rejects unresolved inference placeholders in runtime metadata. Lowering
    /// runs only on successfully checked programs, so an `Inferred` or `Error`
    /// type on a runtime node means a semantic decision was lost instead of
    /// recorded. Declared generic parameters and `Never` (diverged code) stay
    /// legal.
    fn validate_concrete_metadata(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        fn reject(
            diagnostics: &mut Vec<Diagnostic>,
            origin: &Origin,
            label: &str,
            value_type: &CheckedType,
        ) {
            if type_has_placeholder(value_type) {
                diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "lowered {label} contains an unresolved inference placeholder in `{value_type}`"
                    ),
                ));
            }
        }
        fn reject_function(
            diagnostics: &mut Vec<Diagnostic>,
            origin: &Origin,
            label: &str,
            function_type: &CheckedFunctionType,
        ) {
            reject(diagnostics, origin, label, &function_type.parameter);
            reject(diagnostics, origin, label, &function_type.result);
            for resource in &function_type.effects.resources {
                reject(diagnostics, origin, label, &resource.value_type);
            }
        }
        for (_, expression) in self.expressions.iter() {
            if matches!(expression.kind, LoweredExpressionKind::Deferred(_)) {
                continue;
            }
            reject(
                &mut diagnostics,
                &expression.origin,
                "expression type",
                &expression.value_type,
            );
            for resource in &expression.effects.resources {
                reject(
                    &mut diagnostics,
                    &expression.origin,
                    "expression effect resource",
                    &resource.value_type,
                );
            }
            match &expression.kind {
                LoweredExpressionKind::Product(product) => {
                    for element in &product.final_type.elements {
                        reject(
                            &mut diagnostics,
                            &expression.origin,
                            "product field type",
                            &element.value_type,
                        );
                    }
                }
                LoweredExpressionKind::Match(match_) => reject(
                    &mut diagnostics,
                    &expression.origin,
                    "match source type",
                    &match_.source,
                ),
                LoweredExpressionKind::Logical(logical) => reject(
                    &mut diagnostics,
                    &expression.origin,
                    "logical `Bool` type",
                    &logical.bool_type,
                ),
                LoweredExpressionKind::Index(index) => {
                    if let Some(method_type) = &index.method_type {
                        reject_function(
                            &mut diagnostics,
                            &expression.origin,
                            "`Index` method type",
                            method_type,
                        );
                    }
                    for argument in &index.arguments {
                        reject(
                            &mut diagnostics,
                            &expression.origin,
                            "`Index` dispatch argument",
                            argument,
                        );
                    }
                }
                LoweredExpressionKind::StringTemplate(template) => {
                    for part in &template.parts {
                        if let LoweredStringTemplatePart::Interpolation(interpolation) = part {
                            reject(
                                &mut diagnostics,
                                &expression.origin,
                                "interpolation value type",
                                &interpolation.value_type,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        for (_, pattern) in self.patterns.iter() {
            reject(
                &mut diagnostics,
                &pattern.origin,
                "pattern type",
                &pattern.value_type,
            );
        }
        for (_, place) in self.places.iter() {
            reject(
                &mut diagnostics,
                &place.origin,
                "place type",
                &place.value_type,
            );
        }
        for (_, call) in self.calls.iter() {
            reject_function(
                &mut diagnostics,
                &call.origin,
                "call function type",
                &call.function_type,
            );
            reject(
                &mut diagnostics,
                &call.origin,
                "call result type",
                &call.result_type,
            );
        }
        for (_, value) in self.callable_values.iter() {
            reject_function(
                &mut diagnostics,
                &value.origin,
                "callable value function type",
                &value.function_type,
            );
        }
        for (_, provider) in self.resource_providers.iter() {
            reject(
                &mut diagnostics,
                &provider.origin,
                "resource provider type",
                &provider.resource.value_type,
            );
        }
        for (_, use_) in self.resource_uses.iter() {
            reject(
                &mut diagnostics,
                &use_.origin,
                "resource use type",
                &use_.resource.value_type,
            );
        }
        for (_, callback) in self.reactive_callbacks.iter() {
            reject_function(
                &mut diagnostics,
                &callback.origin,
                "reactive callback function type",
                &callback.function_type,
            );
        }
        for (_, operation) in self.reactive_operations.iter() {
            if let LoweredReactiveOperationKind::DerivedCreate { function_type, .. } =
                &operation.kind
            {
                reject_function(
                    &mut diagnostics,
                    &operation.origin,
                    "derived evaluator function type",
                    function_type,
                );
            }
        }
        for (_, plan) in self.coroutine_plans.iter() {
            reject(
                &mut diagnostics,
                &plan.origin,
                "coroutine result type",
                &plan.result_type,
            );
            for resource in &plan.deferred_effects.resources {
                reject(
                    &mut diagnostics,
                    &plan.origin,
                    "coroutine deferred resource",
                    &resource.value_type,
                );
            }
            for awaited in &plan.await_result_types {
                reject(
                    &mut diagnostics,
                    &plan.origin,
                    "coroutine awaited type",
                    awaited,
                );
            }
        }
        for (_, await_) in self.awaits.iter() {
            reject(
                &mut diagnostics,
                &await_.origin,
                "await result type",
                &await_.result_type,
            );
            match &await_.kind {
                LoweredAwaitKind::ChildCoroutine { child_result, .. } => reject(
                    &mut diagnostics,
                    &await_.origin,
                    "await child result type",
                    child_result,
                ),
                LoweredAwaitKind::Task { result } | LoweredAwaitKind::Wait { result } => reject(
                    &mut diagnostics,
                    &await_.origin,
                    "await outcome type",
                    result,
                ),
            }
        }
        diagnostics
    }

    /// Checks capture lists against the symbol catalog: every capture resolves,
    /// no capture duplicates another or one of the capturing function's own
    /// parameters, a function never captures one of its own symbols, and the
    /// shared-cell requirement agrees with the symbol's storage classification.
    fn validate_capture_consistency(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let check_captures = |diagnostics: &mut Vec<Diagnostic>,
                              origin: &Origin,
                              captures: &[LoweredCapture],
                              owner: Option<FunctionId>,
                              parameters: &[SymbolId]| {
            let mut seen = HashSet::new();
            for capture in captures {
                if !seen.insert(capture.symbol) {
                    diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        format!("capture symbol {} appears more than once", capture.symbol.0),
                    ));
                }
                if parameters.contains(&capture.symbol) {
                    diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        format!(
                            "capture symbol {} is also a parameter of its own function",
                            capture.symbol.0
                        ),
                    ));
                }
                match self.symbols.get(capture.symbol) {
                    Some(symbol) => {
                        if let Some(owner) = owner
                            && symbol.owner == Some(owner)
                        {
                            diagnostics.push(Diagnostic::new(
                                origin.span.clone(),
                                format!("function captures its own symbol {}", capture.symbol.0),
                            ));
                        }
                        if capture.requires_cell != symbol.captured_cell {
                            diagnostics.push(Diagnostic::new(
                                origin.span.clone(),
                                format!(
                                    "capture {} shared-cell fact disagrees with its symbol storage",
                                    capture.symbol.0
                                ),
                            ));
                        }
                    }
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "capture",
                        "symbol",
                        capture.symbol.0,
                    )),
                }
            }
        };
        for (_, key, function) in self.functions.iter() {
            check_captures(
                &mut diagnostics,
                &function.origin,
                &function.captures,
                Some(key),
                &function.parameters,
            );
        }
        for (_, callback) in self.reactive_callbacks.iter() {
            check_captures(
                &mut diagnostics,
                &callback.origin,
                &callback.captures,
                None,
                &[],
            );
        }
        for (_, plan) in self.coroutine_plans.iter() {
            check_captures(
                &mut diagnostics,
                &plan.origin,
                &plan.captures,
                Some(plan.thunk),
                &[],
            );
        }
        diagnostics
    }

    /// Walks every arena node through typed arena edges starting from module
    /// initializers and function bodies, and reports nodes that are not
    /// reachable from any root. Deliberate sharing through the occurrence
    /// memo is expected; the traversal visits each node once.
    fn validate_ownership(&self) -> Vec<Diagnostic> {
        let mut reached = Reachability::default();
        // Function effect providers and executable-entry providers are scope
        // roots installed by the owner's prologue: nothing expression-shaped
        // references them, so they seed the traversal directly. `with`
        // providers are reached through their `with` expression or the uses
        // that select them.
        for (id, provider) in self.resource_providers.iter() {
            if provider.kind != LoweredProviderOriginKind::Source {
                reached.owner = Some(provider.owner);
                self.visit_owned_resource_provider(id, &mut reached);
            }
        }
        for (_, initializer) in self.initializers.iter() {
            reached.owner = Some(ExpressionOwner::Module(initializer.module));
            self.visit_owned_block(initializer.body, &mut reached);
        }
        // A coroutine plan is owned by its body thunk's catalog entry, and the
        // function catalog is a root set; `coro` expressions reach the same
        // plans again after materialization.
        for (id, plan) in self.coroutine_plans.iter() {
            if self.functions.get(plan.thunk).is_some() {
                self.visit_owned_coroutine_plan(id, &mut reached);
            }
        }
        for (_, _, function) in self.functions.iter() {
            reached.owner = Some(ExpressionOwner::Function(function.semantic_id));
            self.visit_owned_pattern(function.parameter_pattern, &mut reached);
            if let Some(body) = function.body {
                self.visit_owned_block(body, &mut reached);
            }
            // A function body that is itself an expression (for example a
            // block) was allocated through the dispatcher; the expression node
            // is a root in its own right even though `LoweredFunction` only
            // records the body block.
            let key = ExpressionKey {
                syntax: function.body_syntax,
                owner: ExpressionOwner::Function(function.semantic_id),
                context: ExpressionContext::Primary,
            };
            if let Some(body) = self.expression_lookup.get(&key) {
                self.visit_owned_expression(*body, &mut reached);
            }
        }
        let mut diagnostics = Vec::new();
        for (id, expression) in self.expressions.iter() {
            if !reached.expressions.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    expression.origin.span.clone(),
                    format!(
                        "lowered expression {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, pattern) in self.patterns.iter() {
            if !reached.patterns.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    pattern.origin.span.clone(),
                    format!(
                        "lowered pattern {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, place) in self.places.iter() {
            if !reached.places.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    place.origin.span.clone(),
                    format!(
                        "lowered place {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, block) in self.blocks.iter() {
            if !reached.blocks.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    block.origin.span.clone(),
                    format!(
                        "lowered block {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, item) in self.items.iter() {
            if !reached.items.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    format!(
                        "lowered item {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, call) in self.calls.iter() {
            if !reached.calls.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    format!(
                        "lowered call {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, value) in self.callable_values.iter() {
            if !reached.callable_values.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    value.origin.span.clone(),
                    format!(
                        "lowered callable value {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, provider) in self.resource_providers.iter() {
            if !reached.resource_providers.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    provider.origin.span.clone(),
                    format!(
                        "lowered resource provider {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, use_) in self.resource_uses.iter() {
            if !reached.resource_uses.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    use_.origin.span.clone(),
                    format!(
                        "lowered resource use {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, with) in self.withs.iter() {
            if !reached.withs.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    with.origin.span.clone(),
                    format!(
                        "lowered with {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, operation) in self.reactive_operations.iter() {
            if !reached.reactive_operations.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    operation.origin.span.clone(),
                    format!(
                        "lowered reactive operation {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, callback) in self.reactive_callbacks.iter() {
            if !reached.reactive_callbacks.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    callback.origin.span.clone(),
                    format!(
                        "lowered reactive callback {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, plan) in self.coroutine_plans.iter() {
            if !reached.coroutine_plans.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    plan.origin.span.clone(),
                    format!(
                        "lowered coroutine plan {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, coro) in self.coros.iter() {
            if !reached.coros.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    coro.origin.span.clone(),
                    format!(
                        "lowered coro {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, await_) in self.awaits.iter() {
            if !reached.awaits.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    await_.origin.span.clone(),
                    format!(
                        "lowered await {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        diagnostics.extend(reached.conflicts);
        diagnostics
    }

    fn visit_owned_block(&self, id: BlockId, reached: &mut Reachability) {
        let Some(block) = self.blocks.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::Block(id), &block.origin);
        if !reached.blocks.insert(id) {
            return;
        }
        for item in &block.items {
            self.visit_owned_item(*item, reached);
        }
        if let Some(result) = block.result {
            self.visit_owned_expression(result, reached);
        }
    }

    fn visit_owned_item(&self, id: ItemId, reached: &mut Reachability) {
        let Some(item) = self.items.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::Item(id), &item.origin);
        if !reached.items.insert(id) {
            return;
        }
        match &item.kind {
            LoweredItemKind::Binding(binding) => {
                if let Some(value) = binding.value {
                    self.visit_owned_expression(value, reached);
                }
                if let Some(operation) = binding.reactive {
                    self.visit_owned_reactive_operation(operation, reached);
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                self.visit_owned_pattern(binding.pattern, reached);
                self.visit_owned_expression(binding.value, reached);
            }
            LoweredItemKind::Assignment(assignment) => {
                self.visit_owned_place(assignment.target, reached);
                self.visit_owned_expression(assignment.value, reached);
                if let Some(operation) = assignment.signal_notify {
                    self.visit_owned_reactive_operation(operation, reached);
                }
            }
            LoweredItemKind::Return(item) => self.visit_owned_expression(item.value, reached),
            LoweredItemKind::Break(item) => {
                if let Some(value) = item.value {
                    self.visit_owned_expression(value, reached);
                }
            }
            LoweredItemKind::Continue(_) => {}
            LoweredItemKind::Expression(statement) => {
                self.visit_owned_expression(statement.expression, reached);
            }
        }
    }

    fn visit_owned_expression(&self, id: ExpressionId, reached: &mut Reachability) {
        let Some(expression) = self.expressions.get(id) else {
            return;
        };
        let key_owner = expression.key.owner;
        if let Some(owner) = reached.owner
            && owner != key_owner
        {
            reached.conflicts.push(Diagnostic::new(
                expression.origin.span.clone(),
                format!(
                    "lowered expression occurrence is owned by {key_owner:?} but is reached from {owner:?}"
                ),
            ));
        }
        reached.claim(OwnedNode::Expression(id), key_owner, &expression.origin);
        if !reached.expressions.insert(id) {
            return;
        }
        reached.owner = Some(key_owner);
        match &expression.kind {
            LoweredExpressionKind::Block(block) => self.visit_owned_block(*block, reached),
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.visit_owned_expression(satisfies.value, reached);
            }
            LoweredExpressionKind::Logical(logical) => {
                self.visit_owned_expression(logical.left, reached);
                self.visit_owned_expression(logical.right, reached);
            }
            LoweredExpressionKind::Loop(loop_) => self.visit_owned_block(loop_.body, reached),
            LoweredExpressionKind::Match(match_) => {
                self.visit_owned_expression(match_.subject, reached);
                for arm in &match_.arms {
                    self.visit_owned_pattern(arm.pattern, reached);
                    self.visit_owned_expression(arm.body, reached);
                }
            }
            LoweredExpressionKind::Product(product) => {
                for step in &product.steps {
                    let child = match step {
                        LoweredProductStep::Positional { expression, .. }
                        | LoweredProductStep::Designated { expression, .. }
                        | LoweredProductStep::PositionalSpread { expression, .. }
                        | LoweredProductStep::NamedSpread { expression, .. }
                        | LoweredProductStep::Default { expression, .. } => *expression,
                    };
                    self.visit_owned_expression(child, reached);
                }
            }
            LoweredExpressionKind::RepeatedProduct(repeated) => {
                self.visit_owned_expression(repeated.expression, reached);
            }
            LoweredExpressionKind::Access(access) => {
                self.visit_owned_expression(access.base, reached);
            }
            LoweredExpressionKind::Index(index) => {
                self.visit_owned_expression(index.base, reached);
                self.visit_owned_expression(index.index, reached);
                if let Some(place) = index.base_place {
                    self.visit_owned_place(place, reached);
                }
                if let Some(place) = index.index_place {
                    self.visit_owned_place(place, reached);
                }
            }
            LoweredExpressionKind::StringTemplate(template) => {
                for part in &template.parts {
                    if let LoweredStringTemplatePart::Interpolation(interpolation) = part {
                        self.visit_owned_expression(interpolation.expression, reached);
                    }
                }
            }
            LoweredExpressionKind::Call(call) => self.visit_owned_call(*call, reached),
            LoweredExpressionKind::CallableValue(value) => {
                self.visit_owned_callable_value(*value, reached)
            }
            LoweredExpressionKind::Resource(use_) => self.visit_owned_resource_use(*use_, reached),
            LoweredExpressionKind::With(with) => self.visit_owned_with(*with, reached),
            LoweredExpressionKind::Coro(coro) => self.visit_owned_coro(*coro, reached),
            LoweredExpressionKind::Await(await_) => self.visit_owned_await(*await_, reached),
            LoweredExpressionKind::Name(name) => {
                if let Some(operation) = name.reactive {
                    self.visit_owned_reactive_operation(operation, reached);
                }
            }
            LoweredExpressionKind::Deferred(_)
            | LoweredExpressionKind::Integer(_)
            | LoweredExpressionKind::Float(_)
            | LoweredExpressionKind::String(_)
            | LoweredExpressionKind::CString(_) => {}
        }
    }

    fn visit_owned_resource_provider(
        &self,
        id: LoweredResourceProviderId,
        reached: &mut Reachability,
    ) {
        let Some(provider) = self.resource_providers.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::ResourceProvider(id), &provider.origin);
        if !reached.resource_providers.insert(id) {
            return;
        }
        if let Some(parent) = provider.parent {
            self.visit_owned_resource_provider(parent, reached);
        }
        if let LoweredProviderTarget::Expression(expression) = provider.target {
            self.visit_owned_expression(expression, reached);
        }
    }

    fn visit_owned_resource_use(&self, id: LoweredResourceUseId, reached: &mut Reachability) {
        let Some(use_) = self.resource_uses.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::ResourceUse(id), &use_.origin);
        if !reached.resource_uses.insert(id) {
            return;
        }
        if let Some(provider) = use_.provider {
            self.visit_owned_resource_provider(provider, reached);
        }
    }

    fn visit_owned_with(&self, id: LoweredWithId, reached: &mut Reachability) {
        let Some(with) = self.withs.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::With(id), &with.origin);
        if !reached.withs.insert(id) {
            return;
        }
        self.visit_owned_resource_provider(with.provider, reached);
        self.visit_owned_expression(with.value, reached);
        if let Some(place) = with.place {
            self.visit_owned_place(place, reached);
        }
        self.visit_owned_block(with.body, reached);
    }

    fn visit_owned_reactive_callback(
        &self,
        id: LoweredReactiveCallbackId,
        reached: &mut Reachability,
    ) {
        let Some(callback) = self.reactive_callbacks.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::ReactiveCallback(id), &callback.origin);
        if !reached.reactive_callbacks.insert(id) {
            return;
        }
        if let Some(callable) = callback.callable {
            self.visit_owned_expression(callable, reached);
        }
        for use_ in &callback.resources {
            self.visit_owned_resource_use(*use_, reached);
        }
    }

    fn visit_owned_reactive_operation(
        &self,
        id: LoweredReactiveOperationId,
        reached: &mut Reachability,
    ) {
        let Some(operation) = self.reactive_operations.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::ReactiveOperation(id), &operation.origin);
        if !reached.reactive_operations.insert(id) {
            return;
        }
        match operation.kind {
            LoweredReactiveOperationKind::Reaction {
                callback,
                reactive_provider,
            }
            | LoweredReactiveOperationKind::Until {
                predicate: callback,
                reactive_provider,
            } => {
                self.visit_owned_reactive_callback(callback, reached);
                if let Some(provider) = reactive_provider {
                    self.visit_owned_resource_provider(provider, reached);
                }
            }
            LoweredReactiveOperationKind::Batch { callback } => {
                self.visit_owned_reactive_callback(callback, reached);
            }
            LoweredReactiveOperationKind::SignalCreate { .. }
            | LoweredReactiveOperationKind::SignalRead { .. }
            | LoweredReactiveOperationKind::SignalNotify { .. }
            | LoweredReactiveOperationKind::DerivedRead { .. }
            | LoweredReactiveOperationKind::DerivedCreate { .. }
            | LoweredReactiveOperationKind::Scope
            | LoweredReactiveOperationKind::Snapshot => {}
        }
    }

    fn visit_owned_coroutine_plan(&self, id: LoweredCoroutinePlanId, reached: &mut Reachability) {
        let Some(plan) = self.coroutine_plans.get(id) else {
            return;
        };
        // A plan and its body belong to the body thunk, not to whichever
        // enclosing function links the plan through a `coro` or `await`.
        let owner = ExpressionOwner::Function(plan.thunk);
        reached.claim(OwnedNode::CoroutinePlan(id), owner, &plan.origin);
        if !reached.coroutine_plans.insert(id) {
            return;
        }
        let previous = reached.owner.replace(owner);
        if let Some(body) = plan.body {
            self.visit_owned_block(body, reached);
        }
        for await_ in &plan.awaits {
            self.visit_owned_await(*await_, reached);
        }
        reached.owner = previous;
    }

    fn visit_owned_coro(&self, id: LoweredCoroId, reached: &mut Reachability) {
        let Some(coro) = self.coros.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::Coro(id), &coro.origin);
        if !reached.coros.insert(id) {
            return;
        }
        self.visit_owned_coroutine_plan(coro.plan, reached);
    }

    fn visit_owned_await(&self, id: LoweredAwaitId, reached: &mut Reachability) {
        let Some(await_) = self.awaits.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::Await(id), &await_.origin);
        if !reached.awaits.insert(id) {
            return;
        }
        self.visit_owned_expression(await_.operand, reached);
        if let LoweredAwaitKind::ChildCoroutine {
            plan,
            deferred_resources,
            ..
        } = &await_.kind
        {
            if let Some(plan) = plan {
                self.visit_owned_coroutine_plan(*plan, reached);
            }
            for use_ in deferred_resources {
                self.visit_owned_resource_use(*use_, reached);
            }
        }
    }

    fn visit_owned_call(&self, id: LoweredCallId, reached: &mut Reachability) {
        let Some(call) = self.calls.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::Call(id), &call.origin);
        if !reached.calls.insert(id) {
            return;
        }
        if let Some(callee) = call.callee {
            self.visit_owned_expression(callee, reached);
        }
        if let Some(operation) = call.reactive {
            self.visit_owned_reactive_operation(operation, reached);
        }
        for binding in &call.resource_bindings {
            self.visit_owned_resource_use(*binding, reached);
        }
        for argument in &call.arguments {
            if let Some(expression) = argument.expression {
                self.visit_owned_expression(expression, reached);
            }
            if let Some(place) = argument.place {
                self.visit_owned_place(place, reached);
            }
        }
        for step in &call.steps {
            let expression = match step {
                LoweredCallStep::Callee { expression } => Some(*expression),
                LoweredCallStep::Argument { .. } | LoweredCallStep::Resource { .. } => None,
                LoweredCallStep::ProductElement { expression, .. }
                | LoweredCallStep::ProductSpread { expression, .. }
                | LoweredCallStep::NamedProductSpread { expression, .. }
                | LoweredCallStep::Default { expression, .. } => Some(*expression),
                LoweredCallStep::Invoke => None,
            };
            if let Some(expression) = expression {
                self.visit_owned_expression(expression, reached);
            }
        }
    }

    fn visit_owned_callable_value(&self, id: LoweredCallableValueId, reached: &mut Reachability) {
        let Some(value) = self.callable_values.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::CallableValue(id), &value.origin);
        if !reached.callable_values.insert(id) {
            return;
        }
        if let LoweredCallableTarget::IndirectClosure { callee } = &value.target {
            self.visit_owned_expression(*callee, reached);
        }
    }

    fn visit_owned_pattern(&self, id: PatternId, reached: &mut Reachability) {
        let Some(pattern) = self.patterns.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::Pattern(id), &pattern.origin);
        if !reached.patterns.insert(id) {
            return;
        }
        match &pattern.kind {
            LoweredPatternKind::Wildcard | LoweredPatternKind::Literal { .. } => {}
            LoweredPatternKind::Binding { .. } => {}
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.visit_owned_pattern(*element, reached);
                }
            }
            LoweredPatternKind::Nominal { argument, .. } => {
                self.visit_owned_pattern(*argument, reached);
            }
            LoweredPatternKind::At { binding, pattern } => {
                self.visit_owned_pattern(*binding, reached);
                self.visit_owned_pattern(*pattern, reached);
            }
        }
    }

    fn visit_owned_place(&self, id: PlaceId, reached: &mut Reachability) {
        let Some(place) = self.places.get(id) else {
            return;
        };
        reached.claim_current(OwnedNode::Place(id), &place.origin);
        if !reached.places.insert(id) {
            return;
        }
        match &place.kind {
            LoweredPlaceKind::Temporary { expression } => {
                self.visit_owned_expression(*expression, reached);
            }
            LoweredPlaceKind::Dereference { reference, .. } => {
                self.visit_owned_expression(*reference, reached);
            }
            LoweredPlaceKind::ProductElement { base, .. }
            | LoweredPlaceKind::Representation { base } => {
                self.visit_owned_place(*base, reached);
            }
            LoweredPlaceKind::Indexed { base, index } => {
                self.visit_owned_place(*base, reached);
                self.visit_owned_expression(*index, reached);
            }
            LoweredPlaceKind::Resource { use_ } => {
                self.visit_owned_resource_use(*use_, reached);
            }
            LoweredPlaceKind::Symbol { .. } | LoweredPlaceKind::CapturedCell { .. } => {}
        }
    }

    /// Replays a product's evaluation steps and checks that they fill exactly
    /// the final checked slots with slot/name agreement.
    fn validate_product_plan(
        &self,
        expression: &LoweredExpression,
        product: &LoweredProduct,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let slots = product.final_type.elements.len();
        if product.fields.len() != slots {
            diagnostics.push(Diagnostic::new(
                expression.origin.span.clone(),
                format!(
                    "product final layout has {} fields for {slots} slots",
                    product.fields.len()
                ),
            ));
        }
        let mut replayed = vec![None; slots];
        let check_slot = |diagnostics: &mut Vec<Diagnostic>, slot: usize| -> bool {
            if slot >= slots {
                diagnostics.push(Diagnostic::new(
                    expression.origin.span.clone(),
                    format!("product step targets out-of-range slot {slot} of {slots}"),
                ));
                false
            } else {
                true
            }
        };
        for step in &product.steps {
            match step {
                LoweredProductStep::Positional {
                    expression: child,
                    slot,
                } => {
                    if check_slot(diagnostics, *slot) {
                        replayed[*slot] = Some(*child);
                    }
                }
                LoweredProductStep::Designated {
                    name,
                    expression: child,
                    slot,
                } => {
                    if check_slot(diagnostics, *slot) {
                        if product.final_type.elements[*slot].name.as_deref() != Some(name.as_str())
                        {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                format!(
                                    "designated product step `{name}` targets a slot named `{:?}`",
                                    product.final_type.elements[*slot].name
                                ),
                            ));
                        }
                        replayed[*slot] = Some(*child);
                    }
                }
                LoweredProductStep::PositionalSpread {
                    expression: child,
                    mappings,
                } => {
                    for mapping in mappings {
                        if check_slot(diagnostics, mapping.slot) {
                            replayed[mapping.slot] = Some(*child);
                        }
                    }
                }
                LoweredProductStep::NamedSpread {
                    expression: child,
                    mappings,
                } => {
                    for mapping in mappings {
                        if check_slot(diagnostics, mapping.slot) {
                            if product.final_type.elements[mapping.slot].name.as_deref()
                                != Some(mapping.name.as_str())
                            {
                                diagnostics.push(Diagnostic::new(
                                    expression.origin.span.clone(),
                                    format!(
                                        "named product spread `{}` targets a slot named `{:?}`",
                                        mapping.name,
                                        product.final_type.elements[mapping.slot].name
                                    ),
                                ));
                            }
                            replayed[mapping.slot] = Some(*child);
                        }
                    }
                }
                LoweredProductStep::Default {
                    slot,
                    expression: child,
                    expected,
                } => {
                    if check_slot(diagnostics, *slot) {
                        let slot_type = &product.final_type.elements[*slot].value_type;
                        if !types_agree(expected, slot_type) {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                format!(
                                    "product default for slot {slot} expects `{expected}` but the slot is `{slot_type}`"
                                ),
                            ));
                        }
                        replayed[*slot] = Some(*child);
                    }
                }
            }
        }
        if replayed.len() == product.fields.len()
            && replayed
                .iter()
                .zip(&product.fields)
                .any(|(replayed, field)| replayed != &Some(*field))
        {
            diagnostics.push(Diagnostic::new(
                expression.origin.span.clone(),
                "product final layout disagrees with its replayed evaluation steps",
            ));
        }
    }

    /// Checks that an expression's checked coercion agrees with the node and,
    /// where statically available, with the child it coerces. Contextual
    /// default occurrences may carry a checked type from a different
    /// instantiation, so the child relation is only enforced for primary
    /// occurrences.
    fn validate_expression_coercion(
        &self,
        expression: &LoweredExpression,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(coercion) = &expression.coercion else {
            return;
        };
        // A plan must exist once the coercion types are concrete, and it must
        // equal a fresh computation from the recorded source and target.
        // Templates whose types still contain declared parameters may leave it
        // absent until materialization.
        match &expression.coercion_plan {
            Some(plan) => match LoweredCoercionPlan::plan(&coercion.source, &coercion.target) {
                Ok(expected) if *plan == expected => {}
                Ok(_) => diagnostics.push(Diagnostic::new(
                    expression.origin.span.clone(),
                    "expression coercion plan disagrees with its checked source and target",
                )),
                Err(message) => {
                    diagnostics.push(Diagnostic::new(expression.origin.span.clone(), message))
                }
            },
            None => {
                if instance_resolution::unresolved_type_problem(&coercion.source).is_none()
                    && instance_resolution::unresolved_type_problem(&coercion.target).is_none()
                {
                    diagnostics.push(Diagnostic::new(
                        expression.origin.span.clone(),
                        "expression coercion has no emission plan",
                    ));
                }
            }
        }
        if !matches!(expression.key.context, ExpressionContext::Primary) {
            return;
        }
        // A block can be checked more than once while inference converges, so
        // a recorded coercion may target a type the final expression pass
        // replaced. `satisfies` is single-pass, so its target is authoritative.
        if let LoweredExpressionKind::Satisfies(_) = expression.kind
            && coercion.target != expression.value_type
        {
            diagnostics.push(Diagnostic::new(
                expression.origin.span.clone(),
                format!(
                    "expression coercion target `{}` disagrees with its checked type `{}`",
                    coercion.target, expression.value_type
                ),
            ));
        }
        let coerced_child = match &expression.kind {
            LoweredExpressionKind::Satisfies(satisfies) => Some(satisfies.value),
            LoweredExpressionKind::Block(block) => {
                self.blocks.get(*block).and_then(|block| block.result)
            }
            _ => None,
        };
        if let Some(child) = coerced_child
            && let Some(child) = self.expressions.get(child)
            && child.value_type != coercion.source
            && child.value_type != CheckedType::Never
            && coercion.source != CheckedType::Never
        {
            diagnostics.push(Diagnostic::new(
                expression.origin.span.clone(),
                format!(
                    "expression coercion source `{}` disagrees with its child type `{}`",
                    coercion.source, child.value_type
                ),
            ));
        }
    }

    /// A pattern's test plan must equal a fresh computation from
    /// its recorded subject and kind, and every nested pattern must be
    /// connected to the subject its parent plan supplies. Templates whose
    /// types still contain parameters are recomputed at materialization.
    fn validate_pattern_test_plan(
        &self,
        pattern: &LoweredPattern,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let unresolved = |value_type: &CheckedType| {
            instance_resolution::unresolved_type_problem(value_type).is_some()
        };
        if unresolved(&pattern.test.subject) || unresolved(&pattern.value_type) {
            return;
        }
        let shape = pattern_plan_shape_from_kind(&pattern.kind);
        match pattern_test_plan(
            &pattern.test.subject,
            &pattern.value_type,
            &shape,
            &|id| self.types.get(id).and_then(|meta| meta.builtin),
            self.semantic_ids.string_representation.as_ref(),
        ) {
            Ok((expected, child_subjects)) => {
                if expected != pattern.test {
                    diagnostics.push(Diagnostic::new(
                        pattern.origin.span.clone(),
                        "pattern test plan disagrees with its checked subject and shape",
                    ));
                }
                let child_ids: Vec<PatternId> = match &pattern.kind {
                    LoweredPatternKind::Product { elements, .. } => elements.clone(),
                    LoweredPatternKind::Nominal { argument, .. } => vec![*argument],
                    LoweredPatternKind::At { binding, pattern } => vec![*binding, *pattern],
                    LoweredPatternKind::Wildcard
                    | LoweredPatternKind::Binding { .. }
                    | LoweredPatternKind::Literal { .. } => Vec::new(),
                };
                for (child, subject) in child_ids.iter().zip(&child_subjects) {
                    let Some(child) = self.patterns.get(*child) else {
                        continue;
                    };
                    if !unresolved(&child.test.subject) && child.test.subject != *subject {
                        diagnostics.push(Diagnostic::new(
                            child.origin.span.clone(),
                            "nested pattern test plan subject disagrees with its parent",
                        ));
                    }
                }
            }
            Err(message) => diagnostics.push(Diagnostic::new(pattern.origin.span.clone(), message)),
        }
    }

    /// Checks runtime metadata that requires the checked module: block results
    /// are never also statement items, and expression-statement discard/drop
    /// facts agree with the checked value type.
    fn validate_runtime_metadata(&self, module: &TypedModule) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, block) in self.blocks.iter() {
            let Some(result) = block.result else {
                continue;
            };
            let duplicated = block.items.iter().any(|item| {
                matches!(
                    self.items.get(*item).map(|item| &item.kind),
                    Some(LoweredItemKind::Expression(statement))
                        if statement.expression == result
                )
            });
            if duplicated {
                diagnostics.push(Diagnostic::new(
                    block.origin.span.clone(),
                    "lowered block result is also present as a statement item",
                ));
            }
        }
        for (_, item) in self.items.iter() {
            let LoweredItemKind::Expression(statement) = &item.kind else {
                continue;
            };
            let Some(expression) = self.expressions.get(statement.expression) else {
                continue;
            };
            if statement.drop_result != module.type_needs_drop(&expression.value_type) {
                diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    "expression statement discard/drop fact disagrees with its checked type",
                ));
            }
        }
        diagnostics
    }

    /// Cross-checks `MutateIndex` assignment dispatches against the lowered
    /// indexed place: the checked argument types must agree with the base,
    /// the position expression, and the element type.
    fn validate_mutation_dispatch_arguments(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, item) in self.items.iter() {
            let LoweredItemKind::Assignment(assignment) = &item.kind else {
                continue;
            };
            let Some(dispatch) = &assignment.mutate_index else {
                continue;
            };
            let Some(trait_id) = self
                .trait_methods
                .get(dispatch.method)
                .map(|method| method.trait_id)
            else {
                diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    "assignment `MutateIndex` dispatch method is missing from the trait method catalog",
                ));
                continue;
            };
            match &assignment.evidence {
                Some(evidence) if evidence_matches(evidence, trait_id, Some(dispatch.method)) => {}
                Some(_) => diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    "assignment `MutateIndex` evidence does not match its checked dispatch",
                )),
                None => diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    "assignment `MutateIndex` dispatch has no evidence recipe",
                )),
            }
            let Some(place) = self.places.get(assignment.target) else {
                continue;
            };
            let LoweredPlaceKind::Indexed { base, index } = &place.kind else {
                continue;
            };
            let base_type = self.places.get(*base).map(|base| &base.value_type);
            let position_type = self.expressions.get(*index).map(|index| &index.value_type);
            for (position, actual) in [
                (0usize, base_type),
                (1, position_type),
                (2, Some(&place.value_type)),
            ] {
                if let (Some(expected), Some(actual)) = (dispatch.arguments.get(position), actual)
                    && !types_agree(expected, actual)
                {
                    diagnostics.push(Diagnostic::new(
                        item.origin.span.clone(),
                        format!(
                            "`MutateIndex` dispatch argument {position} is `{expected}` but the lowered place operand is `{actual}`"
                        ),
                    ));
                }
            }
        }
        diagnostics
    }

    /// Traverses every lowered loop body once and checks that each
    /// break/continue item is owned by an enclosing loop at the depth it
    /// recorded. Orphaned exits and inconsistent nested depths diagnose.
    fn validate_loop_exits(&self) -> Vec<Diagnostic> {
        let mut reached = HashMap::<ItemId, usize>::new();
        let mut diagnostics = Vec::new();
        for (_, expression) in self.expressions.iter() {
            if let LoweredExpressionKind::Loop(loop_) = &expression.kind {
                self.collect_loop_items(loop_.body, loop_.depth, &mut reached, &mut diagnostics);
            }
        }
        for (item_id, item) in self.items.iter() {
            let recorded = match &item.kind {
                LoweredItemKind::Break(item) => Some(item.loop_depth),
                LoweredItemKind::Continue(item) => Some(item.loop_depth),
                _ => None,
            };
            let Some(recorded) = recorded else {
                continue;
            };
            match reached.get(&item_id) {
                Some(depth) if *depth == recorded => {}
                Some(depth) => diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    format!(
                        "break/continue targets loop depth {recorded} but is inside loop depth {depth}"
                    ),
                )),
                None => diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    "break/continue item is not owned by an enclosing lowered loop",
                )),
            }
        }
        diagnostics
    }

    fn collect_loop_items(
        &self,
        block: BlockId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(block) = self.blocks.get(block) else {
            return;
        };
        for item in &block.items {
            let item_id = *item;
            let Some(lowered) = self.items.get(item_id) else {
                continue;
            };
            match &lowered.kind {
                LoweredItemKind::Break(_) | LoweredItemKind::Continue(_) => {
                    reached.entry(item_id).or_insert(depth);
                }
                LoweredItemKind::Binding(binding) => {
                    if let Some(value) = binding.value {
                        self.collect_loop_expression(value, depth, reached, diagnostics);
                    }
                }
                LoweredItemKind::PatternBinding(binding) => {
                    self.collect_loop_expression(binding.value, depth, reached, diagnostics);
                }
                LoweredItemKind::Assignment(assignment) => {
                    self.collect_loop_place(assignment.target, depth, reached, diagnostics);
                    self.collect_loop_expression(assignment.value, depth, reached, diagnostics);
                }
                LoweredItemKind::Return(item) => {
                    self.collect_loop_expression(item.value, depth, reached, diagnostics);
                }
                LoweredItemKind::Expression(statement) => {
                    self.collect_loop_expression(statement.expression, depth, reached, diagnostics);
                }
            }
        }
        if let Some(result) = block.result {
            self.collect_loop_expression(result, depth, reached, diagnostics);
        }
    }

    fn collect_loop_expression(
        &self,
        expression: ExpressionId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(expression) = self.expressions.get(expression) else {
            return;
        };
        match &expression.kind {
            LoweredExpressionKind::Loop(inner) => {
                if inner.depth != depth + 1 {
                    diagnostics.push(Diagnostic::new(
                        expression.origin.span.clone(),
                        format!(
                            "nested loop records depth {} instead of {}",
                            inner.depth,
                            depth + 1
                        ),
                    ));
                }
                self.collect_loop_items(inner.body, inner.depth, reached, diagnostics);
            }
            LoweredExpressionKind::Block(block) => {
                self.collect_loop_items(*block, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.collect_loop_expression(satisfies.value, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Logical(logical) => {
                self.collect_loop_expression(logical.left, depth, reached, diagnostics);
                self.collect_loop_expression(logical.right, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Match(match_) => {
                self.collect_loop_expression(match_.subject, depth, reached, diagnostics);
                for arm in &match_.arms {
                    self.collect_loop_expression(arm.body, depth, reached, diagnostics);
                }
            }
            LoweredExpressionKind::Product(product) => {
                for step in &product.steps {
                    let child = match step {
                        LoweredProductStep::Positional { expression, .. }
                        | LoweredProductStep::Designated { expression, .. }
                        | LoweredProductStep::PositionalSpread { expression, .. }
                        | LoweredProductStep::NamedSpread { expression, .. }
                        | LoweredProductStep::Default { expression, .. } => *expression,
                    };
                    self.collect_loop_expression(child, depth, reached, diagnostics);
                }
            }
            LoweredExpressionKind::RepeatedProduct(repeated) => {
                self.collect_loop_expression(repeated.expression, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Access(access) => {
                self.collect_loop_expression(access.base, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Index(index) => {
                self.collect_loop_expression(index.base, depth, reached, diagnostics);
                self.collect_loop_expression(index.index, depth, reached, diagnostics);
            }
            LoweredExpressionKind::StringTemplate(template) => {
                for part in &template.parts {
                    if let LoweredStringTemplatePart::Interpolation(interpolation) = part {
                        self.collect_loop_expression(
                            interpolation.expression,
                            depth,
                            reached,
                            diagnostics,
                        );
                    }
                }
            }
            LoweredExpressionKind::Call(call) => {
                self.collect_loop_call(*call, depth, reached, diagnostics)
            }
            LoweredExpressionKind::CallableValue(value) => {
                self.collect_loop_callable_value(*value, depth, reached, diagnostics)
            }
            LoweredExpressionKind::With(with) => {
                if let Some(with) = self.withs.get(*with) {
                    self.collect_loop_expression(with.value, depth, reached, diagnostics);
                    self.collect_loop_items(with.body, depth, reached, diagnostics);
                }
            }
            LoweredExpressionKind::Coro(coro) => {
                if let Some(plan) = self
                    .coros
                    .get(*coro)
                    .and_then(|coro| self.coroutine_plans.get(coro.plan))
                {
                    if let Some(body) = plan.body {
                        self.collect_loop_items(body, depth, reached, diagnostics);
                    }
                }
            }
            LoweredExpressionKind::Await(await_) => {
                if let Some(await_) = self.awaits.get(*await_) {
                    self.collect_loop_expression(await_.operand, depth, reached, diagnostics);
                }
            }
            LoweredExpressionKind::Deferred(_)
            | LoweredExpressionKind::Resource(_)
            | LoweredExpressionKind::Name(_)
            | LoweredExpressionKind::Integer(_)
            | LoweredExpressionKind::Float(_)
            | LoweredExpressionKind::String(_)
            | LoweredExpressionKind::CString(_) => {}
        }
    }

    fn collect_loop_call(
        &self,
        id: LoweredCallId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(call) = self.calls.get(id) else {
            return;
        };
        if let Some(callee) = call.callee {
            self.collect_loop_expression(callee, depth, reached, diagnostics);
        }
        for argument in &call.arguments {
            if let Some(expression) = argument.expression {
                self.collect_loop_expression(expression, depth, reached, diagnostics);
            }
            if let Some(place) = argument.place {
                self.collect_loop_place(place, depth, reached, diagnostics);
            }
        }
        for step in &call.steps {
            let expression = match step {
                LoweredCallStep::Callee { expression } => Some(*expression),
                LoweredCallStep::Argument { .. } | LoweredCallStep::Resource { .. } => None,
                LoweredCallStep::ProductElement { expression, .. }
                | LoweredCallStep::ProductSpread { expression, .. }
                | LoweredCallStep::NamedProductSpread { expression, .. }
                | LoweredCallStep::Default { expression, .. } => Some(*expression),
                LoweredCallStep::Invoke => None,
            };
            if let Some(expression) = expression {
                self.collect_loop_expression(expression, depth, reached, diagnostics);
            }
        }
    }

    fn collect_loop_callable_value(
        &self,
        id: LoweredCallableValueId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(value) = self.callable_values.get(id) else {
            return;
        };
        if let LoweredCallableTarget::IndirectClosure { callee } = &value.target {
            self.collect_loop_expression(*callee, depth, reached, diagnostics);
        }
    }

    fn collect_loop_place(
        &self,
        place: PlaceId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(place) = self.places.get(place) else {
            return;
        };
        match &place.kind {
            LoweredPlaceKind::Temporary { expression } => {
                self.collect_loop_expression(*expression, depth, reached, diagnostics);
            }
            LoweredPlaceKind::Dereference { reference, .. } => {
                self.collect_loop_expression(*reference, depth, reached, diagnostics);
            }
            LoweredPlaceKind::ProductElement { base, .. }
            | LoweredPlaceKind::Representation { base } => {
                self.collect_loop_place(*base, depth, reached, diagnostics);
            }
            LoweredPlaceKind::Indexed { base, index } => {
                self.collect_loop_place(*base, depth, reached, diagnostics);
                self.collect_loop_expression(*index, depth, reached, diagnostics);
            }
            LoweredPlaceKind::Symbol { .. }
            | LoweredPlaceKind::CapturedCell { .. }
            | LoweredPlaceKind::Resource { .. } => {}
        }
    }

    fn validate_modules_and_initializers(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut initializer_counts = HashMap::<ModuleId, usize>::new();
        for (_, initializer) in self.initializers.iter() {
            *initializer_counts.entry(initializer.module).or_default() += 1;
            if !self.blocks.contains(initializer.body) {
                diagnostics.push(invalid_reference(
                    &initializer.origin,
                    "initializer",
                    "block",
                    initializer.body.index(),
                ));
            }
            if self.modules.get(initializer.module).is_none() {
                diagnostics.push(invalid_reference(
                    &initializer.origin,
                    "initializer",
                    "module",
                    initializer.module.0,
                ));
            }
        }
        for (position, (_, key, info)) in self.modules.iter().enumerate() {
            if info.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "module catalog key {key:?} disagrees with stored semantic id {:?}",
                        info.semantic_id
                    ),
                ));
            }
            if info.initialization_index != position {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "module {key:?} has initialization index {} instead of {position}",
                        info.initialization_index
                    ),
                ));
            }
            if let Some(parent) = info.parent
                && self.modules.get(parent).is_none()
            {
                diagnostics.push(invalid_reference(
                    &info.origin,
                    "module",
                    "module",
                    parent.0,
                ));
            }
            if !self.initializers.contains(info.initializer) {
                diagnostics.push(invalid_reference(
                    &info.origin,
                    "module",
                    "initializer",
                    info.initializer.index(),
                ));
            } else if let Some(initializer) = self.initializers.get(info.initializer)
                && initializer.module != key
            {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "module {key:?} points at an initializer for module {:?}",
                        initializer.module
                    ),
                ));
            }
            match initializer_counts.get(&key).copied() {
                Some(1) => {}
                Some(count) => diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!("module {key:?} has {count} initializers instead of one"),
                )),
                None => diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!("module {key:?} has no initializer"),
                )),
            }
        }
        diagnostics
    }

    fn validate_functions(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, key, function) in self.functions.iter() {
            if function.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    function.origin.span.clone(),
                    format!(
                        "function catalog key {key:?} disagrees with stored semantic id {:?}",
                        function.semantic_id
                    ),
                ));
            }
            if self.modules.get(function.module).is_none() {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "module",
                    function.module.0,
                ));
            }
            if let Some(body) = function.body
                && !self.blocks.contains(body)
            {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "block",
                    body.index(),
                ));
            }
            if !self.patterns.contains(function.parameter_pattern) {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "pattern",
                    function.parameter_pattern.index(),
                ));
            }
            if function.body_origin.syntax != function.body_syntax {
                diagnostics.push(Diagnostic::new(
                    function.origin.span.clone(),
                    format!(
                        "function {key:?} body origin syntax does not match its body syntax id"
                    ),
                ));
            }
            for symbol in function
                .parameters
                .iter()
                .chain(function.captures.iter().map(|capture| &capture.symbol))
            {
                if self.symbols.get(*symbol).is_none() {
                    diagnostics.push(invalid_reference(
                        &function.origin,
                        "function",
                        "symbol",
                        symbol.0,
                    ));
                }
            }
            if let Some(binding) = function.binding_symbol
                && self.symbols.get(binding).is_none()
            {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "symbol",
                    binding.0,
                ));
            }
        }
        diagnostics
    }

    fn validate_symbols(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, key, symbol) in self.symbols.iter() {
            if symbol.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!(
                        "symbol catalog key {key:?} disagrees with stored semantic id {:?}",
                        symbol.semantic_id
                    ),
                ));
            }
            if self.modules.get(symbol.module).is_none() {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "module",
                    symbol.module.0,
                ));
            }
            if let Some(owner) = symbol.owner
                && self.functions.get(owner).is_none()
            {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "function",
                    owner.0,
                ));
            }
            if let Some(function) = symbol.function
                && self.functions.get(function).is_none()
            {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "function",
                    function.0,
                ));
            }
            if let Some(constructor) = symbol.constructor
                && self.types.get(constructor).is_none()
            {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "type",
                    constructor.0,
                ));
            }
            if let Some(singleton) = symbol.singleton
                && self.types.get(singleton).is_none()
            {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "type",
                    singleton.0,
                ));
            }
            let binding = symbol.function.is_some()
                || symbol.constructor.is_some()
                || symbol.singleton.is_some();
            let consistent = match symbol.storage {
                SymbolStorage::ExternalSymbol => symbol.external,
                SymbolStorage::FunctionBinding => binding,
                SymbolStorage::DerivedBinding => symbol.derived,
                SymbolStorage::Signal => symbol.signal,
                SymbolStorage::CapturedCell => symbol.captured_cell,
                SymbolStorage::GlobalStorage
                | SymbolStorage::MutableCell
                | SymbolStorage::ImmutableValue => true,
            };
            if !consistent {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!(
                        "symbol {key:?} storage {:?} disagrees with its classification flags",
                        symbol.storage
                    ),
                ));
            }
            // Module storage facts: a global-storage symbol is module-scoped,
            // a local cell never is, an emitted global has a declared name,
            // and the harness root-region fact is exactly "has a global whose
            // type contains a managed reference".
            if symbol.storage == SymbolStorage::GlobalStorage && !symbol.module_symbol {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!("symbol {key:?} has global storage but is not module-scoped"),
                ));
            }
            if matches!(
                symbol.storage,
                SymbolStorage::MutableCell | SymbolStorage::ImmutableValue
            ) && symbol.module_symbol
            {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!("symbol {key:?} is module-scoped but has no global storage"),
                ));
            }
            if symbol.has_global && !symbol.module_symbol {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!("symbol {key:?} has a module global but is not module-scoped"),
                ));
            }
            if symbol.has_global && symbol.name.is_empty() {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!("symbol {key:?} has a module global without a declared name"),
                ));
            }
            let expected_root = symbol.has_global && checked_type_contains_ref(&symbol.value_type);
            if symbol.global_root != expected_root {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!(
                        "symbol {key:?} global-root fact {} disagrees with `has_global` and its type",
                        symbol.global_root
                    ),
                ));
            }
        }
        diagnostics
    }

    fn validate_types(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, key, info) in self.types.iter() {
            if info.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "type catalog key {key:?} disagrees with stored semantic id {:?}",
                        info.semantic_id
                    ),
                ));
            }
            if self.modules.get(info.module).is_none() {
                diagnostics.push(invalid_reference(
                    &info.origin,
                    "type",
                    "module",
                    info.module.0,
                ));
            }
        }
        diagnostics
    }

    fn validate_traits(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, key, info) in self.traits.iter() {
            if info.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "trait catalog key {key:?} disagrees with stored semantic id {:?}",
                        info.semantic_id
                    ),
                ));
            }
            if self.modules.get(info.module).is_none() {
                diagnostics.push(invalid_reference(
                    &info.origin,
                    "trait",
                    "module",
                    info.module.0,
                ));
            }
            for method in &info.methods {
                match self.trait_methods.get(*method) {
                    Some(record) if record.trait_id == key => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        info.origin.span.clone(),
                        format!("trait {key:?} lists method {method:?} owned by another trait"),
                    )),
                    None => diagnostics.push(invalid_reference(
                        &info.origin,
                        "trait",
                        "trait method",
                        method.0,
                    )),
                }
            }
            for (method, function) in &info.default_methods {
                if !info.methods.contains(method) {
                    diagnostics.push(Diagnostic::new(
                        info.origin.span.clone(),
                        format!(
                            "trait {key:?} has a default function for undeclared method {method:?}"
                        ),
                    ));
                }
                if self.functions.get(*function).is_none() {
                    diagnostics.push(invalid_reference(
                        &info.origin,
                        "trait",
                        "function",
                        function.0,
                    ));
                }
            }
        }
        for (_, key, method) in self.trait_methods.iter() {
            if method.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    method.origin.span.clone(),
                    format!(
                        "trait method catalog key {key:?} disagrees with stored semantic id {:?}",
                        method.semantic_id
                    ),
                ));
            }
            if self.traits.get(method.trait_id).is_none() {
                diagnostics.push(invalid_reference(
                    &method.origin,
                    "trait method",
                    "trait",
                    method.trait_id.0,
                ));
            }
            if let Some(default) = method.default_function
                && self.functions.get(default).is_none()
            {
                diagnostics.push(invalid_reference(
                    &method.origin,
                    "trait method",
                    "function",
                    default.0,
                ));
            }
        }
        for (_, implementation) in self.trait_implementations.iter() {
            if self.traits.get(implementation.trait_id).is_none() {
                diagnostics.push(invalid_reference(
                    &implementation.origin,
                    "trait implementation",
                    "trait",
                    implementation.trait_id.0,
                ));
            }
            for (method, function) in &implementation.methods {
                match self.trait_methods.get(*method) {
                    Some(record) if record.trait_id == implementation.trait_id => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        implementation.origin.span.clone(),
                        format!(
                            "implementation of trait {:?} selects a method from another trait",
                            implementation.trait_id
                        ),
                    )),
                    None => diagnostics.push(invalid_reference(
                        &implementation.origin,
                        "trait implementation",
                        "trait method",
                        method.0,
                    )),
                }
                if self.functions.get(*function).is_none() {
                    diagnostics.push(invalid_reference(
                        &implementation.origin,
                        "trait implementation",
                        "function",
                        function.0,
                    ));
                }
            }
        }
        diagnostics
    }
}

/// Checks that every present standard/runtime ID resolves to the matching
/// catalog family and that resource selections agree with their type IDs.
/// A diagnostic for a broken compiler invariant: a precondition an earlier
/// phase or step establishes, which no source program can violate. Lowering and
/// emission report it instead of panicking, naming the invariant.
pub(crate) fn internal_invariant(span: Span, invariant: &str) -> Diagnostic {
    Diagnostic::new(span, format!("internal invariant violated: {invariant}"))
}

/// Decides which arguments of one call are temporaries the caller drops after
/// the call. An argument with no source place that is not an implicit thunk is
/// owned by the call site. When its parameter is mutated, or borrowed (neither
/// `move` nor `mut`), the callee does not take ownership, so the caller drops
/// it once the call returns. A `move` parameter takes ownership instead.
///
/// Intrinsic and constructor targets consume or copy their arguments
/// themselves and never get caller drops. A borrowed slot must also have its
/// own ABI slot and its own operand: product-spread and whole-product slots
/// share one operand, which this rule leaves to its owner.
pub(crate) fn mark_call_temporary_drops(
    target: &LoweredCallableTarget,
    function_type: &CheckedFunctionType,
    arguments: &mut [LoweredCallArgument],
    operand_type: impl Fn(ExpressionId) -> Option<CheckedType>,
    needs_drop: impl Fn(&CheckedType) -> bool,
) {
    let caller_drops = !matches!(
        target,
        LoweredCallableTarget::Intrinsic { .. } | LoweredCallableTarget::Constructor { .. }
    );
    let whole_mutation = function_type.mutations.contains(&CheckedMutation::Whole);
    let whole_move = function_type.moves.contains(&CheckedMutation::Whole);
    let mut operand_uses = HashMap::<ExpressionId, usize>::new();
    for expression in arguments.iter().filter_map(|argument| argument.expression) {
        *operand_uses.entry(expression).or_default() += 1;
    }
    for argument in arguments.iter_mut() {
        let element = |slot: usize| CheckedMutation::Element(slot);
        let mutation = whole_mutation
            || argument
                .slot
                .is_some_and(|slot| function_type.mutations.contains(&element(slot)));
        let moved = whole_move
            || argument
                .slot
                .is_some_and(|slot| function_type.moves.contains(&element(slot)));
        let own_operand = argument.slot.is_some()
            && argument
                .expression
                .is_some_and(|expression| operand_uses.get(&expression) == Some(&1));
        let borrowed = !mutation && !moved && own_operand;
        argument.drops_after_call = caller_drops
            && argument.place.is_none()
            && argument.thunk.is_none()
            && (mutation || borrowed)
            && needs_drop(&call_temporary_drop_type(
                &argument.expected,
                argument.expression.and_then(&operand_type).as_ref(),
            ));
    }
}

/// The type a call temporary is dropped as: its parameter type, except that a
/// `CString` passed where `CPointer CChar` is expected is the same pointer seen
/// without ownership, so the temporary is still dropped as the owned
/// `CString`.
pub(crate) fn call_temporary_drop_type(
    expected: &CheckedType,
    operand: Option<&CheckedType>,
) -> CheckedType {
    match (expected, operand) {
        (CheckedType::CPointer { pointee }, Some(CheckedType::CString))
            if **pointee == CheckedType::CChar =>
        {
            CheckedType::CString
        }
        _ => expected.clone(),
    }
}

fn validate_semantic_ids(
    ids: &LoweredSemanticIds,
    types: &Catalog<TypeId, LoweredTypeMetadata, LoweredTypeId>,
    traits: &Catalog<TraitId, LoweredTraitMetadata, LoweredTraitId>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (slot, id) in [
        ("natural", ids.natural_trait),
        ("sized", ids.sized_trait),
        ("copy", ids.copy_trait),
        ("clone", ids.clone_trait),
        ("drop", ids.drop_trait),
        ("default", ids.default_trait),
        ("debug", ids.debug_trait),
        ("display", ids.display_trait),
        ("index", ids.index_trait),
        ("mutate_index", ids.mutate_index_trait),
        ("into_iterator", ids.into_iterator_trait),
        ("iterator", ids.iterator_trait),
    ] {
        if let Some(id) = id
            && traits.get(id).is_none()
        {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("lowered semantic {slot} trait {id:?} has no trait catalog record"),
            ));
        }
    }
    for (slot, id) in [
        ("io", ids.io_type),
        ("reactive", ids.reactive_type),
        ("coroutine", ids.coroutine_type),
        ("task", ids.task_type),
        ("completed", ids.completed_type),
        ("cancelled", ids.cancelled_type),
        ("tasks", ids.tasks_type),
        ("scheduler", ids.scheduler_type),
        ("wait", ids.wait_type),
        ("resolver", ids.resolver_type),
        ("completion token", ids.completion_token_type),
    ] {
        if let Some(id) = id
            && types.get(id).is_none()
        {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("lowered semantic {slot} type {id:?} has no type catalog record"),
            ));
        }
    }
    for (slot, resource, expected) in [
        ("io", &ids.io_resource, ids.io_type),
        ("reactive", &ids.reactive_resource, ids.reactive_type),
    ] {
        if let Some(resource) = resource
            && nominal_type_id(&resource.value_type) != expected
        {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("lowered semantic {slot} resource does not match its selected type id"),
            ));
        }
    }
    diagnostics
}

/// Checks that the initialization order lists every loaded module exactly
/// once, diagnosing unknown, duplicate, and missing entries.
fn validate_initialization_order(sources: &[SourceModule], order: &[ModuleId]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen = vec![false; sources.len()];
    for module_id in order {
        if module_id.0 >= sources.len() {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "initialization order references unknown module id {}",
                    module_id.0
                ),
            ));
            continue;
        }
        let source = &sources[module_id.0];
        if std::mem::replace(&mut seen[module_id.0], true) {
            diagnostics.push(Diagnostic::new(
                module_origin(source).span,
                format!(
                    "module `{}` appears more than once in initialization order",
                    source.qualified_name
                ),
            ));
        }
    }
    for (index, was_seen) in seen.iter().enumerate() {
        if !was_seen {
            let source = &sources[index];
            diagnostics.push(Diagnostic::new(
                module_origin(source).span,
                format!(
                    "module `{}` is missing from initialization order",
                    source.qualified_name
                ),
            ));
        }
    }
    diagnostics
}

/// A module's declaration syntax when it has one; file-backed modules without
/// a `mod` declaration use their module syntax origin instead.
fn module_origin(module: &SourceModule) -> Origin {
    let syntax = module
        .syntax
        .declaration_syntax
        .as_ref()
        .unwrap_or(&module.syntax.syntax);
    Origin {
        syntax: syntax.id,
        span: syntax.span.clone(),
    }
}

/// The single, exhaustive lowering decision for a syntax variant. A new
/// `Expression` variant fails to compile here until it has an explicit
/// owned/deferred/rejected decision, and the coverage classifier test keeps
/// the enumerated variant list in agreement.
fn classify_expression(module: &TypedModule, expression: &Expression) -> ExpressionDisposition {
    use ExpressionDisposition::{Ordinary, Rejected, ResourceCoroutine};
    use OrdinaryExpressionFamily as Family;
    use ResourceCoroutineRoute as Route;
    match expression {
        Expression::Function(_) => Ordinary(Family::Function),
        Expression::Call(_) => Ordinary(Family::Call),
        Expression::Satisfies(_) => Ordinary(Family::Satisfies),
        Expression::Match(_) => Ordinary(Family::Match),
        Expression::Loop(_) => Ordinary(Family::Loop),
        Expression::Coro(_) => ResourceCoroutine(Route::CoroutineCreation),
        Expression::Await(await_) => ResourceCoroutine(classify_await_route(module, await_)),
        Expression::Resource(_) => ResourceCoroutine(Route::ResourceUse),
        Expression::With(_) => ResourceCoroutine(Route::ResourceProvider),
        Expression::Block(_) => Ordinary(Family::Block),
        Expression::Product(_) => Ordinary(Family::Product),
        Expression::RepeatedProduct(_) => Ordinary(Family::RepeatedProduct),
        Expression::Access(_) => Ordinary(Family::Access),
        Expression::Index(_) => Ordinary(Family::Index),
        Expression::Unary(_) | Expression::Binary(_) => Rejected,
        Expression::Logical(_) => Ordinary(Family::Logical),
        Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Quote(_)
        | Expression::Splice(_) => Rejected,
        Expression::Name(_) => Ordinary(Family::Name),
        Expression::String(_) => Ordinary(Family::String),
        Expression::StringTemplate(_) => Ordinary(Family::StringTemplate),
        Expression::CString(_) => Ordinary(Family::CString),
        Expression::Integer(_) => Ordinary(Family::Integer),
        Expression::Float(_) => Ordinary(Family::Float),
    }
}

/// The route of an `await` operand: external `Task` and `Wait` handles park on
/// an external record, everything else is a child coroutine frame. The
/// checked operand type selects the route exactly as code generation does.
fn classify_await_route(
    module: &TypedModule,
    await_: &staple_syntax::AwaitExpression,
) -> ResourceCoroutineRoute {
    let Some(operand_type) = module.type_of_expression(await_.operand.syntax().id) else {
        return ResourceCoroutineRoute::AwaitChildCoroutine;
    };
    if module.task_result(operand_type).is_some() {
        return ResourceCoroutineRoute::AwaitTask;
    }
    if module.wait_result(operand_type).is_some() {
        return ResourceCoroutineRoute::AwaitWait;
    }
    ResourceCoroutineRoute::AwaitChildCoroutine
}

/// Whether an `await` operand is a call to the `until` intrinsic. Mirrors the
/// coroutine scanner's single-element-product look-through.
fn await_operand_is_until(module: &TypedModule, operand: &Expression) -> bool {
    let operand = match operand {
        Expression::Product(product) if product.elements.len() == 1 => &product.elements[0].value,
        other => other,
    };
    let Expression::Call(call) = operand else {
        return false;
    };
    module
        .symbol_for(call.callee.syntax().id)
        .and_then(|symbol| module.resolved().intrinsic_function(symbol))
        == Some(IntrinsicFunction::Until)
}

/// The reactive or coroutine route of one compiler intrinsic. The exhaustive
/// match fails to compile when a new intrinsic lacks a route.
fn intrinsic_route(intrinsic: IntrinsicFunction) -> Option<IntrinsicRoute> {
    use crate::IntrinsicFunction as Intrinsic;
    match intrinsic {
        Intrinsic::ReactiveScope => Some(IntrinsicRoute::Reactive(ReactiveIntrinsicRoute::Scope)),
        Intrinsic::Reaction => Some(IntrinsicRoute::Reactive(ReactiveIntrinsicRoute::Reaction)),
        Intrinsic::Batch => Some(IntrinsicRoute::Reactive(ReactiveIntrinsicRoute::Batch)),
        Intrinsic::Until => Some(IntrinsicRoute::Reactive(ReactiveIntrinsicRoute::Until)),
        Intrinsic::Snapshot => Some(IntrinsicRoute::Reactive(ReactiveIntrinsicRoute::Snapshot)),
        Intrinsic::CoroutineBlockOn => {
            Some(IntrinsicRoute::Coroutine(CoroutineIntrinsicRoute::BlockOn))
        }
        Intrinsic::SchedulerCreate => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::SchedulerCreate,
        )),
        Intrinsic::TaskScope => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::TaskScope,
        )),
        Intrinsic::Spawn => Some(IntrinsicRoute::Coroutine(CoroutineIntrinsicRoute::Spawn)),
        Intrinsic::Pump => Some(IntrinsicRoute::Coroutine(CoroutineIntrinsicRoute::Pump)),
        Intrinsic::YieldNow => Some(IntrinsicRoute::Coroutine(CoroutineIntrinsicRoute::YieldNow)),
        Intrinsic::TaskIsFinished => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::TaskIsFinished,
        )),
        Intrinsic::TaskCancel => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::TaskCancel,
        )),
        Intrinsic::Completion => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::Completion,
        )),
        Intrinsic::CompletionWithCancel => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::CompletionWithCancel,
        )),
        Intrinsic::CompletionToken => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::CompletionToken,
        )),
        Intrinsic::CompletionTokenResolve => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::CompletionTokenResolve,
        )),
        Intrinsic::CompletionTokenCancel => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::CompletionTokenCancel,
        )),
        Intrinsic::ResolverComplete => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::ResolverComplete,
        )),
        Intrinsic::ResolverCancel => Some(IntrinsicRoute::Coroutine(
            CoroutineIntrinsicRoute::ResolverCancel,
        )),
        Intrinsic::ToString { .. }
        | Intrinsic::IntegerBinary { .. }
        | Intrinsic::IntegerCompare { .. }
        | Intrinsic::FloatBinary { .. }
        | Intrinsic::FloatCompare { .. }
        | Intrinsic::StringFromCString
        | Intrinsic::StringToCString
        | Intrinsic::StringAdd
        | Intrinsic::SliceLength
        | Intrinsic::SliceGetRef
        | Intrinsic::BufferWithCapacity
        | Intrinsic::BufferLength
        | Intrinsic::BufferCapacity
        | Intrinsic::BufferPush
        | Intrinsic::BufferPop
        | Intrinsic::BufferGet
        | Intrinsic::BufferFreeze
        | Intrinsic::BufferTransfer
        | Intrinsic::BufferClone
        | Intrinsic::RefReplace
        | Intrinsic::Drop => None,
    }
}

/// The stable name of a ordinary expression family.
fn family_name(family: OrdinaryExpressionFamily) -> &'static str {
    match family {
        OrdinaryExpressionFamily::Block => "Block",
        OrdinaryExpressionFamily::Satisfies => "Satisfies",
        OrdinaryExpressionFamily::Match => "Match",
        OrdinaryExpressionFamily::Loop => "Loop",
        OrdinaryExpressionFamily::Product => "Product",
        OrdinaryExpressionFamily::RepeatedProduct => "RepeatedProduct",
        OrdinaryExpressionFamily::Access => "Access",
        OrdinaryExpressionFamily::Index => "Index",
        OrdinaryExpressionFamily::Logical => "Logical",
        OrdinaryExpressionFamily::Name => "Name",
        OrdinaryExpressionFamily::String => "String",
        OrdinaryExpressionFamily::StringTemplate => "StringTemplate",
        OrdinaryExpressionFamily::CString => "CString",
        OrdinaryExpressionFamily::Integer => "Integer",
        OrdinaryExpressionFamily::Float => "Float",
        OrdinaryExpressionFamily::Function => "Function",
        OrdinaryExpressionFamily::Call => "Call",
    }
}

/// The stable name of an expression variant, used by coverage tests and
/// diagnostics. The exhaustive match is the compile-time half of the
/// coverage gate: adding an `Expression` variant fails to compile until it
/// is named here and classified by `classify_expression`.
fn expression_variant_name(expression: &Expression) -> &'static str {
    match expression {
        Expression::Function(_) => "Function",
        Expression::Satisfies(_) => "Satisfies",
        Expression::Match(_) => "Match",
        Expression::Loop(_) => "Loop",
        Expression::Coro(_) => "Coro",
        Expression::Await(_) => "Await",
        Expression::Resource(_) => "Resource",
        Expression::With(_) => "With",
        Expression::Block(_) => "Block",
        Expression::Product(_) => "Product",
        Expression::RepeatedProduct(_) => "RepeatedProduct",
        Expression::Call(_) => "Call",
        Expression::Access(_) => "Access",
        Expression::Index(_) => "Index",
        Expression::Unary(_) => "Unary",
        Expression::Binary(_) => "Binary",
        Expression::Logical(_) => "Logical",
        Expression::SyntaxArgument(_) => "SyntaxArgument",
        Expression::VisibilityArgument(_) => "VisibilityArgument",
        Expression::Quote(_) => "Quote",
        Expression::Splice(_) => "Splice",
        Expression::Name(_) => "Name",
        Expression::String(_) => "String",
        Expression::StringTemplate(_) => "StringTemplate",
        Expression::CString(_) => "CString",
        Expression::Integer(_) => "Integer",
        Expression::Float(_) => "Float",
    }
}

/// Rejects compile-time-only expression nodes that earlier phases must have
/// eliminated. These are lowering diagnostics rather than backend panics.
fn reject_compile_time_expression(expression: &Expression) -> Result<(), Diagnostic> {
    let message = match expression {
        Expression::Unary(unary) => format!(
            "unresolved `{}` operator expression reached lowering",
            unary.operator.text()
        ),
        Expression::Binary(binary) => format!(
            "unresolved `{}` operator expression reached lowering",
            binary.operator.text()
        ),
        Expression::Quote(quote) => {
            format!(
                "unexpanded `{}` expression reached lowering",
                quote.kind.name()
            )
        }
        Expression::Splice(_) => "unexpanded splice expression reached lowering".to_owned(),
        Expression::SyntaxArgument(_) => {
            "unexpanded grouped syntax argument reached lowering".to_owned()
        }
        Expression::VisibilityArgument(_) => "visibility syntax reached lowering".to_owned(),
        _ => return Ok(()),
    };
    Err(Diagnostic::new(expression.syntax().span.clone(), message))
}

/// The function a runtime owner lowers inside, if any. Module initializers
/// have no function context for `Copy`/borrow decisions.
fn owner_function(owner: ExpressionOwner) -> Option<FunctionId> {
    match owner {
        ExpressionOwner::Function(function) => Some(function),
        ExpressionOwner::Module(_) => None,
    }
}

/// Whether an expression is a direct symbol or an access chain rooted in one.
fn expression_has_place_root(module: &ResolvedModule, expression: &Expression) -> bool {
    if module.symbol_for(expression.syntax().id).is_some() {
        return true;
    }
    match expression {
        Expression::Access(access) => expression_has_place_root(module, &access.value),
        _ => false,
    }
}

fn entry_resources(module: &TypedModule) -> Vec<LoweredEntryResource> {
    let mut resources = Vec::new();
    if let Some(resource) = module.io_resource() {
        resources.push(LoweredEntryResource {
            kind: LoweredEntryResourceKind::Io,
            resource,
        });
    }
    if module.entry_reactive_required()
        && let Some(resource) = module.reactive_resource()
    {
        resources.push(LoweredEntryResource {
            kind: LoweredEntryResourceKind::Reactive,
            resource,
        });
    }
    resources
}

fn capture_requires_cell(module: &TypedModule, symbol: SymbolId) -> bool {
    module.resolved().requires_initialization_state(symbol)
        || module.has_mutable_storage(symbol)
        || module.is_derived_symbol(symbol)
}

/// Whether a symbol's place is reached through a shared binding cell rather
/// than direct storage. Mutable/derived/initialization-checked locals and
/// captures use cells; module symbols live in global storage and mutated
/// parameters arrive as caller-provided pointers.

/// Collects binding symbols from a resolved pattern in source order, matching
/// how destructuring patterns bind symbols. Used for function parameters and
/// top-level pattern bindings.
fn pattern_symbols(module: &ResolvedModule, pattern: &Pattern) -> Vec<SymbolId> {
    fn collect(module: &ResolvedModule, pattern: &Pattern, symbols: &mut Vec<SymbolId>) {
        match pattern {
            Pattern::Binding(binding) => {
                if let Some(symbol) = module.symbol_for(binding.syntax.id) {
                    symbols.push(symbol);
                }
            }
            Pattern::At(at) => {
                if let Some(symbol) = module.symbol_for(at.binding.syntax.id) {
                    symbols.push(symbol);
                }
                collect(module, &at.pattern, symbols);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    collect(module, element, symbols);
                }
            }
            Pattern::Nominal(nominal) => collect(module, &nominal.argument, symbols),
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
        }
    }
    let mut symbols = Vec::new();
    collect(module, pattern, &mut symbols);
    symbols
}

fn nominal_type_id(value_type: &CheckedType) -> Option<TypeId> {
    match value_type {
        CheckedType::TypeConstructor { id, .. }
        | CheckedType::Opaque { id, .. }
        | CheckedType::Distinct { id, .. } => Some(*id),
        _ => None,
    }
}

/// Compile-time-only symbols stay out of the runtime catalog: constructors of
/// compiler-owned syntax types. `const` bindings are *not* compile-time-only:
/// their values are folded during checking but the backend still materializes
/// and reads a module global for every runtime reference, so they are ordinary
/// global-storage symbols here.
fn compile_time_only_symbol(module: &TypedModule, symbol: SymbolId) -> bool {
    let resolved = module.resolved();
    resolved.constructor_type(symbol).is_some_and(|id| {
        resolved.recursive_construction(id) == Some(crate::RecursiveConstruction::Syntax)
    })
}

/// Declaration facts snapshotted from module syntax:
/// each module-level symbol's declared name, and whether the backend declares
/// module-level storage for it. Collected once during symbol snapshotting, so
/// the emitter reads records instead of syntax.
struct SymbolDeclarationFacts {
    names: HashMap<SymbolId, String>,
    globals: HashSet<SymbolId>,
}

impl SymbolDeclarationFacts {
    fn collect(module: &TypedModule) -> Self {
        let resolved = module.resolved();
        let mut names = HashMap::new();
        let mut globals = HashSet::new();
        for source in resolved.program().modules() {
            for item in &source.syntax.items {
                match item {
                    Item::Binding(binding) => {
                        let Some(symbol) = resolved.symbol_for(binding.syntax.id) else {
                            continue;
                        };
                        names.insert(symbol, binding.name.clone());
                        // The emitter skips generic bindings (no storage global) and
                        // symbols that already have a declaration global
                        // (externs are predeclared before storage runs).
                        if binding.type_parameters.is_empty()
                            && !resolved.is_external_symbol(symbol)
                        {
                            globals.insert(symbol);
                        }
                    }
                    Item::ExternBlock(block) => {
                        for binding in &block.bindings {
                            if let Some(symbol) = resolved.symbol_for(binding.syntax.id) {
                                names.insert(symbol, binding.name.clone());
                            }
                        }
                    }
                    Item::PatternBinding(binding) => {
                        collect_pattern_facts(resolved, &binding.pattern, &mut names, &mut globals);
                    }
                    _ => {}
                }
            }
        }
        SymbolDeclarationFacts { names, globals }
    }
}

/// Collects declared names and storage symbols from a module-level pattern.
fn collect_pattern_facts(
    resolved: &ResolvedModule,
    pattern: &Pattern,
    names: &mut HashMap<SymbolId, String>,
    globals: &mut HashSet<SymbolId>,
) {
    match pattern {
        Pattern::Binding(binding) => {
            if let Some(symbol) = resolved.symbol_for(binding.syntax.id) {
                names.insert(symbol, binding.name.clone());
                globals.insert(symbol);
            }
        }
        Pattern::At(at) => {
            if let Some(symbol) = resolved.symbol_for(at.binding.syntax.id) {
                names.insert(symbol, at.binding.name.clone());
                globals.insert(symbol);
            }
            collect_pattern_facts(resolved, &at.pattern, names, globals);
        }
        Pattern::Product(product) => {
            for element in &product.elements {
                collect_pattern_facts(resolved, element, names, globals);
            }
        }
        Pattern::Nominal(nominal) => {
            collect_pattern_facts(resolved, &nominal.argument, names, globals);
        }
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
    }
}

/// Whether one checked type contains a garbage-collector-managed reference.
/// Records `checked_type_contains_ref` so
/// `LoweredSymbol::global_root` records the decision lowering-side.
fn checked_type_contains_ref(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::Ref(_) | CheckedType::Buffer(_) => true,
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| checked_type_contains_ref(&element.value_type)),
        CheckedType::Sum(sum) => sum.alternatives.iter().any(checked_type_contains_ref),
        CheckedType::Function(function) => {
            checked_type_contains_ref(&function.parameter)
                || checked_type_contains_ref(&function.result)
        }
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => {
            arguments.iter().any(checked_type_contains_ref)
                || checked_type_contains_ref(representation)
        }
        CheckedType::Opaque { arguments, .. } | CheckedType::TypeConstructor { arguments, .. } => {
            arguments.iter().any(checked_type_contains_ref)
        }
        CheckedType::CPointer { pointee } => checked_type_contains_ref(pointee),
        _ => false,
    }
}

/// Primary storage classification, in documented precedence order. Facts
/// that overlap (mutation, moves, initialization checks, capture cells) stay
/// as independent `LoweredSymbol` flags.
#[allow(clippy::too_many_arguments)]
fn symbol_storage(
    external: bool,
    binding: bool,
    derived: bool,
    signal: bool,
    captured_mutable_cell: bool,
    module_symbol: bool,
    mutable: bool,
) -> SymbolStorage {
    if external {
        SymbolStorage::ExternalSymbol
    } else if binding {
        SymbolStorage::FunctionBinding
    } else if derived {
        SymbolStorage::DerivedBinding
    } else if signal {
        SymbolStorage::Signal
    } else if captured_mutable_cell {
        SymbolStorage::CapturedCell
    } else if module_symbol {
        SymbolStorage::GlobalStorage
    } else if mutable {
        SymbolStorage::MutableCell
    } else {
        SymbolStorage::ImmutableValue
    }
}

/// The storage width used to validate integer literal magnitudes. `ISize`
/// and `USize` match the backend's pointer-sized integers.
fn integer_literal_bit_width(integer_type: IntegerType) -> u32 {
    match integer_type {
        IntegerType::I8 | IntegerType::U8 => 8,
        IntegerType::I16 | IntegerType::U16 => 16,
        IntegerType::I32 | IntegerType::U32 => 32,
        IntegerType::I64 | IntegerType::U64 | IntegerType::ISize | IntegerType::USize => 64,
    }
}

/// One arena node identity, used to attribute every reachable node to exactly
/// one runtime owner during the ownership traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OwnedNode {
    Expression(ExpressionId),
    Pattern(PatternId),
    Place(PlaceId),
    Block(BlockId),
    Item(ItemId),
    Call(LoweredCallId),
    CallableValue(LoweredCallableValueId),
    ResourceProvider(LoweredResourceProviderId),
    ResourceUse(LoweredResourceUseId),
    With(LoweredWithId),
    ReactiveOperation(LoweredReactiveOperationId),
    ReactiveCallback(LoweredReactiveCallbackId),
    CoroutinePlan(LoweredCoroutinePlanId),
    Coro(LoweredCoroId),
    Await(LoweredAwaitId),
}

impl OwnedNode {
    fn label(self) -> &'static str {
        match self {
            OwnedNode::Expression(_) => "expression",
            OwnedNode::Pattern(_) => "pattern",
            OwnedNode::Place(_) => "place",
            OwnedNode::Block(_) => "block",
            OwnedNode::Item(_) => "item",
            OwnedNode::Call(_) => "call",
            OwnedNode::CallableValue(_) => "callable value",
            OwnedNode::ResourceProvider(_) => "resource provider",
            OwnedNode::ResourceUse(_) => "resource use",
            OwnedNode::With(_) => "with",
            OwnedNode::ReactiveOperation(_) => "reactive operation",
            OwnedNode::ReactiveCallback(_) => "reactive callback",
            OwnedNode::CoroutinePlan(_) => "coroutine plan",
            OwnedNode::Coro(_) => "coro",
            OwnedNode::Await(_) => "await",
        }
    }
}

/// Nodes reached by the ownership traversal from runtime roots. Every node is
/// attributed to exactly one runtime owner so cross-owner references, not just
/// unreachable nodes, can be diagnosed.
struct Reachability {
    expressions: HashSet<ExpressionId>,
    patterns: HashSet<PatternId>,
    places: HashSet<PlaceId>,
    blocks: HashSet<BlockId>,
    items: HashSet<ItemId>,
    calls: HashSet<LoweredCallId>,
    callable_values: HashSet<LoweredCallableValueId>,
    resource_providers: HashSet<LoweredResourceProviderId>,
    resource_uses: HashSet<LoweredResourceUseId>,
    withs: HashSet<LoweredWithId>,
    reactive_operations: HashSet<LoweredReactiveOperationId>,
    reactive_callbacks: HashSet<LoweredReactiveCallbackId>,
    coroutine_plans: HashSet<LoweredCoroutinePlanId>,
    coros: HashSet<LoweredCoroId>,
    awaits: HashSet<LoweredAwaitId>,
    /// The runtime owner currently being traversed. Cross-owner links (a
    /// `coro` creation reaching its body thunk's plan) save and restore it.
    owner: Option<ExpressionOwner>,
    /// First owner that reached each node, proving single ownership.
    owners: HashMap<OwnedNode, ExpressionOwner>,
    conflicts: Vec<Diagnostic>,
}

impl Default for Reachability {
    fn default() -> Self {
        Self {
            expressions: HashSet::new(),
            patterns: HashSet::new(),
            places: HashSet::new(),
            blocks: HashSet::new(),
            items: HashSet::new(),
            calls: HashSet::new(),
            callable_values: HashSet::new(),
            resource_providers: HashSet::new(),
            resource_uses: HashSet::new(),
            withs: HashSet::new(),
            reactive_operations: HashSet::new(),
            reactive_callbacks: HashSet::new(),
            coroutine_plans: HashSet::new(),
            coros: HashSet::new(),
            awaits: HashSet::new(),
            owner: None,
            owners: HashMap::new(),
            conflicts: Vec::new(),
        }
    }
}

impl Reachability {
    /// Attributes one reachable node to its runtime owner. A node reached from
    /// two different owners (or from a different owner than an expression's
    /// own occurrence key) is a lowering bug and diagnoses.
    fn claim(&mut self, node: OwnedNode, owner: ExpressionOwner, origin: &Origin) {
        match self.owners.entry(node) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(owner);
            }
            std::collections::hash_map::Entry::Occupied(entry) if *entry.get() != owner => {
                self.conflicts.push(Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "lowered {} is owned by {:?} but is also reached from {:?}",
                        node.label(),
                        entry.get(),
                        owner
                    ),
                ));
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }

    fn claim_current(&mut self, node: OwnedNode, origin: &Origin) {
        if let Some(owner) = self.owner {
            self.claim(node, owner, origin);
        }
    }
}

/// Whether a trait evidence recipe addresses the given trait and, when
/// known, method.
fn evidence_matches(
    evidence: &TraitEvidence,
    trait_id: TraitId,
    method: Option<TraitMethodId>,
) -> bool {
    let (evidence_trait, evidence_method) = match evidence {
        TraitEvidence::ExplicitImplementation {
            trait_id, method, ..
        }
        | TraitEvidence::Structural {
            trait_id, method, ..
        } => (*trait_id, Some(*method)),
        TraitEvidence::DeclaredBound {
            trait_id, method, ..
        } => (*trait_id, *method),
    };
    evidence_trait == trait_id && method.is_none_or(|method| evidence_method == Some(method))
}

/// Whether a checked dispatch argument and a lowered operand type agree well
/// enough for validation. `Inferred`, `Error`, and `Never` occurrences are
/// placeholders rather than real operand types.
fn types_agree(expected: &CheckedType, actual: &CheckedType) -> bool {
    expected == actual
        || matches!(
            expected,
            CheckedType::Inferred | CheckedType::Error | CheckedType::Never
        )
        || matches!(
            actual,
            CheckedType::Inferred | CheckedType::Error | CheckedType::Never
        )
}

/// Whether a checked type is itself an unresolved inference placeholder.
/// `Inferred` and `Error` must never survive as a runtime node's whole type;
/// declared parameters and `Never` (diverged code) are legal. Nested
/// `Inferred` slots are checker sentinels for effect rows and slice elements,
/// so they are not rejected here.
fn type_has_placeholder(value_type: &CheckedType) -> bool {
    matches!(value_type, CheckedType::Inferred | CheckedType::Error)
}

/// The checked function type declared for a symbol, used when a callable
/// occurrence is checked as a coerced (for example sum-alternative) type.
fn symbol_function_type(module: &TypedModule, symbol: SymbolId) -> Option<CheckedFunctionType> {
    match module
        .type_of_symbol(symbol)
        .cloned()
        .or_else(|| module.declared_type_of_symbol(symbol))
    {
        Some(CheckedType::Function(function_type)) => Some(function_type),
        _ => None,
    }
}

/// The checked function type of a call's callee, falling back to the callee
/// symbol's declared function type when the occurrence is checked as a
/// coerced type.
fn checked_call_function_type(
    module: &TypedModule,
    call: &staple_syntax::CallExpression,
    origin: &Origin,
) -> Result<CheckedFunctionType, Diagnostic> {
    if let Some(CheckedType::Function(function_type)) =
        module.type_of_expression(call.callee.syntax().id)
    {
        return Ok(function_type.clone());
    }
    if let Some(symbol) = module.symbol_for(call.callee.syntax().id)
        && let Some(function_type) = symbol_function_type(module, symbol)
    {
        return Ok(function_type);
    }
    Err(Diagnostic::new(
        origin.span.clone(),
        "call has no checked function type",
    ))
}

/// Whether an argument with `slots` elements fits a variadic parameter whose
/// fixed prefix is the parameter's element count.
fn variadic_argument(function_type: &CheckedFunctionType, slots: usize) -> bool {
    matches!(
        function_type.parameter.as_ref(),
        CheckedType::Product(product)
            if product.variadic && slots >= product.elements.len()
    )
}

/// One evaluated call-argument placement before pass modes are applied.
#[derive(Clone)]
struct CallArgumentPlacement {
    expression: Option<ExpressionId>,
    thunk: Option<FunctionId>,
    place: Option<PlaceId>,
}

/// The ordered step that evaluates one call argument. An implicit thunk has
/// no argument occurrence; its closure construction is recorded on the
/// argument itself.
fn call_argument_step(
    argument: usize,
    slot: usize,
    expression: Option<ExpressionId>,
) -> LoweredCallStep {
    match expression {
        Some(expression) => LoweredCallStep::ProductElement {
            argument,
            slot,
            expression,
        },
        None => LoweredCallStep::Argument { argument },
    }
}

/// The flattened ABI slots of a parameter type: a product's element types, or
/// the type itself for every other shape.
fn flattened_parameter_types(parameter: &CheckedType) -> Vec<CheckedType> {
    match parameter {
        CheckedType::Product(product) => product
            .elements
            .iter()
            .map(|element| element.value_type.clone())
            .collect(),
        other => vec![other.clone()],
    }
}

/// Which slots a mutation/move marker list addresses.
fn mutation_slot_mask(count: usize, mutations: &[CheckedMutation]) -> Vec<bool> {
    let whole = mutations.contains(&CheckedMutation::Whole);
    (0..count)
        .map(|index| whole || mutations.contains(&CheckedMutation::Element(index)))
        .collect()
}

/// Compile-time substitutions from a function template to the concrete type
/// recorded at a use site. Effect variables are inferred by the checker as
/// error-shaped function types and are split back into effect substitutions.
fn call_substitutions(
    template: &CheckedFunctionType,
    actual: &CheckedFunctionType,
) -> CallSubstitutions {
    let mut inferred = HashMap::new();
    let _ = infer_type_parameters(
        &CheckedType::Function(template.clone()),
        &CheckedType::Function(actual.clone()),
        &mut inferred,
    );
    substitutions_from_map(inferred)
}

/// Trait-call substitutions: the trait's declared parameter templates mapped
/// onto the completed call-site arguments.
fn trait_call_substitutions(
    parameters: &[CheckedType],
    arguments: &[CheckedType],
) -> CallSubstitutions {
    let mut inferred = HashMap::new();
    if parameters.len() == arguments.len() {
        for (parameter, argument) in parameters.iter().zip(arguments) {
            let _ = infer_type_parameters(parameter, argument, &mut inferred);
        }
    }
    substitutions_from_map(inferred)
}

/// Splits an inferred substitution map into type and effect substitutions.
fn substitutions_from_map(inferred: HashMap<TypeParameterId, CheckedType>) -> CallSubstitutions {
    let mut types = Vec::new();
    let mut effects = Vec::new();
    for (parameter, value_type) in inferred {
        match effect_substitution_of(&value_type) {
            Some(effects_value) => effects.push(CallEffectSubstitution {
                parameter,
                effects: effects_value,
            }),
            None => types.push(CallTypeSubstitution {
                parameter,
                value_type,
            }),
        }
    }
    types.sort_by_key(|substitution| substitution.parameter.0);
    effects.sort_by_key(|substitution| substitution.parameter.0);
    CallSubstitutions { types, effects }
}

/// Decodes the checker's error-shaped function encoding of an effect
/// substitution.
fn effect_substitution_of(value_type: &CheckedType) -> Option<CheckedEffectSet> {
    let CheckedType::Function(function) = value_type else {
        return None;
    };
    (function.parameter.as_ref() == &CheckedType::Error
        && function.result.as_ref() == &CheckedType::Error)
        .then(|| function.effects.clone())
}

/// A source diagnostic for a final product slot that no explicit element or
/// contextual default filled.
fn missing_product_slot_error(
    final_type: &CheckedProductType,
    slot: usize,
    span: &Span,
) -> Diagnostic {
    Diagnostic::new(
        span.clone(),
        match final_type.elements.get(slot) {
            Some(field) => match &field.name {
                Some(name) => format!("missing product field `{name}`"),
                None => format!("missing product element at position {slot}"),
            },
            None => format!("missing product element at position {slot}"),
        },
    )
}

/// Checks that every top-level runtime binding symbol reached the catalog.
fn initializer_symbol_diagnostics(
    module: &TypedModule,
    origins: &HashMap<SymbolId, Origin>,
    symbols: &Catalog<SymbolId, LoweredSymbol, LoweredSymbolId>,
) -> Vec<Diagnostic> {
    let resolved = module.resolved();
    let mut diagnostics = Vec::new();
    let mut check = |symbol: SymbolId| {
        if compile_time_only_symbol(module, symbol) {
            return;
        }
        if symbols.get(symbol).is_none() {
            let origin = origins
                .get(&symbol)
                .cloned()
                .unwrap_or_else(Origin::compiler);
            diagnostics.push(Diagnostic::new(
                origin.span,
                format!(
                    "runtime initializer symbol {symbol:?} is missing from the lowered symbol catalog"
                ),
            ));
        }
    };
    for source in resolved.program().modules() {
        for item in &source.syntax.items {
            match item {
                Item::Binding(binding) => {
                    if let Some(symbol) = resolved.symbol_for(binding.syntax.id) {
                        check(symbol);
                    }
                }
                Item::PatternBinding(binding) => {
                    for symbol in pattern_symbols(resolved, &binding.pattern) {
                        check(symbol);
                    }
                }
                _ => {}
            }
        }
    }
    diagnostics
}

/// Whether a source item has runtime effect. Declaration-only items are
/// compile-time-only and are omitted by lowering.
fn runtime_item(item: &Item) -> bool {
    matches!(
        item,
        Item::Binding(_)
            | Item::PatternBinding(_)
            | Item::Assignment(_)
            | Item::Return(_)
            | Item::Break(_)
            | Item::Continue(_)
            | Item::Expression(_)
    )
}

/// The lowering completeness traversal. It mirrors the lowering walk over the
/// checked program and diagnoses any runtime source function, item, expression,
/// pattern, or assignment place without a lowered counterpart, plus any lowered
/// node invented without a source. Implicit thunks are owned by their function
/// template, so their callback bodies are covered through the function catalog.
struct SourceCoverage<'a> {
    program: &'a LoweredProgram,
    module: &'a TypedModule,
    diagnostics: Vec<Diagnostic>,
    visited: HashSet<(ExpressionOwner, SyntaxId)>,
    lowered_patterns: HashSet<SyntaxId>,
    lowered_places: HashSet<SyntaxId>,
}

impl<'a> SourceCoverage<'a> {
    fn run(program: &'a LoweredProgram, module: &'a TypedModule) -> Vec<Diagnostic> {
        let mut coverage = Self {
            program,
            module,
            diagnostics: Vec::new(),
            visited: HashSet::new(),
            lowered_patterns: program
                .patterns
                .iter()
                .map(|(_, pattern)| pattern.origin.syntax)
                .collect(),
            lowered_places: program
                .places
                .iter()
                .map(|(_, place)| place.origin.syntax)
                .collect(),
        };
        coverage.check_functions();
        coverage.check_modules();
        coverage.diagnostics
    }

    fn missing(&mut self, span: &Span, what: &str, syntax: SyntaxId) {
        self.diagnostics.push(Diagnostic::new(
            span.clone(),
            format!(
                "runtime source {what} (syntax {}) has no lowered counterpart",
                syntax.0
            ),
        ));
    }

    /// Every declared function and implicit thunk has exactly one lowered
    /// template whose body syntax matches, and every lowered template has a
    /// checked source function.
    fn check_functions(&mut self) {
        let mut sources = HashSet::new();
        for function in self
            .module
            .functions()
            .iter()
            .chain(self.module.implicit_thunks())
        {
            sources.insert(function.id);
            let owner = ExpressionOwner::Function(function.id);
            let syntax = function.body.syntax();
            match self.program.functions.get(function.id) {
                Some(lowered) => {
                    if lowered.body_syntax != syntax.id {
                        self.diagnostics.push(Diagnostic::new(
                            syntax.span.clone(),
                            format!(
                                "function `{}` lowered body syntax disagrees with its checked body",
                                function.name
                            ),
                        ));
                    }
                    if lowered.body.is_none() {
                        self.diagnostics.push(Diagnostic::new(
                            syntax.span.clone(),
                            format!("function `{}` has no lowered body block", function.name),
                        ));
                    }
                }
                None => self.diagnostics.push(Diagnostic::new(
                    syntax.span.clone(),
                    format!(
                        "function `{}` (function id {}) has no lowered template",
                        function.name, function.id.0
                    ),
                )),
            }
            self.visit_pattern(&function.pattern);
            self.visit_expression(owner, &function.body);
        }
        for (_, key, function) in self.program.functions.iter() {
            if !sources.contains(&key) {
                self.diagnostics.push(Diagnostic::new(
                    function.origin.span.clone(),
                    format!("lowered function {key:?} has no checked source function"),
                ));
            }
        }
    }

    /// Every module initializer owns exactly the runtime source items of its
    /// module, in source order, and every such item is walked for coverage.
    fn check_modules(&mut self) {
        let source_program = self.module.resolved().program();
        let sources = source_program.modules();
        for module_id in source_program.initialization_order() {
            let Some(source) = sources.get(module_id.0) else {
                continue;
            };
            let owner = ExpressionOwner::Module(*module_id);
            let Some(info) = self.program.modules.get(*module_id) else {
                self.diagnostics.push(Diagnostic::new(
                    source.syntax.syntax.span.clone(),
                    format!("runtime module {module_id:?} has no lowered catalog entry"),
                ));
                continue;
            };
            if let Some(initializer) = self.program.initializers.get(info.initializer) {
                let lowered = self
                    .program
                    .blocks
                    .get(initializer.body)
                    .map(|block| block.items.clone())
                    .unwrap_or_default();
                let runtime = source
                    .syntax
                    .items
                    .iter()
                    .filter(|item| runtime_item(item))
                    .collect::<Vec<_>>();
                if runtime.len() != lowered.len() {
                    self.diagnostics.push(Diagnostic::new(
                        source.syntax.syntax.span.clone(),
                        format!(
                            "module {module_id:?} lowered {} runtime items for {} source items",
                            lowered.len(),
                            runtime.len()
                        ),
                    ));
                }
                for (index, item) in runtime.iter().enumerate() {
                    let Some(lowered) = lowered
                        .get(index)
                        .and_then(|id| self.program.items.get(*id))
                    else {
                        continue;
                    };
                    if lowered.origin.syntax != item.syntax().id {
                        self.diagnostics.push(Diagnostic::new(
                            item.syntax().span.clone(),
                            format!(
                                "module {module_id:?} runtime item {index} does not match its lowered counterpart"
                            ),
                        ));
                    }
                }
            }
            for item in &source.syntax.items {
                self.visit_item(owner, item);
            }
        }
    }

    fn visit_item(&mut self, owner: ExpressionOwner, item: &Item) {
        match item {
            Item::Binding(binding) => {
                if let Some(value) = &binding.value {
                    self.visit_expression(owner, value);
                }
            }
            Item::PatternBinding(binding) => {
                self.visit_pattern(&binding.pattern);
                self.visit_expression(owner, &binding.value);
            }
            Item::Assignment(assignment) => {
                self.visit_place_origin(&assignment.target);
                self.visit_expression(owner, &assignment.value);
            }
            Item::Return(item) => self.visit_expression(owner, &item.value),
            Item::Break(item) => {
                if let Some(value) = &item.value {
                    self.visit_expression(owner, value);
                }
            }
            Item::Continue(_) => {}
            Item::Expression(expression) => self.visit_expression(owner, expression),
            _ => {}
        }
    }

    /// An assignment target lowers to a place rather than an expression. Single
    /// element wrappers are transparent to place lowering, matching `lower_place`.
    fn visit_place_origin(&mut self, expression: &Expression) {
        match expression {
            Expression::Product(product) if product.elements.len() == 1 => {
                self.visit_place_origin(&product.elements[0].value);
            }
            Expression::Satisfies(satisfies) => self.visit_place_origin(&satisfies.value),
            other => {
                let syntax = other.syntax();
                if !self.lowered_places.contains(&syntax.id) {
                    self.missing(&syntax.span, "assignment place", syntax.id);
                }
            }
        }
    }

    fn visit_pattern(&mut self, pattern: &Pattern) {
        let syntax = pattern.syntax();
        if self.module.type_of_pattern(syntax.id).is_none() {
            // Compiler-synthesized implicit-thunk parameters lower from the
            // checked signature instead of checked pattern metadata.
            return;
        }
        if !self.lowered_patterns.contains(&syntax.id) {
            self.missing(&syntax.span, "pattern", syntax.id);
            return;
        }
        match pattern {
            Pattern::Product(product) => {
                for element in &product.elements {
                    self.visit_pattern(element);
                }
            }
            Pattern::Nominal(nominal) => self.visit_pattern(&nominal.argument),
            Pattern::At(at) => {
                self.visit_pattern(&Pattern::Binding(at.binding.as_ref().clone()));
                self.visit_pattern(&at.pattern);
            }
            _ => {}
        }
    }

    fn visit_block(&mut self, owner: ExpressionOwner, block: &staple_syntax::BlockExpression) {
        let last = block.items.len().checked_sub(1);
        let key = ExpressionKey {
            syntax: block.syntax.id,
            owner,
            context: ExpressionContext::Primary,
        };
        let Some(lowered) = self
            .program
            .block_lookup
            .get(&key)
            .and_then(|id| self.program.blocks.get(*id))
        else {
            self.missing(&block.syntax.span, "block", block.syntax.id);
            return;
        };
        let lowered_items = lowered.items.clone();
        let runtime = block
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                (runtime_item(item)
                    && !(Some(index) == last && matches!(item, Item::Expression(_))))
                .then_some(item)
            })
            .collect::<Vec<_>>();
        if runtime.len() != lowered_items.len() {
            self.diagnostics.push(Diagnostic::new(
                block.syntax.span.clone(),
                format!(
                    "block {} lowered {} runtime items for {} source items",
                    block.syntax.id.0,
                    lowered_items.len(),
                    runtime.len()
                ),
            ));
        }
        for (index, item) in runtime.iter().enumerate() {
            if !lowered_items
                .get(index)
                .and_then(|id| self.program.items.get(*id))
                .is_some_and(|lowered| lowered.origin.syntax == item.syntax().id)
            {
                self.diagnostics.push(Diagnostic::new(
                    item.syntax().span.clone(),
                    format!(
                        "block {} runtime item {index} does not match its lowered counterpart",
                        block.syntax.id.0
                    ),
                ));
            }
        }
        for (index, item) in block.items.iter().enumerate() {
            if Some(index) == last
                && let Item::Expression(expression) = item
            {
                self.visit_expression(owner, expression);
            } else {
                self.visit_item(owner, item);
            }
        }
    }

    /// Every runtime expression occurrence has a lowered node under the same
    /// owner with a `Primary` context. Implicit thunks are function templates,
    /// so their bodies are owned by the function catalog instead.
    fn visit_expression(&mut self, owner: ExpressionOwner, expression: &Expression) {
        let syntax = expression.syntax();
        if let Some(thunk) = self.module.implicit_thunk_for(syntax.id)
            && owner != ExpressionOwner::Function(thunk.id)
        {
            if self.program.functions.get(thunk.id).is_none() {
                self.diagnostics.push(Diagnostic::new(
                    syntax.span.clone(),
                    format!(
                        "runtime implicit thunk (function id {}) has no lowered template",
                        thunk.id.0
                    ),
                ));
            }
            return;
        }
        if !matches!(
            classify_expression(self.module, expression),
            ExpressionDisposition::Ordinary(_) | ExpressionDisposition::ResourceCoroutine(_)
        ) {
            // Rejected and deferred families are diagnosed by the validator.
            return;
        }
        if !self.visited.insert((owner, syntax.id)) {
            return;
        }
        let key = ExpressionKey {
            syntax: syntax.id,
            owner,
            context: ExpressionContext::Primary,
        };
        if self.program.expression_lookup.get(&key).is_none() {
            self.diagnostics.push(Diagnostic::new(
                syntax.span.clone(),
                format!(
                    "runtime source {} expression (syntax {}) has no lowered counterpart",
                    expression_variant_name(expression),
                    syntax.id.0
                ),
            ));
            return;
        }
        match expression {
            Expression::Block(block) => self.visit_block(owner, block),
            Expression::Satisfies(satisfies) => self.visit_expression(owner, &satisfies.value),
            Expression::Match(match_) => {
                self.visit_expression(owner, &match_.subject);
                if self.module.match_for(syntax.id).is_some() {
                    for arm in &match_.arms {
                        self.visit_pattern(&arm.pattern);
                        self.visit_expression(owner, &arm.body);
                    }
                }
            }
            Expression::Loop(loop_) => self.visit_block(owner, &loop_.body),
            Expression::Coro(_) => {}
            Expression::Await(await_) => self.visit_expression(owner, &await_.operand),
            Expression::Resource(_) => {}
            Expression::With(with) => {
                self.visit_expression(owner, &with.value);
                self.visit_block(owner, &with.body);
            }
            Expression::Product(product) => {
                for element in &product.elements {
                    self.visit_expression(owner, &element.value);
                }
            }
            Expression::RepeatedProduct(repeated) => {
                self.visit_expression(owner, &repeated.value);
            }
            Expression::Call(call) => self.visit_call(owner, call),
            Expression::Access(access) => {
                if self.module.trait_dispatch_for(syntax.id).is_none()
                    && self.module.symbol_for(syntax.id).is_none()
                {
                    self.visit_expression(owner, &access.value);
                }
            }
            Expression::Index(index) => {
                self.visit_expression(owner, &index.value);
                self.visit_expression(owner, &index.index);
            }
            Expression::Logical(logical) => {
                self.visit_expression(owner, &logical.left);
                self.visit_expression(owner, &logical.right);
            }
            Expression::StringTemplate(template) => {
                for part in &template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        self.visit_expression(owner, &interpolation.expression);
                    }
                }
            }
            Expression::Function(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
            Expression::Unary(_)
            | Expression::Binary(_)
            | Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_) => {}
        }
    }

    /// A call argument lowers either as a whole expression or, when it is a
    /// literal product, as its decomposed ABI slots. Element values and spread
    /// operands are the lowered occurrences in the decomposed case; implicit
    /// thunks are skipped by `visit_expression`.
    fn visit_call_argument(&mut self, owner: ExpressionOwner, argument: &Expression) {
        if let Expression::Product(product) = argument {
            for element in &product.elements {
                self.visit_expression(owner, &element.value);
            }
            return;
        }
        self.visit_expression(owner, argument);
    }

    /// Mirrors the call lowering walk. Only indirect callees, juxtaposed chain
    /// roots, and argument expressions become occurrences; explicit callees are
    /// symbol references, primitive macros decode their literal, and implicit
    /// thunk arguments are function templates covered through the catalog.
    fn visit_call(&mut self, owner: ExpressionOwner, call: &staple_syntax::CallExpression) {
        let route = match self.program.classify_call_route(self.module, owner, call) {
            Ok(route) => route,
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                return;
            }
        };
        match route {
            // Primitive macros decode their literal argument, curried calls are
            // rejected during resolution, and compiler helpers are selected by
            // checked operations rather than source call syntax.
            CallRoute::PrimitiveMacro | CallRoute::CurriedDefault => {}
            CallRoute::Juxtaposed | CallRoute::JuxtaposedIntrinsic => {
                let Some(plan) = self.module.juxtaposed_call_plan(call.syntax.id) else {
                    return;
                };
                let expected = match plan.function.parameter.as_ref() {
                    CheckedType::Product(product) => product.elements.len(),
                    _ => 0,
                };
                if plan.arguments.len() != expected {
                    return;
                }
                let mut current = call;
                let mut root = call.callee.as_ref();
                for index in 0..plan.consumed_calls {
                    self.visit_call_argument(owner, &current.argument);
                    if index + 1 == plan.consumed_calls {
                        break;
                    }
                    let Expression::Call(previous) = root else {
                        break;
                    };
                    current = previous;
                    root = previous.callee.as_ref();
                }
                if route == CallRoute::Juxtaposed {
                    self.visit_expression(owner, root);
                }
            }
            CallRoute::Indirect => {
                self.visit_expression(owner, &call.callee);
                self.visit_call_argument(owner, &call.argument);
            }
            CallRoute::Intrinsic
            | CallRoute::GenericDirect
            | CallRoute::External
            | CallRoute::Constructor
            | CallRoute::TraitImplementation
            | CallRoute::DeclaredTraitBound
            | CallRoute::StructuralTraitMethod => {
                self.visit_call_argument(owner, &call.argument);
            }
        }
    }
}

fn invalid_reference(origin: &Origin, owner: &str, target: &str, index: usize) -> Diagnostic {
    Diagnostic::new(
        origin.span.clone(),
        format!("lowered {owner} has dangling {target} reference {index}"),
    )
}

/// A program accepted by the lowering phase and ready for code generation.
///
/// The fields are intentionally private: callers may pass this value to later
/// compiler phases, but consume only the read-only emission view.
#[derive(Debug, Clone)]
pub struct LoweredModule {
    program: LoweredProgram,
}

/// Catalog families used by integration tests without exposing lowering arenas.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub enum PlannedArtifactFamily {
    StructuralDebug,
    StructuralIndex,
    StructuralMutateIndex,
    StructuralIntoIterator,
    StructuralIterator,
    GcFinalizer,
    GcCellFinalizer,
    GcClosureFinalizer,
    GcBufferFinalizer,
    ExternAdapter,
    CoroutineCodes,
}

impl LoweredModule {
    /// Planned emitted names for every instance of a named source template.
    /// A bare source name also selects module-qualified templates.
    #[doc(hidden)]
    pub fn planned_instance_names(&self, template: &str) -> Vec<String> {
        self.program
            .instances
            .iter()
            .filter_map(|(_, instance)| {
                self.program
                    .functions
                    .get(instance.template)
                    .filter(|function| {
                        function.name == template
                            || function.name.ends_with(&format!(".{template}"))
                    })
                    .map(|_| instance.name.clone())
            })
            .collect()
    }

    /// Planned emitted names selected by the catalog's artifact keys.
    /// Coroutine artifacts own two definitions, so both pair names are returned.
    #[doc(hidden)]
    pub fn planned_artifact_names(&self, family: PlannedArtifactFamily) -> Vec<String> {
        use crate::specialization::{ArtifactRequestKey, GcFinalizerKey};
        use PlannedArtifactFamily::*;
        let mut names = Vec::new();
        for (_, artifact) in self.program.artifacts.iter() {
            let Some(key) = self.program.specializations.artifact(artifact.ordinal) else {
                continue;
            };
            let selected = match (family, key) {
                (StructuralDebug, ArtifactRequestKey::StructuralMethod(method)) => {
                    method.structural == StructuralTraitMethod::Debug
                }
                (StructuralIndex, ArtifactRequestKey::StructuralMethod(method)) => {
                    method.structural == StructuralTraitMethod::Index
                }
                (StructuralMutateIndex, ArtifactRequestKey::StructuralMethod(method)) => {
                    method.structural == StructuralTraitMethod::MutateIndex
                }
                (StructuralIntoIterator, ArtifactRequestKey::StructuralMethod(method)) => {
                    method.structural == StructuralTraitMethod::IntoIterator
                }
                (StructuralIterator, ArtifactRequestKey::StructuralMethod(method)) => {
                    method.structural == StructuralTraitMethod::Iterator
                }
                (GcFinalizer, ArtifactRequestKey::GcFinalizer(_))
                | (GcCellFinalizer, ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Cell(_)))
                | (
                    GcClosureFinalizer,
                    ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment { .. }),
                )
                | (GcBufferFinalizer, ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Buffer(_)))
                | (ExternAdapter, ArtifactRequestKey::ExternAdapter(_))
                | (CoroutineCodes, ArtifactRequestKey::CoroutineCodes(_)) => true,
                _ => false,
            };
            if !selected {
                continue;
            }
            if matches!(key, ArtifactRequestKey::CoroutineCodes(_)) {
                names.push(format!("{}_resume", artifact.name));
                names.push(format!("{}_cleanup", artifact.name));
            } else {
                names.push(artifact.name.clone());
            }
        }
        names
    }

    /// The read-only backend view of the owned lowered program.
    pub(crate) fn program(&self) -> emission::EmissionView<'_> {
        self.program.emission_view()
    }
}

impl LoweredProgram {
    /// Uses module initializer names when free and deterministic suffixes when
    /// modules share a prefix or a catalog symbol.
    fn planned_initializer_names(&self) -> Vec<(InitializerId, String)> {
        let mut used = self.reserved_symbol_names();
        used.extend(
            self.instances
                .iter()
                .map(|(_, instance)| instance.name.clone()),
        );
        for (_, artifact) in self.artifacts.iter() {
            used.insert(artifact.name.clone());
            if matches!(artifact.plan, Some(LoweredArtifactPlan::CoroutineCodes(_))) {
                used.insert(format!("{}_resume", artifact.name));
                used.insert(format!("{}_cleanup", artifact.name));
            }
        }
        self.initializers
            .iter()
            .map(|(id, initializer)| {
                let prefix = self
                    .modules
                    .get(initializer.module)
                    .map(|module| module.symbol_prefix.as_str())
                    .unwrap_or("");
                let base = format!("__staple_init_m{prefix}");
                let mut name = base.clone();
                let mut suffix = 1;
                while !used.insert(name.clone()) {
                    name = format!("{base}.{suffix}");
                    suffix += 1;
                }
                (id, name)
            })
            .collect()
    }

    fn assign_initializer_names(&mut self) {
        for (id, name) in self.planned_initializer_names() {
            self.initializers
                .get_mut(id)
                .expect(
                    "internal invariant violated: initializer naming iterates existing arena IDs",
                )
                .name = name;
        }
    }
}

/// Converts checked compiler state into the code-generation input.
#[derive(Debug, Default, Clone, Copy)]
pub struct Lowerer;

impl Lowerer {
    pub fn new() -> Self {
        Self
    }

    pub fn lower(&self, module: &TypedModule) -> Result<LoweredModule, Vec<Diagnostic>> {
        let mut program = LoweredProgram::default();
        let mut diagnostics = validate_checked_module(module);
        if diagnostics.is_empty() {
            diagnostics.extend(program.snapshot(module));
        }
        diagnostics.extend(program.validate());
        if diagnostics.is_empty() {
            diagnostics.extend(program.build_specialization_worklist());
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.materialize_instance_bodies());
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.close_artifact_catalog(&ProductionHooks));
        }
        if diagnostics.is_empty() {
            program.assign_initializer_names();
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.validate_instance_bodies());
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.validate_specializations());
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.validate_specialization_graph());
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.validate_artifact_closure(&ProductionHooks));
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.validate_initializer_bindings());
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.validate_closed_catalog());
        }
        if diagnostics.is_empty() {
            diagnostics.extend(program.validate_source_coverage(module));
        }
        if diagnostics.is_empty() {
            Ok(LoweredModule { program })
        } else {
            Err(diagnostics)
        }
    }
}

fn validate_checked_module(module: &TypedModule) -> Vec<Diagnostic> {
    module
        .functions()
        .iter()
        .chain(module.implicit_thunks())
        .filter(|function| module.type_of_function(function.id).is_none())
        .map(|function| {
            Diagnostic::new(
                function.body.syntax().span.clone(),
                format!(
                    "cannot lower unchecked function `{}` (function id {})",
                    function.name, function.id.0
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use staple_syntax::{Syntax, Type};

    use crate::{NameResolver, ProgramLoader, TypeChecker};

    fn standard_library_root() -> std::path::PathBuf {
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

    fn checked_program_at(entry: &Path, source: &str, root: &Path) -> TypedModule {
        let program = ProgramLoader::new()
            .with_standard_library_root(standard_library_root())
            .with_module_root(root)
            .load_source_at(entry, source)
            .expect("test source should load");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("test source should resolve");
        TypeChecker::new()
            .check(resolved)
            .expect("test source should type check")
    }

    #[test]
    fn empty_program_has_valid_deterministic_arenas() {
        let program = LoweredProgram::default();
        assert!(program.validate().is_empty());
        assert_eq!(program.expressions.iter().count(), 0);
        assert_eq!(program.functions.iter().count(), 0);
    }

    #[test]
    fn arena_ids_are_dense_and_in_insertion_order() {
        let mut patterns = Arena::<LoweredPattern, PatternId>::default();
        let first = patterns.push(LoweredPattern {
            origin: Origin::compiler(),
            value_type: CheckedType::I32,
            kind: LoweredPatternKind::Wildcard,
            test: LoweredPatternTestPlan::undecided(CheckedType::I32),
        });
        let second = patterns.push(LoweredPattern {
            origin: Origin::compiler(),
            value_type: CheckedType::I64,
            kind: LoweredPatternKind::Wildcard,
            test: LoweredPatternTestPlan::undecided(CheckedType::I64),
        });
        assert_eq!(first.index(), 0);
        assert_eq!(second.index(), 1);
        assert_eq!(
            patterns
                .iter()
                .map(|(id, _)| id.index())
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn catalog_rejects_duplicate_semantic_ids_without_overwriting() {
        let mut catalog = Catalog::<ModuleId, &'static str, LoweredModuleId>::default();
        let id = catalog
            .insert("module", ModuleId(7), Origin::compiler(), "first")
            .expect("first semantic ID should be accepted");
        let diagnostic = catalog
            .insert("module", ModuleId(7), Origin::compiler(), "second")
            .expect_err("duplicate semantic ID should be rejected");
        assert_eq!(id.index(), 0);
        assert_eq!(catalog.get(ModuleId(7)), Some(&"first"));
        assert!(diagnostic.message.contains("duplicate lowered module"));
        assert_eq!(catalog.iter().count(), 1);
    }

    #[test]
    fn catalog_validator_checks_lookup_and_ordered_entries_both_ways() {
        let mut catalog = Catalog::<ModuleId, (), LoweredModuleId>::default();
        let first = catalog
            .insert("module", ModuleId(2), Origin::compiler(), ())
            .expect("first insertion");
        let second = catalog
            .insert("module", ModuleId(9), Origin::compiler(), ())
            .expect("second insertion");
        assert_eq!(
            catalog.iter().map(|(_, key, _)| key).collect::<Vec<_>>(),
            vec![ModuleId(2), ModuleId(9)]
        );

        catalog.by_key.insert(ModuleId(2), second);
        catalog.by_key.insert(ModuleId(11), first);
        let diagnostics = catalog.validate("module");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("lookup disagrees for semantic id ModuleId(2)")
        }));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("instead of ModuleId(11)"))
        );
    }

    #[test]
    fn checked_inventory_apis_are_complete_and_semantically_ordered() {
        let module = checked_program("let answer = 42\n");
        let resolved = module.resolved();

        let symbol_ids = resolved
            .symbols_in_id_order()
            .into_iter()
            .map(|symbol| symbol.id.0)
            .collect::<Vec<_>>();
        assert!(symbol_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let type_ids = resolved
            .types_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(type_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let trait_ids = resolved
            .traits_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(trait_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let method_ids = resolved
            .trait_methods_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(method_ids.windows(2).all(|ids| ids[0] < ids[1]));

        let thunk_ids = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .map(|function| function.id.0)
            .collect::<Vec<_>>();
        assert!(thunk_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let evaluator_symbols = module
            .derived_evaluators_in_symbol_order()
            .into_iter()
            .map(|(symbol, _)| symbol.0)
            .collect::<Vec<_>>();
        assert!(
            evaluator_symbols
                .windows(2)
                .all(|symbols| symbols[0] < symbols[1])
        );
        let checked_method_ids = module
            .trait_method_types_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(checked_method_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let checked_trait_ids = module
            .trait_parameter_arguments_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(checked_trait_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let _ = module.checked_trait_implementations();
        let semantic_ids = module.semantic_ids();
        assert!(semantic_ids.copy_trait.is_some());
        assert!(semantic_ids.io_type.is_some());
    }

    fn snapshot(source: &str) -> LoweredProgram {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let validation = program.validate();
        assert!(validation.is_empty(), "{validation:?}");
        program
    }

    fn entry_module(program: &LoweredProgram) -> (ModuleId, &LoweredModuleInfo) {
        program
            .modules
            .iter()
            .find_map(|(_, key, info)| info.executable_entry.then_some((key, info)))
            .expect("a lowered program should have one executable entry")
    }

    fn item_category(kind: &LoweredItemKind) -> &'static str {
        match kind {
            LoweredItemKind::Binding(_) => "binding",
            LoweredItemKind::PatternBinding(_) => "pattern-binding",
            LoweredItemKind::Assignment(_) => "assignment",
            LoweredItemKind::Return(_) => "return",
            LoweredItemKind::Break(_) => "break",
            LoweredItemKind::Continue(_) => "continue",
            LoweredItemKind::Expression(_) => "expression",
        }
    }

    fn runtime_categories(program: &LoweredProgram, module: ModuleId) -> Vec<&'static str> {
        let initializer = program
            .modules
            .get(module)
            .and_then(|info| program.initializers.get(info.initializer))
            .expect("module should have an initializer");
        program
            .blocks
            .get(initializer.body)
            .expect("initializer body")
            .items
            .iter()
            .map(|item| {
                item_category(&program.items.get(*item).expect("lowered runtime item").kind)
            })
            .collect()
    }

    fn normalized_modules(
        program: &LoweredProgram,
    ) -> Vec<(
        ModuleId,
        String,
        Option<ModuleId>,
        bool,
        usize,
        Vec<&'static str>,
    )> {
        program
            .modules
            .iter()
            .map(|(_, key, info)| {
                (
                    key,
                    info.qualified_name.clone(),
                    info.parent,
                    info.companion,
                    info.initialization_index,
                    runtime_categories(program, key),
                )
            })
            .collect()
    }

    #[test]
    fn module_catalog_matches_initialization_order() {
        let module = checked_program(
            "use dependency.answer\nmod dependency { pub let answer: I32 = 42 }\nlet copy: I32 = answer\n",
        );
        let order = module.resolved().program().initialization_order().to_vec();
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let lowered = program
            .modules
            .iter()
            .map(|(_, key, info)| (key, info))
            .collect::<Vec<_>>();
        assert_eq!(lowered.len(), order.len());
        assert_eq!(
            lowered.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            order
        );
        let (entry_id, _) = entry_module(&program);
        for (index, (key, info)) in lowered.iter().enumerate() {
            assert_eq!(info.semantic_id, *key);
            assert_eq!(info.initialization_index, index);
            assert_eq!(info.executable_entry, *key == entry_id);
            let initializer = program
                .initializers
                .get(info.initializer)
                .expect("every module should have an initializer");
            assert_eq!(initializer.module, *key);
            assert_eq!(initializer.origin, info.origin);
            assert_eq!(initializer.executable_entry, info.executable_entry);
            assert!(program.blocks.contains(initializer.body));
            if let Some(parent) = info.parent {
                assert!(program.modules.get(parent).is_some());
            }
        }

        let entry = program.modules.get(entry_id).expect("entry module");
        assert!(entry.parent.is_none());
        let sources = module.resolved().program();
        assert_eq!(
            entry.origin.syntax,
            sources.module(entry_id).syntax.syntax.id,
            "the entry module has no declaration node"
        );
        assert_eq!(
            entry.initialization_index,
            order.len() - 1,
            "the imported dependency initializes first"
        );
    }

    #[test]
    fn declaration_only_modules_have_empty_initializer_roots() {
        let program = snapshot("type Wrapper = alias I32\n");
        let (entry_id, _) = entry_module(&program);
        assert!(runtime_categories(&program, entry_id).is_empty());
    }

    #[test]
    fn runtime_items_follow_source_order_and_category() {
        let program = snapshot("let first: I32 = 1\nlet second: I32 = first\nfirst == second\n");
        let (entry_id, _) = entry_module(&program);
        assert_eq!(
            runtime_categories(&program, entry_id),
            vec!["binding", "binding", "expression"]
        );
    }

    #[test]
    fn companion_modules_keep_parent_and_companion_metadata() {
        let program =
            snapshot("pub type User = alias I32\ncompanion User { pub let id: I32 = 42 }\n");
        let (entry_id, _) = entry_module(&program);
        let companion = program
            .modules
            .iter()
            .find(|(_, _, info)| info.companion && info.parent == Some(entry_id))
            .expect("the companion module should be lowered");
        assert_eq!(companion.2.parent, Some(entry_id));
        assert_eq!(runtime_categories(&program, companion.1), vec!["binding"]);
    }

    #[test]
    fn entry_initializer_records_the_io_resource() {
        let program = snapshot("let answer: I32 = 42\n");
        let (entry_id, _) = entry_module(&program);
        let entry = program
            .initializers
            .get(program.modules.get(entry_id).unwrap().initializer)
            .expect("entry initializer");
        assert!(entry.executable_entry);
        assert_eq!(entry.resources.len(), 1);
        assert_eq!(entry.resources[0].kind, LoweredEntryResourceKind::Io);
        assert!(matches!(
            entry.resources[0].resource.value_type,
            CheckedType::Opaque { .. }
        ));
        let dependency = program
            .initializers
            .iter()
            .find(|(_, initializer)| initializer.module != entry_id)
            .map(|(_, initializer)| initializer);
        assert!(dependency.is_none() || dependency.unwrap().resources.is_empty());
    }

    #[test]
    fn file_modules_with_declarations_use_their_declaration_origin() {
        let root =
            std::env::temp_dir().join(format!("staple-lower-module-origin-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temp root");
        std::fs::write(
            root.join("tools.sta"),
            "pub mod\npub let answer: I32 = 42\n",
        )
        .expect("write module");
        let entry = root.join("main.sta");
        let module =
            checked_program_at(&entry, "use tools.answer\nlet copy: I32 = answer\n", &root);
        let tools_id = module
            .resolved()
            .program()
            .modules()
            .iter()
            .find(|source| source.path.ends_with("tools.sta"))
            .map(|source| source.id)
            .expect("the file module should be loaded");
        let declaration = module
            .resolved()
            .program()
            .module(tools_id)
            .syntax
            .declaration_syntax
            .clone()
            .expect("the file module declares itself");
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let tools = program.modules.get(tools_id).expect("lowered file module");
        assert_eq!(tools.origin.syntax, declaration.id);
        assert_eq!(runtime_categories(&program, tools_id), vec!["binding"]);
        std::fs::remove_dir_all(root).expect("clean temp root");
    }

    #[test]
    fn module_catalog_is_stable_across_repeated_lowering() {
        let module = checked_program(
            "mod dependency { pub let answer: I32 = 42 }\nlet copy: I32 = dependency.answer\n",
        );
        let mut first = LoweredProgram::default();
        let mut second = LoweredProgram::default();
        assert!(first.snapshot(&module).is_empty());
        assert!(second.snapshot(&module).is_empty());
        assert_eq!(normalized_modules(&first), normalized_modules(&second));
    }

    #[test]
    fn initialization_order_diagnostics_report_unknown_duplicate_and_missing_modules() {
        let source = |id: usize| SourceModule {
            id: ModuleId(id),
            path: std::path::PathBuf::from(format!("module{id}.sta")),
            syntax: staple_syntax::parse("").expect("empty module should parse"),
            parent: None,
            name: None,
            visibility: staple_syntax::Visibility::Public,
            qualified_name: format!("module{id}"),
            companion: false,
        };
        let sources = vec![source(0), source(1)];
        let diagnostics =
            validate_initialization_order(&sources, &[ModuleId(0), ModuleId(0), ModuleId(7)]);
        assert_eq!(diagnostics.len(), 3);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("unknown module id 7"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("appears more than once"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("`module1` is missing"))
        );
    }

    fn lowered_function<'a>(
        program: &'a LoweredProgram,
        name: &str,
    ) -> (FunctionId, &'a LoweredFunction) {
        program
            .functions
            .iter()
            .find_map(|(_, key, function)| function.name.contains(name).then_some((key, function)))
            .unwrap_or_else(|| panic!("`{name}` should have a lowered function"))
    }

    /// The lowered call that invokes `function`, whether the call is direct or
    /// goes through a first-class closure value (the backend's normal route
    /// for declared non-generic functions).
    fn lowered_call_to<'a>(
        program: &'a LoweredProgram,
        module: &TypedModule,
        function: FunctionId,
    ) -> &'a LoweredCall {
        program
            .calls
            .iter()
            .find_map(|(_, call)| {
                let matches = match &call.target {
                    LoweredCallableTarget::DirectFunction {
                        function: target, ..
                    } => *target == function,
                    LoweredCallableTarget::IndirectClosure { callee } => program
                        .expressions
                        .get(*callee)
                        .and_then(|expression| {
                            module
                                .resolved()
                                .symbol_for(expression.key.syntax)
                                .and_then(|symbol| module.function_for_symbol(symbol))
                        })
                        .is_some_and(|target| target == function),
                    _ => false,
                };
                matches.then_some(call)
            })
            .unwrap_or_else(|| panic!("a call to function {} should lower", function.0))
    }

    fn binding_symbol(module: &TypedModule, name: &str) -> SymbolId {
        module
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => {
                    module.symbol_for(binding.syntax.id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("`{name}` should have a symbol"))
    }

    #[test]
    fn function_catalog_lists_declared_functions_before_implicit_thunks() {
        let module = checked_program(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "let mut count = 0\n",
            "let first = evaluate { count = count + 1; count }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let expected = module
            .functions()
            .iter()
            .map(|function| function.id)
            .chain(
                module
                    .implicit_thunks_in_id_order()
                    .into_iter()
                    .map(|function| function.id),
            )
            .collect::<Vec<_>>();
        let actual = program
            .functions
            .iter()
            .map(|(_, key, _)| key)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(
            actual.iter().collect::<HashSet<_>>().len(),
            actual.len(),
            "every function id should appear exactly once"
        );
        assert!(
            actual.len() > module.functions().len(),
            "the callback thunk should be lowered"
        );

        let declared = module.functions().len();
        assert!(
            program
                .functions
                .iter()
                .take(declared)
                .all(|(_, _, function)| function.class.declared)
        );
        assert!(
            program
                .functions
                .iter()
                .skip(declared)
                .all(|(_, _, function)| function.class.implicit_thunk)
        );
    }

    #[test]
    fn function_templates_copy_checked_signatures_parameters_and_bounds() {
        let module = checked_program(concat!(
            "trait Increment T { increment: T -> T }\n",
            "impl Increment I32 { def increment = value => value + 1 }\n",
            "def increment_twice: <T where Increment T> T -> T = value => increment (increment value)\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let source = module
            .functions()
            .iter()
            .find(|function| function.name.contains("increment_twice"))
            .expect("declared function");
        let (key, lowered) = lowered_function(&program, "increment_twice");
        assert_eq!(key, source.id);
        assert_eq!(
            lowered.signature,
            *module
                .type_of_function(source.id)
                .expect("checked signature")
        );
        assert_eq!(
            lowered.bounds,
            module.bounds_of_function(source.id).to_vec()
        );
        assert!(!lowered.bounds.is_empty());
        assert_eq!(lowered.parameter_style, source.parameter_style);
        assert_eq!(
            lowered.binding_symbol,
            source
                .binding_syntax
                .and_then(|syntax| module.resolved().symbol_for(syntax))
        );
        assert_eq!(
            lowered.parameters,
            pattern_symbols(module.resolved(), &source.pattern)
        );
        assert_eq!(lowered.body_syntax, source.body.syntax().id);
        assert_eq!(lowered.body_origin.syntax, source.body.syntax().id);
        let body = lowered
            .body
            .and_then(|body| program.blocks.get(body))
            .expect("every function template should have a lowered body");
        assert_eq!(body.origin, lowered.body_origin);
        assert!(program.patterns.contains(lowered.parameter_pattern));
        assert_eq!(
            lowered.module,
            module
                .resolved()
                .module_for_syntax(source.body.syntax().id)
                .expect("owning module")
        );
        assert!(lowered.class.declared && !lowered.class.implicit_thunk);
    }

    #[test]
    fn implicit_thunk_captures_match_transition_ownership_facts() {
        let module = checked_program(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "def run: () ->{state} I32 = () => {\n",
            "  let mut count = 0\n",
            "  evaluate { count = count + 1; count }\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let thunk = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .next()
            .expect("a callback thunk");
        let lowered = program
            .functions
            .get(thunk.id)
            .expect("lowered implicit thunk");
        assert!(lowered.class.implicit_thunk && !lowered.class.declared);
        assert!(lowered.binding_symbol.is_none());
        assert_eq!(lowered.captures.len(), thunk.captures.len());
        for (capture, symbol) in lowered.captures.iter().zip(&thunk.captures) {
            assert_eq!(capture.symbol, *symbol);
            assert_eq!(
                capture.borrowed,
                module.is_borrowed_capture(thunk.id, *symbol)
            );
            assert_eq!(capture.non_owning, module.is_non_owning_symbol(*symbol));
            assert_eq!(
                capture.requires_cell,
                capture_requires_cell(&module, *symbol)
            );
        }
        assert!(
            lowered
                .captures
                .iter()
                .any(|capture| capture.requires_cell && module.has_mutable_storage(capture.symbol)),
            "the mutable local `count` capture should require a shared cell"
        );
    }

    #[test]
    fn symbols_record_global_names_roots_and_arity_overloads() {
        let program = snapshot(concat!(
            "let managed: Ref I32 = Ref 0\n",
            "let plain = 1\n",
            "def pick: () -> I32 = () => 1\n",
            "def pick: I32 * I32 -> I32 = left * right => left\n",
        ));
        let symbol_named = |name: &str| {
            program
                .symbols
                .iter()
                .filter(|(_, _, symbol)| symbol.name == name)
                .map(|(_, id, symbol)| (id, symbol))
                .collect::<Vec<_>>()
        };

        let managed = symbol_named("managed");
        assert_eq!(managed.len(), 1);
        let (_, managed) = managed[0];
        assert!(managed.has_global && managed.module_symbol);
        assert!(managed.global_root, "a `Ref` global is a harness root");

        let (_, plain) = symbol_named("plain")[0];
        assert!(plain.has_global && !plain.global_root);

        let picks = symbol_named("pick");
        assert_eq!(picks.len(), 2, "both arity overloads lower");
        for (_, pick) in &picks {
            assert!(pick.overloaded, "an overload-set member is recorded");
            assert!(pick.has_global && pick.name == "pick");
        }
        assert!(
            program.validate_symbols().is_empty(),
            "the recorded facts validate"
        );
    }

    #[test]
    fn derived_evaluators_and_coroutine_bodies_are_classified() {
        let module = checked_program(concat!(
            "let signal count = 1\n",
            "let doubled = count + count\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let doubled = binding_symbol(&module, "doubled");
        let evaluator = module
            .derived_evaluator(doubled)
            .expect("derived evaluator thunk");
        let lowered = program
            .functions
            .get(evaluator.id)
            .expect("lowered derived evaluator");
        assert!(lowered.class.derived_evaluator);
        assert!(lowered.class.implicit_thunk);
        assert!(!lowered.class.coroutine_body);
        assert_eq!(
            lowered
                .captures
                .iter()
                .map(|capture| capture.symbol)
                .collect::<Vec<_>>(),
            evaluator.captures
        );

        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "def f: () -> Coroutine{} I32 = () => coro { 42 }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let body = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .find(|function| module.coroutine_plan(function.body.syntax().id).is_some())
            .expect("coroutine body thunk");
        let lowered = program
            .functions
            .get(body.id)
            .expect("lowered coroutine body");
        assert!(lowered.class.coroutine_body);
        assert!(lowered.class.implicit_thunk);
        assert!(!lowered.class.derived_evaluator);
        assert_eq!(lowered.body_syntax, body.body.syntax().id);
    }

    #[test]
    fn callback_thunks_classify_effectful_resource_helpers() {
        let module = checked_program(concat!(
            "let signal count = 0\n",
            "with Reactive = reactive_scope () {\n",
            "  reaction { let current = count; () }\n",
            "  count = 1\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let callback = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .find(|thunk| {
                let class = program.functions.get(thunk.id).expect("thunk").class;
                !class.derived_evaluator && !class.coroutine_body
            })
            .expect("the reaction callback should be an implicit thunk");
        let lowered = program
            .functions
            .get(callback.id)
            .expect("lowered implicit thunk");
        assert!(lowered.class.resource_helper);
        assert!(lowered.signature.effects.state.is_some());

        let module = checked_program(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "let answer = evaluate { 42 }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let pure = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .next()
            .expect("the callback should be an implicit thunk");
        let lowered = program
            .functions
            .get(pure.id)
            .expect("lowered implicit thunk");
        assert!(!lowered.class.resource_helper);
        assert!(lowered.signature.effects == CheckedEffectSet::default());
    }

    fn lowered_symbol(program: &LoweredProgram, symbol: SymbolId) -> &LoweredSymbol {
        program
            .symbols
            .get(symbol)
            .unwrap_or_else(|| panic!("symbol {symbol:?} should be lowered"))
    }

    fn body_block<'a>(program: &'a LoweredProgram, function: &LoweredFunction) -> &'a LoweredBlock {
        program
            .blocks
            .get(function.body.expect("every lowered function has a body"))
            .expect("lowered body block")
    }

    fn body_items<'a>(
        program: &'a LoweredProgram,
        function: &LoweredFunction,
    ) -> Vec<&'a LoweredItem> {
        body_block(program, function)
            .items
            .iter()
            .map(|item| program.items.get(*item).expect("lowered item"))
            .collect()
    }

    fn lowered_pattern(program: &LoweredProgram, pattern: PatternId) -> &LoweredPattern {
        program
            .patterns
            .get(pattern)
            .unwrap_or_else(|| panic!("pattern {} should be lowered", pattern.index()))
    }

    fn lowered_place(program: &LoweredProgram, place: PlaceId) -> &LoweredPlace {
        program
            .places
            .get(place)
            .unwrap_or_else(|| panic!("place {} should be lowered", place.index()))
    }

    #[test]
    fn symbol_catalog_covers_declared_runtime_symbols_in_id_order() {
        let module = checked_program(concat!(
            "const answer: I32 = 42\n",
            "let value: I32 = answer\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let resolved = module.resolved();
        let const_symbol = binding_symbol(&module, "answer");
        assert!(resolved.is_const_symbol(const_symbol));
        // Non-generic `const` bindings materialize a module global, so they
        // stay in the runtime symbol catalog as global storage.
        assert_eq!(
            program
                .symbols
                .get(const_symbol)
                .map(|symbol| symbol.storage),
            Some(SymbolStorage::GlobalStorage)
        );
        let expected = resolved
            .symbols_in_id_order()
            .into_iter()
            .filter(|info| !compile_time_only_symbol(&module, info.id))
            .filter(|info| info.module_symbol || info.owner.is_some())
            .map(|info| info.id)
            .collect::<Vec<_>>();
        let actual = program
            .symbols
            .iter()
            .map(|(_, key, _)| key)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(actual.windows(2).all(|ids| ids[0].0 < ids[1].0));
        for (_, key, symbol) in program.symbols.iter() {
            assert_eq!(symbol.semantic_id, key);
            assert!(module.declared_type_of_symbol(key).is_some());
        }
    }

    #[test]
    fn symbol_storage_classifies_globals_locals_and_parameters() {
        let module = checked_program(concat!(
            "let global: I32 = 1\n",
            "let mut mutable_global: I32 = 2\n",
            "def update: mut I32 -> I32 = mut value: I32 => { value = value + 1; value }\n",
            "def local_values: () -> I32 = () => {\n",
            "  let local: I32 = 3\n",
            "  let mut mutable_local: I32 = 4\n",
            "  local + mutable_local\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let global = lowered_symbol(&program, binding_symbol(&module, "global"));
        assert_eq!(global.storage, SymbolStorage::GlobalStorage);
        assert!(!global.captured_cell && !global.mutated_parameter);
        let mutable_global = lowered_symbol(&program, binding_symbol(&module, "mutable_global"));
        assert_eq!(mutable_global.storage, SymbolStorage::GlobalStorage);

        let update = module
            .functions()
            .iter()
            .find(|function| function.name.contains("update"))
            .expect("update function");
        let parameter = pattern_symbols(module.resolved(), &update.pattern)[0];
        let lowered = lowered_symbol(&program, parameter);
        assert_eq!(lowered.storage, SymbolStorage::MutableCell);
        assert!(lowered.mutated_parameter);
        assert!(module.is_mutated_parameter(parameter));

        let local_values = module
            .functions()
            .iter()
            .find(|function| function.name.contains("local_values"))
            .expect("local_values function");
        let parameters = pattern_symbols(module.resolved(), &local_values.pattern);
        let locals = module
            .resolved()
            .symbols_in_id_order()
            .into_iter()
            .filter(|info| info.owner == Some(local_values.id))
            .filter(|info| !parameters.contains(&info.id))
            .collect::<Vec<_>>();
        assert_eq!(locals.len(), 2);
        assert_eq!(
            lowered_symbol(&program, locals[0].id).storage,
            SymbolStorage::ImmutableValue
        );
        assert_eq!(
            lowered_symbol(&program, locals[1].id).storage,
            SymbolStorage::MutableCell
        );
        assert!(module.has_mutable_storage(locals[1].id));
    }

    #[test]
    fn symbol_storage_classifies_captured_cells_and_borrowed_captures() {
        let module = checked_program(concat!(
            "def counter: () -> () -> I32 = () => {\n",
            "  let mut count: I32 = 0\n",
            "  () => { count = count + 1; count }\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let inner = module
            .functions()
            .iter()
            .find(|function| !function.captures.is_empty())
            .expect("capturing closure");
        let captured = inner.captures[0];
        let lowered = lowered_symbol(&program, captured);
        assert_eq!(lowered.storage, SymbolStorage::CapturedCell);
        assert!(lowered.captured_cell);
        assert!(
            lowered.requires_initialization_check
                == module.resolved().requires_initialization_state(captured)
        );
        assert_eq!(
            lowered.captured_cell,
            capture_requires_cell(&module, captured)
        );

        let module = checked_program(concat!(
            "type MyString = ctor String\n",
            "impl !Copy MyString {}\n",
            "companion MyString {\n",
            "  pub def concat = a: MyString => b: MyString => MyString (a.* + b.*)\n",
            "}\n",
            "def local = (left: MyString, right: MyString) => {\n",
            "  let append = MyString.concat left\n",
            "  append right\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let (_function, borrowed) = module
            .functions()
            .iter()
            .chain(module.implicit_thunks_in_id_order())
            .flat_map(|function| {
                function
                    .captures
                    .iter()
                    .map(move |symbol| (function, *symbol))
            })
            .find(|(function, symbol)| module.is_borrowed_capture(function.id, *symbol))
            .expect("a borrowed capture");
        assert!(!module.is_move_parameter(borrowed));
        let lowered = lowered_symbol(&program, borrowed);
        assert_eq!(lowered.storage, SymbolStorage::ImmutableValue);
        assert_eq!(
            lowered.captured_cell,
            capture_requires_cell(&module, borrowed)
        );
        assert!(!lowered.captured_cell);
        assert_eq!(lowered.owner, module.resolved().symbol_owner(borrowed));
        assert!(lowered.owner.is_some());
    }

    #[test]
    fn symbol_storage_classifies_functions_signals_derived_and_singletons() {
        let module = checked_program(concat!(
            "let signal count: I32 = 1\n",
            "let doubled: I32 = count + count\n",
            "def double: I32 -> I32 = value => value + value\n",
            "type Wrapper = ctor I32\n",
            "type Enabled\n",
            "let enabled: Enabled = Enabled\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let count = lowered_symbol(&program, binding_symbol(&module, "count"));
        assert_eq!(count.storage, SymbolStorage::Signal);
        assert!(count.signal);
        let doubled = lowered_symbol(&program, binding_symbol(&module, "doubled"));
        assert_eq!(doubled.storage, SymbolStorage::DerivedBinding);
        assert!(doubled.derived);

        let double_symbol = binding_symbol(&module, "double");
        let double = lowered_symbol(&program, double_symbol);
        assert_eq!(double.storage, SymbolStorage::FunctionBinding);
        assert_eq!(double.function, module.function_for_symbol(double_symbol));
        assert!(double.function.is_some());

        let wrapper_id = module
            .resolved()
            .type_declarations()
            .iter()
            .find_map(|(id, _)| {
                (module.resolved().type_name(*id) == Some("Wrapper")).then_some(*id)
            })
            .expect("Wrapper type");
        let (constructor, constructor_type) = module
            .resolved()
            .constructors()
            .iter()
            .find(|(_, id)| **id == wrapper_id)
            .expect("Wrapper constructor");
        let lowered = lowered_symbol(&program, *constructor);
        assert_eq!(lowered.storage, SymbolStorage::FunctionBinding);
        assert_eq!(lowered.constructor, Some(*constructor_type));

        let enabled_id = module
            .resolved()
            .type_declarations()
            .iter()
            .find_map(|(id, _)| {
                (module.resolved().type_name(*id) == Some("Enabled")).then_some(*id)
            })
            .expect("Enabled type");
        let (singleton, singleton_type) = module
            .resolved()
            .singleton_values()
            .iter()
            .find(|(_, id)| **id == enabled_id)
            .expect("Enabled singleton");
        let lowered = lowered_symbol(&program, *singleton);
        assert_eq!(lowered.storage, SymbolStorage::FunctionBinding);
        assert_eq!(lowered.singleton, Some(*singleton_type));
    }

    #[test]
    fn symbol_storage_classifies_extern_and_intrinsic_symbols() {
        let module = checked_program(concat!(
            "use std.cinterop.*\n",
            "extern \"c\" {\n",
            "  my_puts: (CPointer CChar) -> I32\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let external = module
            .resolved()
            .symbols_in_id_order()
            .into_iter()
            .find(|info| module.resolved().is_external_symbol(info.id))
            .expect("external symbol");
        let lowered = lowered_symbol(&program, external.id);
        assert_eq!(lowered.storage, SymbolStorage::ExternalSymbol);
        assert!(lowered.external);

        let intrinsic = module
            .resolved()
            .symbols_in_id_order()
            .into_iter()
            .find(|info| module.resolved().intrinsic_function(info.id).is_some())
            .expect("intrinsic symbol");
        let lowered = lowered_symbol(&program, intrinsic.id);
        assert_eq!(lowered.storage, SymbolStorage::GlobalStorage);
        assert!(lowered.intrinsic.is_some());
        assert!(!lowered.external);
    }

    #[test]
    fn symbol_catalog_excludes_macro_quote_placeholders() {
        let module = checked_program(concat!(
            "use std.syntax.(Expr, parse_quote)\n",
            "macro double: Expr -> Expr = value => parse_quote { $value + $value }\n",
            "let answer: I32 = double 21\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let resolved = module.resolved();
        for (_, key, _) in program.symbols.iter() {
            let info = resolved
                .symbols_in_id_order()
                .into_iter()
                .find(|info| info.id == key)
                .expect("catalogued symbol should be declared");
            assert!(info.module_symbol || info.owner.is_some());
            assert!(!resolved.is_const_symbol(key));
        }
    }

    fn lowered_type<'a>(program: &'a LoweredProgram, name: &str) -> &'a LoweredTypeMetadata {
        program
            .types
            .iter()
            .find_map(|(_, _, info)| (info.name == name).then_some(info))
            .unwrap_or_else(|| panic!("`{name}` should be lowered"))
    }

    fn lowered_trait<'a>(program: &'a LoweredProgram, name: &str) -> &'a LoweredTraitMetadata {
        program
            .traits
            .iter()
            .find_map(|(_, _, info)| (info.name == name).then_some(info))
            .unwrap_or_else(|| panic!("`{name}` should be lowered"))
    }

    #[test]
    fn type_catalog_matches_resolver_order_and_keeps_compact_templates() {
        let module = checked_program(concat!(
            "type TestPair T = ctor (T, T)\n",
            "type TestInner = ctor I32\n",
            "type TestOuter = ctor (TestInner, TestInner)\n",
            "type TestAlias = alias TestOuter\n",
            "type TestCallback{E} = alias () ->{E} ()\n",
            "type TestHidden = opaque\n",
            "type TestEnabled\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let expected = module
            .resolved()
            .types_in_id_order()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let actual = program
            .types
            .iter()
            .map(|(_, key, _)| key)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(actual.windows(2).all(|ids| ids[0].0 < ids[1].0));

        let inner = lowered_type(&program, "TestInner");
        assert_eq!(
            Some(inner.module),
            module
                .resolved()
                .definition_module(DefinitionId::Type(inner.semantic_id))
        );
    }

    #[test]
    fn trait_catalog_preserves_parameters_methods_and_defaults() {
        let module = checked_program(concat!(
            "trait TestBase T { test_base: T -> T }\n",
            "trait TestConvert Target Position Output where {Target, Position} ~> Output {\n",
            "  test_convert: (Target, Position) -> Output\n",
            "}\n",
            "trait TestOrdered T where TestBase T {\n",
            "  test_first: T -> Bool\n",
            "  test_second: T -> Bool = value => test_first value\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let convert = lowered_trait(&program, "TestConvert");
        assert_eq!(convert.parameters.len(), 3);
        assert_eq!(convert.methods.len(), 1);
        let method = program
            .trait_methods
            .get(convert.methods[0])
            .expect("converted method");
        assert_eq!(method.name, "test_convert");
        assert_eq!(method.trait_id, convert.semantic_id);
        assert!(method.default_function.is_none());

        let ordered = lowered_trait(&program, "TestOrdered");
        assert!(!ordered.prerequisites.is_empty());
        assert_eq!(ordered.methods.len(), 2);
        assert_eq!(ordered.default_methods.len(), 1);
        assert_eq!(ordered.default_methods[0].0, ordered.methods[1]);
        let names = ordered
            .methods
            .iter()
            .map(|id| {
                program
                    .trait_methods
                    .get(*id)
                    .expect("ordered method")
                    .name
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["test_first", "test_second"]);
        let second = program
            .trait_methods
            .get(ordered.methods[1])
            .expect("second method");
        assert_eq!(second.name, "test_second");
        assert_eq!(second.default_function, Some(ordered.default_methods[0].1));
        assert!(
            program
                .functions
                .get(ordered.default_methods[0].1)
                .is_some()
        );
    }

    #[test]
    fn trait_implementation_catalog_records_arguments_bounds_and_negation() {
        let module = checked_program(concat!(
            "trait TestEq T { test_eq: (T, T) -> Bool }\n",
            "impl TestEq I32 { def test_eq = (left, right) => left == right }\n",
            "type TestHandle = ctor I32\n",
            "impl !Copy TestHandle {}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let eq = lowered_trait(&program, "TestEq");
        let implementation = program
            .trait_implementations
            .iter()
            .map(|(_, value)| value)
            .find(|implementation| implementation.trait_id == eq.semantic_id)
            .expect("TestEq implementation");
        assert_eq!(implementation.arguments, vec![CheckedType::I32]);
        assert!(!implementation.negative);
        assert!(implementation.bounds.is_empty());
        assert_eq!(implementation.methods.len(), 1);
        assert_eq!(implementation.methods[0].0, eq.methods[0]);
        assert!(program.functions.get(implementation.methods[0].1).is_some());

        let copy_trait = module.semantic_ids().copy_trait.expect("Copy trait");
        let negative = program
            .trait_implementations
            .iter()
            .map(|(_, value)| value)
            .find(|implementation| implementation.negative)
            .expect("negative implementation");
        assert_eq!(negative.trait_id, copy_trait);
        assert_eq!(negative.arguments.len(), 1);
        assert!(matches!(
            negative.arguments[0],
            CheckedType::Distinct { .. }
        ));
        assert!(negative.methods.is_empty());
    }

    #[test]
    fn semantic_ids_capture_standard_traits_and_runtime_types() {
        let program = snapshot("let answer: I32 = 42\n");
        let ids = &program.semantic_ids;
        for (slot, trait_id) in [
            ("natural", ids.natural_trait),
            ("sized", ids.sized_trait),
            ("copy", ids.copy_trait),
            ("drop", ids.drop_trait),
            ("default", ids.default_trait),
            ("debug", ids.debug_trait),
            ("display", ids.display_trait),
            ("index", ids.index_trait),
            ("mutate_index", ids.mutate_index_trait),
            ("into_iterator", ids.into_iterator_trait),
            ("iterator", ids.iterator_trait),
        ] {
            let trait_id = trait_id.unwrap_or_else(|| panic!("`{slot}` trait should be selected"));
            assert!(
                program.traits.get(trait_id).is_some(),
                "`{slot}` trait should have a catalog record"
            );
        }
        for (slot, type_id) in [("io", ids.io_type), ("reactive", ids.reactive_type)] {
            let type_id = type_id.unwrap_or_else(|| panic!("`{slot}` type should be selected"));
            assert!(
                program.types.get(type_id).is_some(),
                "`{slot}` type should have a catalog record"
            );
        }
        assert!(ids.string_representation.is_some());
        assert!(ids.io_resource.is_some());
        assert!(ids.reactive_resource.is_some());
        assert!(program.validate().is_empty());

        let program = snapshot(concat!("use std.coroutine.*\n", "let answer: I32 = 42\n",));
        let ids = &program.semantic_ids;
        for (slot, type_id) in [
            ("coroutine", ids.coroutine_type),
            ("task", ids.task_type),
            ("completed", ids.completed_type),
            ("cancelled", ids.cancelled_type),
            ("tasks", ids.tasks_type),
            ("scheduler", ids.scheduler_type),
            ("wait", ids.wait_type),
            ("resolver", ids.resolver_type),
            ("completion token", ids.completion_token_type),
        ] {
            let type_id = type_id.unwrap_or_else(|| panic!("`{slot}` type should be selected"));
            assert!(
                program.types.get(type_id).is_some(),
                "`{slot}` type should have a catalog record"
            );
        }
        assert!(program.validate().is_empty());
    }

    #[test]
    fn entry_reactive_requirement_is_recorded() {
        let program = snapshot(concat!(
            "let signal count = 0\n",
            "reaction { let current = count; () }\n",
            "count = 1\n",
        ));
        let ids = &program.semantic_ids;
        assert!(ids.reactive_resource.is_some());
        assert_eq!(
            nominal_type_id(&ids.reactive_resource.as_ref().unwrap().value_type),
            ids.reactive_type
        );
        let (entry_id, _) = entry_module(&program);
        let entry = program
            .initializers
            .get(program.modules.get(entry_id).unwrap().initializer)
            .expect("entry initializer");
        assert!(
            entry
                .resources
                .iter()
                .any(|resource| resource.kind == LoweredEntryResourceKind::Reactive)
        );
        let reactive_provider = program
            .resource_providers
            .iter()
            .find_map(|(_, provider)| {
                (provider.owner == ExpressionOwner::Module(entry_id)
                    && provider.kind == LoweredProviderOriginKind::EntryParameter
                    && provider.scope_exit == LoweredScopeExit::Reactive)
                    .then_some(provider)
            })
            .expect("the entry should install a reactive provider");
        assert_eq!(reactive_provider.target, LoweredProviderTarget::Entry);
        assert!(
            !reactive_provider.indirect,
            "the entry reactive scope is a direct value"
        );
    }

    #[test]
    fn no_prelude_programs_keep_standard_semantic_ids() {
        let program = snapshot("@no_prelude\npub mod\nlet answer: I32 = 1 + 2\n");
        let ids = &program.semantic_ids;
        assert!(ids.copy_trait.is_some());
        assert!(ids.io_type.is_some());
        assert!(ids.io_resource.is_some());
        assert!(program.validate().is_empty());
    }

    #[test]
    fn validator_rejects_semantic_ids_without_catalog_records() {
        let mut program = LoweredProgram::default();
        program.semantic_ids.copy_trait = Some(TraitId(3));
        program.semantic_ids.io_type = Some(TypeId(4));
        let diagnostics = program.validate();
        assert_eq!(diagnostics.len(), 2);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("copy trait TraitId(3)"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("io type TypeId(4)"))
        );
    }

    fn normalized_program_snapshot(program: &LoweredProgram) -> Vec<String> {
        let mut lines = Vec::new();
        for (_, key, info) in program.modules.iter() {
            lines.push(format!("module {key:?} {info:?}"));
        }
        for (_, key, function) in program.functions.iter() {
            lines.push(format!("function {key:?} {function:?}"));
        }
        for (_, key, symbol) in program.symbols.iter() {
            lines.push(format!("symbol {key:?} {symbol:?}"));
        }
        for (_, key, info) in program.types.iter() {
            lines.push(format!("type {key:?} {info:?}"));
        }
        for (_, key, info) in program.traits.iter() {
            lines.push(format!("trait {key:?} {info:?}"));
        }
        for (_, key, info) in program.trait_methods.iter() {
            lines.push(format!("trait method {key:?} {info:?}"));
        }
        for (_, info) in program.trait_implementations.iter() {
            lines.push(format!("trait implementation {info:?}"));
        }
        for (_, initializer) in program.initializers.iter() {
            lines.push(format!("initializer {initializer:?}"));
        }
        for (_, expression) in program.expressions.iter() {
            lines.push(format!("expression {expression:?}"));
        }
        for (_, pattern) in program.patterns.iter() {
            lines.push(format!("pattern {pattern:?}"));
        }
        for (_, place) in program.places.iter() {
            lines.push(format!("place {place:?}"));
        }
        for (_, block) in program.blocks.iter() {
            lines.push(format!("block {block:?}"));
        }
        for (_, item) in program.items.iter() {
            lines.push(format!("item {item:?}"));
        }
        for (_, call) in program.calls.iter() {
            lines.push(format!("call {call:?}"));
        }
        for (_, value) in program.callable_values.iter() {
            lines.push(format!("callable value {value:?}"));
        }
        for (_, provider) in program.resource_providers.iter() {
            lines.push(format!("resource provider {provider:?}"));
        }
        for (_, use_) in program.resource_uses.iter() {
            lines.push(format!("resource use {use_:?}"));
        }
        for (_, with) in program.withs.iter() {
            lines.push(format!("with {with:?}"));
        }
        for (_, operation) in program.reactive_operations.iter() {
            lines.push(format!("reactive operation {operation:?}"));
        }
        for (_, callback) in program.reactive_callbacks.iter() {
            lines.push(format!("reactive callback {callback:?}"));
        }
        for (_, plan) in program.coroutine_plans.iter() {
            lines.push(format!("coroutine plan {plan:?}"));
        }
        for (_, coro) in program.coros.iter() {
            lines.push(format!("coro {coro:?}"));
        }
        for (_, await_) in program.awaits.iter() {
            lines.push(format!("await {await_:?}"));
        }
        for (_, instance) in program.instances.iter() {
            lines.push(format!("function instance {instance:?}"));
        }
        for (_, artifact) in program.artifacts.iter() {
            let key_family = program
                .specializations
                .artifact(artifact.ordinal)
                .map(|key| key.family_name())
                .unwrap_or("<missing>");
            let plan_family = artifact
                .plan
                .as_ref()
                .map(LoweredArtifactPlan::family_name)
                .unwrap_or("<none>");
            lines.push(format!(
                "artifact {} key={key_family} plan={plan_family}",
                artifact.name
            ));
            if let Some(plan) = &artifact.plan {
                lines.push(format!("  plan {plan:?}"));
            }
            for dependency in &artifact.artifacts {
                lines.push(format!(
                    "  artifact edge {} {}",
                    dependency.artifact.index(),
                    dependency.kind.description()
                ));
            }
            for dependency in &artifact.instances {
                lines.push(format!(
                    "  instance edge {} {}",
                    dependency.instance.index(),
                    dependency.kind.description()
                ));
            }
        }
        for (id, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for use_ in &body.artifact_uses {
                lines.push(format!(
                    "  instance {} use artifact {} {} {:?}",
                    id.index(),
                    use_.artifact.index(),
                    use_.kind.description(),
                    use_.site
                ));
            }
        }
        for (id, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for use_ in &body.instance_uses {
                lines.push(format!(
                    "  instance {} use instance {} {} {:?}",
                    id.index(),
                    use_.instance.index(),
                    use_.kind.description(),
                    use_.site
                ));
            }
        }
        for (id, uses) in program.initializer_artifact_uses.iter().enumerate() {
            for use_ in uses {
                lines.push(format!(
                    "  initializer {id} use artifact {} {} {:?}",
                    use_.artifact.index(),
                    use_.kind.description(),
                    use_.site
                ));
            }
        }
        for (id, edges) in program.initializer_artifacts.iter().enumerate() {
            for edge in edges {
                lines.push(format!(
                    "  initializer {id} artifact edge {} {}",
                    edge.artifact.index(),
                    edge.kind.description()
                ));
            }
        }
        for (id, uses) in program.initializer_instance_uses.iter().enumerate() {
            for use_ in uses {
                lines.push(format!(
                    "  initializer {id} use instance {} {} {:?}",
                    use_.instance.index(),
                    use_.kind.description(),
                    use_.site
                ));
            }
        }
        for (id, edges) in program.initializer_instances.iter().enumerate() {
            for edge in edges {
                lines.push(format!(
                    "  initializer {id} instance edge {} {}",
                    edge.instance.index(),
                    edge.kind.description()
                ));
            }
        }
        lines.push(format!("semantic ids {:?}", program.semantic_ids));
        lines.push(format!("string formatting {:?}", program.string_formatting));
        lines
    }

    #[test]
    fn catalogs_are_stable_across_repeated_lowering() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "let signal count: I32 = 1\n",
            "let doubled: I32 = count + count\n",
            "def add: (I32, I32) -> I32 = (left, right) => left + right\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "type TestBox T = ctor (value: T)\n",
            "type TestHidden = opaque\n",
            "type TestEnabled\n",
            "let enabled: TestEnabled = TestEnabled\n",
            "let boxed: TestBox I32 = TestBox 3\n",
            "def captured: () ->{state} I32 = () => { let mut local = 0; local = local + 1; local }\n",
        ));
        let mut first = LoweredProgram::default();
        let mut second = LoweredProgram::default();
        let diagnostics = first.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(first.validate().is_empty());
        assert!(second.snapshot(&module).is_empty());
        assert!(second.validate().is_empty());

        let first_snapshot = normalized_program_snapshot(&first);
        assert!(!first_snapshot.is_empty());
        assert_eq!(first_snapshot, normalized_program_snapshot(&second));
    }

    #[test]
    fn closure_with_production_hooks_preserves_specialization_identity() {
        let source = concat!(
            "use std.coroutine.*\n",
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def forward: <T where Copy T> T -> T = value => identity value\n",
            "let first: I32 = forward 1\n",
            "type Point = ctor (I32, I32)\n",
            "let make: () -> ((I32, I32) -> Point) = () => Point\n",
            "let p = (1, 2)\n",
            "let text = \"${p:?}\"\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "let created = task ()\n",
        );
        let module = checked_program(source);

        // The pre-closure specialization program: graph plus materialized bodies.
        let mut baseline = LoweredProgram::default();
        assert!(baseline.snapshot(&module).is_empty());
        assert!(baseline.validate().is_empty());
        assert!(baseline.build_specialization_worklist().is_empty());
        assert!(baseline.materialize_instance_bodies().is_empty());
        assert!(baseline.validate_instance_bodies().is_empty());

        let lowered = Lowerer::new().lower(&module).expect("lowering succeeds");
        // Artifact expansion and cleanup scanning append generated
        // artifacts and uses, but the specialization prefix keeps its ordinals,
        // names, and keys. Only appended entries differ, so compare identity
        // rather than the full plan snapshot.
        assert!(lowered.program.instances.len() >= baseline.instances.len());
        assert!(lowered.program.artifacts.len() >= baseline.artifacts.len());
        assert_eq!(
            baseline
                .instances
                .iter()
                .map(|(id, instance)| (id.index(), instance.name.clone()))
                .collect::<Vec<_>>(),
            lowered
                .program
                .instances
                .iter()
                .take(baseline.instances.len())
                .map(|(id, instance)| (id.index(), instance.name.clone()))
                .collect::<Vec<_>>(),
            "specialization instance ordinals and names are unchanged"
        );
        assert_eq!(
            baseline
                .artifacts
                .iter()
                .map(|(id, artifact)| (id.index(), artifact.name.clone()))
                .collect::<Vec<_>>(),
            lowered
                .program
                .artifacts
                .iter()
                .take(baseline.artifacts.len())
                .map(|(id, artifact)| (id.index(), artifact.name.clone()))
                .collect::<Vec<_>>(),
            "specialization artifact ordinals and names are unchanged"
        );
        let mut expanded_adapters = 0;
        for (_, artifact) in lowered.program.artifacts.iter() {
            if let Some(LoweredArtifactPlan::ConstructorAdapter(plan)) = &artifact.plan {
                assert!(
                    !matches!(plan.construction, ConstructorConstruction::Unexpanded),
                    "ProductionHooks expand every constructor adapter"
                );
                expanded_adapters += 1;
            }
        }
        assert!(
            expanded_adapters > 0,
            "the fixture reserves a constructor adapter"
        );
        // The cleanup scanner records cleanup uses and edges, so the fixture
        // must show at least one of each; the validator already proved the
        // one-to-one agreement inside `Lowerer::lower`.
        let mut artifact_uses = 0;
        for (_, instance) in lowered.program.instances.iter() {
            if let Some(body) = &instance.body {
                artifact_uses += body.artifact_uses.len();
            }
        }
        artifact_uses += lowered
            .program
            .initializer_artifact_uses
            .iter()
            .map(Vec::len)
            .sum::<usize>();
        assert!(
            artifact_uses > 0,
            "the cleanup scanner records at least one artifact use"
        );
    }

    #[test]
    fn records_agree_with_checked_metadata_and_are_stable() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "type Counter = ctor (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "let signal count = 0\n",
            "let doubled = count + count\n",
            "def task: () -> Coroutine{mut Counter} I32 = () => coro {\n",
            "  increment ()\n",
            "  0\n",
            "}\n",
            "def driver: () -> Coroutine{mut Counter} I32 = () => coro {\n",
            "  let child = task ()\n",
            "  await child\n",
            "}\n",
            "with Reactive = reactive_scope () {\n",
            "  reaction { let current = count; () }\n",
            "  batch { count = 1 }\n",
            "  let observed = snapshot count\n",
            "}\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
        ));
        let mut first = LoweredProgram::default();
        let mut second = LoweredProgram::default();
        let diagnostics = first.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(first.validate().is_empty());
        assert!(second.snapshot(&module).is_empty());
        assert!(second.validate().is_empty());
        assert_eq!(
            normalized_program_snapshot(&first),
            normalized_program_snapshot(&second),
            "repeated lowering is deterministic for every lowering record"
        );

        // The executable entry installs exactly the checker's resources.
        let entry = first
            .initializers
            .iter()
            .find_map(|(_, initializer)| initializer.executable_entry.then_some(initializer))
            .expect("the executable entry");
        let expected_entry_resources = entry_resources(&module);
        assert_eq!(entry.resources.len(), expected_entry_resources.len());
        for (actual, expected) in entry.resources.iter().zip(&expected_entry_resources) {
            assert_eq!(actual.kind, expected.kind);
            assert_eq!(actual.resource, expected.resource);
        }

        // Providers agree with their checked resource or effect-row position.
        for (_, provider) in first.resource_providers.iter() {
            match provider.target {
                LoweredProviderTarget::Expression(_) => {
                    // The provider's origin syntax is its `with` expression.
                    assert_eq!(
                        Some(&provider.resource),
                        module.resource_for_expression(provider.origin.syntax)
                    );
                }
                LoweredProviderTarget::EffectParameter { position } => {
                    let function = first
                        .functions
                        .iter()
                        .find_map(|(_, _, function)| {
                            (function.body_syntax == provider.origin.syntax).then_some(function)
                        })
                        .expect("the provider's owning function");
                    assert_eq!(
                        provider.resource,
                        function.signature.effects.resources[position]
                    );
                }
                LoweredProviderTarget::Entry => {
                    let entry = first
                        .initializers
                        .iter()
                        .find_map(|(_, initializer)| {
                            initializer.executable_entry.then_some(initializer)
                        })
                        .expect("the executable entry");
                    assert!(entry.resources.iter().any(|resource| {
                        resource.resource == provider.resource
                            && matches!(
                                (resource.kind, provider.scope_exit),
                                (LoweredEntryResourceKind::Io, LoweredScopeExit::Ordinary)
                                    | (
                                        LoweredEntryResourceKind::Reactive,
                                        LoweredScopeExit::Reactive
                                    )
                            )
                    }));
                }
            }
        }

        // Every resource use checks against its occurrence metadata.
        for (_, use_) in first.resource_uses.iter() {
            if let Some(checked) = module.resource_for_expression(use_.origin.syntax) {
                assert_eq!(&use_.resource, checked);
            }
        }

        // Reactive callbacks link the checker's implicit thunks and captures.
        for (_, callback) in first.reactive_callbacks.iter() {
            let Some(thunk) = callback.thunk else {
                continue;
            };
            let checked = module
                .implicit_thunk_for(callback.origin.syntax)
                .expect("checked callback thunk");
            assert_eq!(thunk, checked.id);
            assert_eq!(
                callback
                    .captures
                    .iter()
                    .map(|capture| capture.symbol)
                    .collect::<Vec<_>>(),
                checked.captures
            );
        }

        // Signal and derived bindings match the checked symbol classification.
        for (_, item) in first.items.iter() {
            let LoweredItemKind::Binding(binding) = &item.kind else {
                continue;
            };
            let Some(symbol) = binding.symbol else {
                continue;
            };
            if module.resolved().is_signal_symbol(symbol) {
                assert!(matches!(
                    binding
                        .reactive
                        .and_then(|operation| first.reactive_operations.get(operation))
                        .map(|operation| &operation.kind),
                    Some(LoweredReactiveOperationKind::SignalCreate { symbol: target, .. })
                        if *target == symbol
                ));
            }
            if module.is_derived_symbol(symbol) {
                let Some(LoweredReactiveOperationKind::DerivedCreate {
                    evaluator,
                    function_type,
                    ..
                }) = binding
                    .reactive
                    .and_then(|operation| first.reactive_operations.get(operation))
                    .map(|operation| &operation.kind)
                else {
                    panic!("derived binding should record its evaluator");
                };
                let checked = module
                    .derived_evaluator(symbol)
                    .expect("checked derived evaluator");
                assert_eq!(*evaluator, checked.id);
                assert_eq!(Some(function_type), module.type_of_function(checked.id));
            }
        }

        // Coroutine plans copy every scanner field exactly.
        for (_, plan) in first.coroutine_plans.iter() {
            let checked = module
                .coroutine_plan(plan.body_syntax)
                .expect("checked coroutine plan");
            assert_eq!(plan.result_type, checked.result_type);
            assert_eq!(plan.deferred_effects, checked.deferred_effects);
            assert_eq!(plan.resume_points, checked.resume_points);
            assert_eq!(plan.frame_bindings, checked.frame_bindings);
            assert_eq!(plan.await_result_types, checked.await_result_types);
            assert_eq!(plan.wait_await_states, checked.wait_await_states);
            assert_eq!(plan.until_await_states, checked.until_await_states);
            let thunk = module
                .implicit_thunk_for(plan.body_syntax)
                .expect("checked body thunk");
            assert_eq!(plan.thunk, thunk.id);
            assert_eq!(
                plan.captures
                    .iter()
                    .map(|capture| capture.symbol)
                    .collect::<Vec<_>>(),
                thunk.captures
            );
        }

        // Await sites mirror the checked operand classification.
        for (_, await_) in first.awaits.iter() {
            let operand_syntax = first
                .expressions
                .get(await_.operand)
                .expect("await operand")
                .key
                .syntax;
            let operand_type = module
                .type_of_expression(operand_syntax)
                .expect("checked await operand type");
            match &await_.kind {
                LoweredAwaitKind::Task { result } => {
                    assert_eq!(Some(result), module.task_result(operand_type));
                }
                LoweredAwaitKind::Wait { result } => {
                    assert_eq!(Some(result), module.wait_result(operand_type));
                }
                LoweredAwaitKind::ChildCoroutine { child_result, .. } => {
                    assert_eq!(
                        Some(child_result),
                        module
                            .coroutine_parts(operand_type)
                            .map(|(_, result)| result)
                    );
                }
            }
        }
    }

    #[test]
    fn semantic_ids_match_transition_typed_module_selections() {
        let module = checked_program(concat!("use std.coroutine.*\n", "let answer: I32 = 42\n",));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let checked = module.semantic_ids();
        let ids = &program.semantic_ids;
        assert_eq!(ids.natural_trait, checked.natural_trait);
        assert_eq!(ids.sized_trait, checked.sized_trait);
        assert_eq!(ids.copy_trait, checked.copy_trait);
        assert_eq!(ids.drop_trait, checked.drop_trait);
        assert_eq!(ids.default_trait, checked.default_trait);
        assert_eq!(ids.debug_trait, checked.debug_trait);
        assert_eq!(ids.display_trait, checked.display_trait);
        assert_eq!(ids.index_trait, checked.index_trait);
        assert_eq!(ids.mutate_index_trait, checked.mutate_index_trait);
        assert_eq!(ids.into_iterator_trait, checked.into_iterator_trait);
        assert_eq!(ids.iterator_trait, checked.iterator_trait);
        assert_eq!(ids.io_type, checked.io_type);
        assert_eq!(ids.reactive_type, checked.reactive_type);
        assert_eq!(ids.coroutine_type, checked.coroutine_type);
        assert_eq!(ids.task_type, checked.task_type);
        assert_eq!(ids.completed_type, checked.completed_type);
        assert_eq!(ids.cancelled_type, checked.cancelled_type);
        assert_eq!(ids.tasks_type, checked.tasks_type);
        assert_eq!(ids.scheduler_type, checked.scheduler_type);
        assert_eq!(ids.wait_type, checked.wait_type);
        assert_eq!(ids.resolver_type, checked.resolver_type);
        assert_eq!(ids.completion_token_type, checked.completion_token_type);
        assert_eq!(ids.io_resource, module.io_resource());
        assert_eq!(ids.reactive_resource, module.reactive_resource());
        assert_eq!(
            ids.string_representation.as_ref(),
            module.string_representation()
        );
    }

    #[test]
    fn validator_rejects_inconsistent_catalogs() {
        let module = checked_program("let answer: I32 = 42\n");
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        program.modules.entries.values[0].value.parent = Some(ModuleId(999));
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling module reference 999")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let entry_id = program.modules.entries.values[0].key;
        let body = program.initializers.values[0].body;
        program.initializers.push(LoweredInitializer {
            name: String::new(),
            origin: Origin::compiler(),
            module: entry_id,
            resources: Vec::new(),
            body,
            executable_entry: false,
        });
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("initializers instead of one")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        program.symbols.entries.values[0].value.module = ModuleId(999);
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling module reference 999")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn validator_rejects_dangling_arena_references() {
        let mut program = LoweredProgram::default();
        let key = ExpressionKey {
            syntax: SyntaxId(4),
            owner: ExpressionOwner::Module(ModuleId(0)),
            context: ExpressionContext::Primary,
        };
        let id = program.expressions.push(LoweredExpression {
            key,
            origin: Origin::compiler(),
            value_type: CheckedType::I32,
            effects: CheckedEffectSet::default(),
            coercion: None,
            coercion_plan: None,
            moved_symbols: Vec::new(),
            kind: LoweredExpressionKind::Block(BlockId::from_index(4)),
        });
        program.expression_lookup.insert(key, id);
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling block reference 4")),
            "unexpected diagnostics: {diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("not reachable")),
            "the hand-built expression is unreachable"
        );
    }

    fn compiler_syntax() -> Syntax {
        Syntax::compiler()
    }

    fn wildcard_pattern() -> Pattern {
        Pattern::Wildcard(staple_syntax::WildcardPattern {
            syntax: compiler_syntax(),
            ty: Type::Inferred(staple_syntax::InferredType::new()),
        })
    }

    fn name_expression(name: &str) -> Expression {
        Expression::Name(staple_syntax::NameExpression {
            syntax: compiler_syntax(),
            name: name.to_owned(),
        })
    }

    fn empty_block() -> staple_syntax::BlockExpression {
        staple_syntax::BlockExpression {
            syntax: compiler_syntax(),
            items: Vec::new(),
        }
    }

    fn inferred_type() -> Type {
        Type::Inferred(staple_syntax::InferredType::new())
    }

    fn representative_expressions() -> Vec<(&'static str, Expression)> {
        use staple_syntax::*;
        let syntax = compiler_syntax();
        let mut expressions = Vec::new();
        expressions.push((
            "Function",
            Expression::Function(Box::new(FunctionExpression {
                syntax: syntax.clone(),
                parameter_style: FunctionParameterStyle::Single,
                pattern: wildcard_pattern(),
                body: Box::new(name_expression("value")),
            })),
        ));
        expressions.push((
            "Satisfies",
            Expression::Satisfies(Box::new(SatisfiesExpression {
                syntax: syntax.clone(),
                value: Box::new(name_expression("value")),
                ty: inferred_type(),
            })),
        ));
        expressions.push((
            "Match",
            Expression::Match(MatchExpression {
                syntax: syntax.clone(),
                subject: Box::new(name_expression("value")),
                arms: vec![MatchArm {
                    syntax: syntax.clone(),
                    pattern: wildcard_pattern(),
                    body: name_expression("value"),
                }],
            }),
        ));
        expressions.push((
            "Loop",
            Expression::Loop(LoopExpression {
                syntax: syntax.clone(),
                body: empty_block(),
            }),
        ));
        expressions.push((
            "Coro",
            Expression::Coro(CoroExpression {
                syntax: syntax.clone(),
                body: empty_block(),
            }),
        ));
        expressions.push((
            "Await",
            Expression::Await(AwaitExpression {
                syntax: syntax.clone(),
                operand: Box::new(name_expression("value")),
            }),
        ));
        expressions.push((
            "Resource",
            Expression::Resource(Box::new(ResourceExpression {
                syntax: syntax.clone(),
                resource: inferred_type(),
            })),
        ));
        expressions.push((
            "With",
            Expression::With(Box::new(WithResourceExpression {
                syntax: syntax.clone(),
                resource: inferred_type(),
                mutable: false,
                value: Box::new(name_expression("value")),
                body: empty_block(),
            })),
        ));
        expressions.push(("Block", Expression::Block(empty_block())));
        expressions.push((
            "Product",
            Expression::Product(ProductExpression {
                syntax: syntax.clone(),
                elements: vec![ProductElement {
                    syntax: syntax.clone(),
                    name: None,
                    designated: false,
                    value: name_expression("value"),
                    spread: false,
                    named_spread: false,
                }],
            }),
        ));
        expressions.push((
            "RepeatedProduct",
            Expression::RepeatedProduct(RepeatedProductExpression {
                syntax: syntax.clone(),
                value: Box::new(name_expression("value")),
                count: Box::new(inferred_type()),
            }),
        ));
        expressions.push((
            "Call",
            Expression::Call(CallExpression {
                syntax: syntax.clone(),
                callee: Box::new(name_expression("value")),
                argument: Box::new(name_expression("value")),
            }),
        ));
        expressions.push((
            "Access",
            Expression::Access(AccessExpression {
                syntax: syntax.clone(),
                value: Box::new(name_expression("value")),
                accessor: Accessor::Name("field".to_owned()),
            }),
        ));
        expressions.push((
            "Index",
            Expression::Index(IndexExpression {
                syntax: syntax.clone(),
                value: Box::new(name_expression("value")),
                index: Box::new(name_expression("position")),
            }),
        ));
        expressions.push((
            "Unary",
            Expression::Unary(UnaryExpression {
                syntax: syntax.clone(),
                operator_syntax: syntax.clone(),
                operator: UnaryOperator::Negate,
                operand: Box::new(name_expression("value")),
            }),
        ));
        expressions.push((
            "Binary",
            Expression::Binary(BinaryExpression {
                syntax: syntax.clone(),
                operator_syntax: syntax.clone(),
                operator: BinaryOperator::Add,
                left: Box::new(name_expression("value")),
                right: Box::new(name_expression("value")),
            }),
        ));
        expressions.push((
            "Logical",
            Expression::Logical(LogicalExpression {
                syntax: syntax.clone(),
                operator: LogicalOperator::And,
                left: Box::new(name_expression("value")),
                right: Box::new(name_expression("value")),
                bool_type: inferred_type(),
            }),
        ));
        expressions.push((
            "SyntaxArgument",
            Expression::SyntaxArgument(SyntaxArgumentExpression {
                syntax: syntax.clone(),
            }),
        ));
        expressions.push((
            "VisibilityArgument",
            Expression::VisibilityArgument(VisibilitySyntax {
                syntax: syntax.clone(),
                kind: VisibilityKind::Private,
            }),
        ));
        expressions.push((
            "Quote",
            Expression::Quote(QuoteExpression {
                syntax: syntax.clone(),
                kind: QuoteKind::Quote,
                path: Vec::new(),
                contents: syntax.clone(),
                template: QuoteTemplate::Raw,
            }),
        ));
        expressions.push((
            "Splice",
            Expression::Splice(SpliceExpression {
                syntax: syntax.clone(),
                name: "value".to_owned(),
                repeated: false,
            }),
        ));
        expressions.push(("Name", name_expression("value")));
        expressions.push((
            "String",
            Expression::String(StringExpression {
                syntax: syntax.clone(),
                literal: "\"value\"".to_owned(),
            }),
        ));
        expressions.push((
            "StringTemplate",
            Expression::StringTemplate(StringTemplateExpression {
                syntax: syntax.clone(),
                parts: vec![
                    StringTemplatePart::Literal("value ".to_owned()),
                    StringTemplatePart::Interpolation(StringInterpolation {
                        expression: Box::new(name_expression("value")),
                        format: StringInterpolationFormat::Display,
                    }),
                ],
            }),
        ));
        expressions.push((
            "CString",
            Expression::CString(CStringExpression {
                syntax: syntax.clone(),
                literal: "c\"value\"".to_owned(),
            }),
        ));
        expressions.push((
            "Integer",
            Expression::Integer(IntegerExpression {
                syntax: syntax.clone(),
                literal: "1".to_owned(),
            }),
        ));
        expressions.push((
            "Float",
            Expression::Float(FloatExpression {
                syntax,
                literal: "1.0".to_owned(),
            }),
        ));
        expressions
    }

    #[test]
    fn coverage_classifier_decides_every_expression_variant() {
        use ExpressionDisposition::{Ordinary, Rejected, ResourceCoroutine};
        use ResourceCoroutineRoute as Route;
        let module = checked_program("let value: I32 = 1\n");
        let representatives = representative_expressions();
        let mut names = representatives
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            representatives.len(),
            "representative expressions must cover each variant exactly once"
        );

        for (name, expression) in &representatives {
            assert_eq!(expression_variant_name(expression), *name);
            let expected = match *name {
                "Function" => Ordinary(OrdinaryExpressionFamily::Function),
                "Call" => Ordinary(OrdinaryExpressionFamily::Call),
                "Resource" => ResourceCoroutine(Route::ResourceUse),
                "With" => ResourceCoroutine(Route::ResourceProvider),
                "Coro" => ResourceCoroutine(Route::CoroutineCreation),
                "Await" => ResourceCoroutine(Route::AwaitChildCoroutine),
                "Unary" | "Binary" | "SyntaxArgument" | "VisibilityArgument" | "Quote"
                | "Splice" => Rejected,
                "Satisfies" => Ordinary(OrdinaryExpressionFamily::Satisfies),
                "Match" => Ordinary(OrdinaryExpressionFamily::Match),
                "Loop" => Ordinary(OrdinaryExpressionFamily::Loop),
                "Block" => Ordinary(OrdinaryExpressionFamily::Block),
                "Product" => Ordinary(OrdinaryExpressionFamily::Product),
                "RepeatedProduct" => Ordinary(OrdinaryExpressionFamily::RepeatedProduct),
                "Access" => Ordinary(OrdinaryExpressionFamily::Access),
                "Index" => Ordinary(OrdinaryExpressionFamily::Index),
                "Logical" => Ordinary(OrdinaryExpressionFamily::Logical),
                "Name" => Ordinary(OrdinaryExpressionFamily::Name),
                "String" => Ordinary(OrdinaryExpressionFamily::String),
                "StringTemplate" => Ordinary(OrdinaryExpressionFamily::StringTemplate),
                "CString" => Ordinary(OrdinaryExpressionFamily::CString),
                "Integer" => Ordinary(OrdinaryExpressionFamily::Integer),
                "Float" => Ordinary(OrdinaryExpressionFamily::Float),
                other => panic!("unclassified expression variant {other}"),
            };
            assert_eq!(classify_expression(&module, expression), expected, "{name}");
        }
    }

    fn deferred_families(program: &LoweredProgram) -> Vec<DeferredExpressionFamily> {
        program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match expression.kind {
                LoweredExpressionKind::Deferred(family) => Some(family),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn dispatcher_lowers_every_expression_family_without_deferrals() {
        let module = checked_program(concat!(
            "use std.coroutine.(Coroutine)\n",
            "type Counter = ctor (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "def task: () -> Coroutine{} I32 = () => coro { 42 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro {\n",
            "  let value = await (task ()); value\n",
            "}\n",
            "let closure = (value: I32) => value\n",
            "let applied = task ()\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let check = program.validate();
        assert!(check.is_empty(), "{check:?}");
        let deferred = deferred_families(&program);
        assert!(
            deferred.is_empty(),
            "every callable, resource, and coroutine expression lowers to an owned node; have {deferred:?}"
        );
    }

    #[test]
    fn resource_coroutine_route_table_covers_every_route() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "type Counter = ctor (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "def child: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def spawned: I32 -> Coroutine{} I32 = base => coro { base + 1 }\n",
            "def wait_observer: move Wait I32 -> Coroutine{} () = move w => coro {\n",
            "  let _ = await w; ()\n",
            "}\n",
            "def driver: () -> Coroutine{Tasks} I32 = () => coro {\n",
            "  let c = await (child ())\n",
            "  let task = spawn (spawned c)\n",
            "  let outcome = await task\n",
            "  let _ = outcome; c\n",
            "}\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let mut kinds = program
            .expressions
            .iter()
            .map(|(_, expression)| expression_kind_name(&expression.kind))
            .collect::<Vec<_>>();
        kinds.sort_unstable();
        kinds.dedup();
        for expected in ["resource", "with", "coro", "await"] {
            assert!(
                kinds.iter().any(|kind| kind == expected),
                "every lowering expression should lower concretely; have {kinds:?}"
            );
        }
        let mut families = ResourceCoroutineRoute::ALL
            .iter()
            .map(|route| route.record_family())
            .collect::<Vec<_>>();
        families.sort_unstable();
        families.dedup();
        assert_eq!(
            families,
            vec!["Await", "Coro", "ResourceUse", "With"],
            "every route names exactly one owned record family"
        );
    }

    #[test]
    fn intrinsic_route_table_covers_reactive_and_coroutine_intrinsics() {
        use crate::IntrinsicFunction as I;
        use CoroutineIntrinsicRoute as Coro;
        use IntrinsicRoute::{Coroutine as CoroRoute, Reactive as ReactiveRoute};
        use ReactiveIntrinsicRoute as React;

        let table = [
            (I::ReactiveScope, ReactiveRoute(React::Scope)),
            (I::Reaction, ReactiveRoute(React::Reaction)),
            (I::Batch, ReactiveRoute(React::Batch)),
            (I::Until, ReactiveRoute(React::Until)),
            (I::Snapshot, ReactiveRoute(React::Snapshot)),
            (I::CoroutineBlockOn, CoroRoute(Coro::BlockOn)),
            (I::SchedulerCreate, CoroRoute(Coro::SchedulerCreate)),
            (I::TaskScope, CoroRoute(Coro::TaskScope)),
            (I::Spawn, CoroRoute(Coro::Spawn)),
            (I::Pump, CoroRoute(Coro::Pump)),
            (I::YieldNow, CoroRoute(Coro::YieldNow)),
            (I::TaskIsFinished, CoroRoute(Coro::TaskIsFinished)),
            (I::TaskCancel, CoroRoute(Coro::TaskCancel)),
            (I::Completion, CoroRoute(Coro::Completion)),
            (
                I::CompletionWithCancel,
                CoroRoute(Coro::CompletionWithCancel),
            ),
            (I::CompletionToken, CoroRoute(Coro::CompletionToken)),
            (
                I::CompletionTokenResolve,
                CoroRoute(Coro::CompletionTokenResolve),
            ),
            (
                I::CompletionTokenCancel,
                CoroRoute(Coro::CompletionTokenCancel),
            ),
            (I::ResolverComplete, CoroRoute(Coro::ResolverComplete)),
            (I::ResolverCancel, CoroRoute(Coro::ResolverCancel)),
        ];
        for (intrinsic, expected) in table {
            let route =
                intrinsic_route(intrinsic).expect("reactive/coroutine intrinsic has a route");
            assert_eq!(
                route, expected,
                "intrinsic {intrinsic:?} has an explicit route"
            );
        }
        let mut reactive_routes = table
            .iter()
            .filter_map(|(intrinsic, _)| match intrinsic_route(*intrinsic) {
                Some(IntrinsicRoute::Reactive(route)) => Some(route),
                _ => None,
            })
            .collect::<Vec<_>>();
        reactive_routes.sort_by_key(|route| format!("{route:?}"));
        reactive_routes.dedup();
        assert_eq!(reactive_routes.len(), React::ALL.len());

        let mut coroutine_routes = table
            .iter()
            .filter_map(|(intrinsic, _)| match intrinsic_route(*intrinsic) {
                Some(IntrinsicRoute::Coroutine(route)) => Some(route),
                _ => None,
            })
            .collect::<Vec<_>>();
        coroutine_routes.sort_by_key(|route| format!("{route:?}"));
        coroutine_routes.dedup();
        assert_eq!(coroutine_routes.len(), Coro::ALL.len());

        assert!(intrinsic_route(I::Drop).is_none());
        assert!(intrinsic_route(I::StringAdd).is_none());
    }

    #[test]
    fn call_route_decision_table_covers_every_callable_category() {
        let mut routes = HashSet::new();
        let mut categories = HashSet::new();
        for route in CallRoute::ALL {
            assert!(routes.insert(route), "route {route:?} appears twice");
            categories.insert(route.category());
        }
        for category in LoweredCallableCategory::ALL {
            assert!(
                categories.contains(&category),
                "no call route produces callable category {category:?}"
            );
        }

        let targets = [
            LoweredCallableTarget::DirectFunction {
                function: FunctionId(0),
                environment: LoweredCallEnvironment::None,
            },
            LoweredCallableTarget::IndirectClosure {
                callee: ExpressionId(0),
            },
            LoweredCallableTarget::ExternalFunction {
                symbol: SymbolId(0),
            },
            LoweredCallableTarget::Intrinsic {
                symbol: SymbolId(0),
                intrinsic: IntrinsicFunction::StringAdd,
            },
            LoweredCallableTarget::Constructor {
                symbol: SymbolId(0),
                type_id: TypeId(0),
                recursive: None,
            },
            LoweredCallableTarget::TraitImplementation {
                trait_id: TraitId(0),
                method: TraitMethodId(0),
                function: None,
            },
            LoweredCallableTarget::StructuralTraitMethod {
                trait_id: TraitId(0),
                method: TraitMethodId(0),
                structural: StructuralTraitMethod::Debug,
            },
        ];
        let target_categories = targets
            .iter()
            .map(LoweredCallableTarget::category)
            .collect::<HashSet<_>>();
        assert_eq!(
            target_categories.len(),
            LoweredCallableCategory::ALL.len(),
            "each callable category needs exactly one target representation"
        );
        for category in LoweredCallableCategory::ALL {
            assert!(
                target_categories.contains(&category),
                "no callable target represents category {category:?}"
            );
        }
    }

    #[test]
    fn classifies_every_source_call_route_without_a_fallback() {
        let module = checked_program(concat!(
            "use std.cinterop.*\n",
            "extern \"c\" { external_identity: I32 -> I32 }\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def generic_identity: <T where Copy T> T -> T = value => value\n",
            "let juxtaposed: x: I32 * y: I32 -> I32 = x * y => x + y\n",
            "type TestBox = ctor (value: I32)\n",
            "def sum_product: (I32, I32) -> I32 = pair => {\n",
            "  let mut total: I32 = 0\n",
            "  for value in pair { total = total + value }\n",
            "  total\n",
            "}\n",
            "def route_samples: () -> I32 = () => {\n",
            "  let closure = (value: I32) => value\n",
            "  let indirect: I32 = closure (1)\n",
            "  let direct: I32 = generic_identity 1\n",
            "  let arithmetic: I32 = 1 + 2\n",
            "  let juxtaposed_result: I32 = juxtaposed 1 2\n",
            "  let external_result: I32 = external_identity (1)\n",
            "  let built: TestBox = TestBox (value: 1)\n",
            "  let ctext = c_string \"hi\"\n",
            "  let shown: Bool = test_show 1\n",
            "  direct\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let mut routes = Vec::new();
        for function in module.functions().iter().chain(module.implicit_thunks()) {
            collect_call_routes(
                &program,
                &module,
                ExpressionOwner::Function(function.id),
                &function.body,
                &mut routes,
            );
        }
        // `CurriedDefault` is unreachable from accepted source (curried
        // defaults are rejected during resolution), `PrimitiveMacro` calls are
        // normalized to `Expression::CString` by macro expansion and remain a
        // defensive route. The route/category table test covers both; every
        // source-reachable route is asserted here.
        for route in [
            CallRoute::Juxtaposed,
            CallRoute::JuxtaposedIntrinsic,
            CallRoute::TraitImplementation,
            CallRoute::DeclaredTraitBound,
            CallRoute::StructuralTraitMethod,
            CallRoute::Intrinsic,
            CallRoute::GenericDirect,
            CallRoute::External,
            CallRoute::Indirect,
            CallRoute::Constructor,
        ] {
            assert!(
                routes.contains(&route),
                "expected the source decision table to contain route {route:?}, found {routes:?}"
            );
        }
    }

    fn collect_call_routes(
        program: &LoweredProgram,
        module: &TypedModule,
        owner: ExpressionOwner,
        expression: &Expression,
        routes: &mut Vec<CallRoute>,
    ) {
        if let Expression::Call(call) = expression {
            let route = program
                .classify_call_route(module, owner, call)
                .unwrap_or_else(|diagnostic| {
                    panic!("call route classification failed: {}", diagnostic.message)
                });
            routes.push(route);
        }
        match expression {
            Expression::Function(function) => {
                collect_call_routes(program, module, owner, &function.body, routes);
            }
            Expression::Satisfies(satisfies) => {
                collect_call_routes(program, module, owner, &satisfies.value, routes);
            }
            Expression::Match(match_) => {
                collect_call_routes(program, module, owner, &match_.subject, routes);
                for arm in &match_.arms {
                    collect_call_routes(program, module, owner, &arm.body, routes);
                }
            }
            Expression::Loop(loop_) => {
                collect_block_call_routes(program, module, owner, &loop_.body, routes);
            }
            Expression::Coro(coro) => {
                collect_block_call_routes(program, module, owner, &coro.body, routes);
            }
            Expression::Await(await_) => {
                collect_call_routes(program, module, owner, &await_.operand, routes);
            }
            Expression::Resource(_) => {}
            Expression::With(with) => {
                collect_call_routes(program, module, owner, &with.value, routes);
                collect_block_call_routes(program, module, owner, &with.body, routes);
            }
            Expression::Block(block) => {
                collect_block_call_routes(program, module, owner, block, routes);
            }
            Expression::Product(product) => {
                for element in &product.elements {
                    collect_call_routes(program, module, owner, &element.value, routes);
                }
            }
            Expression::RepeatedProduct(repeated) => {
                collect_call_routes(program, module, owner, &repeated.value, routes);
            }
            Expression::Call(call) => {
                collect_call_routes(program, module, owner, &call.callee, routes);
                collect_call_routes(program, module, owner, &call.argument, routes);
            }
            Expression::Access(access) => {
                collect_call_routes(program, module, owner, &access.value, routes);
            }
            Expression::Index(index) => {
                collect_call_routes(program, module, owner, &index.value, routes);
                collect_call_routes(program, module, owner, &index.index, routes);
            }
            Expression::Unary(unary) => {
                collect_call_routes(program, module, owner, &unary.operand, routes);
            }
            Expression::Binary(binary) => {
                collect_call_routes(program, module, owner, &binary.left, routes);
                collect_call_routes(program, module, owner, &binary.right, routes);
            }
            Expression::Logical(logical) => {
                collect_call_routes(program, module, owner, &logical.left, routes);
                collect_call_routes(program, module, owner, &logical.right, routes);
            }
            Expression::StringTemplate(template) => {
                for part in &template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        collect_call_routes(
                            program,
                            module,
                            owner,
                            &interpolation.expression,
                            routes,
                        );
                    }
                }
            }
            Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn collect_block_call_routes(
        program: &LoweredProgram,
        module: &TypedModule,
        owner: ExpressionOwner,
        block: &staple_syntax::BlockExpression,
        routes: &mut Vec<CallRoute>,
    ) {
        for item in &block.items {
            match item {
                Item::Binding(binding) => {
                    if let Some(value) = &binding.value {
                        collect_call_routes(program, module, owner, value, routes);
                    }
                }
                Item::PatternBinding(binding) => {
                    collect_call_routes(program, module, owner, &binding.value, routes);
                }
                Item::Assignment(assignment) => {
                    collect_call_routes(program, module, owner, &assignment.target, routes);
                    collect_call_routes(program, module, owner, &assignment.value, routes);
                }
                Item::Return(item) => {
                    collect_call_routes(program, module, owner, &item.value, routes);
                }
                Item::Break(item) => {
                    if let Some(value) = &item.value {
                        collect_call_routes(program, module, owner, value, routes);
                    }
                }
                Item::Continue(_) => {}
                Item::Expression(expression) => {
                    collect_call_routes(program, module, owner, expression, routes);
                }
                _ => {}
            }
        }
    }

    fn callable_value_fixture() -> &'static str {
        concat!(
            "use std.cinterop.*\n",
            "use std.slice.Slice\n",
            "use std.fmt.Formatter\n",
            "extern \"c\" { external_identity: I32 -> I32 }\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def generic_identity: <T where Copy T> T -> T = value => value\n",
            "def declared: I32 -> I32 = value => value\n",
            "type TestBox = ctor (value: I32)\n",
            "def make_borrower: String -> () -> Slice U8 = value => () => String.bytes value\n",
            "type MyString = ctor String\n",
            "impl !Copy MyString {}\n",
            "companion MyString {\n",
            "  pub def concat = a: MyString => b: MyString => MyString (a.* + b.*)\n",
            "}\n",
            "def cell_capture: () -> () -> I32 = () => {\n",
            "  let mut count: I32 = 0\n",
            "  () => { count = count + 1; count }\n",
            "}\n",
            "def recursive: I32 -> I32 = value => {\n",
            "  let self_ref = recursive\n",
            "  self_ref value\n",
            "}\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => {\n",
            "  let method: T -> Bool = test_show\n",
            "  method value\n",
            "}\n",
            "def structural_debug: () -> () = () => {\n",
            "  let debug: ((I32, I32), mut Formatter) -> () = Debug.fmt\n",
            "  ()\n",
            "}\n",
            "def value_samples: () -> I32 = () => {\n",
            "  let anonymous = (value: I32) => value\n",
            "  let named = declared\n",
            "  let generic: I32 -> I32 = generic_identity\n",
            "  let built = TestBox\n",
            "  let external = external_identity\n",
            "  let maker = make_borrower\n",
            "  let cell = cell_capture\n",
            "  let recursive_value = recursive\n",
            "  let method = test_show\n",
            "  anonymous 1\n",
            "}\n",
        )
    }

    #[test]
    fn callable_value_routes_cover_every_construction_route() {
        let mut routes = HashSet::new();
        for route in CallableValueRoute::ALL {
            assert!(routes.insert(route), "route {route:?} appears twice");
        }
        assert_eq!(routes.len(), 8);

        let module = checked_program(callable_value_fixture());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let categories = program
            .callable_values
            .iter()
            .map(|(_, value)| value.target.category())
            .collect::<HashSet<_>>();
        for expected in [
            LoweredCallableCategory::DirectKnownFunction,
            LoweredCallableCategory::ExternalFunction,
            LoweredCallableCategory::Constructor,
            LoweredCallableCategory::TraitImplementation,
            LoweredCallableCategory::StructuralTraitMethod,
        ] {
            assert!(
                categories.contains(&expected),
                "the fixture should construct a {expected:?} callable value"
            );
        }
    }

    #[test]
    fn function_values_record_targets_adapters_and_closure_plans() {
        let module = checked_program(callable_value_fixture());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let values = program
            .callable_values
            .iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        let named_function = |suffix: &str, value: &&LoweredCallableValue| match &value.target {
            LoweredCallableTarget::DirectFunction { function, .. } => program
                .functions
                .get(*function)
                .is_some_and(|function| function.name.ends_with(suffix)),
            _ => false,
        };

        assert!(values.iter().any(|value| matches!(
            &value.target,
            LoweredCallableTarget::Constructor { .. }
        ) && value.adapter
            == LoweredCallableAdapter::Constructor));
        assert!(values.iter().any(|value| matches!(
            &value.target,
            LoweredCallableTarget::ExternalFunction { .. }
        ) && value.adapter == LoweredCallableAdapter::External));
        assert!(values.iter().any(|value| matches!(
            value.evidence,
            Some(TraitEvidence::ExplicitImplementation { .. })
        ) && matches!(
            value.target,
            LoweredCallableTarget::TraitImplementation {
                function: Some(_),
                ..
            }
        )));
        assert!(values.iter().any(|value| matches!(
            value.evidence,
            Some(TraitEvidence::DeclaredBound {
                method: Some(_),
                ..
            })
        ) && matches!(
            value.target,
            LoweredCallableTarget::TraitImplementation { function: None, .. }
        )));
        assert!(values.iter().any(|value| matches!(
            value.evidence,
            Some(TraitEvidence::Structural {
                structural: StructuralTraitMethod::Debug,
                ..
            })
        ) && matches!(
            value.target,
            LoweredCallableTarget::StructuralTraitMethod { .. }
        )));

        assert!(values.iter().any(|value| {
            named_function(".declared", value)
                && value
                    .closure
                    .as_ref()
                    .is_some_and(|closure| closure.environment == LoweredClosureEnvironment::Stored)
        }));
        assert!(values.iter().any(|value| {
            named_function(".recursive", value)
                && value
                    .closure
                    .as_ref()
                    .is_some_and(|closure| closure.environment == LoweredClosureEnvironment::Stored)
        }));
        assert!(values.iter().any(|value| {
            named_function(".anonymous", value)
                && value
                    .closure
                    .as_ref()
                    .is_some_and(|closure| closure.environment == LoweredClosureEnvironment::Fresh)
        }));
        assert!(
            values
                .iter()
                .any(|value| named_function(".generic_identity", value)
                    && value.closure.as_ref().is_some_and(|closure| {
                        closure.environment == LoweredClosureEnvironment::Fresh
                            && !closure.substitutions.types.is_empty()
                    }))
        );

        let mut saw_by_value = false;
        let mut saw_borrowed = false;
        let mut saw_shared_cell = false;
        for value in &values {
            let Some(closure) = &value.closure else {
                continue;
            };
            let catalog = program
                .functions
                .get(closure.function)
                .expect("closure function");
            assert_eq!(catalog.captures.len(), closure.captures.len());
            for (catalog_capture, capture) in catalog.captures.iter().zip(&closure.captures) {
                assert_eq!(catalog_capture.symbol, capture.capture.symbol);
                match capture.access {
                    LoweredCaptureAccess::ByValue => {
                        assert!(!capture.capture.borrowed && !capture.capture.requires_cell);
                        saw_by_value = true;
                    }
                    LoweredCaptureAccess::Borrowed => {
                        assert!(capture.capture.borrowed && !capture.capture.requires_cell);
                        saw_borrowed = true;
                    }
                    LoweredCaptureAccess::SharedCell => {
                        assert!(capture.capture.requires_cell);
                        saw_shared_cell = true;
                    }
                }
                assert_eq!(
                    capture.owns_value,
                    capture.access == LoweredCaptureAccess::ByValue && !capture.capture.non_owning
                );
            }
        }
        assert!(
            saw_by_value && saw_borrowed && saw_shared_cell,
            "the fixture should exercise every capture access"
        );

        let mut second = LoweredProgram::default();
        assert!(second.snapshot(&module).is_empty());
        assert!(second.validate().is_empty());
        assert_eq!(
            normalized_program_snapshot(&program),
            normalized_program_snapshot(&second)
        );
    }

    fn call_fixture() -> &'static str {
        concat!(
            "use std.cinterop.*\n",
            "use std.io.IO\n",
            "extern \"c\" {\n",
            "  external_identity: I32 -> I32\n",
            "  external_cstr: CString -> I32\n",
            "}\n",
            "type MoveOnly = ctor String\n",
            "impl !Copy MoveOnly {}\n",
            "def generic_identity: <T where Copy T> T -> T = value => value\n",
            "def declared: I32 -> I32 = value => value\n",
            "def mutate = (mut target: I32) => { target = 1 }\n",
            "def take: MoveOnly -> I32 = value => 1\n",
            "def generic_recursive: <T where Copy T> T -> T = value => generic_recursive value\n",
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "def with_io: () ->{IO} () = () => ()\n",
            "def call_samples: () ->{state} I32 = () => {\n",
            "  let closure = (value: I32) => value\n",
            "  let indirect: I32 = closure (1)\n",
            "  let direct: I32 = generic_identity 1\n",
            "  let declared_result: I32 = declared (1)\n",
            "  let external_result: I32 = external_identity (1)\n",
            "  let c_string_result: I32 = external_cstr (c_string \"x\")\n",
            "  let mut number: I32 = 0\n",
            "  mutate number\n",
            "  mutate (1 + 1)\n",
            "  let value = MoveOnly \"x\"\n",
            "  let borrowed: I32 = take value\n",
            "  let temporary: I32 = take (MoveOnly \"y\")\n",
            "  let recursed: I32 = generic_recursive 1\n",
            "  let evaluated: I32 = evaluate { number = number + 1; number }\n",
            "  indirect\n",
            "}\n",
            "def call_io: () ->{IO} I32 = () => {\n",
            "  with_io ()\n",
            "  0\n",
            "}\n",
        )
    }

    #[test]
    fn ordinary_direct_indirect_external_and_intrinsic_calls_lower() {
        let module = checked_program(call_fixture());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let calls = program
            .calls
            .iter()
            .map(|(_, call)| call)
            .collect::<Vec<_>>();
        assert!(!calls.is_empty());
        let categories = calls
            .iter()
            .map(|call| call.target.category())
            .collect::<HashSet<_>>();
        for expected in [
            LoweredCallableCategory::DirectKnownFunction,
            LoweredCallableCategory::IndirectClosure,
            LoweredCallableCategory::ExternalFunction,
            LoweredCallableCategory::Intrinsic,
        ] {
            assert!(
                categories.contains(&expected),
                "the fixture should lower a {expected:?} call"
            );
        }

        assert!(calls.iter().any(|call| matches!(
            &call.target,
            LoweredCallableTarget::DirectFunction {
                environment: LoweredCallEnvironment::Current,
                ..
            }
        )));
        for call in &calls {
            if let LoweredCallableTarget::IndirectClosure { callee } = call.target {
                assert_eq!(call.callee, Some(callee));
                assert!(matches!(
                    call.steps.first(),
                    Some(LoweredCallStep::Callee { expression }) if *expression == callee
                ));
            }
        }
        assert!(calls.iter().any(|call| matches!(
            call.target,
            LoweredCallableTarget::ExternalFunction { .. }
        ) && call.arguments.iter().any(|argument| {
            argument.drops_after_call && argument.expected == CheckedType::CString
        })));
        assert!(
            calls
                .iter()
                .any(|call| matches!(call.target, LoweredCallableTarget::Intrinsic { .. }))
        );
    }

    #[test]
    fn call_arguments_record_pass_modes_temporaries_and_steps() {
        let module = checked_program(call_fixture());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let calls = program
            .calls
            .iter()
            .map(|(_, call)| call)
            .collect::<Vec<_>>();
        assert!(
            calls
                .iter()
                .any(|call| call.arguments.iter().any(|argument| {
                    argument.pass_mode == LoweredArgumentPassMode::MutablePlace
                        && argument.place.is_some()
                        && !argument.temporary
                }))
        );
        assert!(
            calls
                .iter()
                .any(|call| call.arguments.iter().any(|argument| {
                    argument.pass_mode == LoweredArgumentPassMode::MutablePlace
                        && argument.place.is_none()
                        && argument.temporary
                }))
        );
        assert!(
            calls
                .iter()
                .any(|call| call.arguments.iter().any(|argument| {
                    argument.pass_mode == LoweredArgumentPassMode::BorrowedPointer
                        && argument.place.is_some()
                        && !argument.temporary
                }))
        );
        assert!(
            calls
                .iter()
                .any(|call| call.arguments.iter().any(|argument| {
                    argument.pass_mode == LoweredArgumentPassMode::MaterializedTemporary
                        && argument.temporary
                        && !argument.drops_after_call
                }))
        );
        assert!(calls.iter().any(|call| {
            call.arguments
                .iter()
                .any(|argument| argument.thunk.is_some() && argument.expression.is_none())
        }));
        assert!(
            calls
                .iter()
                .any(|call| call.arguments.iter().any(|argument| {
                    argument.pass_mode == LoweredArgumentPassMode::Value
                        && argument.temporary == false
                }))
        );
        assert!(
            calls
                .iter()
                .any(|call| !call.function_type.mutations.is_empty())
        );
        assert!(calls.iter().any(|call| !call.resource_bindings.is_empty()));

        for call in &calls {
            assert!(
                matches!(call.steps.last(), Some(LoweredCallStep::Invoke)),
                "every call ends with an invocation step"
            );
            assert_eq!(
                call.steps
                    .iter()
                    .filter(|step| matches!(step, LoweredCallStep::Resource { .. }))
                    .count(),
                call.resource_bindings.len()
            );
            let mut slots = HashSet::new();
            for argument in &call.arguments {
                if let Some(slot) = argument.slot {
                    assert!(slots.insert(slot), "call argument slots are unique");
                }
                if argument.temporary {
                    assert_ne!(argument.pass_mode, LoweredArgumentPassMode::Value);
                }
            }
        }
    }

    #[test]
    fn call_resource_bindings_follow_effect_row_order_and_scope() {
        let module = checked_program(concat!(
            "type A = ctor (value: I32)\n",
            "type B = ctor (value: I32)\n",
            "def consume: () ->{A, B} I32 = () => (resource A).value + (resource B).value\n",
            "def driver: () -> I32 = () => {\n",
            "  let a = A (value: 1)\n",
            "  let b = B (value: 2)\n",
            "  with A = a { with B = b { consume () } }\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let (consume_id, _) = lowered_function(&program, "consume");
        let call = lowered_call_to(&program, &module, consume_id);
        assert_eq!(
            call.resource_bindings.len(),
            call.function_type.effects.resources.len()
        );
        let bindings = call
            .resource_bindings
            .iter()
            .map(|binding| program.resource_uses.get(*binding).expect("binding"))
            .collect::<Vec<_>>();
        assert_eq!(bindings.len(), 2);
        for (binding, expected) in bindings.iter().zip(&call.function_type.effects.resources) {
            assert_eq!(binding.kind, LoweredResourceUseKind::HiddenArgument);
            assert_eq!(binding.resource, *expected);
            assert_eq!(binding.pass_mode, LoweredArgumentPassMode::Value);
            assert!(binding.provider.is_some(), "the `with` scope supplies it");
        }
        // Both requirements are `Copy`, so each resolves to its own `with`.
        assert_ne!(bindings[0].provider, bindings[1].provider);
        let (driver_id, _) = lowered_function(&program, "driver");
        for binding in &bindings {
            let provider = program
                .resource_providers
                .get(binding.provider.expect("provider"))
                .expect("provider record");
            assert_eq!(provider.owner, ExpressionOwner::Function(driver_id));
            assert_eq!(provider.kind, LoweredProviderOriginKind::Source);
        }
        // The resource steps follow every explicit argument, keep effect-row
        // order, and precede the invocation.
        assert!(matches!(call.steps.last(), Some(LoweredCallStep::Invoke)));
        let resource_steps = call
            .steps
            .iter()
            .filter_map(|step| match step {
                LoweredCallStep::Resource { resource } => Some(*resource),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(resource_steps, vec![0, 1]);
    }

    #[test]
    fn mutable_call_resource_bindings_borrow_the_provider_place() {
        let module = checked_program(concat!(
            "type Counter = ctor (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let (increment_id, _) = lowered_function(&program, "increment");
        let call = lowered_call_to(&program, &module, increment_id);
        assert_eq!(call.resource_bindings.len(), 1);
        let binding = program
            .resource_uses
            .get(call.resource_bindings[0])
            .expect("binding");
        assert_eq!(binding.kind, LoweredResourceUseKind::HiddenArgument);
        assert_eq!(binding.pass_mode, LoweredArgumentPassMode::BorrowedPointer);
        assert!(binding.indirect);
        let provider = program
            .resource_providers
            .get(binding.provider.expect("provider"))
            .expect("provider record");
        assert!(provider.borrow);
        assert_eq!(provider.storage, LoweredProviderStorage::Place);
    }

    #[test]
    fn calls_without_hidden_resource_abis_stay_unbound() {
        let module = checked_program(transition_fixture());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        for (_, call) in program.calls.iter() {
            match &call.target {
                LoweredCallableTarget::ExternalFunction { .. }
                | LoweredCallableTarget::Intrinsic { .. }
                | LoweredCallableTarget::Constructor { .. } => {
                    assert!(
                        call.resource_bindings.is_empty(),
                        "external, intrinsic, and constructor calls have no hidden resource ABI"
                    );
                }
                _ => assert_eq!(
                    call.resource_bindings.len(),
                    call.function_type.effects.resources.len(),
                    "effectful calls bind every ordered requirement"
                ),
            }
        }

        // A generic effect variable substitutes later; no provider is invented.
        let module = checked_program(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "let answer = evaluate { 42 }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        assert!(
            program
                .calls
                .iter()
                .any(|(_, call)| call.function_type.effects.variable.is_some())
        );
        for (_, call) in program.calls.iter() {
            if call.function_type.effects.variable.is_some() {
                assert!(
                    call.resource_bindings.is_empty(),
                    "an unresolved effect template keeps no concrete binding"
                );
            }
        }
    }

    #[test]
    fn borrowed_temporaries_drop_after_the_call() {
        let module = checked_program(concat!(
            "use std.cinterop.(CString, c_string)\n",
            "use std.io.print\n",
            "extern \"c\" { strlen: CString -> USize }\n",
            "def borrow: CString -> USize = value => strlen value\n",
            "def consume: move CString -> USize = move value => strlen value\n",
            "def apply: (CString -> USize) -> USize = f => f (c_string \"borrowed\")\n",
            "def apply_move: ((move CString) -> USize) -> USize = f => f (c_string \"moved\")\n",
            "let a = apply borrow\n",
            "let b = apply_move consume\n",
            "let c = strlen (CString.from_string \"extern\")\n",
            "print \"variadic\"\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        // A callback's borrowed `CString` temporary is dropped by the caller;
        // a `move` parameter's callee owns it.
        let callbacks = program
            .calls
            .iter()
            .map(|(_, call)| call)
            .filter(|call| {
                matches!(call.target, LoweredCallableTarget::IndirectClosure { .. })
                    && *call.function_type.parameter == CheckedType::CString
            })
            .collect::<Vec<_>>();
        assert_eq!(callbacks.len(), 2, "both callback calls lower");
        for call in callbacks {
            assert_eq!(
                call.arguments[0].drops_after_call,
                call.function_type.moves.is_empty(),
                "only a borrowed callback parameter leaves the temporary to the caller"
            );
        }

        // Native externs borrow every argument, including variadic slots and
        // a `CString` viewed as `CPointer CChar` (`print`'s `printf`).
        let externs = program
            .calls
            .iter()
            .map(|(_, call)| call)
            .filter(|call| matches!(call.target, LoweredCallableTarget::ExternalFunction { .. }))
            .filter(|call| {
                call.arguments.iter().any(|argument| {
                    argument.expression.is_some_and(|expression| {
                        program
                            .expressions
                            .get(expression)
                            .is_some_and(|expression| {
                                matches!(expression.kind, LoweredExpressionKind::Call(_))
                                    || expression.value_type == CheckedType::CString
                            })
                    }) && argument.place.is_none()
                })
            })
            .collect::<Vec<_>>();
        assert!(
            externs.iter().any(|call| call.arguments.len() == 2
                && call
                    .arguments
                    .iter()
                    .all(|argument| argument.drops_after_call)),
            "printf frees its format and its converted argument"
        );
        assert!(
            externs.iter().any(|call| call.arguments.len() == 1
                && call.arguments[0].drops_after_call
                && call.arguments[0].expected == CheckedType::CString),
            "strlen frees its converted temporary"
        );
    }

    #[test]
    fn call_facts_agree_with_checked_function_types() {
        let module = checked_program(call_fixture());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        for (_, call) in program.calls.iter() {
            assert_eq!(
                call.resource_bindings
                    .iter()
                    .map(|binding| program
                        .resource_uses
                        .get(*binding)
                        .expect("resource binding")
                        .resource
                        .clone())
                    .collect::<Vec<_>>(),
                call.function_type.effects.resources
            );
            assert_eq!(call.result_type, *call.function_type.result);
            assert!(
                !matches!(
                    call.target,
                    LoweredCallableTarget::Intrinsic { .. }
                        | LoweredCallableTarget::Constructor { .. }
                ) || call
                    .arguments
                    .iter()
                    .all(|argument| !argument.drops_after_call),
                "intrinsics and constructors own their arguments"
            );
            for symbol in &call.initialization_checks {
                assert!(program.symbols.get(*symbol).is_some());
            }
        }

        let mut second = LoweredProgram::default();
        assert!(second.snapshot(&module).is_empty());
        assert!(second.validate().is_empty());
        assert_eq!(
            normalized_program_snapshot(&program),
            normalized_program_snapshot(&second)
        );
    }

    fn step4_fixture() -> &'static str {
        concat!(
            "use std.core.reference.(Ref)\n",
            "let pair_add: x: I32 * y: I32 -> I32 = x * y => x + y\n",
            "def defaulted: (String, x: I32 = 0, y: I32 = 0) -> I32 = (value, x, y) => x + y\n",
            "def juxtaposed_samples: () -> I32 = () => {\n",
            "  let chained: I32 = pair_add 1 2\n",
            "  let mut reference: Ref I32 = Ref 0\n",
            "  let replaced: I32 = Ref.replace reference 1\n",
            "  chained\n",
            "}\n",
            "let via_value = defaulted\n",
            "let indirect_result = via_value \"a\"\n",
            "def default_samples: () -> I32 = () => {\n",
            "  let plain: I32 = defaulted (\"a\")\n",
            "  let designated: I32 = defaulted (\"b\", .y: 5)\n",
            "  let explicit: I32 = defaulted (\"c\", 1, 2)\n",
            "  let pair = (x: 3, y: 4)\n",
            "  let spread: I32 = defaulted (\"d\", ...pair)\n",
            "  plain\n",
            "}\n",
        )
    }

    #[test]
    fn juxtaposed_calls_consume_inner_chain_nodes_once() {
        let module = checked_program(step4_fixture());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let samples = module
            .functions()
            .iter()
            .find(|function| function.name.contains("juxtaposed_samples"))
            .expect("juxtaposed_samples function");
        let mut chains = Vec::new();
        collect_calls(&samples.body, &mut |call| {
            if module.juxtaposed_call_plan(call.syntax.id).is_some()
                && matches!(call.callee.as_ref(), Expression::Call(_))
            {
                chains.push(call.clone());
            }
        });
        assert_eq!(chains.len(), 2, "the fixture has two multi-layer chains");

        let mut indirect_chain = None;
        let mut intrinsic_chain = None;
        for chain in &chains {
            let lowered = program
                .calls
                .iter()
                .find(|(_, call)| call.origin.syntax == chain.syntax.id)
                .map(|(_, call)| call)
                .expect("the outer chain call");
            match lowered.target {
                LoweredCallableTarget::IndirectClosure { .. } => {
                    indirect_chain = Some((chain, lowered))
                }
                LoweredCallableTarget::Intrinsic {
                    intrinsic: crate::IntrinsicFunction::RefReplace,
                    ..
                } => intrinsic_chain = Some(lowered),
                _ => {}
            }
            let Expression::Call(inner) = chain.callee.as_ref() else {
                panic!("a completed juxtaposed chain has a call callee");
            };
            assert!(program.consumed_calls.contains(&inner.syntax.id));
            assert!(
                !program
                    .expressions
                    .iter()
                    .any(|(_, expression)| expression.origin.syntax == inner.syntax.id),
                "the consumed inner call is not lowered on its own"
            );
        }

        let (chain, lowered) = indirect_chain.expect("the indirect juxtaposed chain");
        assert!(matches!(chain.callee.as_ref(), Expression::Call(_)));
        assert!(lowered.callee.is_some());
        assert_eq!(lowered.arguments.len(), 2);
        assert!(matches!(
            lowered.steps.first(),
            Some(LoweredCallStep::Callee { .. })
        ));
        assert!(matches!(
            lowered.steps.get(1),
            Some(LoweredCallStep::ProductElement { slot: 0, .. })
        ));
        assert!(matches!(
            lowered.steps.get(2),
            Some(LoweredCallStep::ProductElement { slot: 1, .. })
        ));

        let replace_call = intrinsic_chain.expect("the juxtaposed intrinsic call");
        assert!(replace_call.callee.is_none());
        assert_eq!(replace_call.arguments.len(), 2);
        assert_eq!(
            replace_call.arguments[0].pass_mode,
            LoweredArgumentPassMode::MutablePlace
        );
        assert!(matches!(
            replace_call.steps.first(),
            Some(LoweredCallStep::ProductElement { slot: 0, .. })
        ));
    }

    #[test]
    fn call_arguments_lower_defaults_spreads_and_designators() {
        let module = checked_program(step4_fixture());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        // A non-product argument checked against a defaulted product
        // parameter: slot 0 explicit, defaults fill the rest in slot order.
        assert!(program.calls.iter().any(|(_, call)| {
            matches!(
                call.steps.as_slice(),
                [
                    LoweredCallStep::Callee { .. },
                    LoweredCallStep::ProductElement { slot: 0, .. },
                    LoweredCallStep::Default { slot: 1, .. },
                    LoweredCallStep::Default { slot: 2, .. },
                    LoweredCallStep::Invoke
                ]
            )
        }));
        // A designated element fills its named slot; its default is
        // evaluated afterwards in slot order.
        assert!(program.calls.iter().any(|(_, call)| {
            matches!(
                call.steps.as_slice(),
                [
                    LoweredCallStep::Callee { .. },
                    LoweredCallStep::ProductElement { slot: 0, .. },
                    LoweredCallStep::ProductElement { slot: 2, .. },
                    LoweredCallStep::Default { slot: 1, .. },
                    LoweredCallStep::Invoke
                ]
            )
        }));
        // A positional spread expands to ordered slot mappings.
        assert!(program.calls.iter().any(|(_, call)| {
            call.steps.iter().any(|step| {
                matches!(
                    step,
                    LoweredCallStep::ProductSpread { mappings, .. }
                        if mappings
                            == &vec![
                                LoweredSpreadMapping { source: 0, slot: 1 },
                                LoweredSpreadMapping { source: 1, slot: 2 },
                            ]
                )
            })
        }));
        // Every default occurrence owns a contextual occurrence key.
        for (_, call) in program.calls.iter() {
            for step in &call.steps {
                if let LoweredCallStep::Default {
                    expression,
                    expected,
                    ..
                } = step
                {
                    let lowered = program.expressions.get(*expression).expect("default");
                    assert!(matches!(
                        lowered.key.context,
                        ExpressionContext::ContextualDefault { .. }
                    ));
                    assert!(types_agree(expected, &lowered.value_type));
                }
            }
        }

        let mut second = LoweredProgram::default();
        assert!(second.snapshot(&module).is_empty());
        assert!(second.validate().is_empty());
        assert_eq!(
            normalized_program_snapshot(&program),
            normalized_program_snapshot(&second)
        );
    }

    fn collect_calls(
        expression: &Expression,
        visit: &mut impl FnMut(&staple_syntax::CallExpression),
    ) {
        if let Expression::Call(call) = expression {
            visit(call);
            collect_calls(&call.callee, visit);
            collect_calls(&call.argument, visit);
            return;
        }
        match expression {
            Expression::Function(function) => collect_calls(&function.body, visit),
            Expression::Satisfies(satisfies) => collect_calls(&satisfies.value, visit),
            Expression::Match(match_) => {
                collect_calls(&match_.subject, visit);
                for arm in &match_.arms {
                    collect_calls(&arm.body, visit);
                }
            }
            Expression::Loop(loop_) => collect_block_calls(&loop_.body, visit),
            Expression::Coro(coro) => collect_block_calls(&coro.body, visit),
            Expression::Await(await_) => collect_calls(&await_.operand, visit),
            Expression::With(with) => {
                collect_calls(&with.value, visit);
                collect_block_calls(&with.body, visit);
            }
            Expression::Block(block) => collect_block_calls(block, visit),
            Expression::Product(product) => {
                for element in &product.elements {
                    collect_calls(&element.value, visit);
                }
            }
            Expression::RepeatedProduct(repeated) => collect_calls(&repeated.value, visit),
            Expression::Call(_) => unreachable!("calls handled above"),
            Expression::Access(access) => collect_calls(&access.value, visit),
            Expression::Index(index) => {
                collect_calls(&index.value, visit);
                collect_calls(&index.index, visit);
            }
            Expression::Unary(unary) => collect_calls(&unary.operand, visit),
            Expression::Binary(binary) => {
                collect_calls(&binary.left, visit);
                collect_calls(&binary.right, visit);
            }
            Expression::Logical(logical) => {
                collect_calls(&logical.left, visit);
                collect_calls(&logical.right, visit);
            }
            Expression::StringTemplate(template) => {
                for part in &template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        collect_calls(&interpolation.expression, visit);
                    }
                }
            }
            Expression::Resource(_)
            | Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn collect_block_calls(
        block: &staple_syntax::BlockExpression,
        visit: &mut impl FnMut(&staple_syntax::CallExpression),
    ) {
        for item in &block.items {
            match item {
                Item::Binding(binding) => {
                    if let Some(value) = &binding.value {
                        collect_calls(value, visit);
                    }
                }
                Item::PatternBinding(binding) => collect_calls(&binding.value, visit),
                Item::Assignment(assignment) => {
                    collect_calls(&assignment.target, visit);
                    collect_calls(&assignment.value, visit);
                }
                Item::Return(item) => collect_calls(&item.value, visit),
                Item::Break(item) => {
                    if let Some(value) = &item.value {
                        collect_calls(value, visit);
                    }
                }
                Item::Expression(expression) => collect_calls(expression, visit),
                _ => {}
            }
        }
    }

    #[test]
    fn constructor_calls_and_values_record_explicit_targets() {
        let module = checked_program(concat!(
            "use std.core.reference.(Ref)\n",
            "type TestBox = ctor (value: I32)\n",
            "type TestEnabled\n",
            "let enabled: TestEnabled = TestEnabled\n",
            "let maker = TestBox\n",
            "let boxed: TestBox = TestBox (value: 1)\n",
            "let single_ref: Ref I32 = Ref 0\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let constructor_calls = program
            .calls
            .iter()
            .filter_map(|(_, call)| match &call.target {
                LoweredCallableTarget::Constructor { type_id, .. } => Some((*type_id, call)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let constructor_names = constructor_calls
            .iter()
            .map(|(type_id, _)| {
                program
                    .types
                    .get(*type_id)
                    .map(|metadata| metadata.name.as_str())
                    .unwrap_or("<unknown>")
            })
            .collect::<Vec<_>>();
        assert!(constructor_names.contains(&"TestBox"));
        assert!(constructor_names.contains(&"Ref"));

        for (_, call) in &constructor_calls {
            assert!(call.callee.is_none());
            assert!(matches!(call.steps.last(), Some(LoweredCallStep::Invoke)));
        }
        let box_call = constructor_calls
            .iter()
            .find(|(type_id, _)| {
                program
                    .types
                    .get(*type_id)
                    .map(|metadata| metadata.name.as_str())
                    == Some("TestBox")
            })
            .map(|(_, call)| *call)
            .expect("TestBox construction");
        assert_eq!(box_call.arguments.len(), 1);
        assert_eq!(
            box_call.arguments[0].pass_mode,
            LoweredArgumentPassMode::Value
        );
        let ref_call = constructor_calls
            .iter()
            .find(|(type_id, _)| {
                program
                    .types
                    .get(*type_id)
                    .map(|metadata| metadata.name.as_str())
                    == Some("Ref")
            })
            .map(|(_, call)| *call)
            .expect("Ref construction");
        assert!(matches!(
            ref_call.target,
            LoweredCallableTarget::Constructor {
                recursive: Some(RecursiveConstruction::ManagedReference),
                ..
            }
        ));

        // Constructor values keep their adapter; singleton values stay values.
        assert!(program.callable_values.iter().any(|(_, value)| {
            matches!(value.target, LoweredCallableTarget::Constructor { .. })
                && value.adapter == LoweredCallableAdapter::Constructor
        }));
        let singleton_type = program
            .expressions
            .iter()
            .find_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Name(name) => name.singleton,
                _ => None,
            })
            .expect("a singleton name");
        assert!(
            !constructor_calls
                .iter()
                .any(|(type_id, _)| *type_id == singleton_type),
            "singletons are values, not constructor calls"
        );
    }

    #[test]
    fn trait_calls_index_mutation_and_interpolations_carry_evidence() {
        let module = checked_program(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
            "def evidence_samples: () -> Bool = () => {\n",
            "  let arithmetic: I32 = 1 + 2\n",
            "  let pair = (1, 2)\n",
            "  let first: I32 = pair[0]\n",
            "  let mut mutable_pair = (1, 2)\n",
            "  mutable_pair[0] = 3\n",
            "  let answer: I32 = 42\n",
            "  let displayed = \"answer=${answer}\"\n",
            "  let debugged = \"${pair:?}\"\n",
            "  let bound = show_bound 1\n",
            "  True\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        // An explicit trait call records its selected implementation.
        assert!(program.calls.iter().any(|(_, call)| matches!(
            call.evidence,
            Some(TraitEvidence::ExplicitImplementation { .. })
        )));
        // A bound-dependent call keeps its declared prerequisites.
        let bound = program
            .calls
            .iter()
            .find_map(|(_, call)| match &call.evidence {
                Some(TraitEvidence::DeclaredBound {
                    method: Some(_),
                    prerequisites,
                    ..
                }) => Some(prerequisites),
                _ => None,
            })
            .expect("a declared-bound trait call");
        assert!(
            bound.iter().any(|bound| {
                program
                    .traits
                    .get(bound.trait_id)
                    .map(|trait_| trait_.name.as_str())
                    == Some("TestShow")
            }),
            "the declared TestShow bound is retained"
        );

        // Index reads carry structural evidence.
        let index_evidence = program
            .expressions
            .iter()
            .find_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Index(index) => Some(&index.evidence),
                _ => None,
            })
            .expect("an index read");
        assert!(matches!(
            index_evidence,
            TraitEvidence::Structural {
                structural: StructuralTraitMethod::Index,
                ..
            }
        ));

        // Indexed mutations carry structural evidence too.
        let assignment_evidence = program
            .items
            .iter()
            .find_map(|(_, item)| match &item.kind {
                LoweredItemKind::Assignment(assignment) => assignment.evidence.as_ref(),
                _ => None,
            })
            .expect("an indexed assignment");
        assert!(matches!(
            assignment_evidence,
            TraitEvidence::Structural {
                structural: StructuralTraitMethod::MutateIndex,
                ..
            }
        ));

        // Interpolations carry their formatting evidence: explicit for
        // `Display I32`, structural for a product's `Debug`.
        let interpolation_evidence = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::StringTemplate(template) => Some(template.parts.iter()),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                LoweredStringTemplatePart::Interpolation(interpolation) => {
                    Some(&interpolation.evidence)
                }
                LoweredStringTemplatePart::Literal(_) => None,
            })
            .collect::<Vec<_>>();
        assert!(
            interpolation_evidence
                .iter()
                .any(|evidence| matches!(evidence, TraitEvidence::ExplicitImplementation { .. }))
        );
        assert!(interpolation_evidence.iter().any(|evidence| matches!(
            evidence,
            TraitEvidence::Structural {
                structural: StructuralTraitMethod::Debug,
                ..
            }
        )));
    }

    fn transition_fixture() -> &'static str {
        concat!(
            "use std.cinterop.*\n",
            "use std.core.reference.(Ref)\n",
            "use std.io.IO\n",
            "extern \"c\" { external_identity: I32 -> I32 }\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def generic_identity: <T where Copy T> T -> T = value => value\n",
            "let pair_add: x: I32 * y: I32 -> I32 = x * y => x + y\n",
            "def trait_sample: () -> Bool = () => test_show 1\n",
            "def transition_samples: () -> I32 = () => {\n",
            "  let closure = (value: I32) => value\n",
            "  let indirect: I32 = closure (1)\n",
            "  let direct: I32 = generic_identity 1\n",
            "  let external_result: I32 = external_identity (1)\n",
            "  let chained: I32 = pair_add 1 2\n",
            "  let mut reference: Ref I32 = Ref 0\n",
            "  let replaced: I32 = Ref.replace reference 1\n",
            "  let arithmetic: I32 = 1 + 2\n",
            "  indirect\n",
            "}\n",
            "def transition_io: () ->{IO} I32 = () => {\n",
            "  std.io.println \"transition\"\n",
            "  0\n",
            "}\n",
        )
    }

    #[test]
    fn lowered_calls_agree_with_checked_plans_and_signatures() {
        let module = checked_program(transition_fixture());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        for (_, call) in program.calls.iter() {
            assert_eq!(
                call.resource_bindings
                    .iter()
                    .map(|binding| program
                        .resource_uses
                        .get(*binding)
                        .expect("resource binding")
                        .resource
                        .clone())
                    .collect::<Vec<_>>(),
                call.function_type.effects.resources
            );
            assert_eq!(call.result_type, *call.function_type.result);
            match &call.target {
                LoweredCallableTarget::DirectFunction { function, .. } => {
                    let template = module
                        .type_of_function(*function)
                        .expect("checked function template");
                    assert!(
                        template == &call.function_type
                            || contains_type_parameter(&CheckedType::Function(template.clone()))
                    );
                }
                LoweredCallableTarget::IndirectClosure { callee } => {
                    let callee_expression =
                        program.expressions.get(*callee).expect("lowered callee");
                    assert_eq!(
                        module.type_of_expression(callee_expression.key.syntax),
                        Some(&CheckedType::Function(call.function_type.clone()))
                    );
                }
                LoweredCallableTarget::ExternalFunction { symbol } => {
                    assert!(module.resolved().is_external_symbol(*symbol));
                }
                LoweredCallableTarget::Intrinsic { symbol, intrinsic } => {
                    assert_eq!(
                        module.resolved().intrinsic_function(*symbol),
                        Some(*intrinsic)
                    );
                }
                LoweredCallableTarget::Constructor {
                    symbol, type_id, ..
                } => {
                    assert_eq!(module.resolved().constructor_type(*symbol), Some(*type_id));
                }
                LoweredCallableTarget::TraitImplementation { .. }
                | LoweredCallableTarget::StructuralTraitMethod { .. } => {
                    let evidence = call.evidence.as_ref().expect("trait evidence");
                    let (trait_id, method) = match &call.target {
                        LoweredCallableTarget::TraitImplementation {
                            trait_id, method, ..
                        }
                        | LoweredCallableTarget::StructuralTraitMethod {
                            trait_id, method, ..
                        } => (*trait_id, Some(*method)),
                        _ => unreachable!(),
                    };
                    assert!(evidence_matches(evidence, trait_id, method));
                }
            }
            if let Some(plan) = module.juxtaposed_call_plan(call.origin.syntax) {
                assert_eq!(plan.function, call.function_type);
                assert_eq!(plan.arguments.len(), call.arguments.len());
            }
        }

        // Closure captures follow the resolved catalog order and ownership
        // facts.
        for (_, value) in program.callable_values.iter() {
            let Some(closure) = &value.closure else {
                continue;
            };
            let function = module
                .function_by_id(closure.function)
                .expect("resolved closure function");
            assert_eq!(
                closure
                    .captures
                    .iter()
                    .map(|capture| capture.capture.symbol)
                    .collect::<Vec<_>>(),
                function.captures
            );
            for capture in &closure.captures {
                assert_eq!(
                    capture.capture.borrowed,
                    module.is_borrowed_capture(closure.function, capture.capture.symbol)
                );
                assert_eq!(
                    capture.capture.requires_cell,
                    capture_requires_cell(&module, capture.capture.symbol)
                );
            }
        }
    }

    #[test]
    fn validation_rejects_inconsistent_calls_and_deferred_callables() {
        let module = checked_program(transition_fixture());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        // A direct target that owns a callee occurrence diagnoses.
        let indirect = program
            .calls
            .iter()
            .find(|(_, call)| matches!(call.target, LoweredCallableTarget::IndirectClosure { .. }))
            .map(|(id, call)| (id, call.target.clone(), call.callee))
            .expect("an indirect call");
        if let Some(entry) = program.calls.get_mut(indirect.0) {
            entry.target = LoweredCallableTarget::DirectFunction {
                function: FunctionId(0),
                environment: LoweredCallEnvironment::None,
            };
        }
        assert!(program.validate().iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("direct call target owns a callee occurrence")
                || diagnostic
                    .message
                    .contains("callable target has dangling function reference")
        }));

        // Missing resources diagnose against the checked effect row.
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let resource_call = program
            .calls
            .iter()
            .find(|(_, call)| !call.resource_bindings.is_empty())
            .map(|(id, _)| id)
            .expect("a resource-bearing call");
        if let Some(entry) = program.calls.get_mut(resource_call) {
            entry.resource_bindings.clear();
        }
        assert!(program.validate().iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("call resources disagree with its checked effect row")
        }));

        // A remaining callable deferral diagnoses.
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        program.expressions.push(LoweredExpression {
            key: ExpressionKey {
                syntax: SyntaxId(999_999),
                owner: ExpressionOwner::Module(ModuleId(0)),
                context: ExpressionContext::Primary,
            },
            origin: Origin {
                syntax: SyntaxId(999_999),
                span: Span::Compiler,
            },
            value_type: CheckedType::I32,
            effects: CheckedEffectSet::default(),
            coercion: None,
            coercion_plan: None,
            moved_symbols: Vec::new(),
            kind: LoweredExpressionKind::Deferred(DeferredExpressionFamily::Callable),
        });
        assert!(program.validate().iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("callable expression was not lowered")
        }));
    }

    #[test]
    fn validation_requires_complete_ordered_call_steps() {
        let module = checked_program(transition_fixture());
        let mut baseline = LoweredProgram::default();
        assert!(baseline.snapshot(&module).is_empty());
        assert!(baseline.validate().is_empty());
        let call_id = baseline.calls.iter().next().expect("a call").0;

        let mut program = baseline.clone();
        program.calls.get_mut(call_id).unwrap().steps.clear();
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("no invocation step"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("evaluated 0 times"))
        );

        let mut program = baseline.clone();
        program
            .calls
            .get_mut(call_id)
            .unwrap()
            .steps
            .push(LoweredCallStep::Invoke);
        assert!(program.validate().iter().any(|d| {
            d.message
                .contains("invocation must occur exactly once as the final step")
        }));

        let resource_call = baseline
            .calls
            .iter()
            .find(|(_, call)| !call.resource_bindings.is_empty());
        let (resource_id, _) = resource_call.expect("a resource-bearing call");
        let mut program = baseline;
        program
            .calls
            .get_mut(resource_id)
            .unwrap()
            .steps
            .retain(|step| !matches!(step, LoweredCallStep::Resource { .. }));
        assert!(
            program
                .validate()
                .iter()
                .any(|d| { d.message.contains("resource steps are missing") })
        );
    }

    #[test]
    fn c_string_primitive_normalization_reuses_the_decoded_payload() {
        let program = LoweredProgram::default();
        let bytes = program
            .lower_c_string_literal("\"ok\"", Span::Compiler)
            .expect("decodable literal");
        assert_eq!(bytes.bytes, b"ok\0");
        let diagnostic = program
            .lower_c_string_literal("\"a\\0b\"", Span::Compiler)
            .expect_err("interior NUL");
        assert!(diagnostic.message.contains("interior NUL"));
    }

    #[test]
    fn occurrence_keys_deduplicate_ordinary_expressions_and_blocks() {
        let module = checked_program("def block_body = () => { let value: I32 = 1; value }\n");
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == "block_body")
            .expect("block_body function");
        let Expression::Block(block) = &function.body else {
            panic!("expected a block body");
        };
        let owner = ExpressionOwner::Function(function.id);
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let lowered_blocks = program.blocks.iter().count();
        let lowered_expressions = program.expressions.iter().count();

        let first = program
            .lower_block(&module, owner, ExpressionContext::Primary, block)
            .expect("block should lower");
        let second = program
            .lower_block(&module, owner, ExpressionContext::Primary, block)
            .expect("block should lower again");
        assert_eq!(first, second);
        assert_eq!(program.blocks.iter().count(), lowered_blocks);
        assert_eq!(program.expressions.iter().count(), lowered_expressions);

        let first_expression = program
            .lower_expression(&module, owner, ExpressionContext::Primary, &function.body)
            .expect("block expression should lower");
        let second_expression = program
            .lower_expression(&module, owner, ExpressionContext::Primary, &function.body)
            .expect("block expression should lower again");
        assert_eq!(first_expression, second_expression);
        assert!(matches!(
            program.expressions.get(first_expression).unwrap().kind,
            LoweredExpressionKind::Block(id) if id == first
        ));
    }

    #[test]
    fn scalar_literals_decode_once_with_checked_payloads() {
        let module = checked_program(concat!(
            "use std.cinterop.*\n",
            "let small: U8 = 200\n",
            "let wide: I64 = 42\n",
            "let ratio: F32 = 1.5\n",
            "let precise: F64 = 2.25\n",
            "let text = \"hello\\n\"\n",
            "let ctext = c_string \"ok\"\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let integers = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Integer(integer) => {
                    Some((integer.value, integer.integer_type))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(integers.contains(&(200, IntegerType::U8)));
        assert!(integers.contains(&(42, IntegerType::I64)));

        let floats = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Float(float) => Some((float.value, float.float_type)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(floats.iter().any(|(value, ty)| *ty == FloatType::F32
            && (*value - f64::from(1.5_f32)).abs() < f64::EPSILON));
        assert!(floats.contains(&(2.25, FloatType::F64)));

        let strings = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::String(string) => Some(string.value.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(strings.contains(&"hello\n".to_owned()));

        let c_strings = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::CString(string) => Some(string.bytes.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(c_strings.contains(&b"ok\0".to_vec()));

        // Transition comparison: every scalar payload agrees with the checked
        // scalar type recorded for the same syntax occurrence.
        for (_, expression) in program.expressions.iter() {
            match &expression.kind {
                LoweredExpressionKind::Integer(integer) => assert_eq!(
                    module
                        .type_of_expression(expression.key.syntax)
                        .and_then(CheckedType::integer_type),
                    Some(integer.integer_type)
                ),
                LoweredExpressionKind::Float(float) => assert_eq!(
                    module
                        .type_of_expression(expression.key.syntax)
                        .and_then(CheckedType::float_type),
                    Some(float.float_type)
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn ordinary_names_record_storage_and_defer_callable_values() {
        let module = checked_program(concat!(
            "let global: I32 = 7\n",
            "let mut counter: I32 = 0\n",
            "let yes: Bool = True\n",
            "let copy: I32 = global\n",
            "let current: I32 = counter\n",
            "let truth: Bool = yes\n",
            "def identity = (value: I32) => value\n",
            "type Wrapper = ctor (value: I32)\n",
            "type MyString = ctor String\n",
            "companion MyString { pub def make = value: String => MyString (value) }\n",
            "let built = Wrapper (value: 1)\n",
            "let callable = identity\n",
            "let constructor = Wrapper\n",
            "let maker = MyString.make\n",
            "def capture = () => {\n",
            "  let mut count: I32 = 0\n",
            "  let reader = () => { count = count + 1; count }\n",
            "  reader\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let names = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Name(name) => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        let global = binding_symbol(&module, "global");
        let counter = binding_symbol(&module, "counter");
        assert!(
            names
                .iter()
                .any(|name| { name.symbol == global && !name.mutable })
        );
        assert!(
            names
                .iter()
                .any(|name| { name.symbol == counter && name.mutable })
        );
        assert!(
            names.iter().any(|name| name.singleton.is_some()),
            "singleton values keep their identity on the lowered name"
        );
        for (_, expression) in program.expressions.iter() {
            if let LoweredExpressionKind::Name(name) = &expression.kind {
                assert!(program.symbols.get(name.symbol).is_some());
                assert_eq!(
                    name.requires_initialization_check,
                    module
                        .resolved()
                        .requires_initialization_check(expression.key.syntax)
                );
            }
        }

        let callable_values = program
            .callable_values
            .iter()
            .filter(|(_, value)| {
                matches!(
                    value.target,
                    LoweredCallableTarget::Constructor { .. }
                        | LoweredCallableTarget::DirectFunction { .. }
                )
            })
            .count();
        assert!(
            callable_values >= 3,
            "function, constructor, and companion-method values lower to explicit callable values"
        );
    }

    #[test]
    fn structural_access_lowers_representation_product_slice_and_scalar() {
        let module = checked_program(concat!(
            "use std.slice.Slice\n",
            "type MyString = ctor String\n",
            "type Pair = ctor (left: I32, right: I32)\n",
            "type Scalar = ctor (value: I32)\n",
            "let pair = Pair (left: 1, right: 2)\n",
            "let named = pair.left\n",
            "let other = pair.right\n",
            "let indexed = pair.0\n",
            "let text: MyString = MyString \"x\"\n",
            "let inner = text.*\n",
            "let scalar = Scalar (value: 5)\n",
            "let shortcut = scalar.value\n",
            "def slice_field: Ref (Slice I32) -> I32 = values => values.0\n",
            "def through_ref: Ref Pair -> I32 = reference => reference.left\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let accesses = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Access(access) => Some(access),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Representation { .. }))
        );
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Product { index: 0, .. }))
        );
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Product { index: 1, .. }))
        );
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Slice { index: 0, .. }))
        );
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Scalar { .. }))
        );
        assert!(
            accesses.iter().any(|access| match &access.kind {
                LoweredAccessKind::Product { dereference, .. }
                | LoweredAccessKind::Slice { dereference, .. }
                | LoweredAccessKind::Scalar { dereference } => !dereference.is_empty(),
                LoweredAccessKind::Representation { dereference } => !dereference.is_empty(),
            }),
            "access through `Ref` records its crossed payloads"
        );

        // Transition comparison: every access agrees with `CheckedAccess`.
        for (_, expression) in program.expressions.iter() {
            let LoweredExpressionKind::Access(access) = &expression.kind else {
                continue;
            };
            let checked = module
                .access_for(expression.key.syntax)
                .expect("lowered access has checked metadata");
            match (&access.kind, checked) {
                (
                    LoweredAccessKind::Representation { dereference },
                    CheckedAccess::Representation {
                        dereference: checked,
                    },
                ) => assert_eq!(dereference, checked),
                (
                    LoweredAccessKind::Product { index, dereference },
                    CheckedAccess::Product {
                        index: checked_index,
                        dereference: checked_dereference,
                        slice: false,
                        scalar: false,
                    },
                ) => {
                    assert_eq!(index, checked_index);
                    assert_eq!(dereference, checked_dereference);
                }
                (
                    LoweredAccessKind::Slice { index, dereference },
                    CheckedAccess::Product {
                        index: checked_index,
                        dereference: checked_dereference,
                        slice: true,
                        scalar: false,
                    },
                ) => {
                    assert_eq!(index, checked_index);
                    assert_eq!(dereference, checked_dereference);
                }
                (
                    LoweredAccessKind::Scalar { dereference },
                    CheckedAccess::Product {
                        dereference: checked_dereference,
                        scalar: true,
                        ..
                    },
                ) => assert_eq!(dereference, checked_dereference),
                (kind, checked) => panic!("access mismatch: {kind:?} vs {checked:?}"),
            }
        }
    }

    fn replay_product_steps(product: &LoweredProduct) -> Vec<ExpressionId> {
        let mut fields = vec![None; product.final_type.elements.len()];
        for step in &product.steps {
            match step {
                LoweredProductStep::Positional { expression, slot } => {
                    fields[*slot] = Some(*expression)
                }
                LoweredProductStep::Designated {
                    expression, slot, ..
                } => fields[*slot] = Some(*expression),
                LoweredProductStep::PositionalSpread {
                    expression,
                    mappings,
                } => {
                    for mapping in mappings {
                        fields[mapping.slot] = Some(*expression);
                    }
                }
                LoweredProductStep::NamedSpread {
                    expression,
                    mappings,
                } => {
                    for mapping in mappings {
                        fields[mapping.slot] = Some(*expression);
                    }
                }
                LoweredProductStep::Default {
                    slot, expression, ..
                } => fields[*slot] = Some(*expression),
            }
        }
        fields
            .into_iter()
            .map(|field| field.expect("every slot is filled"))
            .collect()
    }

    #[test]
    fn positional_products_expand_spreads_and_defaults() {
        let module = checked_program(concat!(
            "let pair = (left: 2, right: 3)\n",
            "let expanded = (prefix: \"value\", ...pair, suffix: False)\n",
            "let point: (x: I32 = 1, y: I32 = 2) = ()\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let products = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Product(product) => Some(product),
                _ => None,
            })
            .collect::<Vec<_>>();

        let spread = products
            .iter()
            .find(|product| {
                product
                    .steps
                    .iter()
                    .any(|step| matches!(step, LoweredProductStep::PositionalSpread { .. }))
            })
            .expect("the spread product should lower");
        assert_eq!(spread.final_type.elements.len(), 4);
        assert_eq!(spread.fields.len(), 4);
        let mut kinds = spread
            .steps
            .iter()
            .map(|step| match step {
                LoweredProductStep::Positional { .. } => "positional",
                LoweredProductStep::Designated { .. } => "designated",
                LoweredProductStep::PositionalSpread { .. } => "spread",
                LoweredProductStep::NamedSpread { .. } => "named-spread",
                LoweredProductStep::Default { .. } => "default",
            })
            .collect::<Vec<_>>();
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["positional", "positional", "spread"]);
        let LoweredProductStep::PositionalSpread { mappings, .. } = spread
            .steps
            .iter()
            .find(|step| matches!(step, LoweredProductStep::PositionalSpread { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(
            mappings,
            &vec![
                LoweredSpreadMapping { source: 0, slot: 1 },
                LoweredSpreadMapping { source: 1, slot: 2 },
            ]
        );
        assert_eq!(replay_product_steps(spread), spread.fields);

        let defaulted = products
            .iter()
            .find(|product| {
                product
                    .steps
                    .iter()
                    .any(|step| matches!(step, LoweredProductStep::Default { .. }))
            })
            .expect("the default product should lower");
        assert_eq!(defaulted.final_type.elements.len(), 2);
        let defaults = defaulted
            .steps
            .iter()
            .filter_map(|step| match step {
                LoweredProductStep::Default {
                    slot,
                    expression,
                    expected,
                } => Some((*slot, *expression, expected.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(defaults.len(), 2);
        assert_eq!(defaults[0].0, 0);
        assert_eq!(defaults[1].0, 1);
        for (slot, expression, expected) in &defaults {
            assert_eq!(expected, &CheckedType::I32);
            let lowered = program.expressions.get(*expression).expect("default value");
            assert_eq!(lowered.value_type, CheckedType::I32);
            assert!(
                matches!(
                    lowered.key.context,
                    ExpressionContext::ContextualDefault { slot: lowered_slot, .. }
                        if lowered_slot == *slot
                ),
                "defaults use occurrence-aware keys"
            );
        }
        assert_ne!(
            defaults[0].1, defaults[1].1,
            "one default declaration used twice must not alias"
        );
        assert_eq!(replay_product_steps(defaulted), defaulted.fields);
    }

    #[test]
    fn designated_products_resolve_slots_and_overrides() {
        let module = checked_program(concat!(
            "let value: (I32, I32, a: I32, b: I32) = (1, 2, .b: 4, .a: 3)\n",
            "let overridden: (a: I32, b: I32) = (1, 2, .a: 10, .a: 30)\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let designated = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Product(product)
                    if product
                        .steps
                        .iter()
                        .any(|step| matches!(step, LoweredProductStep::Designated { .. })) =>
                {
                    Some(product)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(designated.len(), 2);

        let slots = |product: &LoweredProduct| {
            product
                .steps
                .iter()
                .filter_map(|step| match step {
                    LoweredProductStep::Designated { name, slot, .. } => {
                        Some((name.clone(), *slot))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            slots(designated[0]),
            vec![("b".to_owned(), 3), ("a".to_owned(), 2)]
        );
        for product in &designated {
            assert_eq!(replay_product_steps(product), product.fields);
        }

        // `(1, 2, .a: 10, .a: 30)` keeps the last designator in the final
        // layout while evaluating every source element in order.
        let overridden = &designated[1];
        assert_eq!(overridden.steps.len(), 4);
        assert_eq!(overridden.fields.len(), 2);
        let last = overridden
            .steps
            .iter()
            .rev()
            .find_map(|step| match step {
                LoweredProductStep::Designated {
                    expression, name, ..
                } if name == "a" => Some(*expression),
                _ => None,
            })
            .expect("last `a` designator");
        assert_eq!(overridden.fields[0], last);
    }

    #[test]
    fn named_spreads_expand_fields_and_override_in_source_order() {
        let module = checked_program(concat!(
            "let dimensions = (height: 600, width: 800)\n",
            "let config: (width: I32, height: I32, title: String) = (\n",
            "    ...=dimensions,\n",
            "    title: \"Staple\",\n",
            ")\n",
            "let overridden: (width: I32, height: I32) = (\n",
            "    ...=dimensions,\n",
            "    width: 900,\n",
            ")\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let named = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Product(product)
                    if product
                        .steps
                        .iter()
                        .any(|step| matches!(step, LoweredProductStep::NamedSpread { .. })) =>
                {
                    Some(product)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(named.len(), 2);

        let config = &named[0];
        assert_eq!(config.final_type.elements.len(), 3);
        let LoweredProductStep::NamedSpread { mappings, .. } = &config.steps[0] else {
            panic!("the named spread is the first step");
        };
        assert_eq!(
            mappings,
            &vec![
                LoweredNamedSpreadMapping {
                    name: "height".to_owned(),
                    source: 0,
                    slot: 1,
                },
                LoweredNamedSpreadMapping {
                    name: "width".to_owned(),
                    source: 1,
                    slot: 0,
                },
            ]
        );
        assert!(matches!(
            config.steps[1],
            LoweredProductStep::Designated { ref name, slot: 2, .. } if name == "title"
        ));
        assert_eq!(replay_product_steps(config), config.fields);

        // The later `width: 900` overrides the spread's `width` value.
        let overridden = &named[1];
        assert!(matches!(
            overridden.steps[1],
            LoweredProductStep::Designated { ref name, slot: 0, .. } if name == "width"
        ));
        assert_eq!(replay_product_steps(overridden), overridden.fields);
        assert_eq!(
            overridden.fields[0],
            match overridden.steps[1] {
                LoweredProductStep::Designated { expression, .. } => expression,
                _ => unreachable!(),
            }
        );
    }

    #[test]
    fn repeated_products_record_count_and_collapse() {
        let module = checked_program(concat!(
            "type Count = alias 3\n",
            "let repeated: (I32; 3) = (9; Count)\n",
            "let single: (I32; 1) = (9; 1)\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let repeated = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::RepeatedProduct(repeated) => Some(repeated),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(repeated.len(), 2);
        let three = repeated
            .iter()
            .find(|repeated| repeated.count == LoweredRepeatCount::Fixed(3))
            .expect("count-three repeated product");
        assert!(!three.collapsed);
        let single = repeated
            .iter()
            .find(|repeated| repeated.count == LoweredRepeatCount::Fixed(1))
            .expect("count-one repeated product");
        assert!(single.collapsed);
        for repeated in &repeated {
            assert!(program.expressions.contains(repeated.expression));
        }
    }

    #[test]
    fn generic_repeated_product_retains_symbolic_count() {
        let module = checked_program(concat!(
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T; N) = value => n => (value; N)\n",
            "let repeated: (I32; 3) = repeat 7 3\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let (symbolic_id, symbolic) = program
            .expressions
            .iter()
            .filter_map(|(id, expression)| match &expression.kind {
                LoweredExpressionKind::RepeatedProduct(repeated) => {
                    Some((id, expression, repeated))
                }
                _ => None,
            })
            .find(|(_, expression, _)| matches!(expression.value_type, CheckedType::Array { .. }))
            .map(|(id, expression, repeated)| (id, (expression, repeated)))
            .expect("generic repeated product");
        let CheckedType::Array { count, .. } = &symbolic.0.value_type else {
            unreachable!()
        };
        assert_eq!(
            symbolic.1.count,
            LoweredRepeatCount::Symbolic(count.as_ref().clone())
        );
        assert!(!symbolic.1.collapsed);

        let expression = program
            .expressions
            .values
            .get_mut(symbolic_id.index())
            .unwrap();
        let LoweredExpressionKind::RepeatedProduct(repeated) = &mut expression.kind else {
            unreachable!()
        };
        repeated.count = LoweredRepeatCount::Fixed(1);
        repeated.collapsed = true;
        assert!(program.validate().iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("fixed count disagrees with its symbolic array result type")
        }));
    }

    #[test]
    fn satisfies_lowers_as_wrapper_and_keeps_checked_coercions() {
        let module = checked_program(concat!(
            "use std.slice.Slice\n",
            "let widened = 42 satisfies I8\n",
            "let text: String = \"literal\"\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok (42)\n",
            "let sum: Ok I32 | IOError = Ok (41)\n",
            "let slice: Slice I32 = Ref 8\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (expression, satisfies) = program
            .expressions
            .iter()
            .find_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Satisfies(satisfies) => program
                    .expressions
                    .get(satisfies.value)
                    .filter(|child| child.value_type == CheckedType::I8)
                    .map(|_| (expression, satisfies)),
                _ => None,
            })
            .expect("`42 satisfies I8` lowers as a wrapper");
        assert_eq!(expression.value_type, CheckedType::I8);
        let child = program
            .expressions
            .get(satisfies.value)
            .expect("satisfies child");
        assert_eq!(child.value_type, CheckedType::I8);
        assert_eq!(
            child.value_type,
            module
                .type_of_expression(child.key.syntax)
                .cloned()
                .expect("checked child type")
        );

        // Coercions recorded on deferred call headers (sum injection and
        // `Ref` to `Slice`) are retained for lowering.
        let coercions = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| expression.coercion.clone())
            .collect::<Vec<_>>();
        assert!(coercions.iter().any(|coercion| matches!(
            &coercion.target,
            CheckedType::Sum(sum) if sum.alternatives.len() == 2
        )));
        assert!(coercions.iter().any(|coercion| matches!(
            (&coercion.source, &coercion.target),
            (CheckedType::Ref(_), CheckedType::Slice(_))
        )));
    }

    #[test]
    fn blocks_preserve_nested_results_divergence_and_drop_facts() {
        let module = checked_program(concat!(
            "type Handle = ctor I32\n",
            "impl Drop Handle { def drop = Handle value => () }\n",
            "def nested = () => {\n",
            "  let outer: I32 = { let inner: I32 = 1; inner + 2 }\n",
            "  outer\n",
            "}\n",
            "def early = () => { return 1; 0 }\n",
            "def discards = () => {\n",
            "  Handle 1\n",
            "  ()\n",
            "}\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok (42)\n",
            "def propagates = () => {\n",
            "  let Ok(value)? = read()\n",
            "  value\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        // A nested block is a reachable block expression whose items and
        // result were lowered through the same arena.
        let nested_blocks = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Block(block) => program
                    .blocks
                    .get(*block)
                    .filter(|block| block.items.len() == 1 && block.result.is_some())
                    .map(|_| ()),
                _ => None,
            })
            .count();
        assert!(nested_blocks >= 2, "function and nested blocks lower");

        let (_, early) = lowered_function(&program, "early");
        let block = body_block(&program, early);
        assert!(block.items.iter().any(|item| matches!(
            program.items.get(*item).map(|item| &item.kind),
            Some(LoweredItemKind::Return(_))
        )));
        let result = block.result.expect("unreachable tail keeps its result");
        assert!(!block.items.iter().any(|item| matches!(
            program.items.get(*item).map(|item| &item.kind),
            Some(LoweredItemKind::Expression(statement)) if statement.expression == result
        )));

        let (_, discards) = lowered_function(&program, "discards");
        let block = body_block(&program, discards);
        let discard = block
            .items
            .iter()
            .find_map(
                |item| match program.items.get(*item).map(|item| &item.kind) {
                    Some(LoweredItemKind::Expression(statement)) => Some(statement),
                    _ => None,
                },
            )
            .expect("the discarded value lowers as an expression statement");
        assert!(discard.drop_result, "owned discarded values need a drop");
        let discarded = program
            .expressions
            .get(discard.expression)
            .expect("discarded expression");
        assert!(module.type_needs_drop(&discarded.value_type));

        let (_, propagates) = lowered_function(&program, "propagates");
        let block = body_block(&program, propagates);
        let binding = block
            .items
            .iter()
            .find_map(
                |item| match program.items.get(*item).map(|item| &item.kind) {
                    Some(LoweredItemKind::PatternBinding(binding)) => Some(binding),
                    _ => None,
                },
            )
            .expect("the propagation binding lowers");
        assert!(binding.propagating);
        assert!(binding.propagation.is_some());

        // Every expression statement's discard/drop fact agrees with its
        // checked value type.
        for (_, item) in program.items.iter() {
            let LoweredItemKind::Expression(statement) = &item.kind else {
                continue;
            };
            let expression = program
                .expressions
                .get(statement.expression)
                .expect("statement expression");
            assert_eq!(
                statement.drop_result,
                module.type_needs_drop(&expression.value_type)
            );
        }
    }

    fn expression_body<'a>(program: &'a LoweredProgram, name: &str) -> &'a LoweredExpression {
        let (_, function) = lowered_function(program, name);
        let block = body_block(program, function);
        program
            .expressions
            .get(block.result.expect("function body result"))
            .expect("lowered body expression")
    }

    #[test]
    fn logical_operators_copy_checked_bool_and_operand_order() {
        let module = checked_program(concat!(
            "def both = (left: Bool, right: Bool) => left && right\n",
            "def either = (left: Bool, right: Bool) => left || right\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let both = expression_body(&program, "both");
        let LoweredExpressionKind::Logical(logical) = &both.kind else {
            panic!("`&&` lowers to a logical node");
        };
        assert_eq!(logical.operator, staple_syntax::LogicalOperator::And);
        assert!(program.expressions.contains(logical.left));
        assert!(program.expressions.contains(logical.right));
        let CheckedType::Sum(sum) = &logical.bool_type else {
            panic!("`Bool` is a sum type");
        };
        let expected_true = sum
            .alternatives
            .iter()
            .position(|alternative| {
                matches!(alternative, CheckedType::Distinct { name, .. } if name == "True")
            })
            .expect("`Bool` has a `True` alternative");
        assert_eq!(logical.true_index, expected_true);
        for operand in [logical.left, logical.right] {
            assert_eq!(
                program.expressions.get(operand).unwrap().value_type,
                logical.bool_type
            );
        }

        let either = expression_body(&program, "either");
        let LoweredExpressionKind::Logical(logical) = &either.kind else {
            panic!("`||` lowers to a logical node");
        };
        assert_eq!(logical.operator, staple_syntax::LogicalOperator::Or);
    }

    #[test]
    fn loops_record_drop_facts_depth_and_owned_exits() {
        let module = checked_program(concat!(
            "type Handle = ctor I32\n",
            "impl Drop Handle { def drop = Handle value => () }\n",
            "def select: Bool -> I32 = condition => loop {\n",
            "  match condition { True() => { break 9 }, False() => { continue } }\n",
            "}\n",
            "def nested = () => loop { loop { break () }; break 7 }\n",
            "def forever: () -> () = () => loop { continue }\n",
            "def drop_body: () -> Never = () => loop { Handle 1 }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let select = expression_body(&program, "select");
        let LoweredExpressionKind::Loop(loop_) = &select.kind else {
            panic!("`loop` lowers to a loop node");
        };
        assert_eq!(loop_.depth, 1);
        assert_eq!(loop_.result_type, CheckedType::I32);
        assert!(program.blocks.contains(loop_.body));
        let (break_depth, continue_depth) =
            program
                .items
                .iter()
                .fold(
                    (None, None),
                    |(break_depth, continue_depth), (_, item)| match &item.kind {
                        LoweredItemKind::Break(item) => (Some(item.loop_depth), continue_depth),
                        LoweredItemKind::Continue(item) => (break_depth, Some(item.loop_depth)),
                        _ => (break_depth, continue_depth),
                    },
                );
        assert_eq!(break_depth, Some(1));
        assert_eq!(continue_depth, Some(1));

        let nested = expression_body(&program, "nested");
        let LoweredExpressionKind::Loop(outer) = &nested.kind else {
            panic!("`loop` lowers to a loop node");
        };
        assert_eq!(outer.depth, 1);
        let loops = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Loop(loop_) => Some(loop_),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            loops.iter().any(|loop_| loop_.depth == 2),
            "the nested loop records depth 2"
        );
        let depths = program
            .items
            .iter()
            .filter_map(|(_, item)| match &item.kind {
                LoweredItemKind::Break(item) => Some(item.loop_depth),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(depths.contains(&1), "outer `break 7` targets depth 1");
        assert!(depths.contains(&2), "inner `break ()` targets depth 2");

        let drop_body = expression_body(&program, "drop_body");
        let LoweredExpressionKind::Loop(loop_) = &drop_body.kind else {
            panic!("`loop` lowers to a loop node");
        };
        assert!(
            loop_.drops_body_result,
            "owned loop bodies drop their result"
        );
        assert_eq!(loop_.result_type, CheckedType::Never);

        let forever = expression_body(&program, "forever");
        assert!(
            matches!(forever.kind, LoweredExpressionKind::Loop(_)),
            "`loop` lowers to a loop node"
        );
    }

    #[test]
    fn matches_lower_subject_arms_patterns_and_bound_symbols() {
        let module = checked_program(concat!(
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "def pick = (value: Ok I32 | IOError) => match value {\n",
            "  Ok payload => payload,\n",
            "  other => 0,\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let pick = expression_body(&program, "pick");
        let LoweredExpressionKind::Match(match_) = &pick.kind else {
            panic!("`match` lowers to a match node");
        };
        assert!(program.expressions.contains(match_.subject));
        assert_eq!(match_.arms.len(), 2);
        for arm in &match_.arms {
            assert!(program.patterns.contains(arm.pattern));
            assert!(program.expressions.contains(arm.body));
            for symbol in &arm.bound_symbols {
                assert!(program.symbols.get(*symbol).is_some());
            }
        }
        assert_eq!(match_.arms[0].bound_symbols.len(), 1);
        assert_eq!(match_.arms[1].bound_symbols.len(), 1);
        let subject_type = program
            .expressions
            .get(match_.subject)
            .expect("match subject")
            .value_type
            .clone();
        assert_eq!(match_.source, subject_type);
    }

    #[test]
    fn coercion_and_pattern_plans_match_checked_decisions() {
        let module = checked_program(concat!(
            "use std.slice.Slice\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "type Other = ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok (42)\n",
            "def widen: () -> Ok I32 | IOError | Other = () => read()\n",
            "let injected: Ok I32 | IOError = Ok (41)\n",
            "let slice: Slice I32 = Ref 8\n",
            "def describe = (value: String) => match value {\n",
            "  \"literal\" => 1,\n",
            "  text => 2,\n",
            "}\n",
            "def pick = (value: Ok I32 | IOError) => match value {\n",
            "  Ok payload => payload,\n",
            "  other => 0,\n",
            "}\n",
            "def invert = (flag: Bool) => match flag {\n",
            "  True => 1,\n",
            "  False => 0,\n",
            "}\n",
            "def singleton: True -> I32 = True => 0\n",
            "def borrowed: Ref I32 -> I32 = (Ref inner) => inner\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty(), "{:?}", program.validate());

        // Coercions: every recorded plan is recomputable and the variants name
        // the same alternatives `select_sum_alternative` selects.
        let widen = expression_body(&program, "widen");
        let coercion = widen.coercion.as_ref().expect("widen coercion");
        let plan = widen.coercion_plan.as_ref().expect("widen plan");
        let LoweredCoercionPlan::SumWiden { arms } = plan else {
            panic!("sum widening should lower to SumWiden: {plan:?}");
        };
        let CheckedType::Sum(source_sum) = &coercion.source else {
            panic!("widen source should be a sum");
        };
        let CheckedType::Sum(target_sum) = &coercion.target else {
            panic!("widen target should be a sum");
        };
        assert_eq!(arms.len(), source_sum.alternatives.len());
        for (index, alternative) in source_sum.alternatives.iter().enumerate() {
            let expected = select_sum_alternative(alternative, &target_sum.alternatives)
                .expect("unique widening")
                .expect("widen alternative");
            let arm = arms[index].as_ref().expect("widen arm");
            assert_eq!(arm.target, expected);
            assert_eq!(*arm.payload, LoweredCoercionPlan::Identity);
        }

        let (injected_id, injected) = program
            .expressions
            .iter()
            .find(|(_, expression)| {
                matches!(
                    (&expression.value_type, &expression.coercion_plan),
                    (
                        CheckedType::Sum(_),
                        Some(LoweredCoercionPlan::SumInject { .. })
                    )
                )
            })
            .expect("sum injection plan");
        let LoweredCoercionPlan::SumInject {
            alternative,
            payload,
        } = injected.coercion_plan.as_ref().expect("injection plan")
        else {
            unreachable!()
        };
        assert_eq!(*alternative, 0);
        assert_eq!(**payload, LoweredCoercionPlan::Identity);

        let slice_plan = program
            .expressions
            .iter()
            .find_map(|(_, expression)| match &expression.coercion_plan {
                Some(LoweredCoercionPlan::SliceRef { length }) => Some((expression, *length)),
                _ => None,
            })
            .expect("slice-ref plan");
        let coercion = slice_plan.0.coercion.as_ref().expect("slice coercion");
        assert!(matches!(coercion.source, CheckedType::Ref(_)));
        assert!(matches!(coercion.target, CheckedType::Slice(_)));
        assert_eq!(
            slice_plan.1,
            slice_ref_length(&coercion.source, &coercion.target).expect("fixed reference length")
        );

        // Patterns: the literal payload is decoded, the sum alternatives match
        // the checked positions, and the nominal identities are recorded.
        let describe = expression_body(&program, "describe");
        let LoweredExpressionKind::Match(match_) = &describe.kind else {
            panic!("describe should be a match");
        };
        let literal_id = match_.arms[0].pattern;
        let literal = lowered_pattern(&program, literal_id);
        assert_eq!(literal.test.subject, CheckedType::String);
        assert_eq!(literal.test.literal.as_deref(), Some(&b"literal"[..]));
        assert_eq!(literal.test.sum_alternative, None);
        assert_eq!(literal.test.identity, LoweredPatternIdentity::None);

        let pick = expression_body(&program, "pick");
        let LoweredExpressionKind::Match(match_) = &pick.kind else {
            panic!("pick should be a match");
        };
        let CheckedType::Sum(sum) = &match_.source else {
            panic!("pick subject should be a sum");
        };
        let ok = lowered_pattern(&program, match_.arms[0].pattern);
        let LoweredPatternKind::Nominal {
            target: Some(target),
            ..
        } = &ok.kind
        else {
            panic!("`Ok payload` should be nominal");
        };
        let expected = sum
            .alternatives
            .iter()
            .position(
                |alternative| matches!(alternative, CheckedType::Distinct { id, .. } if id == target),
            )
            .expect("Ok alternative");
        assert_eq!(ok.test.sum_alternative, Some(expected));
        assert_eq!(ok.test.identity, LoweredPatternIdentity::None);
        let catch_all = lowered_pattern(&program, match_.arms[1].pattern);
        assert_eq!(catch_all.test.sum_alternative, None);
        assert_eq!(catch_all.value_type, match_.source);

        let invert = expression_body(&program, "invert");
        let LoweredExpressionKind::Match(match_) = &invert.kind else {
            panic!("invert should be a match");
        };
        let CheckedType::Sum(bool_sum) = &match_.source else {
            panic!("Bool should be a sum");
        };
        for arm in &match_.arms {
            let pattern = lowered_pattern(&program, arm.pattern);
            let LoweredPatternKind::Binding {
                singleton: Some(singleton),
                ..
            } = &pattern.kind
            else {
                panic!("`True`/`False` should lower to singleton bindings");
            };
            assert_eq!(pattern.test.identity, LoweredPatternIdentity::Singleton);
            let expected = bool_sum
                .alternatives
                .iter()
                .position(|alternative| {
                    matches!(alternative, CheckedType::Distinct { id, .. } if id == singleton)
                })
                .expect("singleton alternative");
            assert_eq!(pattern.test.sum_alternative, Some(expected));
        }

        let (_, borrowed) = lowered_function(&program, "borrowed");
        let product = lowered_pattern(&program, borrowed.parameter_pattern);
        let LoweredPatternKind::Product { elements, .. } = &product.kind else {
            panic!("borrowed parameter should be a product");
        };
        let ref_pattern_id = elements[0];
        let ref_pattern = lowered_pattern(&program, ref_pattern_id);
        assert_eq!(ref_pattern.test.identity, LoweredPatternIdentity::Ref);
        let LoweredPatternKind::Nominal { argument, .. } = &ref_pattern.kind else {
            panic!("`Ref inner` should be nominal");
        };
        assert_eq!(
            lowered_pattern(&program, *argument).test.subject,
            CheckedType::I32
        );

        // Validation catches a corrupted plan, a wrong nested subject, and a
        // missing concrete plan.
        let mut corrupted = program.clone();
        corrupted.expressions.values[injected_id.index()].coercion_plan =
            Some(LoweredCoercionPlan::Identity);
        assert!(
            corrupted
                .validate()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("coercion plan disagrees"))
        );

        let mut corrupted = program.clone();
        corrupted.patterns.values[literal_id.index()].test.literal = Some(b"wrong".to_vec());
        assert!(
            corrupted
                .validate()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("pattern test plan disagrees"))
        );

        let mut corrupted = program.clone();
        corrupted.expressions.values[injected_id.index()].coercion_plan = None;
        assert!(
            corrupted
                .validate()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("coercion has no emission plan"))
        );

        let mut corrupted = program.clone();
        let LoweredPatternKind::Nominal { argument, .. } =
            &corrupted.patterns.values[ref_pattern_id.index()].kind
        else {
            unreachable!()
        };
        let argument = *argument;
        corrupted.patterns.values[argument.index()].test.subject = CheckedType::String;
        assert!(corrupted.validate().iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("nested pattern test plan subject disagrees")
        }));
    }

    #[test]
    fn validator_rejects_orphaned_and_misdepth_loop_exits() {
        let module = checked_program("def value: () -> () = () => { loop { break }; () }\n");
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        for entry in program.items.values.iter_mut() {
            if let LoweredItemKind::Break(item) = &mut entry.kind {
                item.loop_depth = 2;
            }
        }
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("targets loop depth 2")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        program.items.push(LoweredItem {
            origin: Origin::compiler(),
            kind: LoweredItemKind::Break(LoweredBreakItem {
                value: None,
                loop_depth: 1,
            }),
        });
        let diagnostics = program.validate();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("not owned by an enclosing lowered loop")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn index_reads_copy_checked_dispatch_recipes_and_temporaries() {
        let module = checked_program(concat!(
            "use std.slice.Slice\n",
            "type Counter = ctor I32\n",
            "impl Index Counter String I32 { def index = (counter, key) => 0 }\n",
            "impl MutateIndex Counter String I32 { def mutate_index = (mut counter, key, move value) => () }\n",
            "def make_counter = () => Counter 0\n",
            "let mut counter = Counter 0\n",
            "let read = counter[\"key\"]\n",
            "let temporary = (make_counter())[\"key\"]\n",
            "counter[\"key\"] = 1\n",
            "(make_counter())[\"key\"] = 1\n",
            "let values: (I32; 2) = (10, 20)\n",
            "let element = values[0]\n",
            "def slice_read: (Slice I32, USize) -> I32 = (values, position) => values[position]\n",
            "def ref_read: (Ref (I32; 2), USize) -> I32 = (values, position) => values[position]\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let indexes = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Index(index) => Some((expression, index)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(indexes.len() >= 5, "stdlib and test indexes both lower");

        let counter_type = module
            .declared_type_of_symbol(binding_symbol(&module, "counter"))
            .expect("counter type");
        let counter_reads = indexes
            .iter()
            .filter(|(_, index)| {
                program
                    .expressions
                    .get(index.base)
                    .is_some_and(|base| base.value_type == counter_type)
            })
            .collect::<Vec<_>>();
        assert_eq!(counter_reads.len(), 2, "direct and temporary reads lower");
        for (expression, index) in &counter_reads {
            assert!(program.expressions.contains(index.base));
            assert!(program.expressions.contains(index.index));
            assert_eq!(
                Some(index.trait_id),
                module.semantic_ids().index_trait,
                "the dispatch's owning trait is copied"
            );
            assert!(index.method_type.is_some(), "instantiated method type");
            assert!(matches!(
                index.dispatch.arguments.first(),
                Some(argument) if *argument == counter_type
            ));
            let base = program.expressions.get(index.base).unwrap();
            let position = program.expressions.get(index.index).unwrap();
            for (position, expected, actual) in [
                (0usize, &index.arguments[0], &base.value_type),
                (1, &index.arguments[1], &position.value_type),
                (2, &index.arguments[2], &expression.value_type),
            ] {
                assert!(
                    types_agree(expected, actual),
                    "dispatch argument {position} `{expected}` vs operand `{actual}`"
                );
            }
            // The read-only `Index` method declares no mutations, so no
            // operand temporary is required.
            assert!(!index.whole_temporary && !index.base_temporary && !index.index_temporary);
        }

        // A mutation of a non-place base materializes the base into a
        // temporary place; the assignment keeps its checked `MutateIndex`
        // dispatch and argument agreement.
        let temporary_targets = program
            .items
            .iter()
            .filter_map(|(_, item)| match &item.kind {
                LoweredItemKind::Assignment(assignment) => Some(assignment),
                _ => None,
            })
            .filter(|assignment| {
                matches!(
                    program.places.get(assignment.target).map(|place| &place.kind),
                    Some(LoweredPlaceKind::Indexed { base, .. })
                        if matches!(
                            program.places.get(*base).map(|place| &place.kind),
                            Some(LoweredPlaceKind::Temporary { .. })
                        )
                )
            })
            .collect::<Vec<_>>();
        assert!(
            !temporary_targets.is_empty(),
            "`(make_counter())[\"key\"] = 1` needs a temporary place"
        );
        for assignment in temporary_targets {
            assert!(assignment.mutate_index.is_some());
        }

        // Structural product, slice, and ref reads all lower through the same
        // checked dispatch recipe rather than a source-level access.
        for name in ["slice_read", "ref_read"] {
            let body = expression_body(&program, name);
            assert!(
                matches!(body.kind, LoweredExpressionKind::Index(_)),
                "`{name}` lowers through the index dispatcher"
            );
        }
        assert!(
            indexes.iter().any(
                |(_, index)| program
                    .expressions
                    .get(index.base)
                    .is_some_and(|base| matches!(
                        &base.value_type,
                        CheckedType::Product(product) if product.elements.len() == 2
                    ))
            ),
            "the fixed-product read lowers through the index dispatcher"
        );

        // The indexed assignment's `MutateIndex` dispatch agrees with the
        // lowered place operands (validated during snapshot).
        let assignments = program
            .items
            .iter()
            .filter_map(|(_, item)| match &item.kind {
                LoweredItemKind::Assignment(assignment) if assignment.mutate_index.is_some() => {
                    Some(assignment)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!assignments.is_empty());
    }

    #[test]
    fn string_templates_record_parts_and_formatting_selections() {
        let module = checked_program(concat!(
            "use std.fmt.Formatter\n",
            "type Label = ctor String\n",
            "impl Display Label {\n",
            "  def fmt = (Label value, mut formatter) => Formatter.write formatter value\n",
            "}\n",
            "def render: <T where Display T> move T -> String = move value => \"value=$value\"\n",
            "let name: String = \"world\"\n",
            "let answer: I32 = 42\n",
            "let product = (answer, name)\n",
            "let message: String = \"hello $name: ${answer}; ${product:?}; \\$5\"\n",
            "let generic: String = render answer\n",
            "let nominal: String = \"label=${Label \"tag\"}\"\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let formatting = program.string_formatting.clone();
        for function in [formatting.constructor, formatting.write, formatting.finish] {
            assert!(
                function.is_some_and(|id| program.functions.get(id).is_some()),
                "checked formatter helper selections resolve in the function catalog"
            );
        }

        let templates = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::StringTemplate(template) => Some(template),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            templates.len() >= 3,
            "message, nominal, and render templates"
        );

        let display_trait = module.semantic_ids().display_trait.expect("Display");
        let debug_trait = module.semantic_ids().debug_trait.expect("Debug");
        let message = templates
            .iter()
            .find(|template| {
                template.parts.iter().any(|part| {
                    matches!(part, LoweredStringTemplatePart::Literal(text) if text == "hello ")
                })
            })
            .expect("the message template lowers");
        let literals = message
            .parts
            .iter()
            .filter_map(|part| match part {
                LoweredStringTemplatePart::Literal(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(literals, vec!["hello ", ": ", "; ", "; $5"]);
        let interpolations = message
            .parts
            .iter()
            .filter_map(|part| match part {
                LoweredStringTemplatePart::Interpolation(interpolation) => Some(interpolation),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(interpolations.len(), 3);
        assert_eq!(
            interpolations[0].format,
            staple_syntax::StringInterpolationFormat::Display
        );
        assert_eq!(interpolations[0].trait_id, display_trait);
        assert_eq!(
            interpolations[1].format,
            staple_syntax::StringInterpolationFormat::Display
        );
        assert_eq!(interpolations[1].trait_id, display_trait);
        assert_eq!(
            interpolations[2].format,
            staple_syntax::StringInterpolationFormat::Debug
        );
        assert_eq!(interpolations[2].trait_id, debug_trait);
        for interpolation in &interpolations {
            assert!(program.expressions.contains(interpolation.expression));
            let value = program
                .expressions
                .get(interpolation.expression)
                .expect("interpolation value");
            assert_eq!(interpolation.value_type, value.value_type);
            let method = program
                .trait_methods
                .get(interpolation.method)
                .expect("formatting method");
            assert_eq!(method.trait_id, interpolation.trait_id);
        }

        // A generic interpolation keeps the declared type parameter and the
        // prelude's `Display` selection; nested interpolation expressions are
        // lowered left-to-right through the same arena.
        let generic = templates
            .iter()
            .find(|template| {
                template.parts.iter().any(|part| {
                    matches!(
                        part,
                        LoweredStringTemplatePart::Interpolation(interpolation)
                            if matches!(interpolation.value_type, CheckedType::Parameter { .. })
                    )
                })
            })
            .expect("the generic template keeps its parameter type");
        let generic_interpolation = generic
            .parts
            .iter()
            .find_map(|part| match part {
                LoweredStringTemplatePart::Interpolation(interpolation)
                    if matches!(interpolation.value_type, CheckedType::Parameter { .. }) =>
                {
                    Some(interpolation)
                }
                _ => None,
            })
            .expect("the generic interpolation");
        assert_eq!(generic_interpolation.trait_id, display_trait);
        assert!(
            program
                .trait_methods
                .get(generic_interpolation.method)
                .is_some_and(|method| method.trait_id == display_trait)
        );
        let nominal = templates
            .iter()
            .find(|template| {
                template.parts.iter().any(|part| {
                    matches!(
                        part,
                        LoweredStringTemplatePart::Literal(text) if text == "label="
                    )
                })
            })
            .expect("the nominal template lowers");
        let LoweredStringTemplatePart::Interpolation(interpolation) = &nominal.parts[1] else {
            panic!("the nominal interpolation is the second part");
        };
        assert!(matches!(
            program
                .expressions
                .get(interpolation.expression)
                .unwrap()
                .kind,
            LoweredExpressionKind::Call(_)
        ));
    }

    #[test]
    fn unreachable_string_templates_still_lower_their_parts() {
        let module = checked_program(concat!(
            "use std.fmt.Formatter\n",
            "let answer: I32 = 42\n",
            "def early = () => { return \"early\"; \"value=${answer}\" }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, early) = lowered_function(&program, "early");
        let block = body_block(&program, early);
        let result = block.result.expect("unreachable tail is still the result");
        let LoweredExpressionKind::StringTemplate(template) =
            &program.expressions.get(result).unwrap().kind
        else {
            panic!("the unreachable tail template still lowers");
        };
        assert!(template.parts.iter().any(|part| matches!(
            part,
            LoweredStringTemplatePart::Interpolation(interpolation)
                if interpolation.value_type == CheckedType::I32
        )));
    }

    /// A fixture exercising every lowering-owned family, every callable
    /// deferral, and the shared checked metadata the transition comparisons
    /// read back.
    fn complete_coverage_source() -> &'static str {
        concat!(
            "use std.cinterop.*\n",
            "use std.coroutine.(Coroutine)\n",
            "use std.fmt.Formatter\n",
            "type Counter = ctor (value: I32)\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "let integer: I32 = 42\n",
            "let float: F64 = 1.5\n",
            "let string: String = \"text\"\n",
            "let cstring = c_string \"c\"\n",
            "let named: I32 = integer\n",
            "let product = (left: 1, right: 2)\n",
            "let left = product.left\n",
            "let values: (I32; 2) = (1, 2)\n",
            "let element = values[0]\n",
            "def logical = (flag: Bool) => flag && flag\n",
            "def looping = () => loop { break 1 }\n",
            "def matching = (value: Ok I32 | IOError) => match value {\n",
            "  Ok inner => inner,\n",
            "  other => 0,\n",
            "}\n",
            "def blocked = () => { let local: I32 = 1; local }\n",
            "let coerced: I8 = 42 satisfies I8\n",
            "let repeated: (I32; 3) = (7; 3)\n",
            "let template: String = \"value=${integer}\"\n",
            "def callable = (value: I32) => value\n",
            "let applied = callable 1\n",
            "let closure = callable\n",
            "def task: () -> Coroutine{} I32 = () => coro { 7 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro { await (task ()) }\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
        )
    }

    fn expression_kind_name(kind: &LoweredExpressionKind) -> String {
        match kind {
            LoweredExpressionKind::Deferred(DeferredExpressionFamily::Callable) => {
                "deferred.callable".to_owned()
            }
            LoweredExpressionKind::Block(_) => "block".to_owned(),
            LoweredExpressionKind::Name(_) => "name".to_owned(),
            LoweredExpressionKind::Integer(_) => "integer".to_owned(),
            LoweredExpressionKind::Float(_) => "float".to_owned(),
            LoweredExpressionKind::String(_) => "string".to_owned(),
            LoweredExpressionKind::CString(_) => "cstring".to_owned(),
            LoweredExpressionKind::Access(_) => "access".to_owned(),
            LoweredExpressionKind::Product(_) => "product".to_owned(),
            LoweredExpressionKind::RepeatedProduct(_) => "repeated-product".to_owned(),
            LoweredExpressionKind::Satisfies(_) => "satisfies".to_owned(),
            LoweredExpressionKind::Logical(_) => "logical".to_owned(),
            LoweredExpressionKind::Loop(_) => "loop".to_owned(),
            LoweredExpressionKind::Match(_) => "match".to_owned(),
            LoweredExpressionKind::Index(_) => "index".to_owned(),
            LoweredExpressionKind::StringTemplate(_) => "string-template".to_owned(),
            LoweredExpressionKind::Call(_) => "call".to_owned(),
            LoweredExpressionKind::CallableValue(_) => "callable-value".to_owned(),
            LoweredExpressionKind::Resource(_) => "resource".to_owned(),
            LoweredExpressionKind::With(_) => "with".to_owned(),
            LoweredExpressionKind::Coro(_) => "coro".to_owned(),
            LoweredExpressionKind::Await(_) => "await".to_owned(),
        }
    }

    #[test]
    fn coverage_fixture_lowers_every_family_concretely() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let mut kinds = program
            .expressions
            .iter()
            .map(|(_, expression)| expression_kind_name(&expression.kind))
            .collect::<Vec<_>>();
        kinds.sort_unstable();
        kinds.dedup();
        for expected in [
            "access",
            "block",
            "call",
            "callable-value",
            "cstring",
            "float",
            "index",
            "integer",
            "logical",
            "loop",
            "match",
            "name",
            "product",
            "repeated-product",
            "satisfies",
            "resource",
            "await",
            "coro",
            "with",
            "string",
            "string-template",
        ] {
            assert!(
                kinds.iter().any(|kind| kind == expected),
                "the coverage fixture should lower a `{expected}` expression; have {kinds:?}"
            );
        }
    }

    /// The lowering completeness gate: every runtime construct of a fixture
    /// that exercises every expression family, resources, reactive operations,
    /// implicit thunks, captures, and coroutines has exactly one lowered
    /// counterpart, and no lowered node lacks a source.
    #[test]
    fn source_constructs_have_exactly_one_lowered_counterpart() {
        let source = format!(
            "{}{}",
            complete_coverage_source(),
            concat!(
                "let signal counter = 0\n",
                "let doubled = counter + counter\n",
                "with Reactive = reactive_scope () {\n",
                "  reaction { let current = counter; () }\n",
                "  batch { counter = 1 }\n",
                "  let observed = snapshot counter\n",
                "}\n",
                "def captured: () -> () -> I32 = () => {\n",
                "  let mut count: I32 = 0\n",
                "  () => { count = count + 1; count }\n",
                "}\n",
            )
        );
        let module = checked_program(&source);
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let coverage = program.validate_source_coverage(&module);
        assert!(coverage.is_empty(), "{coverage:?}");

        // Every checked function and implicit thunk is cataloged exactly once.
        assert_eq!(
            program.functions.iter().count(),
            module.functions().len() + module.implicit_thunks().count()
        );
        assert!(
            module.implicit_thunks().count() > 0,
            "the fixture should exercise implicit thunks"
        );
        assert!(
            program
                .functions
                .iter()
                .any(|(_, _, function)| !function.captures.is_empty()),
            "the fixture should exercise a capturing closure"
        );
    }

    /// Every new validation pass diagnoses directly mutated fixtures instead of
    /// panicking on malformed compiler state.
    #[test]
    fn validator_rejects_corrupted_runtime_ownership_and_coverage() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        assert!(program.validate_source_coverage(&module).is_empty());

        // Two functions sharing one body block.
        {
            let functions = program
                .functions
                .iter()
                .filter_map(|(_, key, function)| function.body.is_some().then_some(key))
                .take(2)
                .collect::<Vec<_>>();
            let [first, second] = functions[..] else {
                panic!("the fixture should lower at least two function bodies");
            };
            assert_ne!(
                program.functions.get(first).unwrap().body,
                program.functions.get(second).unwrap().body
            );
            let body = program.functions.get(second).unwrap().body;
            if let Some(function) = program.functions.get_mut(first) {
                function.body = body;
            }
        }
        assert!(
            program
                .validate()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("shares its body block")),
            "shared function bodies should diagnose"
        );

        // An expression owned by one function reached from another function.
        let (expression, owner) = program
            .expressions
            .iter()
            .find_map(|(id, expression)| match expression.key.owner {
                ExpressionOwner::Function(function) => Some((id, function)),
                ExpressionOwner::Module(_) => None,
            })
            .expect("a function-owned expression");
        let other_body = program
            .functions
            .iter()
            .find_map(|(_, key, function)| (key != owner).then_some(function.body).flatten())
            .expect("another function body");
        let injected = program
            .blocks
            .get(other_body)
            .is_some_and(|block| block.result != Some(expression));
        assert!(injected, "the other body should not already reference it");
        if let Some(block) = program.blocks.get_mut(other_body) {
            block.result = Some(expression);
        }
        assert!(
            program
                .validate()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("is owned by")
                    && diagnostic.message.contains("reached from")),
            "a cross-owner expression reference should diagnose"
        );

        // A block lookup that points at a different block syntax.
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        if let Some((key, id)) = program
            .block_lookup
            .iter()
            .next()
            .map(|(key, id)| (*key, *id))
        {
            program.block_lookup.remove(&key);
            program.block_lookup.insert(
                ExpressionKey {
                    syntax: SyntaxId::COMPILER,
                    ..key
                },
                id,
            );
        }
        assert!(
            program
                .validate()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("different block syntax")),
            "a stale block lookup should diagnose"
        );

        // An unresolved inference placeholder in runtime metadata.
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        let expression = program
            .expressions
            .iter()
            .next()
            .map(|(id, _)| id)
            .expect("a lowered expression");
        if let Some(expression) = program.expressions.get_mut(expression) {
            expression.value_type = CheckedType::Inferred;
        }
        assert!(
            program.validate().iter().any(|diagnostic| diagnostic
                .message
                .contains("unresolved inference placeholder")),
            "an inference placeholder should diagnose"
        );

        // A source expression with its lowered counterpart removed.
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        let key = program
            .expressions
            .iter()
            .next()
            .map(|(_, expression)| expression.key)
            .expect("a lowered expression occurrence");
        program.expression_lookup.remove(&key);
        assert!(
            program
                .validate_source_coverage(&module)
                .iter()
                .any(|diagnostic| {
                    diagnostic.message.contains("has no lowered counterpart")
                        || diagnostic
                            .message
                            .contains("does not match its lowered counterpart")
                }),
            "a missing source counterpart should diagnose"
        );

        // A module initializer with one runtime item removed.
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        let initializer = program
            .initializers
            .iter()
            .find(|(_, initializer)| {
                program
                    .blocks
                    .get(initializer.body)
                    .is_some_and(|block| !block.items.is_empty())
            })
            .map(|(id, _)| id)
            .expect("an initializer with runtime items");
        let body = program.initializers.get(initializer).unwrap().body;
        program.blocks.get_mut(body).unwrap().items.pop();
        assert!(
            program
                .validate_source_coverage(&module)
                .iter()
                .any(|diagnostic| diagnostic.message.contains("runtime items for")),
            "a missing initializer item should diagnose"
        );

        // A lowered function with no checked source function.
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        let template = program
            .functions
            .iter()
            .next()
            .map(|(_, _, function)| function.clone())
            .expect("a lowered function");
        let origin = template.origin.clone();
        let _ = program
            .functions
            .insert("function", FunctionId(9_999), origin, template);
        assert!(
            program
                .validate_source_coverage(&module)
                .iter()
                .any(|diagnostic| diagnostic
                    .message
                    .contains("has no checked source function")),
            "an invented function template should diagnose"
        );
    }

    #[test]
    fn source_coverage_walks_implicit_thunk_body_children() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate_source_coverage(&module).is_empty());
        let key = module
            .implicit_thunks()
            .find_map(|thunk| {
                let Expression::Block(block) = &thunk.body else {
                    return None;
                };
                block.items.iter().find_map(|item| {
                    let Item::Expression(expression) = item else {
                        return None;
                    };
                    let key = ExpressionKey {
                        syntax: expression.syntax().id,
                        owner: ExpressionOwner::Function(thunk.id),
                        context: ExpressionContext::Primary,
                    };
                    program.expression_lookup.contains_key(&key).then_some(key)
                })
            })
            .expect("an implicit thunk with a lowered body child");
        program.expression_lookup.remove(&key);
        assert!(
            program
                .validate_source_coverage(&module)
                .iter()
                .any(|diagnostic| diagnostic.message.contains("has no lowered counterpart")),
            "a missing expression inside an implicit thunk must diagnose"
        );
    }

    #[test]
    fn source_coverage_checks_nested_runtime_items() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate_source_coverage(&module).is_empty());
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == "blocked")
            .expect("fixture's block-bodied function");
        let body = program.functions.get(function.id).unwrap().body.unwrap();
        let block = program.blocks.get_mut(body).unwrap();
        assert!(!block.items.is_empty());
        block.items.remove(0);
        assert!(
            program
                .validate_source_coverage(&module)
                .iter()
                .any(|diagnostic| diagnostic.message.contains("runtime items for")),
            "a missing nested binding item must diagnose"
        );
    }

    #[test]
    fn validation_requires_every_function_body_block() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == "blocked")
            .expect("fixture's block-bodied function");
        program.functions.get_mut(function.id).unwrap().body = None;
        assert!(
            program
                .validate()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("has no lowered body block")),
            "a missing function body must fail structural validation"
        );
        assert!(
            program
                .validate_source_coverage(&module)
                .iter()
                .any(|diagnostic| diagnostic.message.contains("has no lowered body block")),
            "a missing function body must fail checked-source coverage"
        );
    }

    /// Transition comparison for every lowered expression's checked metadata
    /// and every function template's signature and captures. Diverged
    /// occurrences legitimately fall back to `Never` and empty effects.
    #[test]
    fn lowered_metadata_matches_checked_side_tables() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        for (_, expression) in program.expressions.iter() {
            if expression.key.context != ExpressionContext::Primary {
                continue;
            }
            let syntax = expression.key.syntax;
            match module.type_of_expression(syntax) {
                Some(expected) => assert_eq!(
                    &expression.value_type, expected,
                    "expression type for syntax {} should match the checker",
                    syntax.0
                ),
                None => assert_eq!(expression.value_type, CheckedType::Never),
            }
            match module.effects_of_expression(syntax) {
                Some(expected) => assert_eq!(
                    &expression.effects, expected,
                    "expression effects for syntax {} should match the checker",
                    syntax.0
                ),
                None => assert_eq!(expression.effects, CheckedEffectSet::default()),
            }
            assert_eq!(
                expression.coercion.as_ref(),
                module.coercion_for(syntax),
                "expression coercion for syntax {} should match the checker",
                syntax.0
            );
            let mut moved = module.moved_symbols(syntax).collect::<Vec<_>>();
            moved.sort_by_key(|symbol| symbol.0);
            assert_eq!(
                expression.moved_symbols, moved,
                "moved symbols for syntax {} should match the checker",
                syntax.0
            );
        }

        for (_, key, function) in program.functions.iter() {
            let source = module
                .function_by_id(key)
                .expect("every lowered function has a checked source function");
            assert_eq!(
                function
                    .captures
                    .iter()
                    .map(|capture| capture.symbol)
                    .collect::<Vec<_>>(),
                source.captures,
                "capture order for function {key:?} should match the checker"
            );
            assert_eq!(
                Some(&function.signature),
                module.type_of_function(key),
                "signature for function {key:?} should match the checker"
            );
        }
    }

    #[test]
    fn lowered_payloads_agree_with_checked_side_tables() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let formatting = module.string_formatting();
        for (_, expression) in program.expressions.iter() {
            let syntax = expression.key.syntax;
            match &expression.kind {
                LoweredExpressionKind::Integer(integer) => assert_eq!(
                    module
                        .type_of_expression(syntax)
                        .and_then(CheckedType::integer_type),
                    Some(integer.integer_type)
                ),
                LoweredExpressionKind::Float(float) => assert_eq!(
                    module
                        .type_of_expression(syntax)
                        .and_then(CheckedType::float_type),
                    Some(float.float_type)
                ),
                LoweredExpressionKind::Name(name) => assert_eq!(
                    name.requires_initialization_check,
                    module.resolved().requires_initialization_check(syntax)
                ),
                LoweredExpressionKind::Access(access) => {
                    let checked = module
                        .access_for(syntax)
                        .expect("checked access metadata")
                        .clone();
                    let expected = match checked {
                        CheckedAccess::Representation { dereference } => {
                            LoweredAccessKind::Representation { dereference }
                        }
                        CheckedAccess::Product {
                            index,
                            dereference,
                            slice,
                            scalar,
                        } => {
                            if scalar {
                                LoweredAccessKind::Scalar { dereference }
                            } else if slice {
                                LoweredAccessKind::Slice { index, dereference }
                            } else {
                                LoweredAccessKind::Product { index, dereference }
                            }
                        }
                    };
                    assert_eq!(access.kind, expected);
                }
                LoweredExpressionKind::Logical(logical) => assert_eq!(
                    module.logical_for(syntax).map(|checked| &checked.bool_type),
                    Some(&logical.bool_type)
                ),
                LoweredExpressionKind::Match(match_) => assert_eq!(
                    module.match_for(syntax).map(|checked| &checked.source),
                    Some(&match_.source)
                ),
                LoweredExpressionKind::Index(index) => {
                    assert_eq!(module.trait_dispatch_for(syntax), Some(&index.dispatch))
                }
                LoweredExpressionKind::StringTemplate(template) => {
                    for part in &template.parts {
                        let LoweredStringTemplatePart::Interpolation(interpolation) = part else {
                            continue;
                        };
                        let interpolation_syntax = program
                            .expressions
                            .get(interpolation.expression)
                            .expect("interpolation value")
                            .key
                            .syntax;
                        let checked = formatting
                            .interpolations
                            .get(&interpolation_syntax)
                            .unwrap_or_else(|| {
                                panic!(
                                    "interpolation {} has checked formatting metadata",
                                    interpolation.expression.index()
                                )
                            });
                        assert_eq!(interpolation.trait_id, checked.trait_id);
                        assert_eq!(interpolation.method, checked.method);
                        assert_eq!(interpolation.value_type, checked.value_type);
                    }
                }
                LoweredExpressionKind::Product(product) => {
                    if let Some(CheckedType::Product(checked)) = module.type_of_expression(syntax) {
                        if !checked.variadic {
                            assert_eq!(product.final_type.elements.len(), checked.elements.len());
                        }
                    }
                }
                LoweredExpressionKind::RepeatedProduct(repeated) => {
                    let expected = match module.type_of_expression(syntax) {
                        Some(CheckedType::Product(product)) if !product.variadic => {
                            LoweredRepeatCount::Fixed(product.elements.len())
                        }
                        Some(CheckedType::Array { count, .. }) => {
                            LoweredRepeatCount::Symbolic(count.as_ref().clone())
                        }
                        _ => LoweredRepeatCount::Fixed(1),
                    };
                    assert_eq!(repeated.count, expected);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn validator_rejects_orphaned_arena_nodes() {
        let module = checked_program("let value: I32 = 1\n");
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let key = ExpressionKey {
            syntax: SyntaxId(99_999),
            owner: ExpressionOwner::Module(ModuleId(99_999)),
            context: ExpressionContext::Primary,
        };
        let id = program.expressions.push(LoweredExpression {
            key,
            origin: Origin::compiler(),
            value_type: CheckedType::I32,
            effects: CheckedEffectSet::default(),
            coercion: None,
            coercion_plan: None,
            moved_symbols: Vec::new(),
            kind: LoweredExpressionKind::Name(LoweredName {
                symbol: SymbolId(0),
                requires_initialization_check: false,
                mutable: false,
                singleton: None,
                reactive: None,
            }),
        });
        program.expression_lookup.insert(key, id);
        let diagnostics = program.validate();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("is not reachable from any runtime root")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn invalid_literal_payloads_are_lowering_diagnostics() {
        let module = checked_program("use std.cinterop.*\nlet value = c_string \"bad\\0value\"\n");
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("interior NUL byte")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn function_parameter_patterns_lower_every_source_form() {
        let module = checked_program(concat!(
            "type TestOwned = ctor String\n",
            "def pair = (left: I32, right: I32) => left + right\n",
            "def wildcard = (_: I32) => 0\n",
            "def moved: move String -> String = move value => value\n",
            "def moved_nominal: move TestOwned -> String = move TestOwned value => value\n",
            "def singleton: True -> I32 = True => 0\n",
            "def borrowed: Ref I32 -> I32 = (Ref inner) => inner\n",
            "def literal: \"literal\" -> I32 = \"literal\" => 0\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, pair) = lowered_function(&program, "pair");
        let product = lowered_pattern(&program, pair.parameter_pattern);
        assert!(matches!(product.value_type, CheckedType::Product(_)));
        let LoweredPatternKind::Product { elements, .. } = &product.kind else {
            panic!("pair should lower to a product pattern");
        };
        assert_eq!(elements.len(), 2);
        for element in elements {
            let pattern = lowered_pattern(&program, *element);
            let LoweredPatternKind::Binding {
                symbol: Some(symbol),
                singleton: None,
                ..
            } = &pattern.kind
            else {
                panic!("pair elements should bind symbols");
            };
            assert_eq!(pattern.value_type, CheckedType::I32);
            assert!(program.symbols.get(*symbol).is_some());
        }

        let (_, wildcard) = lowered_function(&program, "wildcard");
        let pattern = lowered_pattern(&program, wildcard.parameter_pattern);
        let LoweredPatternKind::Product { elements, .. } = &pattern.kind else {
            panic!("`(_: I32)` should lower to a single-element product");
        };
        assert_eq!(elements.len(), 1);
        let element = lowered_pattern(&program, elements[0]);
        assert!(matches!(element.kind, LoweredPatternKind::Wildcard));
        assert_eq!(element.value_type, CheckedType::I32);

        let (_, moved) = lowered_function(&program, "moved");
        let pattern = lowered_pattern(&program, moved.parameter_pattern);
        let LoweredPatternKind::Binding {
            symbol: Some(_),
            moved: true,
            mutable: false,
            ..
        } = &pattern.kind
        else {
            panic!("a `move` parameter should lower to a moved binding");
        };
        assert_eq!(pattern.value_type, CheckedType::String);

        let (_, moved_nominal) = lowered_function(&program, "moved_nominal");
        let pattern = lowered_pattern(&program, moved_nominal.parameter_pattern);
        let LoweredPatternKind::Nominal {
            moved: true,
            argument,
            ..
        } = &pattern.kind
        else {
            panic!("a `move` nominal parameter should retain its ownership marker");
        };
        assert!(matches!(
            lowered_pattern(&program, *argument).kind,
            LoweredPatternKind::Binding {
                symbol: Some(_),
                ..
            }
        ));

        let (_, singleton) = lowered_function(&program, "singleton");
        let pattern = lowered_pattern(&program, singleton.parameter_pattern);
        let LoweredPatternKind::Binding {
            symbol: None,
            singleton: Some(target),
            ..
        } = &pattern.kind
        else {
            panic!("a singleton parameter should lower to a name-like binding");
        };
        let source = module
            .functions()
            .iter()
            .find(|function| function.name.contains("singleton"))
            .expect("singleton function");
        assert_eq!(
            Some(*target),
            module
                .resolved()
                .type_for_pattern(source.pattern.syntax().id)
        );

        let (_, borrowed) = lowered_function(&program, "borrowed");
        let pattern = lowered_pattern(&program, borrowed.parameter_pattern);
        let LoweredPatternKind::Product { elements, .. } = &pattern.kind else {
            panic!("`(Ref inner)` should lower to a single-element product");
        };
        assert_eq!(elements.len(), 1);
        let pattern = lowered_pattern(&program, elements[0]);
        let LoweredPatternKind::Nominal {
            target: Some(target),
            name,
            moved: false,
            argument,
        } = &pattern.kind
        else {
            panic!("`Ref inner` should lower to a nominal pattern");
        };
        assert_eq!(name, "Ref");
        assert_eq!(
            module.resolved().builtin_type(*target),
            Some(BuiltinType::Ref)
        );
        let argument = lowered_pattern(&program, *argument);
        assert!(matches!(
            argument.kind,
            LoweredPatternKind::Binding {
                symbol: Some(_),
                ..
            }
        ));
        assert_eq!(argument.value_type, CheckedType::I32);

        let (_, literal) = lowered_function(&program, "literal");
        let pattern = lowered_pattern(&program, literal.parameter_pattern);
        let LoweredPatternKind::Literal { literal } = &pattern.kind else {
            panic!("a string literal parameter should lower to a literal pattern");
        };
        assert_eq!(literal, "\"literal\"");
    }

    #[test]
    fn pattern_binding_items_lower_patterns_and_propagation_metadata() {
        let module = checked_program(concat!(
            "pub type Wrapper = pub ctor (value: I32)\n",
            "pub type Ok T = pub ctor T\n",
            "pub type IOError = pub ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok(42)\n",
            "def patterns = () => {\n",
            "  let (first, second) = (1, 2)\n",
            "  let whole@(third, fourth) = (3, 4)\n",
            "  let Wrapper inner = Wrapper (value: 5)\n",
            "  let Ref payload = Ref 6\n",
            "  let Ok(value)? = read()\n",
            "  first + second + third + fourth + whole.0 + inner + payload + value\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let validation = program.validate();
        assert!(validation.is_empty(), "{validation:?}");

        let (_, patterns) = lowered_function(&program, "patterns");
        let items = body_items(&program, patterns);
        assert_eq!(items.len(), 5);

        let LoweredItemKind::PatternBinding(binding) = &items[0].kind else {
            panic!("the product pattern binding should lower");
        };
        assert!(!binding.propagating && binding.propagation.is_none());
        assert!(matches!(
            lowered_pattern(&program, binding.pattern).kind,
            LoweredPatternKind::Product { .. }
        ));

        let LoweredItemKind::PatternBinding(binding) = &items[1].kind else {
            panic!("the at pattern binding should lower");
        };
        let LoweredPatternKind::At {
            binding: at_binding,
            pattern: nested,
        } = &lowered_pattern(&program, binding.pattern).kind
        else {
            panic!("`whole@(third, fourth)` should lower to an at pattern");
        };
        assert!(matches!(
            lowered_pattern(&program, *at_binding).kind,
            LoweredPatternKind::Binding {
                symbol: Some(_),
                ..
            }
        ));
        let LoweredPatternKind::Product { elements, .. } = &lowered_pattern(&program, *nested).kind
        else {
            panic!("the at pattern's nested pattern should be a product");
        };
        assert_eq!(elements.len(), 2);

        let LoweredItemKind::PatternBinding(binding) = &items[2].kind else {
            panic!("the nominal pattern binding should lower");
        };
        let LoweredPatternKind::Nominal {
            name,
            target: Some(target),
            ..
        } = &lowered_pattern(&program, binding.pattern).kind
        else {
            panic!("`Wrapper inner` should lower to a nominal pattern");
        };
        assert_eq!(name, "Wrapper");
        assert_eq!(
            program.types.get(*target).map(|info| info.name.as_str()),
            Some("Wrapper")
        );

        let LoweredItemKind::PatternBinding(binding) = &items[3].kind else {
            panic!("the reference pattern binding should lower");
        };
        assert!(matches!(
            lowered_pattern(&program, binding.pattern).kind,
            LoweredPatternKind::Nominal { .. }
        ));

        let LoweredItemKind::PatternBinding(binding) = &items[4].kind else {
            panic!("the propagating pattern binding should lower");
        };
        assert!(binding.propagating);
        let propagation = binding.propagation.as_ref().expect("checked propagation");
        assert_eq!(propagation.success_index, 0);
        let pattern = lowered_pattern(&program, binding.pattern);
        assert!(matches!(
            &pattern.kind,
            LoweredPatternKind::Nominal { name, .. } if name == "Ok"
        ));
        assert_eq!(pattern.value_type, propagation.source);
    }

    #[test]
    fn assignment_targets_lower_to_explicit_places() {
        let module = checked_program(concat!(
            "type Wrapper = ctor (value: I32)\n",
            "type Counter = ctor I32\n",
            "def places = (mut direct: I32, mut pair: (I32, I32), mut wrapper: Wrapper, mut counter: Counter, mut values: (I32; 2)) => {\n",
            "  direct = 1\n",
            "  pair.0 = 2\n",
            "  wrapper.value = 3\n",
            "  counter.* = 4\n",
            "  values[0] = 5\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, places) = lowered_function(&program, "places");
        let items = body_items(&program, places);
        assert_eq!(items.len(), 5);

        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("`direct = 1` should lower to an assignment item");
        };
        let LoweredPlaceKind::Symbol { symbol } = &lowered_place(&program, assignment.target).kind
        else {
            panic!("a parameter target should lower to symbol storage");
        };
        assert_eq!(assignment.initialization_symbol, Some(*symbol));
        assert!(assignment.mutate_index.is_none());
        assert!(assignment.signal_notify.is_none());
        assert!(!assignment.drop_previous);

        let LoweredItemKind::Assignment(assignment) = &items[1].kind else {
            panic!("`pair.0 = 2` should lower to an assignment item");
        };
        let LoweredPlaceKind::ProductElement {
            base,
            index: 0,
            slice: false,
        } = &lowered_place(&program, assignment.target).kind
        else {
            panic!("`pair.0` should lower to a product element place");
        };
        assert!(program.place_root_symbol(*base).is_some());
        assert_eq!(
            assignment.initialization_symbol,
            program.place_root_symbol(*base),
            "a field write resolves to its base's root symbol "
        );
        assert!(
            assignment.signal_notify.is_none(),
            "the base is not a signal, so the field write does not notify"
        );

        let LoweredItemKind::Assignment(assignment) = &items[2].kind else {
            panic!("`wrapper.value = 3` should lower to an assignment item");
        };
        assert!(matches!(
            &lowered_place(&program, assignment.target).kind,
            LoweredPlaceKind::Representation { .. }
        ));

        let LoweredItemKind::Assignment(assignment) = &items[3].kind else {
            panic!("`counter.* = 4` should lower to an assignment item");
        };
        let LoweredPlaceKind::Representation { base } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`counter.*` should lower to a representation place");
        };
        assert!(matches!(
            &lowered_place(&program, *base).kind,
            LoweredPlaceKind::Symbol { .. }
        ));

        let LoweredItemKind::Assignment(assignment) = &items[4].kind else {
            panic!("`values[0] = 5` should lower to an assignment item");
        };
        let LoweredPlaceKind::Indexed { base, .. } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`values[0]` should lower to an indexed place");
        };
        assert!(matches!(
            &lowered_place(&program, *base).kind,
            LoweredPlaceKind::Symbol { .. }
        ));
        assert!(assignment.mutate_index.is_some());
        assert!(assignment.initialization_symbol.is_none());
    }

    #[test]
    fn assignment_places_cross_references_and_materialize_temporaries() {
        let module = checked_program(concat!(
            "def make_ref: () -> Ref (I32, I32) = () => Ref (1, 2)\n",
            "def through_ref = (mut reference: Ref (I32, I32)) => { reference.0 = 9 }\n",
            "def through_call = () => { (make_ref())[0] = 9 }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, through_ref) = lowered_function(&program, "through_ref");
        let items = body_items(&program, through_ref);
        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("the reference assignment should lower");
        };
        let LoweredPlaceKind::ProductElement { base, index: 0, .. } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`reference.0` should lower to a product element");
        };
        let deref_place = lowered_place(&program, *base);
        let LoweredPlaceKind::Dereference {
            reference,
            dereference: payloads,
        } = &deref_place.kind
        else {
            panic!("crossing `Ref` should lower to a dereference place");
        };
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads.last(), Some(&deref_place.value_type));
        assert!(program.expressions.contains(*reference));
        assert!(assignment.initialization_symbol.is_none());

        let (_, through_call) = lowered_function(&program, "through_call");
        let items = body_items(&program, through_call);
        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("the call-rooted assignment should lower");
        };
        let LoweredPlaceKind::Indexed { base, .. } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`(make_ref())[0]` should lower to an indexed place");
        };
        let LoweredPlaceKind::Temporary { expression } = &lowered_place(&program, *base).kind
        else {
            panic!("a non-place indexed base should materialize a temporary");
        };
        assert!(program.expressions.contains(*expression));
    }

    #[test]
    fn captured_cell_and_resource_places_are_explicit() {
        let module = checked_program(concat!(
            "def counter = () => {\n",
            "  let mut count: I32 = 0\n",
            "  let bump = () => { count = count + 1; count }\n",
            "  bump ()\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let (_, bump) = lowered_function(&program, "bump");
        let items = body_items(&program, bump);
        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("the captured assignment should lower");
        };
        let LoweredPlaceKind::CapturedCell { symbol } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("a captured mutable local should lower to a captured cell");
        };
        assert!(lowered_symbol(&program, *symbol).captured_cell);

        let module = checked_program(concat!(
            "type Counter = ctor (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, increment) = lowered_function(&program, "increment");
        let items = body_items(&program, increment);
        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("the resource assignment should lower");
        };
        let LoweredPlaceKind::Representation { base } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`(resource Counter).value` should lower to a representation place");
        };
        let resource = lowered_place(&program, *base);
        let LoweredPlaceKind::Resource { use_ } = &resource.kind else {
            panic!("the representation base should be a resource place");
        };
        let use_ = program
            .resource_uses
            .get(*use_)
            .expect("the resource place should bind a use");
        assert_eq!(use_.kind, LoweredResourceUseKind::MutablePlace);
        assert!(use_.provider.is_some(), "the effect parameter is in scope");
        assert_eq!(resource.value_type, use_.resource.value_type);
        let Some(id) = nominal_type_id(&resource.value_type) else {
            panic!("a resource type should be nominal");
        };
        assert_eq!(
            program.types.get(id).map(|info| info.name.as_str()),
            Some("Counter")
        );
    }

    /// Provider identities owned by one lowered function, in creation order.
    fn function_providers(
        program: &LoweredProgram,
        function: FunctionId,
    ) -> Vec<LoweredResourceProviderId> {
        program
            .resource_providers
            .iter()
            .filter_map(|(id, provider)| {
                (provider.owner == ExpressionOwner::Function(function)).then_some(id)
            })
            .collect()
    }

    /// Resource reads owned by one lowered function, in expression order.
    fn function_resource_uses(
        program: &LoweredProgram,
        function: FunctionId,
    ) -> Vec<&LoweredResourceUse> {
        program
            .expressions
            .iter()
            .filter(|(_, expression)| expression.key.owner == ExpressionOwner::Function(function))
            .filter_map(|(_, expression)| match expression.kind {
                LoweredExpressionKind::Resource(use_) => program.resource_uses.get(use_),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn resource_uses_bind_the_nearest_matching_provider() {
        let module = checked_program(concat!(
            "type A = ctor (value: I32)\n",
            "type B = ctor (value: I32)\n",
            "def read_a: () ->{A} I32 = () => (resource A).value\n",
            "def read_b: () ->{B} I32 = () => (resource B).value\n",
            "def nested: () -> I32 = () => with A = A (value: 1) {\n",
            "  let outer = (resource A).value\n",
            "  with A = A (value: 2) { (resource A).value }\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, read_a) = lowered_function(&program, "read_a");
        let use_ = function_resource_uses(&program, read_a.semantic_id)
            .into_iter()
            .next()
            .expect("read_a should lower a resource read");
        let provider = program
            .resource_providers
            .get(use_.provider.expect("read_a provider"))
            .expect("provider");
        assert_eq!(provider.kind, LoweredProviderOriginKind::FunctionParameter);
        assert_eq!(
            provider.target,
            LoweredProviderTarget::EffectParameter { position: 0 }
        );
        assert_eq!(
            provider.owner,
            ExpressionOwner::Function(read_a.semantic_id)
        );
        assert!(
            !provider.indirect,
            "a plain `Copy` effect resource is passed by value"
        );

        let (_, nested) = lowered_function(&program, "nested");
        let providers = function_providers(&program, nested.semantic_id);
        assert_eq!(providers.len(), 2, "outer and inner `with` providers");
        let outer = providers[0];
        let inner = providers[1];
        assert_eq!(
            program.resource_providers.get(inner).unwrap().parent,
            Some(outer),
            "the inner provider nests in the outer one"
        );
        assert_eq!(program.resource_providers.get(outer).unwrap().parent, None);

        let reads = function_resource_uses(&program, nested.semantic_id);
        assert_eq!(reads.len(), 2);
        assert_eq!(
            reads[0].provider,
            Some(outer),
            "the binding before the inner `with` selects the outer provider"
        );
        assert_eq!(
            reads[1].provider,
            Some(inner),
            "the read inside the inner `with` selects the shadowing provider"
        );
    }

    #[test]
    fn with_records_provider_storage_scope_exit_and_borrow_facts() {
        let module = checked_program(concat!(
            "type A = ctor (value: I32)\n",
            "type Handle = ctor I32\n",
            "impl Drop Handle { def drop = Handle value => () }\n",
            "def mutable_provider: () -> () = () => {\n",
            "  let mut value = A (value: 1)\n",
            "  with mut A = value { () }\n",
            "}\n",
            "def materialized_provider: () -> () = () => with A = A (value: 2) { () }\n",
            "def borrowed_provider: () -> () = () => with Handle = Handle 1 { () }\n",
            "def aliased_provider: () ->{mut A} () = () => ",
            "  with mut A = resource A { (resource A).value = 2 }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, mutable) = lowered_function(&program, "mutable_provider");
        let providers = function_providers(&program, mutable.semantic_id);
        assert_eq!(providers.len(), 1);
        let provider = program.resource_providers.get(providers[0]).unwrap();
        assert!(provider.borrow, "a mutable provider is borrowed");
        assert!(provider.indirect, "a borrowed provider passes by pointer");
        assert_eq!(provider.storage, LoweredProviderStorage::Place);
        assert_eq!(provider.scope_exit, LoweredScopeExit::Ordinary);

        let (_, materialized) = lowered_function(&program, "materialized_provider");
        let providers = function_providers(&program, materialized.semantic_id);
        assert_eq!(providers.len(), 1);
        let provider = program.resource_providers.get(providers[0]).unwrap();
        assert_eq!(provider.storage, LoweredProviderStorage::Materialized);
        assert_eq!(provider.kind, LoweredProviderOriginKind::Source);
        assert!(matches!(
            provider.target,
            LoweredProviderTarget::Expression(_)
        ));

        let (_, borrowed) = lowered_function(&program, "borrowed_provider");
        let providers = function_providers(&program, borrowed.semantic_id);
        assert_eq!(providers.len(), 1);
        let provider = program.resource_providers.get(providers[0]).unwrap();
        assert!(provider.borrow, "a non-`Copy` provider is borrowed");
        assert!(provider.indirect, "a borrowed provider passes by pointer");
        assert_eq!(provider.storage, LoweredProviderStorage::Materialized);
        let (_, aliased) = lowered_function(&program, "aliased_provider");
        let providers = function_providers(&program, aliased.semantic_id);
        assert_eq!(providers.len(), 2, "effect parameter and nested `with`");
        let provider = program.resource_providers.get(providers[1]).unwrap();
        assert_eq!(provider.storage, LoweredProviderStorage::Place);
        assert_eq!(provider.parent, Some(providers[0]));
    }

    #[test]
    fn reactive_and_entry_providers_record_scope_exit_classifications() {
        let module = checked_program(concat!(
            "let signal count = 0\n",
            "with Reactive = reactive_scope () { count = 1 }\n",
            "def uses_borrowed_reactive: () ->{Reactive} () = () => ",
            "  reaction { () }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (entry_id, _) = entry_module(&program);
        let entry_roots = program
            .resource_providers
            .iter()
            .filter_map(|(id, provider)| {
                (provider.owner == ExpressionOwner::Module(entry_id)
                    && provider.kind == LoweredProviderOriginKind::EntryParameter)
                    .then_some((id, provider))
            })
            .collect::<Vec<_>>();
        assert_eq!(entry_roots.len(), 1, "the entry IO root");
        let io = entry_roots[0].1;
        assert_eq!(io.target, LoweredProviderTarget::Entry);
        assert_eq!(io.scope_exit, LoweredScopeExit::Ordinary);
        assert!(io.indirect);

        let with = program
            .withs
            .iter()
            .find(|(_, with)| {
                program
                    .resource_providers
                    .get(with.provider)
                    .is_some_and(|provider| {
                        provider.owner == ExpressionOwner::Module(entry_id)
                            && provider.kind == LoweredProviderOriginKind::Source
                    })
            })
            .map(|(_, with)| with)
            .expect("the source `with Reactive` should lower");
        let source_provider = program
            .resource_providers
            .get(with.provider)
            .expect("the source provider");
        assert_eq!(source_provider.kind, LoweredProviderOriginKind::Source);
        assert_eq!(source_provider.scope_exit, LoweredScopeExit::Reactive);
        assert_eq!(with.scope_exit, LoweredScopeExit::Reactive);

        let (_, borrowed) = lowered_function(&program, "uses_borrowed_reactive");
        let providers = function_providers(&program, borrowed.semantic_id);
        assert_eq!(providers.len(), 1);
        let provider = program.resource_providers.get(providers[0]).unwrap();
        assert_eq!(provider.kind, LoweredProviderOriginKind::FunctionParameter);
        assert_eq!(provider.scope_exit, LoweredScopeExit::Ordinary);

        program.resource_providers.values[providers[0].index()].scope_exit =
            LoweredScopeExit::Reactive;
        let diagnostics = program.validate();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("function effect parameter cannot own resource scope cleanup")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn validator_rejects_inconsistent_resource_records() {
        let module = checked_program(concat!(
            "type A = ctor (value: I32)\n",
            "def bump: () ->{mut A} () = () => {\n",
            "  (resource A).value = (resource A).value + 1\n",
            "}\n",
            "def nested: () -> I32 = () => with A = A (value: 1) {\n",
            "  let outer = (resource A).value\n",
            "  with A = A (value: 2) { (resource A).value }\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let (_, nested) = lowered_function(&program, "nested");
        let providers = function_providers(&program, nested.semantic_id);
        let inner_provider = providers[1];
        let use_id = program
            .resource_uses
            .iter()
            .find_map(|(id, use_)| (use_.provider == Some(inner_provider)).then_some(id))
            .expect("the inner `with` read");
        // Make the use's expected type disagree with its provider.
        program.resource_uses.values[use_id.index()]
            .resource
            .value_type = CheckedType::I32;
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("selected a provider for")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let (_, nested) = lowered_function(&program, "nested");
        let providers = function_providers(&program, nested.semantic_id);
        // Make the outer provider claim an effect-parameter origin while its
        // target is still an expression.
        program.resource_providers.values[providers[0].index()].kind =
            LoweredProviderOriginKind::FunctionParameter;
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("origin and target disagree")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let place_id = program
            .places
            .iter()
            .find_map(|(id, place)| {
                matches!(place.kind, LoweredPlaceKind::Resource { .. }).then_some(id)
            })
            .expect("the resource assignment place");
        let LoweredPlaceKind::Resource { use_ } =
            program.places.values[place_id.index()].kind.clone()
        else {
            unreachable!()
        };
        program.resource_uses.values[use_.index()].kind = LoweredResourceUseKind::Read;
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("resource read")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    /// Checked programs that are expected to fail checking, for diagnostics
    /// that must stay source errors.
    fn checked_program_diagnostics(source: &str) -> Result<TypedModule, Vec<Diagnostic>> {
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
        TypeChecker::new().check(resolved)
    }

    fn plans_by_resume_points(
        program: &LoweredProgram,
        points: usize,
    ) -> Vec<&LoweredCoroutinePlan> {
        program
            .coroutine_plans
            .iter()
            .filter_map(|(_, plan)| (plan.resume_points == points).then_some(plan))
            .collect()
    }

    #[test]
    fn coroutine_plans_copy_scanner_classifications() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "def child: () -> Coroutine{} I32 = () => coro { 7 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro {\n",
            "  let first = await (child ())\n",
            "  let second = await (child ())\n",
            "  first + second\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let driver = plans_by_resume_points(&program, 2);
        assert_eq!(driver.len(), 1, "the driver owns two suspension points");
        let plan = driver[0];
        assert_eq!(plan.result_type, CheckedType::I32);
        assert!(plan.deferred_effects.resources.is_empty());
        assert_eq!(
            plan.await_result_types,
            vec![CheckedType::I32, CheckedType::I32]
        );
        assert!(plan.wait_await_states.is_empty());
        assert!(plan.until_await_states.is_empty());
        assert_eq!(plan.frame_bindings.len(), 2);
        assert_ne!(plan.frame_bindings[0], plan.frame_bindings[1]);
        for symbol in &plan.frame_bindings {
            assert!(
                lowered_symbol(&program, *symbol).owner.is_some(),
                "frame bindings belong to a lowered function owner"
            );
        }

        let thunk = program.functions.get(plan.thunk).expect("body thunk");
        assert!(thunk.class.coroutine_body);
        assert_eq!(thunk.body, plan.body);
        assert_eq!(thunk.captures.len(), plan.captures.len());
        assert_eq!(
            thunk
                .captures
                .iter()
                .map(|capture| capture.symbol)
                .collect::<Vec<_>>(),
            plan.captures
                .iter()
                .map(|capture| capture.symbol)
                .collect::<Vec<_>>()
        );
        assert!(!plans_by_resume_points(&program, 0).is_empty());
    }

    #[test]
    fn nested_coroutines_own_separate_plans() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "def outer: () -> Coroutine{} I32 = () => coro {\n",
            "  let inner = coro { 1 }\n",
            "  let value = await inner\n",
            "  value\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let outer = plans_by_resume_points(&program, 1);
        let inner = plans_by_resume_points(&program, 0);
        assert_eq!(outer.len(), 1);
        assert_eq!(inner.len(), 1);
        assert_ne!(
            outer[0].thunk, inner[0].thunk,
            "nested body is its own thunk"
        );
        assert_ne!(outer[0].body, inner[0].body);
        assert_ne!(outer[0].body_syntax, inner[0].body_syntax);
        assert_eq!(
            program
                .functions
                .get(outer[0].thunk)
                .unwrap()
                .class
                .coroutine_body,
            true
        );
        assert_eq!(
            program
                .functions
                .get(inner[0].thunk)
                .unwrap()
                .class
                .coroutine_body,
            true
        );
    }

    #[test]
    fn coroutine_plans_preserve_wait_and_until_cancellation_states() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "let signal n = 0\n",
            "def waiter: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { n >= 5 })\n",
            "  ()\n",
            "}\n",
            "def observer: move Wait I32 -> Coroutine{} () = move w => coro {\n",
            "  let _ = await w; ()\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let waiting: Vec<_> = plans_by_resume_points(&program, 1)
            .into_iter()
            .filter(|plan| !plan.until_await_states.is_empty())
            .collect();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].until_await_states, vec![1]);
        assert!(waiting[0].wait_await_states.is_empty());

        let external: Vec<_> = plans_by_resume_points(&program, 1)
            .into_iter()
            .filter(|plan| !plan.wait_await_states.is_empty())
            .collect();
        assert_eq!(external.len(), 1);
        assert_eq!(external[0].wait_await_states, vec![1]);
        assert!(external[0].until_await_states.is_empty());
    }

    #[test]
    fn non_statement_position_await_stays_a_source_diagnostic() {
        let diagnostics = checked_program_diagnostics(concat!(
            "use std.coroutine.*\n",
            "def child: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro {\n",
            "  let value = await (child ()) + 1\n",
            "  value\n",
            "}\n",
        ))
        .expect_err("expression-position `await` should be rejected during checking");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("must be a statement")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    fn lowered_coros(program: &LoweredProgram) -> Vec<&LoweredCoro> {
        program.coros.iter().map(|(_, coro)| coro).collect()
    }

    #[test]
    fn coro_creation_links_plan_and_capture_environment() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "def capturing: () -> Coroutine{} I32 = () => {\n",
            "  let base = 40\n",
            "  coro { base + 2 }\n",
            "}\n",
            "def bare: () -> Coroutine{} I32 = () => coro { 1 }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let coros = lowered_coros(&program);
        assert_eq!(coros.len(), 2);
        let capturing = program
            .coros
            .iter()
            .find_map(|(_, coro)| {
                let plan = program.coroutine_plans.get(coro.plan)?;
                (!plan.captures.is_empty()).then_some(coro)
            })
            .expect("the capturing coro");
        assert_eq!(
            program
                .coroutine_plans
                .get(capturing.plan)
                .unwrap()
                .captures
                .len(),
            1
        );
        assert_eq!(capturing.environment, LoweredClosureEnvironment::Fresh);

        let bare = program
            .coros
            .iter()
            .find_map(|(_, coro)| {
                let plan = program.coroutine_plans.get(coro.plan)?;
                plan.captures.is_empty().then_some(coro)
            })
            .expect("the captureless coro");
        assert_eq!(bare.environment, LoweredClosureEnvironment::None);
    }

    #[test]
    fn await_sites_populate_plans_in_order_with_kinds() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "def child: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def driver: () -> Coroutine{Tasks} I32 = () => coro {\n",
            "  let first = await (child ())\n",
            "  let task = spawn (child ())\n",
            "  let outcome = await task\n",
            "  let _ = outcome\n",
            "  first\n",
            "}\n",
            "let signal flag = 0\n",
            "def waiter: () -> Coroutine{Reactive} () = () => coro {\n",
            "  let _ = await (until { flag >= 0 })\n",
            "  ()\n",
            "}\n",
            "def observer: move Wait I32 -> Coroutine{} () = move w => coro {\n",
            "  let _ = await w; ()\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let driver = plans_by_resume_points(&program, 2);
        assert_eq!(driver.len(), 1);
        let plan = driver[0];
        assert_eq!(plan.awaits.len(), 2);
        let first = program.awaits.get(plan.awaits[0]).expect("first await");
        assert_eq!(first.resume_state, 1);
        assert!(matches!(
            &first.kind,
            LoweredAwaitKind::ChildCoroutine {
                child_result,
                deferred_resources,
                until: false,
                ..
            } if *child_result == CheckedType::I32 && deferred_resources.is_empty()
        ));
        let second = program.awaits.get(plan.awaits[1]).expect("second await");
        assert_eq!(second.resume_state, 2);
        assert!(matches!(
            &second.kind,
            LoweredAwaitKind::Task { result } if *result == CheckedType::I32
        ));

        let until_plan = plans_by_resume_points(&program, 1)
            .into_iter()
            .find(|plan| !plan.until_await_states.is_empty())
            .expect("the until plan");
        let until_await = program
            .awaits
            .get(until_plan.awaits[0])
            .expect("until await");
        assert!(matches!(
            &until_await.kind,
            LoweredAwaitKind::ChildCoroutine { until: true, .. }
        ));

        let wait_plan = plans_by_resume_points(&program, 1)
            .into_iter()
            .find(|plan| !plan.wait_await_states.is_empty())
            .expect("the wait plan");
        let wait_await = program.awaits.get(wait_plan.awaits[0]).expect("wait await");
        assert!(matches!(
            &wait_await.kind,
            LoweredAwaitKind::Wait { result } if *result == CheckedType::I32
        ));
    }

    #[test]
    fn child_deferred_resources_bind_at_the_await_not_at_creation() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "type Counter = ctor (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "def task: () -> Coroutine{mut Counter} I32 = () => coro {\n",
            "  increment ()\n",
            "  0\n",
            "}\n",
            "def driver: () -> Coroutine{mut Counter} I32 = () => coro {\n",
            "  let child = task ()\n",
            "  await child\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        // The child's plan owns a deferred resource...
        let child_plan = plans_by_resume_points(&program, 0)
            .into_iter()
            .find(|plan| !plan.deferred_effects.resources.is_empty())
            .expect("the child plan defers a resource");
        assert_eq!(child_plan.deferred_effects.resources.len(), 1);

        // ...but the creation site only records its capture environment.
        let creation = lowered_coros(&program)
            .into_iter()
            .find(|coro| coro.plan == program.coroutine_plan_lookup[&child_plan.body_syntax])
            .expect("the child creation site");
        assert_eq!(creation.environment, LoweredClosureEnvironment::None);

        // The deferred resource binds when the driver awaits the child.
        let driver_plan = plans_by_resume_points(&program, 1)
            .into_iter()
            .find(|plan| !plan.awaits.is_empty())
            .expect("the driver plan");
        let await_ = program
            .awaits
            .get(driver_plan.awaits[0])
            .expect("the driver await");
        let LoweredAwaitKind::ChildCoroutine {
            deferred_resources, ..
        } = &await_.kind
        else {
            panic!("the driver awaits a child coroutine");
        };
        assert_eq!(deferred_resources.len(), 1);
        let use_ = program
            .resource_uses
            .get(deferred_resources[0])
            .expect("the deferred resource use");
        assert_eq!(use_.kind, LoweredResourceUseKind::HiddenArgument);
        assert_eq!(use_.pass_mode, LoweredArgumentPassMode::BorrowedPointer);
        let provider = program
            .resource_providers
            .get(use_.provider.expect("provider"))
            .expect("provider record");
        assert_eq!(provider.owner, ExpressionOwner::Function(driver_plan.thunk));
    }

    #[test]
    fn validator_rejects_misclassified_awaits() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "def child: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro {\n",
            "  await (child ())\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let await_id = program.awaits.iter().next().expect("an await").0;
        program.awaits.values[await_id.index()].kind = LoweredAwaitKind::Wait {
            result: CheckedType::I32,
        };
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("not classified as")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let plan_id = program
            .coroutine_plans
            .iter()
            .find_map(|(id, plan)| (plan.resume_points == 1).then_some(id))
            .expect("the driver plan");
        program.coroutine_plans.values[plan_id.index()]
            .awaits
            .clear();
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("awaits for")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    /// The lowered binding item for one symbol.
    fn binding_item_for(program: &LoweredProgram, symbol: SymbolId) -> &LoweredBindingItem {
        program
            .items
            .iter()
            .find_map(|(_, item)| match &item.kind {
                LoweredItemKind::Binding(binding) if binding.symbol == Some(symbol) => {
                    Some(binding)
                }
                _ => None,
            })
            .expect("a lowered binding item")
    }

    /// The first tracked read of `symbol` in expression order.
    fn name_reactive_read(
        program: &LoweredProgram,
        symbol: SymbolId,
    ) -> Option<LoweredReactiveOperationId> {
        program
            .expressions
            .iter()
            .find_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Name(name) if name.symbol == symbol => name.reactive,
                _ => None,
            })
    }

    #[test]
    fn signals_and_derived_bindings_record_reactive_operations() {
        let module = checked_program(concat!(
            "let signal global_count = 0\n",
            "let doubled = global_count + global_count\n",
            "def local: () -> I32 = () => {\n",
            "  let signal local_count = 0\n",
            "  local_count = local_count + 1\n",
            "  local_count\n",
            "}\n",
            "global_count = doubled\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let global_symbol = binding_symbol(&module, "global_count");
        let doubled_symbol = binding_symbol(&module, "doubled");
        let (_, local_function) = lowered_function(&program, "local");
        let local_symbol = body_items(&program, local_function)
            .iter()
            .find_map(|item| match &item.kind {
                LoweredItemKind::Binding(binding) if binding.signal => binding.symbol,
                _ => None,
            })
            .expect("the local signal binding symbol");

        let global = binding_item_for(&program, global_symbol);
        let Some(LoweredReactiveOperationKind::SignalCreate { symbol, storage }) = global
            .reactive
            .and_then(|operation| program.reactive_operations.get(operation))
            .map(|operation| &operation.kind)
        else {
            panic!("the global signal binding should create signal storage");
        };
        assert_eq!(*symbol, global_symbol);
        assert_eq!(*storage, LoweredSignalStorage::Global);

        let local = binding_item_for(&program, local_symbol);
        let Some(LoweredReactiveOperationKind::SignalCreate { storage, .. }) = local
            .reactive
            .and_then(|operation| program.reactive_operations.get(operation))
            .map(|operation| &operation.kind)
        else {
            panic!("the local signal binding should create signal storage");
        };
        assert_eq!(*storage, LoweredSignalStorage::LocalCell);

        let derived = binding_item_for(&program, doubled_symbol);
        let Some(LoweredReactiveOperationKind::DerivedCreate {
            symbol,
            evaluator,
            function_type,
            captures,
        }) = derived
            .reactive
            .and_then(|operation| program.reactive_operations.get(operation))
            .map(|operation| &operation.kind)
        else {
            panic!("the derived binding should record its evaluator");
        };
        assert_eq!(*symbol, doubled_symbol);
        assert!(program.functions.get(*evaluator).is_some());
        assert!(function_type.effects.resources.is_empty());
        assert_eq!(
            captures
                .iter()
                .map(|capture| capture.symbol)
                .collect::<Vec<_>>(),
            program
                .functions
                .get(*evaluator)
                .unwrap()
                .captures
                .iter()
                .map(|capture| capture.symbol)
                .collect::<Vec<_>>()
        );

        // Reads track signals and derived bindings distinctly.
        assert!(matches!(
            name_reactive_read(&program, global_symbol)
                .and_then(|operation| program.reactive_operations.get(operation))
                .map(|operation| &operation.kind),
            Some(LoweredReactiveOperationKind::SignalRead { .. })
        ));
        assert!(matches!(
            name_reactive_read(&program, doubled_symbol)
                .and_then(|operation| program.reactive_operations.get(operation))
                .map(|operation| &operation.kind),
            Some(LoweredReactiveOperationKind::DerivedRead { .. })
        ));

        // The local mutation notifies its signal.
        let notify = program.items.iter().find_map(|(_, item)| match &item.kind {
            LoweredItemKind::Assignment(assignment)
                if assignment.initialization_symbol == Some(local_symbol) =>
            {
                assignment.signal_notify
            }
            _ => None,
        });
        assert!(matches!(
            notify
                .and_then(|operation| program.reactive_operations.get(operation))
                .map(|operation| &operation.kind),
            Some(LoweredReactiveOperationKind::SignalNotify { symbol }) if *symbol == local_symbol
        ));
    }

    #[test]
    fn reactive_intrinsics_record_callbacks_ambient_scope_and_purity() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "let signal count = 0\n",
            "with Reactive = reactive_scope () {\n",
            "  let first = reaction { let current = count; () }\n",
            "  batch { count = 1 }\n",
            "  let waiting = until { count >= 1 }\n",
            "  let read = snapshot count\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let operations = program
            .reactive_operations
            .iter()
            .map(|(_, operation)| &operation.kind)
            .collect::<Vec<_>>();
        assert!(
            operations
                .iter()
                .any(|kind| matches!(kind, LoweredReactiveOperationKind::Reaction { .. }))
        );
        assert!(
            operations
                .iter()
                .any(|kind| matches!(kind, LoweredReactiveOperationKind::Batch { .. }))
        );
        assert!(
            operations
                .iter()
                .any(|kind| matches!(kind, LoweredReactiveOperationKind::Until { .. }))
        );
        assert!(
            operations
                .iter()
                .any(|kind| matches!(kind, LoweredReactiveOperationKind::Snapshot))
        );
        // `reactive_scope ()` lowers its stdlib wrapper, whose `__reactive_scope`
        // intrinsic call owns the scope operation.
        assert!(
            operations
                .iter()
                .any(|kind| matches!(kind, LoweredReactiveOperationKind::Scope))
        );

        let reaction = operations
            .iter()
            .find_map(|kind| match kind {
                LoweredReactiveOperationKind::Reaction {
                    callback,
                    reactive_provider,
                } => Some((*callback, *reactive_provider)),
                _ => None,
            })
            .expect("the reaction operation");
        assert!(reaction.1.is_some(), "the ambient scope provides Reactive");
        let callback = program
            .reactive_callbacks
            .get(reaction.0)
            .expect("reaction callback");
        assert!(callback.thunk.is_some() && callback.callable.is_none());
        assert_eq!(
            callback.function_type,
            program
                .functions
                .get(callback.thunk.unwrap())
                .unwrap()
                .signature
        );
        assert!(callback.resources.is_empty());

        let until = operations
            .iter()
            .find_map(|kind| match kind {
                LoweredReactiveOperationKind::Until {
                    predicate,
                    reactive_provider,
                } => Some((*predicate, *reactive_provider)),
                _ => None,
            })
            .expect("the until operation");
        assert!(until.1.is_some());
        let predicate = program
            .reactive_callbacks
            .get(until.0)
            .expect("until predicate");
        assert!(predicate.function_type.effects.resources.is_empty());
    }

    #[test]
    fn validator_rejects_impure_until_predicates_and_unattached_reactive_sites() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "let signal count = 0\n",
            "with Reactive = reactive_scope () {\n",
            "  let waiting = until { count >= 1 }\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let predicate = program
            .reactive_operations
            .iter()
            .find_map(|(_, operation)| match &operation.kind {
                LoweredReactiveOperationKind::Until { predicate, .. } => Some(*predicate),
                _ => None,
            })
            .expect("the until operation");
        let callback = program
            .reactive_callbacks
            .get(predicate)
            .expect("predicate callback");
        let mut function_type = callback.function_type.clone();
        function_type.effects.state = Some(crate::CheckedStateEffect::Write);
        program.reactive_callbacks.values[predicate.index()].function_type = function_type;
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("until` predicate")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let signal_symbol = binding_symbol(&module, "count");
        let item_id = program
            .items
            .iter()
            .find_map(|(id, item)| match &item.kind {
                LoweredItemKind::Binding(binding) if binding.symbol == Some(signal_symbol) => {
                    Some(id)
                }
                _ => None,
            })
            .expect("the signal binding item");
        let mut binding = match &program.items.values[item_id.index()].kind {
            LoweredItemKind::Binding(binding) => binding.clone(),
            _ => unreachable!(),
        };
        binding.reactive = None;
        program.items.values[item_id.index()].kind = LoweredItemKind::Binding(binding);
        let diagnostics = program.validate();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("no reactive creation operation")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn module_items_record_binding_assignment_and_statement_metadata() {
        let program = snapshot(concat!(
            "const answer: I32 = 42\n",
            "let signal count: I32 = 0\n",
            "let doubled: I32 = count + count\n",
            "def generic: <T> move T -> T = move value => value\n",
            "let mut mutable: I32 = 1\n",
            "mutable = 2\n",
            "count = 1\n",
            "type Handle = ctor I32\n",
            "impl Drop Handle { def drop = Handle value => () }\n",
            "def discard = () => {\n",
            "  let owned: Handle = Handle 1\n",
            "  owned\n",
            "  ()\n",
            "}\n",
        ));
        let (entry_id, _) = entry_module(&program);
        let initializer = program
            .initializers
            .get(program.modules.get(entry_id).unwrap().initializer)
            .expect("entry initializer");
        let items = program
            .blocks
            .get(initializer.body)
            .expect("initializer body")
            .items
            .iter()
            .map(|item| program.items.get(*item).expect("item"))
            .collect::<Vec<_>>();

        let LoweredItemKind::Binding(binding) = &items[0].kind else {
            panic!("the const binding should lower");
        };
        // `const` values are folded, but every runtime reference still reads a
        // module global, so the binding is an ordinary runtime binding.
        assert!(!binding.compile_time_only);
        let const_symbol = binding.symbol.expect("the const binding symbol");
        assert!(binding.value.is_some());
        assert_eq!(
            program
                .symbols
                .get(const_symbol)
                .map(|symbol| symbol.storage),
            Some(SymbolStorage::GlobalStorage)
        );

        let LoweredItemKind::Binding(binding) = &items[1].kind else {
            panic!("the signal binding should lower");
        };
        assert!(binding.signal && !binding.derived);
        assert!(!binding.cell, "module bindings use global storage");
        assert!(binding.value.is_some());
        let signal_symbol = binding.symbol.expect("the signal binding symbol");

        let LoweredItemKind::Binding(binding) = &items[2].kind else {
            panic!("the derived binding should lower");
        };
        assert!(binding.derived && !binding.signal);

        let LoweredItemKind::Binding(binding) = &items[3].kind else {
            panic!("the generic binding should lower");
        };
        assert!(binding.generic);
        assert!(binding.value.is_some());

        let LoweredItemKind::Binding(binding) = &items[4].kind else {
            panic!("the mutable binding should lower");
        };
        assert!(!binding.cell);

        let LoweredItemKind::Assignment(assignment) = &items[5].kind else {
            panic!("the mutable global assignment should lower");
        };
        assert!(assignment.mutate_index.is_none());
        assert!(assignment.initialization_symbol.is_some());
        assert!(assignment.signal_notify.is_none() && !assignment.drop_previous);

        let LoweredItemKind::Assignment(assignment) = &items[6].kind else {
            panic!("the signal assignment should lower");
        };
        assert!(assignment.signal_notify.is_some());
        assert_eq!(assignment.initialization_symbol, Some(signal_symbol));

        let (_, discard) = lowered_function(&program, "discard");
        let items = body_items(&program, discard);
        let LoweredItemKind::Expression(statement) = &items[1].kind else {
            panic!("the discarded handle should lower to a statement");
        };
        assert!(statement.drop_result);
        assert!(matches!(
            program
                .expressions
                .get(statement.expression)
                .expect("statement expression")
                .value_type,
            CheckedType::Distinct { .. }
        ));
    }

    #[test]
    fn function_bodies_lower_into_blocks_with_separate_results() {
        let module = checked_program(concat!(
            "def expression_body = () => 42\n",
            "def block_body = () => { let value: I32 = 1; value }\n",
            "def early = () => { return 1; 0 }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let (_, expression_body) = lowered_function(&program, "expression_body");
        let block = body_block(&program, expression_body);
        assert!(block.items.is_empty());
        let result = block.result.expect("the body expression is the result");
        assert_eq!(
            program.expressions.get(result).unwrap().value_type,
            CheckedType::I32
        );

        let (_, block_body) = lowered_function(&program, "block_body");
        let block = body_block(&program, block_body);
        assert_eq!(block.items.len(), 1);
        assert!(matches!(
            program.items.get(block.items[0]).unwrap().kind,
            LoweredItemKind::Binding(_)
        ));
        let result = block.result.expect("the tail expression is the result");
        assert!(program.expressions.contains(result));
        assert!(
            !block.items.iter().any(|item| matches!(
                &program.items.get(*item).unwrap().kind,
                LoweredItemKind::Expression(statement) if statement.expression == result
            )),
            "a block result must not also be an item"
        );

        let (_, early) = lowered_function(&program, "early");
        let block = body_block(&program, early);
        assert_eq!(block.items.len(), 1);
        let LoweredItemKind::Return(item) = &program.items.get(block.items[0]).unwrap().kind else {
            panic!("`return` should lower to a return item");
        };
        assert!(program.expressions.contains(item.value));
        let result = block
            .result
            .expect("the unreachable tail is still the result");
        assert_eq!(
            program.expressions.get(result).unwrap().value_type,
            CheckedType::I32
        );
    }

    #[test]
    fn lowering_rejects_compile_time_only_runtime_nodes() {
        let module = checked_program("let answer: I32 = 1\n");
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let splice = Item::RepeatedItemSplice(staple_syntax::RepeatedItemSplice {
            syntax: staple_syntax::Syntax::compiler(),
            name: "items".to_owned(),
        });
        let owner = ExpressionOwner::Module(ModuleId(0));
        let diagnostic = program
            .lower_item(&module, owner, ExpressionContext::Primary, &splice)
            .expect_err("a repeated item splice should be rejected");
        assert!(diagnostic.message.contains("repeated item splice"));

        let splice = Item::VisibilitySplice(staple_syntax::VisibilitySplice {
            syntax: staple_syntax::Syntax::compiler(),
            name: "item".to_owned(),
            item: Box::new(Item::Expression(Expression::Integer(
                staple_syntax::IntegerExpression {
                    syntax: staple_syntax::Syntax::compiler(),
                    literal: "1".to_owned(),
                },
            ))),
        });
        let diagnostic = program
            .lower_item(&module, owner, ExpressionContext::Primary, &splice)
            .expect_err("a visibility splice should be rejected");
        assert!(diagnostic.message.contains("visibility splice"));

        let source = staple_syntax::parse("1 + 2").expect("binary source should parse");
        let Some(Item::Expression(expression)) = source.items.first() else {
            panic!("`1 + 2` should parse to an expression item");
        };
        let diagnostic = reject_compile_time_expression(expression)
            .expect_err("an unresolved binary expression should be rejected");
        assert!(diagnostic.message.contains("unresolved `+`"));

        let source = staple_syntax::parse("-1").expect("unary source should parse");
        let Some(Item::Expression(expression)) = source.items.first() else {
            panic!("`-1` should parse to an expression item");
        };
        let diagnostic = reject_compile_time_expression(expression)
            .expect_err("an unresolved unary expression should be rejected");
        assert!(diagnostic.message.contains("unresolved `-`"));
    }

    #[test]
    fn validator_rejects_dangling_pattern_place_and_item_references() {
        let module = checked_program(concat!(
            "def places = (mut value: I32) => { value = value + 1; value }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        program.places.values[0].kind = LoweredPlaceKind::ProductElement {
            base: PlaceId::from_index(999_999),
            index: 0,
            slice: false,
        };
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling place reference")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        program.patterns.values[0].kind = LoweredPatternKind::At {
            binding: PatternId::from_index(999_999),
            pattern: PatternId::from_index(999_998),
        };
        let diagnostics = program.validate();
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.message.contains("at pattern"))
                .count(),
            2,
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let (item_id, value) = program
            .items
            .iter()
            .find_map(|(id, item)| match &item.kind {
                LoweredItemKind::Assignment(assignment) => Some((id, assignment.value)),
                _ => None,
            })
            .expect("the assignment should lower");
        program.items.values[item_id.index()].kind =
            LoweredItemKind::Assignment(LoweredAssignmentItem {
                target: PlaceId::from_index(999_999),
                value,
                mutate_index: None,
                evidence: None,
                initialization_symbol: None,
                drop_previous: false,
                drops_base_temporary: false,
                signal_notify: None,
            });
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling place reference")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn arenas_are_stable_across_repeated_lowering() {
        let module = checked_program(concat!(
            "pub type Wrapper = pub ctor (value: I32)\n",
            "pub type Ok T = pub ctor T\n",
            "pub type IOError = pub ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok(42)\n",
            "let signal count: I32 = 0\n",
            "let doubled: I32 = count + count\n",
            "count = 1\n",
            "def places = (mut pair: (I32, I32), mut values: (I32; 2)) => {\n",
            "  pair.0 = 1\n",
            "  values[0] = 2\n",
            "}\n",
            "def patterns = () => {\n",
            "  let (first, second) = (1, 2)\n",
            "  let Ok(value)? = read()\n",
            "  let Wrapper inner = Wrapper (value: first + second + value)\n",
            "  inner\n",
            "}\n",
        ));
        let mut first = LoweredProgram::default();
        let mut second = LoweredProgram::default();
        let diagnostics = first.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(first.validate().is_empty());
        assert!(second.snapshot(&module).is_empty());
        assert!(second.validate().is_empty());

        let first_snapshot = normalized_program_snapshot(&first);
        assert!(!first_snapshot.is_empty());
        assert_eq!(first_snapshot, normalized_program_snapshot(&second));
    }
}
