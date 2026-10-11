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

/// Two obligations name the same trait at equivalent types. Parameters are
/// compared by identity, so a capture's or a signature's spelling of the same
/// parameter matches a bound's.
pub(crate) fn bounds_equivalent(left: &crate::CheckedTraitBound, right: &crate::CheckedTraitBound) -> bool {
    left.trait_id == right.trait_id
        && left.arguments.len() == right.arguments.len()
        && left
            .arguments
            .iter()
            .zip(&right.arguments)
            .all(|(left, right)| crate::types_equivalent(left, right))
}

/// Search direct arguments before any prerequisite path. Visit each equal
/// obligation once per root so cyclic prerequisite graphs terminate.
pub(super) fn environment_recipe(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    obligation: &crate::CheckedTraitBound,
) -> Option<EnvironmentDictionaryRecipe> {
    if let Some(argument) = environment
        .iter()
        .position(|bound| bounds_equivalent(bound, obligation))
    {
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
            if visited[recipe.argument]
                .iter()
                .any(|seen| bounds_equivalent(seen, &next))
            {
                continue;
            }
            let mut route = recipe.clone();
            route
                .prerequisites
                .push(metadata.dictionary_layout.prerequisites.start + index);
            if bounds_equivalent(&next, obligation) {
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
    /// A structural trait member for a compound shape, applied to its
    /// components' recipes (T-E).
    Structural(Box<StructuralRecipe>),
}

/// The run-time dictionary of a structural trait over a product or sum. Its
/// tail holds the compound descriptor, then one component dictionary each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructuralRecipe {
    pub shape: crate::specialization::DynamicStructuralShape,
    pub trait_id: crate::TraitId,
    /// The obligation completed by the derivation, so its associated
    /// arguments are the descriptors the dictionary's prefix holds.
    pub obligation: crate::CheckedTraitBound,
    /// Types whose descriptors the tail stores first, in order.
    pub descriptors: Vec<crate::CheckedType>,
    /// Component dictionaries stored after the descriptors.
    pub components: Vec<DictionaryCallArgument>,
    /// The trait's prerequisite dictionaries, in prefix order.
    pub prerequisites: Vec<Option<DictionaryCallArgument>>,
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
    Structural(Box<StructuralSelection>),
}

#[derive(Debug, Clone)]
pub(super) struct StructuralSelection {
    pub shape: crate::specialization::DynamicStructuralShape,
    pub obligation: crate::CheckedTraitBound,
    pub descriptors: Vec<crate::CheckedType>,
    pub components: Vec<(crate::CheckedTraitBound, DictionarySelection)>,
    pub prerequisites: Vec<Option<(crate::CheckedTraitBound, DictionarySelection)>>,
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
    /// The member in terms of its own parameters, for a request that has no
    /// enclosing instance.
    pub generic: GenericMember,
    /// The member instance's dictionaries at the caller's types, in its
    /// declared-bound order.
    pub dictionaries: Vec<MemberDictionarySelection>,
}

/// A member instance independent of any caller: every relevant type parameter
/// of its function is run-time.
#[derive(Debug, Clone)]
pub(super) struct GenericMember {
    pub function_type: crate::CheckedFunctionType,
    pub substitutions: crate::CallSubstitutions,
    pub evidence: Option<crate::TraitEvidence>,
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
        // A concrete obligation uses a static dictionary when an explicit
        // implementation provides it. A compiler-derived structural trait
        // (the `Debug` of a product or sum) has no implementation to point
        // at, so its dictionary is built from its components' dictionaries,
        // as at run-time types.
        return match static_request(program, obligation, origin) {
            Ok(super::artifact_closure::ClosureRequest::Artifact { key, .. }) => {
                Ok(DictionarySelection::Static(key))
            }
            Ok(_) => unreachable!(),
            Err(error) => select_structural(program, environment, obligation, origin, depth)
                .map_err(|_| error),
        };
    }
    if let Some(recipe) = environment_recipe(program, environment, obligation) {
        return Ok(DictionarySelection::Environment(recipe));
    }
    let Some(chosen) = program.select_runtime_implementation(obligation, origin)? else {
        return select_structural(program, environment, obligation, origin, depth);
    };
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
        let own_scope = method_runtime_scope(program, *method);
        let mut dictionaries = Vec::new();
        for bound in &template.declared_bounds {
            if bound.arguments.iter().any(|argument| {
                let mut referenced = std::collections::BTreeSet::new();
                super::instance_resolution::collect_referenced_parameters(argument, &mut referenced);
                !referenced.is_disjoint(&own_scope.types)
            }) {
                continue;
            }
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
            dictionaries.push(if bounds_equivalent(&at_caller, &built) {
                MemberDictionarySelection::Itself
            } else {
                let selection = select_at_depth(program, environment, &at_caller, origin, depth + 1)?;
                MemberDictionarySelection::Select(at_caller, selection)
            });
        }
        let implementation = program
            .trait_implementations
            .get(chosen.implementation)
            .ok_or_else(|| {
                staple_syntax::Diagnostic::new(
                    origin.span.clone(),
                    "dictionary implementation is absent",
                )
            })?;
        let own_method = implementation
            .methods
            .iter()
            .any(|(candidate, found)| candidate == method && *found == function);
        let header = if own_method {
            implementation.arguments.clone()
        } else {
            metadata.parameters.clone()
        };
        let generic = GenericMember {
            function_type: super::worklist::instantiate_method_type(
                program,
                origin,
                obligation.trait_id,
                *method,
                &header,
            )?,
            substitutions: super::worklist::trait_site_substitutions(
                program,
                obligation.trait_id,
                &header,
            ),
            evidence: own_method.then(|| crate::TraitEvidence::ExplicitImplementation {
                trait_id: obligation.trait_id,
                implementation: chosen.implementation,
                method: *method,
                function,
                arguments: header.clone(),
            }),
        };
        members.push(MemberSelection {
            generic,
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

/// A structural trait over a product or sum at run-time component types:
/// `Debug`, and the `Index`, `MutateIndex` and iteration families over
/// products. Each derives the same completed arguments the checker does.
fn select_structural(
    program: &super::LoweredProgram,
    environment: &[crate::CheckedTraitBound],
    obligation: &crate::CheckedTraitBound,
    origin: &crate::Origin,
    depth: usize,
) -> Result<DictionarySelection, staple_syntax::Diagnostic> {
    use crate::CheckedType;
    use crate::specialization::DynamicStructuralShape as Shape;
    let ids = &program.semantic_ids;
    let unavailable = || {
        let name = program
            .traits
            .get(obligation.trait_id)
            .map(|metadata| metadata.name.clone())
            .unwrap_or_default();
        staple_syntax::Diagnostic::new(
            origin.span.clone(),
            format!(
                "no implementation of trait `{name}` is available at run-time types for this shape"
            ),
        )
    };
    let trait_id = obligation.trait_id;
    let select_component = |bound: crate::CheckedTraitBound| {
        select_at_depth(program, environment, &bound, origin, depth + 1)
            .map(|selection| (bound, selection))
    };
    // Per-element injection into an output sum, or none when the output is
    // the element type.
    let injections = |product: &crate::CheckedProductType, output: &CheckedType| {
        product
            .elements
            .iter()
            .map(|element| match output {
                CheckedType::Sum(sum) if !crate::types_equivalent(output, &element.value_type) => {
                    sum.alternatives
                        .iter()
                        .position(|alternative| {
                            crate::types_equivalent(alternative, &element.value_type)
                        })
                        .map(Some)
                        .ok_or_else(unavailable)
                }
                _ => Ok(None),
            })
            .collect::<Result<Vec<_>, _>>()
    };
    let mut components = Vec::new();
    let (shape, completed, descriptors): (Shape, Vec<CheckedType>, Vec<CheckedType>) = if Some(
        trait_id,
    ) == ids.debug_trait
    {
        let [target] = obligation.arguments.as_slice() else {
            return Err(unavailable());
        };
        let (shape, elements): (_, Vec<CheckedType>) = match target {
            CheckedType::Product(product) if !product.variadic => (
                Shape::DebugProduct {
                    names: product
                        .elements
                        .iter()
                        .map(|element| element.name.clone())
                        .collect(),
                },
                product
                    .elements
                    .iter()
                    .map(|element| element.value_type.clone())
                    .collect(),
            ),
            CheckedType::Sum(sum) => (
                Shape::DebugSum {
                    alternatives: sum.alternatives.len(),
                },
                sum.alternatives.clone(),
            ),
            _ => return Err(unavailable()),
        };
        for element in elements {
            components.push(select_component(crate::CheckedTraitBound {
                trait_id,
                arguments: vec![element],
            })?);
        }
        (shape, obligation.arguments.clone(), vec![target.clone()])
    } else if Some(trait_id) == ids.index_trait || Some(trait_id) == ids.mutate_index_trait {
        let mutate = Some(trait_id) == ids.mutate_index_trait;
        let [target, position, dependent] = obligation.arguments.as_slice() else {
            return Err(unavailable());
        };
        match target {
            CheckedType::Ref(payload) => {
                let delegate = select_component(crate::CheckedTraitBound {
                    trait_id,
                    arguments: vec![payload.as_ref().clone(), position.clone(), dependent.clone()],
                })?;
                let output = selection_obligation(&delegate.1)
                    .and_then(|bound| bound.arguments.get(2).cloned())
                    .unwrap_or_else(|| dependent.clone());
                let position = selection_obligation(&delegate.1)
                    .and_then(|bound| bound.arguments.get(1).cloned())
                    .unwrap_or_else(|| position.clone());
                components.push(delegate);
                (
                    if mutate {
                        Shape::DerefMutateIndex
                    } else {
                        Shape::DerefIndex
                    },
                    vec![target.clone(), position, output],
                    Vec::new(),
                )
            }
            CheckedType::Product(product)
                if !product.variadic
                    && !product.elements.is_empty()
                    && matches!(position, CheckedType::USize | CheckedType::Inferred) =>
            {
                if mutate {
                    let element = product.homogeneous_element().ok_or_else(unavailable)?.clone();
                    if !matches!(dependent, CheckedType::Inferred)
                        && !crate::types_equivalent(dependent, &element)
                    {
                        return Err(unavailable());
                    }
                    (
                        Shape::MutateProduct {
                            length: product.elements.len(),
                        },
                        vec![target.clone(), CheckedType::USize, element],
                        vec![target.clone()],
                    )
                } else {
                    let output = crate::product_item(product);
                    if !matches!(dependent, CheckedType::Inferred)
                        && !crate::types_equivalent(dependent, &output)
                    {
                        return Err(unavailable());
                    }
                    (
                        Shape::IndexProduct {
                            injections: injections(product, &output)?,
                            output_alternatives: sum_width(&output),
                        },
                        vec![target.clone(), CheckedType::USize, output.clone()],
                        vec![target.clone(), output],
                    )
                }
            }
            _ => return Err(unavailable()),
        }
    } else if Some(trait_id) == ids.into_iterator_trait {
        let [source, _] = obligation.arguments.as_slice() else {
            return Err(unavailable());
        };
        let CheckedType::Product(product) = source else {
            return Err(unavailable());
        };
        if product.variadic || product.elements.is_empty() {
            return Err(unavailable());
        }
        let iterator = pair_with_cursor(source);
        (
            Shape::IntoIterProduct,
            vec![source.clone(), iterator.clone()],
            vec![source.clone(), iterator],
        )
    } else if Some(trait_id) == ids.iterator_trait {
        let [iterator, _] = obligation.arguments.as_slice() else {
            return Err(unavailable());
        };
        let CheckedType::Product(pair) = iterator else {
            return Err(unavailable());
        };
        let [first, second] = pair.elements.as_slice() else {
            return Err(unavailable());
        };
        let (CheckedType::Product(product), CheckedType::USize) = (&first.value_type, &second.value_type)
        else {
            return Err(unavailable());
        };
        if product.variadic || product.elements.is_empty() {
            return Err(unavailable());
        }
        let item = crate::product_item(product);
        let metadata = program.traits.get(trait_id).ok_or_else(unavailable)?;
        let method = *metadata.methods.first().ok_or_else(unavailable)?;
        let function_type = super::worklist::instantiate_method_type(
            program,
            origin,
            trait_id,
            method,
            &[iterator.clone(), item.clone()],
        )?;
        let result = function_type.result.as_ref().clone();
        let sum = crate::sum_through_implicit_wrapper(&result).ok_or_else(unavailable)?;
        let yield_representation = CheckedType::Product(crate::CheckedProductType {
            elements: vec![
                crate::CheckedTypeElement {
                    name: None,
                    value_type: item.clone(),
                    default: None,
                },
                crate::CheckedTypeElement {
                    name: None,
                    value_type: iterator.clone(),
                    default: None,
                },
            ],
            variadic: false,
        });
        let mut done = None;
        let mut yield_ = None;
        for (index, alternative) in sum.alternatives.iter().enumerate() {
            if let CheckedType::Wrapper { representation, .. } = alternative {
                if representation.as_ref() == iterator {
                    done = Some(index);
                }
                if representation.as_ref() == &yield_representation {
                    yield_ = Some(index);
                }
            }
        }
        let (Some(done), Some(yield_)) = (done, yield_) else {
            return Err(unavailable());
        };
        (
            Shape::NextProduct {
                injections: injections(product, &item)?,
                item_alternatives: sum_width(&item),
                result_alternatives: sum.alternatives.len(),
                done,
                yield_,
            },
            vec![iterator.clone(), item.clone()],
            vec![iterator.clone(), result],
        )
    } else {
        return Err(unavailable());
    };
    let obligation = crate::CheckedTraitBound {
        trait_id,
        arguments: completed,
    };
    // The trait's prerequisites (an associated type's bound) need their own
    // dictionaries.
    let metadata = program.traits.get(trait_id).ok_or_else(unavailable)?;
    let substitution = dictionary_substitution(metadata, &obligation.arguments).unwrap_or_default();
    let markers = marker_traits(program);
    let mut prerequisites = Vec::new();
    for prerequisite in &metadata.prerequisites {
        if markers.contains(&Some(prerequisite.trait_id)) {
            prerequisites.push(None);
            continue;
        }
        prerequisites.push(Some(select_component(crate::CheckedTraitBound {
            trait_id: prerequisite.trait_id,
            arguments: prerequisite
                .arguments
                .iter()
                .map(|argument| crate::substitute_type(argument.clone(), &substitution))
                .collect(),
        })?));
    }
    Ok(DictionarySelection::Structural(Box::new(StructuralSelection {
        shape,
        obligation,
        descriptors,
        components,
        prerequisites,
    })))
}

/// How many alternatives a sum output has, or zero for any other type.
fn sum_width(output: &crate::CheckedType) -> usize {
    match output {
        crate::CheckedType::Sum(sum) => sum.alternatives.len(),
        _ => 0,
    }
}

/// The `(source, USize)` iterator a product converts into.
fn pair_with_cursor(source: &crate::CheckedType) -> crate::CheckedType {
    crate::CheckedType::Product(crate::CheckedProductType {
        elements: vec![
            crate::CheckedTypeElement {
                name: None,
                value_type: source.clone(),
                default: None,
            },
            crate::CheckedTypeElement {
                name: None,
                value_type: crate::CheckedType::USize,
                default: None,
            },
        ],
        variadic: false,
    })
}

/// The obligation a selection completes, when it completes one.
fn selection_obligation(selection: &DictionarySelection) -> Option<&crate::CheckedTraitBound> {
    match selection {
        DictionarySelection::Build(build) => Some(&build.obligation),
        DictionarySelection::Structural(structural) => Some(&structural.obligation),
        _ => None,
    }
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
    let build = match selection {
        DictionarySelection::Build(build) => build,
        DictionarySelection::Structural(structural) => {
            for (_, component) in &structural.components {
                for_each_member(component, visit);
            }
            for prerequisite in structural.prerequisites.iter().flatten() {
                for_each_member(&prerequisite.1, visit);
            }
            return;
        }
        _ => return,
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
        DictionarySelection::Structural(structural) => {
            let components = structural
                .components
                .iter()
                .map(|(obligation, nested)| {
                    Ok(DictionaryCallArgument {
                        obligation: obligation.clone(),
                        recipe: materialize_recipe(program, nested, instance, origin)?,
                    })
                })
                .collect::<Result<Vec<_>, staple_syntax::Diagnostic>>()?;
            let prerequisites = structural
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
            Ok(DictionaryRecipe::Structural(Box::new(StructuralRecipe {
                shape: structural.shape.clone(),
                trait_id: structural.obligation.trait_id,
                obligation: structural.obligation.clone(),
                descriptors: structural.descriptors.clone(),
                components,
                prerequisites,
            })))
        }
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
                let (_, _, method) = evidence_obligation(&member.evidence);
                let own_scope = method.map(|method| method_runtime_scope(program, method)).unwrap_or_default();
                let descriptors = program
                    .instance_runtime_order(id)
                    .ok_or_else(missing)?
                    .into_iter()
                    .filter(|parameter| !own_scope.types.contains(parameter))
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
    own: &[crate::TypeParameterId],
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
            .chain(own)
            .all(|parameter| !referenced.contains(parameter))
    })
    .collect()
}

/// Where a dictionary list is passed or stored: a call's hidden arguments, or
/// an instantiation adapter's environment at a coerced expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DictionarySite {
    Call(super::LoweredCallId),
    Coercion(super::ExpressionId),
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
    pub entry_type: crate::CheckedFunctionType,
    pub descriptors: Vec<crate::CheckedType>,
    pub dictionaries: Vec<DictionaryCallArgument>,
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
    let (_, _, method) = evidence_obligation(&member.evidence);
    let scope = method.map(|method| method_runtime_scope(program, method)).unwrap_or_default();
    let resolved = program.resolve_instance_request_in_scope(&request, &scope).ok()?;
    let ordinal = program.specializations.instance_ordinal(&resolved.key)?;
    let id = <super::FunctionInstanceId as super::ArenaId>::from_index(ordinal.index());
    program.instances.contains(id).then_some(id)
}

/// The interned instance of a member in terms of its own parameters, all of
/// them run-time. A scheme adapter's builders use these, since the adapter has
/// no enclosing instance.
pub(super) fn member_instance_root(
    program: &super::LoweredProgram,
    origin: &crate::Origin,
    member: &MemberSelection,
) -> Option<super::FunctionInstanceId> {
    use super::instance_resolution::{InstanceResolutionRequest, InstanceResolutionTarget};
    let runtime = program
        .relevant_parameters(member.function)
        .type_parameters()
        .collect::<std::collections::BTreeSet<_>>();
    let request = InstanceResolutionRequest {
        function: member.function,
        origin: origin.clone(),
        function_type: member.generic.function_type.clone(),
        substitutions: member.generic.substitutions.clone(),
        evidence: member.generic.evidence.clone(),
        target: InstanceResolutionTarget::Root,
    };
    let resolved = program
        .resolve_instance_request_with_runtime(&request, &runtime)
        .ok()?;
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
pub(super) fn method_runtime_scope(program: &super::LoweredProgram, method: crate::TraitMethodId) -> super::instance_resolution::RuntimeScope {
    let mut scope = super::instance_resolution::RuntimeScope::default();
    if let Some(member) = program.trait_methods.get(method) {
        fn collect(parameter: &crate::typecheck::CheckedSchemeParameter, scope: &mut super::instance_resolution::RuntimeScope) {
            match parameter {
                crate::typecheck::CheckedSchemeParameter::Type { id, .. } => { scope.types.insert(*id); }
                crate::typecheck::CheckedSchemeParameter::Effect { id, .. } => { scope.effects.insert(*id); }
                crate::typecheck::CheckedSchemeParameter::Product(parameters) => {
                    for parameter in parameters { collect(parameter, scope); }
                }
            }
        }
        for parameter in &member.own_parameters { collect(parameter, &mut scope); }
    }
    scope
}

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
        DictionaryRecipe::Structural(structural) => structural.obligation.clone(),
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
        entry_type: function_type.clone(), function_type,
        descriptors: Vec::new(), dictionaries: Vec::new(),
    })
}

pub(super) fn instantiate_slot_call(
    program: &super::LoweredProgram,
    enclosing: &super::instance_resolution::ResolvedInstanceRequest,
    environment: &[crate::CheckedTraitBound],
    dispatch: &mut DictionarySlotCall,
    method: crate::TraitMethodId,
    actual: Option<&crate::CheckedFunctionType>,
    substitutions: &crate::CallSubstitutions,
    origin: &crate::Origin,
) -> Result<(), staple_syntax::Diagnostic> {
    let member = program.trait_methods.get(method).expect("member metadata");
    let mut map = enclosing.environment.substitution_map();
    let enclosing_map = map.clone();
    map.extend(substitutions.types.iter().map(|s| (s.parameter, crate::substitute_type(s.value_type.clone(), &enclosing_map))));
    for effect in &substitutions.effects {
        map.insert(effect.parameter, crate::typecheck::effect_substitution_type(effect.effects.clone()));
    }
    if let Some(actual) = actual {
        crate::infer_type_parameters(&crate::CheckedType::Function(dispatch.entry_type.clone()),
            &crate::CheckedType::Function(actual.clone()), &mut map);
    }
    dispatch.descriptors = member.own_parameters.iter().filter_map(|parameter| {
        if let crate::typecheck::CheckedSchemeParameter::Type { id, .. } = parameter {
            Some(map.get(id).cloned().unwrap_or_else(|| parameter.substitution_pattern()))
        } else { None }
    }).collect();
    dispatch.function_type = match crate::substitute_type(crate::CheckedType::Function(dispatch.entry_type.clone()), &map) {
        crate::CheckedType::Function(ty) => ty, _ => unreachable!(),
    };
    let trait_map = dictionary_substitution(program.traits.get(dispatch.obligation.trait_id).unwrap(), &dispatch.obligation.arguments).unwrap_or_default();
    map.extend(trait_map);
    let own_bounds = member.scheme.trait_bounds[1..].iter().map(|bound| crate::CheckedTraitBound {
        trait_id: bound.trait_id,
        arguments: bound.arguments.iter().map(|argument| crate::substitute_type(argument.clone(), &map)).collect(),
    }).filter(|bound| !marker_traits(program).contains(&Some(bound.trait_id))).collect();
    dispatch.dictionaries = plan_obligations(program, Some(enclosing), environment, own_bounds, origin)?;
    Ok(())
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

/// The non-marker bounds of a scheme at an instantiation, in scheme order.
pub(super) fn scheme_obligations(
    program: &super::LoweredProgram,
    scheme: &crate::CheckedScheme,
    substitution: &std::collections::HashMap<crate::TypeParameterId, crate::CheckedType>,
) -> Vec<crate::CheckedTraitBound> {
    let markers = marker_traits(program);
    scheme
        .trait_bounds
        .iter()
        .filter(|bound| !markers.contains(&Some(bound.trait_id)))
        .map(|bound| crate::CheckedTraitBound {
            trait_id: bound.trait_id,
            arguments: bound
                .arguments
                .iter()
                .map(|argument| crate::substitute_type(argument.clone(), substitution))
                .collect(),
        })
        .collect()
}

fn plan_obligations(
    program: &super::LoweredProgram,
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
    environment: &[crate::CheckedTraitBound],
    obligations: Vec<crate::CheckedTraitBound>,
    origin: &crate::Origin,
) -> Result<Vec<DictionaryCallArgument>, staple_syntax::Diagnostic> {
    obligations
        .into_iter()
        .map(|obligation| {
            let recipe = plan_recipe(program, enclosing, environment, &obligation, origin)?;
            Ok(DictionaryCallArgument { obligation, recipe })
        })
        .collect()
}

/// What an adapter between a scheme and its instantiation or subsumption
/// needs (T-M): the number of dictionaries the source scheme's entry takes,
/// the number the target scheme passes in, and, for a scheme target, recipes
/// that derive the source's from the target's.
pub(super) fn adapter_dictionaries(
    program: &super::LoweredProgram,
    source: &crate::CheckedScheme,
    types: &[crate::CheckedType],
    target: &crate::CheckedType,
    origin: &crate::Origin,
) -> Result<(usize, usize, Vec<DictionaryCallArgument>), staple_syntax::Diagnostic> {
    let identity = Default::default();
    let source_obligations = |substitution: &std::collections::HashMap<
        crate::TypeParameterId,
        crate::CheckedType,
    >| scheme_obligations(program, source, substitution);
    let count = source_obligations(&identity).len();
    let crate::CheckedType::Forall(target_scheme) = target else {
        return Ok((count, 0, Vec::new()));
    };
    let target_bounds = scheme_obligations(program, target_scheme, &identity);
    let substitution = source
        .type_parameter_ids()
        .into_iter()
        .zip(types.iter().cloned())
        .collect();
    let mut recipes = Vec::new();
    for obligation in source_obligations(&substitution) {
        let selection = select_dictionary(program, &target_bounds, &obligation, origin)?;
        let recipe = materialize_recipe(
            program,
            &selection,
            &mut |member| member_instance_root(program, origin, member),
            origin,
        )?;
        recipes.push(DictionaryCallArgument { obligation, recipe });
    }
    Ok((count, target_bounds.len(), recipes))
}

/// A scheme adapter's entailment at a coerced expression: the target scheme's
/// bounds, and the source's bounds at the instantiation with their selections.
/// `map` substitutes the enclosing instance's parameters.
pub(super) fn adapter_entailment(
    program: &super::LoweredProgram,
    expression: &super::LoweredExpression,
    map: &std::collections::HashMap<crate::TypeParameterId, crate::CheckedType>,
    origin: &crate::Origin,
) -> Result<Vec<(crate::CheckedTraitBound, DictionarySelection)>, staple_syntax::Diagnostic> {
    let (Some(coercion), Some(plan)) = (&expression.coercion, &expression.coercion_plan) else {
        return Ok(Vec::new());
    };
    let Some((leaf, produced, _)) = plan.split_instantiation(&coercion.target) else {
        return Ok(Vec::new());
    };
    let super::LoweredCoercionPlan::AdaptScheme { types } = leaf else {
        return Ok(Vec::new());
    };
    let crate::CheckedType::Forall(source) = &coercion.source else {
        return Ok(Vec::new());
    };
    let crate::CheckedType::Forall(target) = &produced else {
        return Ok(Vec::new());
    };
    let identity = Default::default();
    let target_bounds = scheme_obligations(program, target, &identity)
        .into_iter()
        .map(|bound| crate::CheckedTraitBound {
            trait_id: bound.trait_id,
            arguments: bound
                .arguments
                .into_iter()
                .map(|argument| crate::substitute_type(argument, map))
                .collect(),
        })
        .collect::<Vec<_>>();
    let substitution = source
        .type_parameter_ids()
        .into_iter()
        .zip(
            types
                .iter()
                .map(|ty| crate::substitute_type(ty.clone(), map)),
        )
        .collect();
    scheme_obligations(program, source, &substitution)
        .into_iter()
        .map(|obligation| {
            let obligation = crate::CheckedTraitBound {
                trait_id: obligation.trait_id,
                arguments: obligation
                    .arguments
                    .into_iter()
                    .map(|argument| crate::substitute_type(argument, map))
                    .collect(),
            };
            let selection = select_dictionary(program, &target_bounds, &obligation, origin)?;
            Ok((obligation, selection))
        })
        .collect()
}

/// A scheme adapter's entailment recipes at a coerced expression, with the
/// member instances its builders name.
pub(super) fn adapter_entailment_recipes(
    program: &super::LoweredProgram,
    expression: &super::LoweredExpression,
) -> Vec<DictionaryCallArgument> {
    let Ok(selections) = adapter_entailment(program, expression, &Default::default(), &expression.origin)
    else {
        return Vec::new();
    };
    selections
        .into_iter()
        .filter_map(|(obligation, selection)| {
            let recipe = materialize_recipe(
                program,
                &selection,
                &mut |member| member_instance_root(program, &expression.origin, member),
                &expression.origin,
            )
            .ok()?;
            Some(DictionaryCallArgument { obligation, recipe })
        })
        .collect()
}

/// Builder uses an expression's scheme adapter adds to its owner.
fn adapter_builder_uses(
    program: &super::LoweredProgram,
    expression: &super::LoweredExpression,
    found: &mut Vec<(crate::Origin, BuilderUse)>,
) {
    for argument in adapter_entailment_recipes(program, expression) {
        let mut uses = Vec::new();
        walk_recipe(program, &argument.obligation, &argument.recipe, false, &mut uses);
        found.extend(
            uses.into_iter()
                .map(|(_, use_)| (expression.origin.clone(), use_)),
        );
    }
}

/// Every builder use of one initializer's scheme adapters, in a fixed order.
pub(super) fn initializer_builder_uses(
    program: &super::LoweredProgram,
    initializer: super::InitializerId,
) -> Vec<(crate::Origin, BuilderUse)> {
    struct Visitor<'a> {
        program: &'a super::LoweredProgram,
        found: Vec<(crate::Origin, BuilderUse)>,
    }
    impl super::cleanup_artifacts::LoweredOwnerVisitor for Visitor<'_> {
        fn expression_site(
            &mut self,
            _id: super::ExpressionId,
            expression: &super::LoweredExpression,
        ) -> Result<(), Vec<staple_syntax::Diagnostic>> {
            adapter_builder_uses(self.program, expression, &mut self.found);
            Ok(())
        }
    }
    let mut visitor = Visitor {
        program,
        found: Vec::new(),
    };
    let _ = super::cleanup_artifacts::walk_owner(
        program,
        super::emission::OwnerArenas::Initializer(initializer),
        &mut visitor,
    );
    // Call sites in an initializer pass static dictionaries, or structural
    // ones built at the call from static parts.
    if let Ok(arguments) = initializer_call_arguments(program, initializer) {
        for (site, arguments) in arguments {
            let origin = match site {
                DictionarySite::Call(call) => program.calls.get(call).map(|call| &call.origin),
                DictionarySite::Coercion(expression) => program
                    .expressions
                    .get(expression)
                    .map(|expression| &expression.origin),
            };
            let Some(origin) = origin else {
                continue;
            };
            for argument in arguments {
                if matches!(argument.recipe, DictionaryRecipe::Static(_)) {
                    continue;
                }
                let mut uses = Vec::new();
                walk_recipe(program, &argument.obligation, &argument.recipe, true, &mut uses);
                visitor
                    .found
                    .extend(uses.into_iter().map(|(_, use_)| (origin.clone(), use_)));
            }
        }
    }
    visitor.found
}

/// Dictionaries a call through a polymorphic value passes (T-M): the scheme's
/// bounds at the call's instantiation, in scheme order.
pub(super) fn plan_scheme_call(
    program: &super::LoweredProgram,
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
    environment: &[crate::CheckedTraitBound],
    call: &super::LoweredCall,
) -> Result<Vec<DictionaryCallArgument>, staple_syntax::Diagnostic> {
    let Some(scheme) = &call.scheme else {
        return Ok(Vec::new());
    };
    let substitution = call
        .substitutions
        .types
        .iter()
        .map(|entry| (entry.parameter, entry.value_type.clone()))
        .collect();
    plan_obligations(
        program,
        enclosing,
        environment,
        scheme_obligations(program, scheme, &substitution),
        &call.origin,
    )
}

/// Dictionaries an instantiation adapter stores beside its descriptors
/// (T-M): the source scheme's bounds at the instantiation types.
pub(super) fn plan_instantiation(
    program: &super::LoweredProgram,
    enclosing: Option<&super::instance_resolution::ResolvedInstanceRequest>,
    environment: &[crate::CheckedTraitBound],
    expression: &super::LoweredExpression,
) -> Result<Vec<DictionaryCallArgument>, staple_syntax::Diagnostic> {
    let (Some(coercion), Some(plan)) = (&expression.coercion, &expression.coercion_plan) else {
        return Ok(Vec::new());
    };
    let Some((leaf, _, _)) = plan.split_instantiation(&coercion.target) else {
        return Ok(Vec::new());
    };
    let super::LoweredCoercionPlan::Instantiate { types } = leaf else {
        return Ok(Vec::new());
    };
    let crate::CheckedType::Forall(scheme) = &coercion.source else {
        return Ok(Vec::new());
    };
    let substitution = scheme
        .type_parameter_ids()
        .into_iter()
        .zip(types.iter().cloned())
        .collect();
    plan_obligations(
        program,
        enclosing,
        environment,
        scheme_obligations(program, scheme, &substitution),
        &expression.origin,
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
    let stored = site_stored_dictionary_count(program, callee, value.scheme.is_some()).unwrap_or(0);
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
/// Dictionaries one closure of `instance` stores. A polymorphic value's
/// callers supply the scheme's own bounds, so it stores the enclosing ones
/// (`stored_dictionary_count`). An instantiated closure is called without
/// dictionaries, so it stores every one its entry binds
/// (`entry_dictionary_partition`).
pub(super) fn site_stored_dictionary_count(
    program: &super::LoweredProgram,
    instance: super::FunctionInstanceId,
    polymorphic: bool,
) -> Option<usize> {
    if polymorphic {
        return stored_dictionary_count(program, instance);
    }
    let record = program.instances.get(instance)?;
    let template = program.functions.get(record.template)?;
    let key = program.specializations.instance(record.ordinal)?;
    Some(
        runtime_obligations(
            &program.semantic_ids,
            &template.declared_bounds,
            &record.environment.substitution_map(),
            key.runtime(),
        )
        .len(),
    )
}

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
    Structural(crate::specialization::DynamicStructuralShape, crate::TraitId),
    /// A concrete type whose static descriptor a structural dictionary
    /// stores. A concrete owner has no dynamic environment to build it from.
    Descriptor(crate::CheckedType),
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
        DictionaryRecipe::Structural(structural) => {
            uses.push((
                structural.obligation.clone(),
                BuilderUse::Structural(structural.shape.clone(), structural.trait_id),
            ));
            let associated = program
                .traits
                .get(structural.trait_id)
                .map_or(0, |metadata| metadata.dictionary_layout.associated.len());
            let stored = structural.obligation.arguments
                [structural.obligation.arguments.len() - associated..]
                .iter()
                .chain(&structural.descriptors);
            for ty in stored {
                if !crate::contains_type_parameter(ty) {
                    uses.push((structural.obligation.clone(), BuilderUse::Descriptor(ty.clone())));
                }
            }
            for component in &structural.components {
                walk_recipe(program, &component.obligation, &component.recipe, false, uses);
            }
            for prerequisite in structural.prerequisites.iter().flatten() {
                walk_recipe(
                    program,
                    &prerequisite.obligation,
                    &prerequisite.recipe,
                    false,
                    uses,
                );
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
    for (site, arguments) in &body.dictionary_calls {
        let origin = match site {
            DictionarySite::Call(call) => body.calls.get(*call).map(|call| &call.origin),
            DictionarySite::Coercion(expression) => body
                .expression(*expression)
                .map(|expression| &expression.origin),
        };
        if let Some(origin) = origin {
            for argument in arguments {
                walked(origin, &argument.obligation, &argument.recipe);
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
            for argument in &dispatch.dictionaries {
                walked(&body.origin, &argument.obligation, &argument.recipe);
            }
        }
    }
    for (_, expression) in body.expressions() {
        adapter_builder_uses(program, expression, &mut found);
    }
    found
}

/// The shared slot function of a structural dictionary shape.
fn structural_slot_request(
    shape: crate::specialization::DynamicStructuralShape,
    trait_id: crate::TraitId,
    origin: &crate::Origin,
) -> super::artifact_closure::ClosureRequest {
    super::artifact_closure::ClosureRequest::Artifact {
        key: crate::specialization::ArtifactRequestKey::DynamicStructural(
            crate::specialization::DynamicStructuralKey {
                shape: shape.clone(),
            },
        ),
        plan: super::LoweredArtifactPlan::DynamicStructural(super::DynamicStructuralPlan {
            shape,
            trait_id,
            write: None,
        }),
        kind: super::LoweredArtifactDependencyKind::DynamicStructural,
        origin: origin.clone(),
        use_site: None,
    }
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
    let signature = record
        .body
        .as_ref()
        .ok_or_else(|| missing("dictionary member has no body"))?
        .signature
        .clone();
    let dictionary_count = record.body.as_ref().unwrap().dictionary_bounds.len();
    let bound_dictionaries = member.dictionaries.len();
    let offset = environment + member.region;
    Ok(ClosureRequest::Artifact {
        key: crate::specialization::ArtifactRequestKey::DescriptorBoundEntry(
            crate::specialization::DescriptorBoundEntryKey {
                instance: record.ordinal,
                bound: member.descriptors.len(),
                offset,
            },
        ),
        plan: super::LoweredArtifactPlan::DescriptorBoundEntry(super::DescriptorBoundEntryPlan {
            bound: member.descriptors.len(),
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
    for (site, arguments) in &body.dictionary_calls {
        let origin = match site {
            DictionarySite::Call(call) => body.calls.get(*call).map(|call| &call.origin),
            DictionarySite::Coercion(expression) => body
                .expression(*expression)
                .map(|expression| &expression.origin),
        }
        .ok_or_else(|| {
            vec![staple_syntax::Diagnostic::new(
                body.origin.span.clone(),
                "dictionary recipe names a missing site",
            )]
        })?;
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
                *use_site = Some(dictionary_use_site(*site, index));
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
            BuilderUse::Structural(shape, trait_id) => Ok(structural_slot_request(
                shape, trait_id, &origin,
            )),
            BuilderUse::Descriptor(ty) => super::descriptor_artifacts::descriptor_request(&ty, &origin),
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
    std::collections::BTreeMap<DictionarySite, Vec<DictionaryCallArgument>>,
    Vec<staple_syntax::Diagnostic>,
> {
    struct Visitor<'a> {
        program: &'a super::LoweredProgram,
        initializer: super::InitializerId,
        arguments: std::collections::BTreeMap<DictionarySite, Vec<DictionaryCallArgument>>,
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
                    self.arguments.insert(DictionarySite::Call(id), arguments);
                }
            } else if call.scheme.is_some() {
                let arguments = plan_scheme_call(self.program, None, &[], call)
                    .map_err(|error| vec![error])?;
                if !arguments.is_empty() {
                    self.arguments.insert(DictionarySite::Call(id), arguments);
                }
            }
            Ok(())
        }

        fn expression_site(
            &mut self,
            id: super::ExpressionId,
            expression: &super::LoweredExpression,
        ) -> Result<(), Vec<staple_syntax::Diagnostic>> {
            let arguments = plan_instantiation(self.program, None, &[], expression)
                .map_err(|error| vec![error])?;
            if !arguments.is_empty() {
                self.arguments.insert(DictionarySite::Coercion(id), arguments);
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
    for (site, arguments) in arguments {
        let origin = match site {
            DictionarySite::Call(call) => program.calls.get(call).map(|call| &call.origin),
            DictionarySite::Coercion(expression) => program
                .expressions
                .get(expression)
                .map(|expression| &expression.origin),
        }
        .expect("initializer dictionary site");
        for (index, argument) in arguments.iter().enumerate() {
            // A structural dictionary at concrete types is built at the call
            // from static parts; its uses are requested below.
            let DictionaryRecipe::Static(key) = &argument.recipe else {
                continue;
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
            *use_site = Some(dictionary_use_site(site, index));
            requests.push(request);
        }
    }
    for (index, (origin, use_)) in initializer_builder_uses(program, initializer)
        .into_iter()
        .enumerate()
    {
        let mut request = match use_ {
            BuilderUse::Entry(member, environment) => {
                member_entry_request(program, &member, environment, &origin)
            }
            BuilderUse::Static(obligation) => static_request(program, &obligation, &origin),
            BuilderUse::Structural(shape, trait_id) => {
                Ok(structural_slot_request(shape, trait_id, &origin))
            }
            BuilderUse::Descriptor(ty) => super::descriptor_artifacts::descriptor_request(&ty, &origin),
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

fn dictionary_use_site(
    site: DictionarySite,
    argument: usize,
) -> super::artifact_closure::ArtifactUseSite {
    match site {
        DictionarySite::Call(call) => {
            super::artifact_closure::ArtifactUseSite::CallDictionary { call, argument }
        }
        DictionarySite::Coercion(expression) => {
            super::artifact_closure::ArtifactUseSite::CoercionDictionary {
                expression,
                argument,
            }
        }
    }
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

/// Expand a structural dictionary slot: a product's literals write through
/// the shared `Formatter.write` instance.
pub(super) fn expand_dynamic_structural(
    program: &super::LoweredProgram,
    artifact: super::LoweredArtifactRequestId,
    plan: super::LoweredArtifactPlan,
) -> super::artifact_closure::ExpansionResult {
    use super::artifact_closure::ClosureRequest;
    use super::artifact_plan::PlannedInstance;
    use super::instance_resolution::{InstanceResolutionRequest, InstanceResolutionTarget};
    use super::{LoweredArtifactPlan as Plan, LoweredInstanceDependencyKind as InstanceKind};
    use crate::specialization::DynamicStructuralShape;
    use staple_syntax::Diagnostic;
    let record = program
        .artifacts
        .get(artifact)
        .expect("interned structural slot");
    let origin = &record.origin;
    let Plan::DynamicStructural(mut plan) = plan else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            "structural slot has a mismatched plan",
        )]);
    };
    let mut requests = Vec::new();
    plan.write = None;
    if matches!(plan.shape, DynamicStructuralShape::DebugProduct { .. }) {
        let write = program.string_formatting.write.ok_or_else(|| {
            vec![Diagnostic::new(
                origin.span.clone(),
                "Formatter.write is unavailable for structural Debug",
            )]
        })?;
        let signature = program
            .functions
            .get(write)
            .map(|function| function.signature.clone())
            .ok_or_else(|| {
                vec![Diagnostic::new(
                    origin.span.clone(),
                    "Formatter.write has no lowered template",
                )]
            })?;
        let resolved = program
            .resolve_instance_request(&InstanceResolutionRequest {
                function: write,
                origin: origin.clone(),
                function_type: signature,
                substitutions: Default::default(),
                evidence: None,
                target: InstanceResolutionTarget::Root,
            })
            .map_err(|error| vec![error])?;
        plan.write = Some(PlannedInstance {
            key: resolved.key.clone(),
            instance: None,
            kind: InstanceKind::FormattingWrite,
        });
        requests.push(ClosureRequest::Instance {
            resolved,
            kind: InstanceKind::FormattingWrite,
            origin: origin.clone(),
            use_site: None,
        });
    }
    Ok((Plan::DynamicStructural(plan), requests))
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
        let request = InstanceResolutionRequest {
                function: *function,
                origin: origin.clone(),
                function_type: function_type.clone(),
                substitutions: super::worklist::trait_site_substitutions(
                    program,
                    plan.trait_id,
                    &plan.arguments,
                ),
                evidence: Some(evidence.clone()),
                target: InstanceResolutionTarget::Root,
            };
        let own_scope = method_runtime_scope(program, *method);
        let resolved = program.resolve_instance_request_in_scope(&request, &own_scope)
            .map_err(|error| vec![error])?;
        plan.methods.push(PlannedInstance {
            key: resolved.key.clone(),
            instance: None,
            kind: InstanceKind::TraitMethod,
        });
        let generic_key = resolved.key.clone();
        let generic_map = resolved.environment.substitution_map();
        method_requests.push(ClosureRequest::Instance {
            resolved,
            kind: InstanceKind::TraitMethod,
            origin: origin.clone(),
            use_site: None,
        });
        if !own_scope.is_empty() {
            let signature = match crate::substitute_type(CheckedType::Function(program.functions.get(*function).unwrap().signature.clone()), &generic_map) {
                CheckedType::Function(signature) => signature, _ => unreachable!(),
            };
            let dictionary_count = runtime_obligations(&program.semantic_ids,
                &program.functions.get(*function).unwrap().declared_bounds, &generic_map, generic_key.runtime()).len();
            let key = ArtifactRequestKey::TraitMemberEntry(crate::specialization::TraitMemberEntryKey::Slot(generic_key.clone()));
            plan.slots.push(PlannedArtifact { key: key.clone(), artifact: None, kind: Kind::TraitMemberEntry });
            requests.push(ClosureRequest::Artifact {
                key, plan: Plan::TraitMemberEntry(super::artifact_plan::TraitMemberEntryPlan {
                    method: *method, callee: Some(PlannedInstance { key: generic_key, instance: None, kind: InstanceKind::TraitMethod }),
                    function_type: signature, descriptors: own_scope.types.len(), dictionaries: dictionary_count,
                }), kind: Kind::TraitMemberEntry, origin: origin.clone(), use_site: None,
            });
            continue;
        }
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
