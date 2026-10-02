//! Canonical structural keys for Stage 3 specialization.
//!
//! Stage 3.1 defines the owned, typed identity of source-function instances
//! and of the constructor-adapter and structural-method artifacts Stage 4
//! materializes. Keys are built from semantic IDs and structural checked data
//! only: display names, source spans, contextual defaults, and expanded
//! nominal representations never participate in equality. The worklist that
//! consumes these keys belongs to Stage 3.3.

use std::collections::{HashMap, HashSet};

use staple_syntax::Diagnostic;

use crate::{
    CheckedEffectSet, CheckedFunctionType, CheckedMutation, CheckedResource, CheckedStateEffect,
    CheckedType, FunctionId, InitializerId, LoweredCallableAdapter, LoweredReactiveCallbackId,
    LoweredReactiveOperationId, Origin, StructuralTraitMethod, SymbolId, TraitEvidence, TraitId,
    TraitMethodId, TypeId, TypeParameterId,
};

/// One canonical structural type. Every vector preserves the checked semantic
/// order (field order, alternative order, resource order, argument order);
/// nothing is re-sorted or normalized here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalType {
    Never,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    ISize,
    USize,
    F32,
    F64,
    NumberLiteral(u64),
    String,
    StringLiteralSet(Vec<String>),
    Ref(Box<CanonicalType>),
    Slice(Box<CanonicalType>),
    Buffer(Box<CanonicalType>),
    Array {
        element: Box<CanonicalType>,
        count: Box<CanonicalType>,
    },
    CString,
    CChar,
    Nominal {
        kind: CanonicalNominalKind,
        id: TypeId,
        arguments: Vec<CanonicalType>,
    },
    CPointer {
        pointee: Box<CanonicalType>,
    },
    Product {
        elements: Vec<CanonicalProductElement>,
        variadic: bool,
    },
    Sum {
        alternatives: Vec<CanonicalType>,
    },
    Function(Box<CanonicalFunctionType>),
}

/// The declared nominal form. Kept distinct for the same `TypeId` because the
/// checker's own equality treats `TypeConstructor`, `Opaque`, and `Distinct`
/// as different types even when an alias or representation expansion would
/// display them alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalNominalKind {
    TypeConstructor,
    Opaque,
    Distinct,
}

/// One product element: the declared field name (which the checker keeps as
/// part of product identity) and its canonical value type. Contextual
/// construction defaults are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalProductElement {
    pub name: Option<String>,
    pub value_type: CanonicalType,
}

/// A canonical function shape. `CheckedFunctionType::default` is a source
/// expression and never participates in identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalFunctionType {
    pub parameter: Box<CanonicalType>,
    pub parameter_style: CanonicalParameterStyle,
    pub mutations: Vec<CanonicalMutation>,
    pub moves: Vec<CanonicalMutation>,
    pub effects: CanonicalEffectSet,
    pub result: Box<CanonicalType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalParameterStyle {
    Single,
    Juxtaposed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum CanonicalMutation {
    Whole,
    Element(usize),
}

/// A canonical concrete effect row. Conversion rejects an unsubstituted
/// effect variable, so a key's row is always fully resolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalEffectSet {
    pub resources: Vec<CanonicalResource>,
    pub state: Option<CanonicalStateEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalResource {
    pub value_type: CanonicalType,
    pub mutable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalStateEffect {
    Read,
    Write,
    ReadWrite,
}

impl CanonicalType {
    /// Converts a checked type into a concrete key, rejecting unresolved
    /// parameters and checker placeholders with a diagnostic at the
    /// requesting record's origin.
    pub(crate) fn concrete(value_type: &CheckedType, origin: &Origin) -> Result<Self, Diagnostic> {
        Self::convert(value_type, origin)
    }

    fn convert(value_type: &CheckedType, origin: &Origin) -> Result<Self, Diagnostic> {
        let key = match value_type {
            CheckedType::Inferred => {
                return Err(origin_diagnostic(
                    origin,
                    "cannot form a specialization key from an inferred type",
                ));
            }
            CheckedType::Error => {
                return Err(origin_diagnostic(
                    origin,
                    "cannot form a specialization key from an error type",
                ));
            }
            CheckedType::Never => CanonicalType::Never,
            CheckedType::I8 => CanonicalType::I8,
            CheckedType::I16 => CanonicalType::I16,
            CheckedType::I32 => CanonicalType::I32,
            CheckedType::I64 => CanonicalType::I64,
            CheckedType::U8 => CanonicalType::U8,
            CheckedType::U16 => CanonicalType::U16,
            CheckedType::U32 => CanonicalType::U32,
            CheckedType::U64 => CanonicalType::U64,
            CheckedType::ISize => CanonicalType::ISize,
            CheckedType::USize => CanonicalType::USize,
            CheckedType::F32 => CanonicalType::F32,
            CheckedType::F64 => CanonicalType::F64,
            CheckedType::NumberLiteral(value) => CanonicalType::NumberLiteral(*value),
            CheckedType::String => CanonicalType::String,
            CheckedType::StringLiteralSet(values) => {
                CanonicalType::StringLiteralSet(values.clone())
            }
            CheckedType::Ref(payload) => {
                CanonicalType::Ref(Box::new(Self::convert(payload, origin)?))
            }
            CheckedType::Slice(payload) => {
                CanonicalType::Slice(Box::new(Self::convert(payload, origin)?))
            }
            CheckedType::Buffer(payload) => {
                CanonicalType::Buffer(Box::new(Self::convert(payload, origin)?))
            }
            CheckedType::Array { element, count } => CanonicalType::Array {
                element: Box::new(Self::convert(element, origin)?),
                count: Box::new(Self::convert(count, origin)?),
            },
            CheckedType::CString => CanonicalType::CString,
            CheckedType::CChar => CanonicalType::CChar,
            CheckedType::Parameter { name, .. } => {
                return Err(origin_diagnostic(
                    origin,
                    format!("type parameter `{name}` is not resolved for a concrete key"),
                ));
            }
            CheckedType::TypeConstructor { id, arguments, .. } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::TypeConstructor,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin)?,
            },
            CheckedType::Opaque { id, arguments, .. } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::Opaque,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin)?,
            },
            CheckedType::CPointer { pointee } => CanonicalType::CPointer {
                pointee: Box::new(Self::convert(pointee, origin)?),
            },
            CheckedType::Product(product) => CanonicalType::Product {
                elements: product
                    .elements
                    .iter()
                    .map(|element| {
                        Ok(CanonicalProductElement {
                            name: element.name.clone(),
                            value_type: Self::convert(&element.value_type, origin)?,
                        })
                    })
                    .collect::<Result<Vec<_>, Diagnostic>>()?,
                variadic: product.variadic,
            },
            CheckedType::Sum(sum) => CanonicalType::Sum {
                alternatives: Self::convert_arguments(&sum.alternatives, origin)?,
            },
            CheckedType::Function(function) => {
                // A concrete effect row is encoded as a function type with
                // `Error` parameter and result (see
                // `effect_substitution_type`), and coroutine/task type
                // arguments carry that encoding. Canonicalize it as a
                // never-parameter marker that keeps the canonical effect row,
                // so a `Coroutine{E} T` argument list stays part of the key
                // instead of rejecting the whole type as an error.
                if function.parameter.as_ref() == &CheckedType::Error
                    && function.result.as_ref() == &CheckedType::Error
                {
                    let marker = CheckedFunctionType {
                        parameter: Box::new(CheckedType::Never),
                        result: Box::new(CheckedType::Never),
                        ..function.clone()
                    };
                    CanonicalType::Function(Box::new(CanonicalFunctionType::convert(
                        &marker, origin,
                    )?))
                } else {
                    CanonicalType::Function(Box::new(CanonicalFunctionType::convert(
                        function, origin,
                    )?))
                }
            }
            CheckedType::Distinct {
                id,
                arguments,
                representation: _,
                ..
            } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::Distinct,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin)?,
            },
        };
        Ok(key)
    }

    fn convert_arguments(
        arguments: &[CheckedType],
        origin: &Origin,
    ) -> Result<Vec<CanonicalType>, Diagnostic> {
        arguments
            .iter()
            .map(|argument| Self::convert(argument, origin))
            .collect()
    }
}

impl CanonicalType {}

impl CanonicalFunctionType {
    pub(crate) fn concrete(
        function: &CheckedFunctionType,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(function, origin)
    }

    fn convert(function: &CheckedFunctionType, origin: &Origin) -> Result<Self, Diagnostic> {
        Ok(CanonicalFunctionType {
            parameter: Box::new(CanonicalType::convert(&function.parameter, origin)?),
            parameter_style: match function.parameter_style {
                staple_syntax::FunctionParameterStyle::Single => CanonicalParameterStyle::Single,
                staple_syntax::FunctionParameterStyle::Juxtaposed => {
                    CanonicalParameterStyle::Juxtaposed
                }
            },
            mutations: function.mutations.iter().cloned().map(Into::into).collect(),
            moves: function.moves.iter().cloned().map(Into::into).collect(),
            effects: CanonicalEffectSet::convert(&function.effects, origin)?,
            result: Box::new(CanonicalType::convert(&function.result, origin)?),
        })
    }
}

impl CanonicalEffectSet {
    pub(crate) fn concrete(
        effects: &CheckedEffectSet,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(effects, origin)
    }

    fn convert(effects: &CheckedEffectSet, origin: &Origin) -> Result<Self, Diagnostic> {
        if let Some(variable) = &effects.variable {
            return Err(origin_diagnostic(
                origin,
                format!(
                    "effect variable `{}` is not resolved for a concrete key",
                    variable.name
                ),
            ));
        }
        Ok(CanonicalEffectSet {
            resources: effects
                .resources
                .iter()
                .map(|resource| CanonicalResource::convert(resource, origin))
                .collect::<Result<Vec<_>, Diagnostic>>()?,
            state: effects.state.map(Into::into),
        })
    }
}

impl CanonicalResource {
    fn convert(resource: &CheckedResource, origin: &Origin) -> Result<Self, Diagnostic> {
        Ok(CanonicalResource {
            value_type: CanonicalType::convert(&resource.value_type, origin)?,
            mutable: resource.mutable,
        })
    }
}

impl From<CheckedMutation> for CanonicalMutation {
    fn from(mutation: CheckedMutation) -> Self {
        match mutation {
            CheckedMutation::Whole => CanonicalMutation::Whole,
            CheckedMutation::Element(index) => CanonicalMutation::Element(index),
        }
    }
}

impl From<CheckedStateEffect> for CanonicalStateEffect {
    fn from(state: CheckedStateEffect) -> Self {
        match state {
            CheckedStateEffect::Read => CanonicalStateEffect::Read,
            CheckedStateEffect::Write => CanonicalStateEffect::Write,
            CheckedStateEffect::ReadWrite => CanonicalStateEffect::ReadWrite,
        }
    }
}

/// One ordered substitution entry of an instance key. `Effect` entries carry
/// the whole canonical row for a declared effect parameter.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum InstanceSubstitution {
    Type {
        parameter: TypeParameterId,
        value: CanonicalType,
    },
    Effect {
        parameter: TypeParameterId,
        effects: CanonicalEffectSet,
    },
}

impl InstanceSubstitution {
    pub(crate) fn parameter(&self) -> TypeParameterId {
        match self {
            InstanceSubstitution::Type { parameter, .. }
            | InstanceSubstitution::Effect { parameter, .. } => *parameter,
        }
    }
}

/// The identity of one source-function instance: the template function plus
/// ordered, sorted, conflict-free substitution entries and, when body selection
/// depends on it, canonical trait evidence.
///
/// Equality inputs are fixed here; which parameters are *relevant* to a given
/// template is collected by Stage 3.2 and only affects which entries callers
/// add. A function with no relevant parameters or evidence has exactly one key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct InstanceKey {
    function: FunctionId,
    substitutions: Vec<InstanceSubstitution>,
    evidence: Option<CanonicalEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InstanceKeyError {
    /// The same parameter was given twice.
    DuplicateParameter(TypeParameterId),
    /// A parameter appeared as both a type and an effect entry.
    ConflictingParameter(TypeParameterId),
}

impl InstanceKeyError {
    pub(crate) fn message(self) -> String {
        match self {
            InstanceKeyError::DuplicateParameter(parameter) => format!(
                "instance key repeats the substitution for parameter {}",
                parameter.0
            ),
            InstanceKeyError::ConflictingParameter(parameter) => format!(
                "instance key has both a type and an effect substitution for parameter {}",
                parameter.0
            ),
        }
    }
}

impl InstanceKey {
    /// Builds a concrete key. Entries are stably sorted by parameter ID, and
    /// duplicate, conflicting, or unresolved entries are rejected: an
    /// unresolved request never masquerades as a concrete key.
    pub(crate) fn new(
        function: FunctionId,
        mut substitutions: Vec<InstanceSubstitution>,
        evidence: Option<CanonicalEvidence>,
    ) -> Result<Self, InstanceKeyError> {
        substitutions.sort_by_key(|substitution| substitution.parameter().0);
        for pair in substitutions.windows(2) {
            if pair[0].parameter() == pair[1].parameter() {
                let same_kind = matches!(
                    (&pair[0], &pair[1]),
                    (
                        InstanceSubstitution::Type { .. },
                        InstanceSubstitution::Type { .. }
                    ) | (
                        InstanceSubstitution::Effect { .. },
                        InstanceSubstitution::Effect { .. }
                    )
                );
                return Err(if same_kind {
                    InstanceKeyError::DuplicateParameter(pair[0].parameter())
                } else {
                    InstanceKeyError::ConflictingParameter(pair[0].parameter())
                });
            }
        }
        Ok(InstanceKey {
            function,
            substitutions,
            evidence,
        })
    }

    pub(crate) fn function(&self) -> FunctionId {
        self.function
    }

    pub(crate) fn substitutions(&self) -> &[InstanceSubstitution] {
        &self.substitutions
    }

    pub(crate) fn evidence(&self) -> Option<&CanonicalEvidence> {
        self.evidence.as_ref()
    }
}

/// Canonical trait evidence. Only resolved selections appear here; declared
/// bounds and negative obligations stay in `InstanceRequest` until Stage 3.2
/// replaces them with a concrete selection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalEvidence {
    /// A selected explicit implementation method. The selected method
    /// `FunctionId` is unique per implementation method and is what keeps two
    /// implementations distinct even when their signatures match.
    ExplicitImplementation {
        trait_id: TraitId,
        method: TraitMethodId,
        function: FunctionId,
        arguments: Vec<CanonicalType>,
    },
    /// A selected compiler-generated structural method.
    Structural {
        trait_id: TraitId,
        method: TraitMethodId,
        structural: StructuralTraitMethod,
        arguments: Vec<CanonicalType>,
    },
}

impl CanonicalEvidence {}

/// Converts resolved trait evidence into its canonical key form. Declared
/// bounds and negative obligations never reach a key.
pub(crate) fn canonical_evidence(
    evidence: &TraitEvidence,
    origin: &Origin,
) -> Result<CanonicalEvidence, Diagnostic> {
    match evidence {
        TraitEvidence::ExplicitImplementation {
            trait_id,
            method,
            function,
            arguments,
            ..
        } => Ok(CanonicalEvidence::ExplicitImplementation {
            trait_id: *trait_id,
            method: *method,
            function: *function,
            arguments: canonical_arguments(arguments, origin)?,
        }),
        TraitEvidence::Structural {
            trait_id,
            method,
            structural,
            arguments,
        } => Ok(CanonicalEvidence::Structural {
            trait_id: *trait_id,
            method: *method,
            structural: *structural,
            arguments: canonical_arguments(arguments, origin)?,
        }),
        TraitEvidence::DeclaredBound { trait_id, .. } => Err(origin_diagnostic(
            origin,
            format!(
                "trait {} evidence is still a declared bound; Stage 3.2 must resolve it before an instance key exists",
                trait_id.0
            ),
        )),
    }
}

fn canonical_arguments(
    arguments: &[CheckedType],
    origin: &Origin,
) -> Result<Vec<CanonicalType>, Diagnostic> {
    arguments
        .iter()
        .map(|argument| CanonicalType::concrete(argument, origin))
        .collect()
}

/// The canonical adapter a callable value needs. Mirrors
/// `LoweredCallableAdapter` so artifact keys never depend on the lowered
/// record's own enum ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalAdapterKind {
    None,
    Constructor,
    External,
    NestedClosure,
}

impl From<LoweredCallableAdapter> for CanonicalAdapterKind {
    fn from(adapter: LoweredCallableAdapter) -> Self {
        match adapter {
            LoweredCallableAdapter::None => CanonicalAdapterKind::None,
            LoweredCallableAdapter::Constructor => CanonicalAdapterKind::Constructor,
            LoweredCallableAdapter::External => CanonicalAdapterKind::External,
            LoweredCallableAdapter::NestedClosure => CanonicalAdapterKind::NestedClosure,
        }
    }
}

/// The identity of a generated constructor-adapter body: the constructor's
/// semantic symbol and nominal type, the adapter kind, and the concrete
/// callable type the adapter must expose.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConstructorAdapterKey {
    pub symbol: SymbolId,
    pub type_id: TypeId,
    pub adapter: CanonicalAdapterKind,
    pub callable_type: CanonicalFunctionType,
}

impl ConstructorAdapterKey {
    pub(crate) fn new(
        symbol: SymbolId,
        type_id: TypeId,
        adapter: LoweredCallableAdapter,
        callable_type: &CheckedFunctionType,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Ok(ConstructorAdapterKey {
            symbol,
            type_id,
            adapter: adapter.into(),
            callable_type: CanonicalFunctionType::concrete(callable_type, origin)?,
        })
    }
}

/// The identity of a generated structural-method body. The legacy backend
/// cached `(StructuralTraitMethod, Debug arguments)`: for the seven current
/// methods the trait, method, and callable type are derivable from the
/// structural kind plus completed arguments, but this key keeps them explicit
/// so Stage 4 never depends on that derivability.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct StructuralMethodKey {
    pub structural: StructuralTraitMethod,
    pub trait_id: TraitId,
    pub method: TraitMethodId,
    pub arguments: Vec<CanonicalType>,
    pub callable_type: CanonicalFunctionType,
}

impl StructuralMethodKey {
    pub(crate) fn new(
        structural: StructuralTraitMethod,
        trait_id: TraitId,
        method: TraitMethodId,
        arguments: &[CheckedType],
        callable_type: &CheckedFunctionType,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Ok(StructuralMethodKey {
            structural,
            trait_id,
            method,
            arguments: arguments
                .iter()
                .map(|argument| CanonicalType::concrete(argument, origin))
                .collect::<Result<Vec<_>, Diagnostic>>()?,
            callable_type: CanonicalFunctionType::concrete(callable_type, origin)?,
        })
    }
}

/// The owning lowered record of a per-site generated artifact. Identity uses
/// the owner's dense lowered position, never a display name or a hash, so the
/// same lowered site in two instantiations of one generic template produces
/// two distinct artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ArtifactSiteOwner {
    /// A module initializer body.
    Initializer(InitializerId),
    /// A materialized source-function instance.
    Instance(InstanceOrdinal),
}

/// The identity of one drop-glue plan for a concrete value type. Type-keyed
/// artifacts deduplicate structurally equal concrete types across owners.
pub(crate) type DropGlueKey = CanonicalType;

/// The identity of one garbage-collector finalizer body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum GcFinalizerKey {
    /// A managed `Ref` allocation's payload type.
    Payload(CanonicalType),
    /// A captured binding cell's value type.
    Cell(CanonicalType),
    /// A closure environment keyed by the closure's own function instance and
    /// its ordered concrete capture types; `active_type_substitutions` never
    /// participates in identity.
    ClosureEnvironment {
        closure: InstanceOrdinal,
        captures: Vec<CanonicalType>,
    },
    /// A buffer keyed by its element type.
    Buffer(CanonicalType),
}

/// The identity of one coroutine body's `resume`/`cleanup` pair, keyed by the
/// body thunk's function instance. Two instantiations of one generic enclosing
/// function get two pairs instead of aliasing through the body syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CoroutineCodesKey {
    pub body: InstanceOrdinal,
}

/// The lowered site of a per-site artifact inside its owner. Sites are lowered
/// arena positions, never source `SyntaxId`s; the owner disambiguates which
/// body's arena a position indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ArtifactSite {
    /// A lowered reactive callback record (reaction, batch, or `until`
    /// predicate).
    Callback(LoweredReactiveCallbackId),
    /// A lowered reactive operation record (derived creation).
    Operation(LoweredReactiveOperationId),
}

/// The identity of one reactive runner body (reaction, `until`, or derived),
/// keyed by the owning record and the lowered reactive-operation/binding site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ReactiveRunnerKey {
    pub owner: ArtifactSiteOwner,
    pub site: ArtifactSite,
}

/// The identity of one closure adapter for a non-variadic extern binding,
/// keyed by the extern symbol and the concrete callable type it exposes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ExternAdapterKey {
    pub symbol: SymbolId,
    pub callable_type: CanonicalFunctionType,
}

/// Generated-artifact request keys. The variant is the namespace: keys from
/// different families can never compare or hash equal, even when their numeric
/// IDs coincide. `RuntimeHelper` is deliberately absent: runtime symbols have
/// fixed names and no lowered bodies, so Stage 4 records them as a separate
/// ordered requirement set rather than an artifact family.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ArtifactRequestKey {
    ConstructorAdapter(ConstructorAdapterKey),
    StructuralMethod(StructuralMethodKey),
    DropGlue(DropGlueKey),
    GcFinalizer(GcFinalizerKey),
    CoroutineCodes(CoroutineCodesKey),
    ReactionRunner(ReactiveRunnerKey),
    UntilRunner(ReactiveRunnerKey),
    DerivedRunner(ReactiveRunnerKey),
    ExternAdapter(ExternAdapterKey),
}

impl ArtifactRequestKey {
    /// The stable family name, used by validation, snapshots, and name plans.
    /// The exhaustive match forces every new family to declare its identity.
    pub(crate) fn family_name(&self) -> &'static str {
        match self {
            ArtifactRequestKey::ConstructorAdapter(_) => "constructor-adapter",
            ArtifactRequestKey::StructuralMethod(_) => "structural-method",
            ArtifactRequestKey::DropGlue(_) => "drop-glue",
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(_)) => "gc-finalizer-payload",
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Cell(_)) => "gc-finalizer-cell",
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment { .. }) => {
                "gc-finalizer-closure-environment"
            }
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Buffer(_)) => "gc-finalizer-buffer",
            ArtifactRequestKey::CoroutineCodes(_) => "coroutine-codes",
            ArtifactRequestKey::ReactionRunner(_) => "reaction-runner",
            ArtifactRequestKey::UntilRunner(_) => "until-runner",
            ArtifactRequestKey::DerivedRunner(_) => "derived-runner",
            ArtifactRequestKey::ExternAdapter(_) => "extern-adapter",
        }
    }
}

/// Append-only positions within their own key families. Distinct types keep
/// a source-function instance ID from being used to look up an artifact (or
/// the reverse), even when both families have the same numeric position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct InstanceOrdinal(usize);

impl InstanceOrdinal {
    pub(crate) fn index(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ArtifactOrdinal(usize);

impl ArtifactOrdinal {
    pub(crate) fn index(self) -> usize {
        self.0
    }
}

/// A defensive name-collision report. The ordinal-based naming scheme cannot
/// currently collide, but the catalog verifies it instead of assuming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpecializationNameCollision {
    pub name: String,
}

/// The Stage 3.3 deterministic catalog: interns instance and artifact keys in
/// first-discovery order and plans stable emitted symbol names.
///
/// Stage 3.3 must call `reserve_instance` before visiting an instance body so
/// self-recursion and mutual recursion reuse the already-reserved ordinal
/// instead of interning a second one. `planned_names` is the emission order
/// contract: family order (`instances`, then `artifacts`) followed by ordinal.
/// Names depend only on the ordinal and the structural kind, never on
/// `HashMap` iteration, `Debug` output, or a hash value.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpecializationCatalog {
    instances: Vec<InstanceKey>,
    instance_lookup: HashMap<InstanceKey, usize>,
    artifacts: Vec<ArtifactRequestKey>,
    artifact_lookup: HashMap<ArtifactRequestKey, usize>,
}

impl SpecializationCatalog {
    pub(crate) fn reserve_instance(&mut self, key: InstanceKey) -> InstanceOrdinal {
        if let Some(ordinal) = self.instance_lookup.get(&key) {
            return InstanceOrdinal(*ordinal);
        }
        let ordinal = InstanceOrdinal(self.instances.len());
        self.instance_lookup.insert(key.clone(), ordinal.0);
        self.instances.push(key);
        ordinal
    }

    pub(crate) fn reserve_artifact(&mut self, key: ArtifactRequestKey) -> ArtifactOrdinal {
        if let Some(ordinal) = self.artifact_lookup.get(&key) {
            return ArtifactOrdinal(*ordinal);
        }
        let ordinal = ArtifactOrdinal(self.artifacts.len());
        self.artifact_lookup.insert(key.clone(), ordinal.0);
        self.artifacts.push(key);
        ordinal
    }

    pub(crate) fn instance(&self, ordinal: InstanceOrdinal) -> Option<&InstanceKey> {
        self.instances.get(ordinal.0)
    }

    /// The ordinal already interned for a key, when it was reserved. Stage 3.4
    /// uses this to bind a re-resolved site to the Stage 3.3 instance and
    /// must not introduce a second identity for the same key.
    pub(crate) fn instance_ordinal(&self, key: &InstanceKey) -> Option<InstanceOrdinal> {
        self.instance_lookup
            .get(key)
            .map(|ordinal| InstanceOrdinal(*ordinal))
    }

    pub(crate) fn artifact(&self, ordinal: ArtifactOrdinal) -> Option<&ArtifactRequestKey> {
        self.artifacts.get(ordinal.0)
    }

    /// The ordinal already interned for a generated-artifact key.
    pub(crate) fn artifact_ordinal(&self, key: &ArtifactRequestKey) -> Option<ArtifactOrdinal> {
        self.artifact_lookup
            .get(key)
            .map(|ordinal| ArtifactOrdinal(*ordinal))
    }

    pub(crate) fn instances(&self) -> impl Iterator<Item = (InstanceOrdinal, &InstanceKey)> {
        self.instances
            .iter()
            .enumerate()
            .map(|(index, key)| (InstanceOrdinal(index), key))
    }

    pub(crate) fn artifacts(&self) -> impl Iterator<Item = (ArtifactOrdinal, &ArtifactRequestKey)> {
        self.artifacts
            .iter()
            .enumerate()
            .map(|(index, key)| (ArtifactOrdinal(index), key))
    }

    #[cfg(test)]
    /// The planned emitted symbol names in emission order. Names are
    /// collision-checked: a repeated name is reported instead of silently
    /// aliasing two semantic keys.
    pub(crate) fn planned_names(&self) -> Result<Vec<String>, SpecializationNameCollision> {
        self.planned_names_with(|_| None)
    }

    /// The planned emitted symbol names in emission order, with the D2
    /// declared-name rule: an instance of a non-generic template (empty
    /// substitutions and evidence) keeps its declared mangled name, supplied
    /// by `declared`. Two distinct templates can share a declared name (for
    /// example two nested functions named `first`), so a declared name is
    /// used only when it is still free; the later instance deterministically
    /// falls back to `__staple_instance_{ordinal}`. Every other instance
    /// keeps its ordinal name. All families are collision-checked together,
    /// and only a name that repeats after fallback is reported.
    pub(crate) fn planned_names_with(
        &self,
        declared: impl Fn(&InstanceKey) -> Option<String>,
    ) -> Result<Vec<String>, SpecializationNameCollision> {
        let mut names = Vec::with_capacity(self.instances.len() + self.artifacts.len());
        let mut seen = HashSet::new();
        for (ordinal, key) in self.instances() {
            let name = self.planned_instance_name(ordinal, key, &declared, &seen);
            if !seen.insert(name.clone()) {
                return Err(SpecializationNameCollision { name });
            }
            names.push(name);
        }
        for (ordinal, key) in self.artifacts() {
            let name = artifact_ordinal_name(ordinal, key);
            if !seen.insert(name.clone()) {
                return Err(SpecializationNameCollision { name });
            }
            // A coroutine pair is emitted as two functions, `{name}_resume` and
            // `{name}_cleanup` (Stage 5.3 F3). Both are planned names: reserve
            // them here so they collide with any instance or artifact name and
            // the backend can read them instead of building unchecked names.
            if matches!(key, ArtifactRequestKey::CoroutineCodes(_)) {
                for pair in [format!("{name}_resume"), format!("{name}_cleanup")] {
                    if !seen.insert(pair.clone()) {
                        return Err(SpecializationNameCollision { name: pair });
                    }
                }
            }
            names.push(name);
        }
        Ok(names)
    }

    /// The planned name of one instance, given the names already assigned in
    /// catalog order: the declared mangled name for a non-generic template
    /// instance when it is still free, otherwise the ordinal name.
    fn planned_instance_name(
        &self,
        ordinal: InstanceOrdinal,
        key: &InstanceKey,
        declared: &impl Fn(&InstanceKey) -> Option<String>,
        seen: &HashSet<String>,
    ) -> String {
        if key.substitutions().is_empty() && key.evidence().is_none() {
            if let Some(candidate) = declared(key) {
                if !candidate.is_empty() && !seen.contains(&candidate) {
                    return candidate;
                }
            }
        }
        instance_ordinal_name(ordinal)
    }
}

fn instance_ordinal_name(ordinal: InstanceOrdinal) -> String {
    format!("__staple_instance_{}", ordinal.0)
}

fn artifact_ordinal_name(ordinal: ArtifactOrdinal, key: &ArtifactRequestKey) -> String {
    format!("{}_{}", artifact_name_prefix(key), ordinal.0)
}

/// The stable family prefix of one artifact's planned emitted symbol. Every
/// prefix is distinct so an ordinal-based name can never alias another family.
fn artifact_name_prefix(key: &ArtifactRequestKey) -> String {
    match key {
        ArtifactRequestKey::ConstructorAdapter(_) => "__staple_constructor_adapter".to_owned(),
        ArtifactRequestKey::StructuralMethod(method) => {
            format!("__staple_structural_{}", structural_name(method.structural))
        }
        ArtifactRequestKey::DropGlue(_) => "__staple_drop_glue".to_owned(),
        ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(_)) => {
            "__staple_gc_finalizer_payload".to_owned()
        }
        ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Cell(_)) => {
            "__staple_gc_finalizer_cell".to_owned()
        }
        ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment { .. }) => {
            "__staple_gc_finalizer_closure".to_owned()
        }
        ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Buffer(_)) => {
            "__staple_gc_finalizer_buffer".to_owned()
        }
        ArtifactRequestKey::CoroutineCodes(_) => "__staple_coroutine_codes".to_owned(),
        ArtifactRequestKey::ReactionRunner(_) => "__staple_reaction_runner".to_owned(),
        ArtifactRequestKey::UntilRunner(_) => "__staple_until_runner".to_owned(),
        ArtifactRequestKey::DerivedRunner(_) => "__staple_derived_runner".to_owned(),
        ArtifactRequestKey::ExternAdapter(_) => "__staple_extern_adapter".to_owned(),
    }
}

fn structural_name(structural: StructuralTraitMethod) -> &'static str {
    match structural {
        StructuralTraitMethod::Debug => "debug",
        StructuralTraitMethod::Index => "index",
        StructuralTraitMethod::DerefIndex => "deref_index",
        StructuralTraitMethod::MutateIndex => "mutate_index",
        StructuralTraitMethod::DerefMutateIndex => "deref_mutate_index",
        StructuralTraitMethod::IntoIterator => "into_iterator",
        StructuralTraitMethod::Iterator => "iterator",
    }
}

fn origin_diagnostic(origin: &Origin, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(origin.span.clone(), message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use staple_syntax::{
        Expression, FunctionParameterStyle, IntegerExpression, Span, Syntax, SyntaxId,
    };

    use crate::{
        CheckedFunctionParameterDefault, CheckedProductType, CheckedSumType, CheckedTypeElement,
    };

    fn requesting_origin() -> Origin {
        Origin {
            syntax: SyntaxId(99),
            span: Span::from(10..20),
        }
    }

    fn nominal(id: usize, name: &str) -> CheckedType {
        CheckedType::TypeConstructor {
            id: TypeId(id),
            name: name.to_owned(),
            arguments: Vec::new(),
        }
    }

    fn opaque(id: usize, name: &str) -> CheckedType {
        CheckedType::Opaque {
            id: TypeId(id),
            name: name.to_owned(),
            arguments: Vec::new(),
        }
    }

    fn distinct(
        id: usize,
        name: &str,
        arguments: Vec<CheckedType>,
        representation: CheckedType,
    ) -> CheckedType {
        CheckedType::Distinct {
            id: TypeId(id),
            name: name.to_owned(),
            arguments,
            representation: Box::new(representation),
        }
    }

    fn element(
        name: Option<&str>,
        value_type: CheckedType,
        default: Option<Expression>,
    ) -> CheckedTypeElement {
        CheckedTypeElement {
            name: name.map(str::to_owned),
            value_type,
            default,
        }
    }

    fn integer_default(literal: &str) -> Expression {
        Expression::Integer(IntegerExpression {
            syntax: Syntax::compiler(),
            literal: literal.to_owned(),
        })
    }

    fn function_type(
        parameter: CheckedType,
        parameter_style: FunctionParameterStyle,
        mutations: Vec<CheckedMutation>,
        moves: Vec<CheckedMutation>,
        effects: CheckedEffectSet,
        result: CheckedType,
    ) -> CheckedFunctionType {
        CheckedFunctionType {
            parameter: Box::new(parameter),
            parameter_style,
            default: None,
            mutations,
            moves,
            effects,
            result: Box::new(result),
        }
    }

    fn concrete(value_type: &CheckedType) -> CanonicalType {
        CanonicalType::concrete(value_type, &requesting_origin())
            .expect("concrete conversion should succeed")
    }

    fn canonical_function(function: &CheckedFunctionType) -> CanonicalFunctionType {
        CanonicalFunctionType::concrete(function, &requesting_origin())
            .expect("concrete function conversion should succeed")
    }

    fn node_count(value_type: &CanonicalType) -> usize {
        match value_type {
            CanonicalType::Ref(payload)
            | CanonicalType::Slice(payload)
            | CanonicalType::Buffer(payload)
            | CanonicalType::CPointer { pointee: payload } => 1 + node_count(payload),
            CanonicalType::Array { element, count } => 1 + node_count(element) + node_count(count),
            CanonicalType::Nominal { arguments, .. } => {
                1 + arguments.iter().map(node_count).sum::<usize>()
            }
            CanonicalType::Product { elements, .. } => {
                1 + elements
                    .iter()
                    .map(|element| node_count(&element.value_type))
                    .sum::<usize>()
            }
            CanonicalType::Sum { alternatives } => {
                1 + alternatives.iter().map(node_count).sum::<usize>()
            }
            CanonicalType::Function(function) => {
                1 + node_count(&function.parameter)
                    + node_count(&function.result)
                    + function
                        .effects
                        .resources
                        .iter()
                        .map(|resource| node_count(&resource.value_type))
                        .sum::<usize>()
            }
            _ => 1,
        }
    }

    #[test]
    fn equivalent_checked_types_deduplicate() {
        assert_eq!(
            concrete(&nominal(7, "First")),
            concrete(&nominal(7, "Second"))
        );
        assert_eq!(
            concrete(&opaque(8, "First")),
            concrete(&opaque(8, "Second"))
        );
        assert_eq!(
            concrete(&distinct(
                9,
                "First",
                vec![CheckedType::I32],
                CheckedType::I32,
            )),
            concrete(&distinct(
                9,
                "Second",
                vec![CheckedType::I32],
                CheckedType::Product(CheckedProductType {
                    elements: vec![element(Some("payload"), CheckedType::I32, None)],
                    variadic: false,
                }),
            )),
            "nominal identity ignores display names and expanded representations"
        );
        assert_eq!(
            concrete(&CheckedType::Product(CheckedProductType {
                elements: vec![element(Some("x"), CheckedType::I32, None)],
                variadic: false,
            })),
            concrete(&CheckedType::Product(CheckedProductType {
                elements: vec![element(
                    Some("x"),
                    CheckedType::I32,
                    Some(integer_default("1")),
                )],
                variadic: false,
            })),
            "contextual element defaults never participate in a key"
        );
        let with_default = CheckedFunctionType {
            default: Some(Box::new(CheckedFunctionParameterDefault {
                name: "x".to_owned(),
                value: integer_default("1"),
            })),
            ..function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )
        };
        assert_eq!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            canonical_function(&with_default),
            "a function-source default never participates in a key"
        );
    }

    #[test]
    fn nominal_identity_counts_and_shapes_separate() {
        assert_ne!(concrete(&nominal(1, "Same")), concrete(&nominal(2, "Same")));
        assert_ne!(
            concrete(&nominal(1, "Same")),
            concrete(&opaque(1, "Same")),
            "nominal kind is part of identity"
        );
        assert_ne!(
            concrete(&CheckedType::TypeConstructor {
                id: TypeId(1),
                name: "Same".to_owned(),
                arguments: vec![CheckedType::I32],
            }),
            concrete(&CheckedType::TypeConstructor {
                id: TypeId(1),
                name: "Same".to_owned(),
                arguments: vec![CheckedType::I64],
            }),
            "nominal arguments are part of identity"
        );
        let named = |name: Option<&str>| {
            CheckedType::Product(CheckedProductType {
                elements: vec![element(name, CheckedType::I32, None)],
                variadic: false,
            })
        };
        assert_ne!(concrete(&named(Some("x"))), concrete(&named(Some("y"))));
        assert_ne!(concrete(&named(Some("x"))), concrete(&named(None)));
        assert_ne!(
            concrete(&CheckedType::Product(CheckedProductType {
                elements: vec![
                    element(None, CheckedType::I32, None),
                    element(None, CheckedType::I64, None),
                ],
                variadic: false,
            })),
            concrete(&CheckedType::Product(CheckedProductType {
                elements: vec![
                    element(None, CheckedType::I64, None),
                    element(None, CheckedType::I32, None),
                ],
                variadic: false,
            })),
            "field order is identity"
        );
        assert_ne!(
            concrete(&CheckedType::Product(CheckedProductType {
                elements: vec![element(None, CheckedType::I32, None)],
                variadic: false,
            })),
            concrete(&CheckedType::Product(CheckedProductType {
                elements: vec![element(None, CheckedType::I32, None)],
                variadic: true,
            })),
            "the variadic flag is identity"
        );
        assert_ne!(
            concrete(&CheckedType::Sum(CheckedSumType {
                alternatives: vec![CheckedType::I32, CheckedType::I64],
            })),
            concrete(&CheckedType::Sum(CheckedSumType {
                alternatives: vec![CheckedType::I64, CheckedType::I32],
            })),
            "alternative order is identity"
        );
        assert!(matches!(
            concrete(&CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(2)),
            }),
            CanonicalType::Array { .. }
        ));
        assert_ne!(
            concrete(&CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(2)),
            }),
            concrete(&CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(3)),
            }),
            "repeated counts are identity"
        );
        assert_ne!(
            concrete(&CheckedType::NumberLiteral(1)),
            concrete(&CheckedType::NumberLiteral(2)),
            "literal payloads are identity"
        );
        assert_ne!(
            concrete(&CheckedType::StringLiteralSet(vec![
                "a".to_owned(),
                "b".to_owned()
            ])),
            concrete(&CheckedType::StringLiteralSet(vec![
                "b".to_owned(),
                "a".to_owned()
            ])),
            "literal-set order is preserved"
        );
    }

    #[test]
    fn function_details_and_effect_rows_separate() {
        assert_ne!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Juxtaposed,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            "parameter style is identity"
        );
        assert_ne!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                vec![CheckedMutation::Whole],
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            "mutation masks are identity"
        );
        assert_ne!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                vec![CheckedMutation::Whole, CheckedMutation::Element(1)],
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                vec![CheckedMutation::Element(1), CheckedMutation::Whole],
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            "ordered move masks are identity"
        );
        let effects = |resources: Vec<CheckedResource>, state| CheckedEffectSet {
            variable: None,
            resources,
            state,
        };
        assert_ne!(
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![
                        CheckedResource {
                            value_type: CheckedType::I32,
                            mutable: false,
                        },
                        CheckedResource {
                            value_type: CheckedType::F64,
                            mutable: false,
                        },
                    ],
                    None,
                ),
                CheckedType::I32,
            )),
            canonical_function(&function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![
                        CheckedResource {
                            value_type: CheckedType::F64,
                            mutable: false,
                        },
                        CheckedResource {
                            value_type: CheckedType::I32,
                            mutable: false,
                        },
                    ],
                    None,
                ),
                CheckedType::I32,
            )),
            "resource order is identity"
        );
        assert_ne!(
            concrete(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![CheckedResource {
                        value_type: CheckedType::I32,
                        mutable: true,
                    }],
                    None,
                ),
                CheckedType::I32,
            ))),
            concrete(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![CheckedResource {
                        value_type: CheckedType::I32,
                        mutable: false,
                    }],
                    None,
                ),
                CheckedType::I32,
            ))),
            "resource mutability is identity"
        );
        let stateful = |state| {
            concrete(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(Vec::new(), state),
                CheckedType::I32,
            )))
        };
        assert_ne!(stateful(None), stateful(Some(CheckedStateEffect::Read)));
        assert_ne!(
            stateful(Some(CheckedStateEffect::Read)),
            stateful(Some(CheckedStateEffect::Write))
        );
        assert_ne!(
            stateful(Some(CheckedStateEffect::Write)),
            stateful(Some(CheckedStateEffect::ReadWrite))
        );
        assert_ne!(
            stateful(Some(CheckedStateEffect::ReadWrite)),
            stateful(Some(CheckedStateEffect::Read))
        );
        assert_ne!(
            concrete(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(Vec::new(), None),
                CheckedType::I32,
            ))),
            concrete(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(
                    vec![CheckedResource {
                        value_type: CheckedType::I32,
                        mutable: false,
                    }],
                    None,
                ),
                CheckedType::I32,
            ))),
            "empty and nonempty effect rows are distinct"
        );
    }

    #[test]
    fn parameters_never_form_concrete_keys() {
        let first = CheckedType::Parameter {
            id: TypeParameterId(5),
            name: "First".to_owned(),
            sized: false,
        };
        let diagnostic = CanonicalType::concrete(&first, &requesting_origin())
            .expect_err("an unresolved parameter never forms a concrete key");
        assert_eq!(diagnostic.span, requesting_origin().span);
        assert!(diagnostic.message.contains("`First`"));
        assert!(
            CanonicalType::concrete(
                &CheckedType::Ref(Box::new(CheckedType::Parameter {
                    id: TypeParameterId(6),
                    name: "Nested".to_owned(),
                    sized: false,
                })),
                &requesting_origin(),
            )
            .is_err(),
            "unresolved parameters are rejected at any depth"
        );
    }

    #[test]
    fn unresolved_effects_and_placeholders_fail_concrete_keys() {
        let template = CheckedEffectSet {
            variable: Some(crate::CheckedEffectVariable {
                id: TypeParameterId(3),
                name: "Effects".to_owned(),
            }),
            resources: vec![CheckedResource {
                value_type: CheckedType::I32,
                mutable: false,
            }],
            state: Some(CheckedStateEffect::Read),
        };
        let diagnostic = CanonicalEffectSet::concrete(&template, &requesting_origin())
            .expect_err("an unresolved effect variable never forms a concrete key");
        assert_eq!(diagnostic.span, requesting_origin().span);
        assert!(diagnostic.message.contains("`Effects`"));

        for placeholder in [CheckedType::Inferred, CheckedType::Error] {
            let diagnostic = CanonicalType::concrete(&placeholder, &requesting_origin())
                .expect_err("checker placeholders never form concrete keys");
            assert_eq!(diagnostic.span, requesting_origin().span);
        }
    }

    fn type_substitution(parameter: usize, value_type: CheckedType) -> InstanceSubstitution {
        InstanceSubstitution::Type {
            parameter: TypeParameterId(parameter),
            value: concrete(&value_type),
        }
    }

    fn effect_substitution(parameter: usize, effects: &CheckedEffectSet) -> InstanceSubstitution {
        InstanceSubstitution::Effect {
            parameter: TypeParameterId(parameter),
            effects: CanonicalEffectSet::concrete(effects, &requesting_origin())
                .expect("concrete effect substitution"),
        }
    }

    fn instance_key(
        function: usize,
        substitutions: Vec<InstanceSubstitution>,
        evidence: Option<CanonicalEvidence>,
    ) -> InstanceKey {
        InstanceKey::new(FunctionId(function), substitutions, evidence)
            .expect("well-formed instance key")
    }

    #[test]
    fn instance_keys_order_dedupe_and_reject_conflicts() {
        let unsorted = instance_key(
            1,
            vec![
                type_substitution(2, CheckedType::I64),
                type_substitution(1, CheckedType::I32),
            ],
            None,
        );
        let sorted = instance_key(
            1,
            vec![
                type_substitution(1, CheckedType::I32),
                type_substitution(2, CheckedType::I64),
            ],
            None,
        );
        assert_eq!(unsorted, sorted, "entry order is normalized, not identity");
        assert_eq!(
            unsorted
                .substitutions()
                .iter()
                .map(InstanceSubstitution::parameter)
                .collect::<Vec<_>>(),
            vec![TypeParameterId(1), TypeParameterId(2)]
        );

        let duplicate = InstanceKey::new(
            FunctionId(1),
            vec![
                type_substitution(1, CheckedType::I32),
                type_substitution(1, CheckedType::I64),
            ],
            None,
        )
        .expect_err("duplicate parameter entries are rejected");
        assert_eq!(
            duplicate,
            InstanceKeyError::DuplicateParameter(TypeParameterId(1))
        );

        let conflicting = InstanceKey::new(
            FunctionId(1),
            vec![
                type_substitution(1, CheckedType::I32),
                effect_substitution(1, &CheckedEffectSet::default()),
            ],
            None,
        )
        .expect_err("type and effect entries cannot share a parameter");
        assert_eq!(
            conflicting,
            InstanceKeyError::ConflictingParameter(TypeParameterId(1))
        );

        let unparameterized = instance_key(3, Vec::new(), None);
        assert!(unparameterized.substitutions().is_empty());
        assert!(unparameterized.evidence().is_none());
        assert_eq!(unparameterized, instance_key(3, Vec::new(), None));
    }

    #[test]
    fn identical_signatures_still_separate_on_outer_substitutions() {
        let signature = function_type(
            CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
            FunctionParameterStyle::Single,
            Vec::new(),
            Vec::new(),
            CheckedEffectSet::default(),
            CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                CheckedEffectSet::default(),
                CheckedType::I32,
            )),
        );
        let canonical_signature = canonical_function(&signature);
        let left = instance_key(9, vec![type_substitution(7, CheckedType::I32)], None);
        let right = instance_key(9, vec![type_substitution(7, CheckedType::I64)], None);
        assert_eq!(
            canonical_signature,
            canonical_function(&signature),
            "the callable signature is identical under both outer substitutions"
        );
        assert_ne!(
            left, right,
            "capture/body-relevant outer substitutions still separate instances"
        );
    }

    #[test]
    fn selected_implementations_and_structural_methods_separate() {
        let explicit = |function: usize| CanonicalEvidence::ExplicitImplementation {
            trait_id: TraitId(1),
            method: TraitMethodId(2),
            function: FunctionId(function),
            arguments: vec![concrete(&CheckedType::I32)],
        };
        let structural = |arguments: Vec<CheckedType>| CanonicalEvidence::Structural {
            trait_id: TraitId(1),
            method: TraitMethodId(2),
            structural: StructuralTraitMethod::Index,
            arguments: arguments
                .iter()
                .map(|argument| concrete(argument))
                .collect(),
        };

        assert_eq!(
            instance_key(3, Vec::new(), Some(explicit(4))),
            instance_key(3, Vec::new(), Some(explicit(4)))
        );
        assert_ne!(
            instance_key(3, Vec::new(), Some(explicit(4))),
            instance_key(3, Vec::new(), Some(explicit(5))),
            "two selected implementations stay distinct even with matching signatures"
        );
        assert_ne!(
            instance_key(3, Vec::new(), Some(explicit(4))),
            instance_key(3, Vec::new(), Some(structural(vec![CheckedType::I32]))),
            "explicit and structural selections are different keys"
        );
        assert_ne!(
            instance_key(3, Vec::new(), Some(structural(vec![CheckedType::I32]))),
            instance_key(3, Vec::new(), Some(structural(vec![CheckedType::I64]))),
            "structural arguments are part of evidence identity"
        );
    }

    #[test]
    fn artifact_keys_are_namespaced_and_concrete() {
        let callable = function_type(
            CheckedType::I32,
            FunctionParameterStyle::Single,
            Vec::new(),
            Vec::new(),
            CheckedEffectSet::default(),
            CheckedType::I32,
        );
        let adapter = |symbol: usize, adapter_kind| {
            ConstructorAdapterKey::new(
                SymbolId(symbol),
                TypeId(2),
                adapter_kind,
                &callable,
                &requesting_origin(),
            )
            .expect("concrete constructor-adapter key")
        };
        assert_eq!(
            adapter(1, LoweredCallableAdapter::Constructor),
            adapter(1, LoweredCallableAdapter::Constructor)
        );
        assert_ne!(
            adapter(1, LoweredCallableAdapter::Constructor),
            adapter(9, LoweredCallableAdapter::Constructor),
            "constructor symbol identity is part of the key"
        );
        assert_ne!(
            adapter(1, LoweredCallableAdapter::Constructor),
            adapter(1, LoweredCallableAdapter::External),
            "adapter kind is part of the key"
        );

        let structural = |kind, arguments: Vec<CheckedType>| {
            StructuralMethodKey::new(
                kind,
                TraitId(3),
                TraitMethodId(4),
                &arguments,
                &callable,
                &requesting_origin(),
            )
            .expect("concrete structural-method key")
        };
        assert_eq!(
            structural(StructuralTraitMethod::Index, vec![CheckedType::I32]),
            structural(StructuralTraitMethod::Index, vec![CheckedType::I32])
        );
        assert_ne!(
            structural(StructuralTraitMethod::Index, vec![CheckedType::I32]),
            structural(StructuralTraitMethod::Debug, vec![CheckedType::I32]),
            "structural method kind is part of the key"
        );
        assert_ne!(
            structural(StructuralTraitMethod::Index, vec![CheckedType::I32]),
            structural(StructuralTraitMethod::Index, vec![CheckedType::I64]),
            "structural arguments are part of the key"
        );

        let constructor_key = adapter(1, LoweredCallableAdapter::Constructor);
        let structural_key = structural(StructuralTraitMethod::Index, vec![CheckedType::I32]);
        assert_ne!(
            ArtifactRequestKey::ConstructorAdapter(constructor_key.clone()),
            ArtifactRequestKey::StructuralMethod(structural_key.clone()),
            "constructor-adapter and structural-method namespaces are distinct"
        );
        let unresolved_callable = function_type(
            CheckedType::Parameter {
                id: TypeParameterId(4),
                name: "T".to_owned(),
                sized: false,
            },
            FunctionParameterStyle::Single,
            Vec::new(),
            Vec::new(),
            CheckedEffectSet::default(),
            CheckedType::I32,
        );
        assert_eq!(
            ConstructorAdapterKey::new(
                SymbolId(1),
                TypeId(2),
                LoweredCallableAdapter::Constructor,
                &unresolved_callable,
                &requesting_origin(),
            )
            .expect_err("unresolved callable types never form artifact keys")
            .span,
            requesting_origin().span
        );
        assert_eq!(
            StructuralMethodKey::new(
                StructuralTraitMethod::Index,
                TraitId(3),
                TraitMethodId(4),
                &[CheckedType::Parameter {
                    id: TypeParameterId(5),
                    name: "K".to_owned(),
                    sized: false,
                }],
                &callable,
                &requesting_origin(),
            )
            .expect_err("unresolved structural arguments never form artifact keys")
            .span,
            requesting_origin().span
        );
    }

    fn simple_callable() -> CheckedFunctionType {
        function_type(
            CheckedType::I32,
            FunctionParameterStyle::Single,
            Vec::new(),
            Vec::new(),
            CheckedEffectSet::default(),
            CheckedType::I32,
        )
    }

    fn structural_artifact_key() -> ArtifactRequestKey {
        ArtifactRequestKey::StructuralMethod(
            StructuralMethodKey::new(
                StructuralTraitMethod::Index,
                TraitId(3),
                TraitMethodId(4),
                &[CheckedType::I32],
                &simple_callable(),
                &requesting_origin(),
            )
            .expect("concrete structural artifact"),
        )
    }

    #[test]
    fn catalog_assigns_append_only_first_discovery_ordinals() {
        let mut catalog = SpecializationCatalog::default();
        let first = instance_key(1, vec![type_substitution(1, CheckedType::I32)], None);
        let second = instance_key(2, Vec::new(), None);
        let first_ordinal = catalog.reserve_instance(first.clone());
        let second_ordinal = catalog.reserve_instance(second.clone());
        assert_eq!(first_ordinal.index(), 0);
        assert_eq!(second_ordinal.index(), 1);
        assert_eq!(
            catalog.reserve_instance(first.clone()),
            first_ordinal,
            "re-reserving returns the same ordinal instead of appending"
        );
        assert_eq!(catalog.instances().count(), 2);
        assert_eq!(catalog.instance(first_ordinal), Some(&first));
        assert_eq!(catalog.instance(second_ordinal), Some(&second));

        let artifact = structural_artifact_key();
        let artifact_ordinal = catalog.reserve_artifact(artifact.clone());
        assert_eq!(
            artifact_ordinal.index(),
            0,
            "artifact ordinals are their own append-only family"
        );
        assert_eq!(catalog.reserve_artifact(artifact.clone()), artifact_ordinal);
        assert_eq!(catalog.artifact(artifact_ordinal), Some(&artifact));
        assert_eq!(first_ordinal.index(), artifact_ordinal.index());
        let _: InstanceOrdinal = first_ordinal;
        let _: ArtifactOrdinal = artifact_ordinal;
        assert_eq!(
            catalog.planned_names().expect("unique names"),
            vec![
                "__staple_instance_0".to_owned(),
                "__staple_instance_1".to_owned(),
                "__staple_structural_index_0".to_owned(),
            ]
        );
    }

    #[test]
    fn planned_names_use_declared_names_for_non_generic_instances_and_collision_check() {
        let mut catalog = SpecializationCatalog::default();
        let generic = catalog.reserve_instance(instance_key(
            1,
            vec![type_substitution(1, CheckedType::I32)],
            None,
        ));
        let declared = catalog.reserve_instance(instance_key(2, Vec::new(), None));
        let declared_name = |key: &InstanceKey| {
            (key.function() == FunctionId(2)).then(|| "__staple_mm.name".to_owned())
        };
        assert_eq!(
            catalog
                .planned_names_with(declared_name)
                .expect("unique names"),
            vec![
                "__staple_instance_0".to_owned(),
                "__staple_mm.name".to_owned(),
            ]
        );
        assert_eq!(
            catalog
                .planned_names_with(declared_name)
                .expect("unique names")
                .get(generic.index())
                .map(String::as_str),
            Some("__staple_instance_0"),
            "a generic instance keeps its ordinal name"
        );
        assert_eq!(
            catalog
                .planned_names_with(declared_name)
                .expect("unique names")
                .get(declared.index())
                .map(String::as_str),
            Some("__staple_mm.name")
        );

        // A declared name that aliases a name already assigned in catalog
        // order falls back to the instance's ordinal name instead of failing:
        // two distinct templates can share a declared name.
        let mut aliasing = SpecializationCatalog::default();
        aliasing.reserve_instance(instance_key(
            1,
            vec![type_substitution(1, CheckedType::I32)],
            None,
        ));
        aliasing.reserve_instance(instance_key(2, Vec::new(), None));
        assert_eq!(
            aliasing
                .planned_names_with(|_| Some("__staple_instance_0".to_owned()))
                .expect("the alias falls back"),
            vec![
                "__staple_instance_0".to_owned(),
                "__staple_instance_1".to_owned(),
            ]
        );

        // Two non-generic templates that share a declared name get one
        // declared name and one ordinal fallback, deterministically.
        let mut shared = SpecializationCatalog::default();
        shared.reserve_instance(instance_key(1, Vec::new(), None));
        shared.reserve_instance(instance_key(2, Vec::new(), None));
        assert_eq!(
            shared
                .planned_names_with(|_| Some("__staple_mm.first".to_owned()))
                .expect("unique names"),
            vec![
                "__staple_mm.first".to_owned(),
                "__staple_instance_1".to_owned()
            ]
        );
    }

    #[test]
    fn repeated_runs_produce_identical_keys_order_and_names() {
        fn build() -> SpecializationCatalog {
            let mut catalog = SpecializationCatalog::default();
            let mut by_parameter = HashMap::new();
            by_parameter.insert(TypeParameterId(2), CheckedType::I64);
            by_parameter.insert(TypeParameterId(1), CheckedType::I32);
            let substitutions = by_parameter
                .iter()
                .map(|(parameter, value_type)| InstanceSubstitution::Type {
                    parameter: *parameter,
                    value: concrete(value_type),
                })
                .collect();
            catalog.reserve_instance(
                InstanceKey::new(FunctionId(7), substitutions, None).expect("well-formed key"),
            );
            catalog.reserve_artifact(structural_artifact_key());
            catalog
        }

        let left = build();
        let right = build();
        assert_eq!(
            left.planned_names().expect("unique names"),
            right.planned_names().expect("unique names")
        );
        assert_eq!(
            left.instances()
                .map(|(_, key)| key.clone())
                .collect::<Vec<_>>(),
            right
                .instances()
                .map(|(_, key)| key.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            left.instances().count(),
            1,
            "differently ordered construction collapses to one deterministic key"
        );
    }

    #[derive(Default)]
    struct ConstantHasher;

    impl std::hash::Hasher for ConstantHasher {
        fn finish(&self) -> u64 {
            0
        }

        fn write(&mut self, _: &[u8]) {}
    }

    #[test]
    fn constant_hashes_cannot_alias_distinct_keys() {
        let mut map: HashMap<InstanceKey, usize, std::hash::BuildHasherDefault<ConstantHasher>> =
            Default::default();
        let first = instance_key(1, vec![type_substitution(1, CheckedType::I32)], None);
        let second = instance_key(1, vec![type_substitution(1, CheckedType::I64)], None);
        map.insert(first.clone(), 1);
        map.insert(second.clone(), 2);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&first), Some(&1));
        assert_eq!(map.get(&second), Some(&2));
    }

    #[test]
    fn recursive_nominal_keys_stay_compact() {
        let mut expanded = distinct(11, "Node", vec![CheckedType::I32], CheckedType::I32);
        for _ in 0..64 {
            expanded = distinct(11, "Node", vec![CheckedType::I32], expanded);
        }
        let expanded_key = concrete(&expanded);
        let shallow_key = concrete(&distinct(
            11,
            "Node",
            vec![CheckedType::I32],
            CheckedType::I32,
        ));
        assert_eq!(expanded_key, shallow_key);
        assert_eq!(
            node_count(&expanded_key),
            node_count(&shallow_key),
            "the expanded representation never enters the key"
        );
    }

    fn instance_owner(instance: usize) -> ArtifactSiteOwner {
        ArtifactSiteOwner::Instance(InstanceOrdinal(instance))
    }

    fn callback_site(index: usize) -> ArtifactSite {
        ArtifactSite::Callback(LoweredReactiveCallbackId::for_test(index))
    }

    fn stage_4_artifact_families() -> Vec<ArtifactRequestKey> {
        let value_type = concrete(&nominal(7, "Node"));
        let runner = ReactiveRunnerKey {
            owner: instance_owner(0),
            site: callback_site(0),
        };
        vec![
            ArtifactRequestKey::ConstructorAdapter(
                ConstructorAdapterKey::new(
                    SymbolId(1),
                    TypeId(7),
                    LoweredCallableAdapter::Constructor,
                    &simple_callable(),
                    &requesting_origin(),
                )
                .expect("concrete constructor adapter"),
            ),
            structural_artifact_key(),
            ArtifactRequestKey::DropGlue(value_type.clone()),
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(value_type.clone())),
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Cell(value_type.clone())),
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
                closure: InstanceOrdinal(0),
                captures: vec![concrete(&CheckedType::I32)],
            }),
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Buffer(value_type)),
            ArtifactRequestKey::CoroutineCodes(CoroutineCodesKey {
                body: InstanceOrdinal(0),
            }),
            ArtifactRequestKey::ReactionRunner(runner),
            ArtifactRequestKey::UntilRunner(runner),
            ArtifactRequestKey::DerivedRunner(runner),
            ArtifactRequestKey::ExternAdapter(ExternAdapterKey {
                symbol: SymbolId(1),
                callable_type: canonical_function(&simple_callable()),
            }),
        ]
    }

    #[test]
    fn stage_4_artifact_families_are_namespaced_and_encoded() {
        let families = stage_4_artifact_families();
        assert_eq!(
            families.len(),
            12,
            "every Stage 4.1 artifact family has a representative key"
        );
        let mut names = HashSet::new();
        let mut encodings = HashSet::new();
        for key in &families {
            assert!(
                names.insert(key.family_name()),
                "family name `{}` is unique",
                key.family_name()
            );
            assert!(
                encodings.insert(key.clone()),
                "family `{}` has a distinct key",
                key.family_name()
            );
        }
        assert_eq!(
            names.len(),
            families.len(),
            "no two key families share a family name"
        );
        let runners = [
            ArtifactRequestKey::ReactionRunner(ReactiveRunnerKey {
                owner: instance_owner(0),
                site: callback_site(0),
            }),
            ArtifactRequestKey::UntilRunner(ReactiveRunnerKey {
                owner: instance_owner(0),
                site: callback_site(0),
            }),
            ArtifactRequestKey::DerivedRunner(ReactiveRunnerKey {
                owner: instance_owner(0),
                site: callback_site(0),
            }),
        ];
        assert_ne!(
            runners[0], runners[1],
            "runner kinds never alias on the same owner and site"
        );
        assert_ne!(runners[1], runners[2]);
        assert_ne!(runners[0].clone(), runners[2].clone());
    }

    #[test]
    fn per_site_artifacts_separate_by_owner_and_site() {
        let site = callback_site(0);
        let first = ReactiveRunnerKey {
            owner: instance_owner(0),
            site,
        };
        let second = ReactiveRunnerKey {
            owner: instance_owner(1),
            site,
        };
        assert_ne!(
            ArtifactRequestKey::ReactionRunner(first),
            ArtifactRequestKey::ReactionRunner(second),
            "the same lowered syntax in two instances produces two artifacts"
        );
        let owners = [
            ArtifactSiteOwner::Initializer(InitializerId::for_test(0)),
            ArtifactSiteOwner::Instance(InstanceOrdinal(0)),
        ];
        let mut encodings = HashSet::new();
        for owner in owners {
            let key = ArtifactRequestKey::UntilRunner(ReactiveRunnerKey { owner, site });
            assert!(
                encodings.insert(key.clone()),
                "each owner namespace separates"
            );
        }
        assert_ne!(
            ArtifactRequestKey::UntilRunner(first),
            ArtifactRequestKey::UntilRunner(ReactiveRunnerKey {
                owner: instance_owner(0),
                site: callback_site(1),
            }),
            "distinct sites in one owner separate"
        );
        assert_eq!(
            ArtifactRequestKey::UntilRunner(first),
            ArtifactRequestKey::UntilRunner(ReactiveRunnerKey {
                owner: instance_owner(0),
                site: callback_site(0),
            })
        );

        let captures = vec![concrete(&CheckedType::I32)];
        assert_ne!(
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
                closure: InstanceOrdinal(0),
                captures: captures.clone(),
            }),
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
                closure: InstanceOrdinal(1),
                captures,
            }),
            "closure finalizers separate by closure instance for identical captures"
        );
        let mut coroutine_encodings = HashSet::new();
        for body in 0..2 {
            coroutine_encodings.insert(
                ArtifactRequestKey::CoroutineCodes(CoroutineCodesKey {
                    body: InstanceOrdinal(body),
                })
                .clone(),
            );
        }
        assert_eq!(coroutine_encodings.len(), 2);
    }

    #[test]
    fn type_keyed_artifacts_deduplicate_structurally_equal_types() {
        let first = ArtifactRequestKey::DropGlue(concrete(&nominal(7, "Node")));
        let display_alias = ArtifactRequestKey::DropGlue(concrete(&nominal(7, "AliasName")));
        assert_eq!(
            first, display_alias,
            "display names never separate type-keyed artifacts"
        );
        assert_eq!(first.clone(), display_alias.clone());
        assert_ne!(
            first,
            ArtifactRequestKey::DropGlue(concrete(&nominal(8, "Node")))
        );
        assert_ne!(
            first,
            ArtifactRequestKey::DropGlue(concrete(&CheckedType::I64))
        );
        assert_ne!(
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(concrete(&CheckedType::I32))),
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Cell(concrete(&CheckedType::I32))),
            "finalizer subkinds never deduplicate across kinds"
        );
        assert_ne!(
            ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(concrete(&CheckedType::I32))),
            ArtifactRequestKey::DropGlue(concrete(&CheckedType::I32)),
            "the family tag separates equally typed finalizer and drop glue"
        );
        let adapter = ArtifactRequestKey::ExternAdapter(ExternAdapterKey {
            symbol: SymbolId(3),
            callable_type: canonical_function(&simple_callable()),
        });
        assert_eq!(
            adapter,
            ArtifactRequestKey::ExternAdapter(ExternAdapterKey {
                symbol: SymbolId(3),
                callable_type: canonical_function(&simple_callable()),
            })
        );
        assert_ne!(
            adapter,
            ArtifactRequestKey::ExternAdapter(ExternAdapterKey {
                symbol: SymbolId(4),
                callable_type: canonical_function(&simple_callable()),
            })
        );
    }

    #[test]
    fn every_artifact_key_family_has_a_matching_placeholder_plan() {
        use crate::{
            ConstructorAdapterPlan, ConstructorConstruction, CoroutineCodesPlan, DropGlueBody,
            DropGluePlan, ExternAdapterPlan, GcFinalizerPlan, LoweredArtifactPlan,
            ReactiveRunnerBody, ReactiveRunnerPlan, StructuralBody, StructuralMethodPlan,
        };
        let keys = stage_4_artifact_families();
        let node = nominal(7, "Node");
        let instance = crate::FunctionInstanceId::for_test(0);
        let runner = ReactiveRunnerPlan {
            owner: instance_owner(0),
            site: callback_site(0),
            body: ReactiveRunnerBody::Unexpanded,
        };
        let plans = vec![
            LoweredArtifactPlan::ConstructorAdapter(ConstructorAdapterPlan {
                symbol: SymbolId(1),
                type_id: TypeId(7),
                adapter: LoweredCallableAdapter::Constructor,
                callable_type: simple_callable(),
                construction: ConstructorConstruction::Unexpanded,
            }),
            LoweredArtifactPlan::StructuralMethod(StructuralMethodPlan {
                structural: StructuralTraitMethod::Index,
                trait_id: TraitId(3),
                method: TraitMethodId(4),
                arguments: vec![CheckedType::I32],
                callable_type: simple_callable(),
                body: StructuralBody::Unexpanded,
            }),
            LoweredArtifactPlan::DropGlue(DropGluePlan {
                value_type: node.clone(),
                body: DropGlueBody::Unexpanded,
            }),
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Payload {
                value_type: node.clone(),
                glue: None,
            }),
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Cell {
                value_type: node.clone(),
                glue: None,
            }),
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::ClosureEnvironment {
                closure: instance,
                captures: vec![CheckedType::I32],
                drops: None,
            }),
            LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Buffer {
                element: node.clone(),
                glue: None,
            }),
            LoweredArtifactPlan::CoroutineCodes(CoroutineCodesPlan {
                body: instance,
                frame: None,
            }),
            LoweredArtifactPlan::ReactionRunner(runner.clone()),
            LoweredArtifactPlan::UntilRunner(runner.clone()),
            LoweredArtifactPlan::DerivedRunner(runner),
            LoweredArtifactPlan::ExternAdapter(ExternAdapterPlan {
                symbol: SymbolId(1),
                callable_type: simple_callable(),
                indirect_parameters: Vec::new(),
                declaration: None,
            }),
        ];
        assert_eq!(plans.len(), keys.len());
        let mut plan_names = HashSet::new();
        for plan in &plans {
            assert!(
                plan_names.insert(plan.family_name()),
                "plan family `{}` is unique",
                plan.family_name()
            );
        }
        let key_names = keys
            .iter()
            .map(ArtifactRequestKey::family_name)
            .collect::<HashSet<_>>();
        assert_eq!(
            plan_names, key_names,
            "the placeholder plan families mirror the key families exactly"
        );
        let origin = requesting_origin();
        for (index, (key, plan)) in keys.iter().zip(&plans).enumerate() {
            assert!(
                plan.matches_key(key, &origin),
                "plan `{}` rebuilds key `{}`",
                plan.family_name(),
                key.family_name()
            );
            for (other_index, other) in keys.iter().enumerate() {
                if other_index != index {
                    assert!(
                        !plan.matches_key(other, &origin),
                        "plan `{}` never matches key `{}`",
                        plan.family_name(),
                        other.family_name()
                    );
                }
            }
        }

        // Same family, different identity inputs: the plan must not agree.
        let mismatches = [
            (
                LoweredArtifactPlan::DropGlue(DropGluePlan {
                    value_type: CheckedType::I64,
                    body: DropGlueBody::Unexpanded,
                }),
                ArtifactRequestKey::DropGlue(concrete(&node)),
            ),
            (
                LoweredArtifactPlan::StructuralMethod(StructuralMethodPlan {
                    structural: StructuralTraitMethod::Debug,
                    trait_id: TraitId(3),
                    method: TraitMethodId(4),
                    arguments: vec![CheckedType::I32],
                    callable_type: simple_callable(),
                    body: StructuralBody::Unexpanded,
                }),
                structural_artifact_key(),
            ),
            (
                LoweredArtifactPlan::CoroutineCodes(CoroutineCodesPlan {
                    body: crate::FunctionInstanceId::for_test(1),
                    frame: None,
                }),
                ArtifactRequestKey::CoroutineCodes(CoroutineCodesKey {
                    body: InstanceOrdinal(0),
                }),
            ),
            (
                LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::ClosureEnvironment {
                    closure: crate::FunctionInstanceId::for_test(1),
                    captures: vec![CheckedType::I32],
                    drops: None,
                }),
                ArtifactRequestKey::GcFinalizer(GcFinalizerKey::ClosureEnvironment {
                    closure: InstanceOrdinal(0),
                    captures: vec![concrete(&CheckedType::I32)],
                }),
            ),
            (
                LoweredArtifactPlan::ReactionRunner(ReactiveRunnerPlan {
                    owner: instance_owner(1),
                    site: callback_site(0),
                    body: ReactiveRunnerBody::Unexpanded,
                }),
                ArtifactRequestKey::ReactionRunner(ReactiveRunnerKey {
                    owner: instance_owner(0),
                    site: callback_site(0),
                }),
            ),
            (
                LoweredArtifactPlan::ExternAdapter(ExternAdapterPlan {
                    symbol: SymbolId(2),
                    callable_type: simple_callable(),
                    indirect_parameters: Vec::new(),
                    declaration: None,
                }),
                ArtifactRequestKey::ExternAdapter(ExternAdapterKey {
                    symbol: SymbolId(1),
                    callable_type: canonical_function(&simple_callable()),
                }),
            ),
        ];
        for (plan, key) in &mismatches {
            assert!(
                !plan.matches_key(key, &origin),
                "plan `{}` with different identity inputs must not rebuild key `{}`",
                plan.family_name(),
                key.family_name()
            );
        }
    }
}
