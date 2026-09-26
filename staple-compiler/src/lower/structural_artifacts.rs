//! Stage 4.3: the constructor-adapter and structural-method artifact
//! expanders.
//!
//! Each expander reads one artifact's request-time plan, derives the owned
//! body from lowered metadata (never `TypedModule`), and returns the finished
//! plan plus the closure requests the body needs. Bodies record concrete
//! checked types and catalog identities only; LLVM layout stays in the
//! backend.
//!
//! Constructor adapters are implemented in Step 2; the seven structural
//! method kinds and the nested trait-method selection follow in Steps 3-4.

use std::collections::HashMap;

use staple_syntax::{Diagnostic, Span};

use super::artifact_closure::{ClosureRequest, ExpansionResult};
use super::instance_resolution::{
    InstanceResolutionRequest, InstanceResolutionTarget, SubstitutionEnvironment,
};
use super::worklist::instantiate_method_type;
use super::{
    ArenaId, CallSubstitutions, CallTypeSubstitution, CheckedProductType, ConstructorAdapterPlan,
    ConstructorConstruction, DebugDelegate, DebugStep, DropGluePlan, GcFinalizerPlan,
    IndexedElement, LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId,
    LoweredInstanceDependencyKind, LoweredProgram, LoweredTraitImplementationId, Origin,
    PlannedArtifact, PlannedCallee, PlannedInstance, StructuralBody, StructuralMethodPlan,
    SumAlternative, TraitDelegate, TraitEvidence,
};
use crate::specialization::{
    ArtifactRequestKey, CanonicalType, GcFinalizerKey, StructuralMethodKey,
};
use crate::{
    CheckedFunctionType, CheckedType, CheckedTypeElement, RecursiveConstruction,
    StructuralTraitMethod, TraitId, TraitMethodId, infer_type_parameters,
};

/// Expands one constructor-adapter artifact: the flattened parameter slots,
/// the product the adapter rebuilds, and either ordinary wrapping or a managed
/// `Ref` allocation with its payload finalizer.
pub(super) fn expand_constructor_adapter(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: ConstructorAdapterPlan,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "constructor-adapter expansion received a missing artifact".to_string(),
            )]);
        }
    };
    let (parameters, variadic) = flatten_parameters(&plan.callable_type.parameter);
    let product = build_product(&parameters, variadic);
    let declared = program
        .types
        .get(plan.type_id)
        .and_then(|metadata| metadata.recursive_construction);
    let mut requests = Vec::new();
    let construction = match &*plan.callable_type.result {
        CheckedType::Ref(payload) => {
            if declared != Some(RecursiveConstruction::ManagedReference) {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "constructor adapter for symbol {} returns a managed reference but its lowered type is not a managed-reference construction",
                        plan.symbol.0
                    ),
                )]);
            }
            let payload = payload.as_ref().clone();
            let finalizer = if program.concrete_needs_drop(&payload) {
                let canonical = match CanonicalType::concrete(&payload, &origin) {
                    Ok(canonical) => canonical,
                    Err(diagnostic) => return Err(vec![diagnostic]),
                };
                let key = ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Payload(canonical));
                requests.push(ClosureRequest::Artifact {
                    key: key.clone(),
                    plan: LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Payload {
                        value_type: payload.clone(),
                    }),
                    kind: LoweredArtifactDependencyKind::GcFinalizer,
                    origin: origin.clone(),
                    use_site: None,
                });
                Some(PlannedArtifact {
                    key,
                    artifact: None,
                    kind: LoweredArtifactDependencyKind::GcFinalizer,
                })
            } else {
                None
            };
            ConstructorConstruction::ManagedRef {
                parameters,
                product,
                payload,
                finalizer,
            }
        }
        _ => {
            if declared == Some(RecursiveConstruction::ManagedReference) {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "constructor adapter for symbol {} does not return a managed reference but its lowered type is a managed-reference construction",
                        plan.symbol.0
                    ),
                )]);
            }
            ConstructorConstruction::Value {
                parameters,
                product,
            }
        }
    };
    Ok((
        LoweredArtifactPlan::ConstructorAdapter(ConstructorAdapterPlan {
            construction,
            ..plan
        }),
        requests,
    ))
}

/// Expands one structural-method artifact. Step 3 implements `Index`,
/// `MutateIndex`, `IntoIterator`, and `Iterator.next`; `Debug`, `DerefIndex`,
/// and `DerefMutateIndex` keep the request-time marker until Step 4.
pub(super) fn expand_structural_method(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: StructuralMethodPlan,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "structural-method expansion received a missing artifact".to_string(),
            )]);
        }
    };
    let mut requests = Vec::new();
    let body = match plan.structural {
        StructuralTraitMethod::Index => match structural_index_body(&plan, &origin) {
            Ok(body) => body,
            Err(diagnostic) => return Err(vec![diagnostic]),
        },
        StructuralTraitMethod::MutateIndex => {
            match structural_mutate_body(program, &plan, &origin) {
                Ok((body, request)) => {
                    if let Some(request) = request {
                        requests.push(request);
                    }
                    body
                }
                Err(diagnostic) => return Err(vec![diagnostic]),
            }
        }
        StructuralTraitMethod::IntoIterator => {
            match structural_into_iterator_body(&plan, &origin) {
                Ok(body) => body,
                Err(diagnostic) => return Err(vec![diagnostic]),
            }
        }
        StructuralTraitMethod::Iterator => match structural_next_body(&plan, &origin) {
            Ok(body) => body,
            Err(diagnostic) => return Err(vec![diagnostic]),
        },
        StructuralTraitMethod::DerefIndex => {
            match structural_deref_index_body(program, &plan, &origin) {
                Ok((body, selected_requests)) => {
                    requests.extend(selected_requests);
                    body
                }
                Err(diagnostic) => return Err(vec![diagnostic]),
            }
        }
        StructuralTraitMethod::DerefMutateIndex => {
            match structural_deref_mutate_body(program, &plan, &origin) {
                Ok((body, selected_requests)) => {
                    requests.extend(selected_requests);
                    body
                }
                Err(diagnostic) => return Err(vec![diagnostic]),
            }
        }
        StructuralTraitMethod::Debug => match structural_debug_body(program, &plan, &origin) {
            Ok((body, selected_requests)) => {
                requests.extend(selected_requests);
                body
            }
            Err(diagnostic) => return Err(vec![diagnostic]),
        },
    };
    Ok((
        LoweredArtifactPlan::StructuralMethod(StructuralMethodPlan { body, ..plan }),
        requests,
    ))
}

/// One concrete trait-method selection: the completed arguments, the planned
/// callee, the concrete method type, and the closure request that interns the
/// callee.
pub(super) struct SelectedTraitMethod {
    pub arguments: Vec<CheckedType>,
    pub callee: PlannedCallee,
    pub callee_type: CheckedFunctionType,
    pub request: ClosureRequest,
}

/// Selects one concrete trait method against the owned catalogs, mirroring
/// `TypedModule::trait_impl_method` precedence: an explicit implementation
/// first (with generic implementations resolved from the implementation
/// header), structural derivation otherwise. Any divergence from legacy
/// `trait_method_code` is a resolver bug, not an expander workaround.
pub(super) fn select_concrete_trait_method(
    program: &LoweredProgram,
    origin: &Origin,
    trait_id: TraitId,
    method: TraitMethodId,
    arguments: &[CheckedType],
) -> Result<SelectedTraitMethod, Diagnostic> {
    let evidence = TraitEvidence::DeclaredBound {
        trait_id,
        method: Some(method),
        arguments: arguments.to_vec(),
        prerequisites: Vec::new(),
    };
    let environment = SubstitutionEnvironment::default();
    let Some(resolved) = program.resolve_trait_evidence(origin, Some(&evidence), &environment)?
    else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            format!(
                "trait method selection for trait {} and arguments {arguments:?} did not resolve",
                trait_id.0
            ),
        ));
    };
    let completed = match &resolved {
        TraitEvidence::ExplicitImplementation { arguments, .. }
        | TraitEvidence::Structural { arguments, .. } => arguments.clone(),
        TraitEvidence::DeclaredBound { trait_id, .. }
        | TraitEvidence::RejectedImplementation { trait_id, .. } => {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!(
                    "trait {} evidence did not resolve to a concrete selection",
                    trait_id.0
                ),
            ));
        }
    };
    let callee_type = instantiate_method_type(program, origin, trait_id, method, &completed)?;
    match &resolved {
        TraitEvidence::ExplicitImplementation {
            function,
            implementation,
            ..
        } => {
            let substitutions =
                implementation_substitutions(program, origin, *implementation, &completed)?;
            let evidence = if program.relevant_parameters(*function).is_empty() {
                None
            } else {
                Some(resolved.clone())
            };
            let request = InstanceResolutionRequest {
                function: *function,
                origin: origin.clone(),
                function_type: callee_type.clone(),
                substitutions,
                evidence,
                target: InstanceResolutionTarget::Root,
            };
            let resolved_instance = program.resolve_instance_request(&request)?;
            let callee = PlannedCallee::Instance(PlannedInstance {
                key: resolved_instance.key.clone(),
                instance: None,
                kind: LoweredInstanceDependencyKind::TraitMethod,
            });
            Ok(SelectedTraitMethod {
                arguments: completed,
                callee,
                callee_type,
                request: ClosureRequest::Instance {
                    resolved: resolved_instance,
                    kind: LoweredInstanceDependencyKind::TraitMethod,
                    origin: origin.clone(),
                    use_site: None,
                },
            })
        }
        TraitEvidence::Structural {
            structural,
            arguments,
            ..
        } => {
            let key = StructuralMethodKey::new(
                *structural,
                trait_id,
                method,
                arguments,
                &callee_type,
                origin,
            )?;
            let key = ArtifactRequestKey::StructuralMethod(key);
            let callee = PlannedCallee::Artifact(PlannedArtifact {
                key: key.clone(),
                artifact: None,
                kind: LoweredArtifactDependencyKind::StructuralMethod,
            });
            Ok(SelectedTraitMethod {
                arguments: completed,
                callee,
                callee_type: callee_type.clone(),
                request: ClosureRequest::Artifact {
                    key,
                    plan: LoweredArtifactPlan::StructuralMethod(StructuralMethodPlan {
                        structural: *structural,
                        trait_id,
                        method,
                        arguments: arguments.clone(),
                        callable_type: callee_type,
                        body: StructuralBody::Unexpanded,
                    }),
                    kind: LoweredArtifactDependencyKind::StructuralMethod,
                    origin: origin.clone(),
                    use_site: None,
                },
            })
        }
        TraitEvidence::DeclaredBound { .. } | TraitEvidence::RejectedImplementation { .. } => {
            unreachable!("checked above")
        }
    }
}

/// The method instance's substitutions from its implementation header: match
/// the implementation's declared trait arguments against the completed ones to
/// recover parameters that the method's own signature does not mention.
fn implementation_substitutions(
    program: &LoweredProgram,
    origin: &Origin,
    implementation: LoweredTraitImplementationId,
    completed: &[CheckedType],
) -> Result<CallSubstitutions, Diagnostic> {
    let Some(metadata) = program.trait_implementations.get(implementation) else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            format!(
                "trait implementation {} is missing from the lowered catalog",
                implementation.index()
            ),
        ));
    };
    let mut inferred = HashMap::new();
    for (declared, actual) in metadata.arguments.iter().zip(completed) {
        if !infer_type_parameters(declared, actual, &mut inferred) {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!(
                    "cannot infer implementation {} parameters from completed arguments {completed:?}",
                    implementation.index()
                ),
            ));
        }
    }
    let types = metadata
        .parameters
        .iter()
        .filter_map(|parameter| {
            inferred
                .get(parameter)
                .cloned()
                .map(|value_type| CallTypeSubstitution {
                    parameter: *parameter,
                    value_type,
                })
        })
        .collect();
    Ok(CallSubstitutions {
        types,
        effects: Vec::new(),
    })
}

/// The standard trait's declared first method, from semantic IDs and the owned
/// trait catalog.
fn standard_trait_method(
    program: &LoweredProgram,
    trait_id: Option<TraitId>,
    origin: &Origin,
    name: &str,
) -> Result<(TraitId, TraitMethodId), Diagnostic> {
    let Some(trait_id) = trait_id else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            format!("standard `{name}` trait is missing from the lowered catalog"),
        ));
    };
    let method = program
        .traits
        .get(trait_id)
        .and_then(|metadata| metadata.methods.first().copied())
        .ok_or_else(|| {
            Diagnostic::new(
                origin.span.clone(),
                format!("standard `{name}` trait has no declared method"),
            )
        })?;
    Ok((trait_id, method))
}

/// `DerefIndex`: a non-variadic homogeneous `Copy` product loads directly
/// through the reference; every other payload delegates to its own `Index`.
fn structural_deref_index_body(
    program: &LoweredProgram,
    plan: &StructuralMethodPlan,
    origin: &Origin,
) -> Result<(StructuralBody, Vec<ClosureRequest>), Diagnostic> {
    let target = structural_argument(plan, 0, origin)?;
    let position = structural_argument(plan, 1, origin)?;
    let output = structural_argument(plan, 2, origin)?;
    let CheckedType::Ref(payload) = &target else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            "structural DerefIndex target is not a reference".to_string(),
        ));
    };
    let payload = payload.as_ref().clone();
    if let CheckedType::Product(product) = &payload
        && !product.variadic
        && let Some(element) = product.homogeneous_element()
        && program.concrete_is_copy(element)
    {
        return Ok((
            StructuralBody::DerefIndexLoad {
                element: element.clone(),
                length: product.elements.len(),
                output,
            },
            Vec::new(),
        ));
    }
    let (trait_id, method) =
        standard_trait_method(program, program.semantic_ids.index_trait, origin, "Index")?;
    let selected = select_concrete_trait_method(
        program,
        origin,
        trait_id,
        method,
        &[payload.clone(), position, output],
    )?;
    let delegate = TraitDelegate {
        trait_id,
        method,
        arguments: selected.arguments,
        callee: selected.callee,
        callee_type: selected.callee_type,
    };
    Ok((
        StructuralBody::DerefDelegate { payload, delegate },
        vec![selected.request],
    ))
}

/// `DerefMutateIndex`: the referenced address is the payload's storage, so it
/// always delegates to the payload's own `MutateIndex`.
fn structural_deref_mutate_body(
    program: &LoweredProgram,
    plan: &StructuralMethodPlan,
    origin: &Origin,
) -> Result<(StructuralBody, Vec<ClosureRequest>), Diagnostic> {
    let target = structural_argument(plan, 0, origin)?;
    let position = structural_argument(plan, 1, origin)?;
    let element = structural_argument(plan, 2, origin)?;
    let CheckedType::Ref(payload) = &target else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            "structural DerefMutateIndex target is not a reference".to_string(),
        ));
    };
    let payload = payload.as_ref().clone();
    let (trait_id, method) = standard_trait_method(
        program,
        program.semantic_ids.mutate_index_trait,
        origin,
        "MutateIndex",
    )?;
    let selected = select_concrete_trait_method(
        program,
        origin,
        trait_id,
        method,
        &[payload.clone(), position, element],
    )?;
    let delegate = TraitDelegate {
        trait_id,
        method,
        arguments: selected.arguments,
        callee: selected.callee,
        callee_type: selected.callee_type,
    };
    Ok((
        StructuralBody::DerefDelegate { payload, delegate },
        vec![selected.request],
    ))
}

/// `Debug` for a product (ordered literal/element steps plus the shared
/// `Formatter.write` instance) or a sum (one delegate per alternative in tag
/// order, no literals).
fn structural_debug_body(
    program: &LoweredProgram,
    plan: &StructuralMethodPlan,
    origin: &Origin,
) -> Result<(StructuralBody, Vec<ClosureRequest>), Diagnostic> {
    let (trait_id, method) =
        standard_trait_method(program, program.semantic_ids.debug_trait, origin, "Debug")?;
    let target = structural_argument(plan, 0, origin)?;
    match &target {
        CheckedType::Sum(sum) => {
            let mut requests = Vec::new();
            let mut alternatives = Vec::new();
            for alternative in &sum.alternatives {
                let selected = select_concrete_trait_method(
                    program,
                    origin,
                    trait_id,
                    method,
                    std::slice::from_ref(alternative),
                )?;
                requests.push(selected.request);
                alternatives.push(DebugDelegate {
                    value_type: alternative.clone(),
                    callee: selected.callee,
                    callee_type: selected.callee_type,
                });
            }
            Ok((StructuralBody::SumDebug { alternatives }, requests))
        }
        CheckedType::Product(product) => {
            let Some(write_function) = program.string_formatting.write else {
                return Err(Diagnostic::new(
                    origin.span.clone(),
                    "Formatter.write is unavailable for structural Debug".to_string(),
                ));
            };
            let signature = program
                .functions
                .get(write_function)
                .map(|function| function.signature.clone())
                .ok_or_else(|| {
                    Diagnostic::new(
                        origin.span.clone(),
                        format!(
                            "Formatter.write function {} has no lowered template",
                            write_function.0
                        ),
                    )
                })?;
            let resolved_write = program.resolve_instance_request(&InstanceResolutionRequest {
                function: write_function,
                origin: origin.clone(),
                function_type: signature,
                substitutions: CallSubstitutions::default(),
                evidence: None,
                target: InstanceResolutionTarget::Root,
            })?;
            let write = PlannedInstance {
                key: resolved_write.key.clone(),
                instance: None,
                kind: LoweredInstanceDependencyKind::FormattingWrite,
            };
            let mut requests = vec![ClosureRequest::Instance {
                resolved: resolved_write,
                kind: LoweredInstanceDependencyKind::FormattingWrite,
                origin: origin.clone(),
                use_site: None,
            }];
            let mut steps = vec![DebugStep::Write("(".to_string())];
            for (index, element) in product.elements.iter().enumerate() {
                if index != 0 {
                    steps.push(DebugStep::Write(", ".to_string()));
                }
                if let Some(name) = &element.name {
                    steps.push(DebugStep::Write(name.clone()));
                    steps.push(DebugStep::Write(": ".to_string()));
                }
                let selected = select_concrete_trait_method(
                    program,
                    origin,
                    trait_id,
                    method,
                    std::slice::from_ref(&element.value_type),
                )?;
                requests.push(selected.request);
                steps.push(DebugStep::Element {
                    index,
                    delegate: DebugDelegate {
                        value_type: element.value_type.clone(),
                        callee: selected.callee,
                        callee_type: selected.callee_type,
                    },
                });
            }
            steps.push(DebugStep::Write(")".to_string()));
            Ok((StructuralBody::ProductDebug { steps, write }, requests))
        }
        other => Err(Diagnostic::new(
            origin.span.clone(),
            format!("structural Debug requires a product or sum, got {other}"),
        )),
    }
}

/// The concrete completed trait argument at `index`.
fn structural_argument(
    plan: &StructuralMethodPlan,
    index: usize,
    origin: &Origin,
) -> Result<CheckedType, Diagnostic> {
    plan.arguments.get(index).cloned().ok_or_else(|| {
        Diagnostic::new(
            origin.span.clone(),
            format!(
                "structural {:?} artifact is missing completed argument {index}",
                plan.structural
            ),
        )
    })
}

/// Records a coercion as `(from, to)` only when the types differ.
fn recorded_coercion(from: &CheckedType, to: &CheckedType) -> Option<(CheckedType, CheckedType)> {
    (from != to).then(|| (from.clone(), to.clone()))
}

/// `Index`: a heterogeneous product switches per element and coerces each into
/// the output; a homogeneous product loads the element directly.
fn structural_index_body(
    plan: &StructuralMethodPlan,
    origin: &Origin,
) -> Result<StructuralBody, Diagnostic> {
    let target = structural_argument(plan, 0, origin)?;
    let output = structural_argument(plan, 2, origin)?;
    let CheckedType::Product(product) = &target else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            "structural Index target is not a product".to_string(),
        ));
    };
    if product.homogeneous_element().is_none() {
        let elements = product
            .elements
            .iter()
            .enumerate()
            .map(|(index, element)| IndexedElement {
                index,
                element: element.value_type.clone(),
                coercion: recorded_coercion(&element.value_type, &output),
            })
            .collect();
        Ok(StructuralBody::IndexSwitch { elements, output })
    } else {
        let element = product
            .homogeneous_element()
            .expect("checked above")
            .clone();
        Ok(StructuralBody::IndexLoad {
            element,
            length: product.elements.len(),
            output,
        })
    }
}

/// `MutateIndex`: bounds trap, drop the replaced element when it needs drop,
/// then store. Returns the body and the drop-glue request it needs.
fn structural_mutate_body(
    program: &LoweredProgram,
    plan: &StructuralMethodPlan,
    origin: &Origin,
) -> Result<(StructuralBody, Option<ClosureRequest>), Diagnostic> {
    let target = structural_argument(plan, 0, origin)?;
    let element = structural_argument(plan, 2, origin)?;
    let CheckedType::Product(product) = &target else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            "structural MutateIndex target is not a product".to_string(),
        ));
    };
    let (drop_previous, request) = if program.concrete_needs_drop(&element) {
        let canonical = CanonicalType::concrete(&element, origin)?;
        let key = ArtifactRequestKey::DropGlue(canonical);
        let request = ClosureRequest::Artifact {
            key: key.clone(),
            plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
                value_type: element.clone(),
            }),
            kind: LoweredArtifactDependencyKind::DropGlue,
            origin: origin.clone(),
            use_site: None,
        };
        (
            Some(PlannedArtifact {
                key,
                artifact: None,
                kind: LoweredArtifactDependencyKind::DropGlue,
            }),
            Some(request),
        )
    } else {
        (None, None)
    };
    Ok((
        StructuralBody::MutateReplace {
            element,
            length: product.elements.len(),
            drop_previous,
        },
        request,
    ))
}

/// `IntoIterator`: the source product paired with cursor `0`.
fn structural_into_iterator_body(
    plan: &StructuralMethodPlan,
    origin: &Origin,
) -> Result<StructuralBody, Diagnostic> {
    let source = structural_argument(plan, 0, origin)?;
    let iterator = CheckedType::Product(CheckedProductType {
        elements: vec![
            CheckedTypeElement {
                name: None,
                value_type: source.clone(),
                default: None,
            },
            CheckedTypeElement {
                name: None,
                value_type: CheckedType::USize,
                default: None,
            },
        ],
        variadic: false,
    });
    if let Some(recorded) = plan.arguments.get(1)
        && recorded != &iterator
    {
        return Err(Diagnostic::new(
            origin.span.clone(),
            "structural IntoIterator's completed iterator argument is not the derived `(source, USize)` pair"
                .to_string(),
        ));
    }
    Ok(StructuralBody::IntoIterator { source, iterator })
}

/// `Iterator.next`: the per-element coercions into the item, plus the `Done`
/// and `Yield` alternatives resolved from the result sum's representations.
fn structural_next_body(
    plan: &StructuralMethodPlan,
    origin: &Origin,
) -> Result<StructuralBody, Diagnostic> {
    let iterator = structural_argument(plan, 0, origin)?;
    let item = structural_argument(plan, 1, origin)?;
    let result = plan.callable_type.result.as_ref().clone();
    let CheckedType::Product(iter_product) = &iterator else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            "structural Iterator target is not a `(product, USize)` pair".to_string(),
        ));
    };
    let CheckedType::Product(product) = &iter_product.elements[0].value_type else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            "structural Iterator target does not contain a product".to_string(),
        ));
    };
    let CheckedType::Sum(sum) = &result else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            "structural Iterator result is not a sum".to_string(),
        ));
    };
    let sum = sum.clone();
    let yield_representation = CheckedType::Product(CheckedProductType {
        elements: vec![
            CheckedTypeElement {
                name: None,
                value_type: item.clone(),
                default: None,
            },
            CheckedTypeElement {
                name: None,
                value_type: iterator.clone(),
                default: None,
            },
        ],
        variadic: false,
    });
    let mut done = Vec::new();
    let mut yields = Vec::new();
    for (index, alternative) in sum.alternatives.iter().enumerate() {
        if let CheckedType::Distinct { representation, .. } = alternative {
            if representation.as_ref() == &iterator {
                done.push(index);
            }
            if representation.as_ref() == &yield_representation {
                yields.push(index);
            }
        }
    }
    let done = resolved_alternative(&done, "`IterStep.Done`", origin)?;
    let yield_ = resolved_alternative(&yields, "`IterStep.Yield`", origin)?;
    let elements = product
        .elements
        .iter()
        .enumerate()
        .map(|(index, element)| IndexedElement {
            index,
            element: element.value_type.clone(),
            coercion: recorded_coercion(&element.value_type, &item),
        })
        .collect();
    Ok(StructuralBody::Next {
        product: CheckedType::Product(product.clone()),
        iterator,
        item,
        elements,
        result,
        done: SumAlternative {
            index: done,
            alternative: sum.alternatives[done].clone(),
        },
        yield_: SumAlternative {
            index: yield_,
            alternative: sum.alternatives[yield_].clone(),
        },
    })
}

/// Requires exactly one representation-matching alternative.
fn resolved_alternative(
    indexes: &[usize],
    name: &str,
    origin: &Origin,
) -> Result<usize, Diagnostic> {
    match indexes {
        [index] => Ok(*index),
        [] => Err(Diagnostic::new(
            origin.span.clone(),
            format!("structural Iterator result has no {name} alternative"),
        )),
        _ => Err(Diagnostic::new(
            origin.span.clone(),
            format!("structural Iterator result has an ambiguous {name} alternative"),
        )),
    }
}

/// The flattened parameter slot types the closure ABI exposes, mirroring
/// `flattened_parameter_types`/`compile_parameter_types`: a top-level product
/// parameter inlines its elements, and a single slot stays the parameter's own
/// type.
fn flatten_parameters(parameter: &CheckedType) -> (Vec<CheckedType>, bool) {
    match parameter {
        CheckedType::Product(product) => (
            product
                .elements
                .iter()
                .map(|element| element.value_type.clone())
                .collect(),
            product.variadic,
        ),
        other => (vec![other.clone()], false),
    }
}

/// The value the adapter rebuilds from its flattened slots, mirroring
/// `build_product_value`: one slot is that value unchanged and every other
/// arity is an anonymous product in slot order.
fn build_product(parameters: &[CheckedType], variadic: bool) -> CheckedType {
    match parameters {
        [single] => single.clone(),
        _ => CheckedType::Product(CheckedProductType {
            elements: parameters
                .iter()
                .map(|value_type| CheckedTypeElement {
                    name: None,
                    value_type: value_type.clone(),
                    default: None,
                })
                .collect(),
            variadic,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{
        CheckedProductType, CheckedType, CheckedTypeElement, LoweredModule, Lowerer, NameResolver,
        ProgramLoader, TypeChecker, TypedModule,
    };

    use super::super::ArenaId;
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

    fn lower(source: &str) -> (TypedModule, LoweredModule) {
        let module = checked_program(source);
        let lowered = Lowerer::new().lower(&module).expect("source lowers");
        (module, lowered)
    }

    fn constructor_plans(lowered: &LoweredModule) -> Vec<(usize, &ConstructorAdapterPlan)> {
        lowered
            .program
            .artifacts
            .iter()
            .filter_map(|(id, artifact)| match artifact.plan.as_ref() {
                Some(LoweredArtifactPlan::ConstructorAdapter(plan)) => Some((id.index(), plan)),
                _ => None,
            })
            .collect()
    }

    fn positional_product(elements: &[CheckedType]) -> CheckedType {
        CheckedType::Product(CheckedProductType {
            elements: elements
                .iter()
                .map(|value_type| CheckedTypeElement {
                    name: None,
                    value_type: value_type.clone(),
                    default: None,
                })
                .collect(),
            variadic: false,
        })
    }

    #[test]
    fn ordinary_constructor_value_plans_its_flattened_parameters() {
        let (_, lowered) = lower(concat!(
            "type Point = ctor (I32, I32)\n",
            "let make: () -> ((I32, I32) -> Point) = () => Point\n",
        ));
        let plans = constructor_plans(&lowered);
        assert_eq!(plans.len(), 1, "one constructor adapter");
        let ConstructorConstruction::Value {
            parameters,
            product,
        } = &plans[0].1.construction
        else {
            panic!("an ordinary nominal constructor wraps its product");
        };
        assert_eq!(parameters, &[CheckedType::I32, CheckedType::I32]);
        assert_eq!(
            product,
            &positional_product(&[CheckedType::I32, CheckedType::I32])
        );
    }

    #[test]
    fn managed_ref_constructor_plans_a_finalizer_exactly_when_drop_is_needed() {
        let (module, lowered) = lower(concat!(
            "type Resource = ctor I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
            "let make: () -> (Resource -> Ref Resource) = () => Ref\n",
            "let make_copy: () -> (I32 -> Ref I32) = () => Ref\n",
        ));
        let plans = constructor_plans(&lowered);
        assert_eq!(plans.len(), 2, "one managed-ref adapter per payload type");
        let mut saw_droppable = false;
        let mut saw_copy = false;
        for (_, plan) in &plans {
            let ConstructorConstruction::ManagedRef {
                parameters,
                product,
                payload,
                finalizer,
            } = &plan.construction
            else {
                panic!("a `Ref` constructor result classifies as ManagedRef");
            };
            // The adapter rebuilds the payload from its flattened slots.
            assert_eq!(product, payload);
            assert_eq!(parameters.len(), 1);
            let expected_finalizer = module.type_needs_drop(payload);
            let legacy_is_copy = module.is_copy_type(payload);
            assert_eq!(
                finalizer.is_some(),
                expected_finalizer,
                "the planned finalizer matches the legacy `build_ref_value` predicate"
            );
            if expected_finalizer {
                saw_droppable = true;
                assert!(!legacy_is_copy, "a droppable payload is never `Copy`");
            } else {
                saw_copy = true;
            }
        }
        assert!(saw_droppable, "the fixture exercises a droppable payload");
        assert!(saw_copy, "the fixture exercises a `Copy` payload");
    }

    #[test]
    fn generic_constructor_values_produce_one_adapter_per_instantiation() {
        let (_, lowered) = lower(concat!(
            "def ref_maker: <T where Copy T> () -> (T -> Ref T) = () => Ref\n",
            "let maker_i32: I32 -> Ref I32 = ref_maker ()\n",
            "let maker_u8: U8 -> Ref U8 = ref_maker ()\n",
        ));
        let plans = constructor_plans(&lowered);
        assert_eq!(plans.len(), 2, "one adapter per concrete callable type");
        let mut payloads = plans
            .iter()
            .filter_map(|(_, plan)| match &plan.construction {
                ConstructorConstruction::ManagedRef { payload, .. } => Some(payload.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        payloads.sort_by_key(|payload| format!("{payload:?}"));
        assert_eq!(payloads, vec![CheckedType::I32, CheckedType::U8]);
    }

    fn structural_plans(
        lowered: &LoweredModule,
        kind: StructuralTraitMethod,
    ) -> Vec<&StructuralMethodPlan> {
        lowered
            .program
            .artifacts
            .iter()
            .filter_map(|(_, artifact)| match artifact.plan.as_ref() {
                Some(LoweredArtifactPlan::StructuralMethod(plan)) if plan.structural == kind => {
                    Some(plan)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn index_switches_heterogeneous_products_and_loads_homogeneous_ones() {
        let (_, lowered) = lower(concat!(
            "def index_mixed: (U8, I32) -> (I32 | U8) = pair => pair[0]\n",
            "def index_uniform: (I32, I32) -> I32 = pair => pair[0]\n",
            "let mixed = index_mixed ((1 satisfies U8), 2)\n",
            "let uniform = index_uniform (1, 2)\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::Index);
        assert_eq!(plans.len(), 2, "one Index plan per product shape");
        let mut switch = None;
        let mut load = None;
        for plan in &plans {
            match &plan.body {
                StructuralBody::IndexSwitch { elements, output } => {
                    assert_eq!(elements.len(), 2);
                    assert_eq!(elements[0].element, CheckedType::U8);
                    assert_eq!(elements[1].element, CheckedType::I32);
                    // Every heterogeneous element coerces into the sum output.
                    for element in elements {
                        assert_eq!(
                            element.coercion,
                            Some((element.element.clone(), output.clone()))
                        );
                    }
                    assert!(matches!(output, CheckedType::Sum(_)), "{output:?}");
                    switch = Some(plan);
                }
                StructuralBody::IndexLoad {
                    element,
                    length,
                    output,
                } => {
                    assert_eq!(element, &CheckedType::I32);
                    assert_eq!(*length, 2);
                    assert_eq!(
                        output,
                        &CheckedType::I32,
                        "a homogeneous load needs no coercion"
                    );
                    load = Some(plan);
                }
                other => panic!("unexpected Index body {other:?}"),
            }
        }
        assert!(
            switch.is_some(),
            "heterogeneous indexing switches per element"
        );
        assert!(
            load.is_some(),
            "homogeneous indexing loads the element directly"
        );
    }

    #[test]
    fn mutate_index_records_drop_previous_exactly_when_drop_is_needed() {
        let (module, lowered) = lower(concat!(
            "type Resource = ctor I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
            "def mutate_copy: (I32, I32) -> (I32, I32) = pair => {\n",
            "  let mut copy = pair\n",
            "  copy[0] = 3\n",
            "  copy\n",
            "}\n",
            "def mutate_resource: move (Resource, Resource) -> (Resource, Resource) = move pair => {\n",
            "  let mut copy = pair\n",
            "  copy[0] = Resource 3\n",
            "  copy\n",
            "}\n",
            "let copied = mutate_copy (1, 2)\n",
            "let replaced = mutate_resource (Resource 1, Resource 2)\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::MutateIndex);
        assert_eq!(plans.len(), 2, "one MutateIndex plan per element type");
        for plan in &plans {
            let StructuralBody::MutateReplace {
                element,
                length,
                drop_previous,
            } = &plan.body
            else {
                panic!("MutateIndex plans a replacement: {:?}", plan.body);
            };
            assert_eq!(*length, 2);
            assert_eq!(
                drop_previous.is_some(),
                module.type_needs_drop(element),
                "the planned drop matches the legacy `type_needs_drop` predicate for {element:?}"
            );
            if let Some(drop_previous) = drop_previous {
                let artifact = lowered
                    .program
                    .artifacts
                    .get(LoweredArtifactRequestId::from_index(
                        drop_previous.artifact.expect("bound after closure").index(),
                    ))
                    .expect("drop-glue artifact");
                assert_eq!(
                    lowered.program.specializations.artifact(artifact.ordinal),
                    Some(&drop_previous.key)
                );
            }
        }
    }

    #[test]
    fn iterator_plans_record_the_derived_shape_and_alternatives() {
        let (_, lowered) = lower(concat!(
            "def count_pair: (U8, I32) -> I32 = pair => {\n",
            "  let mut count = 0\n",
            "  for item in pair { count = count + 1 }\n",
            "  count\n",
            "}\n",
            "let counted = count_pair ((1 satisfies U8), 2)\n",
        ));
        let into_iterator = structural_plans(&lowered, StructuralTraitMethod::IntoIterator);
        assert_eq!(into_iterator.len(), 1);
        let StructuralBody::IntoIterator { source, iterator } = &into_iterator[0].body else {
            panic!(
                "IntoIterator plans the derived pair: {:?}",
                into_iterator[0].body
            );
        };
        assert_eq!(
            source,
            &positional_product(&[CheckedType::U8, CheckedType::I32])
        );
        assert_eq!(
            iterator,
            &positional_product(&[source.clone(), CheckedType::USize])
        );

        let next = structural_plans(&lowered, StructuralTraitMethod::Iterator);
        assert_eq!(next.len(), 1);
        let StructuralBody::Next {
            product,
            iterator,
            item,
            elements,
            result,
            done,
            yield_,
        } = &next[0].body
        else {
            panic!("Iterator plans the next step: {:?}", next[0].body);
        };
        assert_eq!(
            product,
            &positional_product(&[CheckedType::U8, CheckedType::I32])
        );
        assert_eq!(
            iterator,
            &positional_product(&[product.clone(), CheckedType::USize])
        );
        let CheckedType::Sum(sum) = item else {
            panic!("a mixed item coerces into a sum: {item:?}");
        };
        assert_eq!(sum.alternatives.len(), 2);
        assert!(matches!(result, CheckedType::Sum(_)));
        assert_eq!(elements.len(), 2);
        for element in elements {
            assert_eq!(
                element.coercion,
                Some((element.element.clone(), item.clone())),
                "each element coerces into the item"
            );
        }
        // `Done` carries the iterator back and `Yield` carries `(item, iter)`.
        let alternatives = match result {
            CheckedType::Sum(sum) => sum.alternatives.clone(),
            _ => unreachable!(),
        };
        assert!(matches!(
            &alternatives[done.index],
            CheckedType::Distinct { representation, .. } if representation.as_ref() == iterator
        ));
        assert_eq!(done.alternative, alternatives[done.index]);
        assert!(matches!(
            &alternatives[yield_.index],
            CheckedType::Distinct { representation, .. }
                if representation.as_ref() == &positional_product(&[item.clone(), iterator.clone()])
        ));
    }

    #[test]
    fn product_debug_plans_literal_steps_and_element_delegates() {
        let (_, lowered) = lower(concat!(
            "def show_unnamed: (I32, I32) -> String = pair => \"${pair:?}\"\n",
            "def show_named: (left: I32, right: I32) -> String = pair => \"${pair:?}\"\n",
            "let a = show_unnamed (1, 2)\n",
            "let b = show_named (1, 2)\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::Debug);
        assert_eq!(
            plans.len(),
            2,
            "named and unnamed products are distinct keys"
        );
        let mut saw_unnamed = false;
        let mut saw_named = false;
        for plan in plans {
            let StructuralBody::ProductDebug { steps, write } = &plan.body else {
                panic!("product Debug plans steps: {:?}", plan.body);
            };
            assert!(write.instance.is_some(), "the write instance is bound");
            let writes = steps
                .iter()
                .filter_map(|step| match step {
                    DebugStep::Write(literal) => Some(literal.as_str()),
                    DebugStep::Element { .. } => None,
                })
                .collect::<Vec<_>>();
            let elements = steps
                .iter()
                .filter_map(|step| match step {
                    DebugStep::Element { index, delegate } => {
                        assert_eq!(delegate.value_type, CheckedType::I32);
                        Some(*index)
                    }
                    DebugStep::Write(_) => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(elements, vec![0, 1], "elements stay in product order");
            if writes.contains(&"left") {
                assert_eq!(
                    writes,
                    vec!["(", "left", ": ", ", ", "right", ": ", ")"],
                    "named elements write name then `: `"
                );
                saw_named = true;
            } else {
                assert_eq!(
                    writes,
                    vec!["(", ", ", ")"],
                    "unnamed elements write separators only"
                );
                saw_unnamed = true;
            }
        }
        assert!(
            saw_unnamed && saw_named,
            "both parameter shapes are covered"
        );
    }

    #[test]
    fn sum_debug_plans_one_delegate_per_alternative_without_literals() {
        let (_, lowered) = lower(concat!(
            "def pick: Bool -> (I32 | U8) = condition => when { condition => 1, else => (1 satisfies U8) }\n",
            "def show_sum: (I32 | U8) -> String = value => \"${value:?}\"\n",
            "let chosen = show_sum (pick True)\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::Debug);
        assert_eq!(plans.len(), 1, "one sum Debug plan");
        let StructuralBody::SumDebug { alternatives } = &plans[0].body else {
            panic!(
                "a sum Debug has per-alternative delegates: {:?}",
                plans[0].body
            );
        };
        assert_eq!(alternatives.len(), 2);
        let mut value_types = alternatives
            .iter()
            .map(|delegate| delegate.value_type.clone())
            .collect::<Vec<_>>();
        value_types.sort_by_key(|value_type| format!("{value_type:?}"));
        assert_eq!(value_types, vec![CheckedType::I32, CheckedType::U8]);
        for delegate in alternatives {
            assert!(
                matches!(delegate.callee, PlannedCallee::Instance(_)),
                "I32 and U8 have explicit Debug implementations"
            );
        }
    }

    #[test]
    fn deref_index_loads_copy_products_and_delegates_heterogeneous_ones() {
        let (module, lowered) = lower(concat!(
            "def deref_uniform: (Ref (I32, I32)) -> I32 = reference => reference[0]\n",
            "def deref_mixed: (Ref (U8, I32)) -> (I32 | U8) = reference => reference[0]\n",
            "let uniform = deref_uniform (Ref (1, 2))\n",
            "let mixed = deref_mixed (Ref ((1 satisfies U8), 2))\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::DerefIndex);
        assert_eq!(plans.len(), 2);
        let mut saw_load = false;
        let mut saw_delegate = false;
        for plan in &plans {
            match &plan.body {
                StructuralBody::DerefIndexLoad {
                    element,
                    length,
                    output,
                } => {
                    assert_eq!(element, &CheckedType::I32);
                    assert_eq!(*length, 2);
                    assert_eq!(output, &CheckedType::I32);
                    // The owned `Copy` predicate must agree with the legacy
                    // per-function predicate the fast path uses.
                    assert_eq!(
                        lowered.program.concrete_is_copy(element),
                        module.is_copy_in_function(element, None),
                        "concrete_is_copy agrees with is_copy_in_function"
                    );
                    saw_load = true;
                }
                StructuralBody::DerefDelegate { payload, delegate } => {
                    assert_eq!(
                        payload,
                        &positional_product(&[CheckedType::U8, CheckedType::I32])
                    );
                    let PlannedCallee::Artifact(delegated) = &delegate.callee else {
                        panic!("the heterogeneous payload delegates to structural Index");
                    };
                    let artifact = lowered
                        .program
                        .artifacts
                        .get(LoweredArtifactRequestId::from_index(
                            delegated.artifact.expect("bound after closure").index(),
                        ))
                        .expect("delegated artifact");
                    assert!(matches!(
                        artifact.plan,
                        Some(LoweredArtifactPlan::StructuralMethod(
                            StructuralMethodPlan {
                                structural: StructuralTraitMethod::Index,
                                body: StructuralBody::IndexSwitch { .. },
                                ..
                            }
                        ))
                    ));
                    saw_delegate = true;
                }
                other => panic!("unexpected DerefIndex body {other:?}"),
            }
        }
        assert!(saw_load && saw_delegate);
    }

    #[test]
    fn deref_index_delegates_to_an_explicit_index_implementation() {
        let (_, lowered) = lower(concat!(
            "type Row = ctor (I32, I32)\n",
            "impl Index Row USize I32 { def index = (row, position) => 7 }\n",
            "def deref_row: (Ref Row, USize) -> I32 = (reference, position) => reference[position]\n",
            "let value = deref_row (Ref (Row (1, 2)), 0)\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::DerefIndex);
        assert_eq!(plans.len(), 1);
        let StructuralBody::DerefDelegate { payload, delegate } = &plans[0].body else {
            panic!("a nominal payload delegates: {:?}", plans[0].body);
        };
        assert_eq!(
            delegate.trait_id,
            lowered.program.semantic_ids.index_trait.unwrap()
        );
        assert_eq!(
            payload,
            &lowered
                .program
                .types
                .iter()
                .find(|(_, _, metadata)| metadata.name == "Row")
                .map(|(_, id, _)| CheckedType::Distinct {
                    id,
                    name: "Row".to_string(),
                    arguments: Vec::new(),
                    representation: Box::new(positional_product(&[
                        CheckedType::I32,
                        CheckedType::I32
                    ])),
                })
                .expect("Row metadata"),
            "the delegate names the payload type, not the reference"
        );
        assert!(
            matches!(delegate.callee, PlannedCallee::Instance(_)),
            "an explicit implementation selects an instance"
        );
    }

    #[test]
    fn deref_mutate_index_delegates_to_the_payloads_mutate_index() {
        let (_, lowered) = lower(concat!(
            "def deref_replace: move (Ref (I32, I32)) -> Ref (I32, I32) = move reference => {\n",
            "  let mut own = reference\n",
            "  own[0] = 3\n",
            "  own\n",
            "}\n",
            "let replaced = deref_replace (Ref (1, 2))\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::DerefMutateIndex);
        assert_eq!(plans.len(), 1);
        let StructuralBody::DerefDelegate { payload, delegate } = &plans[0].body else {
            panic!("DerefMutateIndex always delegates: {:?}", plans[0].body);
        };
        assert_eq!(
            delegate.trait_id,
            lowered.program.semantic_ids.mutate_index_trait.unwrap()
        );
        assert_eq!(
            payload,
            &positional_product(&[CheckedType::I32, CheckedType::I32])
        );
        let PlannedCallee::Artifact(delegated) = &delegate.callee else {
            panic!("the payload's MutateIndex is structural");
        };
        let artifact = lowered
            .program
            .artifacts
            .get(LoweredArtifactRequestId::from_index(
                delegated.artifact.expect("bound after closure").index(),
            ))
            .expect("delegated artifact");
        assert!(matches!(
            artifact.plan,
            Some(LoweredArtifactPlan::StructuralMethod(
                StructuralMethodPlan {
                    structural: StructuralTraitMethod::MutateIndex,
                    body: StructuralBody::MutateReplace { .. },
                    ..
                }
            ))
        ));
    }

    #[test]
    fn debug_of_nested_products_requests_nested_structural_artifacts() {
        let (_, lowered) = lower(concat!(
            "def show_nested: ((I32, I32), (I32, I32)) -> String = nested => \"${nested:?}\"\n",
            "let text = show_nested ((1, 2), (3, 4))\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::Debug);
        assert_eq!(
            plans.len(),
            2,
            "the outer product and one shared inner product expand"
        );
        let outer = plans
            .iter()
            .find(|plan| {
                matches!(
                    &plan.arguments[0],
                    CheckedType::Product(product)
                        if product.elements.len() == 2
                            && product.elements.iter().all(|element| matches!(
                                element.value_type,
                                CheckedType::Product(_)
                            ))
                )
            })
            .expect("the outer product Debug plan");
        let StructuralBody::ProductDebug { steps, .. } = &outer.body else {
            panic!("the outer product plans steps");
        };
        let mut nested_artifacts = 0;
        for step in steps {
            if let DebugStep::Element { delegate, .. } = step {
                let PlannedCallee::Artifact(nested) = &delegate.callee else {
                    panic!("an inner product delegates to a nested structural artifact");
                };
                let artifact = lowered
                    .program
                    .artifacts
                    .get(LoweredArtifactRequestId::from_index(
                        nested.artifact.expect("bound after closure").index(),
                    ))
                    .expect("nested artifact");
                assert!(matches!(
                    artifact.plan,
                    Some(LoweredArtifactPlan::StructuralMethod(
                        StructuralMethodPlan {
                            structural: StructuralTraitMethod::Debug,
                            body: StructuralBody::ProductDebug { .. },
                            ..
                        }
                    ))
                ));
                nested_artifacts += 1;
            }
        }
        assert_eq!(nested_artifacts, 2);
    }

    #[test]
    fn debug_delegates_to_an_explicit_generic_implementation_instance() {
        let (_, lowered) = lower(concat!(
            "type Held T = ctor (T)\n",
            "impl<T where Debug T> Debug (Held T) { def fmt = (Held value, mut formatter) => Debug.fmt (value, formatter) }\n",
            "def show_mixed: (Held I32, I32) -> String = pair => \"${pair:?}\"\n",
            "let text = show_mixed (Held 1, 2)\n",
        ));
        let plans = structural_plans(&lowered, StructuralTraitMethod::Debug);
        let outer = plans
            .iter()
            .find(|plan| {
                matches!(
                    &plan.arguments[0],
                    CheckedType::Product(product)
                        if product.elements.len() == 2
                            && product.elements.iter().any(|element| matches!(
                                element.value_type,
                                CheckedType::Distinct { .. }
                            ))
                )
            })
            .expect("the mixed product Debug plan");
        let StructuralBody::ProductDebug { steps, .. } = &outer.body else {
            panic!("the outer product plans steps");
        };
        let explicit = steps
            .iter()
            .find_map(|step| match step {
                DebugStep::Element { delegate, .. }
                    if matches!(delegate.value_type, CheckedType::Distinct { .. }) =>
                {
                    Some(delegate)
                }
                _ => None,
            })
            .expect("the Held element delegates");
        let PlannedCallee::Instance(instance) = &explicit.callee else {
            panic!("the generic Debug implementation selects an instance");
        };
        // The artifact requested the instance, so the closure interned and
        // materialized it in the next round.
        let bound = instance.instance.expect("bound after closure");
        assert!(
            lowered
                .program
                .instances
                .get(bound)
                .is_some_and(|record| record.body.is_some()),
            "the delegate instance was materialized"
        );
    }

    #[test]
    fn closure_rounds_and_growth_stay_bounded() {
        let (_, lowered) = lower(concat!(
            "type Resource = ctor I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
            "type Held T = ctor (T)\n",
            "impl<T where Debug T> Debug (Held T) { def fmt = (Held value, mut formatter) => Debug.fmt (value, formatter) }\n",
            "def index_pair: <T where Copy T> ((T, T), USize) -> T = (pair, position) => pair[position]\n",
            "def show_nested: ((I32, I32), (I32, I32)) -> String = nested => \"${nested:?}\"\n",
            "def show_mixed: (Held I32, I32) -> String = pair => \"${pair:?}\"\n",
            "def count_held: (Held I32, I32) -> I32 = pair => {\n",
            "  let mut count = 0\n",
            "  for item in pair { count = count + 1 }\n",
            "  count\n",
            "}\n",
            "def mutate_pair: move ((Resource, Resource)) -> (Resource, Resource) = move pair => {\n",
            "  let mut copy = pair\n",
            "  copy[0] = Resource 3\n",
            "  copy\n",
            "}\n",
            "let indexed: I32 = index_pair ((1, 2), 0)\n",
            "let text = show_nested ((1, 2), (3, 4))\n",
            "let held = show_mixed (Held 1, 2)\n",
            "let counted = count_held (Held 1, 2)\n",
            "let replaced = mutate_pair (Resource 1, Resource 2)\n",
        ));
        let stats = lowered
            .program
            .closure_stats
            .expect("the closure ran to completion");
        assert!(
            stats.rounds <= 8,
            "the fixture converges in a handful of rounds: {stats:?}"
        );
        assert!(
            stats.growth <= 256,
            "the fixture stays far below the growth budget: {stats:?}"
        );
    }

    #[test]
    fn structural_kinds_expand_for_each_generic_instantiation() {
        let (_, lowered) = lower(concat!(
            "def use_pair: <T where Copy T> (T, T) -> T = pair => {\n",
            "  let mut copy = pair\n",
            "  copy[0] = pair[1]\n",
            "  let mut last = copy[0]\n",
            "  for item in copy { last = item }\n",
            "  last\n",
            "}\n",
            "let used_i32: I32 = use_pair (1, 2)\n",
            "let used_u8: U8 = use_pair ((1 satisfies U8), (2 satisfies U8))\n",
        ));
        for kind in [
            StructuralTraitMethod::Index,
            StructuralTraitMethod::MutateIndex,
            StructuralTraitMethod::IntoIterator,
            StructuralTraitMethod::Iterator,
        ] {
            let plans = structural_plans(&lowered, kind);
            assert!(
                plans.len() >= 2,
                "{kind:?} expands once per generic instantiation: {}",
                plans.len()
            );
            for plan in plans {
                assert!(
                    plan.body != StructuralBody::Unexpanded,
                    "{kind:?} body is expanded"
                );
            }
        }
    }

    #[test]
    fn named_constructor_value_plans_positional_slots() {
        let (_, lowered) = lower(concat!(
            "type Named = ctor (left: I32, right: I32)\n",
            "let make_named: () -> ((I32, I32) -> Named) = () => Named\n",
        ));
        let plans = constructor_plans(&lowered);
        assert_eq!(plans.len(), 1);
        let ConstructorConstruction::Value {
            parameters,
            product,
        } = &plans[0].1.construction
        else {
            panic!("named parameters still classify as Value");
        };
        assert_eq!(parameters, &[CheckedType::I32, CheckedType::I32]);
        assert_eq!(
            product,
            &positional_product(&[CheckedType::I32, CheckedType::I32])
        );
    }
}
