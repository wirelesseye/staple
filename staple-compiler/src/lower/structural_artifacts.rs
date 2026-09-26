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
    CheckedProductType, ConstructorAdapterPlan, ConstructorConstruction, DropGluePlan,
    GcFinalizerPlan, IndexedElement, LoweredArtifactDependencyKind, LoweredArtifactPlan,
    LoweredArtifactRequestId, LoweredProgram, Origin, PlannedArtifact, StructuralBody,
    StructuralMethodPlan, SumAlternative,
};
use crate::specialization::{ArtifactRequestKey, CanonicalType, GcFinalizerKey};
use crate::{CheckedType, CheckedTypeElement, RecursiveConstruction, StructuralTraitMethod};

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
        StructuralTraitMethod::Debug
        | StructuralTraitMethod::DerefIndex
        | StructuralTraitMethod::DerefMutateIndex => {
            return Ok((LoweredArtifactPlan::StructuralMethod(plan), Vec::new()));
        }
    };
    Ok((
        LoweredArtifactPlan::StructuralMethod(StructuralMethodPlan { body, ..plan }),
        requests,
    ))
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
