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

use staple_syntax::{Diagnostic, Span};

use super::artifact_closure::{ClosureRequest, ExpansionResult};
use super::{
    CheckedProductType, ConstructorAdapterPlan, ConstructorConstruction, GcFinalizerPlan,
    LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId, LoweredProgram,
    PlannedArtifact,
};
use crate::specialization::{ArtifactRequestKey, CanonicalType, GcFinalizerKey};
use crate::{CheckedType, CheckedTypeElement, RecursiveConstruction};

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
