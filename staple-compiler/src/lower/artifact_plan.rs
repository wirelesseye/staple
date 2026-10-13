//! Typed plans for generated artifacts.
//!
//! Each artifact family records concrete identities, selected callees, layout
//! order, and cleanup decisions. Request-time markers carry identity only;
//! family expansion fills their bodies, and validation rejects unexpanded plans.
//! Callees are initially named by canonical key and bound to catalog ordinals
//! after closure. Target-specific LLVM layout stays in codegen.

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
    /// A constructor value's callable adapter. Expansion records the recursive
    /// construction class and the parameter-to-representation mapping.
    ConstructorAdapter(ConstructorAdapterPlan),
    /// A compiler-generated structural method. Expansion records the ordered
    /// labels, element types, selected callees, and formatting edges.
    StructuralMethod(StructuralMethodPlan),
    /// Drop glue for one concrete value type. Expansion records the selected
    /// `Drop` method and the ordered representation cleanup steps.
    DropGlue(DropGluePlan),
    TypeDescriptor(TypeDescriptorPlan),
    TraitDictionary(TraitDictionaryPlan),
    DescriptorDrop(DescriptorDropPlan),
    CompoundDescriptorDrop(crate::specialization::CompoundDescriptorDropKey),
    /// A dynamic-shape closure's forwarding entry. The entry loads the
    /// instance's descriptors from its environment and forwards the call.
    DescriptorBoundEntry(DescriptorBoundEntryPlan),
    TraitMemberEntry(TraitMemberEntryPlan),
    /// A structural trait member at run-time component types: the dictionary
    /// slot of a product or sum, reading its components from the dictionary.
    DynamicStructural(DynamicStructuralPlan),
    /// A closure that forwards to a polymorphic closure: at an instantiated
    /// function type's ABI with the instantiation's descriptors, or at
    /// another scheme's dynamic ABI with descriptors rebuilt from its own.
    InstantiationAdapter(InstantiationAdapterPlan),
    /// A reabstraction thunk: converts a closure between the concrete and
    /// dynamic ABIs.
    ReabstractionThunk(ReabstractionThunkPlan),
    ValueConversion(ValueConversionPlan),
    /// A garbage-collector finalizer. Expansion records the referenced drop glue.
    GcFinalizer(GcFinalizerPlan),
    /// A coroutine `resume`/`cleanup` pair. Expansion records the frame layout
    /// inputs, cleanup drop glue, and thunk-environment finalizer.
    CoroutineCodes(CoroutineCodesPlan),
    /// A reaction subscription runner. Expansion records the callback route,
    /// closure type, ordered resources, and payload slot order.
    ReactionRunner(ReactiveRunnerPlan),
    /// An `until` predicate runner. Expansion records the predicate closure type.
    UntilRunner(ReactiveRunnerPlan),
    /// A derived binding runner. Expansion records the evaluator call shape.
    DerivedRunner(ReactiveRunnerPlan),
    /// An extern closure adapter. Lowering records the eager
    /// foreign-symbol declaration parity facts; the callable sites that use
    /// the adapter live on their owners' use records.
    ExternAdapter(ExternAdapterPlan),
}

/// A static dictionary's ordered pointers and concrete closure tail.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TraitDictionaryPlan {
    pub trait_id: TraitId,
    pub arguments: Vec<CheckedType>,
    pub implementation: super::LoweredTraitImplementationId,
    pub layout: super::TraitDictionaryLayout,
    pub slots: Vec<PlannedArtifact>,
    /// Marker prerequisites have no dictionary; their prefix slot is null.
    pub prerequisites: Vec<Option<PlannedArtifact>>,
    pub associated: Vec<PlannedArtifact>,
    pub methods: Vec<PlannedInstance>,
    pub expanded: bool,
}

/// A descriptor-bound entry: the forwarding function a dynamic-shape
/// closure points at. Its signature receives the descriptors not bound by the
/// environment; the body loads the leading bound descriptors from the closure
/// environment and forwards them followed by the caller's descriptors.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DescriptorBoundEntryPlan {
    /// Number of leading descriptors supplied by the closure environment.
    pub bound: usize,
    /// Descriptor slots before bound dictionaries: the full reserved prefix
    /// for closure environments, or the compact header prefix for slot tails.
    pub dictionary_prefix: usize,
    /// Dictionaries in the full instance ABI and its bound lexical prefix.
    pub dictionary_count: usize,
    pub bound_dictionaries: usize,
    /// Pointer slots before the first bound word in the environment.
    pub environment_offset: usize,
    /// The dynamic instance the entry forwards to.
    pub instance: FunctionInstanceId,
    /// The instance's run-time parameters in descriptor ABI order.
    pub runtime: Vec<crate::TypeParameterId>,
    /// The closure's function type: the dynamic shape without descriptors.
    pub function_type: CheckedFunctionType,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TraitMemberEntryPlan {
    pub method: crate::TraitMethodId,
    pub callee: Option<PlannedInstance>,
    pub function_type: CheckedFunctionType,
    pub descriptors: usize,
    pub dictionaries: usize,
    pub slot: usize,
    pub own_descriptors: usize,
}

/// A structural dictionary slot at run-time component types. Products write
/// their literals through `Formatter.write`; sums write nothing themselves.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DynamicStructuralPlan {
    pub shape: crate::specialization::DynamicStructuralShape,
    /// The trait whose dictionary prefix the slot's tail follows.
    pub trait_id: crate::TraitId,
    pub write: Option<PlannedInstance>,
}

/// A polymorphic-value adapter. Its environment holds the
/// polymorphic closure's code and environment; an instantiation adapter
/// follows them with one descriptor per type parameter of the scheme, in
/// scheme order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InstantiationAdapterPlan {
    pub scheme: crate::CheckedScheme,
    /// The type the adapter exposes: an instantiated function type, whose ABI
    /// is the dynamic shape when it mentions run-time parameters and the
    /// concrete closure ABI otherwise, or another scheme.
    pub target: CheckedType,
    /// The scheme's instantiation, one type per type parameter. For a scheme
    /// target the types mention the target's own parameters.
    pub types: Vec<CheckedType>,
    /// Dictionaries the quantified entry receives: the scheme's non-marker
    /// bounds. An instantiation adapter stores them beside its descriptors.
    pub dictionaries: usize,
    /// A scheme target's own dictionaries, which the adapter receives.
    pub target_dictionaries: usize,
    /// For a scheme target: recipes producing the source's dictionaries from
    /// the target's, in the source's bound order (bound entailment).
    pub entailment: Vec<crate::lower::DictionaryCallArgument>,
    /// Value conversions for function-typed arguments and results, in
    /// `value_conversions::adapter_conversions` order: per argument slot the
    /// concrete-to-dynamic conversion and, for a mutated slot, the
    /// dynamic-to-concrete writeback, then the result's dynamic-to-concrete
    /// conversion. Codegen finds each by its key.
    pub children: Vec<PlannedArtifact>,
}

/// A reabstraction thunk. The concrete side uses the concrete closure
/// ABI; the dynamic side uses the dynamic shape without descriptor parameters.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReabstractionThunkPlan {
    pub dictionary_slot: Option<crate::specialization::StaticDictionarySlot>,
    pub concrete: CheckedFunctionType,
    pub dynamic: CheckedFunctionType,
    pub direction: crate::specialization::ReabstractionDirection,
    /// Parameter conversion, result conversion, and mutable parameter writeback,
    /// in that order.
    pub children: Vec<PlannedArtifact>,
}

/// A by-value conversion. Children are field converters or the leaf closure
/// thunk; every dependency is closed and bound before emission.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ValueConversionPlan {
    pub concrete: CheckedType,
    pub dynamic: CheckedType,
    pub direction: crate::specialization::ReabstractionDirection,
    pub children: Vec<PlannedArtifact>,
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
            LoweredArtifactPlan::TypeDescriptor(_) => "type-descriptor",
            LoweredArtifactPlan::TraitDictionary(_) => "trait-dictionary",
            LoweredArtifactPlan::DescriptorDrop(_) => "descriptor-drop",
            LoweredArtifactPlan::CompoundDescriptorDrop(_) => "compound-descriptor-drop",
            LoweredArtifactPlan::DescriptorBoundEntry(_) => "descriptor-bound-entry",
            LoweredArtifactPlan::TraitMemberEntry(_) => "trait-member-entry",
            LoweredArtifactPlan::DynamicStructural(_) => "dynamic-structural",
            LoweredArtifactPlan::InstantiationAdapter(_) => "instantiation-adapter",
            LoweredArtifactPlan::ReabstractionThunk(_) => "reabstraction-thunk",
            LoweredArtifactPlan::ValueConversion(_) => "value-conversion",
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
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::DynamicClosureEnvironment {
                ..
            }) => "gc-finalizer-dynamic-closure-environment",
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::DynamicBuffer) => {
                "gc-finalizer-dynamic-buffer"
            }
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::DynamicCell) => {
                "gc-finalizer-dynamic-cell"
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
                    && if let Some(scheme) = &plan.scheme {
                        crate::specialization::ConstructorAdapterKey::polymorphic(
                            plan.symbol,
                            plan.type_id,
                            plan.adapter,
                            scheme,
                            origin,
                        )
                        .is_ok_and(|expected| expected == *key)
                    } else {
                        key.scheme.is_none()
                            && CanonicalFunctionType::dynamic(&plan.callable_type, origin)
                                .is_ok_and(|shape| shape == key.callable_type)
                    }
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
            (
                LoweredArtifactPlan::TraitDictionary(plan),
                ArtifactRequestKey::TraitDictionary(key),
            ) => {
                plan.trait_id == key.trait_id
                    && plan.implementation == key.implementation
                    && plan.arguments.len() == key.arguments.len()
                    && plan
                        .arguments
                        .iter()
                        .zip(&key.arguments)
                        .all(|(argument, expected)| same_type(argument, expected))
            }
            (
                LoweredArtifactPlan::TypeDescriptor(plan),
                ArtifactRequestKey::TypeDescriptor(key),
            ) => same_type(&plan.value_type, key),
            (
                LoweredArtifactPlan::DescriptorDrop(plan),
                ArtifactRequestKey::DescriptorDrop(key),
            ) => same_type(&plan.value_type, key),
            (
                LoweredArtifactPlan::CompoundDescriptorDrop(plan),
                ArtifactRequestKey::CompoundDescriptorDrop(key),
            ) => plan == key,
            (
                LoweredArtifactPlan::TraitMemberEntry(plan),
                ArtifactRequestKey::TraitMemberEntry(key),
            ) => match key {
                crate::specialization::TraitMemberEntryKey::Member(method) => {
                    plan.method == *method && plan.callee.is_none()
                }
                crate::specialization::TraitMemberEntryKey::Slot(key) => plan
                    .callee
                    .as_ref()
                    .is_some_and(|callee| &callee.key == key),
            },
            (
                LoweredArtifactPlan::DescriptorBoundEntry(plan),
                ArtifactRequestKey::DescriptorBoundEntry(key),
            ) => {
                plan.instance.index() == key.instance.index()
                    && plan.bound == key.bound
                    && plan.environment_offset == key.offset
                    && plan.bound <= plan.runtime.len()
                    && plan.bound_dictionaries <= plan.dictionary_count
            }
            (
                LoweredArtifactPlan::DynamicStructural(plan),
                ArtifactRequestKey::DynamicStructural(key),
            ) => plan.shape == key.shape,
            (
                LoweredArtifactPlan::InstantiationAdapter(plan),
                ArtifactRequestKey::InstantiationAdapter(key),
            ) => {
                CanonicalType::dynamic(&CheckedType::Forall(Box::new(plan.scheme.clone())), origin)
                    .is_ok_and(|scheme| scheme == key.scheme)
                    && CanonicalType::dynamic(&plan.target, origin)
                        .is_ok_and(|target| target == key.target)
            }
            (
                LoweredArtifactPlan::ReabstractionThunk(plan),
                ArtifactRequestKey::ReabstractionThunk(key),
            ) => {
                same_function(&plan.concrete, &key.concrete)
                    && CanonicalFunctionType::dynamic(&plan.dynamic, origin)
                        .is_ok_and(|shape| shape == key.dynamic)
                    && plan.direction == key.direction
                    && plan.dictionary_slot == key.dictionary_slot
                    && (plan.dictionary_slot.is_none()
                        || plan.direction
                            == crate::specialization::ReabstractionDirection::ConcreteToDynamic)
            }
            (
                LoweredArtifactPlan::ValueConversion(plan),
                ArtifactRequestKey::ValueConversion(key),
            ) => {
                super::value_conversions::concrete_key(&plan.concrete, origin)
                    .is_ok_and(|shape| shape == key.concrete)
                    && CanonicalType::dynamic(&plan.dynamic, origin)
                        .is_ok_and(|shape| shape == key.dynamic)
                    && plan.direction == key.direction
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
                    (
                        GcFinalizerPlan::DynamicClosureEnvironment { closure, .. },
                        GcFinalizerKey::DynamicClosureEnvironment {
                            closure: expected_closure,
                        },
                    ) => closure.index() == expected_closure.index(),
                    (GcFinalizerPlan::DynamicBuffer, GcFinalizerKey::DynamicBuffer) => true,
                    (GcFinalizerPlan::DynamicCell, GcFinalizerKey::DynamicCell) => true,
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
    /// and use recorded artifact requests instead.
    pub(crate) fn supports_planned_callees(&self) -> bool {
        matches!(
            self,
            LoweredArtifactPlan::ConstructorAdapter(_)
                | LoweredArtifactPlan::StructuralMethod(_)
                | LoweredArtifactPlan::DropGlue(_)
                | LoweredArtifactPlan::TypeDescriptor(_)
                | LoweredArtifactPlan::TraitDictionary(_)
                | LoweredArtifactPlan::TraitMemberEntry(_)
                | LoweredArtifactPlan::DescriptorDrop(_)
                | LoweredArtifactPlan::CompoundDescriptorDrop(_)
                | LoweredArtifactPlan::GcFinalizer(_)
                | LoweredArtifactPlan::CoroutineCodes(_)
                | LoweredArtifactPlan::ValueConversion(_)
                | LoweredArtifactPlan::ReabstractionThunk(_)
        )
    }

    /// Whether expansion replaced the request-time marker for this family.
    /// Families without a request-time marker are complete on construction.
    pub(crate) fn is_expanded(&self) -> bool {
        match self {
            LoweredArtifactPlan::ConstructorAdapter(plan) => {
                !matches!(plan.construction, ConstructorConstruction::Unexpanded)
            }
            LoweredArtifactPlan::StructuralMethod(plan) => {
                !matches!(plan.body, StructuralBody::Unexpanded)
            }
            LoweredArtifactPlan::TypeDescriptor(plan) => plan.expanded,
            LoweredArtifactPlan::TraitDictionary(plan) => plan.expanded,
            LoweredArtifactPlan::DescriptorDrop(plan) => plan.glue.is_some(),
            LoweredArtifactPlan::DescriptorBoundEntry(_) => true,
            LoweredArtifactPlan::TraitMemberEntry(_) => true,
            LoweredArtifactPlan::DynamicStructural(_) => true,
            LoweredArtifactPlan::InstantiationAdapter(_) => true,
            LoweredArtifactPlan::ReabstractionThunk(_) => true,
            LoweredArtifactPlan::ValueConversion(_) => true,
            LoweredArtifactPlan::CompoundDescriptorDrop(_) => true,
            LoweredArtifactPlan::DropGlue(plan) => !matches!(plan.body, DropGlueBody::Unexpanded),
            LoweredArtifactPlan::GcFinalizer(plan) => match plan {
                GcFinalizerPlan::Payload { glue, .. }
                | GcFinalizerPlan::Cell { glue, .. }
                | GcFinalizerPlan::Buffer { glue, .. } => glue.is_some(),
                GcFinalizerPlan::ClosureEnvironment { drops, .. } => drops.is_some(),
                GcFinalizerPlan::DynamicClosureEnvironment { .. } => true,
                GcFinalizerPlan::DynamicBuffer => true,
                GcFinalizerPlan::DynamicCell => true,
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
                scheme: _,
                symbol: _,
                type_id: _,
                adapter: _,
                callable_type,
                construction,
                layout: _,
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
                    StructuralBody::SumDebug { alternatives }
                    | StructuralBody::ProductEquality {
                        elements: alternatives,
                        ..
                    } => {
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
            LoweredArtifactPlan::TypeDescriptor(plan) => visit(Value(&plan.value_type)),
            LoweredArtifactPlan::TraitDictionary(plan) => values(&plan.arguments, visit),
            LoweredArtifactPlan::DescriptorDrop(plan) => visit(Value(&plan.value_type)),
            LoweredArtifactPlan::CompoundDescriptorDrop(_) => {}
            LoweredArtifactPlan::DropGlue(DropGluePlan { value_type, body }) => {
                visit(Value(value_type));
                match body {
                    DropGlueBody::Unexpanded
                    | DropGlueBody::UserDrop { .. }
                    | DropGlueBody::CoroutineCleanup
                    | DropGlueBody::RuntimeRelease(_)
                    | DropGlueBody::CStringFree
                    | DropGlueBody::Wrapper { .. } => {}
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
                GcFinalizerPlan::DynamicClosureEnvironment {
                    closure: _,
                    runtime: _,
                    dictionaries: _,
                    captures: _,
                } => {}
                GcFinalizerPlan::DynamicBuffer => {}
                GcFinalizerPlan::DynamicCell => {}
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
            // The function type mentions the instance's run-time parameters by
            // design; the plan is not part of the concrete closed catalog.
            LoweredArtifactPlan::TraitMemberEntry(_) => {}
            LoweredArtifactPlan::DescriptorBoundEntry(DescriptorBoundEntryPlan {
                bound: _,
                dictionary_prefix: _,
                dictionary_count: _,
                bound_dictionaries: _,
                environment_offset: _,
                instance: _,
                runtime: _,
                function_type: _,
            }) => {}
            // A structural slot is keyed by shape alone; its component types
            // arrive through the dictionary at run time.
            LoweredArtifactPlan::DynamicStructural(DynamicStructuralPlan {
                shape: _,
                trait_id: _,
                write: _,
            }) => {}
            // The scheme and target mention the caller's run-time parameters by
            // design, like the descriptor-bound entry above.
            LoweredArtifactPlan::InstantiationAdapter(InstantiationAdapterPlan {
                scheme: _,
                target: _,
                types: _,
                dictionaries: _,
                target_dictionaries: _,
                entailment: _,
                children: _,
            }) => {}
            LoweredArtifactPlan::ReabstractionThunk(ReabstractionThunkPlan {
                dictionary_slot: _,
                concrete: _,
                dynamic: _,
                direction: _,
                children: _,
            }) => {}
            LoweredArtifactPlan::ValueConversion(_) => {}
        }
    }

    fn visit_callees<'a>(&'a self, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
        match self {
            LoweredArtifactPlan::ConstructorAdapter(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::ValueConversion(plan) => {
                for child in &plan.children {
                    visit(PlannedCalleeRef::Artifact(child));
                }
            }
            LoweredArtifactPlan::ReabstractionThunk(plan) => {
                for child in &plan.children {
                    visit(PlannedCalleeRef::Artifact(child));
                }
            }
            LoweredArtifactPlan::InstantiationAdapter(plan) => {
                for child in &plan.children {
                    visit(PlannedCalleeRef::Artifact(child));
                }
            }
            LoweredArtifactPlan::StructuralMethod(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::TraitDictionary(plan) => {
                for slot in &plan.slots {
                    visit(PlannedCalleeRef::Artifact(slot));
                }
                for prerequisite in plan.prerequisites.iter().flatten() {
                    visit(PlannedCalleeRef::Artifact(prerequisite));
                }
                for associated in &plan.associated {
                    visit(PlannedCalleeRef::Artifact(associated));
                }
                for method in &plan.methods {
                    visit(PlannedCalleeRef::Instance(method));
                }
            }
            LoweredArtifactPlan::TypeDescriptor(plan) => {
                for component in &plan.components {
                    visit(PlannedCalleeRef::Artifact(component));
                }
                if let Some(drop) = &plan.drop {
                    visit(PlannedCalleeRef::Artifact(drop));
                }
            }
            LoweredArtifactPlan::DescriptorDrop(plan) => {
                if let Some(glue) = &plan.glue {
                    visit(PlannedCalleeRef::Artifact(glue));
                }
            }
            LoweredArtifactPlan::DropGlue(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::GcFinalizer(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::CoroutineCodes(plan) => plan.visit_callees(visit),
            LoweredArtifactPlan::TraitMemberEntry(plan) => {
                if let Some(callee) = &plan.callee {
                    visit(PlannedCalleeRef::Instance(callee));
                }
            }
            LoweredArtifactPlan::DynamicStructural(plan) => {
                if let Some(write) = &plan.write {
                    visit(PlannedCalleeRef::Instance(write));
                }
            }
            LoweredArtifactPlan::CompoundDescriptorDrop(_)
            | LoweredArtifactPlan::DescriptorBoundEntry(_)
            | LoweredArtifactPlan::ReactionRunner(_)
            | LoweredArtifactPlan::UntilRunner(_)
            | LoweredArtifactPlan::DerivedRunner(_)
            | LoweredArtifactPlan::ExternAdapter(_) => {}
        }
    }

    fn visit_callees_mut<'a>(&'a mut self, visit: &mut impl FnMut(PlannedCalleeRefMut<'a>)) {
        match self {
            LoweredArtifactPlan::ConstructorAdapter(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::ValueConversion(plan) => {
                for child in &mut plan.children {
                    visit(PlannedCalleeRefMut::Artifact(child));
                }
            }
            LoweredArtifactPlan::ReabstractionThunk(plan) => {
                for child in &mut plan.children {
                    visit(PlannedCalleeRefMut::Artifact(child));
                }
            }
            LoweredArtifactPlan::InstantiationAdapter(plan) => {
                for child in &mut plan.children {
                    visit(PlannedCalleeRefMut::Artifact(child));
                }
            }
            LoweredArtifactPlan::StructuralMethod(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::TraitDictionary(plan) => {
                for slot in &mut plan.slots {
                    visit(PlannedCalleeRefMut::Artifact(slot));
                }
                for prerequisite in plan.prerequisites.iter_mut().flatten() {
                    visit(PlannedCalleeRefMut::Artifact(prerequisite));
                }
                for associated in &mut plan.associated {
                    visit(PlannedCalleeRefMut::Artifact(associated));
                }
                for method in &mut plan.methods {
                    visit(PlannedCalleeRefMut::Instance(method));
                }
            }
            LoweredArtifactPlan::TypeDescriptor(plan) => {
                for component in &mut plan.components {
                    visit(PlannedCalleeRefMut::Artifact(component));
                }
                if let Some(drop) = &mut plan.drop {
                    visit(PlannedCalleeRefMut::Artifact(drop));
                }
            }
            LoweredArtifactPlan::DescriptorDrop(plan) => {
                if let Some(glue) = &mut plan.glue {
                    visit(PlannedCalleeRefMut::Artifact(glue));
                }
            }
            LoweredArtifactPlan::DropGlue(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::GcFinalizer(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::CoroutineCodes(plan) => plan.visit_callees_mut(visit),
            LoweredArtifactPlan::TraitMemberEntry(plan) => {
                if let Some(callee) = &mut plan.callee {
                    visit(PlannedCalleeRefMut::Instance(callee));
                }
            }
            LoweredArtifactPlan::DynamicStructural(plan) => {
                if let Some(write) = &mut plan.write {
                    visit(PlannedCalleeRefMut::Instance(write));
                }
            }
            LoweredArtifactPlan::CompoundDescriptorDrop(_)
            | LoweredArtifactPlan::DescriptorBoundEntry(_)
            | LoweredArtifactPlan::ReactionRunner(_)
            | LoweredArtifactPlan::UntilRunner(_)
            | LoweredArtifactPlan::DerivedRunner(_)
            | LoweredArtifactPlan::ExternAdapter(_) => {}
        }
    }
}

/// The plan shape of a constructor value's adapter.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConstructorAdapterPlan {
    pub scheme: Option<crate::CheckedScheme>,
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
    /// Descriptor and compound-drop dependencies of a dynamic representation.
    pub layout: Vec<PlannedArtifact>,
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
    /// Private environment order for descriptors captured by a dynamic constructor.
    pub(crate) fn runtime_parameters(&self) -> Vec<crate::TypeParameterId> {
        if let Some(scheme) = &self.scheme {
            fn collect(
                parameters: &[crate::CheckedSchemeParameter],
                ids: &mut Vec<crate::TypeParameterId>,
            ) {
                for parameter in parameters {
                    match parameter {
                        crate::CheckedSchemeParameter::Type { id, .. } => ids.push(*id),
                        crate::CheckedSchemeParameter::Product(parameters) => {
                            collect(parameters, ids)
                        }
                        _ => {}
                    }
                }
            }
            let mut ids = Vec::new();
            collect(&scheme.parameters, &mut ids);
            return ids;
        }

        fn collect(ty: &CheckedType, ids: &mut std::collections::BTreeSet<crate::TypeParameterId>) {
            match ty {
                CheckedType::Parameter { id, .. } => {
                    ids.insert(*id);
                }
                CheckedType::Product(product) => {
                    for field in &product.elements {
                        collect(&field.value_type, ids);
                    }
                }
                CheckedType::Sum(sum) => {
                    for alternative in &sum.alternatives {
                        collect(alternative, ids);
                    }
                }
                CheckedType::Wrapper { representation, .. } => collect(representation, ids),
                CheckedType::Ref(ty) | CheckedType::Slice(ty) | CheckedType::Buffer(ty) => {
                    collect(ty, ids)
                }
                CheckedType::Array { element, count } => {
                    collect(element, ids);
                    collect(count, ids);
                }
                CheckedType::Function(function) => {
                    collect(&function.parameter, ids);
                    collect(&function.result, ids);
                }
                _ => {}
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        collect(&self.callable_type.parameter, &mut ids);
        collect(&self.callable_type.result, &mut ids);
        ids.into_iter().collect()
    }
    fn visit_callees<'a>(&'a self, visit: &mut impl FnMut(PlannedCalleeRef<'a>)) {
        for layout in &self.layout {
            visit(PlannedCalleeRef::Artifact(layout));
        }
        if let ConstructorConstruction::ManagedRef {
            finalizer: Some(finalizer),
            ..
        } = &self.construction
        {
            visit(PlannedCalleeRef::Artifact(finalizer));
        }
    }

    fn visit_callees_mut<'a>(&'a mut self, visit: &mut impl FnMut(PlannedCalleeRefMut<'a>)) {
        for layout in &mut self.layout {
            visit(PlannedCalleeRefMut::Artifact(layout));
        }
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
    /// Product equality: compare fields in order, stopping at the first mismatch.
    ProductEquality {
        elements: Vec<DebugDelegate>,
        /// `Eq.ne` negates the conjunction of element-level `Eq.eq` results.
        negate: bool,
    },
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
            StructuralBody::SumDebug { alternatives }
            | StructuralBody::ProductEquality {
                elements: alternatives,
                ..
            } => {
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
            StructuralBody::SumDebug { alternatives }
            | StructuralBody::ProductEquality {
                elements: alternatives,
                ..
            } => {
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
    /// The alternative type (a `Wrapper` representation).
    pub alternative: CheckedType,
    /// The concrete injection of `alternative` into the result sum, computed
    /// and revalidated during expansion like `IndexedElement::coercion_plan`.
    pub coercion_plan: super::LoweredCoercionPlan,
}

/// The plan shape of one drop-glue body: the concrete value type and the
/// ordered cleanup decision emission expands inline.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DropGluePlan {
    pub value_type: CheckedType,
    /// The owned cleanup decision. `Unexpanded` at request time.
    pub body: DropGlueBody,
}

/// One drop-glue body, mirroring the emitter decision order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DropGlueBody {
    /// Request-time only: the family expander replaces this marker.
    Unexpanded,
    /// Call the selected user `Drop` implementation on a borrowed pointer,
    /// then drop the representation when it needs drop.
    UserDrop {
        /// The selected `Drop` implementation method instance.
        method: PlannedInstance,
        /// The `Wrapper` representation's own drop glue, requested only when
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
    /// Drop the claimed `Wrapper` representation.
    Wrapper { representation: PlannedArtifact },
}

/// The runtime release one opaque-type drop performs. Expansion turns these
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
            DropGlueBody::Wrapper { representation } => {
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
            DropGlueBody::Wrapper { representation } => {
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
    /// A dynamic buffer: drops the elements through the element
    /// descriptor in the allocation's trailing word.
    DynamicBuffer,
    /// A dynamic shared cell: drops the value through the descriptor in
    /// the allocation's trailing word.
    DynamicCell,
    /// A dynamic closure environment: drops each owned capture through
    /// the descriptors the environment holds.
    DynamicClosureEnvironment {
        /// The closure's own function instance; its position is the key's
        /// instance ordinal.
        closure: FunctionInstanceId,
        /// The instance's run-time parameters in descriptor ABI order.
        runtime: Vec<crate::TypeParameterId>,
        /// Dictionaries stored after the descriptors, before the
        /// capture descriptors.
        dictionaries: usize,
        /// The captures in environment order.
        captures: Vec<DynamicCapture>,
    },
}

/// One capture of a dynamic closure environment.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DynamicCapture {
    pub value_type: CheckedType,
    pub access: crate::LoweredCaptureAccess,
    /// The finalizer drops the capture.
    pub owns: bool,
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
            GcFinalizerPlan::DynamicClosureEnvironment { .. }
            | GcFinalizerPlan::DynamicBuffer
            | GcFinalizerPlan::DynamicCell => {}
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
            GcFinalizerPlan::DynamicClosureEnvironment { .. }
            | GcFinalizerPlan::DynamicBuffer
            | GcFinalizerPlan::DynamicCell => {}
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

/// The declaration parity facts of one extern adapter, recorded so emission can
/// keep the emitter's eager foreign-symbol declaration while emitting the adapter
/// body only for a reachable artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExternDeclaration {
    /// The C symbol's declared arity, used by its callable adapter.
    pub arity: usize,
    /// The emitter declares the foreign symbol and creates this adapter eagerly for
    /// every non-variadic extern binding, used or not. The artifact records
    /// which adapters a callable-value site actually reaches.
    pub eagerly_declared: bool,
}

/// A demand-driven static descriptor. Layout is target-specific and stays in codegen.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TypeDescriptorPlan {
    pub value_type: CheckedType,
    pub components: Vec<PlannedArtifact>,
    pub drop: Option<PlannedArtifact>,
    pub expanded: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DescriptorDropPlan {
    pub value_type: CheckedType,
    pub glue: Option<PlannedArtifact>,
}
