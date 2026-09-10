//! Coroutine lowering plan.
//!
//! Runs after type checking and ownership analysis. For every `coro { ... }`
//! expression it records the metadata code generation needs to emit the
//! coroutine's frame, `resume`, and `cleanup` functions.
//!
//! A coroutine `resume` is a state machine: `switch` on the frame's resume
//! state to the block after the last `await`, with all body-local `let`
//! bindings held in frame cells so they survive a suspension. `await` is
//! restricted to statement position (`let x = await e` or `await e;`) so no
//! mid-expression SSA temporary has to be spilled; the driver
//! (`__staple_coro_drive`) trampolines nested child coroutines so no native
//! stack survives a suspension.

use std::collections::{HashMap, HashSet};

use staple_syntax::{Diagnostic, Expression, Item, Pattern, SyntaxId};

use crate::{CheckedEffectSet, CheckedType, IntrinsicFunction, SymbolId, TypedModule};

#[derive(Debug, Clone)]
pub(crate) struct CoroutinePlan {
    /// The `coro` body block's syntax id — the key the implicit thunk and this
    /// plan share.
    #[allow(dead_code)]
    pub body_syntax: SyntaxId,
    /// The value the coroutine yields (`T` in `Coroutine{E} T`).
    pub result_type: CheckedType,
    /// The coroutine's deferred effect row (`E`).
    pub deferred_effects: CheckedEffectSet,
    /// Symbols captured by the body; code generation reads this from the
    /// implicit thunk directly, kept here for completeness.
    #[allow(dead_code)]
    pub captures: Vec<SymbolId>,
    /// Number of `await` suspension points in the body (not counting nested
    /// `coro` / function bodies). Resume states are `0..=resume_points`.
    pub resume_points: usize,
    /// Body-local `let` bindings, in source order. Each becomes a frame cell so
    /// its value survives across a suspension.
    pub frame_bindings: Vec<SymbolId>,
    /// The result type of each `await` in the body; code generation sizes the
    /// frame's pending-result scratch to the largest of these.
    pub await_result_types: Vec<CheckedType>,
    /// Resume-state indices (`1..=resume_points`) whose `await` parks on a
    /// `Wait` (completion). The cancel unwind must call
    /// `__staple_completion_abandon` on the stashed record for these.
    pub wait_await_states: Vec<usize>,
    /// Resume-state indices whose `await` parks on an `until` child coroutine —
    /// which parks off-queue on an internal completion, so the cancel unwind can
    /// safely run its `cleanup` (tearing down the reaction subscription). Other
    /// child coroutines (`yield_now`, ordinary `coro`) may still be queued, so
    /// they are left for the driver to clean up.
    pub until_await_states: Vec<usize>,
}

/// The `coro` body block syntax ids in the module. Available before ownership
/// analysis (it only needs the parsed AST), so ownership can treat a coroutine
/// body's captures as owned rather than borrowed.
pub(crate) fn coroutine_body_ids(module: &TypedModule) -> HashSet<SyntaxId> {
    let mut coros = Vec::new();
    for source_module in module.resolved().program().modules() {
        for item in &source_module.syntax.items {
            collect_item(item, &mut coros);
        }
    }
    coros.into_iter().map(|coro| coro.body.syntax.id).collect()
}

/// Builds a `coro`-body-syntax-id → plan map for every coroutine in the module,
/// plus any diagnostics from the statement-position restriction on `await`.
pub(crate) fn plan(module: &TypedModule) -> (HashMap<SyntaxId, CoroutinePlan>, Vec<Diagnostic>) {
    let mut coros = Vec::new();
    for source_module in module.resolved().program().modules() {
        for item in &source_module.syntax.items {
            collect_item(item, &mut coros);
        }
    }

    let mut plans = HashMap::new();
    let mut diagnostics = Vec::new();
    for coro in coros {
        let coroutine_type = module.type_of_expression(coro.syntax.id);
        let (deferred_effects, result_type) = coroutine_type
            .and_then(|ty| module.coroutine_parts(ty))
            .map(|(effects, result)| (effects.clone(), result.clone()))
            .unwrap_or((CheckedEffectSet::default(), CheckedType::Error));
        let captures = module
            .implicit_thunk_for(coro.body.syntax.id)
            .map(|thunk| thunk.captures.clone())
            .unwrap_or_default();

        let mut info = BodyInfo::default();
        for item in &coro.body.items {
            scan_item(module, item, &mut info, &mut diagnostics);
        }

        plans.insert(
            coro.body.syntax.id,
            CoroutinePlan {
                body_syntax: coro.body.syntax.id,
                result_type,
                deferred_effects,
                captures,
                resume_points: info.awaits.len(),
                frame_bindings: info.bindings,
                await_result_types: info
                    .awaits
                    .iter()
                    .map(|site| {
                        module
                            .type_of_expression(site.await_syntax)
                            .cloned()
                            .unwrap_or(CheckedType::Error)
                    })
                    .collect(),
                wait_await_states: info
                    .awaits
                    .iter()
                    .enumerate()
                    .filter(|(_, site)| {
                        module
                            .type_of_expression(site.operand_syntax)
                            .is_some_and(|ty| module.is_wait_type(ty))
                    })
                    .map(|(index, _)| index + 1)
                    .collect(),
                until_await_states: info
                    .awaits
                    .iter()
                    .enumerate()
                    .filter(|(_, site)| site.is_until)
                    .map(|(index, _)| index + 1)
                    .collect(),
            },
        );
    }
    (plans, diagnostics)
}

#[derive(Default)]
struct BodyInfo {
    /// Each `await` suspension point in the body, in source order.
    awaits: Vec<AwaitSite>,
    bindings: Vec<SymbolId>,
}

struct AwaitSite {
    await_syntax: SyntaxId,
    operand_syntax: SyntaxId,
    /// The operand is a call to the `until` intrinsic.
    is_until: bool,
}

fn is_until_call(module: &TypedModule, operand: &Expression) -> bool {
    // `(until { p })` parses as a single-element product; look through it.
    let operand = match operand {
        Expression::Product(product) if product.elements.len() == 1 => {
            &product.elements[0].value
        }
        other => other,
    };
    let Expression::Call(call) = operand else {
        return false;
    };
    module
        .symbol_for(call.callee.syntax().id)
        .and_then(|symbol| module.resolved().intrinsic_function(symbol))
        == Some(IntrinsicFunction::Until)
}

/// Walks one coroutine body, counting `await` suspension points, collecting
/// `let` binding symbols for frame cells, and rejecting `await` outside
/// statement position. Does not descend into nested `coro` or function bodies —
/// those are separate coroutines / closures.
fn scan_item(
    module: &TypedModule,
    item: &Item,
    info: &mut BodyInfo,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match item {
        Item::Binding(binding) => {
            if let Some(symbol) = module.symbol_for(binding.syntax.id) {
                info.bindings.push(symbol);
            }
            if let Some(value) = &binding.value {
                if let Expression::Await(await_) = value {
                    info.awaits.push(AwaitSite { await_syntax: await_.syntax.id, operand_syntax: await_.operand.syntax().id, is_until: is_until_call(module, &await_.operand) });
                    scan_expression(module, &await_.operand, info, diagnostics);
                } else {
                    scan_expression(module, value, info, diagnostics);
                }
            }
        }
        Item::PatternBinding(binding) => {
            collect_pattern_bindings(module, &binding.pattern, &mut info.bindings);
            if let Expression::Await(await_) = &binding.value {
                info.awaits.push(AwaitSite { await_syntax: await_.syntax.id, operand_syntax: await_.operand.syntax().id, is_until: is_until_call(module, &await_.operand) });
                scan_expression(module, &await_.operand, info, diagnostics);
            } else {
                scan_expression(module, &binding.value, info, diagnostics);
            }
        }
        Item::Assignment(assignment) => {
            scan_expression(module, &assignment.target, info, diagnostics);
            scan_expression(module, &assignment.value, info, diagnostics);
        }
        Item::Return(item) => scan_expression(module, &item.value, info, diagnostics),
        Item::Break(item) => {
            if let Some(value) = &item.value {
                scan_expression(module, value, info, diagnostics);
            }
        }
        Item::Expression(Expression::Await(await_)) => {
            info.awaits.push(AwaitSite { await_syntax: await_.syntax.id, operand_syntax: await_.operand.syntax().id, is_until: is_until_call(module, &await_.operand) });
            scan_expression(module, &await_.operand, info, diagnostics);
        }
        Item::Expression(expression) => scan_expression(module, expression, info, diagnostics),
        _ => {}
    }
}

/// Walks a sub-expression of a coroutine body. Any `await` reached here is in
/// operand / sub-expression position, which v1 does not lower.
fn scan_expression(
    module: &TypedModule,
    expression: &Expression,
    info: &mut BodyInfo,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match expression {
        Expression::Await(await_) => {
            diagnostics.push(Diagnostic::new(
                await_.syntax.span.clone(),
                "`await` must be a statement inside the coroutine body \
                 (`let x = await e` or `await e`), not a sub-expression",
            ));
            scan_expression(module, &await_.operand, info, diagnostics);
        }
        // Nested coroutines and functions are compiled separately.
        Expression::Coro(_) | Expression::Function(_) => {}
        Expression::Satisfies(value) => {
            scan_expression(module, &value.value, info, diagnostics)
        }
        Expression::Match(match_) => {
            scan_expression(module, &match_.subject, info, diagnostics);
            for arm in &match_.arms {
                collect_pattern_bindings(module, &arm.pattern, &mut info.bindings);
                if let Expression::Block(block) = &arm.body {
                    for item in &block.items {
                        scan_item(module, item, info, diagnostics);
                    }
                } else if let Expression::Await(await_) = &arm.body {
                    info.awaits.push(AwaitSite { await_syntax: await_.syntax.id, operand_syntax: await_.operand.syntax().id, is_until: is_until_call(module, &await_.operand) });
                    scan_expression(module, &await_.operand, info, diagnostics);
                } else {
                    scan_expression(module, &arm.body, info, diagnostics);
                }
            }
        }
        Expression::Loop(loop_) => {
            for item in &loop_.body.items {
                scan_item(module, item, info, diagnostics);
            }
        }
        Expression::With(with) => {
            scan_expression(module, &with.value, info, diagnostics);
            for item in &with.body.items {
                scan_item(module, item, info, diagnostics);
            }
        }
        Expression::Block(block) => {
            for item in &block.items {
                scan_item(module, item, info, diagnostics);
            }
        }
        Expression::Product(product) => {
            for element in &product.elements {
                scan_expression(module, &element.value, info, diagnostics);
            }
        }
        Expression::RepeatedProduct(repeated) => {
            scan_expression(module, &repeated.value, info, diagnostics);
            scan_expression(module, &repeated.count, info, diagnostics);
        }
        Expression::Call(call) => {
            scan_expression(module, &call.callee, info, diagnostics);
            scan_expression(module, &call.argument, info, diagnostics);
        }
        Expression::Access(access) => {
            scan_expression(module, &access.value, info, diagnostics)
        }
        Expression::Index(index) => {
            scan_expression(module, &index.value, info, diagnostics);
            scan_expression(module, &index.index, info, diagnostics);
        }
        Expression::Unary(unary) => {
            scan_expression(module, &unary.operand, info, diagnostics);
        }
        Expression::Binary(binary) => {
            scan_expression(module, &binary.left, info, diagnostics);
            scan_expression(module, &binary.right, info, diagnostics);
        }
        Expression::Logical(logical) => {
            scan_expression(module, &logical.left, info, diagnostics);
            scan_expression(module, &logical.right, info, diagnostics);
        }
        Expression::StringTemplate(template) => {
            for part in &template.parts {
                if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                    scan_expression(module, &interpolation.expression, info, diagnostics);
                }
            }
        }
        Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Resource(_)
        | Expression::Name(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => {}
    }
}

fn collect_pattern_bindings(module: &TypedModule, pattern: &Pattern, out: &mut Vec<SymbolId>) {
    match pattern {
        Pattern::Binding(binding) => {
            if let Some(symbol) = module.symbol_for(binding.syntax.id) {
                out.push(symbol);
            }
        }
        Pattern::At(at) => {
            collect_pattern_bindings(
                module,
                &Pattern::Binding(at.binding.as_ref().clone()),
                out,
            );
            collect_pattern_bindings(module, &at.pattern, out);
        }
        Pattern::Product(product) => {
            for element in &product.elements {
                collect_pattern_bindings(module, element, out);
            }
        }
        Pattern::Nominal(nominal) => collect_pattern_bindings(module, &nominal.argument, out),
        Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
    }
}

// --- coroutine discovery (used by `plan` and `coroutine_body_ids`) ---

fn collect_item<'a>(item: &'a Item, out: &mut Vec<&'a staple_syntax::CoroExpression>) {
    match item {
        Item::Binding(binding) => {
            if let Some(value) = &binding.value {
                collect_expression(value, out);
            }
        }
        Item::PatternBinding(binding) => collect_expression(&binding.value, out),
        Item::Assignment(assignment) => {
            collect_expression(&assignment.target, out);
            collect_expression(&assignment.value, out);
        }
        Item::Return(item) => collect_expression(&item.value, out),
        Item::Break(item) => {
            if let Some(value) = &item.value {
                collect_expression(value, out);
            }
        }
        Item::Expression(expression) => collect_expression(expression, out),
        Item::Submodule(submodule) => {
            for item in &submodule.module.items {
                collect_item(item, out);
            }
        }
        _ => {}
    }
}

fn collect_expression<'a>(
    expression: &'a Expression,
    out: &mut Vec<&'a staple_syntax::CoroExpression>,
) {
    match expression {
        Expression::Coro(coro) => {
            out.push(coro);
            for item in &coro.body.items {
                collect_item(item, out);
            }
        }
        Expression::Await(await_) => collect_expression(&await_.operand, out),
        Expression::Function(function) => collect_expression(&function.body, out),
        Expression::Satisfies(satisfies) => collect_expression(&satisfies.value, out),
        Expression::Match(match_) => {
            collect_expression(&match_.subject, out);
            for arm in &match_.arms {
                collect_expression(&arm.body, out);
            }
        }
        Expression::Loop(loop_) => {
            for item in &loop_.body.items {
                collect_item(item, out);
            }
        }
        Expression::With(with) => {
            collect_expression(&with.value, out);
            for item in &with.body.items {
                collect_item(item, out);
            }
        }
        Expression::Block(block) => {
            for item in &block.items {
                collect_item(item, out);
            }
        }
        Expression::Product(product) => {
            for element in &product.elements {
                collect_expression(&element.value, out);
            }
        }
        Expression::RepeatedProduct(repeated) => {
            collect_expression(&repeated.value, out);
            collect_expression(&repeated.count, out);
        }
        Expression::Call(call) => {
            collect_expression(&call.callee, out);
            collect_expression(&call.argument, out);
        }
        Expression::Access(access) => collect_expression(&access.value, out),
        Expression::Index(index) => {
            collect_expression(&index.value, out);
            collect_expression(&index.index, out);
        }
        Expression::Unary(unary) => {
            collect_expression(&unary.operand, out);
        }
        Expression::Binary(binary) => {
            collect_expression(&binary.left, out);
            collect_expression(&binary.right, out);
        }
        Expression::Logical(logical) => {
            collect_expression(&logical.left, out);
            collect_expression(&logical.right, out);
        }
        Expression::StringTemplate(template) => {
            for part in &template.parts {
                if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                    collect_expression(&interpolation.expression, out);
                }
            }
        }
        Expression::Quote(_)
        | Expression::Splice(_)
        | Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Resource(_)
        | Expression::Name(_)
        | Expression::String(_)
        | Expression::CString(_)
        | Expression::Integer(_)
        | Expression::Float(_) => {}
    }
}
