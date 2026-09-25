//! Canonical structural keys for Stage 3 specialization.
//!
//! Stage 3.1 defines the owned, typed identity of source-function instances
//! and of the constructor-adapter and structural-method artifacts Stage 4
//! materializes. Keys are built from semantic IDs and structural checked data
//! only: display names, source spans, contextual defaults, and expanded
//! nominal representations never participate in equality. The worklist that
//! consumes these keys belongs to Stage 3.3.

#![allow(dead_code)] // Stage 3.1 defines and tests keys before Stage 3.3 uses them.

use std::collections::{HashMap, HashSet};

use staple_syntax::Diagnostic;

use crate::{
    CallSubstitutions, CheckedEffectSet, CheckedFunctionType, CheckedMutation, CheckedResource,
    CheckedStateEffect, CheckedType, FunctionId, LoweredCallableAdapter, Origin,
    StructuralTraitMethod, SymbolId, TraitEvidence, TraitId, TraitMethodId, TypeId,
    TypeParameterId,
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
    /// A template-level declared type parameter. Concrete keys reject it; the
    /// parameter's display name and `sized` metadata are not identity.
    Parameter(TypeParameterId),
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

/// A canonical effect row. Template keys may retain a declared effect-variable
/// ID; concrete keys require Stage 3.2 to have substituted it away.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CanonicalEffectSet {
    pub variable: Option<TypeParameterId>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversionMode {
    /// Template keys keep declared type/effect parameters as typed IDs.
    Template,
    /// Concrete keys reject any unresolved parameter, effect variable, and
    /// `Inferred`/`Error` placeholder.
    Concrete,
}

impl CanonicalType {
    /// Converts a checked type into a template-level key. Declared parameters
    /// stay by semantic ID so nested closure and enclosing-parameter
    /// substitutions can be added by Stage 3.2.
    pub(crate) fn template(value_type: &CheckedType, origin: &Origin) -> Result<Self, Diagnostic> {
        Self::convert(value_type, origin, ConversionMode::Template)
    }

    /// Converts a checked type into a concrete key, rejecting unresolved
    /// parameters and checker placeholders with a diagnostic at the
    /// requesting record's origin.
    pub(crate) fn concrete(value_type: &CheckedType, origin: &Origin) -> Result<Self, Diagnostic> {
        Self::convert(value_type, origin, ConversionMode::Concrete)
    }

    fn convert(
        value_type: &CheckedType,
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Self, Diagnostic> {
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
                CanonicalType::Ref(Box::new(Self::convert(payload, origin, mode)?))
            }
            CheckedType::Slice(payload) => {
                CanonicalType::Slice(Box::new(Self::convert(payload, origin, mode)?))
            }
            CheckedType::Buffer(payload) => {
                CanonicalType::Buffer(Box::new(Self::convert(payload, origin, mode)?))
            }
            CheckedType::Array { element, count } => CanonicalType::Array {
                element: Box::new(Self::convert(element, origin, mode)?),
                count: Box::new(Self::convert(count, origin, mode)?),
            },
            CheckedType::CString => CanonicalType::CString,
            CheckedType::CChar => CanonicalType::CChar,
            CheckedType::Parameter { id, name, .. } => match mode {
                ConversionMode::Template => CanonicalType::Parameter(*id),
                ConversionMode::Concrete => {
                    return Err(origin_diagnostic(
                        origin,
                        format!("type parameter `{name}` is not resolved for a concrete key"),
                    ));
                }
            },
            CheckedType::TypeConstructor { id, arguments, .. } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::TypeConstructor,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin, mode)?,
            },
            CheckedType::Opaque { id, arguments, .. } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::Opaque,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin, mode)?,
            },
            CheckedType::CPointer { pointee } => CanonicalType::CPointer {
                pointee: Box::new(Self::convert(pointee, origin, mode)?),
            },
            CheckedType::Product(product) => CanonicalType::Product {
                elements: product
                    .elements
                    .iter()
                    .map(|element| {
                        Ok(CanonicalProductElement {
                            name: element.name.clone(),
                            value_type: Self::convert(&element.value_type, origin, mode)?,
                        })
                    })
                    .collect::<Result<Vec<_>, Diagnostic>>()?,
                variadic: product.variadic,
            },
            CheckedType::Sum(sum) => CanonicalType::Sum {
                alternatives: Self::convert_arguments(&sum.alternatives, origin, mode)?,
            },
            CheckedType::Function(function) => CanonicalType::Function(Box::new(
                CanonicalFunctionType::convert(function, origin, mode)?,
            )),
            CheckedType::Distinct {
                id,
                arguments,
                representation: _,
                ..
            } => CanonicalType::Nominal {
                kind: CanonicalNominalKind::Distinct,
                id: *id,
                arguments: Self::convert_arguments(arguments, origin, mode)?,
            },
        };
        Ok(key)
    }

    fn convert_arguments(
        arguments: &[CheckedType],
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Vec<CanonicalType>, Diagnostic> {
        arguments
            .iter()
            .map(|argument| Self::convert(argument, origin, mode))
            .collect()
    }
}

impl CanonicalType {
    /// The first declared type parameter reachable from this value, used to
    /// reject programmatically-built substitution values that are not
    /// concrete. Traversal follows structural field order.
    pub(crate) fn unresolved_parameter(&self) -> Option<TypeParameterId> {
        match self {
            CanonicalType::Parameter(parameter) => Some(*parameter),
            CanonicalType::Ref(payload)
            | CanonicalType::Slice(payload)
            | CanonicalType::Buffer(payload)
            | CanonicalType::CPointer { pointee: payload } => payload.unresolved_parameter(),
            CanonicalType::Array { element, count } => element
                .unresolved_parameter()
                .or_else(|| count.unresolved_parameter()),
            CanonicalType::Nominal { arguments, .. }
            | CanonicalType::Sum {
                alternatives: arguments,
            } => arguments
                .iter()
                .find_map(CanonicalType::unresolved_parameter),
            CanonicalType::Product { elements, .. } => elements
                .iter()
                .find_map(|element| element.value_type.unresolved_parameter()),
            CanonicalType::Function(function) => function.unresolved_parameter(),
            _ => None,
        }
    }
}

impl CanonicalFunctionType {
    fn unresolved_parameter(&self) -> Option<TypeParameterId> {
        self.parameter
            .unresolved_parameter()
            .or_else(|| self.effects.unresolved_variable())
            .or_else(|| self.result.unresolved_parameter())
    }

    pub(crate) fn template(
        function: &CheckedFunctionType,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(function, origin, ConversionMode::Template)
    }

    pub(crate) fn concrete(
        function: &CheckedFunctionType,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(function, origin, ConversionMode::Concrete)
    }

    fn convert(
        function: &CheckedFunctionType,
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Self, Diagnostic> {
        Ok(CanonicalFunctionType {
            parameter: Box::new(CanonicalType::convert(&function.parameter, origin, mode)?),
            parameter_style: match function.parameter_style {
                staple_syntax::FunctionParameterStyle::Single => CanonicalParameterStyle::Single,
                staple_syntax::FunctionParameterStyle::Juxtaposed => {
                    CanonicalParameterStyle::Juxtaposed
                }
            },
            mutations: function.mutations.iter().cloned().map(Into::into).collect(),
            moves: function.moves.iter().cloned().map(Into::into).collect(),
            effects: CanonicalEffectSet::convert(&function.effects, origin, mode)?,
            result: Box::new(CanonicalType::convert(&function.result, origin, mode)?),
        })
    }
}

impl CanonicalEffectSet {
    fn unresolved_variable(&self) -> Option<TypeParameterId> {
        self.variable.or_else(|| {
            self.resources
                .iter()
                .find_map(|resource| resource.value_type.unresolved_parameter())
        })
    }

    pub(crate) fn template(
        effects: &CheckedEffectSet,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(effects, origin, ConversionMode::Template)
    }

    pub(crate) fn concrete(
        effects: &CheckedEffectSet,
        origin: &Origin,
    ) -> Result<Self, Diagnostic> {
        Self::convert(effects, origin, ConversionMode::Concrete)
    }

    fn convert(
        effects: &CheckedEffectSet,
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Self, Diagnostic> {
        let variable = match (&effects.variable, mode) {
            (None, _) => None,
            (Some(variable), ConversionMode::Template) => Some(variable.id),
            (Some(variable), ConversionMode::Concrete) => {
                return Err(origin_diagnostic(
                    origin,
                    format!(
                        "effect variable `{}` is not resolved for a concrete key",
                        variable.name
                    ),
                ));
            }
        };
        Ok(CanonicalEffectSet {
            variable,
            resources: effects
                .resources
                .iter()
                .map(|resource| CanonicalResource::convert(resource, origin, mode))
                .collect::<Result<Vec<_>, Diagnostic>>()?,
            state: effects.state.map(Into::into),
        })
    }
}

impl CanonicalResource {
    fn convert(
        resource: &CheckedResource,
        origin: &Origin,
        mode: ConversionMode,
    ) -> Result<Self, Diagnostic> {
        Ok(CanonicalResource {
            value_type: CanonicalType::convert(&resource.value_type, origin, mode)?,
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
    /// A substitution value still contains a declared type parameter.
    UnresolvedTypeParameter(TypeParameterId),
    /// An effect substitution still carries its declared effect variable.
    UnresolvedEffectVariable(TypeParameterId),
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
            InstanceKeyError::UnresolvedTypeParameter(parameter) => format!(
                "instance key substitution for parameter {} still contains a declared type parameter",
                parameter.0
            ),
            InstanceKeyError::UnresolvedEffectVariable(parameter) => format!(
                "instance key effect substitution for parameter {} still names its declared effect variable",
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
        for substitution in &substitutions {
            match substitution {
                InstanceSubstitution::Type { value, .. } => {
                    if let Some(unresolved) = value.unresolved_parameter() {
                        return Err(InstanceKeyError::UnresolvedTypeParameter(unresolved));
                    }
                }
                InstanceSubstitution::Effect { effects, .. } => {
                    if let Some(unresolved) = effects.unresolved_variable() {
                        return Err(InstanceKeyError::UnresolvedEffectVariable(unresolved));
                    }
                }
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

/// A Stage 2 site recipe whose substitutions or evidence may still be
/// unresolved. `resolve` either produces a concrete `InstanceKey` or reports
/// the unresolved input at the requesting record's origin.
#[derive(Debug, Clone)]
pub(crate) struct InstanceRequest {
    pub function: FunctionId,
    pub origin: Origin,
    pub substitutions: CallSubstitutions,
    pub evidence: Option<TraitEvidence>,
}

impl InstanceRequest {
    pub(crate) fn new(
        function: FunctionId,
        origin: Origin,
        substitutions: CallSubstitutions,
        evidence: Option<TraitEvidence>,
    ) -> Self {
        InstanceRequest {
            function,
            origin,
            substitutions,
            evidence,
        }
    }

    pub(crate) fn resolve(&self) -> Result<InstanceKey, Diagnostic> {
        let mut substitutions =
            Vec::with_capacity(self.substitutions.types.len() + self.substitutions.effects.len());
        for substitution in &self.substitutions.types {
            substitutions.push(InstanceSubstitution::Type {
                parameter: substitution.parameter,
                value: CanonicalType::concrete(&substitution.value_type, &self.origin)?,
            });
        }
        for substitution in &self.substitutions.effects {
            substitutions.push(InstanceSubstitution::Effect {
                parameter: substitution.parameter,
                effects: CanonicalEffectSet::concrete(&substitution.effects, &self.origin)?,
            });
        }
        InstanceKey::new(
            self.function,
            substitutions,
            self.evidence
                .as_ref()
                .map(|evidence| self.canonical_evidence(evidence))
                .transpose()?,
        )
        .map_err(|error| origin_diagnostic(&self.origin, error.message()))
    }

    fn canonical_evidence(
        &self,
        evidence: &TraitEvidence,
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
                arguments: self.canonical_arguments(arguments)?,
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
                arguments: self.canonical_arguments(arguments)?,
            }),
            TraitEvidence::DeclaredBound { trait_id, .. } => Err(origin_diagnostic(
                &self.origin,
                format!(
                    "trait {} evidence is still a declared bound; Stage 3.2 must resolve it before an instance key exists",
                    trait_id.0
                ),
            )),
            TraitEvidence::RejectedImplementation { trait_id, .. } => Err(origin_diagnostic(
                &self.origin,
                format!(
                    "trait {} evidence is a negative implementation and never forms an instance key",
                    trait_id.0
                ),
            )),
        }
    }

    fn canonical_arguments(
        &self,
        arguments: &[CheckedType],
    ) -> Result<Vec<CanonicalType>, Diagnostic> {
        arguments
            .iter()
            .map(|argument| CanonicalType::concrete(argument, &self.origin))
            .collect()
    }
}

/// The canonical adapter a callable value needs. Mirrors
/// `LoweredCallableAdapter` so artifact keys never depend on the lowered
/// record's own enum ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CanonicalAdapterKind {
    None,
    Constructor,
    External,
    Curried,
    NestedClosure,
    ImplicitThunk,
}

impl From<LoweredCallableAdapter> for CanonicalAdapterKind {
    fn from(adapter: LoweredCallableAdapter) -> Self {
        match adapter {
            LoweredCallableAdapter::None => CanonicalAdapterKind::None,
            LoweredCallableAdapter::Constructor => CanonicalAdapterKind::Constructor,
            LoweredCallableAdapter::External => CanonicalAdapterKind::External,
            LoweredCallableAdapter::Curried => CanonicalAdapterKind::Curried,
            LoweredCallableAdapter::NestedClosure => CanonicalAdapterKind::NestedClosure,
            LoweredCallableAdapter::ImplicitThunk => CanonicalAdapterKind::ImplicitThunk,
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

/// Generated-artifact request keys. The variant is the namespace: a
/// constructor adapter and a structural method can never compare or hash
/// equal, even when their numeric IDs coincide.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ArtifactRequestKey {
    ConstructorAdapter(ConstructorAdapterKey),
    StructuralMethod(StructuralMethodKey),
}

/// The complete namespace separation for Stage 3.3: source-function instances
/// and generated artifacts are distinct key families.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum SpecializationKey {
    Instance(InstanceKey),
    Artifact(ArtifactRequestKey),
}

/// Version byte of the canonical key encoding. Any change to the encoding
/// rules requires bumping this constant so old and new bytes can never be
/// silently compared as the same identity.
pub(crate) const SPECIALIZATION_KEY_ENCODING_VERSION: u8 = 1;

const INSTANCE_FAMILY_TAG: u8 = 0;
const ARTIFACT_FAMILY_TAG: u8 = 1;
const CONSTRUCTOR_ADAPTER_ARTIFACT_TAG: u8 = 0;
const STRUCTURAL_METHOD_ARTIFACT_TAG: u8 = 1;

impl CanonicalType {
    /// Appends the explicitly tagged canonical encoding of this value. The
    /// encoding is injective over the structural model: tags, lengths, and
    /// ordered fields make every semantic distinction byte-distinct.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            CanonicalType::Never => out.push(0),
            CanonicalType::I8 => out.push(1),
            CanonicalType::I16 => out.push(2),
            CanonicalType::I32 => out.push(3),
            CanonicalType::I64 => out.push(4),
            CanonicalType::U8 => out.push(5),
            CanonicalType::U16 => out.push(6),
            CanonicalType::U32 => out.push(7),
            CanonicalType::U64 => out.push(8),
            CanonicalType::ISize => out.push(9),
            CanonicalType::USize => out.push(10),
            CanonicalType::F32 => out.push(11),
            CanonicalType::F64 => out.push(12),
            CanonicalType::NumberLiteral(value) => {
                out.push(13);
                encode_u64(*value, out);
            }
            CanonicalType::String => out.push(14),
            CanonicalType::StringLiteralSet(values) => {
                out.push(15);
                encode_usize(values.len(), out);
                for value in values {
                    encode_string(value, out);
                }
            }
            CanonicalType::Ref(payload) => {
                out.push(16);
                payload.encode(out);
            }
            CanonicalType::Slice(payload) => {
                out.push(17);
                payload.encode(out);
            }
            CanonicalType::Buffer(payload) => {
                out.push(18);
                payload.encode(out);
            }
            CanonicalType::Array { element, count } => {
                out.push(19);
                element.encode(out);
                count.encode(out);
            }
            CanonicalType::CString => out.push(20),
            CanonicalType::CChar => out.push(21),
            CanonicalType::Parameter(parameter) => {
                out.push(22);
                encode_usize(parameter.0, out);
            }
            CanonicalType::Nominal {
                kind,
                id,
                arguments,
            } => {
                out.push(23);
                out.push(match kind {
                    CanonicalNominalKind::TypeConstructor => 0,
                    CanonicalNominalKind::Opaque => 1,
                    CanonicalNominalKind::Distinct => 2,
                });
                encode_usize(id.0, out);
                encode_types(arguments, out);
            }
            CanonicalType::CPointer { pointee } => {
                out.push(24);
                pointee.encode(out);
            }
            CanonicalType::Product { elements, variadic } => {
                out.push(25);
                encode_usize(elements.len(), out);
                for element in elements {
                    match &element.name {
                        Some(name) => {
                            out.push(1);
                            encode_string(name, out);
                        }
                        None => out.push(0),
                    }
                    element.value_type.encode(out);
                }
                out.push(u8::from(*variadic));
            }
            CanonicalType::Sum { alternatives } => {
                out.push(26);
                encode_types(alternatives, out);
            }
            CanonicalType::Function(function) => {
                out.push(27);
                function.encode(out);
            }
        }
    }
}

impl CanonicalFunctionType {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(0);
        self.parameter.encode(out);
        out.push(match self.parameter_style {
            CanonicalParameterStyle::Single => 0,
            CanonicalParameterStyle::Juxtaposed => 1,
        });
        encode_usize(self.mutations.len(), out);
        for mutation in &self.mutations {
            encode_mutation(*mutation, out);
        }
        encode_usize(self.moves.len(), out);
        for mutation in &self.moves {
            encode_mutation(*mutation, out);
        }
        self.effects.encode(out);
        self.result.encode(out);
    }
}

impl CanonicalEffectSet {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(1);
        match self.variable {
            Some(variable) => {
                out.push(1);
                encode_usize(variable.0, out);
            }
            None => out.push(0),
        }
        encode_usize(self.resources.len(), out);
        for resource in &self.resources {
            resource.encode(out);
        }
        match self.state {
            Some(state) => {
                out.push(1);
                out.push(match state {
                    CanonicalStateEffect::Read => 0,
                    CanonicalStateEffect::Write => 1,
                    CanonicalStateEffect::ReadWrite => 2,
                });
            }
            None => out.push(0),
        }
    }
}

impl CanonicalResource {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(2);
        self.value_type.encode(out);
        out.push(u8::from(self.mutable));
    }
}

impl CanonicalEvidence {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            CanonicalEvidence::ExplicitImplementation {
                trait_id,
                method,
                function,
                arguments,
            } => {
                out.push(0);
                encode_usize(trait_id.0, out);
                encode_usize(method.0, out);
                encode_usize(function.0, out);
                encode_types(arguments, out);
            }
            CanonicalEvidence::Structural {
                trait_id,
                method,
                structural,
                arguments,
            } => {
                out.push(1);
                encode_usize(trait_id.0, out);
                encode_usize(method.0, out);
                out.push(structural_tag(*structural));
                encode_types(arguments, out);
            }
        }
    }
}

impl InstanceKey {
    /// The versioned canonical byte encoding of this key. Equal keys always
    /// produce identical bytes; distinct keys cannot share bytes because every
    /// identity input is tagged and length-prefixed in order.
    pub(crate) fn canonical_encoding(&self) -> Vec<u8> {
        let mut out = vec![SPECIALIZATION_KEY_ENCODING_VERSION, INSTANCE_FAMILY_TAG];
        encode_usize(self.function.0, &mut out);
        encode_usize(self.substitutions.len(), &mut out);
        for substitution in &self.substitutions {
            match substitution {
                InstanceSubstitution::Type { parameter, value } => {
                    out.push(0);
                    encode_usize(parameter.0, &mut out);
                    value.encode(&mut out);
                }
                InstanceSubstitution::Effect { parameter, effects } => {
                    out.push(1);
                    encode_usize(parameter.0, &mut out);
                    effects.encode(&mut out);
                }
            }
        }
        match &self.evidence {
            Some(evidence) => {
                out.push(1);
                evidence.encode(&mut out);
            }
            None => out.push(0),
        }
        out
    }
}

impl ConstructorAdapterKey {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(CONSTRUCTOR_ADAPTER_ARTIFACT_TAG);
        encode_usize(self.symbol.0, out);
        encode_usize(self.type_id.0, out);
        out.push(adapter_tag(self.adapter));
        self.callable_type.encode(out);
    }
}

impl StructuralMethodKey {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(STRUCTURAL_METHOD_ARTIFACT_TAG);
        out.push(structural_tag(self.structural));
        encode_usize(self.trait_id.0, out);
        encode_usize(self.method.0, out);
        encode_types(&self.arguments, out);
        self.callable_type.encode(out);
    }
}

impl ArtifactRequestKey {
    /// The versioned canonical byte encoding of this artifact key, with the
    /// variant tag keeping constructor and structural namespaces distinct.
    pub(crate) fn canonical_encoding(&self) -> Vec<u8> {
        let mut out = vec![SPECIALIZATION_KEY_ENCODING_VERSION, ARTIFACT_FAMILY_TAG];
        match self {
            ArtifactRequestKey::ConstructorAdapter(key) => key.encode(&mut out),
            ArtifactRequestKey::StructuralMethod(key) => key.encode(&mut out),
        }
        out
    }
}

impl SpecializationKey {
    /// The versioned canonical byte encoding across both key families.
    pub(crate) fn canonical_encoding(&self) -> Vec<u8> {
        match self {
            SpecializationKey::Instance(key) => key.canonical_encoding(),
            SpecializationKey::Artifact(key) => key.canonical_encoding(),
        }
    }
}

fn encode_u64(value: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn encode_usize(value: usize, out: &mut Vec<u8>) {
    encode_u64(value as u64, out);
}

fn encode_string(value: &str, out: &mut Vec<u8>) {
    encode_usize(value.len(), out);
    out.extend_from_slice(value.as_bytes());
}

fn encode_types(values: &[CanonicalType], out: &mut Vec<u8>) {
    encode_usize(values.len(), out);
    for value in values {
        value.encode(out);
    }
}

fn encode_mutation(mutation: CanonicalMutation, out: &mut Vec<u8>) {
    match mutation {
        CanonicalMutation::Whole => out.push(0),
        CanonicalMutation::Element(index) => {
            out.push(1);
            encode_usize(index, out);
        }
    }
}

fn adapter_tag(adapter: CanonicalAdapterKind) -> u8 {
    match adapter {
        CanonicalAdapterKind::None => 0,
        CanonicalAdapterKind::Constructor => 1,
        CanonicalAdapterKind::External => 2,
        CanonicalAdapterKind::Curried => 3,
        CanonicalAdapterKind::NestedClosure => 4,
        CanonicalAdapterKind::ImplicitThunk => 5,
    }
}

fn structural_tag(structural: StructuralTraitMethod) -> u8 {
    match structural {
        StructuralTraitMethod::Debug => 0,
        StructuralTraitMethod::Index => 1,
        StructuralTraitMethod::DerefIndex => 2,
        StructuralTraitMethod::MutateIndex => 3,
        StructuralTraitMethod::DerefMutateIndex => 4,
        StructuralTraitMethod::IntoIterator => 5,
        StructuralTraitMethod::Iterator => 6,
    }
}

/// The append-only position of a reserved key. Ordinals are assigned in
/// first-discovery order within their family and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SpecializationOrdinal(usize);

impl SpecializationOrdinal {
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
#[derive(Debug, Default)]
pub(crate) struct SpecializationCatalog {
    instances: Vec<InstanceKey>,
    instance_lookup: HashMap<InstanceKey, usize>,
    artifacts: Vec<ArtifactRequestKey>,
    artifact_lookup: HashMap<ArtifactRequestKey, usize>,
}

impl SpecializationCatalog {
    pub(crate) fn reserve_instance(&mut self, key: InstanceKey) -> SpecializationOrdinal {
        if let Some(ordinal) = self.instance_lookup.get(&key) {
            return SpecializationOrdinal(*ordinal);
        }
        let ordinal = SpecializationOrdinal(self.instances.len());
        self.instance_lookup.insert(key.clone(), ordinal.0);
        self.instances.push(key);
        ordinal
    }

    pub(crate) fn reserve_artifact(&mut self, key: ArtifactRequestKey) -> SpecializationOrdinal {
        if let Some(ordinal) = self.artifact_lookup.get(&key) {
            return SpecializationOrdinal(*ordinal);
        }
        let ordinal = SpecializationOrdinal(self.artifacts.len());
        self.artifact_lookup.insert(key.clone(), ordinal.0);
        self.artifacts.push(key);
        ordinal
    }

    pub(crate) fn instance(&self, ordinal: SpecializationOrdinal) -> Option<&InstanceKey> {
        self.instances.get(ordinal.0)
    }

    pub(crate) fn artifact(&self, ordinal: SpecializationOrdinal) -> Option<&ArtifactRequestKey> {
        self.artifacts.get(ordinal.0)
    }

    pub(crate) fn instances(&self) -> impl Iterator<Item = (SpecializationOrdinal, &InstanceKey)> {
        self.instances
            .iter()
            .enumerate()
            .map(|(index, key)| (SpecializationOrdinal(index), key))
    }

    pub(crate) fn artifacts(
        &self,
    ) -> impl Iterator<Item = (SpecializationOrdinal, &ArtifactRequestKey)> {
        self.artifacts
            .iter()
            .enumerate()
            .map(|(index, key)| (SpecializationOrdinal(index), key))
    }

    /// The planned emitted symbol names in emission order. Names are
    /// collision-checked: a repeated name is reported instead of silently
    /// aliasing two semantic keys.
    pub(crate) fn planned_names(&self) -> Result<Vec<String>, SpecializationNameCollision> {
        let mut names = Vec::with_capacity(self.instances.len() + self.artifacts.len());
        let mut seen = HashSet::new();
        for (ordinal, _) in self.instances() {
            let name = format!("__staple_instance_{}", ordinal.0);
            if !seen.insert(name.clone()) {
                return Err(SpecializationNameCollision { name });
            }
            names.push(name);
        }
        for (ordinal, key) in self.artifacts() {
            let name = match key {
                ArtifactRequestKey::ConstructorAdapter(_) => {
                    format!("__staple_constructor_adapter_{}", ordinal.0)
                }
                ArtifactRequestKey::StructuralMethod(method) => format!(
                    "__staple_structural_{}_{}",
                    structural_name(method.structural),
                    ordinal.0
                ),
            };
            if !seen.insert(name.clone()) {
                return Err(SpecializationNameCollision { name });
            }
            names.push(name);
        }
        Ok(names)
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
        LoweredTraitImplementationId,
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

    fn canonical(value_type: &CheckedType) -> CanonicalType {
        CanonicalType::template(value_type, &requesting_origin())
            .expect("template conversion should succeed")
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
    fn template_conversion_covers_every_checked_type_variant() {
        let function = function_type(
            CheckedType::Product(CheckedProductType {
                elements: vec![element(Some("x"), CheckedType::I32, None)],
                variadic: false,
            }),
            FunctionParameterStyle::Juxtaposed,
            vec![CheckedMutation::Element(0)],
            vec![CheckedMutation::Whole],
            CheckedEffectSet {
                variable: Some(crate::CheckedEffectVariable {
                    id: TypeParameterId(4),
                    name: "E".to_owned(),
                }),
                resources: vec![CheckedResource {
                    value_type: CheckedType::I32,
                    mutable: true,
                }],
                state: Some(CheckedStateEffect::ReadWrite),
            },
            CheckedType::Never,
        );
        let variants = [
            CheckedType::Never,
            CheckedType::I8,
            CheckedType::I16,
            CheckedType::I32,
            CheckedType::I64,
            CheckedType::U8,
            CheckedType::U16,
            CheckedType::U32,
            CheckedType::U64,
            CheckedType::ISize,
            CheckedType::USize,
            CheckedType::F32,
            CheckedType::F64,
            CheckedType::NumberLiteral(9),
            CheckedType::String,
            CheckedType::StringLiteralSet(vec!["a".to_owned()]),
            CheckedType::Ref(Box::new(CheckedType::I32)),
            CheckedType::Slice(Box::new(CheckedType::I32)),
            CheckedType::Buffer(Box::new(CheckedType::I32)),
            CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(3)),
            },
            CheckedType::CString,
            CheckedType::CChar,
            CheckedType::Parameter {
                id: TypeParameterId(1),
                name: "T".to_owned(),
                sized: true,
            },
            nominal(2, "Named"),
            opaque(3, "Opaque"),
            CheckedType::CPointer {
                pointee: Box::new(CheckedType::I32),
            },
            CheckedType::Product(CheckedProductType {
                elements: vec![element(None, CheckedType::I32, None)],
                variadic: true,
            }),
            CheckedType::Sum(CheckedSumType {
                alternatives: vec![CheckedType::I32, CheckedType::F64],
            }),
            CheckedType::Function(function),
            distinct(4, "Distinct", Vec::new(), CheckedType::I32),
        ];
        for value_type in &variants {
            CanonicalType::template(value_type, &requesting_origin()).unwrap_or_else(
                |diagnostic| panic!("{value_type} failed to convert: {diagnostic}"),
            );
        }
        for placeholder in [CheckedType::Inferred, CheckedType::Error] {
            let diagnostic = CanonicalType::template(&placeholder, &requesting_origin())
                .expect_err("checker placeholders never form keys");
            assert_eq!(diagnostic.span, requesting_origin().span);
        }
    }

    #[test]
    fn equivalent_checked_types_deduplicate() {
        assert_eq!(
            canonical(&nominal(7, "First")),
            canonical(&nominal(7, "Second"))
        );
        assert_eq!(
            canonical(&opaque(8, "First")),
            canonical(&opaque(8, "Second"))
        );
        assert_eq!(
            canonical(&distinct(
                9,
                "First",
                vec![CheckedType::I32],
                CheckedType::I32,
            )),
            canonical(&distinct(
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
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![element(Some("x"), CheckedType::I32, None)],
                variadic: false,
            })),
            canonical(&CheckedType::Product(CheckedProductType {
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
        assert_ne!(
            canonical(&nominal(1, "Same")),
            canonical(&nominal(2, "Same"))
        );
        assert_ne!(
            canonical(&nominal(1, "Same")),
            canonical(&opaque(1, "Same")),
            "nominal kind is part of identity"
        );
        assert_ne!(
            canonical(&CheckedType::TypeConstructor {
                id: TypeId(1),
                name: "Same".to_owned(),
                arguments: vec![CheckedType::I32],
            }),
            canonical(&CheckedType::TypeConstructor {
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
        assert_ne!(canonical(&named(Some("x"))), canonical(&named(Some("y"))));
        assert_ne!(canonical(&named(Some("x"))), canonical(&named(None)));
        assert_ne!(
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![
                    element(None, CheckedType::I32, None),
                    element(None, CheckedType::I64, None),
                ],
                variadic: false,
            })),
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![
                    element(None, CheckedType::I64, None),
                    element(None, CheckedType::I32, None),
                ],
                variadic: false,
            })),
            "field order is identity"
        );
        assert_ne!(
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![element(None, CheckedType::I32, None)],
                variadic: false,
            })),
            canonical(&CheckedType::Product(CheckedProductType {
                elements: vec![element(None, CheckedType::I32, None)],
                variadic: true,
            })),
            "the variadic flag is identity"
        );
        assert_ne!(
            canonical(&CheckedType::Sum(CheckedSumType {
                alternatives: vec![CheckedType::I32, CheckedType::I64],
            })),
            canonical(&CheckedType::Sum(CheckedSumType {
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
            canonical(&CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(2)),
            }),
            canonical(&CheckedType::Array {
                element: Box::new(CheckedType::I32),
                count: Box::new(CheckedType::NumberLiteral(3)),
            }),
            "repeated counts are identity"
        );
        assert_ne!(
            canonical(&CheckedType::NumberLiteral(1)),
            canonical(&CheckedType::NumberLiteral(2)),
            "literal payloads are identity"
        );
        assert_ne!(
            canonical(&CheckedType::StringLiteralSet(vec![
                "a".to_owned(),
                "b".to_owned()
            ])),
            canonical(&CheckedType::StringLiteralSet(vec![
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
            canonical(&CheckedType::Function(function_type(
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
            canonical(&CheckedType::Function(function_type(
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
            canonical(&CheckedType::Function(function_type(
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
            canonical(&CheckedType::Function(function_type(
                CheckedType::I32,
                FunctionParameterStyle::Single,
                Vec::new(),
                Vec::new(),
                effects(Vec::new(), None),
                CheckedType::I32,
            ))),
            canonical(&CheckedType::Function(function_type(
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
    fn parameters_are_template_identity_only() {
        let first = CheckedType::Parameter {
            id: TypeParameterId(5),
            name: "First".to_owned(),
            sized: false,
        };
        let second = CheckedType::Parameter {
            id: TypeParameterId(5),
            name: "Second".to_owned(),
            sized: true,
        };
        assert_eq!(canonical(&first), canonical(&second));
        assert_eq!(
            canonical(&first),
            CanonicalType::Parameter(TypeParameterId(5))
        );

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
        let key = CanonicalEffectSet::template(&template, &requesting_origin())
            .expect("template keys retain effect-variable IDs");
        assert_eq!(key.variable, Some(TypeParameterId(3)));

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

    fn substitutions(
        types: &[(usize, CheckedType)],
        effects: &[(usize, CheckedEffectSet)],
    ) -> CallSubstitutions {
        CallSubstitutions {
            types: types
                .iter()
                .map(|(parameter, value_type)| crate::CallTypeSubstitution {
                    parameter: TypeParameterId(*parameter),
                    value_type: value_type.clone(),
                })
                .collect(),
            effects: effects
                .iter()
                .map(|(parameter, effects)| crate::CallEffectSubstitution {
                    parameter: TypeParameterId(*parameter),
                    effects: effects.clone(),
                })
                .collect(),
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

        let unresolved = InstanceKey::new(
            FunctionId(1),
            vec![InstanceSubstitution::Type {
                parameter: TypeParameterId(1),
                value: CanonicalType::Ref(Box::new(CanonicalType::Parameter(TypeParameterId(9)))),
            }],
            None,
        )
        .expect_err("a nested declared parameter is not concrete");
        assert_eq!(
            unresolved,
            InstanceKeyError::UnresolvedTypeParameter(TypeParameterId(9))
        );

        let unresolved_effect = InstanceKey::new(
            FunctionId(1),
            vec![InstanceSubstitution::Effect {
                parameter: TypeParameterId(1),
                effects: CanonicalEffectSet {
                    variable: Some(TypeParameterId(8)),
                    resources: Vec::new(),
                    state: None,
                },
            }],
            None,
        )
        .expect_err("a declared effect variable is not concrete");
        assert_eq!(
            unresolved_effect,
            InstanceKeyError::UnresolvedEffectVariable(TypeParameterId(8))
        );

        let unparameterized = instance_key(3, Vec::new(), None);
        assert!(unparameterized.substitutions().is_empty());
        assert!(unparameterized.evidence().is_none());
        assert_eq!(unparameterized, instance_key(3, Vec::new(), None));
    }

    #[test]
    fn instance_requests_resolve_or_fail_at_their_origin() {
        let origin = requesting_origin();

        let explicit = InstanceRequest::new(
            FunctionId(4),
            origin.clone(),
            substitutions(&[(1, CheckedType::I32)], &[]),
            None,
        );
        let key = explicit.resolve().expect("resolved request");
        assert_eq!(key.function(), FunctionId(4));
        assert_eq!(key.substitutions().len(), 1);

        let explicit_evidence = InstanceRequest::new(
            FunctionId(4),
            origin.clone(),
            CallSubstitutions::default(),
            Some(TraitEvidence::ExplicitImplementation {
                trait_id: TraitId(5),
                implementation: LoweredTraitImplementationId::for_test(1),
                method: TraitMethodId(6),
                function: FunctionId(7),
                arguments: vec![CheckedType::I32],
            }),
        );
        assert!(matches!(
            explicit_evidence
                .resolve()
                .expect("resolved evidence")
                .evidence(),
            Some(CanonicalEvidence::ExplicitImplementation {
                function: FunctionId(7),
                ..
            })
        ));

        let structural_evidence = InstanceRequest::new(
            FunctionId(4),
            origin.clone(),
            CallSubstitutions::default(),
            Some(TraitEvidence::Structural {
                trait_id: TraitId(5),
                method: TraitMethodId(6),
                structural: StructuralTraitMethod::Debug,
                arguments: vec![CheckedType::Product(CheckedProductType {
                    elements: vec![element(Some("x"), CheckedType::I32, None)],
                    variadic: false,
                })],
            }),
        );
        assert!(matches!(
            structural_evidence
                .resolve()
                .expect("resolved structural evidence")
                .evidence(),
            Some(CanonicalEvidence::Structural {
                structural: StructuralTraitMethod::Debug,
                ..
            })
        ));

        let declared = InstanceRequest::new(
            FunctionId(4),
            origin.clone(),
            CallSubstitutions::default(),
            Some(TraitEvidence::DeclaredBound {
                trait_id: TraitId(2),
                method: None,
                arguments: Vec::new(),
                prerequisites: Vec::new(),
            }),
        );
        let diagnostic = declared
            .resolve()
            .expect_err("declared bounds are unresolved");
        assert_eq!(diagnostic.span, origin.span);
        assert!(diagnostic.message.contains("declared bound"));

        let rejected = InstanceRequest::new(
            FunctionId(4),
            origin.clone(),
            CallSubstitutions::default(),
            Some(TraitEvidence::RejectedImplementation {
                trait_id: TraitId(2),
                implementation: LoweredTraitImplementationId::for_test(0),
                arguments: Vec::new(),
            }),
        );
        let diagnostic = rejected
            .resolve()
            .expect_err("negative evidence is not a key");
        assert_eq!(diagnostic.span, origin.span);
        assert!(diagnostic.message.contains("negative implementation"));

        let unresolved = InstanceRequest::new(
            FunctionId(4),
            origin.clone(),
            substitutions(
                &[(
                    1,
                    CheckedType::Parameter {
                        id: TypeParameterId(3),
                        name: "T".to_owned(),
                        sized: false,
                    },
                )],
                &[],
            ),
            None,
        );
        let diagnostic = unresolved
            .resolve()
            .expect_err("unresolved substitutions are not a key");
        assert_eq!(diagnostic.span, origin.span);

        let unresolved_arguments = InstanceRequest::new(
            FunctionId(4),
            origin.clone(),
            CallSubstitutions::default(),
            Some(TraitEvidence::Structural {
                trait_id: TraitId(5),
                method: TraitMethodId(6),
                structural: StructuralTraitMethod::Index,
                arguments: vec![CheckedType::Parameter {
                    id: TypeParameterId(11),
                    name: "K".to_owned(),
                    sized: false,
                }],
            }),
        );
        assert_eq!(
            unresolved_arguments
                .resolve()
                .expect_err("unresolved evidence arguments are not a key")
                .span,
            origin.span
        );
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
        let instance = instance_key(1, vec![type_substitution(1, CheckedType::I32)], None);
        assert_ne!(
            SpecializationKey::Instance(instance.clone()),
            SpecializationKey::Artifact(ArtifactRequestKey::ConstructorAdapter(constructor_key)),
            "source-function instances and generated artifacts never share a namespace"
        );
        assert_eq!(
            SpecializationKey::Instance(instance.clone()),
            SpecializationKey::Instance(instance)
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
    fn canonical_encoding_is_versioned_stable_and_injective() {
        let instance = instance_key(1, vec![type_substitution(1, CheckedType::I32)], None);
        let encoding = instance.canonical_encoding();
        assert_eq!(encoding[0], SPECIALIZATION_KEY_ENCODING_VERSION);
        assert_eq!(
            encoding,
            instance_key(1, vec![type_substitution(1, CheckedType::I32)], None)
                .canonical_encoding()
        );
        assert_eq!(
            SpecializationKey::Instance(instance.clone()).canonical_encoding(),
            encoding
        );
        assert_eq!(
            instance_key(
                1,
                vec![
                    type_substitution(2, CheckedType::I64),
                    type_substitution(1, CheckedType::I32),
                ],
                None
            )
            .canonical_encoding(),
            instance_key(
                1,
                vec![
                    type_substitution(1, CheckedType::I32),
                    type_substitution(2, CheckedType::I64),
                ],
                None
            )
            .canonical_encoding(),
            "construction order never changes the encoding"
        );

        let other = instance_key(1, vec![type_substitution(1, CheckedType::I64)], None);
        assert_ne!(encoding, other.canonical_encoding());
        assert_ne!(
            encoding,
            instance_key(
                1,
                vec![effect_substitution(1, &CheckedEffectSet::default())],
                None
            )
            .canonical_encoding(),
            "type and effect entries with the same parameter still differ"
        );
        assert_ne!(
            encoding,
            instance_key(
                1,
                vec![type_substitution(1, CheckedType::I32)],
                Some(CanonicalEvidence::Structural {
                    trait_id: TraitId(1),
                    method: TraitMethodId(2),
                    structural: StructuralTraitMethod::Debug,
                    arguments: vec![concrete(&CheckedType::I32)],
                })
            )
            .canonical_encoding(),
            "evidence participates in the encoding"
        );

        let artifact = ArtifactRequestKey::ConstructorAdapter(
            ConstructorAdapterKey::new(
                SymbolId(1),
                TypeId(1),
                LoweredCallableAdapter::Constructor,
                &simple_callable(),
                &requesting_origin(),
            )
            .expect("concrete constructor artifact"),
        );
        let artifact_encoding = artifact.canonical_encoding();
        assert_eq!(artifact_encoding[0], SPECIALIZATION_KEY_ENCODING_VERSION);
        assert_ne!(artifact_encoding, encoding);
        assert_eq!(
            SpecializationKey::Artifact(artifact.clone()).canonical_encoding(),
            artifact_encoding
        );
        assert_ne!(
            SpecializationKey::Instance(instance).canonical_encoding(),
            SpecializationKey::Artifact(artifact).canonical_encoding(),
            "the family tag separates instances from artifacts"
        );
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
                .map(|(_, key)| key.canonical_encoding())
                .collect::<Vec<_>>(),
            right
                .instances()
                .map(|(_, key)| key.canonical_encoding())
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
}
