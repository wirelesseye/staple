//! Trait dictionary prefix layout (T-D).
//!
//! Every prefix field is a pointer. Offsets are measured in pointer slots,
//! leaving target pointer width and alignment to codegen. The implementation's
//! descriptor and sub-dictionary environment starts after the fixed prefix.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraitDictionaryLayout {
    /// Trait members, including defaults, in declaration order.
    pub slots: Range<usize>,
    /// Prerequisite dictionaries in declaration order.
    pub prerequisites: Range<usize>,
    /// Associated-type descriptors in declaration order.
    pub associated: Range<usize>,
    /// First pointer slot of the implementation-specific tail.
    pub environment: usize,
}

impl TraitDictionaryLayout {
    pub(crate) fn new(methods: usize, prerequisites: usize, associated: usize) -> Self {
        let associated_start = methods + prerequisites;
        let environment = associated_start + associated;
        Self {
            slots: 0..methods,
            prerequisites: methods..associated_start,
            associated: associated_start..environment,
            environment,
        }
    }
}

/// Runtime dictionary obligations (T-B), in checked bound order.
/// Marker traits are supplied by descriptors or remain compile-time facts.
/// Checked bounds include prerequisites; this list is for recipe planning,
/// not the hidden ABI count, which uses the declared bounds (T-C).
pub(super) fn runtime_obligations(
    ids: &super::LoweredSemanticIds,
    bounds: &[crate::CheckedTraitBound],
    substitution: &std::collections::HashMap<crate::TypeParameterId, crate::CheckedType>,
    runtime: &std::collections::BTreeSet<crate::TypeParameterId>,
) -> Vec<crate::CheckedTraitBound> {
    bounds
        .iter()
        .filter_map(|bound| {
            if [
                ids.copy_trait,
                ids.sized_trait,
                ids.drop_trait,
                ids.natural_trait,
            ]
            .contains(&Some(bound.trait_id))
            {
                return None;
            }
            let arguments = bound
                .arguments
                .iter()
                .map(|argument| crate::substitute_type(argument.clone(), substitution))
                .collect::<Vec<_>>();
            let mut referenced = std::collections::BTreeSet::new();
            for argument in &arguments {
                super::instance_resolution::collect_referenced_parameters(
                    argument,
                    &mut referenced,
                );
            }
            (!referenced.is_disjoint(runtime)).then_some(crate::CheckedTraitBound {
                trait_id: bound.trait_id,
                arguments,
            })
        })
        .collect()
}

/// A route from a hidden argument through zero or more fixed prefix slots.
/// This contains no implementation choice; an equal environment bound wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvironmentDictionaryRecipe {
    pub argument: usize,
    /// Absolute pointer-slot offsets in each successive dictionary prefix.
    pub prerequisites: Vec<usize>,
}

/// Search direct arguments before any prerequisite path. Visit each equal
/// obligation once per root so cyclic prerequisite graphs terminate.
pub(super) fn environment_recipe(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    obligation: &crate::CheckedTraitBound,
) -> Option<EnvironmentDictionaryRecipe> {
    if let Some(argument) = environment.iter().position(|bound| bound == obligation) {
        return Some(EnvironmentDictionaryRecipe {
            argument,
            prerequisites: Vec::new(),
        });
    }
    let mut queue = std::collections::VecDeque::new();
    let mut visited = environment
        .iter()
        .map(|bound| vec![bound.clone()])
        .collect::<Vec<_>>();
    for (argument, bound) in environment.iter().enumerate() {
        queue.push_back((
            bound.clone(),
            EnvironmentDictionaryRecipe {
                argument,
                prerequisites: Vec::new(),
            },
        ));
    }
    while let Some((bound, recipe)) = queue.pop_front() {
        let metadata = program.traits.get(bound.trait_id)?;
        let substitution = dictionary_substitution(metadata, &bound.arguments)?;
        for (index, prerequisite) in metadata.prerequisites.iter().enumerate() {
            let next = crate::CheckedTraitBound {
                trait_id: prerequisite.trait_id,
                arguments: prerequisite
                    .arguments
                    .iter()
                    .map(|argument| crate::substitute_type(argument.clone(), &substitution))
                    .collect(),
            };
            if visited[recipe.argument].contains(&next) {
                continue;
            }
            let mut route = recipe.clone();
            route
                .prerequisites
                .push(metadata.dictionary_layout.prerequisites.start + index);
            if &next == obligation {
                return Some(route);
            }
            visited[recipe.argument].push(next.clone());
            queue.push_back((next, route));
        }
    }
    None
}

fn dictionary_substitution(
    metadata: &super::LoweredTraitMetadata,
    arguments: &[crate::CheckedType],
) -> Option<std::collections::HashMap<crate::TypeParameterId, crate::CheckedType>> {
    if metadata.parameters.len() != arguments.len() {
        return None;
    }
    Some(
        metadata
            .parameters
            .iter()
            .zip(arguments)
            .filter_map(|(parameter, argument)| {
                let crate::CheckedType::Parameter { id, .. } = parameter else {
                    return None;
                };
                Some((*id, argument.clone()))
            })
            .collect(),
    )
}

/// Recompute the obligation carried by a recorded route, checking each slot
/// against the owning trait's prefix. Consumers never infer an implementation.
pub(super) fn environment_recipe_bound(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    recipe: &EnvironmentDictionaryRecipe,
) -> Option<crate::CheckedTraitBound> {
    let mut bound = environment.get(recipe.argument)?.clone();
    for slot in &recipe.prerequisites {
        let metadata = program.traits.get(bound.trait_id)?;
        if !metadata.dictionary_layout.prerequisites.contains(slot) {
            return None;
        }
        let prerequisite = metadata
            .prerequisites
            .get(slot - metadata.dictionary_layout.prerequisites.start)?;
        let substitution = dictionary_substitution(metadata, &bound.arguments)?;
        bound = crate::CheckedTraitBound {
            trait_id: prerequisite.trait_id,
            arguments: prerequisite
                .arguments
                .iter()
                .map(|argument| crate::substitute_type(argument.clone(), &substitution))
                .collect(),
        };
    }
    Some(bound)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DictionaryRecipe {
    Environment(EnvironmentDictionaryRecipe),
    Static(crate::specialization::ArtifactRequestKey),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictionaryCallArgument {
    pub obligation: crate::CheckedTraitBound,
    pub recipe: DictionaryRecipe,
}

/// A trait-member call through a dictionary at a run-time obligation (T-K).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictionarySlotCall {
    pub obligation: crate::CheckedTraitBound,
    pub recipe: DictionaryRecipe,
    /// Absolute pointer-slot offset of the member in the dictionary prefix.
    pub slot: usize,
}

/// The trait, completed arguments and member a recorded evidence names.
pub(super) fn evidence_obligation(
    evidence: &crate::TraitEvidence,
) -> (
    crate::TraitId,
    &[crate::CheckedType],
    Option<crate::TraitMethodId>,
) {
    match evidence {
        crate::TraitEvidence::ExplicitImplementation {
            trait_id,
            arguments,
            method,
            ..
        } => (*trait_id, arguments, Some(*method)),
        crate::TraitEvidence::Structural {
            trait_id,
            arguments,
            method,
            ..
        } => (*trait_id, arguments, Some(*method)),
        crate::TraitEvidence::DeclaredBound {
            trait_id,
            arguments,
            method,
            ..
        } => (*trait_id, arguments, *method),
    }
}

/// The obligation of a trait site when, after the site's substitution, it
/// mentions a run-time parameter of the enclosing instance (T-B). Other sites
/// keep static dispatch.
pub(super) fn site_runtime_obligation(
    ids: &super::LoweredSemanticIds,
    evidence: &crate::TraitEvidence,
    environment: &super::instance_resolution::SubstitutionEnvironment,
    runtime: &std::collections::BTreeSet<crate::TypeParameterId>,
) -> Option<crate::CheckedTraitBound> {
    let (trait_id, arguments, _) = evidence_obligation(evidence);
    let map = environment.substitution_map();
    let bound = crate::CheckedTraitBound {
        trait_id,
        arguments: arguments
            .iter()
            .map(|argument| crate::substitute_type(argument.clone(), &map))
            .collect(),
    };
    runtime_obligations(ids, std::slice::from_ref(&bound), &Default::default(), runtime)
        .into_iter()
        .next()
}

/// Select the dictionary and slot a run-time trait member call reads. An
/// environment bound wins (T-F); other recipe kinds arrive with builders.
pub(super) fn plan_slot_call(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    obligation: &crate::CheckedTraitBound,
    method: crate::TraitMethodId,
    origin: &crate::Origin,
) -> Result<DictionarySlotCall, staple_syntax::Diagnostic> {
    let metadata = program.traits.get(obligation.trait_id).ok_or_else(|| {
        staple_syntax::Diagnostic::new(origin.span.clone(), "dictionary trait is absent")
    })?;
    let slot = metadata
        .methods
        .iter()
        .position(|candidate| *candidate == method)
        .ok_or_else(|| {
            staple_syntax::Diagnostic::new(
                origin.span.clone(),
                "dictionary trait has no such member",
            )
        })?;
    let recipe = environment_recipe(program, environment, obligation).ok_or_else(|| {
        staple_syntax::Diagnostic::new(
            origin.span.clone(),
            format!(
                "trait `{}` is required at a run-time type and no dictionary provides it",
                metadata.name
            ),
        )
    })?;
    Ok(DictionarySlotCall {
        obligation: obligation.clone(),
        recipe: DictionaryRecipe::Environment(recipe),
        slot: metadata.dictionary_layout.slots.start + slot,
    })
}

/// Translate a dynamic callee's declared bounds into the caller's types.
pub(super) fn plan_direct_call(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    call: &super::LoweredCall,
    callee: super::FunctionInstanceId,
) -> Result<Vec<DictionaryCallArgument>, staple_syntax::Diagnostic> {
    plan_instance_dictionaries(
        program,
        environment,
        &call.origin,
        &call.substitutions,
        callee,
        None,
    )
}

/// Dictionaries a callable value stores in its closure environment: the
/// callee's lexical prefix, which its descriptor-bound entry forwards. A
/// scheme value's own dictionaries arrive from its caller (T-C).
pub(super) fn plan_callable_value(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    value: &super::LoweredCallableValue,
    callee: super::FunctionInstanceId,
) -> Result<Vec<DictionaryCallArgument>, staple_syntax::Diagnostic> {
    let stored = stored_dictionary_count(program, callee).unwrap_or(0);
    plan_instance_dictionaries(
        program,
        environment,
        &value.origin,
        &value.substitutions,
        callee,
        Some(stored),
    )
}

/// Dictionaries a dynamic closure environment stores for one instance. They
/// follow the descriptor region and precede capture descriptors.
pub(super) fn stored_dictionary_count(
    program: &super::LoweredProgram,
    instance: super::FunctionInstanceId,
) -> Option<usize> {
    let record = program.instances.get(instance)?;
    let template = program.functions.get(record.template)?;
    let key = program.specializations.instance(record.ordinal)?;
    let substitution = record.environment.substitution_map();
    let count = |bounds: &[crate::CheckedTraitBound]| {
        runtime_obligations(&program.semantic_ids, bounds, &substitution, key.runtime()).len()
    };
    count(&template.declared_bounds).checked_sub(count(&template.own_declared_bounds))
}

fn plan_instance_dictionaries(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    origin: &crate::Origin,
    substitutions: &crate::CallSubstitutions,
    callee: super::FunctionInstanceId,
    limit: Option<usize>,
) -> Result<Vec<DictionaryCallArgument>, staple_syntax::Diagnostic> {
    let record = program.instances.get(callee).ok_or_else(|| {
        staple_syntax::Diagnostic::new(
            origin.span.clone(),
            "dictionary call has no callee instance",
        )
    })?;
    let template = program.functions.get(record.template).ok_or_else(|| {
        staple_syntax::Diagnostic::new(
            origin.span.clone(),
            "dictionary call has no callee template",
        )
    })?;
    let key = program
        .specializations
        .instance(record.ordinal)
        .ok_or_else(|| {
            staple_syntax::Diagnostic::new(origin.span.clone(), "dictionary call has no callee key")
        })?;
    let bounds = runtime_obligations(
        &program.semantic_ids,
        &template.declared_bounds,
        &record.environment.substitution_map(),
        key.runtime(),
    );
    let substitution = substitutions
        .types
        .iter()
        .map(|entry| (entry.parameter, entry.value_type.clone()))
        .collect();
    bounds
        .into_iter()
        .take(limit.unwrap_or(usize::MAX))
        .map(|bound| {
            let obligation = crate::CheckedTraitBound {
                trait_id: bound.trait_id,
                arguments: bound
                    .arguments
                    .into_iter()
                    .map(|argument| crate::substitute_type(argument, &substitution))
                    .collect(),
            };
            let recipe = if let Some(recipe) = environment_recipe(program, environment, &obligation)
            {
                DictionaryRecipe::Environment(recipe)
            } else {
                let super::artifact_closure::ClosureRequest::Artifact { key, .. } =
                    static_request(program, &obligation, origin)?
                else {
                    unreachable!()
                };
                DictionaryRecipe::Static(key)
            };
            Ok(DictionaryCallArgument { obligation, recipe })
        })
        .collect()
}

pub(super) fn scan_instance(
    program: &super::LoweredProgram,
    instance: super::FunctionInstanceId,
) -> super::artifact_closure::ScanResult {
    let Some(body) = program
        .instances
        .get(instance)
        .and_then(|record| record.body.as_ref())
    else {
        return Ok(Vec::new());
    };
    let mut requests = Vec::new();
    for (call, arguments) in &body.dictionary_calls {
        let origin = &body
            .calls
            .get(*call)
            .ok_or_else(|| {
                vec![staple_syntax::Diagnostic::new(
                    body.origin.span.clone(),
                    "dictionary recipe names a missing call",
                )]
            })?
            .origin;
        for (index, argument) in arguments.iter().enumerate() {
            if let DictionaryRecipe::Static(key) = &argument.recipe {
                let mut request = static_request(program, &argument.obligation, origin)
                    .map_err(|error| vec![error])?;
                let super::artifact_closure::ClosureRequest::Artifact {
                    key: selected,
                    use_site,
                    ..
                } = &mut request
                else {
                    unreachable!()
                };
                if selected != key {
                    return Err(vec![staple_syntax::Diagnostic::new(
                        origin.span.clone(),
                        "dictionary recipe selection changed",
                    )]);
                }
                *use_site = Some(super::artifact_closure::ArtifactUseSite::CallDictionary {
                    call: *call,
                    argument: index,
                });
                requests.push(request);
            }
        }
    }
    for (value, arguments) in &body.dictionary_values {
        let origin = &body
            .callable_values
            .get(*value)
            .ok_or_else(|| {
                vec![staple_syntax::Diagnostic::new(
                    body.origin.span.clone(),
                    "dictionary recipe names a missing callable value",
                )]
            })?
            .origin;
        for (index, argument) in arguments.iter().enumerate() {
            if let DictionaryRecipe::Static(key) = &argument.recipe {
                let mut request = static_request(program, &argument.obligation, origin)
                    .map_err(|error| vec![error])?;
                let super::artifact_closure::ClosureRequest::Artifact {
                    key: selected,
                    use_site,
                    ..
                } = &mut request
                else {
                    unreachable!()
                };
                if selected != key {
                    return Err(vec![staple_syntax::Diagnostic::new(
                        origin.span.clone(),
                        "dictionary recipe selection changed",
                    )]);
                }
                *use_site = Some(super::artifact_closure::ArtifactUseSite::ValueDictionary {
                    value: *value,
                    argument: index,
                });
                requests.push(request);
            }
        }
    }
    Ok(requests)
}

pub(super) fn initializer_call_arguments(
    program: &super::LoweredProgram,
    initializer: super::InitializerId,
) -> Result<
    std::collections::BTreeMap<super::LoweredCallId, Vec<DictionaryCallArgument>>,
    Vec<staple_syntax::Diagnostic>,
> {
    struct Visitor<'a> {
        program: &'a super::LoweredProgram,
        initializer: super::InitializerId,
        arguments: std::collections::BTreeMap<super::LoweredCallId, Vec<DictionaryCallArgument>>,
    }
    impl super::cleanup_artifacts::LoweredOwnerVisitor for Visitor<'_> {
        fn call_id_site(
            &mut self,
            id: super::LoweredCallId,
            call: &super::LoweredCall,
        ) -> Result<(), Vec<staple_syntax::Diagnostic>> {
            if let Some(callee) = super::initializer_bindings::direct_call_target(
                self.program,
                self.initializer,
                id,
                call,
            )? {
                let arguments = plan_direct_call(self.program, &[], call, callee)
                    .map_err(|error| vec![error])?;
                if !arguments.is_empty() {
                    self.arguments.insert(id, arguments);
                }
            }
            Ok(())
        }
    }
    let mut visitor = Visitor {
        program,
        initializer,
        arguments: Default::default(),
    };
    super::cleanup_artifacts::walk_owner(
        program,
        super::emission::OwnerArenas::Initializer(initializer),
        &mut visitor,
    )?;
    Ok(visitor.arguments)
}

pub(super) fn scan_initializer(
    program: &super::LoweredProgram,
    initializer: super::InitializerId,
) -> super::artifact_closure::ScanResult {
    let arguments = initializer_call_arguments(program, initializer)?;
    let mut requests = Vec::new();
    for (call, arguments) in arguments {
        let origin = &program
            .calls
            .get(call)
            .expect("initializer dictionary call")
            .origin;
        for (index, argument) in arguments.iter().enumerate() {
            let DictionaryRecipe::Static(key) = &argument.recipe else {
                unreachable!("initializers have no hidden dictionary environment")
            };
            let mut request = static_request(program, &argument.obligation, origin)
                .map_err(|error| vec![error])?;
            let super::artifact_closure::ClosureRequest::Artifact {
                key: selected,
                use_site,
                ..
            } = &mut request
            else {
                unreachable!()
            };
            if selected != key {
                return Err(vec![staple_syntax::Diagnostic::new(
                    origin.span.clone(),
                    "initializer dictionary recipe selection changed",
                )]);
            }
            *use_site = Some(super::artifact_closure::ArtifactUseSite::CallDictionary {
                call,
                argument: index,
            });
            requests.push(request);
        }
    }
    Ok(requests)
}

/// Dictionary ABI partition of an entry: bound lexical arguments precede
/// own scheme arguments. A monomorphic entry binds all descriptors/dictionaries.
pub(super) fn entry_dictionary_partition(
    program: &super::LoweredProgram,
    instance: super::FunctionInstanceId,
    bound_descriptors: usize,
) -> Option<(usize, usize)> {
    let record = program.instances.get(instance)?;
    let template = program.functions.get(record.template)?;
    let key = program.specializations.instance(record.ordinal)?;
    let substitution = record.environment.substitution_map();
    let total = runtime_obligations(
        &program.semantic_ids,
        &template.declared_bounds,
        &substitution,
        key.runtime(),
    )
    .len();
    let bound = if bound_descriptors == key.runtime().len() {
        total
    } else {
        let own = runtime_obligations(
            &program.semantic_ids,
            &template.own_declared_bounds,
            &substitution,
            key.runtime(),
        )
        .len();
        total.checked_sub(own)?
    };
    Some((total, bound))
}

/// A concrete obligation creates a dictionary only at a demand site.
pub(super) fn static_request(
    program: &super::LoweredProgram,
    bound: &crate::CheckedTraitBound,
    origin: &crate::Origin,
) -> Result<super::artifact_closure::ClosureRequest, staple_syntax::Diagnostic> {
    use super::artifact_closure::ClosureRequest;
    use super::artifact_plan::{LoweredArtifactPlan, TraitDictionaryPlan};
    use crate::specialization::{ArtifactRequestKey, CanonicalType, TraitDictionaryKey};
    let bound = program.complete_declared_bound(origin, bound, &Default::default())?;
    let implementation = program.concrete_dictionary_implementation(&bound, origin)?;
    let metadata = program.traits.get(bound.trait_id).ok_or_else(|| {
        staple_syntax::Diagnostic::new(origin.span.clone(), "dictionary trait is absent")
    })?;
    let arguments = bound
        .arguments
        .iter()
        .map(|argument| CanonicalType::concrete(argument, origin))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ClosureRequest::Artifact {
        key: ArtifactRequestKey::TraitDictionary(TraitDictionaryKey {
            trait_id: bound.trait_id,
            arguments,
            implementation,
        }),
        plan: LoweredArtifactPlan::TraitDictionary(TraitDictionaryPlan {
            trait_id: bound.trait_id,
            arguments: bound.arguments,
            implementation,
            layout: metadata.dictionary_layout.clone(),
            slots: Vec::new(),
            prerequisites: Vec::new(),
            associated: Vec::new(),
            methods: Vec::new(),
            expanded: false,
        }),
        kind: super::LoweredArtifactDependencyKind::TraitDictionary,
        origin: origin.clone(),
        use_site: None,
    })
}

pub(super) fn expand(
    program: &super::LoweredProgram,
    artifact: super::LoweredArtifactRequestId,
    plan: super::LoweredArtifactPlan,
) -> super::artifact_closure::ExpansionResult {
    use super::artifact_closure::ClosureRequest;
    use super::artifact_plan::{PlannedArtifact, PlannedInstance, ReabstractionThunkPlan};
    use super::instance_resolution::{InstanceResolutionRequest, InstanceResolutionTarget};
    use super::{
        LoweredArtifactDependencyKind as Kind, LoweredArtifactPlan as Plan,
        LoweredInstanceDependencyKind as InstanceKind,
    };
    use crate::specialization::{
        ArtifactRequestKey, CanonicalFunctionType, ReabstractionDirection, ReabstractionThunkKey,
        StaticDictionarySlot,
    };
    use crate::{CheckedTraitBound, CheckedType, TraitEvidence};
    use staple_syntax::Diagnostic;
    let record = program
        .artifacts
        .get(artifact)
        .expect("interned dictionary");
    let origin = &record.origin;
    let Plan::TraitDictionary(mut plan) = plan else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            "dictionary has a mismatched plan",
        )]);
    };
    let metadata = program.traits.get(plan.trait_id).ok_or_else(|| {
        vec![Diagnostic::new(
            origin.span.clone(),
            "dictionary trait is absent",
        )]
    })?;
    if plan.layout != metadata.dictionary_layout
        || plan.arguments.len() != metadata.parameters.len()
    {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            "dictionary prefix disagrees with its trait",
        )]);
    }
    plan.slots.clear();
    plan.methods.clear();
    plan.prerequisites.clear();
    plan.associated.clear();
    let mut requests = Vec::new();
    let mut method_requests = Vec::new();
    for (index, method) in metadata.methods.iter().enumerate() {
        let evidence = program
            .resolve_trait_evidence(
                origin,
                Some(&TraitEvidence::DeclaredBound {
                    trait_id: plan.trait_id,
                    method: Some(*method),
                    arguments: plan.arguments.clone(),
                    prerequisites: Vec::new(),
                }),
                &Default::default(),
            )
            .map_err(|error| vec![error])?
            .ok_or_else(|| {
                vec![Diagnostic::new(
                    origin.span.clone(),
                    "dictionary method has no evidence",
                )]
            })?;
        let TraitEvidence::ExplicitImplementation {
            implementation,
            function,
            ..
        } = &evidence
        else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                "explicit dictionary selected a structural method",
            )]);
        };
        if *implementation != plan.implementation {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                "dictionary methods select different implementations",
            )]);
        }
        let function_type = super::worklist::instantiate_method_type(
            program,
            origin,
            plan.trait_id,
            *method,
            &plan.arguments,
        )
        .map_err(|error| vec![error])?;
        let resolved = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: *function,
                origin: origin.clone(),
                function_type: function_type.clone(),
                substitutions: super::worklist::trait_site_substitutions(
                    program,
                    plan.trait_id,
                    &plan.arguments,
                ),
                evidence: Some(evidence),
                target: InstanceResolutionTarget::Root,
            })
            .map_err(|error| vec![error])?;
        plan.methods.push(PlannedInstance {
            key: resolved.key.clone(),
            instance: None,
            kind: InstanceKind::TraitMethod,
        });
        method_requests.push(ClosureRequest::Instance {
            resolved,
            kind: InstanceKind::TraitMethod,
            origin: origin.clone(),
            use_site: None,
        });
        let dictionary_slot = Some(StaticDictionarySlot {
            implementation: plan.implementation,
            method: *method,
            closure_offset: plan.layout.environment + 2 * index,
        });
        let key = ArtifactRequestKey::ReabstractionThunk(ReabstractionThunkKey {
            dictionary_slot: dictionary_slot.clone(),
            concrete: CanonicalFunctionType::concrete(&function_type, origin)
                .map_err(|error| vec![error])?,
            dynamic: CanonicalFunctionType::dynamic(&function_type, origin)
                .map_err(|error| vec![error])?,
            direction: ReabstractionDirection::ConcreteToDynamic,
        });
        plan.slots.push(PlannedArtifact {
            key: key.clone(),
            artifact: None,
            kind: Kind::ReabstractionThunk,
        });
        requests.push(ClosureRequest::Artifact {
            key,
            plan: Plan::ReabstractionThunk(ReabstractionThunkPlan {
                dictionary_slot,
                concrete: function_type.clone(),
                dynamic: function_type,
                direction: ReabstractionDirection::ConcreteToDynamic,
                children: Vec::new(),
            }),
            kind: Kind::ReabstractionThunk,
            origin: origin.clone(),
            use_site: None,
        });
    }
    let substitution = metadata
        .parameters
        .iter()
        .zip(&plan.arguments)
        .filter_map(|(parameter, argument)| {
            let CheckedType::Parameter { id, .. } = parameter else {
                return None;
            };
            Some((*id, argument.clone()))
        })
        .collect();
    for prerequisite in &metadata.prerequisites {
        let ids = &program.semantic_ids;
        if [
            ids.copy_trait,
            ids.sized_trait,
            ids.drop_trait,
            ids.natural_trait,
        ]
        .contains(&Some(prerequisite.trait_id))
        {
            plan.prerequisites.push(None);
            continue;
        }
        let bound = CheckedTraitBound {
            trait_id: prerequisite.trait_id,
            arguments: prerequisite
                .arguments
                .iter()
                .map(|argument| crate::substitute_type(argument.clone(), &substitution))
                .collect(),
        };
        let request = static_request(program, &bound, origin).map_err(|error| vec![error])?;
        let ClosureRequest::Artifact { key, kind, .. } = &request else {
            unreachable!()
        };
        plan.prerequisites.push(Some(PlannedArtifact {
            key: key.clone(),
            artifact: None,
            kind: *kind,
        }));
        requests.push(request);
    }
    for argument in &plan.arguments[plan.arguments.len() - metadata.associated_types.len()..] {
        let request = super::descriptor_artifacts::descriptor_request(argument, origin)
            .map_err(|error| vec![error])?;
        let ClosureRequest::Artifact { key, kind, .. } = &request else {
            unreachable!()
        };
        plan.associated.push(PlannedArtifact {
            key: key.clone(),
            artifact: None,
            kind: *kind,
        });
        requests.push(request);
    }
    requests.extend(method_requests);
    plan.expanded = true;
    Ok((Plan::TraitDictionary(plan), requests))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_obligations_follow_substitution_and_preserve_checked_order() {
        use crate::{CheckedTraitBound, CheckedType, TraitId, TypeParameterId};
        use std::collections::{BTreeSet, HashMap};
        let parameter = |id| CheckedType::Parameter {
            id: TypeParameterId(id),
            name: format!("P{id}"),
            sized: true,
            parameter_product_capable: false,
        };
        let bound = |id, argument| CheckedTraitBound {
            trait_id: TraitId(id),
            arguments: vec![argument],
        };
        let ids = super::super::LoweredSemanticIds {
            copy_trait: Some(TraitId(1)),
            sized_trait: Some(TraitId(2)),
            drop_trait: Some(TraitId(3)),
            natural_trait: Some(TraitId(4)),
            ..Default::default()
        };
        let bounds = vec![
            bound(9, CheckedType::Ref(Box::new(parameter(0)))),
            bound(8, parameter(1)),
            bound(7, parameter(2)),
            bound(1, parameter(0)),
            bound(2, parameter(0)),
            bound(3, parameter(0)),
            bound(4, parameter(0)),
        ];
        // P0 becomes a different runtime parameter; P1 becomes concrete;
        // P2 remains irrelevant to this instance. Include a projection-like
        // high runtime ID: dictionary derivation makes no declared-ID assumption.
        let projected = TypeParameterId(1_000_000);
        let substitution = HashMap::from([
            (TypeParameterId(0), parameter(projected.0)),
            (TypeParameterId(1), CheckedType::I32),
        ]);
        let runtime = BTreeSet::from([projected]);
        assert_eq!(
            runtime_obligations(&ids, &bounds, &substitution, &runtime),
            vec![bound(9, CheckedType::Ref(Box::new(parameter(projected.0))))]
        );
        assert!(runtime_obligations(&ids, &bounds, &substitution, &BTreeSet::new()).is_empty());
        let runtime = BTreeSet::from([projected, TypeParameterId(2)]);
        let selected = runtime_obligations(&ids, &bounds, &substitution, &runtime);
        assert_eq!(
            selected
                .iter()
                .map(|bound| bound.trait_id)
                .collect::<Vec<_>>(),
            vec![TraitId(9), TraitId(7)]
        );
    }

    #[test]
    fn empty_and_multi_member_prefixes_are_contiguous() {
        let empty = TraitDictionaryLayout::new(0, 0, 0);
        assert_eq!(empty.environment, 0);
        let layout = TraitDictionaryLayout::new(3, 2, 2);
        assert_eq!(layout.slots, 0..3);
        assert_eq!(layout.prerequisites, 3..5);
        assert_eq!(layout.associated, 5..7);
        assert_eq!(layout.environment, 7);
    }
}
