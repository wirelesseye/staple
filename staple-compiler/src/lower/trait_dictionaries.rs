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
    /// A generic implementation selected at abstract types, applied to the
    /// caller's descriptors and recipes (T-E). The dictionary is built in GC
    /// memory so it may outlive the frame (T-G).
    Builder(Box<BuilderRecipe>),
}

/// The run-time construction of a dictionary from a generic implementation.
/// Its tail holds, per trait member, the descriptors and dictionaries the
/// member's dynamic instance receives, so the member's slot entry forwards
/// them like a closure environment does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuilderRecipe {
    pub implementation: super::LoweredTraitImplementationId,
    /// The obligation at the caller's types, completed by the header.
    pub obligation: crate::CheckedTraitBound,
    pub members: Vec<BuilderMember>,
    /// One recipe per trait prerequisite, in prefix order; markers have none.
    pub prerequisites: Vec<Option<DictionaryCallArgument>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuilderMember {
    pub instance: super::FunctionInstanceId,
    /// First pointer slot of this member's region in the dictionary tail.
    pub region: usize,
    /// The caller's type of each descriptor the instance takes, in its
    /// descriptor ABI order (R-A).
    pub descriptors: Vec<crate::CheckedType>,
    pub dictionaries: Vec<MemberDictionary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberDictionary {
    /// The dictionary being built (a defaulted member's own bound).
    Itself,
    Recipe(DictionaryCallArgument),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictionaryCallArgument {
    pub obligation: crate::CheckedTraitBound,
    pub recipe: DictionaryRecipe,
}

/// A dictionary choice at abstract types (T-F), made before any member
/// instance exists. The worklist requests its members; bodies pair it with
/// the interned instances to form a recipe.
#[derive(Debug, Clone)]
pub(super) enum DictionarySelection {
    Environment(EnvironmentDictionaryRecipe),
    Static(crate::specialization::ArtifactRequestKey),
    Build(Box<BuildSelection>),
}

#[derive(Debug, Clone)]
pub(super) struct BuildSelection {
    pub implementation: super::LoweredTraitImplementationId,
    pub obligation: crate::CheckedTraitBound,
    /// Implementation and trait parameters at the caller's types.
    pub substitution: std::collections::HashMap<crate::TypeParameterId, crate::CheckedType>,
    pub members: Vec<MemberSelection>,
    pub prerequisites: Vec<Option<(crate::CheckedTraitBound, DictionarySelection)>>,
}

#[derive(Debug, Clone)]
pub(super) struct MemberSelection {
    pub function: crate::FunctionId,
    pub function_type: crate::CheckedFunctionType,
    pub evidence: crate::TraitEvidence,
    pub substitutions: crate::CallSubstitutions,
    /// The member instance's dictionaries at the caller's types, in its
    /// declared-bound order.
    pub dictionaries: Vec<MemberDictionarySelection>,
}

#[derive(Debug, Clone)]
pub(super) enum MemberDictionarySelection {
    Itself,
    Select(crate::CheckedTraitBound, DictionarySelection),
}

const SELECTION_DEPTH: usize = 8;

/// Choose how a run-time obligation's dictionary is obtained. An equal
/// environment bound wins; otherwise exactly one implementation is selected
/// and its `where` clause and prerequisites must in turn be obtainable.
pub(super) fn select_dictionary(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    obligation: &crate::CheckedTraitBound,
    origin: &crate::Origin,
) -> Result<DictionarySelection, staple_syntax::Diagnostic> {
    select_at_depth(program, environment, obligation, origin, 0)
}

fn select_at_depth(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    obligation: &crate::CheckedTraitBound,
    origin: &crate::Origin,
    depth: usize,
) -> Result<DictionarySelection, staple_syntax::Diagnostic> {
    use crate::contains_type_parameter;
    if depth > SELECTION_DEPTH {
        return Err(staple_syntax::Diagnostic::new(
            origin.span.clone(),
            "dictionary selection recurses too deeply",
        ));
    }
    if !obligation.arguments.iter().any(contains_type_parameter) {
        let super::artifact_closure::ClosureRequest::Artifact { key, .. } =
            static_request(program, obligation, origin)?
        else {
            unreachable!()
        };
        return Ok(DictionarySelection::Static(key));
    }
    if let Some(recipe) = environment_recipe(program, environment, obligation) {
        return Ok(DictionarySelection::Environment(recipe));
    }
    let chosen = program.select_runtime_implementation(obligation, origin)?;
    let metadata = program.traits.get(obligation.trait_id).ok_or_else(|| {
        staple_syntax::Diagnostic::new(origin.span.clone(), "dictionary trait is absent")
    })?;
    let markers = marker_traits(program);
    // The `where` clause must be obtainable from the caller's dictionaries.
    for clause in &chosen.clauses {
        if markers.contains(&Some(clause.trait_id)) {
            continue;
        }
        select_at_depth(program, environment, clause, origin, depth + 1)?;
    }
    let built = crate::CheckedTraitBound {
        trait_id: obligation.trait_id,
        arguments: chosen.completed.clone(),
    };
    let mut substitution = chosen.substitution.clone();
    substitution.extend(dictionary_substitution(metadata, &chosen.completed).unwrap_or_default());
    let mut members = Vec::new();
    for method in &metadata.methods {
        let function = program
            .implementation_method_function(chosen.implementation, obligation.trait_id, *method)
            .ok_or_else(|| {
                staple_syntax::Diagnostic::new(
                    origin.span.clone(),
                    "dictionary member has no implementation",
                )
            })?;
        let function_type = super::worklist::instantiate_method_type(
            program,
            origin,
            obligation.trait_id,
            *method,
            &chosen.completed,
        )?;
        let template = program.functions.get(function).ok_or_else(|| {
            staple_syntax::Diagnostic::new(origin.span.clone(), "dictionary member has no template")
        })?;
        let mut dictionaries = Vec::new();
        for bound in &template.declared_bounds {
            if markers.contains(&Some(bound.trait_id)) {
                continue;
            }
            let at_caller = crate::CheckedTraitBound {
                trait_id: bound.trait_id,
                arguments: bound
                    .arguments
                    .iter()
                    .map(|argument| crate::substitute_type(argument.clone(), &substitution))
                    .collect(),
            };
            // Only bounds that mention run-time types become dictionaries.
            if !at_caller.arguments.iter().any(contains_type_parameter) {
                continue;
            }
            dictionaries.push(if at_caller == built {
                MemberDictionarySelection::Itself
            } else {
                let selection = select_at_depth(program, environment, &at_caller, origin, depth + 1)?;
                MemberDictionarySelection::Select(at_caller, selection)
            });
        }
        members.push(MemberSelection {
            function,
            function_type,
            evidence: crate::TraitEvidence::ExplicitImplementation {
                trait_id: obligation.trait_id,
                implementation: chosen.implementation,
                method: *method,
                function,
                arguments: chosen.completed.clone(),
            },
            substitutions: super::worklist::trait_site_substitutions(
                program,
                obligation.trait_id,
                &chosen.completed,
            ),
            dictionaries,
        });
    }
    let mut prerequisites = Vec::new();
    let trait_substitution = dictionary_substitution(metadata, &chosen.completed).unwrap_or_default();
    for prerequisite in &metadata.prerequisites {
        if markers.contains(&Some(prerequisite.trait_id)) {
            prerequisites.push(None);
            continue;
        }
        let bound = crate::CheckedTraitBound {
            trait_id: prerequisite.trait_id,
            arguments: prerequisite
                .arguments
                .iter()
                .map(|argument| crate::substitute_type(argument.clone(), &trait_substitution))
                .collect(),
        };
        let selection = select_at_depth(program, environment, &bound, origin, depth + 1)?;
        prerequisites.push(Some((bound, selection)));
    }
    Ok(DictionarySelection::Build(Box::new(BuildSelection {
        implementation: chosen.implementation,
        obligation: built,
        substitution,
        members,
        prerequisites,
    })))
}

fn marker_traits(program: &super::LoweredProgram) -> [Option<crate::TraitId>; 4] {
    let ids = &program.semantic_ids;
    [
        ids.copy_trait,
        ids.sized_trait,
        ids.drop_trait,
        ids.natural_trait,
    ]
}

/// Visit every member of every builder in a selection, nested ones included.
pub(super) fn for_each_member<'a>(
    selection: &'a DictionarySelection,
    visit: &mut dyn FnMut(&'a MemberSelection),
) {
    let DictionarySelection::Build(build) = selection else {
        return;
    };
    for member in &build.members {
        visit(member);
        for dictionary in &member.dictionaries {
            if let MemberDictionarySelection::Select(_, nested) = dictionary {
                for_each_member(nested, visit);
            }
        }
    }
    for prerequisite in build.prerequisites.iter().flatten() {
        for_each_member(&prerequisite.1, visit);
    }
}

/// Form the recipe for a selection once each member has an instance.
/// `instance` pairs a member with its interned dynamic instance.
pub(super) fn materialize_recipe(
    program: &super::LoweredProgram,
    selection: &DictionarySelection,
    instance: &mut dyn FnMut(&MemberSelection) -> Option<super::FunctionInstanceId>,
    origin: &crate::Origin,
) -> Result<DictionaryRecipe, staple_syntax::Diagnostic> {
    match selection {
        DictionarySelection::Environment(recipe) => Ok(DictionaryRecipe::Environment(recipe.clone())),
        DictionarySelection::Static(key) => Ok(DictionaryRecipe::Static(key.clone())),
        DictionarySelection::Build(build) => {
            let mut members = Vec::new();
            let mut region = 0;
            for member in &build.members {
                let missing = || {
                    staple_syntax::Diagnostic::new(
                        origin.span.clone(),
                        "dictionary member has no instance",
                    )
                };
                let id = instance(member).ok_or_else(missing)?;
                let descriptors = program
                    .instance_runtime_order(id)
                    .ok_or_else(missing)?
                    .into_iter()
                    .map(|parameter| {
                        build.substitution.get(&parameter).cloned().ok_or_else(|| {
                            staple_syntax::Diagnostic::new(
                                origin.span.clone(),
                                "dictionary member parameter has no caller type",
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let mut dictionaries = Vec::new();
                for dictionary in &member.dictionaries {
                    dictionaries.push(match dictionary {
                        MemberDictionarySelection::Itself => MemberDictionary::Itself,
                        MemberDictionarySelection::Select(obligation, nested) => {
                            MemberDictionary::Recipe(DictionaryCallArgument {
                                obligation: obligation.clone(),
                                recipe: materialize_recipe(program, nested, instance, origin)?,
                            })
                        }
                    });
                }
                let words = descriptors.len() + dictionaries.len();
                members.push(BuilderMember {
                    instance: id,
                    region,
                    descriptors,
                    dictionaries,
                });
                region += words;
            }
            let prerequisites = build
                .prerequisites
                .iter()
                .map(|prerequisite| {
                    prerequisite
                        .as_ref()
                        .map(|(obligation, nested)| {
                            Ok(DictionaryCallArgument {
                                obligation: obligation.clone(),
                                recipe: materialize_recipe(program, nested, instance, origin)?,
                            })
                        })
                        .transpose()
                })
                .collect::<Result<Vec<_>, staple_syntax::Diagnostic>>()?;
            Ok(DictionaryRecipe::Builder(Box::new(BuilderRecipe {
                implementation: build.implementation,
                obligation: build.obligation.clone(),
                members,
                prerequisites,
            })))
        }
    }
}

/// The dictionaries an instance receives (T-C), in its declared-bound order.
pub(super) fn enclosing_dictionary_bounds(
    program: &super::LoweredProgram,
    enclosing: &super::instance_resolution::ResolvedInstanceRequest,
) -> Vec<crate::CheckedTraitBound> {
    let Some(template) = program.functions.get(enclosing.key.function()) else {
        return Vec::new();
    };
    runtime_obligations(
        &program.semantic_ids,
        &template.declared_bounds,
        &enclosing.environment.substitution_map(),
        enclosing.key.runtime(),
    )
}

/// The obligations a call to `function` passes as dictionaries, at the
/// caller's types, when they mention the caller's run-time parameters. This
/// is the template-level form of the callee instance's run-time bounds.
pub(super) fn callee_runtime_obligations(
    program: &super::LoweredProgram,
    function: crate::FunctionId,
    substitutions: &crate::CallSubstitutions,
    enclosing: &super::instance_resolution::ResolvedInstanceRequest,
) -> Vec<crate::CheckedTraitBound> {
    let Some(callee) = program.functions.get(function) else {
        return Vec::new();
    };
    let runtime = program
        .relevant_parameters(function)
        .type_parameters()
        .collect::<std::collections::BTreeSet<_>>();
    // The caller's types as its own instance spells them, so they compare
    // equal to its dictionary bounds.
    let enclosing_map = enclosing.environment.substitution_map();
    let substitution = substitutions
        .types
        .iter()
        .map(|entry| {
            (
                entry.parameter,
                crate::substitute_type(entry.value_type.clone(), &enclosing_map),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    runtime_obligations(
        &program.semantic_ids,
        &callee.declared_bounds,
        &Default::default(),
        &runtime,
    )
    .into_iter()
    .map(|obligation| crate::CheckedTraitBound {
        trait_id: obligation.trait_id,
        arguments: obligation
            .arguments
            .into_iter()
            .map(|argument| crate::substitute_type(argument, &substitution))
            // A nested function shares its enclosing function's parameters.
            .map(|argument| crate::substitute_type(argument, &enclosing_map))
            .collect(),
    })
    .filter(|obligation| obligation.arguments.iter().any(crate::contains_type_parameter))
    // A scheme value's own dictionaries come from its caller.
    .filter(|obligation| {
        let mut referenced = std::collections::BTreeSet::new();
        for argument in &obligation.arguments {
            super::instance_resolution::collect_referenced_parameters(argument, &mut referenced);
        }
        callee
            .scheme_type_parameters
            .iter()
            .all(|parameter| !referenced.contains(parameter))
    })
    .collect()
}

/// A trait-member call through a dictionary at a run-time obligation (T-K).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictionarySlotCall {
    pub obligation: crate::CheckedTraitBound,
    pub recipe: DictionaryRecipe,
    /// Absolute pointer-slot offset of the member in the dictionary prefix.
    pub slot: usize,
    /// The member's type at the obligation's types: its dynamic ABI.
    pub function_type: crate::CheckedFunctionType,
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

/// The interned dynamic instance of a dictionary member, requested from the
/// enclosing instance exactly as the worklist did.
fn member_instance(
    program: &super::LoweredProgram,
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
    origin: &crate::Origin,
    member: &MemberSelection,
) -> Option<super::FunctionInstanceId> {
    use super::instance_resolution::{InstanceResolutionRequest, InstanceResolutionTarget};
    let enclosing = enclosing?;
    let relevant_empty = program.relevant_parameters(member.function).is_empty();
    let request = InstanceResolutionRequest {
        function: member.function,
        origin: origin.clone(),
        function_type: member.function_type.clone(),
        substitutions: member.substitutions.clone(),
        evidence: (!relevant_empty).then(|| member.evidence.clone()),
        target: InstanceResolutionTarget::Nested(enclosing),
    };
    let resolved = program.resolve_instance_request(&request).ok()?;
    let ordinal = program.specializations.instance_ordinal(&resolved.key)?;
    let id = <super::FunctionInstanceId as super::ArenaId>::from_index(ordinal.index());
    program.instances.contains(id).then_some(id)
}

/// Plan how a dictionary for `obligation` is obtained in the enclosing
/// instance, as a recipe whose builders name interned member instances.
pub(super) fn plan_recipe(
    program: &super::LoweredProgram,
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
    environment: &[crate::CheckedTraitBound],
    obligation: &crate::CheckedTraitBound,
    origin: &crate::Origin,
) -> Result<DictionaryRecipe, staple_syntax::Diagnostic> {
    let selection = select_dictionary(program, environment, obligation, origin)?;
    materialize_recipe(
        program,
        &selection,
        &mut |member| member_instance(program, enclosing, origin, member),
        origin,
    )
}

/// Select the dictionary and slot a run-time trait member call reads: an
/// environment bound, or a dictionary built from the selected implementation.
pub(super) fn plan_slot_call(
    program: &super::LoweredProgram,
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
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
    let recipe = plan_recipe(program, enclosing, environment, obligation, origin)?;
    // A built dictionary completes the obligation's associated arguments.
    let obligation = match &recipe {
        DictionaryRecipe::Builder(build) => build.obligation.clone(),
        _ => obligation.clone(),
    };
    let function_type = super::worklist::instantiate_method_type(
        program,
        origin,
        obligation.trait_id,
        method,
        &obligation.arguments,
    )?;
    Ok(DictionarySlotCall {
        obligation,
        recipe,
        slot: metadata.dictionary_layout.slots.start + slot,
        function_type,
    })
}

/// Translate a dynamic callee's declared bounds into the caller's types.
pub(super) fn plan_direct_call(
    program: &super::LoweredProgram,
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
    environment: &[crate::CheckedTraitBound],
    call: &super::LoweredCall,
    callee: super::FunctionInstanceId,
) -> Result<Vec<DictionaryCallArgument>, staple_syntax::Diagnostic> {
    plan_instance_dictionaries(
        program,
        enclosing,
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
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
    environment: &[crate::CheckedTraitBound],
    value: &super::LoweredCallableValue,
    callee: super::FunctionInstanceId,
) -> Result<Vec<DictionaryCallArgument>, staple_syntax::Diagnostic> {
    let stored = stored_dictionary_count(program, callee).unwrap_or(0);
    plan_instance_dictionaries(
        program,
        enclosing,
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
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
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
            let recipe = plan_recipe(program, enclosing, environment, &obligation, origin)?;
            Ok(DictionaryCallArgument { obligation, recipe })
        })
        .collect()
}

/// A request a built dictionary adds to its owner: a slot entry, or a static
/// dictionary a nested recipe names.
#[derive(Debug, Clone)]
pub(super) enum BuilderUse {
    Entry(BuilderMember, usize),
    Static(crate::CheckedTraitBound),
}

/// Walk a recipe's builders in a fixed order, collecting their uses.
/// `top_level` recipes keep their own use-site request elsewhere.
fn walk_recipe(
    program: &super::LoweredProgram,
    obligation: &crate::CheckedTraitBound,
    recipe: &DictionaryRecipe,
    top_level: bool,
    uses: &mut Vec<(crate::CheckedTraitBound, BuilderUse)>,
) {
    match recipe {
        DictionaryRecipe::Environment(_) => {}
        DictionaryRecipe::Static(_) => {
            if !top_level {
                uses.push((obligation.clone(), BuilderUse::Static(obligation.clone())));
            }
        }
        DictionaryRecipe::Builder(build) => {
            let Some(metadata) = program.traits.get(build.obligation.trait_id) else {
                return;
            };
            let environment = metadata.dictionary_layout.environment;
            for member in &build.members {
                uses.push((
                    build.obligation.clone(),
                    BuilderUse::Entry(member.clone(), environment),
                ));
                for dictionary in &member.dictionaries {
                    if let MemberDictionary::Recipe(argument) = dictionary {
                        walk_recipe(program, &argument.obligation, &argument.recipe, false, uses);
                    }
                }
            }
            for argument in build.prerequisites.iter().flatten() {
                walk_recipe(program, &argument.obligation, &argument.recipe, false, uses);
            }
        }
    }
}

/// Every builder use of one body, in a deterministic order that use sites
/// index into.
pub(super) fn body_builder_uses(
    program: &super::LoweredProgram,
    body: &super::LoweredInstanceBody,
) -> Vec<(crate::Origin, BuilderUse)> {
    let mut found = Vec::new();
    let mut walked = |origin: &crate::Origin,
                      obligation: &crate::CheckedTraitBound,
                      recipe: &DictionaryRecipe| {
        let mut uses = Vec::new();
        walk_recipe(program, obligation, recipe, true, &mut uses);
        found.extend(uses.into_iter().map(|(_, use_)| (origin.clone(), use_)));
    };
    for (call, arguments) in &body.dictionary_calls {
        if let Some(call) = body.calls.get(*call) {
            for argument in arguments {
                walked(&call.origin, &argument.obligation, &argument.recipe);
            }
        }
    }
    for (value, arguments) in &body.dictionary_values {
        if let Some(value) = body.callable_values.get(*value) {
            for argument in arguments {
                walked(&value.origin, &argument.obligation, &argument.recipe);
            }
        }
    }
    for target in body.bindings.values() {
        if let super::LoweredBoundTarget::Dictionary(dispatch) = target {
            walked(&body.origin, &dispatch.obligation, &dispatch.recipe);
        }
    }
    found
}

/// The forwarding entry of one built-dictionary member.
fn member_entry_request(
    program: &super::LoweredProgram,
    member: &BuilderMember,
    environment: usize,
    origin: &crate::Origin,
) -> Result<super::artifact_closure::ClosureRequest, staple_syntax::Diagnostic> {
    use super::artifact_closure::ClosureRequest;
    let missing = |what: &str| staple_syntax::Diagnostic::new(origin.span.clone(), what.to_owned());
    let record = program
        .instances
        .get(member.instance)
        .ok_or_else(|| missing("dictionary member instance is absent"))?;
    let runtime = program
        .instance_runtime_order(member.instance)
        .ok_or_else(|| missing("dictionary member has no descriptor order"))?;
    if runtime.is_empty() {
        return Err(missing(
            "dictionary members that need no descriptors are not supported yet",
        ));
    }
    let signature = record
        .body
        .as_ref()
        .ok_or_else(|| missing("dictionary member has no body"))?
        .signature
        .clone();
    let (dictionary_count, bound_dictionaries) =
        entry_dictionary_partition(program, member.instance, runtime.len())
            .ok_or_else(|| missing("dictionary member has no dictionary partition"))?;
    let offset = environment + member.region;
    Ok(ClosureRequest::Artifact {
        key: crate::specialization::ArtifactRequestKey::DescriptorBoundEntry(
            crate::specialization::DescriptorBoundEntryKey {
                instance: record.ordinal,
                bound: runtime.len(),
                offset,
            },
        ),
        plan: super::LoweredArtifactPlan::DescriptorBoundEntry(super::DescriptorBoundEntryPlan {
            bound: runtime.len(),
            dictionary_count,
            bound_dictionaries,
            environment_offset: offset,
            instance: member.instance,
            runtime,
            function_type: signature,
        }),
        kind: super::LoweredArtifactDependencyKind::DescriptorBoundEntry,
        origin: origin.clone(),
        use_site: None,
    })
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
    for (index, (origin, use_)) in body_builder_uses(program, body).into_iter().enumerate() {
        let mut request = match use_ {
            BuilderUse::Entry(member, environment) => {
                member_entry_request(program, &member, environment, &origin)
            }
            BuilderUse::Static(obligation) => static_request(program, &obligation, &origin),
        }
        .map_err(|error| vec![error])?;
        let super::artifact_closure::ClosureRequest::Artifact { use_site, .. } = &mut request
        else {
            unreachable!()
        };
        *use_site = Some(super::artifact_closure::ArtifactUseSite::DictionaryBuilder(index));
        requests.push(request);
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
                let arguments = plan_direct_call(self.program, None, &[], call, callee)
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
