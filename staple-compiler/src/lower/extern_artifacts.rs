//! Stage 4.6: the extern-adapter artifact expander and scanner.
//!
//! `expand_extern_adapter` fills one `ExternAdapter` plan from the lowered
//! foreign binding: the adapter's declared arity and the eager-declaration
//! parity facts Stage 5 needs. The adapter body itself is a direct call to the
//! foreign symbol, so the plan names no lowered callee.
//!
//! The scanner walks one owner through the shared Stage 4.4 owner walker and
//! requests one adapter per non-variadic extern binding used as a first-class
//! callable value (`ExternAdapterValue`). Legacy creates an adapter eagerly for
//! every non-variadic extern binding; the artifact records which adapters a
//! callable-value site actually reaches, and the plan's declaration facts keep
//! the eager foreign-symbol parity for Stage 5. A variadic extern used as a
//! first-class value is rejected here with the legacy diagnostic, because the
//! adapter's direct call cannot forward a variadic argument list.

use staple_syntax::{Diagnostic, Span};

use super::artifact_closure::{ArtifactUseSite, ClosureRequest, ExpansionResult, ScanResult};
use super::cleanup_artifacts::{LoweredOwnerVisitor, walk_owner};
use super::emission::OwnerArenas;
use super::{
    ArenaId, ExternAdapterPlan, ExternDeclaration, FunctionInstanceId, InitializerId,
    LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId,
    LoweredCallableAdapter, LoweredCallableTarget, LoweredCallableValue, LoweredCallableValueId,
    LoweredProgram, LoweredSymbol, Origin,
};
use crate::CheckedType;
use crate::specialization::{ArtifactRequestKey, CanonicalFunctionType, ExternAdapterKey};

/// The variadic-value diagnostic, matching the legacy backend's wording so the
/// failure moves phases without changing meaning.
const VARIADIC_VALUE_DIAGNOSTIC: &str =
    "variadic external functions cannot be used as first-class values";

/// Expands one extern adapter: the eager-declaration facts for the foreign
/// symbol the adapter directly calls.
pub(super) fn expand_extern_adapter(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: ExternAdapterPlan,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "extern-adapter expansion received a missing artifact".to_string(),
            )]);
        }
    };
    if let Some(problem) = variadic_parameter(&plan.callable_type) {
        return Err(vec![Diagnostic::new(origin.span.clone(), problem)]);
    }
    let Some(symbol) = program.symbols.get(plan.symbol) else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!("extern adapter names missing symbol {}", plan.symbol.0),
        )]);
    };
    if !symbol.external {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "extern adapter names symbol {} that is not an external binding",
                plan.symbol.0
            ),
        )]);
    }
    check_symbol_type(&plan, symbol, &origin)?;
    let declaration = ExternDeclaration {
        arity: declared_arity(&plan),
        eagerly_declared: true,
    };
    let indirect_parameters = adapter_indirect_parameters(program, &plan.callable_type);
    Ok((
        LoweredArtifactPlan::ExternAdapter(ExternAdapterPlan {
            symbol: plan.symbol,
            callable_type: plan.callable_type,
            indirect_parameters,
            declaration: Some(declaration),
        }),
        Vec::new(),
    ))
}

/// The adapter's closure-ABI pass modes: whether each flattened value
/// parameter is a pointer that the adapter must load before the native call.
/// This mirrors `Backend::indirect_parameter_mask` over the same `Copy`
/// decision the backend's `LayoutContext` uses.
fn adapter_indirect_parameters(
    program: &LoweredProgram,
    callable_type: &crate::CheckedFunctionType,
) -> Vec<bool> {
    let types = super::flattened_parameter_types(&callable_type.parameter);
    let mutation_mask = super::mutation_slot_mask(types.len(), &callable_type.mutations);
    let move_mask = super::mutation_slot_mask(types.len(), &callable_type.moves);
    types
        .iter()
        .enumerate()
        .map(|(index, value_type)| {
            mutation_mask[index] || (!move_mask[index] && !program.concrete_is_copy(value_type))
        })
        .collect()
}

/// The adapter's declared arity, mirroring legacy's overloaded `name.arityN`
/// spelling: a juxtaposed parameter product counts its elements, everything
/// else is one parameter.
fn declared_arity(plan: &ExternAdapterPlan) -> usize {
    if plan.callable_type.parameter_style == staple_syntax::FunctionParameterStyle::Juxtaposed {
        match plan.callable_type.parameter.as_ref() {
            CheckedType::Product(product) => product.elements.len(),
            _ => 1,
        }
    } else {
        1
    }
}

/// A variadic parameter makes the adapter impossible, mirroring
/// `declare_external_functions`, which skips adapter creation for variadic
/// externs.
fn variadic_parameter(callable_type: &crate::CheckedFunctionType) -> Option<String> {
    match callable_type.parameter.as_ref() {
        CheckedType::Product(product) if product.variadic => {
            Some(VARIADIC_VALUE_DIAGNOSTIC.to_string())
        }
        _ => None,
    }
}

/// The external binding's own checked type must be the callable type the
/// adapter exposes; a mismatch means the requester read the wrong symbol.
fn check_symbol_type(
    plan: &ExternAdapterPlan,
    symbol: &LoweredSymbol,
    origin: &Origin,
) -> Result<(), Vec<Diagnostic>> {
    let matches = match &symbol.value_type {
        CheckedType::Function(function_type) => function_type == &plan.callable_type,
        _ => false,
    };
    if matches {
        return Ok(());
    }
    Err(vec![Diagnostic::new(
        origin.span.clone(),
        format!(
            "extern adapter for symbol {} disagrees with the external binding's checked type",
            plan.symbol.0
        ),
    )])
}

/// Scans one materialized instance body for extern callable values.
pub(super) fn scan_instance(program: &LoweredProgram, instance: FunctionInstanceId) -> ScanResult {
    let Some(record) = program.instances.get(instance) else {
        return Ok(Vec::new());
    };
    let Some(body) = record.body.as_ref() else {
        return Ok(Vec::new());
    };
    let mut visitor = ExternScanVisitor {
        program,
        requests: Vec::new(),
    };
    walk_owner(program, OwnerArenas::Instance(body), &mut visitor)?;
    Ok(visitor.requests)
}

/// Scans one module initializer for extern callable values.
pub(super) fn scan_initializer(program: &LoweredProgram, initializer: InitializerId) -> ScanResult {
    if program.initializers.get(initializer).is_none() {
        return Ok(Vec::new());
    }
    let mut visitor = ExternScanVisitor {
        program,
        requests: Vec::new(),
    };
    walk_owner(program, OwnerArenas::Initializer(initializer), &mut visitor)?;
    Ok(visitor.requests)
}

/// The scanning visitor: every extern callable value becomes one adapter
/// request with its exact use site.
struct ExternScanVisitor<'program> {
    program: &'program LoweredProgram,
    requests: Vec<ClosureRequest>,
}

impl LoweredOwnerVisitor for ExternScanVisitor<'_> {
    fn callable_value_site(
        &mut self,
        id: LoweredCallableValueId,
        value: &LoweredCallableValue,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        if value.adapter != LoweredCallableAdapter::External {
            return Ok(());
        }
        let LoweredCallableTarget::ExternalFunction { symbol } = &value.target else {
            return Ok(());
        };
        if let Some(problem) = variadic_parameter(&value.function_type) {
            return Err(vec![Diagnostic::new(origin.span.clone(), problem)]);
        }
        let callable_type = CanonicalFunctionType::concrete(&value.function_type, origin)
            .map_err(|diagnostic| vec![diagnostic])?;
        self.requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::ExternAdapter(ExternAdapterKey {
                symbol: *symbol,
                callable_type,
            }),
            plan: LoweredArtifactPlan::ExternAdapter(ExternAdapterPlan {
                symbol: *symbol,
                callable_type: value.function_type.clone(),
                indirect_parameters: adapter_indirect_parameters(
                    self.program,
                    &value.function_type,
                ),
                declaration: None,
            }),
            kind: LoweredArtifactDependencyKind::ExternAdapter,
            origin: origin.clone(),
            use_site: Some(ArtifactUseSite::ExternAdapterValue(id)),
        });
        Ok(())
    }
}

/// Validates the Stage 4.6 extern-adapter plans and uses:
///
/// - every expanded adapter re-expands to itself from the lowered symbol, so a
///   plan whose symbol is not a matching external binding is rejected;
/// - every `ExternAdapterValue` use names the adapter key its callable value
///   builds, so a use can never be bound to another extern or another
///   callable type.
pub(super) fn check_stage_4_6(program: &LoweredProgram, diagnostics: &mut Vec<Diagnostic>) {
    for (id, artifact) in program.artifacts.iter() {
        let Some(LoweredArtifactPlan::ExternAdapter(plan)) = artifact.plan.clone() else {
            continue;
        };
        if plan.declaration.is_none() {
            continue; // The registered expander rejects markers itself.
        }
        match expand_extern_adapter(program, id, plan.clone()) {
            Ok((rebuilt, _)) => {
                if !rebuilt.eq_ignoring_bindings(&LoweredArtifactPlan::ExternAdapter(plan)) {
                    diagnostics.push(Diagnostic::new(
                        artifact.origin.span.clone(),
                        format!(
                            "extern-adapter artifact {} disagrees with its lowered external binding",
                            id.index()
                        ),
                    ));
                }
            }
            Err(mut problems) => diagnostics.append(&mut problems),
        }
    }
    check_adapter_uses(program, diagnostics);
}

/// Every `ExternAdapterValue` use names the same symbol and callable type its
/// callable value builds.
fn check_adapter_uses(program: &LoweredProgram, diagnostics: &mut Vec<Diagnostic>) {
    for (_, instance) in program.instances.iter() {
        let Some(body) = instance.body.as_ref() else {
            continue;
        };
        for use_ in &body.artifact_uses {
            let ArtifactUseSite::ExternAdapterValue(value) = use_.site else {
                continue;
            };
            check_adapter_use(program, body.callable_value(value), use_, diagnostics);
        }
    }
    for (index, _) in program.initializers.iter() {
        let Some(uses) = program.initializer_artifact_uses.get(index.index()) else {
            continue;
        };
        for use_ in uses {
            let ArtifactUseSite::ExternAdapterValue(value) = use_.site else {
                continue;
            };
            check_adapter_use(
                program,
                program.callable_values.get(value),
                use_,
                diagnostics,
            );
        }
    }
}

fn check_adapter_use(
    program: &LoweredProgram,
    value: Option<&LoweredCallableValue>,
    use_: &super::LoweredArtifactUse,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(ArtifactRequestKey::ExternAdapter(key)) =
        program.specializations.artifact(use_.artifact)
    else {
        return; // The key agreement check reports a mismatched key.
    };
    let Some(value) = value else {
        diagnostics.push(Diagnostic::new(
            use_.origin.span.clone(),
            "an extern adapter use names a missing callable value".to_string(),
        ));
        return;
    };
    let LoweredCallableTarget::ExternalFunction { symbol } = &value.target else {
        diagnostics.push(Diagnostic::new(
            use_.origin.span.clone(),
            "an extern adapter use names a callable value that is not an extern".to_string(),
        ));
        return;
    };
    let matches = key.symbol == *symbol
        && CanonicalFunctionType::concrete(&value.function_type, &use_.origin)
            .is_ok_and(|callable_type| callable_type == key.callable_type);
    if !matches {
        diagnostics.push(Diagnostic::new(
            use_.origin.span.clone(),
            "an extern adapter use names a different symbol or callable type than its value"
                .to_string(),
        ));
    }
}
