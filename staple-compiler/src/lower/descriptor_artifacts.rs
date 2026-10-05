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
