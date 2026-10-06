//! Owned plans for recursive concrete/dynamic value conversion.
use super::artifact_closure::{ClosureRequest, ExpansionResult};
use super::{LoweredArtifactDependencyKind as Kind, LoweredArtifactPlan as Plan, PlannedArtifact};
use crate::specialization::{
    ArtifactRequestKey, CanonicalFunctionType, CanonicalType, ReabstractionDirection as Direction,
    ReabstractionThunkKey, ValueConversionKey,
};
use crate::{CheckedType, Origin, ReabstractionThunkPlan, ValueConversionPlan};
use staple_syntax::Diagnostic;

pub(super) fn needs_conversion(concrete: &CheckedType, dynamic: &CheckedType) -> bool {
    match (concrete, dynamic) {
        (CheckedType::Function(_), CheckedType::Function(function)) => {
            crate::contains_type_parameter(&CheckedType::Function(function.clone()))
        }
        (CheckedType::Product(left), CheckedType::Product(right)) => left
            .elements
            .iter()
            .zip(&right.elements)
            .any(|(left, right)| needs_conversion(&left.value_type, &right.value_type)),
        (CheckedType::Sum(left), CheckedType::Sum(right)) => left
            .alternatives
            .iter()
            .zip(&right.alternatives)
            .any(|(left, right)| needs_conversion(left, right)),
        (
            CheckedType::Wrapper {
                representation: left,
                ..
            },
            CheckedType::Wrapper {
                representation: right,
                ..
            },
        ) => needs_conversion(left, right),
        (CheckedType::Array { element: left, .. }, CheckedType::Array { element: right, .. }) => {
            needs_conversion(left, right)
        }
        _ => false,
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
            concrete: CanonicalType::concrete(concrete, origin)?,
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
                            concrete: CanonicalFunctionType::concrete(concrete, origin)
                                .map_err(|error| vec![error])?,
                            dynamic: CanonicalFunctionType::dynamic(dynamic, origin)
                                .map_err(|error| vec![error])?,
                            direction: plan.direction,
                        }),
                        plan: Plan::ReabstractionThunk(ReabstractionThunkPlan {
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
                    for (left, right) in left.elements.iter().zip(&right.elements) {
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
                )
                | (
                    CheckedType::Array { element: left, .. },
                    CheckedType::Array { element: right, .. },
                ) => {
                    if needs_conversion(left, right) {
                        requests.push(
                            request(left, right, plan.direction, origin)
                                .map_err(|error| vec![error])?,
                        );
                    }
                }
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
