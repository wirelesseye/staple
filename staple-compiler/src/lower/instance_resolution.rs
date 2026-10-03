//! Relevant-parameter collection, substitution composition, and
//! declared trait-evidence resolution for one specialization request.
//!
//! This module walks the owned lowering `LoweredProgram`; it never consults
//! `TypedModule`, LLVM state, or debug-formatted keys. Each function template
//! is scanned under its own `FunctionId`, so a nested body contributes through
//! the record that constructs or invokes it, not as ordinary children.

use super::internal_invariant;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use staple_syntax::Diagnostic;

use crate::specialization::{
    CanonicalEffectSet, CanonicalType, InstanceKey, InstanceSubstitution, canonical_evidence,
};
use crate::{
    CallSubstitutions, CheckedEffectSet, CheckedFunctionType, CheckedTraitBound,
    CheckedTraitImplementation, CheckedType, FunctionId, StructuralTraitMethod, TraitEvidence,
    TraitId, TraitMethodId, TypeParameterId, contains_inferred_type, contains_type_parameter,
    drop_implementation_applies, effect_substitution_type, effect_substitution_value,
    infer_type_parameters, is_copy_type, is_default_type, merge_types, structural_trait_arguments,
    substitute_effect_set, substitute_type,
};

use super::*;

/// The type and effect parameters whose concrete value can change one
/// template's signature, body metadata, captures, layout, or selected
/// evidence. Iteration is in ascending parameter-ID order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RelevantParameters {
    types: BTreeSet<TypeParameterId>,
    effects: BTreeSet<TypeParameterId>,
    /// Declaration names retained for diagnostics only; never identity.
    names: BTreeMap<TypeParameterId, String>,
}

impl RelevantParameters {
    fn notice_type(&mut self, parameter: TypeParameterId, name: &str) {
        self.types.insert(parameter);
        self.names
            .entry(parameter)
            .or_insert_with(|| name.to_owned());
    }

    fn notice_effect(&mut self, parameter: TypeParameterId, name: &str) {
        self.effects.insert(parameter);
        self.names
            .entry(parameter)
            .or_insert_with(|| name.to_owned());
    }

    /// The declared name of a relevant parameter, when it was observed.
    pub(crate) fn name(&self, parameter: TypeParameterId) -> Option<&str> {
        self.names.get(&parameter).map(String::as_str)
    }

    /// Diagnostic text for one parameter: its declared name when known.
    pub(crate) fn display(&self, parameter: TypeParameterId) -> String {
        match self.name(parameter) {
            Some(name) => format!("`{name}`"),
            None => format!("id {}", parameter.0),
        }
    }

    #[cfg(test)]
    pub(crate) fn contains_type(&self, parameter: TypeParameterId) -> bool {
        self.types.contains(&parameter)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.types.is_empty() && self.effects.is_empty()
    }

    pub(crate) fn type_parameters(&self) -> impl Iterator<Item = TypeParameterId> + '_ {
        self.types.iter().copied()
    }

    pub(crate) fn effect_parameters(&self) -> impl Iterator<Item = TypeParameterId> + '_ {
        self.effects.iter().copied()
    }
}

#[cfg(test)]
/// Every record family the parameter collector visits. The collector's
/// exhaustive matches over the lowered enums are the compile-time half of the
/// coverage contract; this list is the declared decision table checked by
/// `parameter_record_families` tests. Adding a lowered variant requires adding
/// its family here (or an explicit no-parameter decision in the collector).
pub(crate) const PARAMETER_RECORD_FAMILIES: &[&str] = &[
    "function.signature",
    "function.parameter-pattern",
    "function.parameters",
    "function.captures",
    "function.coroutine-plan",
    "expression.header",
    "expression.block",
    "expression.name",
    "expression.integer",
    "expression.float",
    "expression.string",
    "expression.cstring",
    "expression.access",
    "expression.product",
    "expression.repeated-product",
    "expression.satisfies",
    "expression.logical",
    "expression.loop",
    "expression.match",
    "expression.index",
    "expression.string-template",
    "expression.call",
    "expression.callable-value",
    "expression.resource",
    "expression.with",
    "expression.coro",
    "expression.await",
    "item.binding",
    "item.pattern-binding",
    "item.assignment",
    "item.return",
    "item.break",
    "item.continue",
    "item.expression",
    "pattern",
    "place",
    "resource-provider",
    "resource-use",
    "reactive-operation",
    "reactive-callback",
    "trait-evidence",
];

/// Where a substitution value came from. Retained so a conflict can name the
/// two disagreeing sources instead of silently overwriting one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubstitutionSource {
    /// A value already concrete in the enclosing instance environment.
    EnclosingInstance,
    /// A value recorded on the lowering call or callable-value site recipe.
    Site,
    /// A value inferred from the complete checked callable type at the site.
    Inferred,
}

impl SubstitutionSource {
    pub(crate) fn description(self) -> &'static str {
        match self {
            SubstitutionSource::EnclosingInstance => "the enclosing instance",
            SubstitutionSource::Site => "the call site",
            SubstitutionSource::Inferred => "the checked callable type",
        }
    }
}

/// One substitution value. Type and effect-row parameters stay distinct: a
/// type parameter can never receive an effect row or the reverse.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SubstitutionValue {
    Type(CheckedType),
    Effects(CheckedEffectSet),
}

impl SubstitutionValue {
    fn kind_name(&self) -> &'static str {
        match self {
            SubstitutionValue::Type(_) => "a type",
            SubstitutionValue::Effects(_) => "an effect row",
        }
    }

    fn describe(&self) -> String {
        match self {
            SubstitutionValue::Type(value_type) => value_type.to_string(),
            SubstitutionValue::Effects(effects) => effects.to_string(),
        }
    }

    fn referenced_parameters(&self, out: &mut BTreeSet<TypeParameterId>) {
        match self {
            SubstitutionValue::Type(value_type) => collect_referenced_parameters(value_type, out),
            SubstitutionValue::Effects(effects) => {
                if let Some(variable) = &effects.variable {
                    out.insert(variable.id);
                }
                for resource in &effects.resources {
                    collect_referenced_parameters(&resource.value_type, out);
                }
            }
        }
    }

    /// A checker placeholder or a bare effect wildcard: never authoritative
    /// over a concrete value from another source.
    fn is_placeholder(&self) -> bool {
        match self {
            SubstitutionValue::Type(value_type) => {
                matches!(value_type, CheckedType::Inferred | CheckedType::Error)
            }
            SubstitutionValue::Effects(effects) => {
                effects.variable.is_some()
                    && effects.resources.is_empty()
                    && effects.state.is_none()
            }
        }
    }

    fn is_self_placeholder(&self, parameter: TypeParameterId) -> bool {
        match self {
            SubstitutionValue::Type(CheckedType::Parameter { id, .. }) => *id == parameter,
            SubstitutionValue::Effects(effects) => {
                effects
                    .variable
                    .as_ref()
                    .is_some_and(|variable| variable.id == parameter)
                    && effects.resources.is_empty()
                    && effects.state.is_none()
            }
            _ => false,
        }
    }

    fn substitute(&self, map: &HashMap<TypeParameterId, CheckedType>) -> SubstitutionValue {
        match self {
            SubstitutionValue::Type(value_type) => {
                SubstitutionValue::Type(substitute_type(value_type.clone(), map))
            }
            SubstitutionValue::Effects(effects) => {
                SubstitutionValue::Effects(substitute_effect_set(effects.clone(), map))
            }
        }
    }
}

/// One resolved entry of a `SubstitutionEnvironment`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SubstitutionEntry {
    pub source: SubstitutionSource,
    pub value: SubstitutionValue,
}

/// A resolved, conflict-free, concrete-or-chain environment for one request.
/// Values may still reference another parameter of the same request; the final
/// concreteness check rejects any that remain unresolved for a relevant
/// parameter.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SubstitutionEnvironment {
    entries: BTreeMap<TypeParameterId, SubstitutionEntry>,
}

impl SubstitutionEnvironment {
    pub(crate) fn entry(&self, parameter: TypeParameterId) -> Option<&SubstitutionEntry> {
        self.entries.get(&parameter)
    }

    pub(crate) fn type_value(&self, parameter: TypeParameterId) -> Option<&CheckedType> {
        match self.entries.get(&parameter).map(|entry| &entry.value) {
            Some(SubstitutionValue::Type(value_type)) => Some(value_type),
            _ => None,
        }
    }

    pub(crate) fn effect_value(&self, parameter: TypeParameterId) -> Option<&CheckedEffectSet> {
        match self.entries.get(&parameter).map(|entry| &entry.value) {
            Some(SubstitutionValue::Effects(effects)) => Some(effects),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (TypeParameterId, &SubstitutionEntry)> + '_ {
        self.entries
            .iter()
            .map(|(parameter, entry)| (*parameter, entry))
    }

    /// The checker-shaped substitution map: type entries as-is, effect entries
    /// encoded as the checker's error-shaped function carrier. The carrier
    /// never enters a key; it exists only so the checker's own
    /// `substitute_type`/`substitute_effect_set` can be reused verbatim.
    pub(crate) fn substitution_map(&self) -> HashMap<TypeParameterId, CheckedType> {
        self.entries
            .iter()
            .map(|(parameter, entry)| {
                let value = match &entry.value {
                    SubstitutionValue::Type(value_type) => value_type.clone(),
                    SubstitutionValue::Effects(effects) => {
                        effect_substitution_type(effects.clone())
                    }
                };
                (*parameter, value)
            })
            .collect()
    }

    /// Retains only the entries for the given relevant parameters. Values that
    /// transitively reference other outer parameters keep whatever entries
    /// were needed to resolve them; callers hold the unpruned environment
    /// while resolving.
    pub(crate) fn pruned(&self, relevant: &RelevantParameters) -> SubstitutionEnvironment {
        let mut entries = BTreeMap::new();
        for parameter in relevant
            .type_parameters()
            .chain(relevant.effect_parameters())
        {
            if let Some(entry) = self.entries.get(&parameter) {
                entries.insert(parameter, entry.clone());
            }
        }
        SubstitutionEnvironment { entries }
    }
}

fn collect_referenced_parameters(value_type: &CheckedType, out: &mut BTreeSet<TypeParameterId>) {
    match value_type {
        CheckedType::Parameter { id, .. } => {
            out.insert(*id);
        }
        CheckedType::Ref(payload)
        | CheckedType::Slice(payload)
        | CheckedType::Buffer(payload)
        | CheckedType::CPointer { pointee: payload } => collect_referenced_parameters(payload, out),
        CheckedType::Array { element, count } => {
            collect_referenced_parameters(element, out);
            collect_referenced_parameters(count, out);
        }
        CheckedType::TypeConstructor { arguments, .. } | CheckedType::Opaque { arguments, .. } => {
            for argument in arguments {
                collect_referenced_parameters(argument, out);
            }
        }
        CheckedType::Product(product) => {
            for element in &product.elements {
                collect_referenced_parameters(&element.value_type, out);
            }
        }
        CheckedType::Sum(sum) => {
            for alternative in &sum.alternatives {
                collect_referenced_parameters(alternative, out);
            }
        }
        CheckedType::Function(function) => {
            collect_referenced_parameters(&function.parameter, out);
            collect_referenced_effect_parameters(&function.effects, out);
            collect_referenced_parameters(&function.result, out);
        }
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => {
            for argument in arguments {
                collect_referenced_parameters(argument, out);
            }
            collect_referenced_parameters(representation, out);
        }
        CheckedType::Inferred
        | CheckedType::Error
        | CheckedType::Never
        | CheckedType::I32
        | CheckedType::I8
        | CheckedType::I16
        | CheckedType::I64
        | CheckedType::U8
        | CheckedType::U16
        | CheckedType::U32
        | CheckedType::U64
        | CheckedType::ISize
        | CheckedType::USize
        | CheckedType::F32
        | CheckedType::F64
        | CheckedType::NumberLiteral(_)
        | CheckedType::String
        | CheckedType::StringLiteralSet(_)
        | CheckedType::CString
        | CheckedType::CChar => {}
    }
}

fn collect_referenced_effect_parameters(
    effects: &CheckedEffectSet,
    out: &mut BTreeSet<TypeParameterId>,
) {
    if let Some(variable) = &effects.variable {
        out.insert(variable.id);
    }
    for resource in &effects.resources {
        collect_referenced_parameters(&resource.value_type, out);
    }
}

#[derive(Debug, Clone)]
struct SubstitutionCandidate {
    value: SubstitutionValue,
    source: SubstitutionSource,
}

/// Gathers raw candidates from the enclosing environment, the site recipe,
/// and the checked callable type, then resolves them transitively. Conflicts
/// are detected after substitution, so a chain (`T -> U`, `U -> I32`) agrees
/// with a later direct value instead of being misreported.
struct EnvironmentBuilder {
    candidates: BTreeMap<TypeParameterId, Vec<SubstitutionCandidate>>,
    origin: Origin,
}

impl EnvironmentBuilder {
    fn new(origin: &Origin) -> Self {
        EnvironmentBuilder {
            candidates: BTreeMap::new(),
            origin: origin.clone(),
        }
    }

    fn add(
        &mut self,
        parameter: TypeParameterId,
        value: SubstitutionValue,
        source: SubstitutionSource,
    ) {
        // `P -> P` is a tautology, not a substitution; keeping it would look
        // like a cycle and would also hide a genuinely missing value.
        if value.is_self_placeholder(parameter) {
            return;
        }
        self.candidates
            .entry(parameter)
            .or_default()
            .push(SubstitutionCandidate { value, source });
    }

    fn add_enclosing(&mut self, environment: &SubstitutionEnvironment) {
        for (parameter, entry) in environment.iter() {
            self.add(
                parameter,
                entry.value.clone(),
                SubstitutionSource::EnclosingInstance,
            );
        }
    }

    /// Adds the site recipe with every value already substituted through the
    /// enclosing environment, so a self-mapping like `T -> T` recorded by the
    /// checker at a recursive site resolves to the enclosing concrete value
    /// instead of masquerading as a cycle.
    fn add_site(
        &mut self,
        substitutions: &CallSubstitutions,
        enclosing: &HashMap<TypeParameterId, CheckedType>,
    ) {
        for substitution in &substitutions.types {
            let value_type = substitute_type(substitution.value_type.clone(), enclosing);
            match effect_substitution_value(&value_type) {
                Some(effects) => self.add(
                    substitution.parameter,
                    SubstitutionValue::Effects(effects.clone()),
                    SubstitutionSource::Site,
                ),
                None => self.add(
                    substitution.parameter,
                    SubstitutionValue::Type(value_type),
                    SubstitutionSource::Site,
                ),
            }
        }
        for substitution in &substitutions.effects {
            let effects = substitute_effect_set(substitution.effects.clone(), enclosing);
            self.add(
                substitution.parameter,
                SubstitutionValue::Effects(effects),
                SubstitutionSource::Site,
            );
        }
    }

    fn add_inferred(&mut self, inferred: HashMap<TypeParameterId, CheckedType>) {
        let mut entries = inferred.into_iter().collect::<Vec<_>>();
        entries.sort_by_key(|(parameter, _)| parameter.0);
        for (parameter, value_type) in entries {
            match effect_substitution_value(&value_type) {
                Some(effects) => self.add(
                    parameter,
                    SubstitutionValue::Effects(effects.clone()),
                    SubstitutionSource::Inferred,
                ),
                None => self.add(
                    parameter,
                    SubstitutionValue::Type(value_type),
                    SubstitutionSource::Inferred,
                ),
            }
        }
    }

    fn resolve(&self) -> Result<SubstitutionEnvironment, Diagnostic> {
        let order = self.resolution_order()?;
        let mut entries = BTreeMap::new();
        let mut map: HashMap<TypeParameterId, CheckedType> = HashMap::new();
        for parameter in order {
            let Some(candidates) = self.candidates.get(&parameter) else {
                continue;
            };
            let mut merged: Option<SubstitutionCandidate> = None;
            for candidate in candidates {
                let value = candidate.value.substitute(&map);
                merged = Some(match merged {
                    None => SubstitutionCandidate {
                        value,
                        source: candidate.source,
                    },
                    Some(existing) => SubstitutionCandidate {
                        value: merge_substitution_values(
                            &self.origin,
                            parameter,
                            existing.value,
                            existing.source,
                            value,
                            candidate.source,
                        )?
                        .substitute(&map),
                        source: existing.source,
                    },
                });
            }
            let Some(merged) = merged else {
                continue;
            };
            let encoded = match &merged.value {
                SubstitutionValue::Type(value_type) => value_type.clone(),
                SubstitutionValue::Effects(effects) => effect_substitution_type(effects.clone()),
            };
            map.insert(parameter, encoded);
            entries.insert(
                parameter,
                SubstitutionEntry {
                    source: merged.source,
                    value: merged.value,
                },
            );
        }
        Ok(SubstitutionEnvironment { entries })
    }

    /// Stably orders parameter resolution by ascending ID with dependencies
    /// first. A self-reference or longer cycle is reported before any
    /// substitution runs, rather than relying on repeated substitution to
    /// converge.
    fn resolution_order(&self) -> Result<Vec<TypeParameterId>, Diagnostic> {
        let mut dependencies: BTreeMap<TypeParameterId, BTreeSet<TypeParameterId>> =
            BTreeMap::new();
        let mut dependents: BTreeMap<TypeParameterId, BTreeSet<TypeParameterId>> = BTreeMap::new();
        for (parameter, candidates) in &self.candidates {
            let mut referenced = BTreeSet::new();
            for candidate in candidates {
                candidate.value.referenced_parameters(&mut referenced);
            }
            for dependency in referenced {
                if dependency == *parameter {
                    return Err(self.cycle_diagnostic(&[*parameter]));
                }
                if self.candidates.contains_key(&dependency) {
                    dependencies
                        .entry(*parameter)
                        .or_default()
                        .insert(dependency);
                    dependents.entry(dependency).or_default().insert(*parameter);
                }
            }
        }
        let mut indegree: BTreeMap<TypeParameterId, usize> = self
            .candidates
            .keys()
            .map(|parameter| {
                (
                    *parameter,
                    dependencies.get(parameter).map_or(0, BTreeSet::len),
                )
            })
            .collect();
        let mut ready: BTreeSet<TypeParameterId> = indegree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(parameter, _)| *parameter)
            .collect();
        let mut order = Vec::with_capacity(self.candidates.len());
        while let Some(parameter) = ready.iter().next().copied() {
            ready.remove(&parameter);
            order.push(parameter);
            if let Some(children) = dependents.get(&parameter) {
                for child in children {
                    if let Some(degree) = indegree.get_mut(child) {
                        *degree -= 1;
                        if *degree == 0 {
                            ready.insert(*child);
                        }
                    }
                }
            }
        }
        if order.len() != self.candidates.len() {
            let remaining: BTreeSet<TypeParameterId> = self
                .candidates
                .keys()
                .filter(|parameter| !order.contains(parameter))
                .copied()
                .collect();
            return Err(self.cycle_diagnostic(&self.find_cycle(&remaining, &dependencies)));
        }
        Ok(order)
    }

    fn find_cycle(
        &self,
        remaining: &BTreeSet<TypeParameterId>,
        dependencies: &BTreeMap<TypeParameterId, BTreeSet<TypeParameterId>>,
    ) -> Vec<TypeParameterId> {
        let start = remaining
            .iter()
            .next()
            .copied()
            .expect("internal invariant violated: a cycle has at least one remaining node");
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        let mut current = start;
        while visited.insert(current) {
            path.push(current);
            let Some(next) = dependencies
                .get(&current)
                .and_then(|dependencies| dependencies.iter().find(|next| remaining.contains(next)))
            else {
                break;
            };
            if let Some(position) = path.iter().position(|parameter| parameter == next) {
                return path[position..].to_vec();
            }
            current = *next;
        }
        path
    }

    fn cycle_diagnostic(&self, cycle: &[TypeParameterId]) -> Diagnostic {
        let mut names = cycle.to_vec();
        if let Some(first) = cycle.first() {
            names.push(*first);
        }
        let described = names
            .iter()
            .map(|parameter| format!("{}", parameter.0))
            .collect::<Vec<_>>()
            .join(" -> ");
        Diagnostic::new(
            self.origin.span.clone(),
            format!("substitution cycle between instance parameters: {described}"),
        )
    }
}

fn merge_substitution_values(
    origin: &Origin,
    parameter: TypeParameterId,
    existing: SubstitutionValue,
    existing_source: SubstitutionSource,
    incoming: SubstitutionValue,
    incoming_source: SubstitutionSource,
) -> Result<SubstitutionValue, Diagnostic> {
    let conflict = |existing: &SubstitutionValue, incoming: &SubstitutionValue| {
        Diagnostic::new(
            origin.span.clone(),
            format!(
                "conflicting substitutions for parameter {}: {} gives {}, while {} gives {}",
                parameter.0,
                existing_source.description(),
                existing.describe(),
                incoming_source.description(),
                incoming.describe()
            ),
        )
    };
    // The complete checked callable type is authoritative for the target
    // template's own parameters. A nested trait request that re-instantiates
    // a declaration the enclosing instance already maps is legal same-function
    // recursion at a different type (`Ref (Ref T)` comparing its `Ref T`
    // elements); the fresh inferred value must win, exactly as the backend's
    // specialization queue keeps the inferred substitutions and only fills
    // missing entries from the active environment. The enclosing environment
    // stays the fallback for parameters the callable type does not determine,
    // such as captured outer parameters.
    let same_kind = matches!(
        (&existing, &incoming),
        (SubstitutionValue::Type(_), SubstitutionValue::Type(_))
            | (SubstitutionValue::Effects(_), SubstitutionValue::Effects(_))
    );
    if same_kind {
        match (existing_source, incoming_source) {
            (SubstitutionSource::EnclosingInstance, SubstitutionSource::Inferred)
                if !incoming.is_placeholder() =>
            {
                return Ok(incoming);
            }
            (SubstitutionSource::Inferred, SubstitutionSource::EnclosingInstance)
                if !existing.is_placeholder() =>
            {
                return Ok(existing);
            }
            _ => {}
        }
    }
    match (existing, incoming) {
        (SubstitutionValue::Type(existing), SubstitutionValue::Type(incoming)) => {
            if existing == incoming {
                return Ok(SubstitutionValue::Type(existing));
            }
            if existing == CheckedType::Error || existing == CheckedType::Inferred {
                return Ok(SubstitutionValue::Type(incoming));
            }
            if incoming == CheckedType::Error || incoming == CheckedType::Inferred {
                return Ok(SubstitutionValue::Type(existing));
            }
            match merge_types(existing.clone(), incoming.clone()) {
                Some(merged) if merged != CheckedType::Error => Ok(SubstitutionValue::Type(merged)),
                _ => Err(conflict(
                    &SubstitutionValue::Type(existing),
                    &SubstitutionValue::Type(incoming),
                )),
            }
        }
        (SubstitutionValue::Effects(existing), SubstitutionValue::Effects(incoming)) => {
            if existing == incoming {
                return Ok(SubstitutionValue::Effects(existing));
            }
            let existing_is_wildcard = existing.variable.is_some()
                && existing.resources.is_empty()
                && existing.state.is_none();
            let incoming_is_wildcard = incoming.variable.is_some()
                && incoming.resources.is_empty()
                && incoming.state.is_none();
            if existing_is_wildcard {
                return Ok(SubstitutionValue::Effects(incoming));
            }
            if incoming_is_wildcard {
                return Ok(SubstitutionValue::Effects(existing));
            }
            Err(conflict(
                &SubstitutionValue::Effects(existing),
                &SubstitutionValue::Effects(incoming),
            ))
        }
        (existing, incoming) => Err(Diagnostic::new(
            origin.span.clone(),
            format!(
                "parameter {} receives {} from {} but {} from {}",
                parameter.0,
                existing.kind_name(),
                existing_source.description(),
                incoming.kind_name(),
                incoming_source.description()
            ),
        )),
    }
}

/// Requires every relevant parameter to have a concrete value of the right
/// kind, reporting the first unresolved one at the request origin. A
/// nonrelevant outer parameter never reaches this check and so never enters a
/// key.
pub(crate) fn require_concrete_substitutions(
    environment: &SubstitutionEnvironment,
    relevant: &RelevantParameters,
    origin: &Origin,
) -> Result<(), Diagnostic> {
    for parameter in relevant.type_parameters() {
        let Some(entry) = environment.entry(parameter) else {
            return Err(unresolved_parameter_diagnostic(
                origin,
                relevant,
                parameter,
                "type",
                "no substitution is available",
            ));
        };
        match &entry.value {
            SubstitutionValue::Type(value_type) => {
                if let Some(problem) = unresolved_type_problem(value_type) {
                    return Err(unresolved_parameter_diagnostic(
                        origin, relevant, parameter, "type", problem,
                    ));
                }
            }
            SubstitutionValue::Effects(_) => {
                return Err(unresolved_parameter_diagnostic(
                    origin,
                    relevant,
                    parameter,
                    "type",
                    "the substitution is an effect row",
                ));
            }
        }
    }
    for parameter in relevant.effect_parameters() {
        let Some(entry) = environment.entry(parameter) else {
            return Err(unresolved_parameter_diagnostic(
                origin,
                relevant,
                parameter,
                "effect",
                "no substitution is available",
            ));
        };
        match &entry.value {
            SubstitutionValue::Effects(effects) => {
                if let Some(variable) = &effects.variable {
                    return Err(unresolved_parameter_diagnostic(
                        origin,
                        relevant,
                        parameter,
                        "effect",
                        &format!("the row still names effect variable `{}`", variable.name),
                    ));
                }
                for resource in &effects.resources {
                    if let Some(problem) = unresolved_type_problem(&resource.value_type) {
                        return Err(unresolved_parameter_diagnostic(
                            origin, relevant, parameter, "effect", problem,
                        ));
                    }
                }
            }
            SubstitutionValue::Type(_) => {
                return Err(unresolved_parameter_diagnostic(
                    origin,
                    relevant,
                    parameter,
                    "effect",
                    "the substitution is a type",
                ));
            }
        }
    }
    Ok(())
}

fn unresolved_parameter_diagnostic(
    origin: &Origin,
    relevant: &RelevantParameters,
    parameter: TypeParameterId,
    kind: &str,
    problem: &str,
) -> Diagnostic {
    Diagnostic::new(
        origin.span.clone(),
        format!(
            "cannot resolve {kind} parameter {} for this instance: {problem}",
            relevant.display(parameter)
        ),
    )
}

/// The first unresolved placeholder inside a substituted value, if any. A
/// declared parameter reference or a checker placeholder never reaches an
/// instance boundary.
pub(crate) fn unresolved_type_problem(value_type: &CheckedType) -> Option<&'static str> {
    if contains_type_parameter(value_type) {
        return Some("its value still contains a declared parameter");
    }
    if contains_placeholder(value_type) {
        return Some("its value contains an inferred or error placeholder");
    }
    None
}

fn contains_placeholder(value_type: &CheckedType) -> bool {
    match value_type {
        CheckedType::Inferred | CheckedType::Error => true,
        CheckedType::Ref(payload)
        | CheckedType::Slice(payload)
        | CheckedType::Buffer(payload)
        | CheckedType::CPointer { pointee: payload } => contains_placeholder(payload),
        CheckedType::Array { element, count } => {
            contains_placeholder(element) || contains_placeholder(count)
        }
        CheckedType::TypeConstructor { arguments, .. } | CheckedType::Opaque { arguments, .. } => {
            arguments.iter().any(contains_placeholder)
        }
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| contains_placeholder(&element.value_type)),
        CheckedType::Sum(sum) => sum.alternatives.iter().any(contains_placeholder),
        CheckedType::Function(function) => {
            contains_placeholder(&function.parameter)
                || function
                    .effects
                    .resources
                    .iter()
                    .any(|resource| contains_placeholder(&resource.value_type))
                || contains_placeholder(&function.result)
        }
        CheckedType::Distinct {
            arguments,
            representation,
            ..
        } => arguments.iter().any(contains_placeholder) || contains_placeholder(representation),
        _ => false,
    }
}

/// Owned-metadata view of one trait selector. It mirrors the checker's
/// `resolve_trait_obligation`/`dispatch_matching_implementations` rules over
/// the lowered trait and implementation catalogs, so evidence selection never
/// consults `TypedModule`, LLVM state, or a runtime implementation search.
struct TraitSelectionContext<'a> {
    program: &'a LoweredProgram,
    implementations: Vec<CheckedTraitImplementation>,
    implementation_ids: Vec<LoweredTraitImplementationId>,
    /// Substituted declared prerequisites in scope for this request.
    bounds: Vec<CheckedTraitBound>,
    visited: RefCell<Vec<(TraitId, Vec<CheckedType>)>>,
    cycle_hit: Cell<bool>,
    /// A matching implementation header failed its conditional bounds.
    prerequisite_failed: Cell<bool>,
}

impl<'a> TraitSelectionContext<'a> {
    fn new(program: &'a LoweredProgram, bounds: Vec<CheckedTraitBound>) -> Self {
        let mut implementations = Vec::new();
        let mut implementation_ids = Vec::new();
        for (id, metadata) in program.trait_implementations.iter() {
            implementations.push(CheckedTraitImplementation {
                span: metadata.origin.span.clone(),
                trait_id: metadata.trait_id,
                parameters: metadata.parameters.iter().copied().collect(),
                arguments: metadata.arguments.clone(),
                bounds: metadata.bounds.clone(),
                negative: metadata.negative,
                methods: metadata.methods.iter().copied().collect(),
            });
            implementation_ids.push(id);
        }
        TraitSelectionContext {
            program,
            implementations,
            implementation_ids,
            bounds,
            visited: RefCell::new(Vec::new()),
            cycle_hit: Cell::new(false),
            prerequisite_failed: Cell::new(false),
        }
    }

    /// Expands declared bounds through the owned trait prerequisite catalog,
    /// mirroring the checker's `expand_trait_bounds`, so a transitive
    /// prerequisite such as `TestDerived T` implying `TestBase T` is visible
    /// to selection.
    fn expand_bounds(&self, bounds: Vec<CheckedTraitBound>) -> Vec<CheckedTraitBound> {
        let mut expanded = Vec::new();
        for bound in bounds {
            self.expand_bound(bound, &mut expanded);
        }
        expanded
    }

    fn expand_bound(&self, bound: CheckedTraitBound, expanded: &mut Vec<CheckedTraitBound>) {
        if expanded.len() > 64 || expanded.contains(&bound) {
            return;
        }
        expanded.push(bound.clone());
        let Some(metadata) = self.program.traits.get(bound.trait_id) else {
            return;
        };
        if metadata.parameters.len() != bound.arguments.len() {
            return;
        }
        let mut substitutions = HashMap::new();
        for (parameter, argument) in metadata.parameters.iter().zip(&bound.arguments) {
            let _ = infer_type_parameters(parameter, argument, &mut substitutions);
        }
        for prerequisite in &metadata.prerequisites {
            let prerequisite = CheckedTraitBound {
                trait_id: prerequisite.trait_id,
                arguments: prerequisite
                    .arguments
                    .iter()
                    .cloned()
                    .map(|argument| substitute_type(argument, &substitutions))
                    .collect(),
            };
            self.expand_bound(prerequisite, expanded);
        }
    }

    fn trait_name(&self, trait_id: TraitId) -> String {
        self.program
            .traits
            .get(trait_id)
            .map(|trait_| trait_.name.clone())
            .unwrap_or_else(|| format!("trait {}", trait_id.0))
    }

    fn is_copy(&self, value_type: &CheckedType) -> bool {
        is_copy_type(
            value_type,
            self.program.semantic_ids.copy_trait,
            self.program.semantic_ids.drop_trait,
            &self.implementations,
            &self.bounds,
            &|bound| self.drop_bound_holds(bound),
        )
    }

    /// Whether a `Drop` implementation applies to a concrete
    /// type under the general matching rule.
    fn drop_applies(&self, value_type: &CheckedType) -> bool {
        drop_implementation_applies(
            value_type,
            self.program.semantic_ids.drop_trait,
            &self.implementations,
            &|bound| self.drop_bound_holds(bound),
        )
    }

    /// The bound-discharge callback of the general drop-implementation
    /// predicate: a `Copy` bound asks the structural `Copy` predicate, any
    /// other bound asks the owned-catalog obligation resolver.
    fn drop_bound_holds(&self, bound: &CheckedTraitBound) -> bool {
        if Some(bound.trait_id) == self.program.semantic_ids.copy_trait {
            bound
                .arguments
                .first()
                .is_some_and(|argument| self.is_copy(argument))
        } else {
            self.resolve_obligation(bound.trait_id, &bound.arguments)
                .is_some()
        }
    }

    fn structural(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Option<(Vec<CheckedType>, StructuralTraitMethod)> {
        structural_trait_arguments(
            trait_id,
            arguments,
            self.program.semantic_ids.index_trait,
            self.program.semantic_ids.mutate_index_trait,
            self.program.semantic_ids.into_iterator_trait,
            self.program.semantic_ids.iterator_trait,
            self.program.semantic_ids.debug_trait,
            |value_type| self.is_copy(value_type),
            |value_type| {
                self.program.semantic_ids.debug_trait.is_some_and(|debug| {
                    self.obligation_available(debug, std::slice::from_ref(value_type))
                })
            },
            |trait_id, arguments| self.resolve_obligation(trait_id, arguments),
        )
    }

    fn obligation_available(&self, trait_id: TraitId, arguments: &[CheckedType]) -> bool {
        self.resolve_obligation(trait_id, arguments).is_some()
    }

    fn resolve_obligation(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Option<Vec<CheckedType>> {
        let key = (trait_id, arguments.to_vec());
        {
            let mut visited = self.visited.borrow_mut();
            if visited.len() > 64 || visited.contains(&key) {
                self.cycle_hit.set(true);
                return None;
            }
            visited.push(key);
        }
        let result = self.resolve_obligation_inner(trait_id, arguments);
        self.visited.borrow_mut().pop();
        result
    }

    fn resolve_obligation_inner(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Option<Vec<CheckedType>> {
        if let Some((completed, _)) = self.structural(trait_id, arguments) {
            return Some(completed);
        }
        if arguments.iter().any(contains_inferred_type) {
            return self.complete_obligation_arguments(trait_id, arguments);
        }
        self.obligation_available_exact(trait_id, arguments)
            .then(|| arguments.to_vec())
    }

    fn obligation_available_exact(&self, trait_id: TraitId, arguments: &[CheckedType]) -> bool {
        if let Some((_, _)) = self.structural(trait_id, arguments) {
            return true;
        }
        if self
            .bounds
            .iter()
            .any(|bound| bound.trait_id == trait_id && bound.arguments == arguments)
        {
            return true;
        }
        let [target] = arguments else {
            return !self
                .matching_implementations(trait_id, arguments)
                .is_empty();
        };
        if Some(trait_id) == self.program.semantic_ids.natural_trait {
            return matches!(target, CheckedType::NumberLiteral(_));
        }
        if Some(trait_id) == self.program.semantic_ids.sized_trait {
            return target.is_sized();
        }
        if Some(trait_id) == self.program.semantic_ids.copy_trait {
            return self.is_copy(target);
        }
        if Some(trait_id) == self.program.semantic_ids.default_trait {
            if let Some(default_trait) = self.program.semantic_ids.default_trait
                && is_default_type(target, default_trait, &self.implementations, &self.bounds)
            {
                return true;
            }
            return !self
                .matching_implementations(trait_id, arguments)
                .is_empty();
        }
        !self
            .matching_implementations(trait_id, arguments)
            .is_empty()
    }

    /// Implementation indices whose header unifies with `arguments` and whose
    /// conditional bounds hold. Negative implementations never prove an
    /// obligation and never provide a method.
    fn matching_implementations(&self, trait_id: TraitId, arguments: &[CheckedType]) -> Vec<usize> {
        let mut matches = Vec::new();
        for (index, implementation) in self.implementations.iter().enumerate() {
            if implementation.trait_id != trait_id
                || implementation.negative
                || implementation.arguments.len() != arguments.len()
            {
                continue;
            }
            let mut substitutions = HashMap::new();
            let unifies =
                implementation
                    .arguments
                    .iter()
                    .zip(arguments)
                    .all(|(template, actual)| {
                        infer_type_parameters(template, actual, &mut substitutions)
                    });
            if !unifies {
                continue;
            }
            if self.implementation_bounds_hold(implementation, &substitutions) {
                matches.push(index);
            }
        }
        matches
    }

    fn implementation_bounds_hold(
        &self,
        implementation: &CheckedTraitImplementation,
        substitutions: &HashMap<TypeParameterId, CheckedType>,
    ) -> bool {
        implementation.bounds.iter().all(|bound| {
            let bound_arguments = bound
                .arguments
                .iter()
                .cloned()
                .map(|argument| substitute_type(argument, substitutions))
                .collect::<Vec<_>>();
            if bound_arguments
                .iter()
                .any(|argument| contains_type_parameter(argument))
            {
                self.prerequisite_failed.set(true);
                return false;
            }
            if Some(bound.trait_id) == self.program.semantic_ids.copy_trait {
                let holds = bound_arguments
                    .first()
                    .is_some_and(|value| self.is_copy(value));
                if !holds {
                    self.prerequisite_failed.set(true);
                }
                return holds;
            }
            let holds = self.obligation_available(bound.trait_id, &bound_arguments);
            if !holds {
                self.prerequisite_failed.set(true);
            }
            holds
        })
    }

    fn negative_match(&self, trait_id: TraitId, arguments: &[CheckedType]) -> bool {
        self.implementations.iter().any(|implementation| {
            implementation.trait_id == trait_id
                && implementation.negative
                && implementation.arguments.len() == arguments.len()
                && implementation
                    .arguments
                    .iter()
                    .zip(arguments)
                    .all(|(template, actual)| {
                        let mut substitutions = HashMap::new();
                        infer_type_parameters(template, actual, &mut substitutions)
                    })
        })
    }

    /// Completes functional-dependency or inferred positions from declared
    /// bounds and implementation headers, exactly like the checker's inferred
    /// branch: unify the known positions, substitute into each candidate
    /// header, and require every candidate to agree.
    fn complete_obligation_arguments(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Option<Vec<CheckedType>> {
        let mut candidates = self.all_completion_candidates(trait_id, arguments);
        let mut completed = candidates.drain(..).next()?;
        for candidate in candidates {
            completed = merge_trait_arguments(&completed, &candidate)?;
        }
        Some(completed)
    }

    fn all_completion_candidates(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
    ) -> Vec<Vec<CheckedType>> {
        let mut candidates = self
            .bounds
            .iter()
            .filter(|bound| bound.trait_id == trait_id && bound.arguments.len() == arguments.len())
            .filter_map(|bound| {
                let mut substitutions = HashMap::new();
                let matches_known =
                    bound
                        .arguments
                        .iter()
                        .zip(arguments)
                        .all(|(template, actual)| {
                            contains_inferred_type(actual)
                                || infer_type_parameters(template, actual, &mut substitutions)
                        });
                matches_known.then(|| {
                    bound
                        .arguments
                        .iter()
                        .cloned()
                        .map(|argument| substitute_type(argument, &substitutions))
                        .collect()
                })
            })
            .collect::<Vec<_>>();
        for implementation in &self.implementations {
            if implementation.trait_id != trait_id
                || implementation.arguments.len() != arguments.len()
            {
                continue;
            }
            let mut substitutions = HashMap::new();
            let unifies =
                implementation
                    .arguments
                    .iter()
                    .zip(arguments)
                    .all(|(template, actual)| {
                        contains_inferred_type(actual)
                            || infer_type_parameters(template, actual, &mut substitutions)
                    });
            if !unifies || !self.implementation_bounds_hold(implementation, &substitutions) {
                continue;
            }
            candidates.push(
                implementation
                    .arguments
                    .iter()
                    .cloned()
                    .map(|argument| substitute_type(argument, &substitutions))
                    .collect(),
            );
        }
        candidates
    }

    /// The completed header arguments for one matched implementation.
    fn completed_arguments(
        &self,
        index: usize,
        arguments: &[CheckedType],
    ) -> Option<Vec<CheckedType>> {
        let implementation = self.implementations.get(index)?;
        let mut substitutions = HashMap::new();
        let unifies = implementation
            .arguments
            .iter()
            .zip(arguments)
            .all(|(template, actual)| infer_type_parameters(template, actual, &mut substitutions));
        unifies.then(|| {
            implementation
                .arguments
                .iter()
                .cloned()
                .map(|argument| substitute_type(argument, &substitutions))
                .collect()
        })
    }

    fn select_method(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
        method: TraitMethodId,
        origin: &Origin,
    ) -> Result<TraitEvidence, Diagnostic> {
        let matches = self.matching_implementations(trait_id, arguments);
        if let Some(index) = matches.first().copied() {
            let completed = self
                .completed_arguments(index, arguments)
                .ok_or_else(|| no_match_diagnostic(self, origin, trait_id, arguments, false))?;
            for other in matches.iter().skip(1) {
                let Some(other_completed) = self.completed_arguments(*other, arguments) else {
                    continue;
                };
                let Some(merged) = merge_trait_arguments(&completed, &other_completed) else {
                    return Err(ambiguous_diagnostic(self, origin, trait_id, arguments));
                };
                if merged != completed {
                    return Err(ambiguous_diagnostic(self, origin, trait_id, arguments));
                }
            }
            let metadata = self
                .program
                .trait_implementations
                .get(self.implementation_ids[index])
                .ok_or_else(|| {
                    internal_invariant(
                        origin.span.clone(),
                        "matched implementation is in the catalog",
                    )
                })?;
            let function = metadata
                .methods
                .iter()
                .find(|(candidate, _)| *candidate == method)
                .map(|(_, function)| *function)
                .or_else(|| self.default_method(trait_id, method));
            let Some(function) = function else {
                return Err(no_match_diagnostic(
                    self, origin, trait_id, arguments, false,
                ));
            };
            return Ok(TraitEvidence::ExplicitImplementation {
                trait_id,
                implementation: self.implementation_ids[index],
                method,
                function,
                arguments: completed,
            });
        }
        if let Some((completed, structural)) = self.structural(trait_id, arguments) {
            return Ok(TraitEvidence::Structural {
                trait_id,
                method,
                structural,
                arguments: completed,
            });
        }
        let negative = self.negative_match(trait_id, arguments);
        Err(no_match_diagnostic(
            self, origin, trait_id, arguments, negative,
        ))
    }

    fn default_method(&self, trait_id: TraitId, method: TraitMethodId) -> Option<FunctionId> {
        self.program
            .traits
            .get(trait_id)?
            .default_methods
            .iter()
            .find(|(candidate, _)| *candidate == method)
            .map(|(_, function)| *function)
    }

    /// Proves an obligation without selecting a method target.
    fn prove_obligation(
        &self,
        trait_id: TraitId,
        arguments: &[CheckedType],
        origin: &Origin,
    ) -> Result<(), Diagnostic> {
        if self.obligation_available(trait_id, arguments) {
            Ok(())
        } else {
            Err(no_match_diagnostic(
                self,
                origin,
                trait_id,
                arguments,
                self.negative_match(trait_id, arguments),
            ))
        }
    }
}

fn merge_trait_arguments(left: &[CheckedType], right: &[CheckedType]) -> Option<Vec<CheckedType>> {
    if left.len() != right.len() {
        return None;
    }
    left.iter()
        .cloned()
        .zip(right.iter().cloned())
        .map(|(left, right)| merge_types(left, right))
        .collect()
}

fn describe_arguments(arguments: &[CheckedType]) -> String {
    arguments
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn no_match_diagnostic(
    context: &TraitSelectionContext<'_>,
    origin: &Origin,
    trait_id: TraitId,
    arguments: &[CheckedType],
    negative: bool,
) -> Diagnostic {
    let message = if negative {
        format!(
            "trait `{}` is rejected for `{}` by a negative implementation",
            context.trait_name(trait_id),
            describe_arguments(arguments)
        )
    } else if context.cycle_hit.get() {
        format!(
            "trait `{}` for `{}` has an unresolved cyclic obligation",
            context.trait_name(trait_id),
            describe_arguments(arguments)
        )
    } else if context.prerequisite_failed.get() {
        format!(
            "trait `{}` for `{}` has an unsatisfied conditional prerequisite",
            context.trait_name(trait_id),
            describe_arguments(arguments)
        )
    } else {
        format!(
            "no implementation of trait `{}` is available for `{}`",
            context.trait_name(trait_id),
            describe_arguments(arguments)
        )
    };
    Diagnostic::new(origin.span.clone(), message)
}

fn ambiguous_diagnostic(
    context: &TraitSelectionContext<'_>,
    origin: &Origin,
    trait_id: TraitId,
    arguments: &[CheckedType],
) -> Diagnostic {
    Diagnostic::new(
        origin.span.clone(),
        format!(
            "ambiguous implementation of trait `{}` for `{}`",
            context.trait_name(trait_id),
            describe_arguments(arguments)
        ),
    )
}

impl LoweredProgram {
    /// Resolves one evidence recipe against the concrete environment.
    ///
    /// Explicit and structural selections already recorded by lowering are
    /// preserved and validated; a declared bound is matched against the owned
    /// implementation catalog in declaration order using the checker's
    /// unification, conditional-bound, negative-implementation, functional
    /// dependency, and structural-derivation rules. A `method: None` recipe
    /// proves an obligation but returns no target evidence.
    pub(crate) fn resolve_trait_evidence(
        &self,
        origin: &Origin,
        evidence: Option<&TraitEvidence>,
        environment: &SubstitutionEnvironment,
    ) -> Result<Option<TraitEvidence>, Diagnostic> {
        let Some(evidence) = evidence else {
            return Ok(None);
        };
        let map = environment.substitution_map();
        match evidence {
            TraitEvidence::ExplicitImplementation {
                trait_id,
                implementation,
                method,
                function,
                arguments,
            } => {
                let arguments = substitute_concrete_arguments(arguments, &map, origin)?;
                let context = TraitSelectionContext::new(self, Vec::new());
                let metadata = self
                    .trait_implementations
                    .get(*implementation)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "recorded trait implementation {} is missing from the lowered catalog",
                                implementation.index()
                            ),
                        )
                    })?;
                if metadata.trait_id != *trait_id {
                    return Err(Diagnostic::new(
                        origin.span.clone(),
                        "recorded explicit evidence names the wrong trait implementation",
                    ));
                }
                let selected = metadata
                    .methods
                    .iter()
                    .find(|(candidate, _)| candidate == method)
                    .map(|(_, function)| *function)
                    .or_else(|| context.default_method(*trait_id, *method));
                if selected != Some(*function) {
                    return Err(Diagnostic::new(
                        origin.span.clone(),
                        format!(
                            "recorded explicit selection for trait `{}` no longer provides method {}",
                            context.trait_name(*trait_id),
                            method.0
                        ),
                    ));
                }
                let Some(index) = context
                    .implementation_ids
                    .iter()
                    .position(|id| id == implementation)
                else {
                    return Err(Diagnostic::new(
                        origin.span.clone(),
                        format!(
                            "recorded trait implementation {} is missing from the lowered catalog",
                            implementation.index()
                        ),
                    ));
                };
                if !context
                    .matching_implementations(*trait_id, &arguments)
                    .contains(&index)
                {
                    return Err(Diagnostic::new(
                        origin.span.clone(),
                        format!(
                            "recorded explicit selection for trait `{}` no longer matches `{}`",
                            context.trait_name(*trait_id),
                            describe_arguments(&arguments)
                        ),
                    ));
                }
                Ok(Some(TraitEvidence::ExplicitImplementation {
                    trait_id: *trait_id,
                    implementation: *implementation,
                    method: *method,
                    function: *function,
                    arguments,
                }))
            }
            TraitEvidence::Structural {
                trait_id,
                method,
                structural,
                arguments,
            } => {
                let substituted = arguments
                    .iter()
                    .map(|argument| substitute_type(argument.clone(), &map))
                    .collect::<Vec<_>>();
                let context = TraitSelectionContext::new(self, Vec::new());
                let Some((completed, derived)) = context.structural(*trait_id, &substituted) else {
                    return Err(Diagnostic::new(
                        origin.span.clone(),
                        format!(
                            "recorded structural selection for trait `{}` no longer applies to `{}`",
                            context.trait_name(*trait_id),
                            describe_arguments(&substituted)
                        ),
                    ));
                };
                if derived != *structural {
                    return Err(Diagnostic::new(
                        origin.span.clone(),
                        format!(
                            "recorded structural selection for trait `{}` disagrees with the derived method",
                            context.trait_name(*trait_id)
                        ),
                    ));
                }
                for argument in &completed {
                    if let Some(problem) = unresolved_type_problem(argument) {
                        return Err(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "structural selection for trait `{}` is not concrete: {problem}",
                                context.trait_name(*trait_id)
                            ),
                        ));
                    }
                }
                Ok(Some(TraitEvidence::Structural {
                    trait_id: *trait_id,
                    method: *method,
                    structural: *structural,
                    arguments: completed,
                }))
            }
            TraitEvidence::DeclaredBound {
                trait_id,
                method,
                arguments,
                prerequisites,
            } => {
                let mut substituted = arguments
                    .iter()
                    .map(|argument| substitute_type(argument.clone(), &map))
                    .collect::<Vec<_>>();
                let bounds = TraitSelectionContext::new(self, Vec::new())
                    .expand_bounds(substitute_bounds(prerequisites, &map));
                let context = TraitSelectionContext::new(self, bounds);
                if substituted.iter().any(contains_inferred_type) {
                    substituted = context
                        .complete_obligation_arguments(*trait_id, &substituted)
                        .ok_or_else(|| {
                            Diagnostic::new(
                                origin.span.clone(),
                                format!(
                                    "cannot complete the arguments of trait `{}` for `{}` from the owned catalogs",
                                    context.trait_name(*trait_id),
                                    describe_arguments(&substituted)
                                ),
                            )
                        })?;
                }
                for argument in &substituted {
                    if let Some(problem) = unresolved_type_problem(argument) {
                        return Err(Diagnostic::new(
                            origin.span.clone(),
                            format!(
                                "trait `{}` evidence is not concrete: {problem}",
                                context.trait_name(*trait_id)
                            ),
                        ));
                    }
                }
                match method {
                    Some(method) => context
                        .select_method(*trait_id, &substituted, *method, origin)
                        .map(Some),
                    None => {
                        context.prove_obligation(*trait_id, &substituted, origin)?;
                        Ok(None)
                    }
                }
            }
        }
    }
}

/// The opaque runtime types whose drop takes a dedicated cleanup route instead
/// of a structural drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeOpaqueKind {
    Coroutine,
    Scheduler,
    Wait,
    Resolver,
    CompletionToken,
}

impl LoweredProgram {
    /// Whether a fully substituted value type is `Copy`, using the owned trait
    /// and implementation catalogs. A concrete instance has no declared
    /// parameters, so only implementation-derived facts decide.
    pub(crate) fn concrete_is_copy(&self, value_type: &CheckedType) -> bool {
        TraitSelectionContext::new(self, Vec::new()).is_copy(value_type)
    }

    /// The opaque runtime type identity of a fully substituted type, exactly
    /// as `TypedModule`'s `is_*_type` predicates select it.
    pub(crate) fn runtime_opaque_kind(
        &self,
        value_type: &CheckedType,
    ) -> Option<RuntimeOpaqueKind> {
        let CheckedType::Opaque { id, .. } = value_type else {
            return None;
        };
        let ids = &self.semantic_ids;
        if Some(*id) == ids.coroutine_type {
            Some(RuntimeOpaqueKind::Coroutine)
        } else if Some(*id) == ids.scheduler_type {
            Some(RuntimeOpaqueKind::Scheduler)
        } else if Some(*id) == ids.wait_type {
            Some(RuntimeOpaqueKind::Wait)
        } else if Some(*id) == ids.resolver_type {
            Some(RuntimeOpaqueKind::Resolver)
        } else if Some(*id) == ids.completion_token_type {
            Some(RuntimeOpaqueKind::CompletionToken)
        } else {
            None
        }
    }

    /// Whether a fully substituted value type needs a drop, mirroring
    /// `TypedModule::type_needs_drop` with the owned trait catalogs and the
    /// coroutine/runtime type identities.
    pub(crate) fn concrete_needs_drop(&self, value_type: &CheckedType) -> bool {
        if self.runtime_opaque_kind(value_type).is_some() {
            return true;
        }
        concrete_type_needs_drop(self, value_type)
    }

    /// Whether a `Drop` implementation applies to a fully substituted type
    /// under the general matching rule, using the owned catalogs and the same
    /// bound-discharge callback the checker uses.
    pub(crate) fn concrete_drop_implementation_applies(&self, value_type: &CheckedType) -> bool {
        TraitSelectionContext::new(self, Vec::new()).drop_applies(value_type)
    }

    /// Completes one declared trait bound for a concrete instance: substitutes
    /// the instance environment, then fills functional-dependency or inferred
    /// positions from the owned catalogs exactly as the resolver does for a
    /// `DeclaredBound` recipe. Specialization stores the completed bound as
    /// body metadata, so no placeholder survives into an emitted body.
    pub(crate) fn complete_declared_bound(
        &self,
        origin: &Origin,
        bound: &CheckedTraitBound,
        environment: &SubstitutionEnvironment,
    ) -> Result<CheckedTraitBound, Diagnostic> {
        let map = environment.substitution_map();
        let substituted = CheckedTraitBound {
            trait_id: bound.trait_id,
            arguments: bound
                .arguments
                .iter()
                .map(|argument| substitute_type(argument.clone(), &map))
                .collect(),
        };
        if !substituted.arguments.iter().any(contains_inferred_type) {
            for argument in &substituted.arguments {
                if let Some(problem) = unresolved_type_problem(argument) {
                    return Err(Diagnostic::new(
                        origin.span.clone(),
                        format!("trait bound argument `{argument}` is not concrete: {problem}"),
                    ));
                }
            }
            return Ok(substituted);
        }
        let bounds = TraitSelectionContext::new(self, Vec::new())
            .expand_bounds(substitute_bounds(std::slice::from_ref(bound), &map));
        let context = TraitSelectionContext::new(self, bounds);
        let completed = context
            .complete_obligation_arguments(substituted.trait_id, &substituted.arguments)
            .filter(|completed| !completed.iter().any(contains_inferred_type))
            .or_else(|| {
                complete_bound_from_implementations(
                    self,
                    substituted.trait_id,
                    &substituted.arguments,
                )
            })
            .ok_or_else(|| {
                Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "cannot complete the arguments of trait `{}` for this instance",
                        context.trait_name(substituted.trait_id)
                    ),
                )
            })?;
        for argument in &completed {
            if let Some(problem) = unresolved_type_problem(argument) {
                return Err(Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "completed trait bound argument `{argument}` is not concrete: {problem}"
                    ),
                ));
            }
        }
        Ok(CheckedTraitBound {
            trait_id: substituted.trait_id,
            arguments: completed,
        })
    }
}

fn concrete_type_needs_drop(program: &LoweredProgram, value_type: &CheckedType) -> bool {
    if program.concrete_drop_implementation_applies(value_type) {
        return true;
    }
    match value_type {
        CheckedType::CString => true,
        CheckedType::Buffer(_) => false,
        CheckedType::Product(product) => product
            .elements
            .iter()
            .any(|element| concrete_type_needs_drop(program, &element.value_type)),
        CheckedType::Sum(sum) => sum
            .alternatives
            .iter()
            .any(|alternative| concrete_type_needs_drop(program, alternative)),
        CheckedType::Distinct { representation, .. } => {
            concrete_type_needs_drop(program, representation)
        }
        _ => false,
    }
}

/// Completes functional-dependency positions of a concrete bound from the
/// owned trait-implementation catalog: every non-negative implementation of
/// the trait whose known positions unify contributes a candidate, and all
/// candidates must agree. This is the implementation-side counterpart of
/// `TraitSelectionContext::complete_obligation_arguments`.
pub(crate) fn complete_bound_from_implementations(
    program: &LoweredProgram,
    trait_id: TraitId,
    arguments: &[CheckedType],
) -> Option<Vec<CheckedType>> {
    let mut candidates = Vec::new();
    for (_, implementation) in program.trait_implementations.iter() {
        if implementation.negative
            || implementation.trait_id != trait_id
            || implementation.arguments.len() != arguments.len()
        {
            continue;
        }
        let mut substitutions = HashMap::new();
        let matches = implementation
            .arguments
            .iter()
            .zip(arguments)
            .all(|(template, actual)| {
                contains_inferred_type(actual)
                    || infer_type_parameters(template, actual, &mut substitutions)
            });
        if matches {
            let candidate = implementation
                .arguments
                .iter()
                .cloned()
                .map(|argument| substitute_type(argument, &substitutions))
                .collect::<Vec<_>>();
            if !candidate.iter().any(contains_inferred_type)
                && !candidate.iter().any(contains_type_parameter)
            {
                candidates.push(candidate);
            }
        }
    }
    let mut completed = candidates.drain(..).next()?;
    for candidate in candidates {
        completed = merge_trait_arguments(&completed, &candidate)?;
    }
    Some(completed)
}

fn substitute_bounds(
    bounds: &[CheckedTraitBound],
    map: &HashMap<TypeParameterId, CheckedType>,
) -> Vec<CheckedTraitBound> {
    bounds
        .iter()
        .map(|bound| CheckedTraitBound {
            trait_id: bound.trait_id,
            arguments: bound
                .arguments
                .iter()
                .cloned()
                .map(|argument| substitute_type(argument, map))
                .collect(),
        })
        .collect()
}

fn substitute_concrete_arguments(
    arguments: &[CheckedType],
    map: &HashMap<TypeParameterId, CheckedType>,
    origin: &Origin,
) -> Result<Vec<CheckedType>, Diagnostic> {
    let mut substituted = Vec::with_capacity(arguments.len());
    for argument in arguments {
        let value = substitute_type(argument.clone(), map);
        if let Some(problem) = unresolved_type_problem(&value) {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!("trait evidence argument `{value}` is not concrete: {problem}"),
            ));
        }
        substituted.push(value);
    }
    Ok(substituted)
}

/// The complete result of resolving one specialization request: the concrete
/// key, the concrete environment reused for body substitution, the relevant
/// parameter set, and the resolved evidence. Only the worklist interns keys and
/// decides reachability; this value carries no catalog position.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedInstanceRequest {
    pub key: InstanceKey,
    pub environment: SubstitutionEnvironment,
    pub relevant: RelevantParameters,
    pub evidence: Option<TraitEvidence>,
}

/// How a request relates to the enclosing instance.
#[derive(Clone, Copy)]
pub(crate) enum InstanceResolutionTarget<'a> {
    /// A root request: no enclosing instance environment.
    Root,
    /// A nested request whose environment composes with the enclosing one.
    Nested(&'a ResolvedInstanceRequest),
    /// A same-function recursive reference that reuses the current closure
    /// environment. The completed key must equal the enclosing instance's key.
    Current(&'a ResolvedInstanceRequest),
}

/// One specialization resolver input: the target function, the requesting origin,
/// the complete checked callable type at the site, the raw lowering recipe, and
/// the enclosing-instance relationship.
pub(crate) struct InstanceResolutionRequest<'a> {
    pub function: FunctionId,
    pub origin: Origin,
    pub function_type: CheckedFunctionType,
    pub substitutions: CallSubstitutions,
    pub evidence: Option<TraitEvidence>,
    pub target: InstanceResolutionTarget<'a>,
}

impl LoweredProgram {
    /// Resolves one request end to end: composed environment, selected
    /// evidence, concrete key, and the relevant environment for specialization.
    ///
    /// A `Current` target enforces the existing prohibition on polymorphic
    /// recursion: a recursive request whose normalized key differs from the
    /// enclosing instance key diagnoses instead of interning a second
    /// specialization, and an equal key returns the enclosing instance.
    pub(crate) fn resolve_instance_request(
        &self,
        request: &InstanceResolutionRequest<'_>,
    ) -> Result<ResolvedInstanceRequest, Diagnostic> {
        let enclosing = match request.target {
            InstanceResolutionTarget::Root => None,
            InstanceResolutionTarget::Nested(enclosing)
            | InstanceResolutionTarget::Current(enclosing) => Some(enclosing),
        };
        let resolved = (|| {
            let (environment, relevant) = self.resolve_substitutions(
                request.function,
                &request.origin,
                &request.function_type,
                &request.substitutions,
                enclosing.map(|enclosing| &enclosing.environment),
            )?;
            let evidence = self.resolve_trait_evidence(
                &request.origin,
                request.evidence.as_ref(),
                &environment,
            )?;
            let key = build_instance_key(
                request.function,
                &environment,
                &relevant,
                evidence.as_ref(),
                &request.origin,
            )?;
            Ok::<_, Diagnostic>((environment, relevant, evidence, key))
        })();
        let (environment, relevant, evidence, key) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                if matches!(request.target, InstanceResolutionTarget::Current(_)) {
                    return Err(self.polymorphic_recursion_diagnostic(request, Some(&error)));
                }
                return Err(error);
            }
        };
        if let InstanceResolutionTarget::Current(enclosing) = request.target {
            if key != enclosing.key {
                return Err(self.polymorphic_recursion_diagnostic(request, None));
            }
            return Ok(ResolvedInstanceRequest {
                key: enclosing.key.clone(),
                environment: enclosing.environment.clone(),
                relevant: enclosing.relevant.clone(),
                evidence: enclosing.evidence.clone(),
            });
        }
        Ok(ResolvedInstanceRequest {
            key,
            environment,
            relevant,
            evidence,
        })
    }

    fn polymorphic_recursion_diagnostic(
        &self,
        request: &InstanceResolutionRequest<'_>,
        cause: Option<&Diagnostic>,
    ) -> Diagnostic {
        let name = self
            .functions
            .get(request.function)
            .map(|function| function.name.clone())
            .unwrap_or_else(|| format!("function {}", request.function.0));
        let message = match cause {
            Some(cause) => format!(
                "polymorphic recursion: recursive call to `{name}` does not reuse the enclosing instance ({})",
                cause.message
            ),
            None => format!(
                "polymorphic recursion: recursive call to `{name}` requests a different instance than the enclosing one"
            ),
        };
        Diagnostic::new(request.origin.span.clone(), message)
    }
}

/// Builds the concrete key from the pruned relevant environment and resolved
/// evidence. Every value and evidence argument is canonicalized with the
/// specialization concrete converters, which reject leftover declared parameters,
/// effect variables, and checker placeholders.
fn build_instance_key(
    function: FunctionId,
    environment: &SubstitutionEnvironment,
    relevant: &RelevantParameters,
    evidence: Option<&TraitEvidence>,
    origin: &Origin,
) -> Result<InstanceKey, Diagnostic> {
    let mut substitutions = Vec::new();
    for parameter in relevant.type_parameters() {
        let Some(value_type) = environment.type_value(parameter) else {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!(
                    "cannot form an instance key: type parameter {} has no concrete value",
                    relevant.display(parameter)
                ),
            ));
        };
        substitutions.push(InstanceSubstitution::Type {
            parameter,
            value: CanonicalType::concrete(value_type, origin)?,
        });
    }
    for parameter in relevant.effect_parameters() {
        let Some(effects) = environment.effect_value(parameter) else {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!(
                    "cannot form an instance key: effect parameter {} has no concrete row",
                    relevant.display(parameter)
                ),
            ));
        };
        substitutions.push(InstanceSubstitution::Effect {
            parameter,
            effects: CanonicalEffectSet::concrete(effects, origin)?,
        });
    }
    let evidence = evidence
        .map(|evidence| canonical_evidence(evidence, origin))
        .transpose()?;
    InstanceKey::new(function, substitutions, evidence)
        .map_err(|error| Diagnostic::new(origin.span.clone(), error.message()))
}

struct ParameterCollector<'a> {
    program: &'a LoweredProgram,
    relevant: RelevantParameters,
    families: BTreeSet<&'static str>,
    blocks: HashSet<BlockId>,
    items: HashSet<ItemId>,
    expressions: HashSet<ExpressionId>,
    places: HashSet<PlaceId>,
    patterns: HashSet<PatternId>,
    callbacks: HashSet<LoweredReactiveCallbackId>,
    operations: HashSet<LoweredReactiveOperationId>,
    plans: HashSet<LoweredCoroutinePlanId>,
}

impl<'a> ParameterCollector<'a> {
    fn new(program: &'a LoweredProgram) -> Self {
        ParameterCollector {
            program,
            relevant: RelevantParameters::default(),
            families: BTreeSet::new(),
            blocks: HashSet::new(),
            items: HashSet::new(),
            expressions: HashSet::new(),
            places: HashSet::new(),
            patterns: HashSet::new(),
            callbacks: HashSet::new(),
            operations: HashSet::new(),
            plans: HashSet::new(),
        }
    }

    fn family(&mut self, family: &'static str) {
        self.families.insert(family);
    }

    fn collect_function(&mut self, function: FunctionId) {
        let Some(function) = self.program.functions.get(function) else {
            return;
        };
        let signature = function.signature.clone();
        let parameter_pattern = function.parameter_pattern;
        let parameters = function.parameters.clone();
        let captures = function.captures.clone();
        let body = function.body;
        self.family("function.signature");
        self.collect_function_type(&signature);
        // Declared bounds are deliberately not scanned on their own: a bound
        // that never reaches a signature, body, capture, or evidence record
        // cannot change the emitted instance. Bounds participate through the
        // evidence recipes that reference them.
        self.family("function.parameter-pattern");
        self.collect_pattern(parameter_pattern);
        if !parameters.is_empty() {
            self.family("function.parameters");
            for symbol in &parameters {
                self.collect_symbol_type(*symbol);
            }
        }
        if !captures.is_empty() {
            self.family("function.captures");
            for capture in &captures {
                self.collect_symbol_type(capture.symbol);
            }
        }
        if let Some(plan) = self
            .program
            .coroutine_plan_by_thunk
            .get(&function.semantic_id)
            .copied()
        {
            self.family("function.coroutine-plan");
            self.collect_coroutine_plan(plan);
        }
        if let Some(body) = body {
            self.collect_block(body);
        }
    }

    #[cfg(test)]
    fn collect_initializer(&mut self, initializer: InitializerId) {
        if let Some(initializer) = self.program.initializers.get(initializer) {
            self.collect_block(initializer.body);
        }
    }

    fn collect_symbol_type(&mut self, symbol: SymbolId) {
        let Some(metadata) = self.program.symbols.get(symbol) else {
            return;
        };
        // A function binding whose declared type still contains a declared
        // parameter is a compile-time generic template: the backend stores
        // only its initialization state and never a runtime value of that
        // type, so its parameters cannot make anything relevant. This also
        // covers a generic local function's shared-cell capture, whose
        // recorded capture type is the template itself.
        if metadata.storage == SymbolStorage::FunctionBinding
            && contains_type_parameter(&metadata.value_type)
        {
            return;
        }
        let value_type = metadata.value_type.clone();
        self.collect_type(&value_type);
    }

    fn collect_function_type(&mut self, function: &CheckedFunctionType) {
        self.collect_type(&function.parameter);
        self.collect_effect_set(&function.effects);
        self.collect_type(&function.result);
    }

    fn collect_effect_set(&mut self, effects: &CheckedEffectSet) {
        if let Some(variable) = &effects.variable {
            self.relevant.notice_effect(variable.id, &variable.name);
        }
        for resource in &effects.resources {
            self.collect_type(&resource.value_type);
        }
    }

    fn collect_bound(&mut self, bound: &CheckedTraitBound) {
        for argument in &bound.arguments {
            self.collect_type(argument);
        }
    }

    /// Collects every declared type/effect parameter reachable from a checked
    /// type. `Distinct.representation` is not expanded: its semantics are
    /// carried by the nominal ID plus arguments, matching canonical keys.
    fn collect_type(&mut self, value_type: &CheckedType) {
        match value_type {
            CheckedType::Inferred | CheckedType::Error => {}
            CheckedType::Never
            | CheckedType::I32
            | CheckedType::I8
            | CheckedType::I16
            | CheckedType::I64
            | CheckedType::U8
            | CheckedType::U16
            | CheckedType::U32
            | CheckedType::U64
            | CheckedType::ISize
            | CheckedType::USize
            | CheckedType::F32
            | CheckedType::F64
            | CheckedType::NumberLiteral(_)
            | CheckedType::String
            | CheckedType::StringLiteralSet(_)
            | CheckedType::CString
            | CheckedType::CChar => {}
            CheckedType::Parameter { id, name, .. } => self.relevant.notice_type(*id, name),
            CheckedType::Ref(payload)
            | CheckedType::Slice(payload)
            | CheckedType::Buffer(payload)
            | CheckedType::CPointer { pointee: payload } => self.collect_type(payload),
            CheckedType::Array { element, count } => {
                self.collect_type(element);
                self.collect_type(count);
            }
            CheckedType::TypeConstructor { arguments, .. }
            | CheckedType::Opaque { arguments, .. } => {
                for argument in arguments {
                    self.collect_type(argument);
                }
            }
            CheckedType::Product(product) => {
                for element in &product.elements {
                    self.collect_type(&element.value_type);
                }
            }
            CheckedType::Sum(sum) => {
                for alternative in &sum.alternatives {
                    self.collect_type(alternative);
                }
            }
            CheckedType::Function(function) => self.collect_function_type(function),
            CheckedType::Distinct { arguments, .. } => {
                for argument in arguments {
                    self.collect_type(argument);
                }
            }
        }
    }

    fn collect_block(&mut self, block: BlockId) {
        if !self.blocks.insert(block) {
            return;
        }
        let Some(block) = self.program.blocks.get(block) else {
            return;
        };
        let items = block.items.clone();
        let result = block.result;
        for item in items {
            self.collect_item(item);
        }
        if let Some(result) = result {
            self.collect_expression(result);
        }
    }

    fn collect_item(&mut self, item: ItemId) {
        if !self.items.insert(item) {
            return;
        }
        let Some(item) = self.program.items.get(item) else {
            return;
        };
        match &item.kind {
            LoweredItemKind::Binding(binding) => {
                self.family("item.binding");
                // A generic binding's value is a compile-time function
                // template: the backend records only its initialization state
                // and never evaluates the template value, so its declared
                // parameters cannot make anything relevant to this instance.
                if !binding.generic {
                    if let Some(symbol) = binding.symbol {
                        self.collect_symbol_type(symbol);
                    }
                    if let Some(value) = binding.value {
                        self.collect_expression(value);
                    }
                    if let Some(operation) = binding.reactive {
                        self.collect_reactive_operation(operation);
                    }
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                self.family("item.pattern-binding");
                self.collect_pattern(binding.pattern);
                self.collect_expression(binding.value);
                if let Some(propagation) = &binding.propagation {
                    self.collect_type(&propagation.source);
                    self.collect_type(&propagation.result);
                }
            }
            LoweredItemKind::Assignment(assignment) => {
                self.family("item.assignment");
                self.collect_place(assignment.target);
                self.collect_expression(assignment.value);
                if let Some(dispatch) = &assignment.mutate_index {
                    for argument in &dispatch.arguments {
                        self.collect_type(argument);
                    }
                }
                if let Some(evidence) = &assignment.evidence {
                    self.collect_evidence(evidence);
                }
                if let Some(operation) = assignment.signal_notify {
                    self.collect_reactive_operation(operation);
                }
            }
            LoweredItemKind::Return(item) => {
                self.family("item.return");
                self.collect_expression(item.value);
            }
            LoweredItemKind::Break(item) => {
                self.family("item.break");
                if let Some(value) = item.value {
                    self.collect_expression(value);
                }
            }
            LoweredItemKind::Continue(_) => {
                self.family("item.continue");
            }
            LoweredItemKind::Expression(item) => {
                self.family("item.expression");
                self.collect_expression(item.expression);
            }
        }
    }

    fn collect_expression(&mut self, expression: ExpressionId) {
        if !self.expressions.insert(expression) {
            return;
        }
        let Some(expression) = self.program.expressions.get(expression) else {
            return;
        };
        self.family("expression.header");
        let value_type = expression.value_type.clone();
        let effects = expression.effects.clone();
        let coercion = expression.coercion.clone();
        self.collect_type(&value_type);
        self.collect_effect_set(&effects);
        if let Some(coercion) = &coercion {
            self.collect_type(&coercion.source);
            self.collect_type(&coercion.target);
        }
        match &expression.kind {
            // No accepted lowered program retains a deferral, so this branch
            // has no record family of its own; it is still an explicit
            // decision so a new unlowered route cannot be silently collected.
            LoweredExpressionKind::Deferred(_) => {}
            LoweredExpressionKind::Block(block) => {
                self.family("expression.block");
                self.collect_block(*block);
            }
            LoweredExpressionKind::Name(name) => {
                self.family("expression.name");
                self.collect_symbol_type(name.symbol);
                if let Some(operation) = name.reactive {
                    self.collect_reactive_operation(operation);
                }
            }
            LoweredExpressionKind::Integer(_) => self.family("expression.integer"),
            LoweredExpressionKind::Float(_) => self.family("expression.float"),
            LoweredExpressionKind::String(_) => self.family("expression.string"),
            LoweredExpressionKind::CString(_) => self.family("expression.cstring"),
            LoweredExpressionKind::Access(access) => {
                self.family("expression.access");
                self.collect_expression(access.base);
                match &access.kind {
                    LoweredAccessKind::Representation { dereference }
                    | LoweredAccessKind::Product { dereference, .. }
                    | LoweredAccessKind::Slice { dereference, .. }
                    | LoweredAccessKind::Scalar { dereference } => {
                        for value_type in dereference {
                            self.collect_type(value_type);
                        }
                    }
                }
            }
            LoweredExpressionKind::Product(product) => {
                self.family("expression.product");
                for element in &product.final_type.elements {
                    self.collect_type(&element.value_type);
                }
                for step in &product.steps {
                    match step {
                        LoweredProductStep::Positional { expression, .. }
                        | LoweredProductStep::Designated { expression, .. } => {
                            self.collect_expression(*expression);
                        }
                        LoweredProductStep::PositionalSpread { expression, .. }
                        | LoweredProductStep::NamedSpread { expression, .. } => {
                            self.collect_expression(*expression);
                        }
                        LoweredProductStep::Default {
                            expression,
                            expected,
                            ..
                        } => {
                            self.collect_expression(*expression);
                            self.collect_type(expected);
                        }
                    }
                }
            }
            LoweredExpressionKind::RepeatedProduct(product) => {
                self.family("expression.repeated-product");
                self.collect_expression(product.expression);
                if let LoweredRepeatCount::Symbolic(count) = &product.count {
                    self.collect_type(count);
                }
            }
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.family("expression.satisfies");
                self.collect_expression(satisfies.value);
            }
            LoweredExpressionKind::Logical(logical) => {
                self.family("expression.logical");
                self.collect_expression(logical.left);
                self.collect_expression(logical.right);
                self.collect_type(&logical.bool_type);
            }
            LoweredExpressionKind::Loop(loop_) => {
                self.family("expression.loop");
                self.collect_block(loop_.body);
                self.collect_type(&loop_.result_type);
            }
            LoweredExpressionKind::Match(match_) => {
                self.family("expression.match");
                self.collect_expression(match_.subject);
                self.collect_type(&match_.source);
                for arm in &match_.arms {
                    self.collect_pattern(arm.pattern);
                    self.collect_expression(arm.body);
                    for symbol in &arm.bound_symbols {
                        self.collect_symbol_type(*symbol);
                    }
                }
            }
            LoweredExpressionKind::Index(index) => {
                self.family("expression.index");
                self.collect_expression(index.base);
                self.collect_expression(index.index);
                if let Some(place) = index.base_place {
                    self.collect_place(place);
                }
                if let Some(place) = index.index_place {
                    self.collect_place(place);
                }
                for argument in &index.arguments {
                    self.collect_type(argument);
                }
                if let Some(method_type) = &index.method_type {
                    self.collect_function_type(method_type);
                }
                self.collect_evidence(&index.evidence);
            }
            LoweredExpressionKind::StringTemplate(template) => {
                self.family("expression.string-template");
                for part in &template.parts {
                    match part {
                        LoweredStringTemplatePart::Literal(_) => {}
                        LoweredStringTemplatePart::Interpolation(interpolation) => {
                            self.collect_expression(interpolation.expression);
                            self.collect_type(&interpolation.value_type);
                            self.collect_evidence(&interpolation.evidence);
                        }
                    }
                }
            }
            LoweredExpressionKind::Call(call) => {
                self.family("expression.call");
                self.collect_call(*call);
            }
            LoweredExpressionKind::CallableValue(value) => {
                self.family("expression.callable-value");
                self.collect_callable_value(*value);
            }
            LoweredExpressionKind::Resource(use_) => {
                self.family("expression.resource");
                self.collect_resource_use(*use_);
            }
            LoweredExpressionKind::With(with) => {
                self.family("expression.with");
                if let Some(with) = self.program.withs.get(*with) {
                    self.collect_resource_provider(with.provider);
                    self.collect_expression(with.value);
                    self.collect_block(with.body);
                }
            }
            LoweredExpressionKind::Coro(coro) => {
                self.family("expression.coro");
                if let Some(coro) = self.program.coros.get(*coro) {
                    self.collect_coroutine_creation(coro.plan);
                }
            }
            LoweredExpressionKind::Await(await_) => {
                self.family("expression.await");
                if let Some(await_) = self.program.awaits.get(*await_) {
                    self.collect_expression(await_.operand);
                    self.collect_type(&await_.result_type);
                    match &await_.kind {
                        LoweredAwaitKind::ChildCoroutine {
                            child_result,
                            deferred_resources,
                            ..
                        } => {
                            self.collect_type(child_result);
                            for resource in deferred_resources {
                                self.collect_resource_use(*resource);
                            }
                        }
                        LoweredAwaitKind::Task { result } | LoweredAwaitKind::Wait { result } => {
                            self.collect_type(result);
                        }
                    }
                }
            }
        }
    }

    fn collect_call(&mut self, call: LoweredCallId) {
        let Some(call) = self.program.calls.get(call) else {
            return;
        };
        self.collect_function_type(&call.function_type);
        for argument in &call.arguments {
            if let Some(expression) = argument.expression {
                self.collect_expression(expression);
            }
            self.collect_type(&argument.expected);
            if let Some(place) = argument.place {
                self.collect_place(place);
            }
        }
        for binding in &call.resource_bindings {
            self.collect_resource_use(*binding);
        }
        for step in &call.steps {
            match step {
                LoweredCallStep::Callee { expression }
                | LoweredCallStep::ProductElement { expression, .. }
                | LoweredCallStep::ProductSpread { expression, .. }
                | LoweredCallStep::NamedProductSpread { expression, .. } => {
                    self.collect_expression(*expression);
                }
                LoweredCallStep::Argument { .. }
                | LoweredCallStep::Resource { .. }
                | LoweredCallStep::Invoke => {}
                LoweredCallStep::Default {
                    expression,
                    expected,
                    ..
                } => {
                    self.collect_expression(*expression);
                    self.collect_type(expected);
                }
            }
        }
        self.collect_type(&call.result_type);
        if let Some(operation) = call.reactive {
            self.collect_reactive_operation(operation);
        }
        for substitution in &call.substitutions.types {
            self.collect_type(&substitution.value_type);
        }
        for substitution in &call.substitutions.effects {
            self.collect_effect_set(&substitution.effects);
        }
        if let Some(evidence) = &call.evidence {
            self.collect_evidence(evidence);
        }
    }

    fn collect_callable_value(&mut self, value: LoweredCallableValueId) {
        let Some(value) = self.program.callable_values.get(value) else {
            return;
        };
        self.collect_function_type(&value.function_type);
        if let Some(closure) = &value.closure {
            for capture in &closure.captures {
                // Collect through the symbol so a compile-time template's
                // shared-cell capture never contributes its declared
                // parameters; the capture's recorded type is the template.
                self.collect_symbol_type(capture.capture.symbol);
            }
            for substitution in &closure.substitutions.types {
                self.collect_type(&substitution.value_type);
            }
            for substitution in &closure.substitutions.effects {
                self.collect_effect_set(&substitution.effects);
            }
        }
        for substitution in &value.substitutions.types {
            self.collect_type(&substitution.value_type);
        }
        for substitution in &value.substitutions.effects {
            self.collect_effect_set(&substitution.effects);
        }
        if let Some(evidence) = &value.evidence {
            self.collect_evidence(evidence);
        }
    }

    fn collect_place(&mut self, place: PlaceId) {
        if !self.places.insert(place) {
            return;
        }
        let Some(place) = self.program.places.get(place) else {
            return;
        };
        self.family("place");
        self.collect_type(&place.value_type);
        match &place.kind {
            LoweredPlaceKind::Symbol { symbol } | LoweredPlaceKind::CapturedCell { symbol } => {
                self.collect_symbol_type(*symbol);
            }
            LoweredPlaceKind::Temporary { expression } => self.collect_expression(*expression),
            LoweredPlaceKind::Resource { use_ } => self.collect_resource_use(*use_),
            LoweredPlaceKind::Dereference {
                reference,
                dereference,
            } => {
                self.collect_expression(*reference);
                for value_type in dereference {
                    self.collect_type(value_type);
                }
            }
            LoweredPlaceKind::ProductElement { base, .. }
            | LoweredPlaceKind::Representation { base } => self.collect_place(*base),
            LoweredPlaceKind::Indexed { base, index } => {
                self.collect_place(*base);
                self.collect_expression(*index);
            }
        }
    }

    fn collect_pattern(&mut self, pattern: PatternId) {
        if !self.patterns.insert(pattern) {
            return;
        }
        let Some(pattern) = self.program.patterns.get(pattern) else {
            return;
        };
        self.family("pattern");
        self.collect_type(&pattern.value_type);
        self.collect_type(&pattern.test.subject);
        match &pattern.kind {
            LoweredPatternKind::Wildcard => {}
            LoweredPatternKind::Binding { symbol, .. } => {
                if let Some(symbol) = symbol {
                    self.collect_symbol_type(*symbol);
                }
            }
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.collect_pattern(*element);
                }
            }
            LoweredPatternKind::Nominal { argument, .. } => self.collect_pattern(*argument),
            LoweredPatternKind::Literal { .. } => {}
            LoweredPatternKind::At { binding, pattern } => {
                self.collect_pattern(*binding);
                self.collect_pattern(*pattern);
            }
        }
    }

    fn collect_resource_provider(&mut self, provider: LoweredResourceProviderId) {
        let Some(provider) = self.program.resource_providers.get(provider) else {
            return;
        };
        self.family("resource-provider");
        self.collect_type(&provider.resource.value_type);
    }

    fn collect_resource_use(&mut self, use_: LoweredResourceUseId) {
        let Some(use_) = self.program.resource_uses.get(use_) else {
            return;
        };
        self.family("resource-use");
        self.collect_type(&use_.resource.value_type);
        if let Some(provider) = use_.provider {
            self.collect_resource_provider(provider);
        }
    }

    fn collect_reactive_operation(&mut self, operation: LoweredReactiveOperationId) {
        if !self.operations.insert(operation) {
            return;
        }
        let Some(operation) = self.program.reactive_operations.get(operation) else {
            return;
        };
        self.family("reactive-operation");
        match &operation.kind {
            LoweredReactiveOperationKind::SignalCreate { symbol, .. }
            | LoweredReactiveOperationKind::SignalRead { symbol }
            | LoweredReactiveOperationKind::SignalNotify { symbol }
            | LoweredReactiveOperationKind::DerivedRead { symbol } => {
                self.collect_symbol_type(*symbol);
            }
            LoweredReactiveOperationKind::DerivedCreate {
                symbol,
                function_type,
                captures,
                ..
            } => {
                self.collect_symbol_type(*symbol);
                self.collect_function_type(function_type);
                for capture in captures {
                    self.collect_symbol_type(capture.symbol);
                }
            }
            LoweredReactiveOperationKind::Scope | LoweredReactiveOperationKind::Snapshot => {}
            LoweredReactiveOperationKind::Reaction {
                callback,
                reactive_provider,
            } => {
                self.collect_reactive_callback(*callback);
                if let Some(provider) = reactive_provider {
                    self.collect_resource_provider(*provider);
                }
            }
            LoweredReactiveOperationKind::Until {
                predicate,
                reactive_provider,
            } => {
                self.collect_reactive_callback(*predicate);
                if let Some(provider) = reactive_provider {
                    self.collect_resource_provider(*provider);
                }
            }
            LoweredReactiveOperationKind::Batch { callback } => {
                self.collect_reactive_callback(*callback);
            }
        }
    }

    fn collect_reactive_callback(&mut self, callback: LoweredReactiveCallbackId) {
        if !self.callbacks.insert(callback) {
            return;
        }
        let Some(callback) = self.program.reactive_callbacks.get(callback) else {
            return;
        };
        self.family("reactive-callback");
        self.collect_function_type(&callback.function_type);
        for capture in &callback.captures {
            self.collect_symbol_type(capture.symbol);
        }
        for resource in &callback.resources {
            self.collect_resource_use(*resource);
        }
        if let Some(callable) = callback.callable {
            self.collect_expression(callable);
        }
    }

    /// A creation site uses the child's result, deferred effects, and capture
    /// layout, but its frame locals and awaits belong to the body thunk.
    fn collect_coroutine_creation(&mut self, plan: LoweredCoroutinePlanId) {
        let Some(plan) = self.program.coroutine_plans.get(plan) else {
            return;
        };
        self.collect_type(&plan.result_type);
        self.collect_effect_set(&plan.deferred_effects);
        for capture in &plan.captures {
            self.collect_symbol_type(capture.symbol);
        }
    }

    fn collect_coroutine_plan(&mut self, plan: LoweredCoroutinePlanId) {
        if !self.plans.insert(plan) {
            return;
        }
        let Some(plan) = self.program.coroutine_plans.get(plan) else {
            return;
        };
        self.collect_type(&plan.result_type);
        self.collect_effect_set(&plan.deferred_effects);
        for capture in &plan.captures {
            self.collect_symbol_type(capture.symbol);
        }
        for symbol in &plan.frame_bindings {
            self.collect_symbol_type(*symbol);
        }
        for value_type in &plan.await_result_types {
            self.collect_type(value_type);
        }
        for await_ in &plan.awaits {
            if let Some(await_) = self.program.awaits.get(*await_) {
                self.collect_expression(await_.operand);
                self.collect_type(&await_.result_type);
                if let LoweredAwaitKind::ChildCoroutine {
                    child_result,
                    deferred_resources,
                    ..
                } = &await_.kind
                {
                    self.collect_type(child_result);
                    for resource in deferred_resources {
                        self.collect_resource_use(*resource);
                    }
                } else if let LoweredAwaitKind::Task { result }
                | LoweredAwaitKind::Wait { result } = &await_.kind
                {
                    self.collect_type(result);
                }
            }
        }
    }

    fn collect_evidence(&mut self, evidence: &TraitEvidence) {
        self.family("trait-evidence");
        match evidence {
            TraitEvidence::ExplicitImplementation { arguments, .. }
            | TraitEvidence::Structural { arguments, .. } => {
                for argument in arguments {
                    self.collect_type(argument);
                }
            }
            TraitEvidence::DeclaredBound {
                arguments,
                prerequisites,
                ..
            } => {
                for argument in arguments {
                    self.collect_type(argument);
                }
                for bound in prerequisites {
                    self.collect_bound(bound);
                }
            }
        }
    }
}

impl LoweredProgram {
    /// The type and effect parameters relevant to one function template.
    ///
    /// The scan is deterministic: arenas are visited in insertion order and
    /// shared nodes are visited once. Nested function bodies are scanned under
    /// their own `FunctionId`; this function only follows the records that
    /// construct or invoke them.
    pub(crate) fn relevant_parameters(&self, function: FunctionId) -> RelevantParameters {
        let mut collector = ParameterCollector::new(self);
        collector.collect_function(function);
        collector.relevant
    }

    /// Builds the resolved substitution environment for one request.
    ///
    /// The enclosing environment and the site recipe are merged first, then
    /// the target template's full checked signature is matched against the
    /// complete checked callable type at the site and any missing values are
    /// inferred from it. The inference runs against a pre-resolved site type,
    /// so enclosing values are honored, and every relevant parameter must end
    /// up concrete or a source diagnostic at `origin` is returned.
    pub(crate) fn resolve_substitutions(
        &self,
        function: FunctionId,
        origin: &Origin,
        function_type: &CheckedFunctionType,
        substitutions: &CallSubstitutions,
        enclosing: Option<&SubstitutionEnvironment>,
    ) -> Result<(SubstitutionEnvironment, RelevantParameters), Diagnostic> {
        let Some(template) = self.functions.get(function) else {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!("function {} has no lowered template", function.0),
            ));
        };
        let relevant = self.relevant_parameters(function);
        let signature = template.signature.clone();
        let mut builder = EnvironmentBuilder::new(origin);
        let enclosing_map = enclosing
            .map(|enclosing| enclosing.substitution_map())
            .unwrap_or_default();
        if let Some(enclosing) = enclosing {
            builder.add_enclosing(enclosing);
        }
        builder.add_site(substitutions, &enclosing_map);
        let pre_resolved = builder.resolve()?;
        let map = pre_resolved.substitution_map();
        let actual = substitute_type(CheckedType::Function(function_type.clone()), &map);
        let mut inferred = HashMap::new();
        if !infer_type_parameters(
            &CheckedType::Function(signature.clone()),
            &actual,
            &mut inferred,
        ) {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!(
                    "checked callable type `{}` does not match the template signature `{}` for function {}",
                    CheckedType::Function(function_type.clone()),
                    CheckedType::Function(signature.clone()),
                    function.0
                ),
            ));
        }
        builder.add_inferred(inferred);
        let environment = builder.resolve()?;
        require_concrete_substitutions(&environment, &relevant, origin)?;
        Ok((environment, relevant))
    }

    /// The substitution environment composed from the enclosing
    /// instance and an evidence-only site's recipe (a trait call, index read,
    /// indexed assignment, or formatting interpolation).
    ///
    /// No target-template inference runs here: the selected function may only
    /// be known after the evidence resolves, so this environment is used to
    /// resolve the recipe first and to concretize recorded method types for
    /// structural artifact keys.
    pub(crate) fn site_environment(
        &self,
        origin: &Origin,
        substitutions: &CallSubstitutions,
        enclosing: Option<&SubstitutionEnvironment>,
    ) -> Result<SubstitutionEnvironment, Diagnostic> {
        let mut builder = EnvironmentBuilder::new(origin);
        let enclosing_map = enclosing
            .map(|enclosing| enclosing.substitution_map())
            .unwrap_or_default();
        if let Some(enclosing) = enclosing {
            builder.add_enclosing(enclosing);
        }
        builder.add_site(substitutions, &enclosing_map);
        builder.resolve()
    }

    /// Every record family the collector visits for one function template.
    /// Used by the coverage test to prove each parameter-bearing family has an
    /// explicit collector decision.
    #[cfg(test)]
    pub(crate) fn parameter_record_families(&self, function: FunctionId) -> BTreeSet<&'static str> {
        let mut collector = ParameterCollector::new(self);
        collector.collect_function(function);
        collector.families
    }

    /// The record families the collector visits under every module
    /// initializer, for the coverage test.
    #[cfg(test)]
    pub(crate) fn initializer_record_families(&self) -> BTreeSet<&'static str> {
        let mut collector = ParameterCollector::new(self);
        for (id, _) in self.initializers.iter() {
            collector.collect_initializer(id);
        }
        collector.families
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{
        CheckedTypeElement, NameResolver, ProgramLoader, TypeChecker, TypedModule,
        contains_type_parameter,
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

    fn lower(source: &str) -> (TypedModule, LoweredProgram) {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());
        (module, program)
    }

    fn function_id(program: &LoweredProgram, name: &str) -> FunctionId {
        program
            .functions
            .iter()
            .find(|(_, _, function)| {
                function.name == name || function.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"))
    }

    fn relevance(program: &LoweredProgram, name: &str) -> RelevantParameters {
        program.relevant_parameters(function_id(program, name))
    }

    #[test]
    fn relevant_parameter_collection_is_deterministic() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied = identity 1\n",
        ));
        let name = "identity";
        let function = function_id(&program, name);
        let first = program.relevant_parameters(function);
        assert_eq!(first, program.relevant_parameters(function));
        assert_eq!(first.type_parameters().count(), 1);
        assert_eq!(first.effect_parameters().count(), 0);
    }

    #[test]
    fn signature_only_parameter_is_found() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied = identity 1\n",
        ));
        let relevant = relevance(&program, "identity");
        assert!(!relevant.is_empty());
        assert_eq!(relevant.effect_parameters().count(), 0);
        assert_eq!(relevant.type_parameters().count(), 1);
    }

    #[test]
    fn body_only_parameter_is_found() {
        let (_, program) = lower(concat!(
            "type Phantom T = ctor ()\n",
            "def phantom: <T> () -> Phantom T = () => Phantom ()\n",
            "def body_only: <T> I32 -> I32 = value => {\n",
            "  let hidden: Phantom T = phantom ()\n",
            "  value\n",
            "}\n",
        ));
        let function = function_id(&program, "body_only");
        assert!(
            !contains_type_parameter(&CheckedType::Function(
                program.functions.get(function).unwrap().signature.clone()
            )),
            "the body-only fixture must keep its signature concrete"
        );
        let relevant = program.relevant_parameters(function);
        assert_eq!(relevant.effect_parameters().count(), 0);
        assert_eq!(relevant.type_parameters().count(), 1);
    }

    #[test]
    fn capture_only_parameter_is_found_under_its_own_function() {
        let (_, program) = lower(concat!(
            "def capture_only: <T where Copy T> T -> () -> I32 = value => () => {\n",
            "  let copied: T = value\n",
            "  0\n",
            "}\n",
            "let made = capture_only 1\n",
        ));
        let mut found_capture_only = false;
        for (_, _, function) in program.functions.iter() {
            let relevant = program.relevant_parameters(function.semantic_id);
            if !relevant.is_empty()
                && !contains_type_parameter(&CheckedType::Function(function.signature.clone()))
                && !function.captures.is_empty()
            {
                found_capture_only = true;
            }
        }
        assert!(
            found_capture_only,
            "a nested closure whose signature is concrete but whose captures mention an outer parameter must be relevant"
        );
    }

    #[test]
    fn evidence_only_and_effect_only_parameters_are_found() {
        let (_, program) = lower(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
            "def effectful: <effect E> () ->{E} () = () => ()\n",
            "let applied = show_bound 1\n",
        ));
        let show_bound = function_id(&program, "show_bound");
        let relevant = program.relevant_parameters(show_bound);
        assert_eq!(relevant.type_parameters().count(), 1);
        let families = program.parameter_record_families(show_bound);
        assert!(
            families.contains("trait-evidence"),
            "the declared-bound call evidence must be scanned: {families:?}"
        );

        let effectful = relevance(&program, "effectful");
        assert_eq!(effectful.type_parameters().count(), 0);
        assert_eq!(effectful.effect_parameters().count(), 1);
    }

    #[test]
    fn irrelevant_outer_parameter_is_excluded() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def irrelevant: <T where Copy T> I32 -> I32 = value => value\n",
            "let applied = identity 1\n",
            "let concrete = irrelevant 1\n",
        ));
        let relevant = relevance(&program, "irrelevant");
        assert!(
            relevant.is_empty(),
            "an unused outer parameter must not enter the relevant set: {relevant:?}"
        );
    }

    #[test]
    fn parameter_record_families_are_unique_and_cover_the_collector() {
        let mut seen = BTreeSet::new();
        for family in PARAMETER_RECORD_FAMILIES {
            assert!(
                seen.insert(*family),
                "record family {family} appears twice in the decision table"
            );
        }
    }

    #[test]
    fn coverage_fixture_exercises_every_collector_family() {
        let (_, program) = lower(concat!(
            "use std.cinterop.*\n",
            "use std.coroutine.(Coroutine)\n",
            "use std.fmt.Formatter\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "type Counter = ctor (value: I32)\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "type Phantom T = ctor ()\n",
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def body_only: <T> I32 -> I32 = value => {\n",
            "  let hidden: Phantom T = Phantom ()\n",
            "  value\n",
            "}\n",
            "def capture_only: <T where Copy T> T -> () -> I32 = value => () => {\n",
            "  let copied: T = value\n",
            "  0\n",
            "}\n",
            "def effectful: <effect E> () ->{E} () = () => ()\n",
            "def callable = (value: I32) => value\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
            "let integer: I32 = identity 42\n",
            "let shown: Bool = show_bound 1\n",
            "let floated: F64 = 1.5\n",
            "let string: String = \"text\"\n",
            "let cstring = c_string \"c\"\n",
            "let product = (left: 1, right: 2)\n",
            "let left = product.left\n",
            "let values: (I32; 2) = (1, 2)\n",
            "let element = values[0]\n",
            "def logical = (flag: Bool) => flag && flag\n",
            "def looping = () => loop { break 1 }\n",
            "def continuing = () => loop { continue\nbreak 1 }\n",
            "def returning: () -> I32 = () => { return 1 }\n",
            "def matching = (value: Ok I32 | IOError) => match value {\n",
            "  Ok inner => inner,\n",
            "  other => 0,\n",
            "}\n",
            "def blocked = () => { let local: I32 = 1; local }\n",
            "def repeat = () => { let repeated: (I32; 3) = (7; 3); repeated }\n",
            "def templated = () => { let rendered: String = \"value=${integer}\"; rendered }\n",
            "let coerced: I8 = 42 satisfies I8\n",
            "let repeated: (I32; 3) = (7; 3)\n",
            "let template: String = \"value=${integer}\"\n",
            "let applied = callable 1\n",
            "let closure = callable\n",
            "def task: () -> Coroutine{} I32 = () => coro { 7 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro { await (task ()) }\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
            "def scoped_increment = () => { with mut Counter = counter { increment (); () } }\n",
            "let signal observed = 0\n",
            "let doubled_signal = observed + observed\n",
            "with Reactive = reactive_scope () {\n",
            "  reaction { let current = observed; () }\n",
            "  batch { observed = 1 }\n",
            "  let snapshotted = snapshot observed\n",
            "}\n",
        ));
        let mut visited = program.initializer_record_families();
        for (_, _, function) in program.functions.iter() {
            visited.extend(program.parameter_record_families(function.semantic_id));
        }
        for family in PARAMETER_RECORD_FAMILIES {
            assert!(
                visited.contains(family),
                "the coverage fixture never reached collector family {family}; \
                 add a record of that family or record an explicit no-parameter decision"
            );
        }
    }

    fn test_origin() -> Origin {
        Origin {
            syntax: SyntaxId(7),
            span: Span::from(0..3),
        }
    }

    fn parameter_type(id: usize, name: &str) -> CheckedType {
        CheckedType::Parameter {
            id: TypeParameterId(id),
            name: name.to_owned(),
            sized: true,
        }
    }

    fn nominal(id: usize, name: &str) -> CheckedType {
        CheckedType::TypeConstructor {
            id: TypeId(id),
            name: name.to_owned(),
            arguments: Vec::new(),
        }
    }

    fn io_effects() -> CheckedEffectSet {
        CheckedEffectSet::canonical(vec![CheckedResource {
            value_type: nominal(11, "IO"),
            mutable: false,
        }])
    }

    fn build_environment(
        candidates: Vec<(usize, SubstitutionValue, SubstitutionSource)>,
    ) -> Result<SubstitutionEnvironment, Diagnostic> {
        let origin = test_origin();
        let mut builder = EnvironmentBuilder::new(&origin);
        for (parameter, value, source) in candidates {
            builder.add(TypeParameterId(parameter), value, source);
        }
        builder.resolve()
    }

    fn chain_candidate(
        parameter: usize,
        referenced: usize,
        source: SubstitutionSource,
    ) -> (usize, SubstitutionValue, SubstitutionSource) {
        (
            parameter,
            SubstitutionValue::Type(parameter_type(referenced, "T")),
            source,
        )
    }

    #[test]
    fn agreeing_sources_merge_and_conflicts_diagnose() {
        let environment = build_environment(vec![
            (
                0,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::EnclosingInstance,
            ),
            (
                0,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::Site,
            ),
            (
                0,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::Inferred,
            ),
            (
                1,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::Site,
            ),
            (
                1,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::Inferred,
            ),
        ])
        .expect("agreeing sources resolve");
        assert_eq!(
            environment.type_value(TypeParameterId(0)),
            Some(&CheckedType::I32)
        );
        assert_eq!(
            environment.type_value(TypeParameterId(1)),
            Some(&CheckedType::I32)
        );

        // A placeholder candidate is completed by a concrete one, not treated
        // as a conflict.
        let environment = build_environment(vec![
            (
                0,
                SubstitutionValue::Type(CheckedType::Inferred),
                SubstitutionSource::Inferred,
            ),
            (
                0,
                SubstitutionValue::Type(CheckedType::U8),
                SubstitutionSource::Site,
            ),
        ])
        .expect("a placeholder is completed");
        assert_eq!(
            environment.type_value(TypeParameterId(0)),
            Some(&CheckedType::U8)
        );

        let error = build_environment(vec![
            (
                3,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::Site,
            ),
            (
                3,
                SubstitutionValue::Type(CheckedType::U8),
                SubstitutionSource::Inferred,
            ),
        ])
        .expect_err("conflicting concrete values diagnose");
        assert!(
            error
                .message
                .contains("conflicting substitutions for parameter 3"),
            "{error:?}"
        );
        assert!(error.message.contains("the call site"), "{error:?}");
        assert!(error.message.contains("checked callable type"), "{error:?}");
    }

    #[test]
    fn type_effect_kind_mismatch_diagnoses() {
        let error = build_environment(vec![
            (
                4,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::Site,
            ),
            (
                4,
                SubstitutionValue::Effects(io_effects()),
                SubstitutionSource::Inferred,
            ),
        ])
        .expect_err("a type and an effect row for one parameter diagnose");
        assert!(
            error.message.contains("receives a type from the call site"),
            "{error:?}"
        );
        assert!(
            error
                .message
                .contains("an effect row from the checked callable type"),
            "{error:?}"
        );
    }

    #[test]
    fn transitive_chains_resolve_in_parameter_order() {
        let environment = build_environment(vec![
            chain_candidate(5, 6, SubstitutionSource::Site),
            (
                6,
                SubstitutionValue::Type(CheckedType::I64),
                SubstitutionSource::EnclosingInstance,
            ),
        ])
        .expect("a chain resolves");
        assert_eq!(
            environment.type_value(TypeParameterId(5)),
            Some(&CheckedType::I64)
        );

        let environment = build_environment(vec![
            chain_candidate(7, 8, SubstitutionSource::Site),
            chain_candidate(8, 9, SubstitutionSource::Site),
            (
                9,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::EnclosingInstance,
            ),
        ])
        .expect("a longer chain resolves");
        assert_eq!(
            environment.type_value(TypeParameterId(7)),
            Some(&CheckedType::I32)
        );
        assert_eq!(
            environment.type_value(TypeParameterId(8)),
            Some(&CheckedType::I32)
        );
    }

    #[test]
    fn self_and_longer_cycles_are_detected_before_substitution() {
        let error = build_environment(vec![(
            10,
            SubstitutionValue::Type(CheckedType::Ref(Box::new(parameter_type(10, "T")))),
            SubstitutionSource::Site,
        )])
        .expect_err("a self-reference diagnoses");
        assert!(error.message.contains("substitution cycle"), "{error:?}");

        let tautology = build_environment(vec![chain_candidate(10, 10, SubstitutionSource::Site)])
            .expect("a tautological self-mapping is not a cycle");
        assert!(tautology.is_empty());

        let error = build_environment(vec![
            chain_candidate(11, 12, SubstitutionSource::Site),
            chain_candidate(12, 11, SubstitutionSource::Site),
        ])
        .expect_err("a longer cycle diagnoses");
        assert!(error.message.contains("substitution cycle"), "{error:?}");
    }

    #[test]
    fn relevant_parameters_must_be_concrete() {
        let origin = test_origin();
        let mut relevant = RelevantParameters::default();
        relevant.notice_type(TypeParameterId(20), "T");
        relevant.notice_effect(TypeParameterId(21), "E");

        let empty = SubstitutionEnvironment::default();
        let error = require_concrete_substitutions(&empty, &relevant, &origin)
            .expect_err("a missing entry diagnoses");
        assert!(error.message.contains("`T`"), "{error:?}");

        let mut builder = EnvironmentBuilder::new(&origin);
        builder.add(
            TypeParameterId(20),
            SubstitutionValue::Type(CheckedType::I32),
            SubstitutionSource::Site,
        );
        let half = builder.resolve().expect("type-only environment");
        let error = require_concrete_substitutions(&half, &relevant, &origin)
            .expect_err("a missing effect entry diagnoses");
        assert!(error.message.contains("`E`"), "{error:?}");

        let mut builder = EnvironmentBuilder::new(&origin);
        builder.add(
            TypeParameterId(20),
            SubstitutionValue::Type(CheckedType::Inferred),
            SubstitutionSource::Site,
        );
        builder.add(
            TypeParameterId(21),
            SubstitutionValue::Effects(CheckedEffectSet {
                variable: Some(crate::CheckedEffectVariable {
                    id: TypeParameterId(99),
                    name: "Outer".to_owned(),
                }),
                resources: Vec::new(),
                state: None,
            }),
            SubstitutionSource::Site,
        );
        let placeholder = builder.resolve().expect("placeholder environment resolves");
        let error = require_concrete_substitutions(&placeholder, &relevant, &origin)
            .expect_err("placeholders diagnose");
        assert!(error.message.contains("placeholder"), "{error:?}");

        let mut builder = EnvironmentBuilder::new(&origin);
        builder.add(
            TypeParameterId(20),
            SubstitutionValue::Type(CheckedType::I32),
            SubstitutionSource::Site,
        );
        builder.add(
            TypeParameterId(21),
            SubstitutionValue::Effects(io_effects()),
            SubstitutionSource::Site,
        );
        let concrete = builder.resolve().expect("concrete environment resolves");
        require_concrete_substitutions(&concrete, &relevant, &origin)
            .expect("a concrete environment passes");

        let pruned = concrete.pruned(&relevant);
        assert_eq!(pruned.len(), 2);
        assert!(pruned.type_value(TypeParameterId(20)).is_some());
        assert!(pruned.effect_value(TypeParameterId(21)).is_some());
    }

    fn direct_call<'a>(program: &'a LoweredProgram, name: &str) -> (FunctionId, &'a LoweredCall) {
        let target = function_id(program, name);
        let call = program
            .calls
            .iter()
            .find_map(|(_, call)| match &call.target {
                LoweredCallableTarget::DirectFunction { function, .. } if *function == target => {
                    Some(call)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no direct call to {name}"));
        (target, call)
    }

    fn resolve_named_call(
        program: &LoweredProgram,
        name: &str,
    ) -> (SubstitutionEnvironment, RelevantParameters) {
        let (function, call) = direct_call(program, name);
        program
            .resolve_substitutions(
                function,
                &call.origin,
                &call.function_type,
                &call.substitutions,
                None,
            )
            .unwrap_or_else(|diagnostic| panic!("{name} should resolve: {diagnostic:?}"))
    }

    #[test]
    fn result_only_parameter_infers_from_the_checked_callable_type() {
        let (_, program) = lower(concat!(
            "type Phantom T = ctor ()\n",
            "def phantom_result: <T> () -> Phantom T = () => Phantom ()\n",
            "let hidden: Phantom I32 = phantom_result ()\n",
        ));
        let (environment, relevant) = resolve_named_call(&program, "phantom_result");
        let parameter = relevant.type_parameters().next().expect("T is relevant");
        assert_eq!(
            environment.type_value(parameter),
            Some(&CheckedType::I32),
            "the result-only argument resolves to its concrete value"
        );
    }

    #[test]
    fn nonempty_fixed_effect_rows_infer_from_the_callable_type() {
        let (_, program) = lower(concat!(
            "use std.io.IO\n",
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "def with_io: () ->{IO} () = () => ()\n",
            "def call_io: () ->{IO} I32 = () => evaluate { with_io (); 0 }\n",
        ));
        let (environment, relevant) = resolve_named_call(&program, "evaluate");
        let type_parameter = relevant.type_parameters().next().expect("T is relevant");
        let effect_parameter = relevant.effect_parameters().next().expect("E is relevant");
        assert_eq!(
            environment.type_value(type_parameter),
            Some(&CheckedType::I32)
        );
        let effects = environment
            .effect_value(effect_parameter)
            .expect("E resolves to a concrete row");
        assert!(effects.variable.is_none());
        assert!(
            !effects.resources.is_empty(),
            "the IO resource stays in the row: {effects:?}"
        );
    }

    #[test]
    fn empty_effect_rows_resolve() {
        let (_, program) = lower(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "let pure: I32 = evaluate { 0 }\n",
        ));
        let (environment, relevant) = resolve_named_call(&program, "evaluate");
        let effect_parameter = relevant.effect_parameters().next().expect("E is relevant");
        let effects = environment
            .effect_value(effect_parameter)
            .expect("E resolves");
        assert!(effects.variable.is_none());
        assert!(effects.is_empty(), "the row stays empty: {effects:?}");
    }

    #[test]
    fn state_effects_resolve_into_the_effect_row() {
        let (_, program) = lower(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "let mut count = 0\n",
            "def use_state: () ->{state} I32 = () => evaluate { count = count + 1; count }\n",
        ));
        let (environment, relevant) = resolve_named_call(&program, "evaluate");
        let effect_parameter = relevant.effect_parameters().next().expect("E is relevant");
        let effects = environment
            .effect_value(effect_parameter)
            .expect("E resolves");
        assert!(effects.variable.is_none());
        assert!(
            effects.state.is_some(),
            "the state effect stays in the row: {effects:?}"
        );
    }

    #[test]
    fn partial_site_substitutions_are_completed_by_inference() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied: I32 = identity 1\n",
        ));
        let (function, call) = direct_call(&program, "identity");
        let (environment, relevant) = program
            .resolve_substitutions(
                function,
                &call.origin,
                &call.function_type,
                &CallSubstitutions::default(),
                None,
            )
            .expect("an empty site recipe is completed by the checked callable type");
        let parameter = relevant.type_parameters().next().expect("T");
        assert_eq!(environment.type_value(parameter), Some(&CheckedType::I32));
    }

    #[test]
    fn inferred_values_override_a_stale_enclosing_instantiation() {
        // Legal same-function recursion at a different type: the enclosing
        // instance maps `T` to `Ref I32` while the nested call re-instantiates
        // it to `I32`. The fresh inferred value must win, in either candidate
        // order, matching the backend's specialization queue.
        for candidates in [
            vec![
                (
                    0,
                    SubstitutionValue::Type(CheckedType::Ref(Box::new(CheckedType::I32))),
                    SubstitutionSource::EnclosingInstance,
                ),
                (
                    0,
                    SubstitutionValue::Type(CheckedType::I32),
                    SubstitutionSource::Inferred,
                ),
            ],
            vec![
                (
                    0,
                    SubstitutionValue::Type(CheckedType::I32),
                    SubstitutionSource::Inferred,
                ),
                (
                    0,
                    SubstitutionValue::Type(CheckedType::Ref(Box::new(CheckedType::I32))),
                    SubstitutionSource::EnclosingInstance,
                ),
            ],
        ] {
            let environment = build_environment(candidates).expect("the fresh inferred value wins");
            assert_eq!(
                environment.type_value(TypeParameterId(0)),
                Some(&CheckedType::I32)
            );
        }

        // An inferred placeholder never overrides a concrete enclosing value.
        let environment = build_environment(vec![
            (
                0,
                SubstitutionValue::Type(CheckedType::Inferred),
                SubstitutionSource::Inferred,
            ),
            (
                0,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::EnclosingInstance,
            ),
        ])
        .expect("the concrete enclosing value completes the placeholder");
        assert_eq!(
            environment.type_value(TypeParameterId(0)),
            Some(&CheckedType::I32)
        );

        // A recorded site value still conflicts with the fresh inference.
        let error = build_environment(vec![
            (
                0,
                SubstitutionValue::Type(CheckedType::U8),
                SubstitutionSource::Site,
            ),
            (
                0,
                SubstitutionValue::Type(CheckedType::I32),
                SubstitutionSource::Inferred,
            ),
        ])
        .expect_err("site and inference still diagnose");
        assert!(
            error.message.contains("conflicting substitutions"),
            "{error:?}"
        );
    }

    #[test]
    fn recorded_and_inferred_sources_conflict_at_the_origin() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied: I32 = identity 1\n",
        ));
        let (function, call) = direct_call(&program, "identity");
        let parameter = program
            .relevant_parameters(function)
            .type_parameters()
            .next()
            .expect("T");
        let substitutions = CallSubstitutions {
            types: vec![crate::CallTypeSubstitution {
                parameter,
                value_type: CheckedType::U8,
            }],
            effects: Vec::new(),
        };
        let error = program
            .resolve_substitutions(
                function,
                &call.origin,
                &call.function_type,
                &substitutions,
                None,
            )
            .expect_err("a recorded value that disagrees with inference diagnoses");
        assert!(
            error.message.contains("conflicting substitutions"),
            "{error:?}"
        );

        let substitutions = CallSubstitutions {
            types: Vec::new(),
            effects: vec![crate::CallEffectSubstitution {
                parameter,
                effects: io_effects(),
            }],
        };
        let error = program
            .resolve_substitutions(
                function,
                &call.origin,
                &call.function_type,
                &substitutions,
                None,
            )
            .expect_err("an effect row recorded for a type parameter diagnoses");
        assert!(
            error.message.contains("receives an effect row"),
            "{error:?}"
        );
    }

    #[test]
    fn unconstrained_and_mismatched_requests_diagnose_at_the_origin() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let applied: I32 = identity 1\n",
        ));
        let (function, call) = direct_call(&program, "identity");
        let unresolved = CheckedFunctionType {
            parameter: Box::new(parameter_type(99, "Outer")),
            result: Box::new(parameter_type(99, "Outer")),
            ..checked_function_type(CheckedType::I32, CheckedType::I32)
        };
        let error = program
            .resolve_substitutions(
                function,
                &call.origin,
                &unresolved,
                &CallSubstitutions::default(),
                None,
            )
            .expect_err("an unconstrained parameter diagnoses");
        assert!(
            error.message.contains("cannot resolve type parameter"),
            "{error:?}"
        );
        assert!(error.message.contains("`T`"), "{error:?}");

        let mismatched = checked_function_type(CheckedType::I32, CheckedType::U8);
        let error = program
            .resolve_substitutions(
                function,
                &call.origin,
                &mismatched,
                &CallSubstitutions::default(),
                None,
            )
            .expect_err("a mismatched callable type diagnoses");
        assert!(
            error
                .message
                .contains("does not match the template signature"),
            "{error:?}"
        );
    }

    fn checked_function_type(parameter: CheckedType, result: CheckedType) -> CheckedFunctionType {
        CheckedFunctionType {
            parameter: Box::new(parameter),
            parameter_style: staple_syntax::FunctionParameterStyle::Single,
            default: None,
            mutations: Vec::new(),
            moves: Vec::new(),
            effects: CheckedEffectSet::default(),
            result: Box::new(result),
        }
    }

    fn trait_id_named(program: &LoweredProgram, name: &str) -> TraitId {
        program
            .traits
            .iter()
            .find(|(_, _, trait_)| {
                trait_.name == name || trait_.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered trait named {name}"))
    }

    fn direct_calls<'a>(
        program: &'a LoweredProgram,
        name: &str,
    ) -> Vec<(FunctionId, &'a LoweredCall)> {
        let target = function_id(program, name);
        program
            .calls
            .iter()
            .filter_map(|(_, call)| match &call.target {
                LoweredCallableTarget::DirectFunction { function, .. } if *function == target => {
                    Some((target, call))
                }
                _ => None,
            })
            .collect()
    }

    fn enclosing_environment(program: &LoweredProgram, name: &str) -> SubstitutionEnvironment {
        let (function, call) = direct_call(program, name);
        let (environment, _) = program
            .resolve_substitutions(
                function,
                &call.origin,
                &call.function_type,
                &call.substitutions,
                None,
            )
            .unwrap_or_else(|diagnostic| panic!("{name} should resolve: {diagnostic:?}"));
        environment
    }

    fn declared_bound_evidence(program: &LoweredProgram, trait_name: &str) -> TraitEvidence {
        let trait_id = trait_id_named(program, trait_name);
        program
            .calls
            .iter()
            .filter_map(|(_, call)| call.evidence.as_ref())
            .chain(
                program
                    .callable_values
                    .iter()
                    .filter_map(|(_, value)| value.evidence.as_ref()),
            )
            .find(|evidence| {
                matches!(
                    evidence,
                    TraitEvidence::DeclaredBound { trait_id: candidate, .. } if *candidate == trait_id
                )
            })
            .cloned()
            .unwrap_or_else(|| panic!("no declared-bound evidence for trait {trait_name}"))
    }

    /// The declared-bound evidence recorded inside one function template's own
    /// body, excluding nested closure bodies and other functions.
    fn declared_bound_evidence_in(
        program: &LoweredProgram,
        trait_name: &str,
        owner: FunctionId,
    ) -> TraitEvidence {
        let trait_id = trait_id_named(program, trait_name);
        for (_, expression) in program.expressions.iter() {
            if expression.key.owner != ExpressionOwner::Function(owner) {
                continue;
            }
            let evidence = match &expression.kind {
                LoweredExpressionKind::Call(call) => program
                    .calls
                    .get(*call)
                    .and_then(|call| call.evidence.as_ref()),
                LoweredExpressionKind::CallableValue(value) => program
                    .callable_values
                    .get(*value)
                    .and_then(|value| value.evidence.as_ref()),
                _ => None,
            };
            if let Some(evidence) = evidence
                && matches!(
                    evidence,
                    TraitEvidence::DeclaredBound { trait_id: candidate, .. } if *candidate == trait_id
                )
            {
                return evidence.clone();
            }
        }
        panic!("no declared-bound evidence for trait {trait_name} in the template body")
    }

    fn resolve_evidence(
        program: &LoweredProgram,
        evidence: &TraitEvidence,
        environment: &SubstitutionEnvironment,
    ) -> TraitEvidence {
        program
            .resolve_trait_evidence(&test_origin(), Some(evidence), environment)
            .unwrap_or_else(|diagnostic| panic!("evidence should resolve: {diagnostic:?}"))
            .expect("a method target")
    }

    fn explicit_function(evidence: &TraitEvidence) -> FunctionId {
        match evidence {
            TraitEvidence::ExplicitImplementation { function, .. } => *function,
            other => panic!("expected an explicit implementation, got {other:?}"),
        }
    }

    fn implementation_arguments(evidence: &TraitEvidence) -> Vec<CheckedType> {
        match evidence {
            TraitEvidence::ExplicitImplementation { arguments, .. }
            | TraitEvidence::Structural { arguments, .. } => arguments.clone(),
            other => panic!("expected resolved evidence, got {other:?}"),
        }
    }

    #[test]
    fn resolves_direct_explicit_methods_and_defaults() {
        let (module, program) = lower(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "trait TestDefaulted T {\n",
            "  test_primary: T -> Bool\n",
            "  test_fallback: T -> Bool = value => test_primary value\n",
            "}\n",
            "impl TestDefaulted I32 { def test_primary = _ => True }\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
            "def fallback_bound: <T where TestDefaulted T> T -> Bool = value => test_fallback value\n",
            "let shown: Bool = show_bound 1\n",
            "let fell: Bool = fallback_bound 1\n",
        ));
        let show_bound = function_id(&program, "show_bound");
        let environment = enclosing_environment(&program, "show_bound");
        let evidence = declared_bound_evidence_in(&program, "TestShow", show_bound);
        let evidence = resolve_evidence(&program, &evidence, &environment);
        assert_eq!(implementation_arguments(&evidence), vec![CheckedType::I32]);
        let show_trait = trait_id_named(&program, "TestShow");
        let show_method = program.traits.get(show_trait).expect("show trait").methods[0];
        let expected = module
            .trait_impl_method(show_trait, &[CheckedType::I32], show_method)
            .expect("the checker selects the same method");
        assert_eq!(explicit_function(&evidence), expected);

        let fallback_bound = function_id(&program, "fallback_bound");
        let environment = enclosing_environment(&program, "fallback_bound");
        let evidence = declared_bound_evidence_in(&program, "TestDefaulted", fallback_bound);
        let evidence = resolve_evidence(&program, &evidence, &environment);
        let defaulted = program
            .traits
            .get(trait_id_named(&program, "TestDefaulted"))
            .expect("defaulted trait");
        let fallback_method = defaulted
            .methods
            .iter()
            .copied()
            .find(|method| {
                program
                    .trait_methods
                    .get(*method)
                    .is_some_and(|method| method.name == "test_fallback")
            })
            .expect("fallback method");
        let default_function = defaulted
            .default_methods
            .iter()
            .find(|(method, _)| *method == fallback_method)
            .map(|(_, function)| *function)
            .expect("a default implementation");
        assert_eq!(explicit_function(&evidence), default_function);
    }

    #[test]
    fn signature_identical_implementations_stay_distinct() {
        let (_, program) = lower(concat!(
            "type Alpha = ctor ()\n",
            "type Beta = ctor ()\n",
            "trait TestTag T { test_tag: T -> I32 }\n",
            "impl TestTag Alpha { def test_tag = _ => 1 }\n",
            "impl TestTag Beta { def test_tag = _ => 2 }\n",
            "def use_tag: <T where TestTag T> T -> I32 = value => test_tag value\n",
            "let one: I32 = use_tag (Alpha ())\n",
            "let two: I32 = use_tag (Beta ())\n",
        ));
        let calls = direct_calls(&program, "use_tag");
        assert_eq!(calls.len(), 2);
        let mut functions = Vec::new();
        for (function, call) in calls {
            let (environment, _) = program
                .resolve_substitutions(
                    function,
                    &call.origin,
                    &call.function_type,
                    &call.substitutions,
                    None,
                )
                .expect("the outer instance resolves");
            let evidence = declared_bound_evidence_in(&program, "TestTag", function);
            let evidence = resolve_evidence(&program, &evidence, &environment);
            functions.push(explicit_function(&evidence));
        }
        assert_ne!(
            functions[0], functions[1],
            "two implementations with the same callable signature stay distinct"
        );
    }

    #[test]
    fn conditional_implementations_discharge_their_bounds() {
        let (module, program) = lower(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "trait TestGuarded T { guarded: T -> Bool }\n",
            "impl<T where TestShow T> TestGuarded T { def guarded = value => test_show value }\n",
            "type Plain = ctor ()\n",
            "def use_guarded: <T where TestGuarded T> T -> Bool = value => guarded value\n",
            "let ok: Bool = use_guarded 1\n",
        ));
        let use_guarded = function_id(&program, "use_guarded");
        let environment = enclosing_environment(&program, "use_guarded");
        let evidence = declared_bound_evidence_in(&program, "TestGuarded", use_guarded);
        let evidence = resolve_evidence(&program, &evidence, &environment);
        assert_eq!(implementation_arguments(&evidence), vec![CheckedType::I32]);
        let guarded = trait_id_named(&program, "TestGuarded");
        let guarded_method = program.traits.get(guarded).expect("guarded trait").methods[0];
        let expected = module
            .trait_impl_method(guarded, &[CheckedType::I32], guarded_method)
            .expect("the checker selects the conditional implementation");
        assert_eq!(explicit_function(&evidence), expected);

        let plain = program
            .types
            .iter()
            .find(|(_, _, metadata)| metadata.name == "Plain")
            .expect("Plain type")
            .2;
        let plain_type = CheckedType::Distinct {
            id: plain.semantic_id,
            name: plain.name.clone(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::empty_product()),
        };
        let evidence = TraitEvidence::DeclaredBound {
            trait_id: trait_id_named(&program, "TestGuarded"),
            method: Some(guarded_method),
            arguments: vec![plain_type],
            prerequisites: Vec::new(),
        };
        let error = program
            .resolve_trait_evidence(
                &test_origin(),
                Some(&evidence),
                &SubstitutionEnvironment::default(),
            )
            .expect_err("an unsatisfied conditional bound diagnoses");
        assert!(
            error
                .message
                .contains("unsatisfied conditional prerequisite"),
            "{error:?}"
        );
    }

    #[test]
    fn transitive_prerequisites_are_discharged_recursively() {
        let (_, program) = lower(concat!(
            "trait TestBase T { base_test: T -> Bool }\n",
            "trait TestDerived T where TestBase T { derived_test: T -> Bool }\n",
            "impl TestBase I32 { def base_test = _ => True }\n",
            "impl TestDerived I32 { def derived_test = value => base_test value }\n",
            "def use_derived: <T where TestDerived T> T -> Bool = value => base_test value\n",
            "let result: Bool = use_derived 1\n",
        ));
        let use_derived = function_id(&program, "use_derived");
        let environment = enclosing_environment(&program, "use_derived");
        let evidence = declared_bound_evidence_in(&program, "TestBase", use_derived);
        let evidence = resolve_evidence(&program, &evidence, &environment);
        assert_eq!(implementation_arguments(&evidence), vec![CheckedType::I32]);

        // A conditional implementation discharges its own bound through
        // another implementation, not through a declared precondition.
        let (_, program) = lower(concat!(
            "trait TestInner T { inner_test: T -> I32 }\n",
            "trait TestOuter T { outer_test: T -> I32 }\n",
            "impl TestInner I32 { def inner_test = _ => 1 }\n",
            "impl<T where TestInner T> TestOuter T { def outer_test = value => inner_test value }\n",
            "def use_outer: <T where TestOuter T> T -> I32 = value => outer_test value\n",
            "let result: I32 = use_outer 1\n",
        ));
        let use_outer = function_id(&program, "use_outer");
        let environment = enclosing_environment(&program, "use_outer");
        let evidence = declared_bound_evidence_in(&program, "TestOuter", use_outer);
        let evidence = resolve_evidence(&program, &evidence, &environment);
        assert_eq!(implementation_arguments(&evidence), vec![CheckedType::I32]);
    }

    #[test]
    fn functional_dependencies_complete_inferred_arguments() {
        let (_, program) = lower(concat!(
            "trait TestConvert Target Position Output where {Target, Position} ~> Output {\n",
            "  test_convert: (Target, Position) -> Output\n",
            "}\n",
            "impl TestConvert I32 I32 I32 { def test_convert = pair => pair.0 }\n",
        ));
        let convert = trait_id_named(&program, "TestConvert");
        let method = program.traits.get(convert).expect("convert trait").methods[0];
        let evidence = TraitEvidence::DeclaredBound {
            trait_id: convert,
            method: Some(method),
            arguments: vec![CheckedType::I32, CheckedType::I32, CheckedType::Inferred],
            prerequisites: Vec::new(),
        };
        let resolved = program
            .resolve_trait_evidence(
                &test_origin(),
                Some(&evidence),
                &SubstitutionEnvironment::default(),
            )
            .expect("the dependent argument completes")
            .expect("a method target");
        assert_eq!(
            implementation_arguments(&resolved),
            vec![CheckedType::I32, CheckedType::I32, CheckedType::I32]
        );
    }

    #[test]
    fn completion_ignores_bounds_with_different_known_arguments() {
        let (_, program) = lower(concat!(
            "trait TestConvert Target Output where Target ~> Output {\n",
            "  test_convert: Target -> Output\n",
            "}\n",
        ));
        let convert = trait_id_named(&program, "TestConvert");
        let context = TraitSelectionContext::new(
            &program,
            vec![
                CheckedTraitBound {
                    trait_id: convert,
                    arguments: vec![CheckedType::I64, CheckedType::U64],
                },
                CheckedTraitBound {
                    trait_id: convert,
                    arguments: vec![CheckedType::I32, CheckedType::U32],
                },
            ],
        );
        assert_eq!(
            context
                .complete_obligation_arguments(convert, &[CheckedType::I32, CheckedType::Inferred]),
            Some(vec![CheckedType::I32, CheckedType::U32]),
            "an unrelated declared bound must not conflict with the matching bound"
        );
    }

    #[test]
    fn structural_selections_and_obligation_only_bounds_resolve() {
        let (_, program) = lower("let pair = (1, 2)\n");
        let index = program
            .semantic_ids
            .index_trait
            .expect("Index in the prelude");
        let method = program.traits.get(index).expect("Index trait").methods[0];
        let product = CheckedType::Product(CheckedProductType {
            elements: vec![
                CheckedTypeElement {
                    name: None,
                    value_type: CheckedType::I32,
                    default: None,
                },
                CheckedTypeElement {
                    name: None,
                    value_type: CheckedType::I32,
                    default: None,
                },
            ],
            variadic: false,
        });
        let evidence = TraitEvidence::DeclaredBound {
            trait_id: index,
            method: Some(method),
            arguments: vec![product, CheckedType::USize, CheckedType::I32],
            prerequisites: Vec::new(),
        };
        let resolved = program
            .resolve_trait_evidence(
                &test_origin(),
                Some(&evidence),
                &SubstitutionEnvironment::default(),
            )
            .expect("the structural derivation resolves")
            .expect("a structural target");
        match resolved {
            TraitEvidence::Structural {
                structural: StructuralTraitMethod::Index,
                arguments,
                ..
            } => assert_eq!(arguments.len(), 3),
            other => panic!("expected a structural Index selection, got {other:?}"),
        }

        let copy = program
            .semantic_ids
            .copy_trait
            .expect("Copy in the prelude");
        let evidence = TraitEvidence::DeclaredBound {
            trait_id: copy,
            method: None,
            arguments: vec![CheckedType::I32],
            prerequisites: Vec::new(),
        };
        let resolved = program
            .resolve_trait_evidence(
                &test_origin(),
                Some(&evidence),
                &SubstitutionEnvironment::default(),
            )
            .expect("a method-less bound proves its obligation");
        assert!(resolved.is_none(), "no method target is invented");
    }

    #[test]
    fn negative_implementations_are_rejected() {
        let (_, program) = lower(concat!(
            "type MyString = ctor String\n",
            "impl !Copy MyString {}\n",
        ));
        let negative = program
            .trait_implementations
            .iter()
            .find(|(_, metadata)| metadata.negative)
            .expect("a negative implementation");
        let argument = negative.1.arguments[0].clone();
        let evidence = TraitEvidence::DeclaredBound {
            trait_id: negative.1.trait_id,
            method: None,
            arguments: vec![argument],
            prerequisites: Vec::new(),
        };
        let error = program
            .resolve_trait_evidence(
                &test_origin(),
                Some(&evidence),
                &SubstitutionEnvironment::default(),
            )
            .expect_err("a negative implementation rejects the obligation");
        assert!(
            error.message.contains("negative implementation"),
            "{error:?}"
        );
    }

    #[test]
    fn ambiguous_and_cyclic_obligations_are_rejected() {
        let (_, program) = lower(concat!(
            "trait TestConvert Target Position Output where {Target, Position} ~> Output {\n",
            "  test_convert: (Target, Position) -> Output\n",
            "}\n",
            "impl TestConvert I32 I32 I32 { def test_convert = pair => pair.0 }\n",
        ));
        let convert = trait_id_named(&program, "TestConvert");
        let method = program.traits.get(convert).expect("convert trait").methods[0];
        let mut context = TraitSelectionContext::new(&program, Vec::new());
        let mut conflicting = context.implementations[0].clone();
        // Two headers can both unify an inferred position but disagree on the
        // completion; the checker's coherence rules prevent this in a real
        // program, so the defensive ambiguity path is exercised directly.
        conflicting.arguments = vec![CheckedType::I32, CheckedType::I32, nominal(5, "Other")];
        context.implementations.push(conflicting);
        context
            .implementation_ids
            .push(LoweredTraitImplementationId::for_test(0));
        let error = context
            .select_method(
                convert,
                &[CheckedType::I32, CheckedType::I32, CheckedType::Inferred],
                method,
                &test_origin(),
            )
            .expect_err("two disagreeing completions diagnose");
        assert!(error.message.contains("ambiguous"), "{error:?}");

        let (_, program) = lower(concat!(
            "trait TestCycle T { cycle_test: T -> Bool }\n",
            "impl<T where TestCycle T> TestCycle T { def cycle_test = _ => True }\n",
        ));
        let cycle = trait_id_named(&program, "TestCycle");
        let method = program.traits.get(cycle).expect("cycle trait").methods[0];
        let context = TraitSelectionContext::new(&program, Vec::new());
        let error = context
            .select_method(cycle, &[CheckedType::I32], method, &test_origin())
            .expect_err("a cyclic obligation diagnoses");
        assert!(error.message.contains("cyclic obligation"), "{error:?}");
    }

    #[test]
    fn bounds_with_outer_parameters_resolve_after_substitution() {
        let (_, program) = lower(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def outer_bound: <T where TestShow T> T -> () -> Bool = value => () => test_show value\n",
            "let applied = (outer_bound 1) ()\n",
        ));
        let evidence = declared_bound_evidence(&program, "TestShow");
        let TraitEvidence::DeclaredBound { arguments, .. } = &evidence else {
            unreachable!()
        };
        assert!(
            arguments.iter().any(contains_type_parameter),
            "the template recipe keeps the declared parameter: {arguments:?}"
        );
        let environment = enclosing_environment(&program, "outer_bound");
        let resolved = program
            .resolve_trait_evidence(&test_origin(), Some(&evidence), &environment)
            .expect("the bound resolves once the outer parameter is concrete")
            .expect("a method target");
        assert_eq!(implementation_arguments(&resolved), vec![CheckedType::I32]);
    }

    #[test]
    fn recorded_explicit_and_structural_evidence_is_preserved() {
        let (_, program) = lower(concat!(
            "use std.fmt.Formatter\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "let direct: Bool = test_show 1\n",
            "let pair = (1, 2)\n",
            "let first = pair[0]\n",
            "let debugged = \"${pair:?}\"\n",
        ));
        let explicit = program
            .calls
            .iter()
            .filter_map(|(_, call)| call.evidence.as_ref())
            .find(|evidence| matches!(evidence, TraitEvidence::ExplicitImplementation { .. }))
            .cloned()
            .expect("an explicit implementation recipe");
        let resolved = program
            .resolve_trait_evidence(
                &test_origin(),
                Some(&explicit),
                &SubstitutionEnvironment::default(),
            )
            .expect("the recorded explicit selection validates")
            .expect("a method target");
        match (&explicit, &resolved) {
            (
                TraitEvidence::ExplicitImplementation {
                    function: before, ..
                },
                TraitEvidence::ExplicitImplementation {
                    function: after, ..
                },
            ) => assert_eq!(before, after),
            _ => panic!("expected the explicit selection to be preserved"),
        }

        let structural = program
            .expressions
            .iter()
            .find_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Index(index) => match &index.evidence {
                    evidence @ TraitEvidence::Structural { .. } => Some(evidence.clone()),
                    _ => None,
                },
                LoweredExpressionKind::StringTemplate(template) => {
                    template.parts.iter().find_map(|part| match part {
                        LoweredStringTemplatePart::Interpolation(interpolation) => {
                            match &interpolation.evidence {
                                evidence @ TraitEvidence::Structural { .. } => {
                                    Some(evidence.clone())
                                }
                                _ => None,
                            }
                        }
                        LoweredStringTemplatePart::Literal(_) => None,
                    })
                }
                _ => None,
            })
            .expect("a structural recipe");
        assert!(
            matches!(structural, TraitEvidence::Structural { .. }),
            "fixture should carry structural evidence: {structural:?}"
        );
        let resolved = program
            .resolve_trait_evidence(
                &test_origin(),
                Some(&structural),
                &SubstitutionEnvironment::default(),
            )
            .expect("the recorded structural selection validates")
            .expect("a structural target");
        match (&structural, &resolved) {
            (
                TraitEvidence::Structural {
                    structural: before, ..
                },
                TraitEvidence::Structural {
                    structural: after, ..
                },
            ) => assert_eq!(before, after),
            _ => panic!("expected the structural selection to be preserved"),
        }
    }

    fn resolved_root(program: &LoweredProgram, name: &str) -> ResolvedInstanceRequest {
        let target = function_id(program, name);
        let call = program
            .calls
            .iter()
            .find_map(|(_, call)| match &call.target {
                LoweredCallableTarget::DirectFunction {
                    function,
                    environment: LoweredCallEnvironment::None,
                } if *function == target => Some(call),
                _ => None,
            })
            .or_else(|| {
                program
                    .calls
                    .iter()
                    .find_map(|(_, call)| match &call.target {
                        LoweredCallableTarget::DirectFunction { function, .. }
                            if *function == target =>
                        {
                            Some(call)
                        }
                        _ => None,
                    })
            })
            .unwrap_or_else(|| panic!("no call to {name}"));
        program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: target,
                origin: call.origin.clone(),
                function_type: call.function_type.clone(),
                substitutions: call.substitutions.clone(),
                evidence: call.evidence.clone(),
                target: InstanceResolutionTarget::Root,
            })
            .unwrap_or_else(|diagnostic| panic!("{name} should resolve: {diagnostic:?}"))
    }

    #[test]
    fn repeated_equivalent_requests_yield_equal_keys() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "let second: I32 = identity 1\n",
        ));
        let calls = direct_calls(&program, "identity");
        assert_eq!(calls.len(), 2);
        let mut keys = Vec::new();
        for (function, call) in calls {
            let resolved = program
                .resolve_instance_request(&InstanceResolutionRequest {
                    function,
                    origin: call.origin.clone(),
                    function_type: call.function_type.clone(),
                    substitutions: call.substitutions.clone(),
                    evidence: call.evidence.clone(),
                    target: InstanceResolutionTarget::Root,
                })
                .expect("the request resolves");
            keys.push(resolved.key);
        }
        assert_eq!(keys[0], keys[1], "equivalent requests yield equal keys");
    }

    #[test]
    fn irrelevant_outer_substitutions_deduplicate() {
        let (_, program) = lower(concat!(
            "def ignore: <T where Copy T> I32 -> I32 = value => value\n",
            "let applied = ignore 1\n",
        ));
        let function = function_id(&program, "ignore");
        let parameter =
            match &program.functions.get(function).expect("ignore").bounds[0].arguments[0] {
                CheckedType::Parameter { id, .. } => *id,
                other => panic!("expected a declared bound parameter, got {other:?}"),
            };
        let function_type = checked_function_type(CheckedType::I32, CheckedType::I32);
        let mut keys = Vec::new();
        for value_type in [CheckedType::I32, CheckedType::U8] {
            let substitutions = CallSubstitutions {
                types: vec![crate::CallTypeSubstitution {
                    parameter,
                    value_type,
                }],
                effects: Vec::new(),
            };
            let resolved = program
                .resolve_instance_request(&InstanceResolutionRequest {
                    function,
                    origin: test_origin(),
                    function_type: function_type.clone(),
                    substitutions,
                    evidence: None,
                    target: InstanceResolutionTarget::Root,
                })
                .expect("the irrelevant parameter is not required");
            assert!(
                resolved.key.substitutions().is_empty(),
                "an irrelevant outer parameter never enters the key"
            );
            keys.push(resolved.key);
        }
        assert_eq!(keys[0], keys[1]);
    }

    #[test]
    fn capture_dependent_substitutions_separate_instances() {
        let (_, program) = lower(concat!(
            "def maker: <T where Copy T> T -> () -> I32 = value => () => {\n",
            "  let copied: T = value\n",
            "  0\n",
            "}\n",
            "let made_i32 = maker 1\n",
            "let made_string = maker \"x\"\n",
        ));
        let thunk = program
            .functions
            .iter()
            .find(|(_, _, function)| {
                !function.captures.is_empty()
                    && function.signature.parameter.as_ref() == &CheckedType::empty_product()
            })
            .map(|(_, id, _)| id)
            .expect("the anonymous closure");
        let (value_id, value) = program
            .callable_values
            .iter()
            .find(|(_, value)| {
                matches!(
                    &value.target,
                    LoweredCallableTarget::DirectFunction { function, .. } if *function == thunk
                ) && value.closure.is_some()
            })
            .expect("the closure construction value");
        let _ = value_id;

        let mut keys = Vec::new();
        for call in direct_calls(&program, "maker") {
            let outer = program
                .resolve_instance_request(&InstanceResolutionRequest {
                    function: call.0,
                    origin: call.1.origin.clone(),
                    function_type: call.1.function_type.clone(),
                    substitutions: call.1.substitutions.clone(),
                    evidence: call.1.evidence.clone(),
                    target: InstanceResolutionTarget::Root,
                })
                .expect("the enclosing instance resolves");
            let resolved = program
                .resolve_instance_request(&InstanceResolutionRequest {
                    function: thunk,
                    origin: value.origin.clone(),
                    function_type: value.function_type.clone(),
                    substitutions: value.substitutions.clone(),
                    evidence: value.evidence.clone(),
                    target: InstanceResolutionTarget::Nested(&outer),
                })
                .expect("the nested closure resolves");
            assert!(
                !resolved.key.substitutions().is_empty(),
                "the captured outer parameter enters the closure key"
            );
            keys.push(resolved.key);
        }
        assert_eq!(keys.len(), 2);
        assert_ne!(
            keys[0], keys[1],
            "different captured outer arguments separate the closure instances"
        );
    }

    #[test]
    fn same_key_recursion_succeeds_and_changed_keys_diagnose() {
        let (_, program) = lower(concat!(
            "def recursive: <T where Copy T> T -> T = value => recursive value\n",
            "let result: I32 = recursive 1\n",
        ));
        let outer = resolved_root(&program, "recursive");
        let (recursive, inner) = program
            .calls
            .iter()
            .find_map(|(_, call)| match &call.target {
                LoweredCallableTarget::DirectFunction {
                    function,
                    environment: LoweredCallEnvironment::Current,
                } => Some((*function, call)),
                _ => None,
            })
            .expect("the recursive call keeps the current environment");
        let resolved = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: recursive,
                origin: inner.origin.clone(),
                function_type: inner.function_type.clone(),
                substitutions: inner.substitutions.clone(),
                evidence: inner.evidence.clone(),
                target: InstanceResolutionTarget::Current(&outer),
            })
            .expect("same-key recursion succeeds");
        assert_eq!(resolved.key, outer.key);

        let changed = checked_function_type(CheckedType::U8, CheckedType::U8);
        let error = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: recursive,
                origin: test_origin(),
                function_type: changed,
                substitutions: CallSubstitutions::default(),
                evidence: None,
                target: InstanceResolutionTarget::Current(&outer),
            })
            .expect_err("a changed recursive substitution diagnoses");
        assert!(error.message.contains("polymorphic recursion"), "{error:?}");
    }

    #[test]
    fn function_valued_recursion_reuses_the_current_environment() {
        let (_, program) = lower(concat!(
            "def recursive_value: I32 -> I32 = value => {\n",
            "  let self_ref = recursive_value\n",
            "  self_ref value\n",
            "}\n",
            "let result: I32 = recursive_value 1\n",
        ));
        let recursive_value = function_id(&program, "recursive_value");
        let signature = program
            .functions
            .get(recursive_value)
            .expect("recursive_value")
            .signature
            .clone();
        let outer = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: recursive_value,
                origin: test_origin(),
                function_type: signature,
                substitutions: CallSubstitutions::default(),
                evidence: None,
                target: InstanceResolutionTarget::Root,
            })
            .expect("the outer instance resolves");
        let (value_id, value) = program
            .callable_values
            .iter()
            .find(|(_, value)| {
                matches!(
                    &value.target,
                    LoweredCallableTarget::DirectFunction { function, .. }
                        if *function == recursive_value
                ) && value
                    .closure
                    .as_ref()
                    .is_some_and(|closure| closure.environment == LoweredClosureEnvironment::Stored)
            })
            .expect("the recursive function value");
        let _ = value_id;
        let resolved = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: match value.target {
                    LoweredCallableTarget::DirectFunction { function, .. } => function,
                    _ => unreachable!(),
                },
                origin: value.origin.clone(),
                function_type: value.function_type.clone(),
                substitutions: value.substitutions.clone(),
                evidence: value.evidence.clone(),
                target: InstanceResolutionTarget::Current(&outer),
            })
            .expect("the recursive value reuses the current instance");
        assert_eq!(resolved.key, outer.key);
    }

    #[test]
    fn unresolved_requests_never_reach_a_key() {
        let (_, program) = lower(concat!(
            "trait TestAbsent T { absent: T -> Bool }\n",
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def absent_bound: <T where TestAbsent T> T -> Bool = value => absent value\n",
            "let applied: I32 = identity 1\n",
        ));
        let identity = function_id(&program, "identity");
        let unresolved = CheckedFunctionType {
            parameter: Box::new(parameter_type(99, "Outer")),
            result: Box::new(parameter_type(99, "Outer")),
            ..checked_function_type(CheckedType::I32, CheckedType::I32)
        };
        let error = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: identity,
                origin: test_origin(),
                function_type: unresolved,
                substitutions: CallSubstitutions::default(),
                evidence: None,
                target: InstanceResolutionTarget::Root,
            })
            .expect_err("an unresolved parameter never reaches a key");
        assert!(
            error.message.contains("cannot resolve type parameter"),
            "{error:?}"
        );

        let absent_bound = function_id(&program, "absent_bound");
        let absent_result = program
            .functions
            .get(absent_bound)
            .expect("absent_bound")
            .signature
            .result
            .as_ref()
            .clone();
        let evidence = declared_bound_evidence_in(&program, "TestAbsent", absent_bound);
        let error = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: absent_bound,
                origin: test_origin(),
                function_type: checked_function_type(CheckedType::I32, absent_result),
                substitutions: CallSubstitutions {
                    types: vec![crate::CallTypeSubstitution {
                        parameter: match evidence {
                            TraitEvidence::DeclaredBound { ref arguments, .. } => {
                                match &arguments[0] {
                                    CheckedType::Parameter { id, .. } => *id,
                                    other => panic!("expected a parameter, got {other:?}"),
                                }
                            }
                            _ => unreachable!(),
                        },
                        value_type: CheckedType::I32,
                    }],
                    effects: Vec::new(),
                },
                evidence: Some(evidence),
                target: InstanceResolutionTarget::Root,
            })
            .expect_err("unresolved evidence never reaches a key");
        assert!(
            error.message.contains("no implementation of trait"),
            "{error:?}"
        );
    }

    fn checked_program_at(
        entry: &std::path::Path,
        source: &str,
        root: &std::path::Path,
    ) -> TypedModule {
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
    fn cross_module_generic_functions_resolve_against_owned_catalogs() {
        let root =
            std::env::temp_dir().join(format!("staple-instance-resolution-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temp root");
        std::fs::write(
            root.join("tools.sta"),
            "pub mod\npub def wrap: <T where Copy T> T -> T = value => value\n",
        )
        .expect("write module");
        let entry = root.join("main.sta");
        let module =
            checked_program_at(&entry, "use tools.wrap\nlet wrapped: I32 = wrap 1\n", &root);
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());
        let (function, call) = direct_call(&program, "wrap");
        let resolved = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function,
                origin: call.origin.clone(),
                function_type: call.function_type.clone(),
                substitutions: call.substitutions.clone(),
                evidence: call.evidence.clone(),
                target: InstanceResolutionTarget::Root,
            })
            .expect("a cross-module generic request resolves");
        let parameter = resolved
            .relevant
            .type_parameters()
            .next()
            .expect("T is relevant");
        assert_eq!(
            resolved.environment.type_value(parameter),
            Some(&CheckedType::I32)
        );
    }

    #[test]
    fn coroutine_body_thunks_keep_their_outer_parameters() {
        let (_, program) = lower(concat!(
            "use std.coroutine.*\n",
            "def make_task: <T where Copy T> T -> Coroutine{} T = value => coro { value }\n",
            "let task = make_task 1\n",
        ));
        let thunk = program
            .functions
            .iter()
            .find(|(_, _, function)| function.class.coroutine_body)
            .map(|(_, id, _)| id)
            .expect("the coroutine body thunk");
        let relevant = program.relevant_parameters(thunk);
        assert_eq!(
            relevant.type_parameters().count(),
            1,
            "the captured outer parameter stays relevant to the coroutine thunk"
        );
        let outer = resolved_root(&program, "make_task");
        let resolved = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: thunk,
                origin: test_origin(),
                function_type: match &program.functions.get(thunk).expect("thunk").signature {
                    signature => signature.clone(),
                },
                substitutions: CallSubstitutions::default(),
                evidence: None,
                target: InstanceResolutionTarget::Nested(&outer),
            })
            .expect("the coroutine thunk instance resolves");
        assert!(!resolved.key.substitutions().is_empty());
    }

    #[test]
    fn coroutine_creation_does_not_collect_child_body_only_parameters() {
        let (_, mut program) = lower(concat!(
            "use std.coroutine.*\n",
            "def make_task: <T where Copy T> T -> Coroutine{} T = value => coro { value }\n",
            "let task = make_task 1\n",
        ));
        let (plan_id, thunk) = program
            .coroutine_plans
            .iter()
            .map(|(id, plan)| (id, plan.thunk))
            .next()
            .expect("a coroutine plan");
        let child_only = TypeParameterId(9999);
        program
            .coroutine_plans
            .get_mut(plan_id)
            .expect("coroutine plan")
            .await_result_types
            .push(CheckedType::Parameter {
                id: child_only,
                name: "ChildOnly".to_owned(),
                sized: true,
            });
        let outer = relevance(&program, "make_task");
        assert!(!outer.contains_type(child_only));
        assert!(program.relevant_parameters(thunk).contains_type(child_only));
    }

    #[test]
    fn resolved_values_agree_with_checked_specialization_inference() {
        let (module, program) = lower(concat!(
            "type Phantom T = ctor ()\n",
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def phantom_result: <T> () -> Phantom T = () => Phantom ()\n",
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T; N) = value => n => (value; N)\n",
            "let applied: I32 = identity 1\n",
            "let hidden: Phantom I32 = phantom_result ()\n",
            "let repeated: (I32; 3) = repeat 7 3\n",
        ));
        for name in ["identity", "phantom_result", "repeat"] {
            let (function, call) = direct_call(&program, name);
            let template = module
                .type_of_function(function)
                .expect("checked template signature");
            let mut checked_decision = HashMap::new();
            let unifies = infer_type_parameters(
                &CheckedType::Function(template.clone()),
                &CheckedType::Function(call.function_type.clone()),
                &mut checked_decision,
            );
            assert!(unifies, "the emitter path infers `{name}` substitutions");
            let (environment, _) = program
                .resolve_substitutions(
                    function,
                    &call.origin,
                    &call.function_type,
                    &call.substitutions,
                    None,
                )
                .unwrap_or_else(|diagnostic| panic!("{name} should resolve: {diagnostic:?}"));
            for (parameter, value_type) in checked_decision {
                match effect_substitution_value(&value_type) {
                    Some(effects) => {
                        let resolved = environment.effect_value(parameter).unwrap_or_else(|| {
                            panic!("`{name}` should resolve effect parameter {parameter:?}")
                        });
                        assert_eq!(
                            resolved, effects,
                            "`{name}` effect row agrees with the emitter"
                        );
                    }
                    None => {
                        let resolved = environment.type_value(parameter).unwrap_or_else(|| {
                            panic!("`{name}` should resolve type parameter {parameter:?}")
                        });
                        assert_eq!(
                            resolved, &value_type,
                            "`{name}` type substitution agrees with the emitter"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn resolved_methods_agree_with_the_checker_selector() {
        let (module, program) = lower(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "trait TestDefaulted T {\n",
            "  test_primary: T -> Bool\n",
            "  test_fallback: T -> Bool = value => test_primary value\n",
            "}\n",
            "impl TestDefaulted I32 { def test_primary = _ => True }\n",
            "trait TestInner T { inner_test: T -> I32 }\n",
            "trait TestOuter T { outer_test: T -> I32 }\n",
            "impl TestInner I32 { def inner_test = _ => 1 }\n",
            "impl<T where TestInner T> TestOuter T { def outer_test = value => inner_test value }\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
            "def fallback_bound: <T where TestDefaulted T> T -> Bool = value => test_fallback value\n",
            "def use_outer: <T where TestOuter T> T -> I32 = value => outer_test value\n",
            "let shown: Bool = show_bound 1\n",
            "let fell: Bool = fallback_bound 1\n",
            "let out: I32 = use_outer 1\n",
        ));
        for (name, trait_name) in [
            ("show_bound", "TestShow"),
            ("fallback_bound", "TestDefaulted"),
            ("use_outer", "TestOuter"),
        ] {
            let owner = function_id(&program, name);
            let environment = enclosing_environment(&program, name);
            let evidence = declared_bound_evidence_in(&program, trait_name, owner);
            let resolved = resolve_evidence(&program, &evidence, &environment);
            let TraitEvidence::ExplicitImplementation {
                trait_id,
                method,
                arguments,
                function,
                ..
            } = &resolved
            else {
                panic!("expected an explicit selection for {name}");
            };
            let checked_decision = module
                .trait_impl_method(*trait_id, arguments, *method)
                .expect("the emitter selector agrees an implementation exists");
            assert_eq!(
                *function, checked_decision,
                "the resolver agrees with the emitter selector for {name}"
            );
        }
    }

    #[test]
    fn resolved_keys_feed_the_append_only_catalog() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "let second: I32 = identity 1\n",
        ));
        let mut catalog = crate::specialization::SpecializationCatalog::default();
        let mut ordinals = Vec::new();
        for (function, call) in direct_calls(&program, "identity") {
            let resolved = program
                .resolve_instance_request(&InstanceResolutionRequest {
                    function,
                    origin: call.origin.clone(),
                    function_type: call.function_type.clone(),
                    substitutions: call.substitutions.clone(),
                    evidence: call.evidence.clone(),
                    target: InstanceResolutionTarget::Root,
                })
                .expect("the request resolves");
            ordinals.push(catalog.reserve_instance(resolved.key));
        }
        assert_eq!(
            ordinals[0], ordinals[1],
            "equivalent resolved requests reserve one instance"
        );
        assert_eq!(catalog.instances().count(), 1);
    }

    #[test]
    fn repeated_product_counts_and_curried_layers_resolve() {
        let (_, program) = lower(concat!(
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T; N) = value => n => (value; N)\n",
            "let repeated: (I32; 3) = repeat 7 3\n",
        ));
        let (environment, relevant) = resolve_named_call(&program, "repeat");
        let mut parameters = relevant.type_parameters();
        let first = parameters.next().expect("T");
        let second = parameters.next().expect("N");
        assert!(parameters.next().is_none());
        match environment.type_value(first) {
            Some(value) => assert_eq!(value, &CheckedType::I32),
            None => panic!("T should resolve"),
        }
        match environment.type_value(second) {
            Some(CheckedType::NumberLiteral(3)) | Some(CheckedType::USize) => {}
            other => panic!("N should resolve to the repeated count, got {other:?}"),
        }
    }
}
