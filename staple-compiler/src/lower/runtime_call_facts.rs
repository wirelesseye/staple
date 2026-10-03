//! Intrinsic facts recorded before LLVM emission.

use super::{LoweredCallArgument, LoweredCallableTarget, LoweredSemanticIds};
use crate::{CheckedFunctionType, CheckedType, IntrinsicFunction};

/// Per-call facts whose the emitter equivalents query checked runtime types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LoweredRuntimeCallFacts {
    pub completion_value_type: Option<CheckedType>,
    pub coroutine: Option<LoweredCoroutineActivation>,
}

/// A coroutine's concrete result and the activation resource slots. Indices
/// address the call's resource_bindings, preserving the deferred row's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoweredCoroutineActivation {
    pub result_type: CheckedType,
    pub deferred_resources: Vec<usize>,
    pub tasks_resource: Option<usize>,
}

impl LoweredCallableTarget {
    /// Intrinsics retain resource metadata even though their runtime ABI does
    /// not receive the normal function's hidden argument list.
    pub(crate) fn records_resources(&self) -> bool {
        match self {
            Self::ExternalFunction { .. } | Self::Constructor { .. } => false,
            Self::Intrinsic { intrinsic, .. } => matches!(
                intrinsic,
                IntrinsicFunction::Spawn | IntrinsicFunction::CoroutineBlockOn
            ),
            _ => true,
        }
    }
}

impl LoweredRuntimeCallFacts {
    /// Recomputed from the concrete call after instance substitution. Runtime
    /// type recognition stays in lowering; emitters read these facts verbatim.
    pub(crate) fn for_call(
        target: &LoweredCallableTarget,
        arguments: &[LoweredCallArgument],
        function: &CheckedFunctionType,
        ids: &LoweredSemanticIds,
    ) -> Result<Self, String> {
        let mut facts = Self::default();
        let LoweredCallableTarget::Intrinsic { intrinsic, .. } = target else {
            return Ok(facts);
        };
        match intrinsic {
            IntrinsicFunction::Completion
            | IntrinsicFunction::CompletionWithCancel
            | IntrinsicFunction::CompletionToken => {
                let CheckedType::Product(product) = function.result.as_ref() else {
                    return Err("completion result is not a wait product".into());
                };
                let Some(CheckedType::Opaque { id, arguments, .. }) =
                    product.elements.first().map(|element| &element.value_type)
                else {
                    return Err("completion result is missing its Wait payload".into());
                };
                if Some(*id) != ids.wait_type || arguments.len() != 1 {
                    return Err("completion result is missing its Wait payload".into());
                }
                facts.completion_value_type = Some(arguments[0].clone());
            }
            IntrinsicFunction::ResolverComplete => {
                facts.completion_value_type = Some(
                    arguments
                        .last()
                        .filter(|_| arguments.len() == 2)
                        .ok_or("resolver completion is missing its value argument")?
                        .expected
                        .clone(),
                );
            }
            IntrinsicFunction::Spawn | IntrinsicFunction::CoroutineBlockOn => {
                let Some(CheckedType::Opaque { id, arguments, .. }) =
                    arguments.first().map(|argument| &argument.expected)
                else {
                    return Err("coroutine activation operand is not a coroutine".into());
                };
                if Some(*id) != ids.coroutine_type || arguments.len() != 2 {
                    return Err("coroutine activation operand is not a coroutine".into());
                }
                let effects = crate::typecheck::effect_substitution_value(&arguments[0])
                    .ok_or("coroutine activation has no deferred effect row")?;
                let deferred_resources = effects
                    .resources
                    .iter()
                    .map(|required| {
                        function
                            .effects
                            .resources
                            .iter()
                            .position(|resource| resource == required)
                            .ok_or_else(|| {
                                format!(
                                    "coroutine activation is missing resource `{}`",
                                    required.value_type
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let tasks_resource = function.effects.resources.iter().position(|resource| {
                    matches!(&resource.value_type, CheckedType::Opaque { id, .. } if Some(*id) == ids.tasks_type)
                });
                if *intrinsic == IntrinsicFunction::Spawn && tasks_resource.is_none() {
                    return Err("spawn is missing its Tasks resource binding".into());
                }
                facts.coroutine = Some(LoweredCoroutineActivation {
                    result_type: arguments[1].clone(),
                    deferred_resources,
                    tasks_resource,
                });
            }
            _ => {}
        }
        Ok(facts)
    }
}
