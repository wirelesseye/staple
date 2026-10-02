//! Ownership cleanup and garbage-collector finalizer planning.
//!
//! Drop plans select user implementations, runtime releases, and recursive
//! product, sum, and distinct cleanup. Canonical key deduplication terminates
//! recursive requests. Finalizers record payload, cell, capture, and buffer
//! cleanup; scanners attach uses to owners and collect owned bindings in order.
//! Codegen expands these validated drop plans inline at their recorded sites.

use std::collections::HashSet;

use staple_syntax::{Diagnostic, Span};

use super::artifact_closure::{ArtifactUseSite, ClosureRequest, ExpansionResult, ScanResult};
use super::emission::OwnerArenas;
use super::instance_resolution::{
    InstanceResolutionRequest, InstanceResolutionTarget, RuntimeOpaqueKind,
};
use super::{
    ArenaId, BlockId, DropGlueBody, DropGluePlan, DroppedAlternative, DroppedCapture,
    DroppedElement, ExpressionId, GcFinalizerPlan, InitializerId, ItemId,
    LoweredArtifactDependencyKind, LoweredArtifactPlan, LoweredArtifactRequestId,
    LoweredInstanceDependencyKind, LoweredItemKind, LoweredOwnedBinding, LoweredProgram, Origin,
    OwnedStorage, PatternId, PlaceId, PlannedArtifact, PlannedCallee, PlannedInstance,
    RuntimeRelease, SymbolId,
};
use crate::specialization::{ArtifactRequestKey, CanonicalType, GcFinalizerKey};
use crate::{CheckedType, IntrinsicFunction};

/// Expands one drop-glue artifact: the selected cleanup body for the plan's
/// concrete value type, mirroring the emitter decision order, plus the nested
/// glue requests the body needs.
pub(super) fn expand_drop_glue(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: DropGluePlan,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "drop-glue expansion received a missing artifact".to_string(),
            )]);
        }
    };
    let value_type = plan.value_type.clone();
    if !program.concrete_needs_drop(&value_type) {
        // A `DropGlue` key is only ever requested for a type that needs drop;
        // requesting one for a type the emitter would no-op means the requester's
        // `needs_drop` predicate and this expander disagree.
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!("drop glue for `{value_type}` does not need drop"),
        )]);
    }
    let mut requests = Vec::new();
    let body = drop_glue_body(program, &value_type, &origin, &mut requests)?;
    Ok((
        LoweredArtifactPlan::DropGlue(DropGluePlan { value_type, body }),
        requests,
    ))
}

/// Expands one finalizer artifact: the referenced drop glue for the payload,
/// cell value, or buffer element, or the ordered capture drops for a closure
/// environment. The closure environment reads its capture metadata from the
/// closure instance's own materialized body, never from the requesting site.
pub(super) fn expand_gc_finalizer(
    program: &LoweredProgram,
    artifact: LoweredArtifactRequestId,
    plan: GcFinalizerPlan,
) -> ExpansionResult {
    let origin = match program.artifacts.get(artifact) {
        Some(record) => record.origin.clone(),
        None => {
            return Err(vec![Diagnostic::new(
                Span::Compiler,
                "gc-finalizer expansion received a missing artifact".to_string(),
            )]);
        }
    };
    let mut requests = Vec::new();
    let plan = match plan {
        GcFinalizerPlan::Payload { value_type, .. } => {
            let glue = request_finalizer_glue(program, &value_type, &origin, &mut requests)?;
            GcFinalizerPlan::Payload {
                value_type,
                glue: Some(glue),
            }
        }
        GcFinalizerPlan::Cell { value_type, .. } => {
            let glue = request_finalizer_glue(program, &value_type, &origin, &mut requests)?;
            GcFinalizerPlan::Cell {
                value_type,
                glue: Some(glue),
            }
        }
        GcFinalizerPlan::Buffer { element, .. } => {
            let glue = request_finalizer_glue(program, &element, &origin, &mut requests)?;
            GcFinalizerPlan::Buffer {
                element,
                glue: Some(glue),
            }
        }
        GcFinalizerPlan::ClosureEnvironment {
            closure,
            captures,
            drops: _,
        } => {
            let drops =
                closure_environment_drops(program, closure, &captures, &origin, &mut requests)?;
            GcFinalizerPlan::ClosureEnvironment {
                closure,
                captures,
                drops: Some(drops),
            }
        }
    };
    Ok((LoweredArtifactPlan::GcFinalizer(plan), requests))
}

/// Requests the drop glue a payload/cell/element finalizer calls. The
/// finalizer only exists when its value needs drop, so a non-droppable type is
/// a requester/expander disagreement.
fn request_finalizer_glue(
    program: &LoweredProgram,
    value_type: &CheckedType,
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<PlannedArtifact, Vec<Diagnostic>> {
    if !program.concrete_needs_drop(value_type) {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!("gc finalizer value `{value_type}` does not need drop"),
        )]);
    }
    request_drop_glue(program, value_type, origin, requests)
}

/// The captures a closure-environment finalizer drops, in reverse capture
/// order, mirroring ensure closure finalizer: skip captures that require
/// initialization state, have mutable storage, are derived, or are borrowed,
/// then drop the rest that need drop.
fn closure_environment_drops(
    program: &LoweredProgram,
    closure: crate::FunctionInstanceId,
    captures: &[CheckedType],
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<Vec<DroppedCapture>, Vec<Diagnostic>> {
    let Some(instance) = program.instances.get(closure) else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "closure finalizer names missing function instance {}",
                closure.index()
            ),
        )]);
    };
    let Some(body) = instance.body.as_ref() else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "closure finalizer names function instance {} with no materialized body",
                closure.index()
            ),
        )]);
    };
    let body_captures = body.captures();
    if body_captures.len() != captures.len() {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!(
                "closure finalizer for instance {} lists {} captures but the closure has {}",
                closure.index(),
                captures.len(),
                body_captures.len()
            ),
        )]);
    }
    for (capture, expected) in body_captures.iter().zip(captures) {
        let actual = CanonicalType::concrete(&capture.value_type, origin)
            .map_err(|diagnostic| vec![diagnostic])?;
        let expected =
            CanonicalType::concrete(expected, origin).map_err(|diagnostic| vec![diagnostic])?;
        if actual != expected {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                format!(
                    "closure finalizer for instance {} lists a capture type that disagrees with the closure's own capture metadata",
                    closure.index()
                ),
            )]);
        }
    }
    let mut drops = Vec::new();
    for (index, capture) in body_captures.iter().enumerate().rev() {
        if capture.requires_initialization_state
            || capture.mutable_storage
            || capture.derived
            || capture.capture.borrowed
        {
            continue;
        }
        if !program.concrete_needs_drop(&capture.value_type) {
            continue;
        }
        let glue = request_finalizer_glue(program, &capture.value_type, origin, requests)?;
        drops.push(DroppedCapture {
            index,
            value_type: capture.value_type.clone(),
            glue,
        });
    }
    Ok(drops)
}

// ---------------------------------------------------------------------------
// artifact planning scanner and owned-binding collector.
// ---------------------------------------------------------------------------

/// One owned-binding draft produced by the shared walk, before its glue is
/// bound through the owner's use records. Visible to sibling scanners that
/// implement the family-neutral visitor.
pub(super) struct OwnedBindingDraft {
    symbol: SymbolId,
    storage: OwnedStorage,
    value_type: CheckedType,
    origin: Origin,
}

/// The sites one owner walk reports. The walker itself is family-neutral: the
/// artifact planning cleanup scanner and the artifact planning coroutine/reactive scanners each
/// override only the hooks they own (every hook defaults to ignoring the
/// site), so the traversal exists once.
pub(super) trait LoweredOwnerVisitor {
    fn drop_site(
        &mut self,
        _site: ArtifactUseSite,
        _value_type: &CheckedType,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn finalizer_site(
        &mut self,
        _site: ArtifactUseSite,
        _key: GcFinalizerKey,
        _plan: GcFinalizerPlan,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn owned_binding(&mut self, _draft: OwnedBindingDraft) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn cell_finalizer(
        &mut self,
        _symbol: SymbolId,
        _value_type: &CheckedType,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn instance_use(
        &mut self,
        _site: ArtifactUseSite,
        _resolved: super::instance_resolution::ResolvedInstanceRequest,
        _kind: LoweredInstanceDependencyKind,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// A `coro` creation in this owner.
    fn coro_creation(
        &mut self,
        _id: super::LoweredCoroId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One reactive-operation occurrence in this owner (`LoweredCall.reactive`,
    /// a binding's derived creation, a name's tracked read, or an assignment's
    /// write notification).
    fn reactive_operation(
        &mut self,
        _id: super::LoweredReactiveOperationId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One complete call after its callee and arguments were visited. Runtime
    /// requirements are recorded from the call's target.
    fn call_site(&mut self, _call: &super::LoweredCall) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One complete call with its owner-local ID. emission binds initializer
    /// dispatch sites here; the default keeps every other visitor unchanged.
    fn call_id_site(
        &mut self,
        _id: super::LoweredCallId,
        _call: &super::LoweredCall,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One caller-level item with its owner-local ID, before it is walked.
    fn item_site(
        &mut self,
        _id: ItemId,
        _item: &super::LoweredItem,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One expression with its owner-local ID, before it is walked.
    fn expression_site(
        &mut self,
        _id: ExpressionId,
        _expression: &super::LoweredExpression,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One `await` record with its owner-local ID, before its operand.
    fn await_id_site(
        &mut self,
        _id: super::LoweredAwaitId,
        _await_: &super::LoweredAwait,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// Whether the walk descends into a derived binding's value expression.
    /// The emitter never evaluates a derived binding inline, so cleanup scanning
    /// stops at the binding; emission's initializer binder mirrors the
    /// worklist traversal, which requests the value's sites under the
    /// enclosing owner.
    fn walks_derived_binding_values(&self) -> bool {
        false
    }

    /// One first-class callable value. artifact planning requests extern adapters and
    /// records closure-environment requirements here.
    fn callable_value_site(
        &mut self,
        _id: super::LoweredCallableValueId,
        _value: &super::LoweredCallableValue,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One binding item. artifact planning records captured binding cells here.
    fn binding_site(
        &mut self,
        _binding: &super::LoweredBindingItem,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One pattern. artifact planning records string-literal comparison here.
    fn pattern_site(
        &mut self,
        _pattern: &super::LoweredPattern,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One string literal expression. artifact planning records its GC-allocated data
    /// here; a `c_string` literal lowers to `CString` instead and never reaches
    /// this hook.
    fn string_literal_site(&mut self, _origin: &Origin) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One string template. artifact planning records literal-data allocation here.
    fn string_template_site(
        &mut self,
        _template: &super::LoweredStringTemplate,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    /// One `await` record. artifact planning records the completion surfaces the
    /// suspension implies.
    fn await_site(
        &mut self,
        _await_: &super::LoweredAwait,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }
}

/// The scanning visitor: every site becomes a closure request with its exact
/// use site, so the engine records a use and an edge.
struct ScanVisitor<'a> {
    program: &'a LoweredProgram,
    requests: Vec<ClosureRequest>,
}

impl ScanVisitor<'_> {
    fn request_drop(
        &mut self,
        site: ArtifactUseSite,
        value_type: &CheckedType,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        if !self.program.concrete_needs_drop(value_type) {
            return Ok(());
        }
        let canonical =
            CanonicalType::concrete(value_type, origin).map_err(|diagnostic| vec![diagnostic])?;
        self.requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::DropGlue(canonical),
            plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
                value_type: value_type.clone(),
                body: DropGlueBody::Unexpanded,
            }),
            kind: LoweredArtifactDependencyKind::DropGlue,
            origin: origin.clone(),
            use_site: Some(site),
        });
        Ok(())
    }
}

impl LoweredOwnerVisitor for ScanVisitor<'_> {
    fn drop_site(
        &mut self,
        site: ArtifactUseSite,
        value_type: &CheckedType,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.request_drop(site, value_type, origin)
    }

    fn finalizer_site(
        &mut self,
        site: ArtifactUseSite,
        key: GcFinalizerKey,
        plan: GcFinalizerPlan,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(key),
            plan: LoweredArtifactPlan::GcFinalizer(plan),
            kind: LoweredArtifactDependencyKind::GcFinalizer,
            origin: origin.clone(),
            use_site: Some(site),
        });
        Ok(())
    }

    fn owned_binding(&mut self, draft: OwnedBindingDraft) -> Result<(), Vec<Diagnostic>> {
        self.request_drop(
            ArtifactUseSite::OwnedBinding(draft.symbol),
            &draft.value_type,
            &draft.origin,
        )
    }

    fn cell_finalizer(
        &mut self,
        symbol: SymbolId,
        value_type: &CheckedType,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        if !self.program.concrete_needs_drop(value_type) {
            return Ok(());
        }
        let canonical =
            CanonicalType::concrete(value_type, origin).map_err(|diagnostic| vec![diagnostic])?;
        self.requests.push(ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(GcFinalizerKey::Cell(canonical)),
            plan: LoweredArtifactPlan::GcFinalizer(GcFinalizerPlan::Cell {
                value_type: value_type.clone(),
                glue: None,
            }),
            kind: LoweredArtifactDependencyKind::GcFinalizer,
            origin: origin.clone(),
            use_site: Some(ArtifactUseSite::CellFinalizer(symbol)),
        });
        Ok(())
    }

    fn instance_use(
        &mut self,
        site: ArtifactUseSite,
        resolved: super::instance_resolution::ResolvedInstanceRequest,
        kind: LoweredInstanceDependencyKind,
        origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        self.requests.push(ClosureRequest::Instance {
            resolved,
            kind,
            origin: origin.clone(),
            use_site: Some(site),
        });
        Ok(())
    }

    fn coro_creation(
        &mut self,
        _id: super::LoweredCoroId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        // The artifact planning coroutine scanner owns `CoroCreation` requests.
        Ok(())
    }
}

/// The collecting visitor: records owned bindings for the post-closure pass.
struct CollectVisitor {
    drafts: Vec<OwnedBindingDraft>,
}

impl LoweredOwnerVisitor for CollectVisitor {
    fn drop_site(
        &mut self,
        _site: ArtifactUseSite,
        _value_type: &CheckedType,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn finalizer_site(
        &mut self,
        _site: ArtifactUseSite,
        _key: GcFinalizerKey,
        _plan: GcFinalizerPlan,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn owned_binding(&mut self, draft: OwnedBindingDraft) -> Result<(), Vec<Diagnostic>> {
        self.drafts.push(draft);
        Ok(())
    }

    fn cell_finalizer(
        &mut self,
        _symbol: SymbolId,
        _value_type: &CheckedType,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn instance_use(
        &mut self,
        _site: ArtifactUseSite,
        _resolved: super::instance_resolution::ResolvedInstanceRequest,
        _kind: LoweredInstanceDependencyKind,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn coro_creation(
        &mut self,
        _id: super::LoweredCoroId,
        _origin: &Origin,
    ) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }
}

/// Walks one owner in lowered evaluation order, reporting every site through
/// its visitor. The traversal mirrors the specialization first-visit order:
/// parameters first, then block items in order, then the block result, with
/// each expression's operands in evaluation order. artifact planning first used it for
/// cleanup decisions; artifact planning reuses it for coroutine and reactive sites.
struct LoweredWalker<'a> {
    program: &'a LoweredProgram,
    owner: OwnerArenas<'a>,
    visitor: &'a mut dyn LoweredOwnerVisitor,
    visited_blocks: HashSet<BlockId>,
    visited_items: HashSet<ItemId>,
    visited_expressions: HashSet<ExpressionId>,
    visited_patterns: HashSet<PatternId>,
    visited_places: HashSet<PlaceId>,
    visited_calls: HashSet<super::LoweredCallId>,
    visited_callable_values: HashSet<super::LoweredCallableValueId>,
    seen_symbols: HashSet<SymbolId>,
}

/// A scan error that cannot be attributed to one site.
pub(super) type WalkResult = Result<(), Vec<Diagnostic>>;

impl<'a> LoweredWalker<'a> {
    fn run(mut self) -> WalkResult {
        if let OwnerArenas::Instance(body) = self.owner {
            let parameter_pattern = body.parameter_pattern;
            self.walk_pattern(parameter_pattern)?;
        }
        let root = match self.owner {
            OwnerArenas::Instance(body) => body.root,
            OwnerArenas::Initializer(id) => self
                .program
                .initializers
                .get(id)
                .map(|initializer| initializer.body),
        };
        if let Some(root) = root {
            self.walk_block(root)?;
        }
        Ok(())
    }

    fn walk_block(&mut self, id: BlockId) -> WalkResult {
        if !self.visited_blocks.insert(id) {
            return Ok(());
        }
        let Some(block) = self.owner.block(self.program, id) else {
            return Ok(());
        };
        let items = block.items.clone();
        let result = block.result;
        for item in items {
            self.walk_item(item)?;
        }
        if let Some(result) = result {
            self.walk_expression(result)?;
        }
        Ok(())
    }

    fn walk_item(&mut self, id: ItemId) -> WalkResult {
        if !self.visited_items.insert(id) {
            return Ok(());
        }
        let Some(item) = self.owner.item(self.program, id) else {
            return Ok(());
        };
        let origin = item.origin.clone();
        let kind = item.kind.clone();
        self.visitor.item_site(id, item)?;
        match kind {
            LoweredItemKind::Binding(binding) => {
                self.visitor.binding_site(&binding, &origin)?;
                if binding.generic {
                    return Ok(());
                }
                if let Some(operation) = binding.reactive {
                    self.visitor.reactive_operation(operation, &origin)?;
                }
                let symbol = binding.symbol;
                let value_type = binding.value.and_then(|value| {
                    self.owner
                        .expression(self.program, value)
                        .map(|expression| expression.value_type.clone())
                });
                let Some(symbol) = symbol else {
                    return Ok(());
                };
                let Some(value_type) = value_type else {
                    return Ok(());
                };
                // A derived binding is evaluated lazily by its own evaluator
                // thunk; the emitter allocates its cell and never emits the
                // initializer inline. The emission binder still walks the
                // value because the worklist requests its sites under the
                // enclosing owner.
                if binding.derived {
                    self.register_binding(symbol, &value_type, &origin)?;
                    if self.visitor.walks_derived_binding_values()
                        && let Some(value) = binding.value
                    {
                        self.walk_expression(value)?;
                    }
                    return Ok(());
                }
                // A mutable binding allocates its cell before the value
                // evaluates; other bindings register after it.
                let is_cell = self.is_cell_symbol(symbol);
                if is_cell {
                    self.register_binding(symbol, &value_type, &origin)?;
                }
                if let Some(value) = binding.value {
                    self.walk_expression(value)?;
                }
                if !is_cell {
                    self.register_binding(symbol, &value_type, &origin)?;
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                self.walk_expression(binding.value)?;
                self.walk_pattern(binding.pattern)?;
            }
            LoweredItemKind::Assignment(assignment) => {
                self.walk_place(assignment.target)?;
                self.walk_expression(assignment.value)?;
                if assignment.drop_previous
                    && let Some(value_type) = self
                        .owner
                        .place(self.program, assignment.target)
                        .map(|place| place.value_type.clone())
                {
                    self.visitor.drop_site(
                        ArtifactUseSite::ReplacedValue(id),
                        &value_type,
                        &origin,
                    )?;
                }
                if assignment.drops_base_temporary
                    && let Some(base_type) = self
                        .owner
                        .place(self.program, assignment.target)
                        .and_then(|place| match &place.kind {
                            super::LoweredPlaceKind::Indexed { base, .. } => {
                                self.owner.place(self.program, *base)
                            }
                            _ => None,
                        })
                        .map(|base| base.value_type.clone())
                {
                    self.visitor.drop_site(
                        ArtifactUseSite::MutateIndexTemporary(id),
                        &base_type,
                        &origin,
                    )?;
                }
                if let Some(operation) = assignment.signal_notify {
                    self.visitor.reactive_operation(operation, &origin)?;
                }
            }
            LoweredItemKind::Return(item) => {
                self.walk_expression(item.value)?;
            }
            LoweredItemKind::Break(item) => {
                if let Some(value) = item.value {
                    self.walk_expression(value)?;
                }
            }
            LoweredItemKind::Continue(_) => {}
            LoweredItemKind::Expression(item) => {
                let value_type = self
                    .owner
                    .expression(self.program, item.expression)
                    .map(|expression| expression.value_type.clone());
                if item.drop_result
                    && let Some(value_type) = value_type
                {
                    self.visitor.drop_site(
                        ArtifactUseSite::DiscardedResult(id),
                        &value_type,
                        &origin,
                    )?;
                }
                self.walk_expression(item.expression)?;
            }
        }
        Ok(())
    }

    fn walk_place(&mut self, id: PlaceId) -> WalkResult {
        if !self.visited_places.insert(id) {
            return Ok(());
        }
        let Some(place) = self.owner.place(self.program, id) else {
            return Ok(());
        };
        let kind = place.kind.clone();
        match kind {
            super::LoweredPlaceKind::Temporary { expression } => {
                self.walk_expression(expression)?
            }
            super::LoweredPlaceKind::Dereference {
                reference,
                dereference: _,
            } => self.walk_expression(reference)?,
            super::LoweredPlaceKind::ProductElement { base, .. }
            | super::LoweredPlaceKind::Representation { base } => self.walk_place(base)?,
            super::LoweredPlaceKind::Indexed { base, index } => {
                self.walk_place(base)?;
                self.walk_expression(index)?;
            }
            super::LoweredPlaceKind::Symbol { .. }
            | super::LoweredPlaceKind::CapturedCell { .. }
            | super::LoweredPlaceKind::Resource { .. } => {}
        }
        Ok(())
    }

    fn walk_pattern(&mut self, id: PatternId) -> WalkResult {
        if !self.visited_patterns.insert(id) {
            return Ok(());
        }
        let Some(pattern) = self.owner.pattern(self.program, id) else {
            return Ok(());
        };
        let pattern = pattern.clone();
        let origin = pattern.origin.clone();
        self.visitor.pattern_site(&pattern, &origin)?;
        let value_type = pattern.value_type.clone();
        let kind = pattern.kind.clone();
        match kind {
            super::LoweredPatternKind::Wildcard => {
                self.visitor.drop_site(
                    ArtifactUseSite::WildcardDiscard(id),
                    &value_type,
                    &origin,
                )?;
            }
            super::LoweredPatternKind::Binding {
                symbol: Some(symbol),
                ..
            } => {
                self.register_binding(symbol, &value_type, &origin)?;
            }
            super::LoweredPatternKind::Binding { symbol: None, .. }
            | super::LoweredPatternKind::Literal { .. } => {}
            super::LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.walk_pattern(element)?;
                }
            }
            super::LoweredPatternKind::Nominal { argument, .. } => {
                self.walk_pattern(argument)?;
            }
            super::LoweredPatternKind::At { binding, pattern } => {
                self.walk_pattern(binding)?;
                self.walk_pattern(pattern)?;
            }
        }
        Ok(())
    }

    fn walk_expression(&mut self, id: ExpressionId) -> WalkResult {
        if !self.visited_expressions.insert(id) {
            return Ok(());
        }
        let Some(expression) = self.owner.expression(self.program, id) else {
            return Ok(());
        };
        let origin = expression.origin.clone();
        let kind = expression.kind.clone();
        self.visitor.expression_site(id, expression)?;
        match kind {
            super::LoweredExpressionKind::Deferred(_) => {}
            super::LoweredExpressionKind::Block(block) => self.walk_block(block)?,
            super::LoweredExpressionKind::Name(name) => {
                if let Some(operation) = name.reactive {
                    self.visitor.reactive_operation(operation, &origin)?;
                }
            }
            super::LoweredExpressionKind::String(_) => {
                self.visitor.string_literal_site(&origin)?;
            }
            super::LoweredExpressionKind::Integer(_)
            | super::LoweredExpressionKind::Float(_)
            | super::LoweredExpressionKind::CString(_) => {}
            super::LoweredExpressionKind::Access(access) => self.walk_expression(access.base)?,
            super::LoweredExpressionKind::Product(product) => {
                for step in &product.steps {
                    match step {
                        super::LoweredProductStep::Positional { expression, .. }
                        | super::LoweredProductStep::Designated { expression, .. }
                        | super::LoweredProductStep::PositionalSpread { expression, .. }
                        | super::LoweredProductStep::NamedSpread { expression, .. }
                        | super::LoweredProductStep::Default { expression, .. } => {
                            self.walk_expression(*expression)?;
                        }
                    }
                }
                for field in &product.fields {
                    self.walk_expression(*field)?;
                }
            }
            super::LoweredExpressionKind::RepeatedProduct(product) => {
                self.walk_expression(product.expression)?;
            }
            super::LoweredExpressionKind::Satisfies(satisfies) => {
                self.walk_expression(satisfies.value)?;
            }
            super::LoweredExpressionKind::Logical(logical) => {
                self.walk_expression(logical.left)?;
                self.walk_expression(logical.right)?;
            }
            super::LoweredExpressionKind::Loop(loop_) => {
                self.walk_block(loop_.body)?;
                if loop_.drops_body_result {
                    // The emitter drops the loop body's block value, not the
                    // `break`-value result type.
                    let body_result = self
                        .owner
                        .block(self.program, loop_.body)
                        .and_then(|block| block.result)
                        .and_then(|result| self.owner.expression(self.program, result))
                        .map(|expression| expression.value_type.clone());
                    if let Some(body_result) = body_result {
                        self.visitor.drop_site(
                            ArtifactUseSite::LoopBodyResult(id),
                            &body_result,
                            &origin,
                        )?;
                    }
                }
            }
            super::LoweredExpressionKind::Match(match_) => {
                self.walk_expression(match_.subject)?;
                for arm in &match_.arms {
                    self.walk_pattern(arm.pattern)?;
                    self.walk_expression(arm.body)?;
                }
            }
            super::LoweredExpressionKind::Index(index) => {
                self.walk_expression(index.base)?;
                self.walk_expression(index.index)?;
                // drop mutation temporaries drops the call's operand
                // temporaries after the call, in reverse collection order.
                if let Some(method_type) = &index.method_type {
                    if index.operands.whole_drops_after_call {
                        self.visitor.drop_site(
                            ArtifactUseSite::IndexTemporary {
                                expression: id,
                                operand: None,
                            },
                            &method_type.parameter,
                            &origin,
                        )?;
                    }
                    let types = match method_type.parameter.as_ref() {
                        CheckedType::Product(product) => product
                            .elements
                            .iter()
                            .map(|element| element.value_type.clone())
                            .collect::<Vec<_>>(),
                        other => vec![other.clone()],
                    };
                    for (operand, drops) in index.operands.drops_after_call.iter().enumerate().rev()
                    {
                        if *drops && let Some(value_type) = types.get(operand) {
                            self.visitor.drop_site(
                                ArtifactUseSite::IndexTemporary {
                                    expression: id,
                                    operand: Some(operand),
                                },
                                value_type,
                                &origin,
                            )?;
                        }
                    }
                }
            }
            super::LoweredExpressionKind::StringTemplate(template) => {
                self.visitor.string_template_site(&template, &origin)?;
                for part in &template.parts {
                    if let super::LoweredStringTemplatePart::Interpolation(interpolation) = part {
                        self.walk_expression(interpolation.expression)?;
                    }
                }
            }
            super::LoweredExpressionKind::Call(call) => self.walk_call(call)?,
            super::LoweredExpressionKind::CallableValue(value) => {
                self.walk_callable_value(value, id)?;
            }
            super::LoweredExpressionKind::Resource(_) => {}
            super::LoweredExpressionKind::With(with) => {
                if let Some(with) = self.owner.with(self.program, with) {
                    let value = with.value;
                    let body = with.body;
                    self.walk_expression(value)?;
                    self.walk_block(body)?;
                }
            }
            super::LoweredExpressionKind::Coro(coro) => {
                self.visitor.coro_creation(coro, &origin)?;
            }
            super::LoweredExpressionKind::Await(await_id) => {
                // An `await` operand is evaluated by the awaiting frame, so
                // its creations, reactive operations, and cleanups are this
                // owner's sites.
                if let Some(await_) = self.owner.await_record(self.program, await_id) {
                    let await_ = await_.clone();
                    self.visitor.await_id_site(await_id, &await_)?;
                    self.visitor.await_site(&await_, &origin)?;
                    self.walk_expression(await_.operand)?;
                }
            }
        }
        Ok(())
    }

    fn walk_call(&mut self, id: super::LoweredCallId) -> WalkResult {
        if !self.visited_calls.insert(id) {
            return Ok(());
        }
        let Some(call) = self.owner.call(self.program, id) else {
            return Ok(());
        };
        let call = call.clone();
        let origin = call.origin.clone();
        let target = call.target.clone();
        let steps = call.steps.clone();
        let arguments = call.arguments.clone();
        let result_type = call.result_type.clone();
        let c_string_temporary = call.c_string_temporary;
        if let Some(callee) = call.callee {
            self.walk_expression(callee)?;
        }
        for step in &steps {
            match step {
                super::LoweredCallStep::Callee { expression }
                | super::LoweredCallStep::ProductElement { expression, .. }
                | super::LoweredCallStep::ProductSpread { expression, .. }
                | super::LoweredCallStep::NamedProductSpread { expression, .. }
                | super::LoweredCallStep::Default { expression, .. } => {
                    self.walk_expression(*expression)?;
                }
                super::LoweredCallStep::Argument { argument } => {
                    if let Some(expression) = arguments
                        .get(*argument)
                        .and_then(|argument| argument.expression)
                    {
                        self.walk_expression(expression)?;
                    }
                    // A reactive intrinsic's callback thunk is the
                    // `ReactiveCallbackEnvironment` site (artifact planning); every
                    // other implicit thunk argument builds its closure here.
                    if let Some(thunk) =
                        arguments.get(*argument).and_then(|argument| argument.thunk)
                        && !matches!(target, super::LoweredCallableTarget::Intrinsic { .. })
                    {
                        self.thunk_argument_environment(id, *argument, thunk, &origin)?;
                    }
                }
                super::LoweredCallStep::Resource { .. } | super::LoweredCallStep::Invoke => {}
            }
        }
        self.visitor.call_site(&call)?;
        self.visitor.call_id_site(id, &call)?;

        // Call-specific cleanup runs when the invocation executes.
        match &target {
            super::LoweredCallableTarget::Constructor {
                recursive: Some(_), ..
            } => {
                if let CheckedType::Ref(payload) = &result_type
                    && self.program.concrete_needs_drop(payload)
                {
                    let canonical = CanonicalType::concrete(payload, &origin)
                        .map_err(|diagnostic| vec![diagnostic])?;
                    self.visitor.finalizer_site(
                        ArtifactUseSite::RefConstruction(id),
                        GcFinalizerKey::Payload(canonical),
                        GcFinalizerPlan::Payload {
                            value_type: payload.as_ref().clone(),
                            glue: None,
                        },
                        &origin,
                    )?;
                }
            }
            super::LoweredCallableTarget::Intrinsic { intrinsic, .. } => match intrinsic {
                IntrinsicFunction::Drop => {
                    if let Some(argument) = arguments.first() {
                        self.visitor.drop_site(
                            ArtifactUseSite::DropIntrinsic(id),
                            &argument.expected,
                            &origin,
                        )?;
                    }
                }
                IntrinsicFunction::StringFromCString => {
                    self.visitor.drop_site(
                        ArtifactUseSite::CStringConversion(id),
                        &CheckedType::CString,
                        &origin,
                    )?;
                }
                IntrinsicFunction::ResolverComplete => {
                    if let Some(argument) = arguments.last() {
                        self.visitor.drop_site(
                            ArtifactUseSite::CompletionOrphan(id),
                            &argument.expected,
                            &origin,
                        )?;
                    }
                }
                IntrinsicFunction::BufferWithCapacity => {
                    if let CheckedType::Buffer(element) = &result_type
                        && self.program.concrete_needs_drop(element)
                    {
                        let canonical = CanonicalType::concrete(element, &origin)
                            .map_err(|diagnostic| vec![diagnostic])?;
                        self.visitor.finalizer_site(
                            ArtifactUseSite::BufferAllocation(id),
                            GcFinalizerKey::Buffer(canonical),
                            GcFinalizerPlan::Buffer {
                                element: element.as_ref().clone(),
                                glue: None,
                            },
                            &origin,
                        )?;
                    }
                }
                IntrinsicFunction::BufferClone => {
                    if let CheckedType::Buffer(element) = &result_type {
                        // The per-element `Clone` call is an instance use of the
                        // selected method, first in the site order.
                        if let Some(clone_trait) = self.program.semantic_ids.clone_trait
                            && let Some(method) = self
                                .program
                                .traits
                                .get(clone_trait)
                                .and_then(|trait_| trait_.methods.first())
                                .copied()
                        {
                            let selected =
                                super::structural_artifacts::select_concrete_trait_method_with_kind(
                                    self.program,
                                    &origin,
                                    clone_trait,
                                    method,
                                    std::slice::from_ref(element),
                                    LoweredInstanceDependencyKind::CloneMethod,
                                )
                                .map_err(|diagnostic| vec![diagnostic])?;
                            if let ClosureRequest::Instance { resolved, .. } = selected.request {
                                self.visitor.instance_use(
                                    ArtifactUseSite::BufferCloneElement(id),
                                    resolved,
                                    LoweredInstanceDependencyKind::CloneMethod,
                                    &origin,
                                )?;
                            }
                        }
                    }
                    if let CheckedType::Buffer(element) = &result_type
                        && self.program.concrete_needs_drop(element)
                    {
                        let canonical = CanonicalType::concrete(element, &origin)
                            .map_err(|diagnostic| vec![diagnostic])?;
                        self.visitor.finalizer_site(
                            ArtifactUseSite::BufferCloneFinalizer(id),
                            GcFinalizerKey::Buffer(canonical),
                            GcFinalizerPlan::Buffer {
                                element: element.as_ref().clone(),
                                glue: None,
                            },
                            &origin,
                        )?;
                    }
                }
                _ => {}
            },
            super::LoweredCallableTarget::DirectFunction { .. }
            | super::LoweredCallableTarget::IndirectClosure { .. }
            | super::LoweredCallableTarget::ExternalFunction { .. }
            | super::LoweredCallableTarget::Constructor {
                recursive: None, ..
            }
            | super::LoweredCallableTarget::TraitImplementation { .. }
            | super::LoweredCallableTarget::StructuralTraitMethod { .. } => {}
        }

        if let Some(operation) = call.reactive {
            self.visitor.reactive_operation(operation, &origin)?;
        }
        if c_string_temporary {
            self.visitor.drop_site(
                ArtifactUseSite::CStringTemporary(id),
                &CheckedType::CString,
                &origin,
            )?;
        }
        // The emitter drops mutation temporaries in reverse collection order.
        for (index, argument) in arguments.iter().enumerate().rev() {
            if argument.drops_after_call {
                self.visitor.drop_site(
                    ArtifactUseSite::CallTemporary {
                        call: id,
                        argument: index,
                    },
                    &argument.expected,
                    &origin,
                )?;
            }
        }
        Ok(())
    }

    /// Requests the closure-environment finalizer of one implicit thunk
    /// argument. The emitter builds the thunk's closure over the current scope when
    /// the argument evaluates (compile adapted call argument →
    /// build closure) and installs the finalizer under the same gate as a
    /// fresh callable value: a non-empty environment with some capture that
    /// neither requires initialization state nor is borrowed and needs drop.
    /// The captures are the thunk instance's concrete captures.
    fn thunk_argument_environment(
        &mut self,
        call: super::LoweredCallId,
        argument: usize,
        thunk: crate::FunctionId,
        origin: &Origin,
    ) -> WalkResult {
        let thunk_instance = match self.owner {
            OwnerArenas::Instance(body) => body
                .binding(super::LoweredBindingSite::CallArgumentThunk { call, argument })
                .and_then(|binding| match binding {
                    super::LoweredBoundTarget::Instance(instance) => Some(*instance),
                    _ => None,
                }),
            OwnerArenas::Initializer(_) => None,
        };
        let thunk_instance = match thunk_instance {
            Some(instance) => instance,
            None => {
                // Initializer thunk arguments resolve the thunk's instance the
                // same way specialization does (root target, template signature).
                let Some(function_type) = self
                    .program
                    .functions
                    .get(thunk)
                    .map(|function| function.signature.clone())
                else {
                    return Err(vec![Diagnostic::new(
                        origin.span.clone(),
                        "implicit thunk argument has no lowered template".to_string(),
                    )]);
                };
                let request = InstanceResolutionRequest {
                    function: thunk,
                    origin: origin.clone(),
                    function_type,
                    substitutions: super::CallSubstitutions::default(),
                    evidence: None,
                    target: InstanceResolutionTarget::Root,
                };
                let resolved = self
                    .program
                    .resolve_instance_request(&request)
                    .map_err(|diagnostic| vec![diagnostic])?;
                let Some(ordinal) = self.program.specializations.instance_ordinal(&resolved.key)
                else {
                    return Err(vec![Diagnostic::new(
                        origin.span.clone(),
                        "initializer thunk argument instance was never interned".to_string(),
                    )]);
                };
                super::FunctionInstanceId::from_index(ordinal.index())
            }
        };
        let Some(record) = self.program.instances.get(thunk_instance) else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                "thunk argument instance has no catalog record".to_string(),
            )]);
        };
        let Some(body) = record.body.as_ref() else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                "thunk argument instance has no materialized body".to_string(),
            )]);
        };
        let gate = body.captures.iter().any(|capture| {
            !capture.requires_initialization_state
                && !capture.capture.borrowed
                && self.program.concrete_needs_drop(&capture.value_type)
        });
        if !gate {
            return Ok(());
        }
        let captures = body
            .captures
            .iter()
            .map(|capture| CanonicalType::concrete(&capture.value_type, origin))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|diagnostic| vec![diagnostic])?;
        let capture_types = body
            .captures
            .iter()
            .map(|capture| capture.value_type.clone())
            .collect();
        let ordinal = record.ordinal;
        self.visitor.finalizer_site(
            ArtifactUseSite::ThunkArgumentEnvironment { call, argument },
            GcFinalizerKey::ClosureEnvironment {
                closure: ordinal,
                captures,
            },
            GcFinalizerPlan::ClosureEnvironment {
                closure: thunk_instance,
                captures: capture_types,
                drops: None,
            },
            origin,
        )
    }

    fn walk_callable_value(
        &mut self,
        id: super::LoweredCallableValueId,
        _expression: ExpressionId,
    ) -> WalkResult {
        if !self.visited_callable_values.insert(id) {
            return Ok(());
        }
        let Some(value) = self.owner.callable_value(self.program, id) else {
            return Ok(());
        };
        let value = value.clone();
        let origin = value.origin.clone();
        self.visitor.callable_value_site(id, &value, &origin)?;
        let Some(closure) = &value.closure else {
            return Ok(());
        };
        // Only a fresh, non-empty environment installs a closure finalizer.
        if closure.environment != super::LoweredClosureEnvironment::Fresh
            || closure.captures.is_empty()
        {
            return Ok(());
        }
        // The install gate: some capture that neither requires initialization
        // state nor is borrowed has a droppable type.
        let gate = closure.captures.iter().any(|capture| {
            !capture.requires_initialization_state
                && !capture.capture.borrowed
                && self.program.concrete_needs_drop(&capture.value_type)
        });
        if !gate {
            return Ok(());
        }
        let closure_instance = match self.owner {
            OwnerArenas::Instance(body) => body
                .binding(super::LoweredBindingSite::CallableValue(id))
                .and_then(|binding| match binding {
                    super::LoweredBoundTarget::Instance(instance) => Some(*instance),
                    _ => None,
                }),
            OwnerArenas::Initializer(_) => None,
        };
        let closure_instance = match closure_instance {
            Some(instance) => instance,
            None => {
                // Initializer closures have no binding table; resolve the
                // closure function's instance the same way specialization does.
                let request = InstanceResolutionRequest {
                    function: closure.function,
                    origin: origin.clone(),
                    function_type: value.function_type.clone(),
                    substitutions: closure.substitutions.clone(),
                    evidence: None,
                    target: InstanceResolutionTarget::Root,
                };
                let resolved = self
                    .program
                    .resolve_instance_request(&request)
                    .map_err(|diagnostic| vec![diagnostic])?;
                let Some(ordinal) = self.program.specializations.instance_ordinal(&resolved.key)
                else {
                    // specialization interns every initializer closure instance
                    // before the closure runs, so a missing key is a bug, not
                    // an unrequested closure.
                    return Err(vec![Diagnostic::new(
                        origin.span.clone(),
                        "initializer closure instance was never interned".to_string(),
                    )]);
                };
                super::FunctionInstanceId::from_index(ordinal.index())
            }
        };
        let captures = closure
            .captures
            .iter()
            .map(|capture| CanonicalType::concrete(&capture.value_type, &origin))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|diagnostic| vec![diagnostic])?;
        let Some(ordinal) = self
            .program
            .instances
            .get(closure_instance)
            .map(|instance| instance.ordinal)
        else {
            return Err(vec![Diagnostic::new(
                origin.span.clone(),
                "closure environment instance has no catalog record".to_string(),
            )]);
        };
        self.visitor.finalizer_site(
            ArtifactUseSite::ClosureEnvironment(id),
            GcFinalizerKey::ClosureEnvironment {
                closure: ordinal,
                captures: captures.clone(),
            },
            GcFinalizerPlan::ClosureEnvironment {
                closure: closure_instance,
                captures: closure
                    .captures
                    .iter()
                    .map(|capture| capture.value_type.clone())
                    .collect(),
                drops: None,
            },
            &origin,
        )
    }

    fn is_cell_symbol(&self, symbol: SymbolId) -> bool {
        self.program
            .symbols
            .get(symbol)
            .is_some_and(|record| record.mutable_storage || record.derived)
    }

    /// Reports one bound symbol, mirroring bind pattern value and
    /// compile item: a mutable or derived binding owns its cell (or gets a
    /// captured-cell finalizer), every other binding owns its value unless it
    /// is non-owning or arrives through a mutated-parameter pointer.
    fn register_binding(
        &mut self,
        symbol: SymbolId,
        value_type: &CheckedType,
        origin: &Origin,
    ) -> WalkResult {
        if !self.seen_symbols.insert(symbol) {
            return Ok(());
        }
        // A coroutine body's frame binding is a pre-seeded frame cell: the emitter
        // neither registers it as owned nor drops it at completion. Its drop
        // is the pair plan's `unwind_drop`, emitted only on the cancel unwind.
        if let OwnerArenas::Instance(body) = self.owner
            && let Some(plan_id) = body.plan_template
            && let Some(plan) = body.plan(plan_id)
            && plan.frame_bindings.contains(&symbol)
        {
            return Ok(());
        }
        if !self.program.concrete_needs_drop(value_type) {
            return Ok(());
        }
        let Some(record) = self.program.symbols.get(symbol) else {
            return Ok(());
        };
        // A symbol with real module storage is written into its global and
        // never owned at scope exit. `module_symbol` alone is not that fact: a
        // `let` inside a top-level `with`/block is module-scoped but receives
        // no global, and the emitter still owns and drops it (it keys on its own
        // storage map, mirrored by `has_global`).
        if record.mutated_parameter
            || matches!(
                record.storage,
                super::SymbolStorage::FunctionBinding | super::SymbolStorage::ExternalSymbol
            )
            || (record.storage == super::SymbolStorage::GlobalStorage && record.has_global)
        {
            return Ok(());
        }
        let draft = |storage| OwnedBindingDraft {
            symbol,
            storage,
            value_type: value_type.clone(),
            origin: origin.clone(),
        };
        if record.mutable_storage || record.derived {
            if record.captured {
                self.visitor.cell_finalizer(symbol, value_type, origin)?;
            } else {
                self.visitor.owned_binding(draft(OwnedStorage::Cell))?;
            }
        } else if !record.non_owning {
            self.visitor.owned_binding(draft(OwnedStorage::Value))?;
        }
        Ok(())
    }
}

/// Scans one materialized instance body for cleanup sites, in lowered
/// evaluation order.
pub(super) fn scan_instance(
    program: &LoweredProgram,
    instance: super::FunctionInstanceId,
) -> ScanResult {
    let Some(record) = program.instances.get(instance) else {
        return Ok(Vec::new());
    };
    let Some(body) = record.body.as_ref() else {
        return Ok(Vec::new());
    };
    let mut visitor = ScanVisitor {
        program,
        requests: Vec::new(),
    };
    walk_owner(program, OwnerArenas::Instance(body), &mut visitor)?;
    Ok(visitor.requests)
}

/// Scans one module initializer for cleanup sites, in lowered evaluation
/// order.
pub(super) fn scan_initializer(program: &LoweredProgram, initializer: InitializerId) -> ScanResult {
    if program.initializers.get(initializer).is_none() {
        return Ok(Vec::new());
    }
    let mut visitor = ScanVisitor {
        program,
        requests: Vec::new(),
    };
    walk_owner(program, OwnerArenas::Initializer(initializer), &mut visitor)?;
    Ok(visitor.requests)
}

pub(super) fn walk_owner(
    program: &LoweredProgram,
    owner: OwnerArenas<'_>,
    visitor: &mut dyn LoweredOwnerVisitor,
) -> WalkResult {
    LoweredWalker {
        program,
        owner,
        visitor,
        visited_blocks: HashSet::new(),
        visited_items: HashSet::new(),
        visited_expressions: HashSet::new(),
        visited_patterns: HashSet::new(),
        visited_places: HashSet::new(),
        visited_calls: HashSet::new(),
        visited_callable_values: HashSet::new(),
        seen_symbols: HashSet::new(),
    }
    .run()
}

/// Collects every owner's owned bindings and binds each one's glue through the
/// owner's `OwnedBinding` use record. Runs once after the closure fixed point.
pub(super) fn collect_owned_bindings(program: &mut LoweredProgram) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut instance_drafts = Vec::new();
    for (id, instance) in program.instances.iter() {
        let Some(body) = &instance.body else {
            continue;
        };
        let mut visitor = CollectVisitor { drafts: Vec::new() };
        if let Err(mut problems) = walk_owner(program, OwnerArenas::Instance(body), &mut visitor) {
            diagnostics.append(&mut problems);
            continue;
        }
        instance_drafts.push((id, visitor.drafts));
    }
    let mut initializer_drafts = Vec::new();
    for (id, _) in program.initializers.iter() {
        let mut visitor = CollectVisitor { drafts: Vec::new() };
        if let Err(mut problems) = walk_owner(program, OwnerArenas::Initializer(id), &mut visitor) {
            diagnostics.append(&mut problems);
            continue;
        }
        initializer_drafts.push((id, visitor.drafts));
    }

    for (id, drafts) in instance_drafts {
        let uses = program
            .instances
            .get(id)
            .and_then(|instance| instance.body.as_ref())
            .map(|body| body.artifact_uses.clone())
            .unwrap_or_default();
        let mut records = Vec::new();
        for draft in drafts {
            match bind_owned_binding(&uses, draft, &mut diagnostics) {
                Some(record) => records.push(record),
                None => continue,
            }
        }
        if let Some(instance) = program.instances.get_mut(id)
            && let Some(body) = instance.body.as_mut()
        {
            body.owned_bindings = records;
        }
    }
    for (id, drafts) in initializer_drafts {
        let uses = program
            .initializer_artifact_uses
            .get(id.index())
            .cloned()
            .unwrap_or_default();
        let mut records = Vec::new();
        for draft in drafts {
            match bind_owned_binding(&uses, draft, &mut diagnostics) {
                Some(record) => records.push(record),
                None => continue,
            }
        }
        if let Some(slot) = program.initializer_owned_bindings.get_mut(id.index()) {
            *slot = records;
        }
    }
    diagnostics
}

/// Binds one draft's glue through the owner's use records and produces the
/// stored record.
fn bind_owned_binding(
    uses: &[super::LoweredArtifactUse],
    draft: OwnedBindingDraft,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<LoweredOwnedBinding> {
    let glue = uses.iter().find_map(|use_| {
        (use_.site == ArtifactUseSite::OwnedBinding(draft.symbol)).then_some(use_.artifact)
    });
    let Some(glue) = glue else {
        diagnostics.push(Diagnostic::new(
            draft.origin.span.clone(),
            format!(
                "owned binding symbol {} has no recorded drop-glue use",
                draft.symbol.0
            ),
        ));
        return None;
    };
    Some(LoweredOwnedBinding {
        symbol: draft.symbol,
        storage: draft.storage,
        value_type: draft.value_type,
        glue: Some(glue),
    })
}

/// Validates the collected owned bindings: every record carries a bound glue
/// that matches its concrete type, every owned-binding use has a record, and
/// no owner repeats a symbol.
pub(super) fn check_owned_bindings(program: &LoweredProgram, diagnostics: &mut Vec<Diagnostic>) {
    for (id, instance) in program.instances.iter() {
        let Some(body) = &instance.body else {
            continue;
        };
        check_owner_bindings(
            program,
            &format!("function instance {}", id.index()),
            &body.owned_bindings,
            &body.artifact_uses,
            diagnostics,
        );
    }
    for (id, _) in program.initializers.iter() {
        if let Some(records) = program.initializer_owned_bindings.get(id.index()) {
            let uses = program
                .initializer_artifact_uses
                .get(id.index())
                .cloned()
                .unwrap_or_default();
            check_owner_bindings(
                program,
                &format!("initializer {}", id.index()),
                records,
                &uses,
                diagnostics,
            );
        }
    }
}

fn check_owner_bindings(
    program: &LoweredProgram,
    owner: &str,
    records: &[LoweredOwnedBinding],
    uses: &[super::LoweredArtifactUse],
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut seen = HashSet::new();
    for record in records {
        if !seen.insert(record.symbol) {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("{owner} records owned binding {} twice", record.symbol.0),
            ));
        }
        let canonical = match CanonicalType::concrete(&record.value_type, &Origin::compiler()) {
            Ok(canonical) => canonical,
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                continue;
            }
        };
        let Some(glue) = record.glue else {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("{owner} has an unbound owned binding {}", record.symbol.0),
            ));
            continue;
        };
        let key_matches = program
            .specializations
            .artifact(glue)
            .is_some_and(|key| *key == ArtifactRequestKey::DropGlue(canonical));
        if !key_matches {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "{owner} binds owned binding {} to a drop glue that is not its value type",
                    record.symbol.0
                ),
            ));
        }
        let use_matches = uses.iter().any(|use_| {
            use_.site == ArtifactUseSite::OwnedBinding(record.symbol) && use_.artifact == glue
        });
        if !use_matches {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "{owner} has no owned-binding use record for symbol {}",
                    record.symbol.0
                ),
            ));
        }
    }
    for use_ in uses {
        if let ArtifactUseSite::OwnedBinding(symbol) = use_.site
            && !records.iter().any(|record| record.symbol == symbol)
        {
            diagnostics.push(Diagnostic::new(
                use_.origin.span.clone(),
                format!("{owner} has an owned-binding use with no record"),
            ));
        }
    }
}

/// Builds one drop-glue body in the emitter decision order.
fn drop_glue_body(
    program: &LoweredProgram,
    value_type: &CheckedType,
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<DropGlueBody, Vec<Diagnostic>> {
    if let Some(method) = user_drop_method(program, value_type, origin, requests)? {
        let representation = match value_type {
            CheckedType::Distinct { representation, .. }
                if program.concrete_needs_drop(representation) =>
            {
                Some(request_drop_glue(
                    program,
                    representation,
                    origin,
                    requests,
                )?)
            }
            _ => None,
        };
        return Ok(DropGlueBody::UserDrop {
            method,
            representation,
        });
    }
    if let Some(kind) = program.runtime_opaque_kind(value_type) {
        return Ok(match kind {
            RuntimeOpaqueKind::Coroutine => DropGlueBody::CoroutineCleanup,
            RuntimeOpaqueKind::Scheduler => {
                DropGlueBody::RuntimeRelease(RuntimeRelease::SchedulerDestroy)
            }
            RuntimeOpaqueKind::Wait => DropGlueBody::RuntimeRelease(RuntimeRelease::WaitDrop),
            RuntimeOpaqueKind::Resolver => {
                DropGlueBody::RuntimeRelease(RuntimeRelease::ResolverDrop)
            }
            RuntimeOpaqueKind::CompletionToken => {
                DropGlueBody::RuntimeRelease(RuntimeRelease::CompletionTokenRelease)
            }
        });
    }
    match value_type {
        CheckedType::CString => Ok(DropGlueBody::CStringFree),
        CheckedType::Product(product) => {
            // The emitter iterates and drops fields in reverse element order,
            // skipping every field that does not need drop.
            let mut fields = Vec::new();
            for (index, element) in product.elements.iter().enumerate().rev() {
                if !program.concrete_needs_drop(&element.value_type) {
                    continue;
                }
                let glue = request_drop_glue(program, &element.value_type, origin, requests)?;
                fields.push(DroppedElement {
                    index,
                    value_type: element.value_type.clone(),
                    glue,
                });
            }
            Ok(DropGlueBody::Product { fields })
        }
        CheckedType::Sum(sum) => {
            // The emitter switches on the tag in alternative order and drops only
            // alternatives that need drop.
            let mut alternatives = Vec::new();
            for (index, alternative) in sum.alternatives.iter().enumerate() {
                if !program.concrete_needs_drop(alternative) {
                    continue;
                }
                let glue = request_drop_glue(program, alternative, origin, requests)?;
                alternatives.push(DroppedAlternative {
                    index,
                    value_type: alternative.clone(),
                    glue,
                });
            }
            Ok(DropGlueBody::Sum { alternatives })
        }
        CheckedType::Distinct { representation, .. } => {
            if !program.concrete_needs_drop(representation) {
                return Err(vec![Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "drop glue for `{value_type}` needs drop but no cleanup rule applies to its representation"
                    ),
                )]);
            }
            let glue = request_drop_glue(program, representation, origin, requests)?;
            Ok(DropGlueBody::Distinct {
                representation: glue,
            })
        }
        other => Err(vec![Diagnostic::new(
            origin.span.clone(),
            format!("drop glue for `{other}` needs drop but no cleanup rule applies"),
        )]),
    }
}

/// Selects the user `Drop` method for a type and requests its instance, or
/// returns `None` when the emitter would fall through to the opaque/structural
/// branches.
/// The selected user `Drop` method for one concrete type, or `None` when no
/// implementation applies. emission selects through the ordinary trait
/// resolver with the `DropMethod` edge kind, so a generic implementation is
/// resolved with its substitutions and becomes a specialized instance.
fn user_drop_method(
    program: &LoweredProgram,
    value_type: &CheckedType,
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<Option<PlannedInstance>, Vec<Diagnostic>> {
    let Some(drop_trait) = program.semantic_ids.drop_trait else {
        return Ok(None);
    };
    if !program.concrete_drop_implementation_applies(value_type) {
        return Ok(None);
    }
    let Some(method) = program
        .traits
        .get(drop_trait)
        .and_then(|trait_| trait_.methods.first())
        .copied()
    else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            "the `Drop` trait declares no method".to_string(),
        )]);
    };
    let selected = super::structural_artifacts::select_concrete_trait_method_with_kind(
        program,
        origin,
        drop_trait,
        method,
        std::slice::from_ref(value_type),
        LoweredInstanceDependencyKind::DropMethod,
    )
    .map_err(|diagnostic| vec![diagnostic])?;
    let PlannedCallee::Instance(instance) = selected.callee else {
        return Err(vec![Diagnostic::new(
            origin.span.clone(),
            "a `Drop` selection resolved to a non-instance callee".to_string(),
        )]);
    };
    requests.push(selected.request);
    Ok(Some(instance))
}

/// Requests one nested `DropGlue` artifact and returns its planned callee.
pub(super) fn request_drop_glue(
    program: &LoweredProgram,
    value_type: &CheckedType,
    origin: &Origin,
    requests: &mut Vec<ClosureRequest>,
) -> Result<PlannedArtifact, Vec<Diagnostic>> {
    debug_assert!(program.concrete_needs_drop(value_type));
    let canonical =
        CanonicalType::concrete(value_type, origin).map_err(|diagnostic| vec![diagnostic])?;
    let key = ArtifactRequestKey::DropGlue(canonical);
    requests.push(ClosureRequest::Artifact {
        key: key.clone(),
        plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
            value_type: value_type.clone(),
            body: DropGlueBody::Unexpanded,
        }),
        kind: LoweredArtifactDependencyKind::DropGlue,
        origin: origin.clone(),
        use_site: None,
    });
    Ok(PlannedArtifact {
        key,
        artifact: None,
        kind: LoweredArtifactDependencyKind::DropGlue,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use crate::specialization::{ArtifactRequestKey, CanonicalType};
    use crate::{
        ArenaId, CheckedType, DropGlueBody, DropGluePlan, FunctionInstanceId, LoweredArtifactPlan,
        LoweredArtifactRequestId, LoweredClosureCapture, LoweredInstanceDependencyKind,
        LoweredProgram, Lowerer, NameResolver, Origin, OwnedStorage, ProgramLoader, RuntimeRelease,
        TypeChecker, TypedModule,
    };

    use super::super::artifact_closure::{
        ArtifactFamilyHooks, ArtifactUseSite, ClosureRequest, ExpansionResult, ScanResult,
    };

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

    /// The specialization program: graph built and materialized, closure not run.
    fn lowered_fixture(source: &str) -> LoweredProgram {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());
        assert!(program.build_specialization_worklist().is_empty());
        assert!(program.materialize_instance_bodies().is_empty());
        program
    }

    /// The concrete checked type of one declared function's parameter, taken
    /// from its materialized instance body.
    fn parameter_type(program: &LoweredProgram, name: &str, index: usize) -> CheckedType {
        let template = program
            .functions
            .iter()
            .find(|(_, _, function)| {
                function.name == name || function.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"));
        program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == template)
            .and_then(|(_, instance)| instance.body.as_ref())
            .and_then(|body| body.parameters.get(index))
            .map(|parameter| parameter.value_type.clone())
            .unwrap_or_else(|| panic!("no parameter {index} on {name}"))
    }

    /// A hook set that scripts drop-glue requests onto one seed instance and
    /// delegates every `DropGlue` artifact to the production expander.
    struct CleanupHooks {
        seed: FunctionInstanceId,
        origin: Origin,
        types: Vec<(CheckedType, u32)>,
    }

    impl ArtifactFamilyHooks for CleanupHooks {
        fn scan_initializer(
            &self,
            program: &LoweredProgram,
            initializer: crate::InitializerId,
        ) -> ScanResult {
            super::scan_initializer(program, initializer)
        }

        fn scan_instance(
            &self,
            program: &LoweredProgram,
            instance: FunctionInstanceId,
        ) -> ScanResult {
            let mut requests = super::scan_instance(program, instance)?;
            if instance != self.seed {
                return Ok(requests);
            }
            for (value_type, site) in &self.types {
                let canonical = CanonicalType::concrete(value_type, &self.origin).unwrap_or_else(
                    |diagnostic| panic!("type {value_type:?} is not concrete: {diagnostic:?}"),
                );
                requests.push(ClosureRequest::Artifact {
                    key: ArtifactRequestKey::DropGlue(canonical),
                    plan: LoweredArtifactPlan::DropGlue(DropGluePlan {
                        value_type: value_type.clone(),
                        body: DropGlueBody::Unexpanded,
                    }),
                    kind: super::super::LoweredArtifactDependencyKind::DropGlue,
                    origin: self.origin.clone(),
                    use_site: Some(ArtifactUseSite::Test(*site)),
                });
            }
            Ok(requests)
        }

        fn expand(
            &self,
            program: &LoweredProgram,
            artifact: LoweredArtifactRequestId,
        ) -> ExpansionResult {
            let record = program.artifacts.get(artifact).expect("artifact");
            let plan = record.plan.clone().expect("plan");
            match program.specializations.artifact(record.ordinal) {
                Some(ArtifactRequestKey::DropGlue(_)) => {
                    let LoweredArtifactPlan::DropGlue(plan) = plan else {
                        unreachable!("a drop-glue key carries a drop-glue plan")
                    };
                    super::expand_drop_glue(program, artifact, plan)
                }
                _ => Ok((plan, Vec::new())),
            }
        }

        fn expands_body(&self, key: &ArtifactRequestKey) -> bool {
            matches!(key, ArtifactRequestKey::DropGlue(_))
        }
    }

    fn drop_glue_plan<'a>(
        program: &'a LoweredProgram,
        value_type: &CheckedType,
    ) -> &'a DropGluePlan {
        let origin = Origin::compiler();
        let canonical = CanonicalType::concrete(value_type, &origin).expect("concrete type");
        let ordinal = program
            .specializations
            .artifact_ordinal(&ArtifactRequestKey::DropGlue(canonical))
            .expect("the drop-glue key is interned");
        let artifact = program
            .artifacts
            .iter()
            .find(|(_, record)| record.ordinal == ordinal)
            .map(|(_, record)| record)
            .expect("the artifact record exists");
        match artifact.plan.as_ref().expect("expanded plan") {
            LoweredArtifactPlan::DropGlue(plan) => plan,
            other => panic!("expected a drop-glue plan, got {other:?}"),
        }
    }

    const CLEANUP_FIXTURE: &str = concat!(
        "use std.cinterop.(CString, c_string)\n",
        "use std.coroutine.*\n",
        "type Resource = ctor I32\n",
        "impl Drop Resource { def drop = Resource value => () }\n",
        "type Handle = ctor CString\n",
        "impl Drop Handle { def drop = Handle value => () }\n",
        "type Wrapped = ctor CString\n",
        "type Box T = ctor (T)\n",
        "impl<T where Copy T> Drop (Box T) { def drop = Box value => () }\n",
        "type Chain = ctor ((CString) | (Ref Chain))\n",
        "def expose_resource: Resource -> I32 = value => 0\n",
        "def expose_handle: Handle -> I32 = value => 0\n",
        "def expose_wrapped: Wrapped -> I32 = value => 0\n",
        "def expose_box: (Box CString) -> I32 = value => 0\n",
        "def expose_box_handle: (Box Handle) -> I32 = value => 0\n",
        "def expose_box_i32: (Box I32) -> I32 = value => 0\n",
        "def expose_product: (I32, CString) -> I32 = value => 0\n",
        "def expose_nested: ((I32, CString), I32) -> I32 = value => 0\n",
        "def expose_sum: (CString | I32) -> I32 = value => 0\n",
        "def expose_chain: Chain -> I32 = value => 0\n",
        "def expose_coroutine: (Coroutine{} I32) -> I32 = value => 0\n",
        "def expose_scheduler: Scheduler -> I32 = value => 0\n",
        "def expose_wait: (Wait I32) -> I32 = value => 0\n",
        "def expose_resolver: (Resolver I32) -> I32 = value => 0\n",
        "def expose_token: CompletionToken -> I32 = value => 0\n",
        "let kept = 1\n",
    );

    #[test]
    fn drop_glue_bodies_follow_cleanup_selection_order() {
        let module = checked_program(CLEANUP_FIXTURE);
        let mut program = lowered_fixture(CLEANUP_FIXTURE);
        let seed = FunctionInstanceId::from_index(0);
        let origin = program
            .instances
            .get(seed)
            .expect("seed instance")
            .origin
            .clone();
        let resource = parameter_type(&program, "expose_resource", 0);
        let handle = parameter_type(&program, "expose_handle", 0);
        let wrapped = parameter_type(&program, "expose_wrapped", 0);
        let box_c_string = parameter_type(&program, "expose_box", 0);
        let box_handle = parameter_type(&program, "expose_box_handle", 0);
        let box_i32 = parameter_type(&program, "expose_box_i32", 0);
        let product = parameter_type(&program, "expose_product", 0);
        let nested = parameter_type(&program, "expose_nested", 0);
        let sum = parameter_type(&program, "expose_sum", 0);
        let chain = parameter_type(&program, "expose_chain", 0);
        let coroutine = parameter_type(&program, "expose_coroutine", 0);
        let scheduler = parameter_type(&program, "expose_scheduler", 0);
        let wait = parameter_type(&program, "expose_wait", 0);
        let resolver = parameter_type(&program, "expose_resolver", 0);
        let token = parameter_type(&program, "expose_token", 0);

        let mut types = vec![(CheckedType::CString, 0)];
        for (site, value_type) in [
            resource.clone(),
            handle.clone(),
            wrapped.clone(),
            box_c_string.clone(),
            box_handle.clone(),
            box_i32.clone(),
            product.clone(),
            nested.clone(),
            sum.clone(),
            chain.clone(),
            coroutine.clone(),
            scheduler.clone(),
            wait.clone(),
            resolver.clone(),
            token.clone(),
        ]
        .into_iter()
        .enumerate()
        {
            types.push((value_type, site as u32 + 1));
        }
        let hooks = CleanupHooks {
            seed,
            origin,
            types,
        };
        let diagnostics = program.close_artifact_catalog(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        // CString: the `free` branch.
        assert_eq!(
            drop_glue_plan(&program, &CheckedType::CString).body,
            DropGlueBody::CStringFree
        );

        // User `Drop` on a non-represented distinct: no representation glue.
        let plan = drop_glue_plan(&program, &resource);
        let DropGlueBody::UserDrop {
            method,
            representation,
        } = &plan.body
        else {
            panic!("Resource selects a user drop: {:?}", plan.body);
        };
        assert!(representation.is_none(), "I32 does not need drop");
        assert!(
            program.concrete_drop_implementation_applies(&resource),
            "the general predicate selects the Resource drop method"
        );
        assert!(method.instance.is_some(), "the Resource method is bound");
        assert_eq!(
            method.kind,
            LoweredInstanceDependencyKind::DropMethod,
            "the selected method edge is a drop-method edge"
        );

        // User `Drop` on a represented distinct: the representation glue is the
        // nested CString glue and is only requested because CString needs drop.
        let plan = drop_glue_plan(&program, &handle);
        let DropGlueBody::UserDrop {
            method,
            representation,
        } = &plan.body
        else {
            panic!("Handle selects a user drop: {:?}", plan.body);
        };
        assert!(method.instance.is_some());
        let representation = representation
            .as_ref()
            .expect("the CString representation needs drop");
        let representation_ordinal = drop_glue_ordinal(&program, representation);
        assert_eq!(
            program.specializations.artifact(representation_ordinal),
            Some(&ArtifactRequestKey::DropGlue(
                CanonicalType::concrete(&CheckedType::CString, &Origin::compiler())
                    .expect("concrete")
            ))
        );

        // A represented distinct with no user `Drop`: the distinct branch.
        let plan = drop_glue_plan(&program, &wrapped);
        let DropGlueBody::Distinct { representation } = &plan.body else {
            panic!("Wrapped selects the distinct branch: {:?}", plan.body);
        };
        assert_eq!(
            program
                .specializations
                .artifact(drop_glue_ordinal(&program, representation)),
            Some(&ArtifactRequestKey::DropGlue(
                CanonicalType::concrete(&CheckedType::CString, &Origin::compiler())
                    .expect("concrete")
            ))
        );

        // Product: only droppable fields, in reverse element order.
        let plan = drop_glue_plan(&program, &product);
        let DropGlueBody::Product { fields } = &plan.body else {
            panic!("a product selects the product branch: {:?}", plan.body);
        };
        assert_eq!(fields.len(), 1, "I32 fields are skipped");
        assert_eq!(fields[0].index, 1, "the CString field is index 1");
        assert_eq!(fields[0].value_type, CheckedType::CString);

        // Nested products: the outer field glue is the inner product's glue.
        let plan = drop_glue_plan(&program, &nested);
        let DropGlueBody::Product { fields } = &plan.body else {
            panic!(
                "a nested product selects the product branch: {:?}",
                plan.body
            );
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].index, 0);
        assert_eq!(fields[0].value_type, product);
        let nested_ordinal = drop_glue_ordinal(&program, &fields[0].glue);
        let nested_plan = match program
            .artifacts
            .iter()
            .find(|(_, record)| record.ordinal == nested_ordinal)
            .and_then(|(_, record)| record.plan.as_ref())
        {
            Some(LoweredArtifactPlan::DropGlue(plan)) => plan,
            other => panic!("expected a nested drop-glue plan, got {other:?}"),
        };
        assert!(matches!(nested_plan.body, DropGlueBody::Product { .. }));

        // Sum: only droppable alternatives, in tag order.
        let plan = drop_glue_plan(&program, &sum);
        let DropGlueBody::Sum { alternatives } = &plan.body else {
            panic!("a sum selects the sum branch: {:?}", plan.body);
        };
        assert_eq!(alternatives.len(), 1, "I32 alternatives are skipped");
        assert_eq!(alternatives[0].index, 0);
        assert_eq!(alternatives[0].value_type, CheckedType::CString);

        // A recursive nominal type through a reference: the reference field is
        // skipped, so the glue body terminates on the CString.
        let plan = drop_glue_plan(&program, &chain);
        let DropGlueBody::Distinct { representation } = &plan.body else {
            panic!("Chain selects the distinct branch: {:?}", plan.body);
        };
        let chain_representation = drop_glue_ordinal(&program, representation);
        let chain_representation_plan = match program
            .artifacts
            .iter()
            .find(|(_, record)| record.ordinal == chain_representation)
            .and_then(|(_, record)| record.plan.as_ref())
        {
            Some(LoweredArtifactPlan::DropGlue(plan)) => plan,
            other => panic!("expected the chain representation glue, got {other:?}"),
        };
        let DropGlueBody::Sum { alternatives } = &chain_representation_plan.body else {
            panic!(
                "Chain's representation is a sum: {:?}",
                chain_representation_plan.body
            );
        };
        assert_eq!(alternatives.len(), 1);
        assert_eq!(alternatives[0].value_type, CheckedType::CString);

        // Runtime opaques: the dedicated cleanup routes.
        assert_eq!(
            drop_glue_plan(&program, &coroutine).body,
            DropGlueBody::CoroutineCleanup
        );
        assert_eq!(
            drop_glue_plan(&program, &scheduler).body,
            DropGlueBody::RuntimeRelease(RuntimeRelease::SchedulerDestroy)
        );
        assert_eq!(
            drop_glue_plan(&program, &wait).body,
            DropGlueBody::RuntimeRelease(RuntimeRelease::WaitDrop)
        );
        assert_eq!(
            drop_glue_plan(&program, &resolver).body,
            DropGlueBody::RuntimeRelease(RuntimeRelease::ResolverDrop)
        );
        assert_eq!(
            drop_glue_plan(&program, &token).body,
            DropGlueBody::RuntimeRelease(RuntimeRelease::CompletionTokenRelease)
        );

        // the generic `impl<T where Copy T> Drop (Box T)`
        // applies to `Box I32` (its bound discharges), so it selects a user
        // drop; `Box CString` and `Box Handle` fail the `Copy` bound and keep
        // the structural distinct branch. Two instantiations produce two keys.
        assert_ne!(
            CanonicalType::concrete(&box_c_string, &Origin::compiler()).expect("concrete"),
            CanonicalType::concrete(&box_handle, &Origin::compiler()).expect("concrete")
        );
        let plan = drop_glue_plan(&program, &box_i32);
        let DropGlueBody::UserDrop {
            method,
            representation,
        } = &plan.body
        else {
            panic!("Box I32 selects the user drop: {:?}", plan.body);
        };
        assert!(representation.is_none(), "I32 does not need drop");
        assert!(method.instance.is_some(), "the Box I32 method is bound");
        assert_eq!(
            method.kind,
            LoweredInstanceDependencyKind::DropMethod,
            "the Box I32 edge is a drop-method edge"
        );
        for box_type in [&box_c_string, &box_handle] {
            assert!(
                !program.concrete_drop_implementation_applies(box_type),
                "the `Copy` bound does not hold for `{box_type}`"
            );
            let plan = drop_glue_plan(&program, box_type);
            assert!(
                matches!(plan.body, DropGlueBody::Distinct { .. }),
                "Box CString selects the distinct branch: {:?}",
                plan.body
            );
        }

        // Every expanded key passes the checker agreement gate against the typed
        // module: needs-drop gating and user-drop selection agree exactly.
        for (_, artifact) in program.artifacts.iter() {
            let Some(LoweredArtifactPlan::DropGlue(plan)) = &artifact.plan else {
                continue;
            };
            assert!(
                module.type_needs_drop(&plan.value_type),
                "a drop-glue key is only requested for a droppable type"
            );
            assert_eq!(
                module.type_needs_drop(&plan.value_type),
                program.concrete_needs_drop(&plan.value_type),
                "needs-drop diverges for `{}`",
                plan.value_type
            );
            assert_eq!(
                module.is_copy_type(&plan.value_type),
                program.concrete_is_copy(&plan.value_type),
                "Copy diverges for `{}`",
                plan.value_type
            );
            let applies = program.concrete_drop_implementation_applies(&plan.value_type);
            match &plan.body {
                DropGlueBody::UserDrop { method, .. } => {
                    assert!(applies, "a planned user drop has a matching implementation");
                    assert!(method.instance.is_some(), "the method is bound");
                }
                DropGlueBody::Unexpanded => panic!("drop glue was never expanded"),
                _ => {
                    assert!(
                        !applies,
                        "a non-user body is only planned when no user drop matches"
                    );
                }
            }
        }
    }

    /// Resolves the artifact ordinal of a bound planned glue callee.
    fn drop_glue_ordinal(
        _program: &LoweredProgram,
        glue: &super::PlannedArtifact,
    ) -> crate::specialization::ArtifactOrdinal {
        glue.artifact.expect("bound after closure")
    }

    #[test]
    fn drop_glue_converges_and_reports_closure_stats() {
        let source = concat!(
            "type Resource = ctor I32\n",
            "impl Drop Resource { def drop = Resource value => () }\n",
            "def mutate_resource: move (Resource, Resource) -> (Resource, Resource) = move pair => {\n",
            "  let mut copy = pair\n",
            "  copy[0] = Resource 3\n",
            "  copy\n",
            "}\n",
            "let replaced = mutate_resource (Resource 1, Resource 2)\n",
        );
        let module = checked_program(source);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("source should lower through the production closure");
        let stats = lowered
            .program
            .closure_stats
            .expect("the production closure records its stats");
        assert!(stats.rounds >= 1, "{stats:?}");
        assert!(
            stats.rounds <= 4,
            "drop glue converges in a few rounds: {stats:?}"
        );
        assert!(
            stats.growth <= 16,
            "drop glue growth stays small: {stats:?}"
        );
        // Every requested drop glue is expanded.
        for (_, artifact) in lowered.program.artifacts.iter() {
            if let Some(LoweredArtifactPlan::DropGlue(plan)) = &artifact.plan {
                assert!(
                    !matches!(plan.body, DropGlueBody::Unexpanded),
                    "drop glue {} was expanded",
                    artifact.ordinal.index()
                );
            }
        }
        eprintln!("artifact drop-glue closure stats: {stats:?}");
    }

    // ------------------------------------------------------------------
    // artifact planning gc-finalizer fixtures.
    // ------------------------------------------------------------------

    use crate::GcFinalizerPlan;
    use crate::specialization::GcFinalizerKey;

    /// A hook set that scripts exact finalizer requests onto one seed instance
    /// and delegates both cleanup families to the production expanders.
    struct FinalizerHooks {
        seed: FunctionInstanceId,
        requests: Vec<ClosureRequest>,
    }

    impl ArtifactFamilyHooks for FinalizerHooks {
        fn scan_initializer(
            &self,
            program: &LoweredProgram,
            initializer: crate::InitializerId,
        ) -> ScanResult {
            super::scan_initializer(program, initializer)
        }

        fn scan_instance(
            &self,
            program: &LoweredProgram,
            instance: FunctionInstanceId,
        ) -> ScanResult {
            let mut requests = super::scan_instance(program, instance)?;
            if instance != self.seed {
                return Ok(requests);
            }
            requests.extend(self.requests.clone());
            Ok(requests)
        }

        fn expand(
            &self,
            program: &LoweredProgram,
            artifact: LoweredArtifactRequestId,
        ) -> ExpansionResult {
            let record = program.artifacts.get(artifact).expect("artifact");
            let plan = record.plan.clone().expect("plan");
            match (
                program.specializations.artifact(record.ordinal),
                plan.clone(),
            ) {
                (Some(ArtifactRequestKey::DropGlue(_)), LoweredArtifactPlan::DropGlue(plan)) => {
                    super::expand_drop_glue(program, artifact, plan)
                }
                (
                    Some(ArtifactRequestKey::GcFinalizer(_)),
                    LoweredArtifactPlan::GcFinalizer(plan),
                ) => super::expand_gc_finalizer(program, artifact, plan),
                _ => Ok((plan, Vec::new())),
            }
        }

        fn expands_body(&self, key: &ArtifactRequestKey) -> bool {
            matches!(
                key,
                ArtifactRequestKey::DropGlue(_) | ArtifactRequestKey::GcFinalizer(_)
            )
        }
    }

    const FINALIZER_FIXTURE: &str = concat!(
        "use std.cinterop.*\n",
        "extern \"c\" { inspect: CString -> I32 }\n",
        "type Owned = ctor CString\n",
        "type CellValue = ctor CString\n",
        "type BorrowedValue = ctor CString\n",
        "type Wrapped = ctor CString\n",
        "type DerivedValue = ctor CString\n",
        "def make_owned = (move value: Owned) => { let callback = () => inspect (value.*); callback }\n",
        "def make_mutable = () => {\n",
        "  let mut cell = CellValue (c_string \"a\")\n",
        "  cell = CellValue (c_string \"b\")\n",
        "  let callback = () => inspect (cell.*)\n",
        "  callback\n",
        "}\n",
        "def use_borrowed = (value: BorrowedValue) => { let callback = () => inspect (value.*); callback () }\n",
        "def peek: <T> T -> I32 = _ => 0\n",
        "def make_generic: <T> move T -> (() -> I32) = move value => () => peek value\n",
        "def use_derived = () => {\n",
        "  let signal count = 1\n",
        "  let derived_value = when { count > 0 => DerivedValue (c_string \"x\"), else => DerivedValue (c_string \"y\") }\n",
        "  let callback = () => peek derived_value\n",
        "  callback ()\n",
        "}\n",
        "let owned = make_owned (Owned (c_string \"owned\"))\n",
        "let mutable = make_mutable ()\n",
        "let borrowed = use_borrowed (BorrowedValue (c_string \"borrowed\"))\n",
        "let generic_string = make_generic (c_string \"generic\")\n",
        "let generic_wrapped = make_generic (Wrapped (c_string \"wrapped\"))\n",
    );

    /// The first closure instance whose own captures include `capture`.
    fn closure_instance_capturing(
        program: &LoweredProgram,
        capture: &CheckedType,
    ) -> FunctionInstanceId {
        closure_instance_capturing_filtered(program, capture, |_| true)
    }

    /// The first closure instance with a capture of `capture`'s type that also
    /// satisfies `filter`; disambiguates closures sharing one capture type.
    fn closure_instance_capturing_filtered(
        program: &LoweredProgram,
        capture: &CheckedType,
        filter: impl Fn(&crate::LoweredInstanceCapture) -> bool,
    ) -> FunctionInstanceId {
        program
            .instances
            .iter()
            .find_map(|(id, instance)| {
                let body = instance.body.as_ref()?;
                body.captures()
                    .iter()
                    .any(|candidate| &candidate.value_type == capture && filter(candidate))
                    .then_some(id)
            })
            .unwrap_or_else(|| panic!("no closure instance captures {capture:?}"))
    }

    fn finalizer_request(
        key: GcFinalizerKey,
        plan: GcFinalizerPlan,
        origin: &Origin,
        site: u32,
    ) -> ClosureRequest {
        ClosureRequest::Artifact {
            key: ArtifactRequestKey::GcFinalizer(key),
            plan: LoweredArtifactPlan::GcFinalizer(plan),
            kind: super::super::LoweredArtifactDependencyKind::GcFinalizer,
            origin: origin.clone(),
            use_site: Some(ArtifactUseSite::Test(site)),
        }
    }

    fn finalizer_plan<'a>(
        program: &'a LoweredProgram,
        key: &GcFinalizerKey,
    ) -> &'a GcFinalizerPlan {
        let ordinal = program
            .specializations
            .artifact_ordinal(&ArtifactRequestKey::GcFinalizer(key.clone()))
            .expect("the finalizer key is interned");
        let artifact = program
            .artifacts
            .iter()
            .find(|(_, record)| record.ordinal == ordinal)
            .map(|(_, record)| record)
            .expect("the artifact record exists");
        match artifact.plan.as_ref().expect("expanded plan") {
            LoweredArtifactPlan::GcFinalizer(plan) => plan,
            other => panic!("expected a gc-finalizer plan, got {other:?}"),
        }
    }

    fn assert_finalizer_glue(
        program: &LoweredProgram,
        glue: &super::PlannedArtifact,
        expected: &CheckedType,
    ) {
        let ordinal = drop_glue_ordinal(program, glue);
        assert_eq!(
            program.specializations.artifact(ordinal),
            Some(&ArtifactRequestKey::DropGlue(
                CanonicalType::concrete(expected, &Origin::compiler()).expect("concrete")
            )),
            "the finalizer's glue is the expected drop-glue key"
        );
    }

    #[test]
    fn finalizer_bodies_reference_their_drop_glue_and_capture_drops() {
        let mut program = lowered_fixture(FINALIZER_FIXTURE);
        let seed = FunctionInstanceId::from_index(0);
        let origin = program
            .instances
            .get(seed)
            .expect("seed instance")
            .origin
            .clone();

        let owned_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "Owned"),
            name: "Owned".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };
        let cell_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "CellValue"),
            name: "CellValue".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };
        let borrowed_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "BorrowedValue"),
            name: "BorrowedValue".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };
        let derived_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "DerivedValue"),
            name: "DerivedValue".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };
        let wrapped_capture = CheckedType::Distinct {
            id: nominal_type_id(&program, "Wrapped"),
            name: "Wrapped".to_string(),
            arguments: Vec::new(),
            representation: Box::new(CheckedType::CString),
        };

        let owned_closure = closure_instance_capturing(&program, &owned_capture);
        let mutable_closure = closure_instance_capturing(&program, &cell_capture);
        let borrowed_closure = closure_instance_capturing(&program, &borrowed_capture);
        let generic_string_closure = closure_instance_capturing(&program, &CheckedType::CString);
        let generic_wrapped_closure = closure_instance_capturing(&program, &wrapped_capture);
        let derived_closure =
            closure_instance_capturing_filtered(&program, &derived_capture, |capture| {
                capture.derived
            });
        assert_ne!(
            generic_string_closure, generic_wrapped_closure,
            "two generic instantiations produce two closure instances"
        );

        let request_for = |closure: FunctionInstanceId,
                           capture: &CheckedType,
                           plan: GcFinalizerPlan,
                           site: u32| {
            let ordinal = program
                .instances
                .get(closure)
                .expect("closure instance")
                .ordinal;
            let canonical = CanonicalType::concrete(capture, &origin).expect("concrete capture");
            finalizer_request(
                GcFinalizerKey::ClosureEnvironment {
                    closure: ordinal,
                    captures: vec![canonical],
                },
                plan,
                &origin,
                site,
            )
        };
        let requests = vec![
            finalizer_request(
                GcFinalizerKey::Payload(
                    CanonicalType::concrete(&CheckedType::CString, &origin).expect("concrete"),
                ),
                GcFinalizerPlan::Payload {
                    value_type: CheckedType::CString,
                    glue: None,
                },
                &origin,
                0,
            ),
            finalizer_request(
                GcFinalizerKey::Cell(
                    CanonicalType::concrete(&owned_capture, &origin).expect("concrete"),
                ),
                GcFinalizerPlan::Cell {
                    value_type: owned_capture.clone(),
                    glue: None,
                },
                &origin,
                1,
            ),
            finalizer_request(
                GcFinalizerKey::Buffer(
                    CanonicalType::concrete(&cell_capture, &origin).expect("concrete"),
                ),
                GcFinalizerPlan::Buffer {
                    element: cell_capture.clone(),
                    glue: None,
                },
                &origin,
                2,
            ),
            request_for(
                owned_closure,
                &owned_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: owned_closure,
                    captures: vec![owned_capture.clone()],
                    drops: None,
                },
                3,
            ),
            request_for(
                mutable_closure,
                &cell_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: mutable_closure,
                    captures: vec![cell_capture.clone()],
                    drops: None,
                },
                4,
            ),
            request_for(
                borrowed_closure,
                &borrowed_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: borrowed_closure,
                    captures: vec![borrowed_capture.clone()],
                    drops: None,
                },
                5,
            ),
            request_for(
                generic_string_closure,
                &CheckedType::CString,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: generic_string_closure,
                    captures: vec![CheckedType::CString],
                    drops: None,
                },
                6,
            ),
            request_for(
                generic_wrapped_closure,
                &wrapped_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: generic_wrapped_closure,
                    captures: vec![wrapped_capture.clone()],
                    drops: None,
                },
                7,
            ),
            request_for(
                derived_closure,
                &derived_capture,
                GcFinalizerPlan::ClosureEnvironment {
                    closure: derived_closure,
                    captures: vec![derived_capture.clone()],
                    drops: None,
                },
                8,
            ),
        ];
        let hooks = FinalizerHooks { seed, requests };
        let diagnostics = program.close_artifact_catalog(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_artifact_closure(&hooks);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        // Payload/Cell/Buffer: every expanded finalizer has bound glue.
        for (key, expected) in [
            (
                GcFinalizerKey::Payload(canonical(&CheckedType::CString)),
                CheckedType::CString,
            ),
            (
                GcFinalizerKey::Cell(canonical(&owned_capture)),
                owned_capture.clone(),
            ),
            (
                GcFinalizerKey::Buffer(canonical(&cell_capture)),
                cell_capture.clone(),
            ),
        ] {
            match finalizer_plan(&program, &key) {
                GcFinalizerPlan::Payload { glue, .. }
                | GcFinalizerPlan::Cell { glue, .. }
                | GcFinalizerPlan::Buffer { glue, .. } => {
                    let glue = glue.as_ref().expect("the finalizer is expanded");
                    assert_finalizer_glue(&program, glue, &expected);
                }
                other => panic!("unexpected finalizer plan {other:?}"),
            }
        }

        // The by-value droppable capture is dropped by its closure finalizer.
        let owned_key = GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(owned_closure)
                .expect("instance")
                .ordinal,
            captures: vec![canonical(&owned_capture)],
        };
        let GcFinalizerPlan::ClosureEnvironment { drops, .. } =
            finalizer_plan(&program, &owned_key)
        else {
            panic!("expected a closure-environment plan")
        };
        let drops = drops.as_ref().expect("expanded");
        assert_eq!(drops.len(), 1, "Owned is dropped: {drops:?}");
        assert_eq!(drops[0].index, 0);
        assert_finalizer_glue(&program, &drops[0].glue, &owned_capture);

        // The mutable capture fires the install gate but the body drops
        // nothing (has mutable storage).
        let mutable_key = GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(mutable_closure)
                .expect("instance")
                .ordinal,
            captures: vec![canonical(&cell_capture)],
        };
        let GcFinalizerPlan::ClosureEnvironment { drops, .. } =
            finalizer_plan(&program, &mutable_key)
        else {
            panic!("expected a closure-environment plan")
        };
        assert!(
            drops.as_ref().expect("expanded").is_empty(),
            "a mutable capture is skipped by the finalizer body"
        );
        let mutable_site = closure_site_capture(&program, mutable_closure, &cell_capture);
        assert!(
            !mutable_site.requires_initialization_state && !mutable_site.capture.borrowed,
            "the install gate is not excluded by initialization or borrowing"
        );

        // A borrowed capture is skipped by the body and excluded by the gate.
        let borrowed_key = GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(borrowed_closure)
                .expect("instance")
                .ordinal,
            captures: vec![canonical(&borrowed_capture)],
        };
        let GcFinalizerPlan::ClosureEnvironment { drops, .. } =
            finalizer_plan(&program, &borrowed_key)
        else {
            panic!("expected a closure-environment plan")
        };
        assert!(
            drops.as_ref().expect("expanded").is_empty(),
            "a borrowed capture is skipped"
        );
        let borrowed_site = closure_site_capture(&program, borrowed_closure, &borrowed_capture);
        assert!(borrowed_site.capture.borrowed, "the capture is borrowed");

        // A derived capture fires the gate but the body skips it.
        let derived_key = GcFinalizerKey::ClosureEnvironment {
            closure: program
                .instances
                .get(derived_closure)
                .expect("instance")
                .ordinal,
            captures: vec![canonical(&derived_capture)],
        };
        let GcFinalizerPlan::ClosureEnvironment { drops, .. } =
            finalizer_plan(&program, &derived_key)
        else {
            panic!("expected a closure-environment plan")
        };
        assert!(
            drops.as_ref().expect("expanded").is_empty(),
            "a derived capture is skipped by the finalizer body"
        );

        // Two generic instantiations produce two plans that each drop their own
        // capture.
        for (closure, capture) in [
            (generic_string_closure, CheckedType::CString),
            (generic_wrapped_closure, wrapped_capture.clone()),
        ] {
            let key = GcFinalizerKey::ClosureEnvironment {
                closure: program.instances.get(closure).expect("instance").ordinal,
                captures: vec![canonical(&capture)],
            };
            let GcFinalizerPlan::ClosureEnvironment { drops, .. } = finalizer_plan(&program, &key)
            else {
                panic!("expected a closure-environment plan")
            };
            let drops = drops.as_ref().expect("expanded");
            assert_eq!(drops.len(), 1, "{capture:?}: {drops:?}");
            assert_finalizer_glue(&program, &drops[0].glue, &capture);
        }

        // Every planned dropped capture agrees with the construction site's
        // `drops_value`, and the install gate matches for every Fresh closure
        // construction.
        for (id, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for (_, value) in body.callable_values.iter() {
                let Some(closure) = &value.closure else {
                    continue;
                };
                if closure.environment != crate::LoweredClosureEnvironment::Fresh
                    || closure.captures.is_empty()
                {
                    continue;
                }
                let closure_instance = closure_instance_capturing_types(
                    &program,
                    closure.function,
                    closure.captures.iter().map(|capture| &capture.value_type),
                )
                .unwrap_or_else(|| {
                    panic!(
                        "closure in instance {} has no interned instance",
                        id.index()
                    )
                });
                let key = GcFinalizerKey::ClosureEnvironment {
                    closure: program
                        .instances
                        .get(closure_instance)
                        .expect("closure instance")
                        .ordinal,
                    captures: closure
                        .captures
                        .iter()
                        .map(|capture| canonical(&capture.value_type))
                        .collect(),
                };
                let plan = program
                    .specializations
                    .artifact_ordinal(&ArtifactRequestKey::GcFinalizer(key))
                    .map(|ordinal| {
                        program
                            .artifacts
                            .iter()
                            .find(|(_, record)| record.ordinal == ordinal)
                            .map(|(_, record)| record)
                            .expect("artifact")
                    })
                    .and_then(|artifact| match artifact.plan.as_ref() {
                        Some(LoweredArtifactPlan::GcFinalizer(
                            GcFinalizerPlan::ClosureEnvironment { drops, .. },
                        )) => drops.clone(),
                        _ => None,
                    });
                let gate = closure.captures.iter().any(|capture| {
                    !capture.requires_initialization_state
                        && !capture.capture.borrowed
                        && program.concrete_needs_drop(&capture.value_type)
                });
                if gate {
                    let drops = plan.unwrap_or_else(|| {
                        panic!(
                            "the install gate fired for a closure in instance {} but no finalizer was requested",
                            id.index()
                        )
                    });
                    let dropped = drops.iter().map(|drop| drop.index).collect::<HashSet<_>>();
                    let expected = closure
                        .captures
                        .iter()
                        .enumerate()
                        .filter(|(_, capture)| {
                            capture.owns_value && program.concrete_needs_drop(&capture.value_type)
                        })
                        .map(|(index, _)| index)
                        .collect::<HashSet<_>>();
                    assert_eq!(
                        dropped,
                        expected,
                        "the finalizer drops exactly the owned droppable captures in instance {}",
                        id.index()
                    );
                } else if let Some(drops) = plan {
                    // The gate excludes this construction, so the emitter never
                    // installs the finalizer; a plan requested by a
                    // cleanup fixture must still drop nothing.
                    assert!(
                        drops.is_empty(),
                        "a finalizer for a gate-excluded closure drops nothing in instance {}",
                        id.index()
                    );
                }
            }
        }
        eprintln!(
            "artifact finalizer closure stats: {:?}",
            program.closure_stats
        );
    }

    const BUFFER_CLONE_FIXTURE: &str = concat!(
        "use std.buffer.*\n",
        "use std.clone.Clone\n",
        "use std.cinterop.(CString, c_string)\n",
        "type Owned = ctor I32\n",
        "impl Drop Owned { def drop = Owned value => () }\n",
        "impl Clone Owned { def clone = Owned value => Owned value }\n",
        "def clone_copy: (Buffer I32) -> Buffer I32 = buffer => Clone.clone buffer\n",
        "def clone_owned: (Buffer Owned) -> Buffer Owned = buffer => Clone.clone buffer\n",
        "def clone_nested: (Buffer (Buffer I32)) -> Buffer (Buffer I32) = buffer => Clone.clone buffer\n",
        "let kept = 1\n",
    );

    #[test]
    fn buffer_clone_sites_select_the_clone_instance_and_finalizer() {
        let module = checked_program(BUFFER_CLONE_FIXTURE);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("the buffer-clone fixture lowers and validates");
        let program = &lowered.program;
        let clone_trait = program
            .semantic_ids
            .clone_trait
            .expect("the checker selected the Clone trait");
        let method = program
            .traits
            .get(clone_trait)
            .and_then(|trait_| trait_.methods.first())
            .copied()
            .expect("Clone declares a method");

        let mut element_sites = 0;
        let mut finalizer_sites = 0;
        for (_, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for use_ in &body.instance_uses {
                let ArtifactUseSite::BufferCloneElement(call) = use_.site else {
                    continue;
                };
                element_sites += 1;
                assert_eq!(
                    use_.kind,
                    LoweredInstanceDependencyKind::CloneMethod,
                    "the element copy is a clone-method instance use"
                );
                let call = body.call(call).expect("the clone call");
                let CheckedType::Buffer(element) = &call.result_type else {
                    panic!("a buffer clone result is a buffer: {:?}", call.result_type);
                };
                let expected = module
                    .trait_impl_method(clone_trait, std::slice::from_ref(element), method)
                    .expect("the emitter selects a Clone method for the element");
                let bound = program
                    .instances
                    .get(use_.instance)
                    .expect("the selected clone instance");
                assert_eq!(
                    bound.template, expected,
                    "the planned element clone matches the typed-module selection for {element}"
                );
            }
            for use_ in &body.artifact_uses {
                if matches!(use_.site, ArtifactUseSite::BufferCloneFinalizer(_)) {
                    finalizer_sites += 1;
                }
            }
        }
        assert!(
            element_sites >= 3,
            "every buffer clone selects an element Clone instance: {element_sites}"
        );
        // Only the CString-backed element needs a buffer finalizer.
        assert!(
            finalizer_sites >= 1,
            "a buffer of droppable elements clones with a destination finalizer: {finalizer_sites}"
        );
    }

    /// The single instance of one declared function.
    fn instance_of(program: &LoweredProgram, name: &str) -> FunctionInstanceId {
        let template = program
            .functions
            .iter()
            .find(|(_, _, function)| {
                function.name == name || function.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"));
        program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == template)
            .map(|(id, _)| id)
            .unwrap_or_else(|| panic!("no instance of {name}"))
    }

    fn instance_bindings(program: &LoweredProgram, name: &str) -> Vec<(OwnedStorage, CheckedType)> {
        program
            .instances
            .get(instance_of(program, name))
            .and_then(|instance| instance.body.as_ref())
            .map(|body| {
                body.owned_bindings
                    .iter()
                    .map(|record| (record.storage, record.value_type.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    const SCANNER_FIXTURE: &str = concat!(
        "use std.cinterop.*\n",
        "extern \"c\" { inspect: CString -> I32 }\n",
        "def make_c: () -> CString = () => c_string \"x\"\n",
        "def extern_temp: () -> I32 = () => inspect (c_string \"x\")\n",
        "def discard_result: () -> () = () => { make_c (); () }\n",
        "def ignore: (CString) -> I32 = _ => 0\n",
        "def drop_body: () -> Never = () => loop { make_c () }\n",
        "def owned_param: move CString -> CString = move value => value\n",
        "def mutate_c: move (CString, CString) -> (CString, CString) = move pair => {\n",
        "  let mut copy = pair\n",
        "  copy[0] = c_string \"b\"\n",
        "  copy\n",
        "}\n",
        "def cell_finalizer: () -> (() -> I32) = () => {\n",
        "  let mut cell = c_string \"a\"\n",
        "  cell = c_string \"b\"\n",
        "  () => inspect cell\n",
        "}\n",
        "def closure_env: move CString -> (() -> I32) = move value => () => inspect value\n",
        "def make_ref: () -> Ref CString = () => Ref (c_string \"x\")\n",
        "def convert: CString -> String = value => CString.to_string value\n",
        "def nested: (I32) -> I32 = value => {\n",
        "  let outer = c_string \"a\"\n",
        "  when { value > 0 => { let inner = c_string \"b\"; inspect inner }, else => inspect outer }\n",
        "}\n",
        "def loop_local: () -> I32 = () => loop {\n",
        "  let item = c_string \"x\"\n",
        "  break (inspect item)\n",
        "}\n",
        "def loop_body_drop: () -> Never = () => loop { make_c () }\n",
        "let discarded = discard_result ()\n",
        "let ignored = ignore (c_string \"x\")\n",
        "let mutated = mutate_c (c_string \"a\", c_string \"c\")\n",
        "let celled = cell_finalizer ()\n",
        "let closure = closure_env (c_string \"e\")\n",
        "let reference = make_ref ()\n",
        "let converted = convert (c_string \"c\")\n",
        "let nested_value = nested 1\n",
    );

    #[test]
    fn cleanup_scanner_records_sites_and_owned_bindings() {
        let module = checked_program(SCANNER_FIXTURE);
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("the scanner fixture lowers and validates");
        let program = &lowered.program;

        let mut sites = HashSet::new();
        for (_, instance) in program.instances.iter() {
            if let Some(body) = &instance.body {
                sites.extend(body.artifact_uses.iter().map(|use_| use_.site));
            }
        }
        for uses in &program.initializer_artifact_uses {
            sites.extend(uses.iter().map(|use_| use_.site));
        }
        let has = |predicate: fn(&ArtifactUseSite) -> bool| sites.iter().any(predicate);
        assert!(
            has(|site| matches!(site, ArtifactUseSite::DiscardedResult(_))),
            "a discarded CString statement site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::ReplacedValue(_))),
            "an assignment replacing a droppable value site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::LoopBodyResult(_))),
            "a loop body result drop site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::CStringTemporary(_))),
            "an extern C-string temporary drop site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::WildcardDiscard(_))),
            "a wildcard parameter discard site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::OwnedBinding(_))),
            "an owned binding site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::CellFinalizer(_))),
            "a captured cell finalizer site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::ClosureEnvironment(_))),
            "a closure environment finalizer site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::RefConstruction(_))),
            "a managed `Ref` construction finalizer site"
        );
        assert!(
            has(|site| matches!(site, ArtifactUseSite::CStringConversion(_))),
            "a C-string conversion drop site"
        );

        // A moved parameter is owned; a wildcard parameter registers nothing.
        assert_eq!(
            instance_bindings(program, "owned_param"),
            vec![(OwnedStorage::Value, CheckedType::CString)]
        );
        assert!(instance_bindings(program, "ignore").is_empty());

        // A moved product parameter and a mutable local: value then cell, in
        // registration order.
        let mutated = instance_bindings(program, "mutate_c");
        assert_eq!(mutated.len(), 2, "{mutated:?}");
        assert_eq!(mutated[0].0, OwnedStorage::Value);
        assert_eq!(mutated[1].0, OwnedStorage::Cell);
        assert!(
            matches!(&mutated[0].1, CheckedType::Product(_)),
            "the pair parameter owns the product value: {mutated:?}"
        );
        assert_eq!(mutated[0].1, mutated[1].1);

        // The captured mutable cell is a finalizer, not an owned binding.
        assert!(
            instance_bindings(program, "cell_finalizer").is_empty(),
            "a captured cell is cleaned up by its GC finalizer"
        );

        // Nested block locals register in evaluation order: the outer `let`
        // then the match-arm block's `let`.
        let nested = instance_bindings(program, "nested");
        assert_eq!(
            nested,
            vec![
                (OwnedStorage::Value, CheckedType::CString),
                (OwnedStorage::Value, CheckedType::CString)
            ],
            "the function's own and nested block locals"
        );

        // A loop-body local registers like any other block local.
        assert_eq!(
            instance_bindings(program, "loop_local"),
            vec![(OwnedStorage::Value, CheckedType::CString)]
        );

        // The order is deterministic across repeated lowering.
        let second = Lowerer::new()
            .lower(&module)
            .expect("the second lowering validates");
        let first = program
            .instances
            .iter()
            .map(|(id, instance)| {
                let bindings = instance
                    .body
                    .as_ref()
                    .map(|body| {
                        body.owned_bindings
                            .iter()
                            .map(|record| {
                                (
                                    record.symbol.0,
                                    record.storage,
                                    CanonicalType::concrete(
                                        &record.value_type,
                                        &Origin::compiler(),
                                    )
                                    .expect("concrete owned type"),
                                    record.glue.map(|glue| glue.index()),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                (id.index(), bindings)
            })
            .collect::<Vec<_>>();
        let second = second
            .program
            .instances
            .iter()
            .map(|(id, instance)| {
                let bindings = instance
                    .body
                    .as_ref()
                    .map(|body| {
                        body.owned_bindings
                            .iter()
                            .map(|record| {
                                (
                                    record.symbol.0,
                                    record.storage,
                                    CanonicalType::concrete(
                                        &record.value_type,
                                        &Origin::compiler(),
                                    )
                                    .expect("concrete owned type"),
                                    record.glue.map(|glue| glue.index()),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                (id.index(), bindings)
            })
            .collect::<Vec<_>>();
        assert_eq!(first, second, "owned-binding order is deterministic");
    }

    #[test]
    fn cleanup_scanner_gate_covers_the_standard_library() {
        let module = checked_program(concat!(
            "use std.cinterop.*\n",
            "use std.coroutine.*\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "def make_c: () -> CString = () => c_string \"x\"\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def consume: move Coroutine{} I32 -> I32 = move value => 1\n",
            "let created = task ()\n",
            "let debugged = \"${(1, 2):?}\"\n",
            "let consumed = consume (task ())\n",
            "let stale = { let value = make_c (); inspect value }\n",
        ));
        let lowered = Lowerer::new()
            .lower(&module)
            .expect("the standard-library fixture lowers and validates");
        assert!(
            lowered
                .program
                .instances
                .iter()
                .any(
                    |(_, instance)| instance.body.as_ref().is_some_and(|body| body
                        .owned_bindings
                        .iter()
                        .any(|record| record.glue.is_some()))
                ),
            "the standard-library fixture records owned bindings with bound glue"
        );
        eprintln!(
            "artifact scanner closure stats: {:?}",
            lowered.program.closure_stats
        );
    }

    fn nominal_type_id(program: &LoweredProgram, name: &str) -> crate::TypeId {
        program
            .types
            .iter()
            .find(|(_, _, metadata)| metadata.name == name)
            .map(|(_, _, metadata)| metadata.semantic_id)
            .unwrap_or_else(|| panic!("no lowered type named {name}"))
    }

    fn canonical(value_type: &CheckedType) -> CanonicalType {
        CanonicalType::concrete(value_type, &Origin::compiler()).expect("a concrete type")
    }

    fn closure_site_capture<'a>(
        program: &'a LoweredProgram,
        closure: FunctionInstanceId,
        capture: &CheckedType,
    ) -> &'a LoweredClosureCapture {
        let template = program
            .instances
            .get(closure)
            .expect("closure instance")
            .template;
        for (_, instance) in program.instances.iter() {
            let Some(body) = &instance.body else {
                continue;
            };
            for (_, value) in body.callable_values.iter() {
                let Some(construction) = &value.closure else {
                    continue;
                };
                if construction.function != template {
                    continue;
                }
                if let Some(found) = construction
                    .captures
                    .iter()
                    .find(|candidate| &candidate.value_type == capture)
                {
                    return found;
                }
            }
        }
        panic!("no construction site captures {capture:?}")
    }

    fn closure_instance_capturing_types<'a>(
        program: &'a LoweredProgram,
        function: crate::FunctionId,
        capture_types: impl Iterator<Item = &'a CheckedType>,
    ) -> Option<FunctionInstanceId> {
        let expected = capture_types.cloned().collect::<Vec<_>>();
        program.instances.iter().find_map(|(id, instance)| {
            if instance.template != function {
                return None;
            }
            let body = instance.body.as_ref()?;
            let captures = body
                .captures()
                .iter()
                .map(|capture| capture.value_type.clone())
                .collect::<Vec<_>>();
            (captures == expected).then_some(id)
        })
    }
}
