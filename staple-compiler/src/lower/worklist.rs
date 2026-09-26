//! Stage 3.3: the deterministic specialization worklist.
//!
//! The worklist is the reachable function-instance graph for one lowered
//! program. Roots are module initializer bodies in program initialization
//! order followed by the concrete function templates the current backend
//! emits eagerly (every template whose checked signature contains no declared
//! type parameter and whose relevant-parameter set is empty, so a nested
//! closure that captures an enclosing parameter is never seeded without its
//! construction site's environment). Coroutine body thunks stay demand-driven.
//!
//! Every requested `InstanceKey` is interned in the `SpecializationCatalog`
//! before the instance body is visited, so self-recursion and mutual recursion
//! converge on the reserved ordinal instead of interning a second key. Bodies
//! are materialized in Stage 3.4; this module records identity, root and
//! first-discovery order, typed dependency edges, and generated-artifact
//! requests only.

use std::collections::{HashMap, HashSet};

use staple_syntax::{Diagnostic, Span};

use crate::specialization::{
    ArtifactOrdinal, ArtifactRequestKey, ConstructorAdapterKey, InstanceOrdinal,
    SpecializationCatalog, SpecializationNameCollision, StructuralMethodKey,
};
use crate::{
    CheckedFunctionType, CheckedType, FunctionId, TraitId, TraitMethodId, contains_type_parameter,
    infer_type_parameters, substitute_type,
};

use super::instance_resolution::{InstanceResolutionRequest, InstanceResolutionTarget};
use super::{
    Arena, ArenaId, BlockId, CallSubstitutions, ExpressionId, FunctionInstanceId, InitializerId,
    ItemId, LoweredArtifactRequestId, LoweredAwaitId, LoweredCall, LoweredCallEnvironment,
    LoweredCallId, LoweredCallStep, LoweredCallableAdapter, LoweredCallableTarget,
    LoweredCallableValueId, LoweredClosureEnvironment, LoweredCoroId, LoweredCoroutinePlanId,
    LoweredExpressionKind, LoweredItemKind, LoweredPlaceKind, LoweredProductStep, LoweredProgram,
    LoweredReactiveCallbackId, LoweredReactiveOperationId, LoweredReactiveOperationKind,
    LoweredStringTemplatePart, LoweredWithId, Origin, PatternId, PlaceId, RelevantParameters,
    ResolvedInstanceRequest, SubstitutionEnvironment, TraitEvidence,
};

/// One reachable function instance in first-discovery order.
#[derive(Debug, Clone)]
pub(crate) struct LoweredFunctionInstance {
    /// The template declaration origin.
    pub origin: Origin,
    /// The source function template this instance materializes in Stage 3.4.
    pub template: FunctionId,
    /// Position in `SpecializationCatalog`, equal to the arena ID index.
    pub ordinal: InstanceOrdinal,
    /// Stable generated symbol name planned by the catalog.
    pub name: String,
    /// How the instance was first requested. Roots precede discovered
    /// dependencies in the stable emission order.
    pub request: LoweredInstanceRequest,
    /// The resolved environment Stage 3.4 reuses for body substitution.
    pub environment: SubstitutionEnvironment,
    /// The template's relevant parameter set.
    pub relevant: RelevantParameters,
    /// The resolved selection when it changes this instance's emitted body.
    pub evidence: Option<TraitEvidence>,
    /// Typed dependency edges in the order the sites were traversed.
    pub dependencies: Vec<LoweredInstanceDependency>,
    /// Generated-artifact references in traversal order.
    pub artifacts: Vec<LoweredArtifactDependency>,
    /// The concrete Stage 3.4 body. `None` until materialization runs and for
    /// instances whose template has no runtime body (externs, intrinsics).
    pub body: Option<super::instance_body::LoweredInstanceBody>,
}

/// How one function instance entered the graph.
#[derive(Debug, Clone)]
pub(crate) enum LoweredInstanceRequest {
    /// Discovered from a module initializer body in initialization order.
    Initializer {
        initializer: InitializerId,
        kind: LoweredInstanceDependencyKind,
        origin: Origin,
    },
    /// Seeded because the current backend emits this concrete template eagerly.
    EagerTemplate,
    /// Discovered while traversing another instance's body.
    Dependency {
        owner: FunctionInstanceId,
        kind: LoweredInstanceDependencyKind,
        origin: Origin,
    },
}

/// The lowered-record route that requested a function instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LoweredInstanceDependencyKind {
    /// A direct call target.
    DirectCall,
    /// The formatter constructor selected for a string template.
    FormattingConstructor,
    /// The formatter finish function selected for a string template.
    FormattingFinish,
    /// A function-valued name, selector, or closure construction.
    CallableValue,
    /// An implicit thunk adapted into a call argument.
    ImplicitThunkArgument,
    /// The method function selected by a trait-dependent site.
    TraitMethod,
    /// A derived binding's evaluator thunk.
    DerivedEvaluator,
    /// A reaction/`until`/`batch` callback thunk.
    ReactiveCallback,
    /// A coroutine body thunk created by a `coro` expression.
    CoroutineBody,
}

impl LoweredInstanceDependencyKind {
    /// Stable description for diagnostics and snapshots.
    pub(crate) fn description(self) -> &'static str {
        match self {
            LoweredInstanceDependencyKind::DirectCall => "direct-call",
            LoweredInstanceDependencyKind::FormattingConstructor => "formatting-constructor",
            LoweredInstanceDependencyKind::FormattingFinish => "formatting-finish",
            LoweredInstanceDependencyKind::CallableValue => "callable-value",
            LoweredInstanceDependencyKind::ImplicitThunkArgument => "implicit-thunk-argument",
            LoweredInstanceDependencyKind::TraitMethod => "trait-method",
            LoweredInstanceDependencyKind::DerivedEvaluator => "derived-evaluator",
            LoweredInstanceDependencyKind::ReactiveCallback => "reactive-callback",
            LoweredInstanceDependencyKind::CoroutineBody => "coroutine-body",
        }
    }
}

/// One typed edge from an instance body to a requested instance.
#[derive(Debug, Clone)]
pub(crate) struct LoweredInstanceDependency {
    pub instance: FunctionInstanceId,
    pub origin: Origin,
    pub kind: LoweredInstanceDependencyKind,
}

/// One constructor-adapter or structural-method request.
#[derive(Debug, Clone)]
pub(crate) struct LoweredArtifactRequest {
    pub ordinal: ArtifactOrdinal,
    /// Stable generated symbol name planned by the catalog.
    pub name: String,
    /// The requesting site.
    pub origin: Origin,
    pub request: LoweredArtifactRequestRoot,
}

/// Who first requested a generated artifact.
#[derive(Debug, Clone)]
pub(crate) enum LoweredArtifactRequestRoot {
    Initializer {
        initializer: InitializerId,
        origin: Origin,
    },
    Instance {
        instance: FunctionInstanceId,
        kind: LoweredArtifactDependencyKind,
        origin: Origin,
    },
}

/// The lowered-record route that requested a generated artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LoweredArtifactDependencyKind {
    /// A constructor value adapter.
    ConstructorAdapter,
    /// A compiler-generated structural trait method.
    StructuralMethod,
}

/// One artifact reference recorded on its requesting instance.
#[derive(Debug, Clone)]
pub(crate) struct LoweredArtifactDependency {
    pub artifact: ArtifactOrdinal,
    pub origin: Origin,
    pub kind: LoweredArtifactDependencyKind,
}

/// A compiler-helper request the Stage 3 graph carries unresolved. Stage 4
/// generates the helper body and discovers its dependencies before LLVM
/// migration.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCompilerHelperRequest {
    pub function: FunctionId,
    pub origin: Origin,
    pub requested_by: LoweredHelperRequester,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredHelperRequester {
    Initializer(InitializerId),
    Instance(FunctionInstanceId),
}

/// The built worklist before it is installed on the lowered program.
pub(super) struct SpecializationParts {
    pub instances: Arena<LoweredFunctionInstance, FunctionInstanceId>,
    pub artifacts: Arena<LoweredArtifactRequest, LoweredArtifactRequestId>,
    pub helper_requests: Vec<LoweredCompilerHelperRequest>,
    pub catalog: SpecializationCatalog,
}

/// Builds the deterministic worklist for one lowered program.
pub(super) fn build(program: &LoweredProgram) -> Result<SpecializationParts, Vec<Diagnostic>> {
    WorklistBuilder::new(program).build()
}

/// The record that requested an instance or artifact.
#[derive(Debug, Clone, Copy)]
enum TraversalOwner {
    Initializer(InitializerId),
    Instance(FunctionInstanceId),
}

impl TraversalOwner {
    fn helper_requester(self) -> LoweredHelperRequester {
        match self {
            TraversalOwner::Initializer(initializer) => {
                LoweredHelperRequester::Initializer(initializer)
            }
            TraversalOwner::Instance(instance) => LoweredHelperRequester::Instance(instance),
        }
    }
}

/// Visited nodes for one root/instance body traversal. Shared occurrences are
/// visited once; arena trees cannot cycle, so the sets also bound repeated
/// work across shared memoized expressions.
#[derive(Default)]
struct TraversalVisited {
    blocks: HashSet<BlockId>,
    items: HashSet<ItemId>,
    expressions: HashSet<ExpressionId>,
    places: HashSet<PlaceId>,
    patterns: HashSet<PatternId>,
    calls: HashSet<LoweredCallId>,
    callable_values: HashSet<LoweredCallableValueId>,
    operations: HashSet<LoweredReactiveOperationId>,
    callbacks: HashSet<LoweredReactiveCallbackId>,
    coros: HashSet<LoweredCoroId>,
    awaits: HashSet<LoweredAwaitId>,
    plans: HashSet<LoweredCoroutinePlanId>,
    arguments: HashSet<(LoweredCallId, usize)>,
}

impl TraversalVisited {
    fn clear(&mut self) {
        self.blocks.clear();
        self.items.clear();
        self.expressions.clear();
        self.places.clear();
        self.patterns.clear();
        self.calls.clear();
        self.callable_values.clear();
        self.operations.clear();
        self.callbacks.clear();
        self.coros.clear();
        self.awaits.clear();
        self.plans.clear();
        self.arguments.clear();
    }
}

struct WorklistBuilder<'a> {
    program: &'a LoweredProgram,
    catalog: SpecializationCatalog,
    instances: Arena<LoweredFunctionInstance, FunctionInstanceId>,
    artifacts: Arena<LoweredArtifactRequest, LoweredArtifactRequestId>,
    helper_requests: Vec<LoweredCompilerHelperRequest>,
    queue: Vec<FunctionInstanceId>,
    cursor: usize,
    diagnostics: Vec<Diagnostic>,
    visited: TraversalVisited,
}

impl<'a> WorklistBuilder<'a> {
    fn new(program: &'a LoweredProgram) -> Self {
        WorklistBuilder {
            program,
            catalog: SpecializationCatalog::default(),
            instances: Arena::default(),
            artifacts: Arena::default(),
            helper_requests: Vec::new(),
            queue: Vec::new(),
            cursor: 0,
            diagnostics: Vec::new(),
            visited: TraversalVisited::default(),
        }
    }

    fn build(mut self) -> Result<SpecializationParts, Vec<Diagnostic>> {
        self.seed_initializers();
        self.seed_eager_templates();
        self.process_queue();
        if !self.diagnostics.is_empty() {
            return Err(self.diagnostics);
        }
        self.assign_names()?;
        Ok(SpecializationParts {
            instances: self.instances,
            artifacts: self.artifacts,
            helper_requests: self.helper_requests,
            catalog: self.catalog,
        })
    }

    /// Root order begins with module initializer bodies in program
    /// initialization order; lowering inserts initializers in that order.
    fn seed_initializers(&mut self) {
        let initializers = self
            .program
            .initializers
            .iter()
            .map(|(id, initializer)| (id, initializer.body))
            .collect::<Vec<_>>();
        for (id, body) in initializers {
            self.visited.clear();
            self.traverse_block(body, TraversalOwner::Initializer(id), None);
        }
    }

    /// Root order continues with the same concrete templates the current
    /// backend emits eagerly: signature-free templates and implicit thunks
    /// with no declared parameter. Coroutine body thunks stay demand-driven,
    /// and unused generic templates are never seeded.
    fn seed_eager_templates(&mut self) {
        let eager = self
            .program
            .functions
            .iter()
            .filter(|(_, _, function)| !function.class.coroutine_body)
            .filter(|(_, _, function)| {
                !contains_type_parameter(&CheckedType::Function(function.signature.clone()))
            })
            // A concrete signature is not sufficient: a nested closure whose
            // body or captures still depend on an enclosing parameter needs its
            // construction site's environment, so it stays demand-driven
            // instead of becoming a root without an enclosing instance.
            .filter(|(_, id, _)| self.program.relevant_parameters(*id).is_empty())
            .map(|(_, id, function)| (id, function.signature.clone()))
            .collect::<Vec<_>>();
        for (function, signature) in eager {
            self.request_eager(function, signature);
        }
    }

    fn request_eager(&mut self, function: FunctionId, signature: CheckedFunctionType) {
        let origin = self.function_origin(function);
        let request = InstanceResolutionRequest {
            function,
            origin: origin.clone(),
            function_type: signature,
            substitutions: CallSubstitutions::default(),
            evidence: None,
            target: InstanceResolutionTarget::Root,
        };
        match self.program.resolve_instance_request(&request) {
            Ok(resolved) => {
                self.intern_resolved(resolved, LoweredInstanceRequest::EagerTemplate);
            }
            Err(diagnostic) => self.diagnostics.push(diagnostic),
        }
    }

    fn process_queue(&mut self) {
        while self.cursor < self.queue.len() {
            let instance = self.queue[self.cursor];
            self.cursor += 1;
            self.traverse_instance(instance);
        }
    }

    fn traverse_instance(&mut self, instance: FunctionInstanceId) {
        let enclosing = self.resolved_request(instance);
        let function = enclosing.key.function();
        let body = self
            .program
            .functions
            .get(function)
            .and_then(|function| function.body);
        let Some(body) = body else {
            return;
        };
        self.visited.clear();
        self.traverse_block(body, TraversalOwner::Instance(instance), Some(&enclosing));
    }

    /// Reconstructs the resolver view of an interned instance for nested and
    /// current-environment requests.
    fn resolved_request(&self, instance: FunctionInstanceId) -> ResolvedInstanceRequest {
        let record = self
            .instances
            .get(instance)
            .expect("worklist queue holds interned instances");
        let key = self
            .catalog
            .instance(record.ordinal)
            .expect("interned instance has a catalog key")
            .clone();
        let origin = match &record.request {
            LoweredInstanceRequest::Initializer { origin, .. }
            | LoweredInstanceRequest::Dependency { origin, .. } => origin.clone(),
            LoweredInstanceRequest::EagerTemplate => record.origin.clone(),
        };
        ResolvedInstanceRequest {
            key,
            environment: record.environment.clone(),
            relevant: record.relevant.clone(),
            evidence: record.evidence.clone(),
            origin,
        }
    }

    fn function_origin(&self, function: FunctionId) -> Origin {
        self.program
            .functions
            .get(function)
            .map(|function| function.origin.clone())
            .unwrap_or_else(Origin::compiler)
    }

    fn function_signature(&self, function: FunctionId) -> Option<CheckedFunctionType> {
        self.program
            .functions
            .get(function)
            .map(|function| function.signature.clone())
    }

    fn intern_resolved(
        &mut self,
        resolved: ResolvedInstanceRequest,
        request: LoweredInstanceRequest,
    ) -> FunctionInstanceId {
        let key = resolved.key.clone();
        let ordinal = self.catalog.reserve_instance(key);
        let id = FunctionInstanceId::from_index(ordinal.index());
        if id.index() >= self.instances.len() {
            let origin = self.function_origin(resolved.key.function());
            // Only the template's own relevant parameters stay in the
            // instance environment. The unpruned environment also carries the
            // requesting site's trait-parameter mappings, which are not
            // declared by this template and would otherwise collide with the
            // same trait parameter's mappings at nested sites.
            let environment = resolved.environment.pruned(&resolved.relevant);
            self.instances.push(LoweredFunctionInstance {
                origin,
                template: resolved.key.function(),
                ordinal,
                name: String::new(),
                request,
                environment,
                relevant: resolved.relevant,
                evidence: resolved.evidence,
                dependencies: Vec::new(),
                artifacts: Vec::new(),
                body: None,
            });
            self.queue.push(id);
        }
        id
    }

    fn record_instance_edge(
        &mut self,
        owner: TraversalOwner,
        instance: FunctionInstanceId,
        origin: &Origin,
        kind: LoweredInstanceDependencyKind,
    ) {
        if let TraversalOwner::Instance(owner) = owner
            && let Some(record) = self.instances.get_mut(owner)
        {
            record.dependencies.push(LoweredInstanceDependency {
                instance,
                origin: origin.clone(),
                kind,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn request_function(
        &mut self,
        function: FunctionId,
        origin: &Origin,
        function_type: CheckedFunctionType,
        substitutions: CallSubstitutions,
        evidence: Option<TraitEvidence>,
        target: InstanceResolutionTarget<'_>,
        owner: TraversalOwner,
        kind: LoweredInstanceDependencyKind,
    ) -> Option<FunctionInstanceId> {
        // A function with no relevant template parameters has exactly one
        // instance. The requesting site's callable type may be a coerced view
        // of the template (for example a `Never` result coerced to the
        // enclosing function's result type), which carries no instance
        // identity; the template signature is then the correct request.
        let (function_type, substitutions, evidence) =
            if self.program.relevant_parameters(function).is_empty() {
                match self.function_signature(function) {
                    Some(signature) => (signature, CallSubstitutions::default(), None),
                    None => (function_type, substitutions, evidence),
                }
            } else {
                (function_type, substitutions, evidence)
            };
        let request = InstanceResolutionRequest {
            function,
            origin: origin.clone(),
            function_type,
            substitutions,
            evidence,
            target,
        };
        match self.program.resolve_instance_request(&request) {
            Ok(resolved) => {
                let request_root = match owner {
                    TraversalOwner::Initializer(initializer) => {
                        LoweredInstanceRequest::Initializer {
                            initializer,
                            kind,
                            origin: origin.clone(),
                        }
                    }
                    TraversalOwner::Instance(instance) => LoweredInstanceRequest::Dependency {
                        owner: instance,
                        kind,
                        origin: origin.clone(),
                    },
                };
                let id = self.intern_resolved(resolved, request_root);
                self.record_instance_edge(owner, id, origin, kind);
                Some(id)
            }
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                None
            }
        }
    }

    fn request_artifact(
        &mut self,
        key: ArtifactRequestKey,
        origin: &Origin,
        owner: TraversalOwner,
        kind: LoweredArtifactDependencyKind,
    ) {
        let ordinal = self.catalog.reserve_artifact(key);
        if ordinal.index() >= self.artifacts.len() {
            let request = match owner {
                TraversalOwner::Initializer(initializer) => {
                    LoweredArtifactRequestRoot::Initializer {
                        initializer,
                        origin: origin.clone(),
                    }
                }
                TraversalOwner::Instance(instance) => LoweredArtifactRequestRoot::Instance {
                    instance,
                    kind,
                    origin: origin.clone(),
                },
            };
            self.artifacts.push(LoweredArtifactRequest {
                ordinal,
                name: String::new(),
                origin: origin.clone(),
                request,
            });
        }
        if let TraversalOwner::Instance(instance) = owner
            && let Some(record) = self.instances.get_mut(instance)
        {
            record.artifacts.push(LoweredArtifactDependency {
                artifact: ordinal,
                origin: origin.clone(),
                kind,
            });
        }
    }

    fn request_helper(&mut self, function: FunctionId, origin: &Origin, owner: TraversalOwner) {
        self.helper_requests.push(LoweredCompilerHelperRequest {
            function,
            origin: origin.clone(),
            requested_by: owner.helper_requester(),
        });
    }

    fn assign_names(&mut self) -> Result<(), Vec<Diagnostic>> {
        let names = match self.catalog.planned_names() {
            Ok(names) => names,
            Err(SpecializationNameCollision { name }) => {
                return Err(vec![Diagnostic::new(
                    Span::Compiler,
                    format!("specialization name collision: `{name}`"),
                )]);
            }
        };
        let instance_count = self.instances.len();
        for (index, name) in names.iter().take(instance_count).enumerate() {
            if let Some(record) = self
                .instances
                .get_mut(FunctionInstanceId::from_index(index))
            {
                record.name = name.clone();
            }
        }
        for (index, name) in names.iter().skip(instance_count).enumerate() {
            if let Some(record) = self
                .artifacts
                .get_mut(LoweredArtifactRequestId::from_index(index))
            {
                record.name = name.clone();
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Body traversal, in lowered evaluation order.
    // ------------------------------------------------------------------

    fn traverse_block(
        &mut self,
        block: BlockId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.blocks.insert(block) {
            return;
        }
        let program = self.program;
        let Some(block) = program.blocks.get(block) else {
            return;
        };
        let items = block.items.clone();
        let result = block.result;
        for item in items {
            self.traverse_item(item, owner, enclosing);
        }
        if let Some(result) = result {
            self.traverse_expression(result, owner, enclosing);
        }
    }

    fn traverse_item(
        &mut self,
        item: ItemId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.items.insert(item) {
            return;
        }
        let program = self.program;
        let Some(item) = program.items.get(item) else {
            return;
        };
        match &item.kind {
            LoweredItemKind::Binding(binding) => {
                // A generic binding's value is a compile-time function
                // template: the backend records only its initialization state
                // and never evaluates the template value.
                if !binding.generic {
                    if let Some(value) = binding.value {
                        self.traverse_expression(value, owner, enclosing);
                    }
                    if let Some(operation) = binding.reactive {
                        self.traverse_reactive_operation(operation, owner, enclosing);
                    }
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                self.traverse_expression(binding.value, owner, enclosing);
            }
            LoweredItemKind::Assignment(assignment) => {
                self.traverse_place(assignment.target, owner, enclosing);
                self.traverse_expression(assignment.value, owner, enclosing);
                if let (Some(dispatch), Some(evidence)) =
                    (&assignment.mutate_index, &assignment.evidence)
                {
                    let trait_id = evidence_trait_id(evidence);
                    let substitutions =
                        trait_site_substitutions(program, trait_id, &dispatch.arguments);
                    self.handle_trait_site(
                        &item.origin,
                        trait_id,
                        dispatch.method,
                        evidence,
                        None,
                        substitutions,
                        enclosing,
                        owner,
                    );
                }
                if let Some(operation) = assignment.signal_notify {
                    self.traverse_reactive_operation(operation, owner, enclosing);
                }
            }
            LoweredItemKind::Return(item) => {
                self.traverse_expression(item.value, owner, enclosing);
            }
            LoweredItemKind::Break(item) => {
                if let Some(value) = item.value {
                    self.traverse_expression(value, owner, enclosing);
                }
            }
            LoweredItemKind::Continue(_) => {}
            LoweredItemKind::Expression(item) => {
                self.traverse_expression(item.expression, owner, enclosing);
            }
        }
    }

    fn traverse_expression(
        &mut self,
        expression: ExpressionId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.expressions.insert(expression) {
            return;
        }
        let program = self.program;
        let Some(expression) = program.expressions.get(expression) else {
            return;
        };
        match &expression.kind {
            LoweredExpressionKind::Deferred(_) | LoweredExpressionKind::Stage26Deferred(_) => {}
            LoweredExpressionKind::Block(block) => {
                self.traverse_block(*block, owner, enclosing);
            }
            LoweredExpressionKind::Name(name) => {
                if let Some(operation) = name.reactive {
                    self.traverse_reactive_operation(operation, owner, enclosing);
                }
            }
            LoweredExpressionKind::Integer(_)
            | LoweredExpressionKind::Float(_)
            | LoweredExpressionKind::String(_)
            | LoweredExpressionKind::CString(_) => {}
            LoweredExpressionKind::Access(access) => {
                self.traverse_expression(access.base, owner, enclosing);
            }
            LoweredExpressionKind::Product(product) => {
                for step in &product.steps {
                    match step {
                        LoweredProductStep::Positional { expression, .. }
                        | LoweredProductStep::Designated { expression, .. }
                        | LoweredProductStep::PositionalSpread { expression, .. }
                        | LoweredProductStep::NamedSpread { expression, .. }
                        | LoweredProductStep::Default { expression, .. } => {
                            self.traverse_expression(*expression, owner, enclosing);
                        }
                    }
                }
                for field in &product.fields {
                    self.traverse_expression(*field, owner, enclosing);
                }
            }
            LoweredExpressionKind::RepeatedProduct(product) => {
                self.traverse_expression(product.expression, owner, enclosing);
            }
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.traverse_expression(satisfies.value, owner, enclosing);
            }
            LoweredExpressionKind::Logical(logical) => {
                self.traverse_expression(logical.left, owner, enclosing);
                self.traverse_expression(logical.right, owner, enclosing);
            }
            LoweredExpressionKind::Loop(loop_) => {
                self.traverse_block(loop_.body, owner, enclosing);
            }
            LoweredExpressionKind::Match(match_) => {
                self.traverse_expression(match_.subject, owner, enclosing);
                for arm in &match_.arms {
                    self.traverse_pattern(arm.pattern, owner, enclosing);
                    self.traverse_expression(arm.body, owner, enclosing);
                }
            }
            LoweredExpressionKind::Index(index) => {
                self.traverse_expression(index.base, owner, enclosing);
                self.traverse_expression(index.index, owner, enclosing);
                let substitutions =
                    trait_site_substitutions(program, index.trait_id, &index.arguments);
                self.handle_trait_site(
                    &expression.origin,
                    index.trait_id,
                    index.dispatch.method,
                    &index.evidence,
                    index.method_type.clone(),
                    substitutions,
                    enclosing,
                    owner,
                );
            }
            LoweredExpressionKind::StringTemplate(template) => {
                self.request_formatting_helper(
                    program.string_formatting.constructor,
                    &expression.origin,
                    owner,
                    enclosing,
                    LoweredInstanceDependencyKind::FormattingConstructor,
                );
                for part in &template.parts {
                    let LoweredStringTemplatePart::Interpolation(interpolation) = part else {
                        continue;
                    };
                    self.traverse_expression(interpolation.expression, owner, enclosing);
                    let substitutions = trait_site_substitutions(
                        program,
                        interpolation.trait_id,
                        std::slice::from_ref(&interpolation.value_type),
                    );
                    self.handle_trait_site(
                        &expression.origin,
                        interpolation.trait_id,
                        interpolation.method,
                        &interpolation.evidence,
                        None,
                        substitutions,
                        enclosing,
                        owner,
                    );
                }
                self.request_formatting_helper(
                    program.string_formatting.finish,
                    &expression.origin,
                    owner,
                    enclosing,
                    LoweredInstanceDependencyKind::FormattingFinish,
                );
            }
            LoweredExpressionKind::Call(call) => {
                self.traverse_call(*call, owner, enclosing);
            }
            LoweredExpressionKind::CallableValue(value) => {
                self.traverse_callable_value(*value, owner, enclosing);
            }
            LoweredExpressionKind::Resource(_) => {}
            LoweredExpressionKind::With(with) => {
                self.traverse_with(*with, owner, enclosing);
            }
            LoweredExpressionKind::Coro(coro) => {
                self.traverse_coro(*coro, owner, enclosing);
            }
            LoweredExpressionKind::Await(await_) => {
                self.traverse_await(*await_, owner, enclosing);
            }
        }
    }

    fn traverse_pattern(
        &mut self,
        pattern: PatternId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.patterns.insert(pattern) {
            return;
        }
        let program = self.program;
        let Some(pattern) = program.patterns.get(pattern) else {
            return;
        };
        match &pattern.kind {
            super::LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.traverse_pattern(*element, owner, enclosing);
                }
            }
            super::LoweredPatternKind::Nominal { argument, .. } => {
                self.traverse_pattern(*argument, owner, enclosing);
            }
            super::LoweredPatternKind::At { binding, pattern } => {
                self.traverse_pattern(*binding, owner, enclosing);
                self.traverse_pattern(*pattern, owner, enclosing);
            }
            super::LoweredPatternKind::Wildcard
            | super::LoweredPatternKind::Binding { .. }
            | super::LoweredPatternKind::Literal { .. } => {}
        }
    }

    fn traverse_place(
        &mut self,
        place: PlaceId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.places.insert(place) {
            return;
        }
        let program = self.program;
        let Some(place) = program.places.get(place) else {
            return;
        };
        match &place.kind {
            LoweredPlaceKind::Symbol { .. }
            | LoweredPlaceKind::CapturedCell { .. }
            | LoweredPlaceKind::Resource { .. } => {}
            LoweredPlaceKind::Temporary { expression } => {
                self.traverse_expression(*expression, owner, enclosing);
            }
            LoweredPlaceKind::Dereference { reference, .. } => {
                self.traverse_expression(*reference, owner, enclosing);
            }
            LoweredPlaceKind::ProductElement { base, .. }
            | LoweredPlaceKind::Representation { base } => {
                self.traverse_place(*base, owner, enclosing);
            }
            LoweredPlaceKind::Indexed { base, index } => {
                self.traverse_place(*base, owner, enclosing);
                self.traverse_expression(*index, owner, enclosing);
            }
        }
    }

    fn traverse_with(
        &mut self,
        with: LoweredWithId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        let program = self.program;
        let Some(with) = program.withs.get(with) else {
            return;
        };
        self.traverse_expression(with.value, owner, enclosing);
        self.traverse_block(with.body, owner, enclosing);
    }

    fn traverse_coro(
        &mut self,
        coro: LoweredCoroId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        let program = self.program;
        if !self.visited.coros.insert(coro) {
            return;
        }
        let Some(coro) = program.coros.get(coro) else {
            return;
        };
        let Some(plan) = program.coroutine_plans.get(coro.plan) else {
            return;
        };
        if !self.visited.plans.insert(coro.plan) {
            return;
        }
        let thunk = plan.thunk;
        let Some(mut function_type) = self.function_signature(thunk) else {
            return;
        };
        // The checker can leave the thunk signature's deferred effect
        // variable uninstantiated while the creation site's plan records the
        // concrete row. The plan's row is authoritative for the instantiated
        // thunk type; inference then maps the signature's variable onto the
        // part of the row the signature does not fix itself. A genuine outer
        // effect parameter stays in the row and is resolved by the enclosing
        // instance.
        function_type.effects = plan.deferred_effects.clone();
        self.request_function(
            thunk,
            &coro.origin,
            function_type,
            CallSubstitutions::default(),
            None,
            nested_target(enclosing),
            owner,
            LoweredInstanceDependencyKind::CoroutineBody,
        );
    }

    fn traverse_await(
        &mut self,
        await_: LoweredAwaitId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.awaits.insert(await_) {
            return;
        }
        let program = self.program;
        let Some(await_) = program.awaits.get(await_) else {
            return;
        };
        self.traverse_expression(await_.operand, owner, enclosing);
        // A child plan's thunk is discovered at its `coro` creation site,
        // which owns the capture environment; the operand traversal reaches a
        // direct creation. No request is issued from the await link itself.
    }

    fn traverse_call(
        &mut self,
        call_id: LoweredCallId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.calls.insert(call_id) {
            return;
        }
        let program = self.program;
        let Some(call) = program.calls.get(call_id) else {
            return;
        };
        for step in &call.steps {
            match step {
                LoweredCallStep::Callee { expression } => {
                    self.traverse_expression(*expression, owner, enclosing);
                }
                LoweredCallStep::Argument { argument } => {
                    self.traverse_call_argument(call_id, call, *argument, owner, enclosing);
                }
                LoweredCallStep::ProductElement { expression, .. }
                | LoweredCallStep::ProductSpread { expression, .. }
                | LoweredCallStep::NamedProductSpread { expression, .. }
                | LoweredCallStep::Default { expression, .. } => {
                    self.traverse_expression(*expression, owner, enclosing);
                }
                LoweredCallStep::Resource { .. } => {}
                LoweredCallStep::Invoke => self.traverse_call_target(call, owner, enclosing),
            }
        }
        for index in 0..call.arguments.len() {
            self.traverse_call_argument(call_id, call, index, owner, enclosing);
        }
        if let Some(operation) = call.reactive {
            self.traverse_reactive_operation(operation, owner, enclosing);
        }
    }

    fn request_formatting_helper(
        &mut self,
        function: Option<FunctionId>,
        origin: &Origin,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
        kind: LoweredInstanceDependencyKind,
    ) {
        let Some(function) = function else {
            return;
        };
        let Some(function_type) = self.function_signature(function) else {
            return;
        };
        self.request_function(
            function,
            origin,
            function_type,
            CallSubstitutions::default(),
            None,
            nested_target(enclosing),
            owner,
            kind,
        );
    }

    fn traverse_call_argument(
        &mut self,
        call_id: LoweredCallId,
        call: &LoweredCall,
        index: usize,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.arguments.insert((call_id, index)) {
            return;
        }
        let Some(argument) = call.arguments.get(index) else {
            return;
        };
        if let Some(expression) = argument.expression {
            self.traverse_expression(expression, owner, enclosing);
        }
        if let Some(place) = argument.place {
            self.traverse_place(place, owner, enclosing);
        }
        if let Some(thunk) = argument.thunk {
            let Some(function_type) = self.function_signature(thunk) else {
                return;
            };
            self.request_function(
                thunk,
                &call.origin,
                function_type,
                CallSubstitutions::default(),
                None,
                nested_target(enclosing),
                owner,
                LoweredInstanceDependencyKind::ImplicitThunkArgument,
            );
        }
    }

    fn traverse_call_target(
        &mut self,
        call: &LoweredCall,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        match &call.target {
            LoweredCallableTarget::DirectFunction {
                function,
                environment,
            } => {
                let target = match environment {
                    LoweredCallEnvironment::Current => current_target(enclosing),
                    LoweredCallEnvironment::None => nested_target(enclosing),
                };
                self.request_function(
                    *function,
                    &call.origin,
                    call.function_type.clone(),
                    call.substitutions.clone(),
                    call.evidence.clone(),
                    target,
                    owner,
                    LoweredInstanceDependencyKind::DirectCall,
                );
            }
            LoweredCallableTarget::TraitImplementation {
                trait_id, method, ..
            }
            | LoweredCallableTarget::StructuralTraitMethod {
                trait_id, method, ..
            } => {
                let Some(evidence) = &call.evidence else {
                    self.diagnostics.push(Diagnostic::new(
                        call.origin.span.clone(),
                        "trait call has no evidence recipe",
                    ));
                    return;
                };
                self.handle_trait_site(
                    &call.origin,
                    *trait_id,
                    *method,
                    evidence,
                    Some(call.function_type.clone()),
                    call.substitutions.clone(),
                    enclosing,
                    owner,
                );
            }
            LoweredCallableTarget::CompilerHelper { function } => {
                self.request_helper(*function, &call.origin, owner);
            }
            LoweredCallableTarget::IndirectClosure { .. }
            | LoweredCallableTarget::ExternalFunction { .. }
            | LoweredCallableTarget::Intrinsic { .. }
            | LoweredCallableTarget::Constructor { .. } => {}
        }
    }

    fn traverse_callable_value(
        &mut self,
        value_id: LoweredCallableValueId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.callable_values.insert(value_id) {
            return;
        }
        let program = self.program;
        let Some(value) = program.callable_values.get(value_id) else {
            return;
        };
        match &value.target {
            LoweredCallableTarget::DirectFunction {
                function,
                environment,
            } => {
                let current = *environment == LoweredCallEnvironment::Current
                    || value.closure.as_ref().is_some_and(|closure| {
                        closure.environment == LoweredClosureEnvironment::Current
                    });
                let target = if current {
                    current_target(enclosing)
                } else {
                    nested_target(enclosing)
                };
                self.request_function(
                    *function,
                    &value.origin,
                    value.function_type.clone(),
                    value.substitutions.clone(),
                    value.evidence.clone(),
                    target,
                    owner,
                    LoweredInstanceDependencyKind::CallableValue,
                );
            }
            LoweredCallableTarget::TraitImplementation {
                trait_id, method, ..
            }
            | LoweredCallableTarget::StructuralTraitMethod {
                trait_id, method, ..
            } => {
                let Some(evidence) = &value.evidence else {
                    self.diagnostics.push(Diagnostic::new(
                        value.origin.span.clone(),
                        "trait-method value has no evidence recipe",
                    ));
                    return;
                };
                self.handle_trait_site(
                    &value.origin,
                    *trait_id,
                    *method,
                    evidence,
                    Some(value.function_type.clone()),
                    value.substitutions.clone(),
                    enclosing,
                    owner,
                );
            }
            LoweredCallableTarget::Constructor {
                symbol, type_id, ..
            } => {
                self.request_constructor_adapter(
                    *symbol,
                    *type_id,
                    value.adapter,
                    &value.function_type,
                    &value.origin,
                    enclosing,
                    owner,
                );
            }
            LoweredCallableTarget::CompilerHelper { function } => {
                self.request_helper(*function, &value.origin, owner);
            }
            LoweredCallableTarget::IndirectClosure { .. }
            | LoweredCallableTarget::ExternalFunction { .. }
            | LoweredCallableTarget::Intrinsic { .. } => {}
        }
    }

    fn request_constructor_adapter(
        &mut self,
        symbol: crate::SymbolId,
        type_id: crate::TypeId,
        adapter: LoweredCallableAdapter,
        callable_type: &CheckedFunctionType,
        origin: &Origin,
        enclosing: Option<&ResolvedInstanceRequest>,
        owner: TraversalOwner,
    ) {
        let environment = enclosing.map(|enclosing| &enclosing.environment);
        let concrete = match concretize_function_type(callable_type, environment, origin) {
            Ok(concrete) => concrete,
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                return;
            }
        };
        match ConstructorAdapterKey::new(symbol, type_id, adapter, &concrete, origin) {
            Ok(key) => self.request_artifact(
                ArtifactRequestKey::ConstructorAdapter(key),
                origin,
                owner,
                LoweredArtifactDependencyKind::ConstructorAdapter,
            ),
            Err(diagnostic) => self.diagnostics.push(diagnostic),
        }
    }

    /// Resolves one trait-dependent site and records either the selected
    /// method function's instance or a typed structural-method artifact. The
    /// recipe is resolved under the enclosing instance's environment first,
    /// because a declared bound's selected function is only known after
    /// substitution.
    #[allow(clippy::too_many_arguments)]
    fn handle_trait_site(
        &mut self,
        origin: &Origin,
        trait_id: TraitId,
        method: TraitMethodId,
        evidence: &TraitEvidence,
        recorded_type: Option<CheckedFunctionType>,
        substitutions: CallSubstitutions,
        enclosing: Option<&ResolvedInstanceRequest>,
        owner: TraversalOwner,
    ) {
        let program = self.program;
        let site_environment = match program.site_environment(
            origin,
            &substitutions,
            enclosing.map(|enclosing| &enclosing.environment),
        ) {
            Ok(environment) => environment,
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                return;
            }
        };
        let resolved =
            match program.resolve_trait_evidence(origin, Some(evidence), &site_environment) {
                Ok(resolved) => resolved,
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    return;
                }
            };
        let Some(resolved) = resolved else {
            return;
        };
        match &resolved {
            TraitEvidence::ExplicitImplementation {
                function,
                arguments,
                ..
            } => {
                let function_type = match recorded_type.clone() {
                    Some(function_type) => function_type,
                    None => {
                        match instantiate_method_type(program, origin, trait_id, method, arguments)
                        {
                            Ok(function_type) => function_type,
                            Err(diagnostic) => {
                                self.diagnostics.push(diagnostic);
                                return;
                            }
                        }
                    }
                };
                let evidence = if program.relevant_parameters(*function).is_empty() {
                    None
                } else {
                    Some(resolved.clone())
                };
                self.request_function(
                    *function,
                    origin,
                    function_type,
                    substitutions,
                    evidence,
                    nested_target(enclosing),
                    owner,
                    LoweredInstanceDependencyKind::TraitMethod,
                );
            }
            TraitEvidence::Structural {
                trait_id,
                method,
                structural,
                arguments,
            } => {
                let callable_type = match recorded_type.clone() {
                    Some(function_type) => {
                        match concretize_function_type(
                            &function_type,
                            Some(&site_environment),
                            origin,
                        ) {
                            Ok(function_type) => function_type,
                            Err(diagnostic) => {
                                self.diagnostics.push(diagnostic);
                                return;
                            }
                        }
                    }
                    None => {
                        match instantiate_method_type(
                            program, origin, *trait_id, *method, arguments,
                        ) {
                            Ok(function_type) => function_type,
                            Err(diagnostic) => {
                                self.diagnostics.push(diagnostic);
                                return;
                            }
                        }
                    }
                };
                match StructuralMethodKey::new(
                    *structural,
                    *trait_id,
                    *method,
                    arguments,
                    &callable_type,
                    origin,
                ) {
                    Ok(key) => self.request_artifact(
                        ArtifactRequestKey::StructuralMethod(key),
                        origin,
                        owner,
                        LoweredArtifactDependencyKind::StructuralMethod,
                    ),
                    Err(diagnostic) => self.diagnostics.push(diagnostic),
                }
            }
            TraitEvidence::DeclaredBound { .. } | TraitEvidence::RejectedImplementation { .. } => {
                self.diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    "trait evidence did not resolve to a concrete selection",
                ));
            }
        }
    }
}

/// Instantiates a trait method's declared function type from the owned
/// catalogs, mirroring `TypedModule::instantiated_trait_method_type`.
pub(super) fn instantiate_method_type(
    program: &LoweredProgram,
    origin: &Origin,
    trait_id: TraitId,
    method: TraitMethodId,
    arguments: &[CheckedType],
) -> Result<CheckedFunctionType, Diagnostic> {
    let Some(trait_metadata) = program.traits.get(trait_id) else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            format!("trait {} is missing from the lowered catalog", trait_id.0),
        ));
    };
    let Some(method_metadata) = program.trait_methods.get(method) else {
        return Err(Diagnostic::new(
            origin.span.clone(),
            format!(
                "trait method {} is missing from the lowered catalog",
                method.0
            ),
        ));
    };
    if trait_metadata.parameters.len() != arguments.len() {
        return Err(Diagnostic::new(
            origin.span.clone(),
            format!(
                "trait {} has {} parameters for {} completed arguments",
                trait_metadata.name,
                trait_metadata.parameters.len(),
                arguments.len()
            ),
        ));
    }
    let mut inferred = HashMap::new();
    for (parameter, argument) in trait_metadata.parameters.iter().zip(arguments) {
        if !infer_type_parameters(parameter, argument, &mut inferred) {
            return Err(Diagnostic::new(
                origin.span.clone(),
                format!(
                    "cannot instantiate method `{}` of trait `{}` for argument `{argument}`",
                    method_metadata.name, trait_metadata.name
                ),
            ));
        }
    }
    match substitute_type(method_metadata.value_type.clone(), &inferred) {
        CheckedType::Function(function_type) => Ok(function_type),
        other => Err(Diagnostic::new(
            origin.span.clone(),
            format!(
                "instantiated method `{}` of trait `{}` is not a function: {other}",
                method_metadata.name, trait_metadata.name
            ),
        )),
    }
}

impl<'a> WorklistBuilder<'a> {
    fn traverse_reactive_operation(
        &mut self,
        operation: LoweredReactiveOperationId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.operations.insert(operation) {
            return;
        }
        let program = self.program;
        let Some(operation) = program.reactive_operations.get(operation) else {
            return;
        };
        match &operation.kind {
            LoweredReactiveOperationKind::DerivedCreate {
                evaluator,
                function_type,
                ..
            } => {
                self.request_function(
                    *evaluator,
                    &operation.origin,
                    function_type.clone(),
                    CallSubstitutions::default(),
                    None,
                    nested_target(enclosing),
                    owner,
                    LoweredInstanceDependencyKind::DerivedEvaluator,
                );
            }
            LoweredReactiveOperationKind::Reaction { callback, .. }
            | LoweredReactiveOperationKind::Until {
                predicate: callback,
                ..
            }
            | LoweredReactiveOperationKind::Batch { callback } => {
                self.traverse_reactive_callback(*callback, owner, enclosing);
            }
            LoweredReactiveOperationKind::SignalCreate { .. }
            | LoweredReactiveOperationKind::SignalRead { .. }
            | LoweredReactiveOperationKind::SignalNotify { .. }
            | LoweredReactiveOperationKind::DerivedRead { .. }
            | LoweredReactiveOperationKind::Scope
            | LoweredReactiveOperationKind::Snapshot => {}
        }
    }

    fn traverse_reactive_callback(
        &mut self,
        callback: LoweredReactiveCallbackId,
        owner: TraversalOwner,
        enclosing: Option<&ResolvedInstanceRequest>,
    ) {
        if !self.visited.callbacks.insert(callback) {
            return;
        }
        let program = self.program;
        let Some(callback) = program.reactive_callbacks.get(callback) else {
            return;
        };
        if let Some(thunk) = callback.thunk {
            self.request_function(
                thunk,
                &callback.origin,
                callback.function_type.clone(),
                CallSubstitutions::default(),
                None,
                nested_target(enclosing),
                owner,
                LoweredInstanceDependencyKind::ReactiveCallback,
            );
        }
        if let Some(callable) = callback.callable {
            self.traverse_expression(callable, owner, enclosing);
        }
    }
}

fn nested_target(enclosing: Option<&ResolvedInstanceRequest>) -> InstanceResolutionTarget<'_> {
    match enclosing {
        Some(enclosing) => InstanceResolutionTarget::Nested(enclosing),
        None => InstanceResolutionTarget::Root,
    }
}

/// A `Current` environment needs its enclosing instance; a request without one
/// is treated as a root request, which the resolver still validates.
fn current_target(enclosing: Option<&ResolvedInstanceRequest>) -> InstanceResolutionTarget<'_> {
    match enclosing {
        Some(enclosing) => InstanceResolutionTarget::Current(enclosing),
        None => InstanceResolutionTarget::Root,
    }
}

fn evidence_trait_id(evidence: &TraitEvidence) -> TraitId {
    match evidence {
        TraitEvidence::ExplicitImplementation { trait_id, .. }
        | TraitEvidence::Structural { trait_id, .. }
        | TraitEvidence::DeclaredBound { trait_id, .. }
        | TraitEvidence::RejectedImplementation { trait_id, .. } => *trait_id,
    }
}

/// Builds the site recipe for an evidence-only site from the trait's declared
/// parameters and the site's completed arguments, matching the recipe lowered
/// calls already record.
pub(super) fn trait_site_substitutions(
    program: &LoweredProgram,
    trait_id: TraitId,
    arguments: &[CheckedType],
) -> CallSubstitutions {
    let parameters = program
        .traits
        .get(trait_id)
        .map(|trait_| trait_.parameters.clone())
        .unwrap_or_default();
    super::trait_call_substitutions(&parameters, arguments)
}

/// Substitutes a recorded method type through the enclosing or site
/// environment. Leftover declared parameters are reported by the artifact key
/// constructor at `origin`.
pub(super) fn concretize_function_type(
    function_type: &CheckedFunctionType,
    environment: Option<&SubstitutionEnvironment>,
    origin: &Origin,
) -> Result<CheckedFunctionType, Diagnostic> {
    let mut value_type = CheckedType::Function(function_type.clone());
    if let Some(environment) = environment {
        let map = environment.substitution_map();
        value_type = substitute_type(value_type, &map);
    }
    match value_type {
        CheckedType::Function(function_type) => Ok(function_type),
        other => Err(Diagnostic::new(
            origin.span.clone(),
            format!("substituted callable type is not a function: {other}"),
        )),
    }
}

impl LoweredProgram {
    /// Builds and installs the Stage 3.3 worklist. Every failure is a source
    /// diagnostic at the requesting site; a partially built graph is
    /// discarded.
    pub(super) fn build_specialization_worklist(&mut self) -> Vec<Diagnostic> {
        match build(self) {
            Ok(parts) => {
                self.instances = parts.instances;
                self.artifacts = parts.artifacts;
                self.helper_requests = parts.helper_requests;
                self.specializations = parts.catalog;
                Vec::new()
            }
            Err(diagnostics) => diagnostics,
        }
    }

    /// Validates the installed worklist against its own catalog: dense
    /// ordinals, catalog agreement, planned names, and in-range edges.
    pub(super) fn validate_specializations(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let catalog_instances = self.specializations.instances().count();
        if catalog_instances != self.instances.len() {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "specialization catalog holds {catalog_instances} instances for {} interned instances",
                    self.instances.len()
                ),
            ));
        }
        let catalog_artifacts = self.specializations.artifacts().count();
        if catalog_artifacts != self.artifacts.len() {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "specialization catalog holds {catalog_artifacts} artifacts for {} interned artifacts",
                    self.artifacts.len()
                ),
            ));
        }
        let names = match self.specializations.planned_names() {
            Ok(names) => names,
            Err(SpecializationNameCollision { name }) => {
                diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!("specialization name collision: `{name}`"),
                ));
                Vec::new()
            }
        };
        for (id, instance) in self.instances.iter() {
            if instance.ordinal.index() != id.index() {
                diagnostics.push(Diagnostic::new(
                    instance.origin.span.clone(),
                    format!(
                        "function instance {} is stored at ordinal {}",
                        id.index(),
                        instance.ordinal.index()
                    ),
                ));
            }
            match self.specializations.instance(instance.ordinal) {
                Some(key) if key.function() == instance.template => {}
                _ => diagnostics.push(Diagnostic::new(
                    instance.origin.span.clone(),
                    format!(
                        "function instance {} does not agree with its specialization key",
                        id.index()
                    ),
                )),
            }
            if names.get(id.index()) != Some(&instance.name) {
                diagnostics.push(Diagnostic::new(
                    instance.origin.span.clone(),
                    format!("function instance {} has an unstable name", id.index()),
                ));
            }
            for dependency in &instance.dependencies {
                if !self.instances.contains(dependency.instance) {
                    diagnostics.push(Diagnostic::new(
                        dependency.origin.span.clone(),
                        format!(
                            "function instance {} depends on missing instance {}",
                            id.index(),
                            dependency.instance.index()
                        ),
                    ));
                }
            }
            for artifact in &instance.artifacts {
                if artifact.artifact.index() >= self.artifacts.len() {
                    diagnostics.push(Diagnostic::new(
                        artifact.origin.span.clone(),
                        format!(
                            "function instance {} references missing artifact {}",
                            id.index(),
                            artifact.artifact.index()
                        ),
                    ));
                }
            }
        }
        for (id, artifact) in self.artifacts.iter() {
            if artifact.ordinal.index() != id.index() {
                diagnostics.push(Diagnostic::new(
                    artifact.origin.span.clone(),
                    format!(
                        "artifact {} is stored at ordinal {}",
                        id.index(),
                        artifact.ordinal.index()
                    ),
                ));
            }
            if !matches!(
                self.specializations.artifact(artifact.ordinal),
                Some(ArtifactRequestKey::ConstructorAdapter(_))
                    | Some(ArtifactRequestKey::StructuralMethod(_))
            ) {
                diagnostics.push(Diagnostic::new(
                    artifact.origin.span.clone(),
                    format!(
                        "artifact {} does not agree with its specialization key",
                        id.index()
                    ),
                ));
            }
            let name_index = self.instances.len() + id.index();
            if names.get(name_index) != Some(&artifact.name) {
                diagnostics.push(Diagnostic::new(
                    artifact.origin.span.clone(),
                    format!("artifact {} has an unstable name", id.index()),
                ));
            }
        }
        for request in &self.helper_requests {
            if self.functions.get(request.function).is_none() {
                diagnostics.push(Diagnostic::new(
                    request.origin.span.clone(),
                    format!(
                        "compiler-helper request names missing function {}",
                        request.function.0
                    ),
                ));
            }
        }
        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{
        NameResolver, ProgramLoader, SubstitutionValue, TypeChecker, TypeParameterId, TypedModule,
    };

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

    fn lower(source: &str) -> (TypedModule, LoweredProgram) {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());
        let diagnostics = program.build_specialization_worklist();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_specializations();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        (module, program)
    }

    fn function_id(program: &LoweredProgram, name: &str) -> FunctionId {
        program
            .functions
            .iter()
            .find(|(_, _, function)| {
                function.name == name || function.name.ends_with(&format!(".{name}"))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"))
    }

    fn instances_of<'a>(
        program: &'a LoweredProgram,
        function: FunctionId,
    ) -> Vec<&'a LoweredFunctionInstance> {
        program
            .instances
            .iter()
            .filter(|(_, instance)| instance.template == function)
            .map(|(_, instance)| instance)
            .collect()
    }

    fn first_type_parameter(instance: &LoweredFunctionInstance) -> TypeParameterId {
        instance
            .relevant
            .type_parameters()
            .next()
            .expect("fixture should have one relevant type parameter")
    }

    fn graph_snapshot(program: &LoweredProgram) -> String {
        let mut out = String::new();
        for (id, instance) in program.instances.iter() {
            let name = program
                .functions
                .get(instance.template)
                .map(|function| function.name.as_str())
                .unwrap_or("<missing>");
            out.push_str(&format!(
                "instance {} {} template={name}\n",
                id.index(),
                instance.name
            ));
            for (parameter, entry) in instance.environment.iter() {
                out.push_str(&format!("  env {} = {:?}\n", parameter.0, entry.value));
            }
            out.push_str(&format!("  request {:?}\n", instance.request));
            for dependency in &instance.dependencies {
                out.push_str(&format!(
                    "  dependency {} {} {:?}\n",
                    dependency.instance.index(),
                    dependency.kind.description(),
                    dependency.origin.span
                ));
            }
            for artifact in &instance.artifacts {
                out.push_str(&format!(
                    "  artifact {} {:?}\n",
                    artifact.artifact.index(),
                    artifact.kind
                ));
            }
        }
        for (id, artifact) in program.artifacts.iter() {
            out.push_str(&format!("artifact {} {}\n", id.index(), artifact.name));
        }
        out
    }

    #[test]
    fn eager_roots_match_the_legacy_backend_set() {
        let (module, program) = lower(concat!(
            "def plain: I32 -> I32 = value => value\n",
            "def unused: <T where Copy T> T -> T = value => value\n",
            "let value: I32 = 1\n",
        ));
        let expected = module
            .functions()
            .iter()
            .chain(module.implicit_thunks())
            .filter(|function| {
                !contains_type_parameter(&CheckedType::Function(
                    module
                        .type_of_function(function.id)
                        .expect("checked function")
                        .clone(),
                ))
            })
            .filter(|function| !program.coroutine_plan_by_thunk.contains_key(&function.id))
            .map(|function| function.id)
            .collect::<HashSet<_>>();
        let seeded = program
            .instances
            .iter()
            .filter(|(_, instance)| {
                matches!(instance.request, LoweredInstanceRequest::EagerTemplate)
            })
            .map(|(_, instance)| instance.template)
            .collect::<HashSet<_>>();
        assert!(
            seeded.is_subset(&expected),
            "every eagerly seeded instance is a template the backend emits eagerly"
        );
        let interned = program
            .instances
            .iter()
            .map(|(_, instance)| instance.template)
            .collect::<HashSet<_>>();
        assert!(
            expected.is_subset(&interned),
            "every backend eager template has an instance"
        );
        assert!(
            instances_of(&program, function_id(&program, "unused")).is_empty(),
            "an unused generic template is never seeded"
        );
    }

    #[test]
    fn concrete_signature_closures_with_enclosing_parameters_stay_demand_driven() {
        let (_, program) = lower(concat!(
            "def show_thunk: <T where Copy T, Display T> move T -> (() -> String) = move value => () => \"value=$value\"\n",
            "let shown = show_thunk 1\n",
        ));
        assert!(
            program.instances.iter().all(|(_, instance)| {
                instance.relevant.is_empty()
                    || !matches!(instance.request, LoweredInstanceRequest::EagerTemplate)
            }),
            "a template with relevant parameters is never seeded as a root"
        );
        let nested = program
            .instances
            .iter()
            .find(|(_, instance)| {
                !instance.relevant.is_empty()
                    && program
                        .functions
                        .get(instance.template)
                        .is_some_and(|function| {
                            !contains_type_parameter(&CheckedType::Function(
                                function.signature.clone(),
                            ))
                        })
            })
            .map(|(_, instance)| instance)
            .expect("the concrete-signature nested closure is still discovered");
        assert!(matches!(
            nested.request,
            LoweredInstanceRequest::Dependency {
                kind: LoweredInstanceDependencyKind::CallableValue,
                ..
            }
        ));
    }

    #[test]
    fn repeated_uses_deduplicate_and_separate_substitutions() {
        let (_, program) = lower(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "let second: I32 = identity 1\n",
            "let third: U8 = identity (1 satisfies U8)\n",
        ));
        let identity = function_id(&program, "identity");
        let instances = instances_of(&program, identity);
        assert_eq!(instances.len(), 2, "one instance per distinct substitution");
        let mut values = instances
            .iter()
            .map(|instance| {
                let parameter = first_type_parameter(instance);
                instance
                    .environment
                    .type_value(parameter)
                    .cloned()
                    .expect("the relevant parameter is concrete")
            })
            .collect::<Vec<_>>();
        values.sort_by_key(|value| format!("{value:?}"));
        assert_eq!(values, vec![CheckedType::I32, CheckedType::U8]);
        assert!(
            instances.iter().all(|instance| matches!(
                instance.request,
                LoweredInstanceRequest::Initializer { .. }
            )),
            "initializer-discovered instances record the requesting initializer"
        );
    }

    #[test]
    fn call_arguments_discover_instances_before_the_invoked_target() {
        let (_, program) = lower(concat!(
            "def outer: <T where Copy T> T -> T = value => value\n",
            "def inner: <T where Copy T> T -> T = value => value\n",
            "let result: I32 = outer (inner 1)\n",
        ));
        let outer = instances_of(&program, function_id(&program, "outer"))[0];
        let inner = instances_of(&program, function_id(&program, "inner"))[0];
        assert!(inner.ordinal.index() < outer.ordinal.index());
    }

    #[test]
    fn shared_default_lookup_unifies_all_header_arguments_together() {
        let mut program = LoweredProgram::default();
        let trait_id = TraitId(0);
        let method = TraitMethodId(0);
        let function = FunctionId(0);
        let parameter_id = TypeParameterId(0);
        let parameter = CheckedType::Parameter {
            id: parameter_id,
            name: "T".to_owned(),
            sized: true,
        };
        program
            .trait_implementations
            .push(super::super::LoweredTraitImplementationMetadata {
                origin: Origin::compiler(),
                trait_id,
                parameters: vec![parameter_id],
                arguments: vec![parameter.clone(), parameter],
                bounds: Vec::new(),
                negative: false,
                methods: vec![(method, function)],
            });
        let concrete =
            program
                .trait_implementations
                .push(super::super::LoweredTraitImplementationMetadata {
                    origin: Origin::compiler(),
                    trait_id,
                    parameters: Vec::new(),
                    arguments: vec![CheckedType::I32, CheckedType::U8],
                    bounds: Vec::new(),
                    negative: false,
                    methods: vec![(method, function)],
                });
        assert_eq!(
            program.trait_implementation_id(
                trait_id,
                method,
                function,
                &[CheckedType::I32, CheckedType::U8],
            ),
            Some(concrete)
        );
    }

    #[test]
    fn string_templates_record_formatter_calls() {
        let (_, program) = lower(concat!(
            "def render: <T where Display T> move T -> String = move value => \"value=$value\"\n",
            "let result: String = render 1\n",
        ));
        let render = instances_of(&program, function_id(&program, "render"))[0];
        for (function, kind) in [
            (
                program.string_formatting.constructor.unwrap(),
                LoweredInstanceDependencyKind::FormattingConstructor,
            ),
            (
                program.string_formatting.finish.unwrap(),
                LoweredInstanceDependencyKind::FormattingFinish,
            ),
        ] {
            assert!(render.dependencies.iter().any(|dependency| {
                dependency.kind == kind
                    && program
                        .instances
                        .get(dependency.instance)
                        .is_some_and(|instance| instance.template == function)
            }));
        }
    }

    #[test]
    fn generic_local_function_templates_do_not_leak_parameters() {
        let (_, program) = lower(concat!(
            "def outer: () -> I32 = () => {\n",
            "  def recur: <T> T -> T = value => recur value\n",
            "  recur 1\n",
            "}\n",
        ));
        let outer = function_id(&program, "outer");
        let instance = instances_of(&program, outer)
            .into_iter()
            .next()
            .expect("outer is an eager root");
        assert!(
            instance.relevant.is_empty(),
            "a compile-time local generic template cannot make a parameter relevant"
        );
        assert!(instance.environment.is_empty());
    }

    #[test]
    fn nested_generic_calls_discover_their_dependencies() {
        let (_, program) = lower(concat!(
            "def inner: <T where Copy T> T -> T = value => value\n",
            "def outer: <T where Copy T> T -> T = value => inner value\n",
            "let applied: I32 = outer 1\n",
        ));
        let inner = function_id(&program, "inner");
        let outer = function_id(&program, "outer");
        let outer_instance = instances_of(&program, outer)
            .into_iter()
            .next()
            .expect("outer instance");
        assert_eq!(
            outer_instance
                .environment
                .type_value(first_type_parameter(outer_instance)),
            Some(&CheckedType::I32)
        );
        let dependency = outer_instance
            .dependencies
            .iter()
            .find(|dependency| {
                program
                    .instances
                    .get(dependency.instance)
                    .is_some_and(|instance| instance.template == inner)
            })
            .expect("outer depends on inner");
        assert_eq!(dependency.kind, LoweredInstanceDependencyKind::DirectCall);
        let inner_instance = program
            .instances
            .get(dependency.instance)
            .expect("inner instance");
        assert_eq!(
            inner_instance
                .environment
                .type_value(first_type_parameter(inner_instance)),
            Some(&CheckedType::I32),
            "the nested call propagates the enclosing substitution"
        );
    }

    #[test]
    fn recursion_terminates_on_the_interned_instance() {
        let (_, program) = lower(concat!(
            "def recursive: <T where Copy T> T -> T = value => recursive value\n",
            "let result: I32 = recursive 1\n",
        ));
        let recursive = function_id(&program, "recursive");
        let instances = instances_of(&program, recursive);
        assert_eq!(
            instances.len(),
            1,
            "same-key recursion interns one instance"
        );
        let id = program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == recursive)
            .map(|(id, _)| id)
            .expect("recursive instance");
        assert!(
            instances[0]
                .dependencies
                .iter()
                .any(|dependency| dependency.instance == id
                    && dependency.kind == LoweredInstanceDependencyKind::DirectCall),
            "the recursive call keeps a self dependency"
        );

        let (_, program) = lower(concat!(
            "def ping: <T where Copy T> T -> T = value => pong value\n",
            "def pong: <T where Copy T> T -> T = value => ping value\n",
            "let result: I32 = ping 1\n",
        ));
        let ping = function_id(&program, "ping");
        let pong = function_id(&program, "pong");
        assert_eq!(instances_of(&program, ping).len(), 1);
        assert_eq!(instances_of(&program, pong).len(), 1);
        let ping_instance = instances_of(&program, ping).into_iter().next().unwrap();
        let pong_instance = instances_of(&program, pong).into_iter().next().unwrap();
        assert!(ping_instance.dependencies.iter().any(|dependency| {
            program
                .instances
                .get(dependency.instance)
                .is_some_and(|instance| instance.template == pong)
        }));
        assert!(pong_instance.dependencies.iter().any(|dependency| {
            program
                .instances
                .get(dependency.instance)
                .is_some_and(|instance| instance.template == ping)
        }));
    }

    #[test]
    fn closure_captures_discover_parameterized_thunks() {
        let (_, program) = lower(concat!(
            "def maker: <T where Copy T> T -> () -> T = value => () => value\n",
            "let made_i32 = maker 1\n",
            "let made_string = maker \"x\"\n",
        ));
        let maker = function_id(&program, "maker");
        for maker_instance in instances_of(&program, maker) {
            let dependency = maker_instance
                .dependencies
                .iter()
                .find(|dependency| dependency.kind == LoweredInstanceDependencyKind::CallableValue)
                .expect("the closure construction is a dependency");
            let closure = program
                .instances
                .get(dependency.instance)
                .expect("closure instance");
            assert!(
                !closure.environment.is_empty(),
                "the captured outer substitution enters the closure instance"
            );
        }
    }

    #[test]
    fn declared_bound_calls_select_concrete_methods() {
        let (module, program) = lower(concat!(
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def show_bound: <T where TestShow T> T -> Bool = value => test_show value\n",
            "let shown: Bool = show_bound 1\n",
        ));
        let show_bound = function_id(&program, "show_bound");
        let instance = instances_of(&program, show_bound)
            .into_iter()
            .next()
            .expect("show_bound instance");
        let dependency = instance
            .dependencies
            .iter()
            .find(|dependency| dependency.kind == LoweredInstanceDependencyKind::TraitMethod)
            .expect("the declared bound resolves to a method instance");
        let method = program
            .instances
            .get(dependency.instance)
            .expect("method instance");
        let trait_id = program
            .traits
            .iter()
            .find(|(_, _, metadata)| metadata.name == "TestShow")
            .map(|(_, id, _)| id)
            .expect("TestShow trait");
        let method_id = program
            .traits
            .get(trait_id)
            .expect("trait metadata")
            .methods[0];
        let expected = module
            .trait_impl_method(trait_id, &[CheckedType::I32], method_id)
            .expect("the checker selects the same method");
        assert_eq!(method.template, expected);
        assert!(
            method.environment.is_empty(),
            "a concrete implementation method has no template substitutions"
        );
    }

    #[test]
    fn shared_default_methods_pick_the_matching_implementation() {
        let (_, program) = lower(concat!(
            "trait TestShow T {\n",
            "  test_show: T -> Bool\n",
            "  test_other: T -> Bool = value => test_show value\n",
            "}\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "impl TestShow U8 { def test_show = _ => True }\n",
            "def use_other: <T where TestShow T> T -> Bool = value => test_other value\n",
            "let applied: Bool = use_other 1\n",
            "let other: Bool = use_other (1 satisfies U8)\n",
        ));
        let use_other = function_id(&program, "use_other");
        assert_eq!(instances_of(&program, use_other).len(), 2);
        let default = program
            .functions
            .iter()
            .find(|(_, _, function)| function.name.ends_with(".test_other"))
            .map(|(_, id, _)| id)
            .or_else(|| {
                program
                    .functions
                    .iter()
                    .find(|(_, _, function)| function.name.ends_with("test_other"))
                    .map(|(_, id, _)| id)
            });
        if let Some(default) = default {
            let instances = instances_of(&program, default);
            assert!(
                instances.len() <= 2,
                "the shared default method keeps one instance per substitution"
            );
            for instance in instances {
                assert!(
                    instance.dependencies.iter().any(|dependency| {
                        dependency.kind == LoweredInstanceDependencyKind::TraitMethod
                    }),
                    "the default body's declared bound resolves under its own substitution"
                );
                if !instance.relevant.is_empty() {
                    assert!(
                        instance.environment.iter().any(|(_, entry)| {
                            entry.value == SubstitutionValue::Type(CheckedType::I32)
                                || entry.value == SubstitutionValue::Type(CheckedType::U8)
                        }),
                        "a generic shared default keeps its concrete substitution"
                    );
                }
            }
        }
    }

    #[test]
    fn structural_methods_and_constructor_adapters_are_recorded() {
        let (_, program) = lower(concat!(
            "type Point = ctor (I32, I32)\n",
            "let make: () -> ((I32, I32) -> Point) = () => Point\n",
            "let p = (1, 2)\n",
            "let text = \"${p:?}\"\n",
        ));
        assert!(
            program.artifacts.iter().any(|(_, artifact)| matches!(
                program.specializations.artifact(artifact.ordinal),
                Some(ArtifactRequestKey::ConstructorAdapter(_))
            )),
            "a constructor value records a constructor-adapter artifact"
        );
        assert!(
            program.artifacts.iter().any(|(_, artifact)| matches!(
                program.specializations.artifact(artifact.ordinal),
                Some(ArtifactRequestKey::StructuralMethod(key))
                    if key.structural == crate::StructuralTraitMethod::Debug
            )),
            "a structural debug interpolation records a structural artifact"
        );
    }

    #[test]
    fn coroutine_body_thunks_stay_demand_driven() {
        let (_, program) = lower(concat!(
            "use std.coroutine.*\n",
            "def task: () -> Coroutine{} I32 = () => coro { 1 }\n",
            "def unused_task: <T where Copy T> T -> Coroutine{} T = value => coro { value }\n",
            "let created = task ()\n",
        ));
        let coroutine_instances = program
            .instances
            .iter()
            .filter(|(_, instance)| {
                program
                    .functions
                    .get(instance.template)
                    .is_some_and(|function| function.class.coroutine_body)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            coroutine_instances.len(),
            1,
            "only the reachable coroutine body thunk becomes an instance"
        );
        let (_, instance) = coroutine_instances[0];
        let dependency = program
            .instances
            .iter()
            .find(|(_, owner)| {
                owner
                    .dependencies
                    .iter()
                    .any(|dependency| dependency.instance.index() == instance.ordinal.index())
            })
            .map(|(_, owner)| owner)
            .expect("the creation site depends on the body thunk");
        assert!(
            dependency.dependencies.iter().any(|dependency| {
                dependency.kind == LoweredInstanceDependencyKind::CoroutineBody
            })
        );
    }

    #[test]
    fn reactive_derived_and_callback_thunks_are_discovered() {
        let (_, program) = lower(concat!(
            "let signal count = 0\n",
            "let doubled = count + count\n",
            "reaction { () }\n",
        ));
        let evaluator = program
            .instances
            .iter()
            .find(|(_, instance)| {
                program
                    .functions
                    .get(instance.template)
                    .is_some_and(|function| function.class.derived_evaluator)
            })
            .map(|(_, instance)| instance)
            .expect("the derived evaluator becomes an instance");
        assert!(matches!(
            evaluator.request,
            LoweredInstanceRequest::Initializer {
                kind: LoweredInstanceDependencyKind::DerivedEvaluator,
                ..
            }
        ));

        let callback = program
            .instances
            .iter()
            .find(|(_, instance)| {
                matches!(
                    instance.request,
                    LoweredInstanceRequest::Initializer {
                        kind: LoweredInstanceDependencyKind::ReactiveCallback
                            | LoweredInstanceDependencyKind::ImplicitThunkArgument,
                        ..
                    }
                )
            })
            .map(|(_, instance)| instance);
        assert!(
            callback.is_some(),
            "the reaction callback thunk becomes an instance"
        );
    }

    #[test]
    fn repeated_lowering_yields_the_same_instance_graph() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def constant: <T where Copy T> T -> I32 -> T = value => ignored => value\n",
            "let first: I32 = identity 1\n",
            "let second: U8 = identity (1 satisfies U8)\n",
            "let held = constant \"x\"\n",
        );
        let (_, first) = lower(source);
        let (_, second) = lower(source);
        assert_eq!(graph_snapshot(&first), graph_snapshot(&second));

        let mut rebuilt = LoweredProgram::default();
        let module = checked_program(source);
        assert!(rebuilt.snapshot(&module).is_empty());
        let diagnostics = rebuilt.build_specialization_worklist();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let baseline = graph_snapshot(&rebuilt);
        let diagnostics = rebuilt.build_specialization_worklist();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(baseline, graph_snapshot(&rebuilt));
    }
}
