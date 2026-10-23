//! Owned plans for recursive concrete/dynamic value conversion.
use super::artifact_closure::{ClosureRequest, ExpansionResult};
use super::{LoweredArtifactDependencyKind as Kind, LoweredArtifactPlan as Plan, PlannedArtifact};
use crate::specialization::{
    ArtifactRequestKey, CanonicalFunctionType, CanonicalType, ReabstractionDirection as Direction,
    ReabstractionThunkKey, ValueConversionKey,
};
use crate::{CheckedType, Origin, ReabstractionThunkPlan, ValueConversionPlan};
use staple_syntax::Diagnostic;

/// One value an instantiation adapter converts between the concrete closure
/// ABI of its target and the dynamic shape of the scheme body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AdapterConversion {
    pub concrete: CheckedType,
    pub dynamic: CheckedType,
}

/// The conversions an instantiation adapter applies, one entry per argument
/// slot of its target signature and one for the result. The slots mirror the
/// adapter's own layout: a whole-parameter mutation is one slot, a product
/// packed into a bare type parameter converts nothing (the body sees an opaque
/// value), and otherwise the target's and the body's flattened parameters pair
/// up.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AdapterConversions {
    pub arguments: Vec<Option<AdapterConversion>>,
    pub result: Option<AdapterConversion>,
}

/// The adapter conversions for instantiating `body` (a scheme's body) at
/// `target`. A target type may mix the caller's run-time parameters with
/// concrete functions; its conversion then reads the layout from a
/// descriptor (see [`takes_descriptor`]).
pub(crate) fn adapter_conversions(
    target: &crate::CheckedFunctionType,
    body: &crate::CheckedFunctionType,
) -> Result<AdapterConversions, String> {
    let pair = |concrete: &CheckedType, dynamic: &CheckedType| {
        needs_conversion(concrete, dynamic).then(|| AdapterConversion {
            concrete: concrete.clone(),
            dynamic: dynamic.clone(),
        })
    };
    let whole = target.mutations.contains(&crate::CheckedMutation::Whole);
    let target_slots = super::flattened_parameter_types(&target.parameter);
    let pack = !whole
        && !matches!(body.parameter.as_ref(), CheckedType::Product(_))
        && matches!(target.parameter.as_ref(), CheckedType::Product(_));
    let arguments = if whole {
        vec![pair(&target.parameter, &body.parameter)]
    } else if pack {
        vec![None; target_slots.len()]
    } else {
        let body_slots = super::flattened_parameter_types(&body.parameter);
        if body_slots.len() != target_slots.len() {
            return Err("instantiation adapter parameter shapes disagree".to_owned());
        }
        target_slots
            .iter()
            .zip(&body_slots)
            .map(|(concrete, dynamic)| pair(concrete, dynamic))
            .collect()
    };
    Ok(AdapterConversions {
        arguments,
        result: pair(&target.result, &body.result),
    })
}

/// Whether a value converts between the two shapes: some function inside it
/// is concrete on one side and mentions run-time parameters on the other. A
/// function that mentions run-time parameters on both sides already has the
/// dynamic-shape ABI.
pub(crate) fn needs_conversion(concrete: &CheckedType, dynamic: &CheckedType) -> bool {
    super::needs_concrete_function_conversion(concrete, dynamic)
}

/// Whether a conversion of `concrete` reads its layout from a run-time
/// descriptor: its concrete side mixes the surrounding run-time parameters
/// with the concrete functions it converts. Such a conversion takes the
/// descriptor of its value as a third argument.
pub(crate) fn takes_descriptor(concrete: &CheckedType) -> bool {
    crate::contains_type_parameter(concrete)
}

/// The key of a conversion's concrete side. A side that mentions run-time
/// parameters keys by its symbolic shape.
pub(crate) fn concrete_key(
    concrete: &CheckedType,
    origin: &Origin,
) -> Result<CanonicalType, Diagnostic> {
    if takes_descriptor(concrete) {
        CanonicalType::dynamic(concrete, origin)
    } else {
        CanonicalType::concrete(concrete, origin)
    }
}

pub(super) fn request(
    concrete: &CheckedType,
    dynamic: &CheckedType,
    direction: Direction,
    origin: &Origin,
) -> Result<ClosureRequest, Diagnostic> {
    Ok(ClosureRequest::Artifact {
        key: ArtifactRequestKey::ValueConversion(ValueConversionKey {
            concrete: concrete_key(concrete, origin)?,
            dynamic: CanonicalType::dynamic(dynamic, origin)?,
            direction,
        }),
        plan: Plan::ValueConversion(ValueConversionPlan {
            concrete: concrete.clone(),
            dynamic: dynamic.clone(),
            direction,
            children: Vec::new(),
        }),
        kind: Kind::ValueConversion,
        origin: origin.clone(),
        use_site: None,
    })
}

fn planned(request: &ClosureRequest) -> PlannedArtifact {
    let ClosureRequest::Artifact { key, kind, .. } = request else {
        unreachable!()
    };
    PlannedArtifact {
        key: key.clone(),
        artifact: None,
        kind: *kind,
    }
}

pub(super) fn expand(plan: Plan, origin: &Origin) -> ExpansionResult {
    let mut requests = Vec::new();
    let plan = match plan {
        Plan::ValueConversion(mut plan) => {
            plan.children.clear();
            match (&plan.concrete, &plan.dynamic) {
                (CheckedType::Function(concrete), CheckedType::Function(dynamic))
                    if needs_conversion(&plan.concrete, &plan.dynamic) =>
                {
                    requests.push(ClosureRequest::Artifact {
                        key: ArtifactRequestKey::ReabstractionThunk(ReabstractionThunkKey {
                            dictionary_slot: None,
                            concrete: CanonicalFunctionType::concrete(concrete, origin)
                                .map_err(|error| vec![error])?,
                            dynamic: CanonicalFunctionType::dynamic(dynamic, origin)
                                .map_err(|error| vec![error])?,
                            direction: plan.direction,
                        }),
                        plan: Plan::ReabstractionThunk(ReabstractionThunkPlan {
                            dictionary_slot: None,
                            concrete: concrete.clone(),
                            dynamic: dynamic.clone(),
                            direction: plan.direction,
                            children: Vec::new(),
                        }),
                        kind: Kind::ReabstractionThunk,
                        origin: origin.clone(),
                        use_site: None,
                    });
                }
                (CheckedType::Product(left), CheckedType::Product(right)) => {
                    for (left, right) in left.iter_positions().zip(right.iter_positions()) {
                        if needs_conversion(&left.value_type, &right.value_type) {
                            requests.push(
                                request(
                                    &left.value_type,
                                    &right.value_type,
                                    plan.direction,
                                    origin,
                                )
                                .map_err(|error| vec![error])?,
                            );
                        }
                    }
                }
                (CheckedType::Sum(left), CheckedType::Sum(right)) => {
                    for (left, right) in left.alternatives.iter().zip(&right.alternatives) {
                        if needs_conversion(left, right) {
                            requests.push(
                                request(left, right, plan.direction, origin)
                                    .map_err(|error| vec![error])?,
                            );
                        }
                    }
                }
                (
                    CheckedType::Wrapper {
                        representation: left,
                        ..
                    },
                    CheckedType::Wrapper {
                        representation: right,
                        ..
                    },
                ) => {}
                _ => {}
            }
            plan.children = requests.iter().map(planned).collect();
            Plan::ValueConversion(plan)
        }
        Plan::ReabstractionThunk(mut plan) => {
            let inverse = match plan.direction {
                Direction::ConcreteToDynamic => Direction::DynamicToConcrete,
                Direction::DynamicToConcrete => Direction::ConcreteToDynamic,
            };
            requests.push(
                request(
                    &plan.concrete.parameter,
                    &plan.dynamic.parameter,
                    inverse,
                    origin,
                )
                .map_err(|error| vec![error])?,
            );
            requests.push(
                request(
                    &plan.concrete.result,
                    &plan.dynamic.result,
                    plan.direction,
                    origin,
                )
                .map_err(|error| vec![error])?,
            );
            requests.push(
                request(
                    &plan.concrete.parameter,
                    &plan.dynamic.parameter,
                    plan.direction,
                    origin,
                )
                .map_err(|error| vec![error])?,
            );
            plan.children = requests.iter().map(planned).collect();
            Plan::ReabstractionThunk(plan)
        }
        _ => {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                "value-conversion expansion received a mismatched plan",
            )]);
        }
    };
    Ok((plan, requests))
}
