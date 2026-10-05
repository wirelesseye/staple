//! Demand-driven type descriptors and their concrete drop entry points.
use super::artifact_closure::{ClosureRequest, ExpansionResult};
use super::{
    DescriptorDropPlan, DropGlueBody, DropGluePlan, LoweredArtifactDependencyKind as Kind,
    LoweredArtifactPlan as Plan, LoweredArtifactRequestId, LoweredProgram, PlannedArtifact,
    TypeDescriptorPlan,
};
use crate::specialization::{ArtifactRequestKey, CanonicalType};
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
        CheckedType::Forall(_)
            | CheckedType::ParameterProduct(_)
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
        CheckedType::Wrapper { representation, .. } => collect_concrete_types(representation, types),
        _ => {}
    }
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
    }
    for (_, place) in body.places.iter() {
        collect(&place.value_type, &mut types);
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

pub(super) fn compound_body_shapes(
    body: &super::LoweredInstanceBody,
) -> Vec<crate::specialization::CompoundDescriptorDropKey> {
    use crate::specialization::{
        CompoundDescriptorDropKey as Key, CompoundDescriptorShape as Shape,
    };
    fn collect(ty: &CheckedType, shapes: &mut Vec<Key>) {
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
                collect(representation, shapes);
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
            collect(child, shapes);
        }
    }
    let mut shapes = Vec::new();
    collect(&body.signature.parameter, &mut shapes);
    collect(&body.signature.result, &mut shapes);
    for (_, expression) in body.expressions() {
        collect(&expression.value_type, &mut shapes);
        if let Some(coercion) = &expression.coercion {
            collect(&coercion.source, &mut shapes);
            collect(&coercion.target, &mut shapes);
        }
    }
    if let Some(coercion) = &body.body_coercion {
        collect(&coercion.source, &mut shapes);
        collect(&coercion.target, &mut shapes);
    }
    for (_, call) in body.calls.iter() {
        collect(&call.function_type.parameter, &mut shapes);
        for entry in &call.substitutions.types {
            collect(&entry.value_type, &mut shapes);
        }
    }
    shapes
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
        .is_some_and(|key| !key.runtime().is_empty());
    let dynamic_call = body.bindings.values().any(|target| match target {
        super::LoweredBoundTarget::Instance(id) => program
            .instances
            .get(*id)
            .and_then(|callee| program.specializations.instance(callee.ordinal))
            .is_some_and(|key| !key.runtime().is_empty()),
        _ => false,
    });
    if !dynamic && !dynamic_call {
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
        if callable
            .closure
            .as_ref()
            .is_some_and(|closure| !closure.captures.is_empty())
        {
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
        if runtime.is_empty() {
            continue;
        }
        let Some(ordinal) = program
            .instances
            .get(*callee)
            .map(|instance| instance.ordinal)
        else {
            continue;
        };
        requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::DescriptorBoundEntry(
                crate::specialization::DescriptorBoundEntryKey { instance: ordinal },
            ),
            plan: Plan::DescriptorBoundEntry(crate::DescriptorBoundEntryPlan {
                instance: *callee,
                runtime,
                function_type: callable.function_type.clone(),
            }),
            kind: Kind::DescriptorBoundEntry,
            origin: record.origin.clone(),
            use_site: Some(
                super::artifact_closure::ArtifactUseSite::DescriptorBoundEntry(*value_id),
            ),
        });
    }
    Ok(requests)
}

/// Descriptors a module initializer's concrete bridge into a dynamic
/// implementation needs: the concrete types named by the calls it redirects.
/// Concrete types a module initializer's differential bridge needs, in the
/// deterministic scanner order. Used by both the scanner and use-site
/// validation.
pub(super) fn concrete_initializer_types(
    program: &LoweredProgram,
    id: super::InitializerId,
) -> Vec<CheckedType> {
    if !program.dictionary_pass_generic_calls {
        return Vec::new();
    }
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
    if types.is_empty() {
        return Ok(Vec::new());
    }
    let origin = program
        .initializers
        .get(id)
        .map_or_else(crate::Origin::compiler, |initializer| {
            initializer.origin.clone()
        });
    let mut requests = Vec::new();
    for (index, ty) in types.iter().enumerate() {
        let mut request = descriptor_request(ty, &origin).map_err(|diagnostic| vec![diagnostic])?;
        let ClosureRequest::Artifact { use_site, .. } = &mut request else {
            unreachable!()
        };
        *use_site =
            Some(super::artifact_closure::ArtifactUseSite::InstanceTypeDescriptor(index));
        requests.push(request);
    }
    Ok(requests)
}

struct InitializerDescriptorVisitor<'a> {
    program: &'a LoweredProgram,
    types: Vec<CheckedType>,
}

impl super::cleanup_artifacts::LoweredOwnerVisitor for InitializerDescriptorVisitor<'_> {
    fn call_site(
        &mut self,
        call: &super::LoweredCall,
    ) -> Result<(), Vec<staple_syntax::Diagnostic>> {
        // Only the calls the differential option actually redirects need
        // concrete descriptors for their run-time type arguments.
        let Some(function) = (match &call.target {
            super::LoweredCallableTarget::DirectFunction { function, .. } => Some(*function),
            _ => None,
        }) else {
            return Ok(());
        };
        if self
            .program
            .differential_call_runtime(function, &std::collections::BTreeSet::new(), false)
            .is_none()
        {
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
