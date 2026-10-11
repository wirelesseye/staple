//! Demand-driven type descriptors and their concrete drop entry points.
use super::artifact_closure::{ClosureRequest, ExpansionResult};
use super::{
    ArenaId, DescriptorDropPlan, DropGlueBody, DropGluePlan, LoweredArtifactDependencyKind as Kind,
    LoweredArtifactPlan as Plan, LoweredArtifactRequestId, LoweredProgram, PlannedArtifact,
    TypeDescriptorPlan,
};
use crate::specialization::{ArtifactRequestKey, CanonicalType, ReabstractionDirection};
use crate::{CheckedType, Origin};
use staple_syntax::Diagnostic;

pub(super) fn descriptor_request(
    value_type: &CheckedType,
    origin: &Origin,
) -> Result<ClosureRequest, Diagnostic> {
    Ok(ClosureRequest::Artifact {
        key: ArtifactRequestKey::TypeDescriptor(CanonicalType::concrete(value_type, origin)?),
        plan: Plan::TypeDescriptor(TypeDescriptorPlan {
            value_type: value_type.clone(),
            components: Vec::new(),
            drop: None,
            expanded: false,
        }),
        kind: Kind::TypeDescriptor,
        origin: origin.clone(),
        use_site: None,
    })
}

/// One concrete leaf type of a dynamic context. Compound types containing a
/// run-time parameter are not collected: their descriptors are built at run
/// time from their components.
pub(super) fn collect_concrete_types(ty: &CheckedType, types: &mut Vec<CheckedType>) {
    if matches!(
        ty,
        CheckedType::ParameterProduct(_)
            | CheckedType::TypeConstructor { .. }
    ) {
        return;
    }
    if !crate::contains_type_parameter(ty) {
        if !types.contains(ty) {
            types.push(ty.clone());
        }
        return;
    }
    match ty {
        CheckedType::Product(product) => {
            for field in &product.elements {
                collect_concrete_types(&field.value_type, types);
            }
        }
        CheckedType::Sum(sum) => {
            for alternative in &sum.alternatives {
                collect_concrete_types(alternative, types);
            }
        }
        CheckedType::Wrapper { representation, .. } => {
            collect_concrete_types(representation, types)
        }
        _ => {}
    }
}

/// The instantiation types of an expression's polymorphic-value coercion.
fn instantiation_types(expression: &super::LoweredExpression) -> &[CheckedType] {
    expression
        .coercion_plan
        .as_ref()
        .map_or(&[], super::LoweredCoercionPlan::instantiation_types)
}

/// The adapter request for one expression that instantiates a polymorphic
/// value, or `None` for every other expression.
fn instantiation_request(
    program: &super::LoweredProgram,
    id: super::ExpressionId,
    expression: &super::LoweredExpression,
) -> Result<Option<ClosureRequest>, Vec<Diagnostic>> {
    let (Some(coercion), Some(plan)) = (&expression.coercion, &expression.coercion_plan) else {
        return Ok(None);
    };
    // The instantiation may be the first step of a wrapper introduction or a
    // sum injection; the adapter produces the leaf's type.
    let Some((leaf, produced, _)) = plan.split_instantiation(&coercion.target) else {
        return Ok(None);
    };
    let (super::LoweredCoercionPlan::Instantiate { types }
    | super::LoweredCoercionPlan::AdaptScheme { types }) = leaf
    else {
        return Ok(None);
    };
    let CheckedType::Forall(scheme) = &coercion.source else {
        return Ok(None);
    };
    let key = crate::specialization::InstantiationAdapterKey {
        scheme: CanonicalType::dynamic(&coercion.source, &expression.origin)
            .map_err(|error| vec![error])?,
        target: CanonicalType::dynamic(&produced, &expression.origin)
            .map_err(|error| vec![error])?,
    };
    let (dictionaries, target_dictionaries, entailment) = super::trait_dictionaries::adapter_dictionaries(
        program,
        scheme,
        types,
        &produced,
        &expression.origin,
    )
    .map_err(|error| vec![error])?;
    Ok(Some(ClosureRequest::Artifact {
        key: ArtifactRequestKey::InstantiationAdapter(key),
        plan: Plan::InstantiationAdapter(crate::InstantiationAdapterPlan {
            scheme: scheme.as_ref().clone(),
            target: produced,
            types: types.clone(),
            dictionaries,
            target_dictionaries,
            entailment,
            children: Vec::new(),
        }),
        kind: Kind::InstantiationAdapter,
        origin: expression.origin.clone(),
        use_site: Some(super::artifact_closure::ArtifactUseSite::InstantiationAdapter(id)),
    }))
}

/// Concrete leaves used by a dynamic body. The deterministic order also names
/// scanner use sites, so closure validation can prove each descriptor is owned.
pub(super) fn concrete_body_types(body: &super::LoweredInstanceBody) -> Vec<CheckedType> {
    fn collect(ty: &CheckedType, types: &mut Vec<CheckedType>) {
        collect_concrete_types(ty, types)
    }
    let mut types = Vec::new();
    collect(&body.signature.parameter, &mut types);
    collect(&body.signature.result, &mut types);
    for (_, expression) in body.expressions() {
        collect(&expression.value_type, &mut types);
        if let Some(coercion) = &expression.coercion {
            collect(&coercion.source, &mut types);
            collect(&coercion.target, &mut types);
        }
        for ty in instantiation_types(expression) {
            collect(ty, &mut types);
        }
    }
    for (_, place) in body.places.iter() {
        collect(&place.value_type, &mut types);
    }
    for capture in &body.captures {
        collect(&capture.value_type, &mut types);
    }
    if let Some(coercion) = &body.body_coercion {
        collect(&coercion.source, &mut types);
        collect(&coercion.target, &mut types);
    }
    for (_, call) in body.calls.iter() {
        collect(&call.function_type.parameter, &mut types);
        for entry in &call.substitutions.types {
            collect(&entry.value_type, &mut types);
        }
    }
    types
}

pub(super) fn collect_compound_shapes(
    ty: &CheckedType,
    shapes: &mut Vec<crate::specialization::CompoundDescriptorDropKey>,
) {
    use crate::specialization::{
        CompoundDescriptorDropKey as Key, CompoundDescriptorShape as Shape,
    };

    if !crate::contains_type_parameter(ty) {
        return;
    }
    let (shape, children): (Shape, Vec<&CheckedType>) = match ty {
        CheckedType::Product(product) => (
            Shape::Product,
            product
                .elements
                .iter()
                .map(|field| &field.value_type)
                .collect(),
        ),
        CheckedType::Sum(sum) => (Shape::Sum, sum.alternatives.iter().collect()),
        CheckedType::Wrapper { representation, .. } => {
            collect_compound_shapes(representation, shapes);
            return;
        }
        CheckedType::Ref(pointee) => {
            collect_compound_shapes(pointee, shapes);
            return;
        }
        _ => return,
    };
    let key = Key {
        shape,
        count: children
            .len()
            .try_into()
            .expect("checked aggregate arity fits u32"),
    };
    if !shapes.contains(&key) {
        shapes.push(key);
    }
    for child in children {
        collect_compound_shapes(child, shapes);
    }
}

pub(super) fn compound_body_shapes(
    body: &super::LoweredInstanceBody,
) -> Vec<crate::specialization::CompoundDescriptorDropKey> {
    let mut shapes = Vec::new();
    collect_compound_shapes(&body.signature.parameter, &mut shapes);
    collect_compound_shapes(&body.signature.result, &mut shapes);
    for (_, expression) in body.expressions() {
        collect_compound_shapes(&expression.value_type, &mut shapes);
        if let Some(coercion) = &expression.coercion {
            collect_compound_shapes(&coercion.source, &mut shapes);
            collect_compound_shapes(&coercion.target, &mut shapes);
        }
        for ty in instantiation_types(expression) {
            collect_compound_shapes(ty, &mut shapes);
        }
    }
    if let Some(coercion) = &body.body_coercion {
        collect_compound_shapes(&coercion.source, &mut shapes);
        collect_compound_shapes(&coercion.target, &mut shapes);
    }
    for (_, call) in body.calls.iter() {
        collect_compound_shapes(&call.function_type.parameter, &mut shapes);
        for entry in &call.substitutions.types {
            collect_compound_shapes(&entry.value_type, &mut shapes);
        }
    }
    shapes
}

/// Reabstraction-thunk requests for one owner's recorded boundary
/// conversions (R5).
fn reabstraction_requests(
    conversions: &std::collections::BTreeMap<
        (super::LoweredCallId, Option<usize>),
        super::LoweredReabstraction,
    >,
    origin: &Origin,
) -> Result<Vec<ClosureRequest>, Vec<Diagnostic>> {
    let mut requests = Vec::new();
    for ((call, argument), conversion) in conversions {
        let direction = match conversion.direction {
            super::LoweredReabstractionDirection::ConcreteToDynamic => {
                ReabstractionDirection::ConcreteToDynamic
            }
            super::LoweredReabstractionDirection::DynamicToConcrete => {
                ReabstractionDirection::DynamicToConcrete
            }
        };
        let mut request = super::value_conversions::request(
            &conversion.concrete,
            &conversion.dynamic,
            direction,
            origin,
        )
        .map_err(|error| vec![error])?;
        let ClosureRequest::Artifact { use_site, .. } = &mut request else {
            unreachable!()
        };
        *use_site = Some(
            super::artifact_closure::ArtifactUseSite::ReabstractionThunk {
                call: *call,
                argument: *argument,
            },
        );
        requests.push(request);
        if conversion.writeback {
            let argument = argument.expect("writeback belongs to an argument");
            let inverse = match direction {
                ReabstractionDirection::ConcreteToDynamic => {
                    ReabstractionDirection::DynamicToConcrete
                }
                ReabstractionDirection::DynamicToConcrete => {
                    ReabstractionDirection::ConcreteToDynamic
                }
            };
            let mut request = super::value_conversions::request(
                &conversion.concrete,
                &conversion.dynamic,
                inverse,
                origin,
            )
            .map_err(|error| vec![error])?;
            let ClosureRequest::Artifact { use_site, .. } = &mut request else {
                unreachable!()
            };
            *use_site = Some(
                super::artifact_closure::ArtifactUseSite::ReabstractionWriteback {
                    call: *call,
                    argument,
                },
            );
            requests.push(request);
        }
    }
    Ok(requests)
}

pub(super) fn scan_instance(
    program: &LoweredProgram,
    id: super::FunctionInstanceId,
) -> super::artifact_closure::ScanResult {
    let Some(instance) = program.instances.get(id) else {
        return Ok(Vec::new());
    };
    let Some(body) = &instance.body else {
        return Ok(Vec::new());
    };
    let dynamic = program
        .specializations
        .instance(instance.ordinal)
        .is_some_and(|key| key.is_dynamic());
    let dynamic_call = body.bindings.values().any(|target| match target {
        super::LoweredBoundTarget::Instance(id) => program
            .instances
            .get(*id)
            .and_then(|callee| program.specializations.instance(callee.ordinal))
            .is_some_and(|key| key.is_dynamic()),
        _ => false,
    });
    let polymorphic_call = body.calls.iter().any(|(_, call)| call.scheme.is_some())
        || body.expressions().any(|(_, expression)| {
            matches!(
                (&expression.coercion, &expression.coercion_plan),
                (Some(coercion), Some(plan)) if plan.split_instantiation(&coercion.target).is_some()
            )
        });
    if !dynamic && !dynamic_call && !polymorphic_call {
        return Ok(Vec::new());
    }
    let mut requests = concrete_body_types(body)
        .iter()
        .enumerate()
        .map(|(index, ty)| {
            let mut request =
                descriptor_request(ty, &body.origin).map_err(|diagnostic| vec![diagnostic])?;
            let ClosureRequest::Artifact { use_site, .. } = &mut request else {
                unreachable!()
            };
            *use_site =
                Some(super::artifact_closure::ArtifactUseSite::InstanceTypeDescriptor(index));
            Ok(request)
        })
        .collect::<super::artifact_closure::ScanResult>()?;
    for (index, key) in compound_body_shapes(body).into_iter().enumerate() {
        requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::CompoundDescriptorDrop(key),
            plan: Plan::CompoundDescriptorDrop(key),
            kind: Kind::CompoundDescriptorDrop,
            origin: body.origin.clone(),
            use_site: Some(super::artifact_closure::ArtifactUseSite::InstanceCompoundDrop(index)),
        });
    }
    // A dynamic-shape closure points at a descriptor-bound entry (R-C).
    for (_, record) in body.expressions() {
        let super::LoweredExpressionKind::CallableValue(value_id) = &record.kind else {
            continue;
        };
        let Some(callable) = body.callable_value(*value_id) else {
            continue;
        };
        if !matches!(
            callable.target,
            super::LoweredCallableTarget::DirectFunction { .. }
                | super::LoweredCallableTarget::TraitImplementation { .. }
        ) {
            continue;
        }
        let Some(super::LoweredBoundTarget::Instance(callee)) = body
            .bindings
            .get(&super::LoweredBindingSite::CallableValue(*value_id))
        else {
            continue;
        };
        let Some(runtime) = program.instance_runtime_order(*callee) else {
            continue;
        };
        let bound = if callable.scheme.is_some() {
            let declared = program
                .instances
                .get(*callee)
                .and_then(|instance| program.functions.get(instance.template))
                .map(|template| template.scheme_type_parameters.as_slice())
                .unwrap_or(&[]);
            runtime
                .iter()
                .take_while(|parameter| !declared.contains(parameter))
                .count()
        } else {
            runtime.len()
        };
        // Without enclosing descriptors, the polymorphic closure points
        // straight at the dynamic instance (B-C), without a forwarding entry.
        if bound == 0
            && callable
                .closure
                .as_ref()
                .is_none_or(|closure| closure.captures.is_empty())
        {
            continue;
        }
        let Some(ordinal) = program
            .instances
            .get(*callee)
            .map(|instance| instance.ordinal)
        else {
            continue;
        };
        if bound > 0 {
            requests.push(ClosureRequest::Artifact {
                key: ArtifactRequestKey::DescriptorBoundEntry(
                    crate::specialization::DescriptorBoundEntryKey {
                        instance: ordinal,
                        bound,
                        offset: 0,
                    },
                ),
                plan: Plan::DescriptorBoundEntry(crate::DescriptorBoundEntryPlan {
                    bound,
                    dictionary_prefix: runtime.len(),
                    dictionary_count: super::trait_dictionaries::entry_dictionary_partition(
                        program, *callee, bound,
                    )
                    .expect("dictionary partition")
                    .0,
                    bound_dictionaries: super::trait_dictionaries::entry_dictionary_partition(
                        program, *callee, bound,
                    )
                    .expect("dictionary partition")
                    .1,
                    environment_offset: 0,
                    instance: *callee,
                    runtime: runtime.clone(),
                    function_type: callable.function_type.clone(),
                }),
                kind: Kind::DescriptorBoundEntry,
                origin: record.origin.clone(),
                use_site: Some(
                    super::artifact_closure::ArtifactUseSite::DescriptorBoundEntry(*value_id),
                ),
            });
        }
        if callable.scheme.is_none()
            && !crate::contains_type_parameter(&CheckedType::Function(
                callable.function_type.clone(),
            ))
        {
            let concrete = crate::specialization::CanonicalFunctionType::concrete(
                &callable.function_type,
                &record.origin,
            )
            .map_err(|error| vec![error])?;
            let dynamic = crate::specialization::CanonicalFunctionType::dynamic(
                &callable.function_type,
                &record.origin,
            )
            .map_err(|error| vec![error])?;
            requests.push(ClosureRequest::Artifact {
                key: ArtifactRequestKey::ReabstractionThunk(
                    crate::specialization::ReabstractionThunkKey {
                        dictionary_slot: None,
                        concrete,
                        dynamic,
                        direction: ReabstractionDirection::DynamicToConcrete,
                    },
                ),
                plan: Plan::ReabstractionThunk(crate::ReabstractionThunkPlan {
                    dictionary_slot: None,
                    concrete: callable.function_type.clone(),
                    dynamic: callable.function_type.clone(),
                    direction: ReabstractionDirection::DynamicToConcrete,
                    children: Vec::new(),
                }),
                kind: Kind::ReabstractionThunk,
                origin: record.origin.clone(),
                use_site: Some(
                    super::artifact_closure::ArtifactUseSite::ConcreteCapturedClosure(*value_id),
                ),
            });
        }
        // Captured dynamic closures install a descriptor-based finalizer (R-D).
        if let Some(closure) = &callable.closure
            && !closure.captures.is_empty()
        {
            requests.push(ClosureRequest::Artifact {
                key: ArtifactRequestKey::GcFinalizer(
                    crate::specialization::GcFinalizerKey::DynamicClosureEnvironment {
                        closure: ordinal,
                    },
                ),
                plan: Plan::GcFinalizer(crate::GcFinalizerPlan::DynamicClosureEnvironment {
                    closure: *callee,
                    dictionaries: super::trait_dictionaries::site_stored_dictionary_count(
                        program,
                        *callee,
                        callable.scheme.is_some(),
                    )
                    .unwrap_or(0),
                    runtime,
                    captures: closure
                        .captures
                        .iter()
                        .map(|capture| crate::DynamicCapture {
                            value_type: capture.value_type.clone(),
                            access: capture.access,
                            owns: capture.owns_value,
                        })
                        .collect(),
                }),
                kind: Kind::GcFinalizer,
                origin: record.origin.clone(),
                use_site: Some(
                    super::artifact_closure::ArtifactUseSite::ClosureEnvironment(*value_id),
                ),
            });
        }
    }
    // Implicit thunk arguments use the same descriptor-bound closure entry
    // and capture finalizer as written lambdas.
    for (site, target) in &body.bindings {
        let super::LoweredBindingSite::CallArgumentThunk { call, argument } = site else {
            continue;
        };
        let super::LoweredBoundTarget::Instance(callee) = target else {
            continue;
        };
        let Some(runtime) = program.instance_runtime_order(*callee) else {
            continue;
        };
        // A thunk that is dynamic only through its effects has no
        // descriptors to bind: it is its own entry, but its captures still
        // need the dynamic finalizer.
        if !program.instance_is_dynamic(*callee) {
            continue;
        }
        let Some(record) = program.instances.get(*callee) else {
            continue;
        };
        let Some(thunk) = &record.body else {
            continue;
        };
        if !runtime.is_empty() {
            requests.push(ClosureRequest::Artifact {
                key: ArtifactRequestKey::DescriptorBoundEntry(
                    crate::specialization::DescriptorBoundEntryKey {
                        instance: record.ordinal,
                        bound: runtime.len(),
                        offset: 0,
                    },
                ),
                plan: Plan::DescriptorBoundEntry(crate::DescriptorBoundEntryPlan {
                    bound: runtime.len(),
                    dictionary_prefix: runtime.len(),
                    dictionary_count: super::trait_dictionaries::entry_dictionary_partition(
                        program,
                        *callee,
                        runtime.len(),
                    )
                    .expect("thunk dictionary partition")
                    .0,
                    bound_dictionaries: super::trait_dictionaries::entry_dictionary_partition(
                        program,
                        *callee,
                        runtime.len(),
                    )
                    .expect("thunk dictionary partition")
                    .1,
                    environment_offset: 0,
                    instance: *callee,
                    runtime: runtime.clone(),
                    function_type: thunk.signature.clone(),
                }),
                kind: Kind::DescriptorBoundEntry,
                origin: thunk.origin.clone(),
                use_site: Some(
                    super::artifact_closure::ArtifactUseSite::DescriptorBoundThunk {
                        call: *call,
                        argument: *argument,
                    },
                ),
            });
        }
        if !thunk.captures.is_empty() {
            requests.push(ClosureRequest::Artifact {
                key: ArtifactRequestKey::GcFinalizer(
                    crate::specialization::GcFinalizerKey::DynamicClosureEnvironment {
                        closure: record.ordinal,
                    },
                ),
                plan: Plan::GcFinalizer(crate::GcFinalizerPlan::DynamicClosureEnvironment {
                    closure: *callee,
                    dictionaries: super::trait_dictionaries::stored_dictionary_count(
                        program, *callee,
                    )
                    .unwrap_or(0),
                    runtime,
                    captures: thunk
                        .captures
                        .iter()
                        .map(|capture| crate::DynamicCapture {
                            value_type: capture.value_type.clone(),
                            access: if capture.requires_initialization_state
                                || capture.mutable_storage
                                || capture.derived
                            {
                                crate::LoweredCaptureAccess::SharedCell
                            } else if capture.capture.borrowed {
                                crate::LoweredCaptureAccess::Borrowed
                            } else {
                                crate::LoweredCaptureAccess::ByValue
                            },
                            owns: !capture.capture.borrowed && !capture.capture.non_owning,
                        })
                        .collect(),
                }),
                kind: Kind::GcFinalizer,
                origin: thunk.origin.clone(),
                use_site: Some(
                    super::artifact_closure::ArtifactUseSite::ThunkArgumentEnvironment {
                        call: *call,
                        argument: *argument,
                    },
                ),
            });
        }
    }
    // A dynamic buffer allocation installs the shared dynamic buffer
    // finalizer (R6). `__buffer_clone` allocates its copy the same way.
    if let Some((call_id, _)) = body.calls.iter().find(|(_, call)| {
        matches!(
            &call.target,
            super::LoweredCallableTarget::Intrinsic {
                intrinsic: crate::IntrinsicFunction::BufferWithCapacity
                    | crate::IntrinsicFunction::BufferClone,
                ..
            }
        )
    }) {
        requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(
                crate::specialization::GcFinalizerKey::DynamicBuffer,
            ),
            plan: Plan::GcFinalizer(crate::GcFinalizerPlan::DynamicBuffer),
            kind: Kind::GcFinalizer,
            origin: body.origin.clone(),
            use_site: Some(super::artifact_closure::ArtifactUseSite::BufferAllocation(
                call_id,
            )),
        });
    }
    // Managed references use the same descriptor-bearing payload as cells.
    if let Some((call_id, _)) = body.calls.iter().find(|(_, call)| {
        matches!(
            &call.target,
            super::LoweredCallableTarget::Constructor {
                recursive: Some(crate::RecursiveConstruction::ManagedReference),
                ..
            }
        )
    }) {
        requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(
                crate::specialization::GcFinalizerKey::DynamicCell,
            ),
            plan: Plan::GcFinalizer(crate::GcFinalizerPlan::DynamicCell),
            kind: Kind::GcFinalizer,
            origin: body.origin.clone(),
            use_site: Some(super::artifact_closure::ArtifactUseSite::RefConstruction(
                call_id,
            )),
        });
    }
    // A captured mutable local allocates a dynamic shared cell and installs
    // the shared dynamic cell finalizer (R-D).
    for (_, item) in body.items() {
        let super::LoweredItemKind::Binding(binding) = &item.kind else {
            continue;
        };
        if !binding.cell
            || binding.signal
            || binding.derived
            || binding.generic
            || binding.compile_time_only
        {
            continue;
        }
        let Some(symbol) = binding.symbol else {
            continue;
        };
        let captured = program
            .symbols
            .get(symbol)
            .is_some_and(|record| record.captured && (record.mutable_storage || record.derived));
        if !captured {
            continue;
        }
        requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(
                crate::specialization::GcFinalizerKey::DynamicCell,
            ),
            plan: Plan::GcFinalizer(crate::GcFinalizerPlan::DynamicCell),
            kind: Kind::GcFinalizer,
            origin: body.origin.clone(),
            use_site: Some(super::artifact_closure::ArtifactUseSite::DynamicCellFinalizer(symbol)),
        });
    }
    for (id, expression) in body.expressions() {
        requests.extend(instantiation_request(program, id, expression)?);
    }
    requests.extend(reabstraction_requests(&body.conversions, &body.origin)?);
    Ok(requests)
}

/// Concrete descriptors used by an initializer's polymorphic calls and
/// differential bridges, in deterministic scanner order. Shared by the
/// scanner and use-site validation.
pub(super) fn concrete_initializer_types(
    program: &LoweredProgram,
    id: super::InitializerId,
) -> Vec<CheckedType> {
    let mut visitor = InitializerDescriptorVisitor {
        program,
        types: Vec::new(),
    };
    if super::cleanup_artifacts::walk_owner(
        program,
        super::emission::OwnerArenas::Initializer(id),
        &mut visitor,
    )
    .is_err()
    {
        return Vec::new();
    }
    visitor.types
}

pub(super) fn scan_initializer(
    program: &LoweredProgram,
    id: super::InitializerId,
) -> super::artifact_closure::ScanResult {
    let types = concrete_initializer_types(program, id);
    let mut adapters = InitializerAdapterVisitor {
        program,
        requests: Vec::new(),
    };
    super::cleanup_artifacts::walk_owner(
        program,
        super::emission::OwnerArenas::Initializer(id),
        &mut adapters,
    )?;
    if types.is_empty() && adapters.requests.is_empty() {
        return Ok(Vec::new());
    }
    let origin = program
        .initializers
        .get(id)
        .map_or_else(crate::Origin::compiler, |initializer| {
            initializer.origin.clone()
        });
    let mut requests = adapters.requests;
    for (index, ty) in types.iter().enumerate() {
        let mut request = descriptor_request(ty, &origin).map_err(|diagnostic| vec![diagnostic])?;
        let ClosureRequest::Artifact { use_site, .. } = &mut request else {
            unreachable!()
        };
        *use_site = Some(super::artifact_closure::ArtifactUseSite::InstanceTypeDescriptor(index));
        requests.push(request);
    }
    if let Some(conversions) = program.initializer_conversions.get(id.index()) {
        requests.extend(reabstraction_requests(conversions, &origin)?);
    }
    Ok(requests)
}

struct InitializerDescriptorVisitor<'a> {
    program: &'a LoweredProgram,
    types: Vec<CheckedType>,
}

/// Collects the instantiation-adapter requests of an initializer's
/// polymorphic-value coercions.
struct InitializerAdapterVisitor<'a> {
    program: &'a super::LoweredProgram,
    requests: Vec<ClosureRequest>,
}

impl super::cleanup_artifacts::LoweredOwnerVisitor for InitializerAdapterVisitor<'_> {
    fn expression_site(
        &mut self,
        id: super::ExpressionId,
        expression: &super::LoweredExpression,
    ) -> Result<(), Vec<staple_syntax::Diagnostic>> {
        self.requests
            .extend(instantiation_request(self.program, id, expression)?);
        Ok(())
    }
}

impl super::cleanup_artifacts::LoweredOwnerVisitor for InitializerDescriptorVisitor<'_> {
    fn expression_site(
        &mut self,
        _id: super::ExpressionId,
        expression: &super::LoweredExpression,
    ) -> Result<(), Vec<staple_syntax::Diagnostic>> {
        for ty in instantiation_types(expression) {
            collect_concrete_types(ty, &mut self.types);
        }
        Ok(())
    }

    fn call_site(
        &mut self,
        call: &super::LoweredCall,
    ) -> Result<(), Vec<staple_syntax::Diagnostic>> {
        let needs_descriptors = call.scheme.is_some()
            || match &call.target {
                super::LoweredCallableTarget::DirectFunction { function, .. } => self
                    .program
                    .differential_call_runtime(
                        *function,
                        &std::collections::BTreeSet::new(),
                        false,
                        &call.function_type,
                        &call.substitutions,
                    )
                    .is_some(),
                _ => false,
            };
        if !needs_descriptors {
            return Ok(());
        }
        collect_concrete_types(&call.function_type.parameter, &mut self.types);
        collect_concrete_types(&call.function_type.result, &mut self.types);
        for entry in &call.substitutions.types {
            collect_concrete_types(&entry.value_type, &mut self.types);
        }
        Ok(())
    }
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

pub(super) fn expand(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: Plan,
) -> ExpansionResult {
    let origin = program
        .artifacts
        .get(artifact)
        .expect("interned descriptor artifact")
        .origin
        .clone();
    let mut requests = Vec::new();
    let plan = match plan {
        Plan::TypeDescriptor(mut plan) => {
            let components: Vec<&CheckedType> = match &plan.value_type {
                CheckedType::Product(product) => product
                    .elements
                    .iter()
                    .map(|field| &field.value_type)
                    .collect(),
                CheckedType::Sum(sum) => sum.alternatives.iter().collect(),
                CheckedType::Wrapper { representation, .. } => vec![representation],
                // Pointer-like descriptors keep fixed layout; their operations
                // get element descriptors from the body's type arguments.
                _ => Vec::new(),
            };
            plan.components.clear();
            for component in components {
                let request =
                    descriptor_request(component, &origin).map_err(|error| vec![error])?;
                plan.components.push(planned(&request));
                requests.push(request);
            }
            plan.drop = None;
            if program.concrete_needs_drop(&plan.value_type) {
                let request = ClosureRequest::Artifact {
                    key: ArtifactRequestKey::DescriptorDrop(
                        CanonicalType::concrete(&plan.value_type, &origin)
                            .map_err(|error| vec![error])?,
                    ),
                    plan: Plan::DescriptorDrop(DescriptorDropPlan {
                        value_type: plan.value_type.clone(),
                        glue: None,
                    }),
                    kind: Kind::DescriptorDrop,
                    origin: origin.clone(),
                    use_site: None,
                };
                plan.drop = Some(planned(&request));
                requests.push(request);
            }
            plan.expanded = true;
            Plan::TypeDescriptor(plan)
        }
        Plan::CompoundDescriptorDrop(plan) => Plan::CompoundDescriptorDrop(plan),
        Plan::DescriptorBoundEntry(plan) => Plan::DescriptorBoundEntry(plan),
        Plan::TraitMemberEntry(plan) => {
            if let Some(callee) = &plan.callee {
                let ordinal = program.specializations.instance_ordinal(&callee.key).ok_or_else(|| vec![Diagnostic::new(origin.span.clone(), "generic slot instance was not reserved")])?;
                let id = <super::FunctionInstanceId as super::ArenaId>::from_index(ordinal.index());
                let instance = program.instances.get(id).ok_or_else(|| vec![Diagnostic::new(origin.span.clone(), "generic slot instance is absent")])?;
                requests.push(ClosureRequest::Instance {
                    resolved: super::instance_resolution::ResolvedInstanceRequest {
                        key: callee.key.clone(), environment: instance.environment.clone(),
                        relevant: instance.relevant.clone(), evidence: instance.evidence.clone(),
                    }, kind: callee.kind, origin: origin.clone(), use_site: None,
                });
            }
            Plan::TraitMemberEntry(plan)
        },
        Plan::InstantiationAdapter(mut plan) => {
            // An adapter converts function-typed arguments and results
            // between the target's ABI and the scheme body's (R5). A scheme
            // target converts at its own body.
            plan.children.clear();
            let target = match &plan.target {
                CheckedType::Forall(scheme) => scheme.body.as_ref(),
                target => target,
            };
            if let (CheckedType::Function(target), CheckedType::Function(body)) =
                (target, plan.scheme.body.as_ref())
            {
                use crate::specialization::ReabstractionDirection as Direction;
                let conversions = super::value_conversions::adapter_conversions(target, body)
                    .map_err(|message| vec![Diagnostic::new(origin.span.clone(), message)])?;
                let whole = target.mutations.contains(&crate::CheckedMutation::Whole);
                let mut children = Vec::new();
                for (index, conversion) in conversions.arguments.iter().enumerate() {
                    let Some(conversion) = conversion else {
                        continue;
                    };
                    children.push(
                        super::value_conversions::request(
                            &conversion.concrete,
                            &conversion.dynamic,
                            Direction::ConcreteToDynamic,
                            &origin,
                        )
                        .map_err(|error| vec![error])?,
                    );
                    if whole
                        || target
                            .mutations
                            .contains(&crate::CheckedMutation::Element(index))
                    {
                        children.push(
                            super::value_conversions::request(
                                &conversion.concrete,
                                &conversion.dynamic,
                                Direction::DynamicToConcrete,
                                &origin,
                            )
                            .map_err(|error| vec![error])?,
                        );
                    }
                }
                if let Some(conversion) = &conversions.result {
                    children.push(
                        super::value_conversions::request(
                            &conversion.concrete,
                            &conversion.dynamic,
                            Direction::DynamicToConcrete,
                            &origin,
                        )
                        .map_err(|error| vec![error])?,
                    );
                }
                plan.children = children.iter().map(planned).collect();
                requests.extend(children);
            }
            // A scheme-target adapter builds the source scheme's descriptors
            // at entry: its concrete leaves need static descriptors and its
            // compound shapes their cleanup entries.
            if matches!(plan.target, CheckedType::Forall(_)) {
                let mut leaves = Vec::new();
                let mut shapes = Vec::new();
                for ty in &plan.types {
                    collect_concrete_types(ty, &mut leaves);
                    collect_compound_shapes(ty, &mut shapes);
                }
                for ty in &leaves {
                    requests.push(descriptor_request(ty, &origin).map_err(|error| vec![error])?);
                }
                for key in shapes {
                    requests.push(ClosureRequest::Artifact {
                        key: ArtifactRequestKey::CompoundDescriptorDrop(key),
                        plan: Plan::CompoundDescriptorDrop(key),
                        kind: Kind::CompoundDescriptorDrop,
                        origin: origin.clone(),
                        use_site: None,
                    });
                }
            }
            Plan::InstantiationAdapter(plan)
        }
        Plan::ReabstractionThunk(plan) => Plan::ReabstractionThunk(plan),
        Plan::ValueConversion(plan) => Plan::ValueConversion(plan),
        Plan::DescriptorDrop(mut plan) => {
            let request = ClosureRequest::Artifact {
                key: ArtifactRequestKey::DropGlue(
                    CanonicalType::concrete(&plan.value_type, &origin)
                        .map_err(|error| vec![error])?,
                ),
                plan: Plan::DropGlue(DropGluePlan {
                    value_type: plan.value_type.clone(),
                    body: DropGlueBody::Unexpanded,
                }),
                kind: Kind::DropGlue,
                origin: origin.clone(),
                use_site: None,
            };
            plan.glue = Some(planned(&request));
            requests.push(request);
            Plan::DescriptorDrop(plan)
        }
        _ => {
            return Err(vec![Diagnostic::new(
                origin.span,
                "descriptor artifact carries a mismatched plan",
            )]);
        }
    };
    Ok((plan, requests))
}

#[cfg(test)]
mod tests {
    use super::super::ArenaId;
    use super::super::artifact_closure::ProductionHooks;
    use super::super::worklist::{GraphRecorder, TraversalOwner};
    use super::*;
    use crate::{CodeGenerator, Lowerer, NameResolver, ProgramLoader, TypeChecker};
    use std::path::Path;

    #[test]
    fn static_descriptors_expand_and_emit_drop_entries() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("stdlib");
        let loaded = ProgramLoader::new()
            .with_standard_library_root(&root)
            .load_source(
                concat!(
                    "use std.cinterop.(CString, c_string)\n",
                    "type Payload = wrap (I32, CString)\n",
                    "let value = Payload (1, c_string \"value\")\n",
                ),
                &root,
            )
            .unwrap();
        let typed = TypeChecker::new()
            .check(NameResolver::new().resolve_program(loaded).unwrap())
            .unwrap();
        let mut module = Lowerer::new().lower(&typed).unwrap();
        let value_type = module
            .program
            .symbols
            .iter()
            .find(|(_, _, symbol)| symbol.name == "value")
            .unwrap()
            .2
            .value_type
            .clone();
        let initializer = module.program.initializers.iter().next().unwrap().0;
        let request = descriptor_request(&value_type, &Origin::compiler()).unwrap();
        let ClosureRequest::Artifact {
            key,
            plan,
            kind,
            origin,
            ..
        } = request
        else {
            unreachable!()
        };
        let mut recorder = GraphRecorder::from_parts(module.program.take_graph());
        let (descriptor, created) = recorder.request_closure_artifact(
            key.clone(),
            Some(plan),
            &origin,
            TraversalOwner::Initializer(initializer),
            kind,
        );
        assert!(created);
        module.program.install_graph(recorder.into_parts());
        let problems = module.program.close_artifact_catalog(&ProductionHooks);
        assert!(problems.is_empty(), "{problems:?}");
        let problems = module.program.validate_artifact_closure(&ProductionHooks);
        assert!(problems.is_empty(), "{problems:?}");
        let record = module
            .program
            .artifacts
            .get(super::super::LoweredArtifactRequestId::from_index(
                descriptor.index(),
            ))
            .unwrap();
        let Some(Plan::TypeDescriptor(plan)) = &record.plan else {
            panic!("descriptor plan")
        };
        assert!(plan.expanded);
        assert_eq!(plan.components.len(), 1);
        assert!(plan.drop.is_some());
        assert!(
            module
                .program
                .artifacts
                .iter()
                .any(|(_, artifact)| matches!(artifact.plan, Some(Plan::DescriptorDrop(_))))
        );
        let context = inkwell::context::Context::create();
        let ir = CodeGenerator::new(&context)
            .compile_module(&module)
            .unwrap();
        assert!(ir.contains("internal constant {"));
        assert!(ir.contains("define internal void @__staple_descriptor_drop_"));
        assert!(ir.contains("descriptor.drop.value"));
    }
}
