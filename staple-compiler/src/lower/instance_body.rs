//! Stage 3.4: materialize concrete instance bodies.
//!
//! Every reachable `LoweredFunctionInstance` gets one immutable, instance-owned
//! body. Bodies are cloned from the Stage 2 template arenas through a
//! per-family old-to-new map and substituted with the Stage 3.2 environment, so
//! no body lookup needs the generic template or a `TypedModule` side table for
//! type arguments, call targets, or trait selection. Dispatch sites are bound
//! to the Stage 3.3 graph identities recorded in
//! `LoweredFunctionInstance::{dependencies, artifacts}`.
//!
//! Bodies are materialized in Stage 3.3 ordinal order. Instance-local ID types
//! stay distinct from template IDs by living in these per-instance arenas:
//! a body reference can never resolve into the template or another instance.

use std::collections::{BTreeMap, HashMap, HashSet};

use staple_syntax::{Diagnostic, FunctionParameterStyle, Span};

use crate::specialization::{
    ArtifactOrdinal, ArtifactRequestKey, ConstructorAdapterKey, InstanceKey, StructuralMethodKey,
};
use crate::{
    CheckedCoercion, CheckedEffectSet, CheckedFunctionType, CheckedMutation, CheckedProductType,
    CheckedPropagation, CheckedResource, CheckedTraitBound, CheckedTraitDispatch, CheckedType,
    FunctionId, SymbolId, TypeId, TypeParameterId, contains_type_parameter, substitute_effect_set,
    substitute_type,
};

use super::instance_resolution::{
    InstanceResolutionRequest, InstanceResolutionTarget, ResolvedInstanceRequest,
};
use super::worklist::{
    LoweredArtifactDependencyKind, LoweredInstanceDependencyKind, concretize_function_type,
    instantiate_method_type, trait_site_substitutions,
};
use super::{
    Arena, ArenaId, BlockId, CallSubstitutions, ExpressionId, FunctionInstanceId, ItemId,
    LoweredArtifactUse, LoweredAwait, LoweredAwaitId, LoweredAwaitKind, LoweredBlock, LoweredCall,
    LoweredCallEnvironment, LoweredCallId, LoweredCallStep, LoweredCallableCategory,
    LoweredCallableTarget, LoweredCallableValue, LoweredCallableValueId, LoweredCapture,
    LoweredClosureCapture, LoweredClosureConstruction, LoweredClosureEnvironment, LoweredCoro,
    LoweredCoroId, LoweredCoroutinePlan, LoweredCoroutinePlanId, LoweredExpression,
    LoweredExpressionKind, LoweredInstanceUse, LoweredItem, LoweredItemKind, LoweredName,
    LoweredPattern, LoweredPatternKind, LoweredPlace, LoweredPlaceKind, LoweredProgram,
    LoweredReactiveCallbackId, LoweredReactiveOperationId, LoweredReactiveOperationKind,
    LoweredResourceProvider, LoweredResourceProviderId, LoweredResourceUse, LoweredResourceUseId,
    LoweredStringTemplatePart, LoweredWith, LoweredWithId, Origin, PatternId, PlaceId,
    TraitEvidence,
};

/// One parameter of an instance body: the template symbol plus its concrete
/// checked type, so no body consumer has to ask the global symbol catalog.
#[derive(Debug, Clone)]
pub(crate) struct LoweredInstanceParameter {
    pub symbol: SymbolId,
    pub value_type: CheckedType,
}

/// One capture of an instance body: the template capture record plus its
/// concrete checked type and the cleanup facts the closure finalizer mirrors.
#[derive(Debug, Clone)]
pub(crate) struct LoweredInstanceCapture {
    pub capture: LoweredCapture,
    pub value_type: CheckedType,
    /// The captured symbol requires initialization state.
    pub requires_initialization_state: bool,
    /// The captured symbol needs mutable storage.
    pub mutable_storage: bool,
    /// The captured symbol is a derived binding.
    pub derived: bool,
}

/// A dispatch or construction site inside one instance body. Sites are keys
/// into the per-body binding tables; IDs are instance-local.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LoweredBindingSite {
    /// A direct, trait-dispatched, external, intrinsic, constructor, or helper
    /// call target.
    Call(LoweredCallId),
    /// A first-class callable value's target or closure construction.
    CallableValue(LoweredCallableValueId),
    /// An implicit thunk adapted into one call argument.
    CallArgumentThunk {
        call: LoweredCallId,
        argument: usize,
    },
    /// An `Index` read's trait dispatch.
    Index(ExpressionId),
    /// A string-template interpolation's formatting dispatch.
    Interpolation { template: ExpressionId, part: usize },
    /// The formatter constructor selected for a string template.
    FormattingConstructor(ExpressionId),
    /// The formatter finish function selected for a string template.
    FormattingFinish(ExpressionId),
    /// The formatter write function a string template's literal parts use.
    FormattingWrite(ExpressionId),
    /// An indexed assignment's `MutateIndex` dispatch.
    IndexedAssignment(ItemId),
    /// A derived binding's evaluator thunk.
    DerivedEvaluator(LoweredReactiveOperationId),
    /// A reaction, `until`, or `batch` callback thunk.
    ReactiveCallback(LoweredReactiveCallbackId),
    /// A `coro` creation's body thunk.
    Coro(LoweredCoroId),
    /// An `await`'s child coroutine plan owner.
    AwaitChildPlan(LoweredAwaitId),
}

/// The concrete target bound at one site. Variants distinguish source-function
/// instances from generated artifacts and the non-instance routes that
/// intentionally stay indirect/external/intrinsic. A `CompilerHelper` site has
/// no binding: the graph rejects the category with a diagnostic.
#[derive(Debug, Clone)]
pub(crate) enum LoweredBoundTarget {
    /// A known source function interned as a concrete instance.
    Instance(FunctionInstanceId),
    /// A generated constructor-adapter or structural-method artifact.
    Artifact(ArtifactOrdinal),
    /// The site keeps its non-source route (indirect closure, external,
    /// intrinsic, or ordinary constructor call).
    Route(LoweredCallableCategory),
}

impl LoweredBoundTarget {
    /// The dependency-edge kind a source-function binding must agree with.
    pub(crate) fn instance_id(&self) -> Option<FunctionInstanceId> {
        match self {
            LoweredBoundTarget::Instance(id) => Some(*id),
            _ => None,
        }
    }

    /// The artifact ordinal a generated-artifact binding must agree with.
    pub(crate) fn artifact_ordinal(&self) -> Option<ArtifactOrdinal> {
        match self {
            LoweredBoundTarget::Artifact(ordinal) => Some(*ordinal),
            _ => None,
        }
    }
}

/// How the backend tracks one owned binding for scope-exit cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnedStorage {
    /// An SSA local value with a live flag.
    Value,
    /// A binding cell dropped conditionally on its cell state.
    Cell,
}

/// One owned binding record: the symbol the legacy backend registers with
/// `track_symbol_ownership` or `allocate_binding_cell`, with its concrete
/// type and the drop glue bound through the `OwnedBinding` use record.
#[derive(Debug, Clone)]
pub(crate) struct LoweredOwnedBinding {
    pub symbol: SymbolId,
    /// The owner-local pattern that introduces the symbol, when the binding
    /// has one (a parameter, `let` pattern, or match arm pattern). A plain
    /// `let name = value` binding lowers to a binding item without a pattern.
    pub pattern: Option<PatternId>,
    pub storage: OwnedStorage,
    /// The concrete value type from the owner's binding pattern.
    pub value_type: CheckedType,
    /// The drop-glue artifact the scope-exit cleanup calls. Filled by the
    /// post-closure owned-binding collector through the use record.
    pub glue: Option<ArtifactOrdinal>,
}

/// One concrete, instance-owned function body.
#[derive(Debug, Clone)]
pub(crate) struct LoweredInstanceBody {
    /// The template this body materializes.
    pub template: FunctionId,
    /// The template declaration origin.
    pub origin: Origin,
    /// The concrete function signature; every checked value is substituted.
    pub signature: CheckedFunctionType,
    /// Substituted bounds used by the body.
    pub bounds: Vec<CheckedTraitBound>,
    pub parameter_style: FunctionParameterStyle,
    /// The instance-local parameter pattern.
    pub parameter_pattern: PatternId,
    /// Parameters in declaration order with their concrete types.
    pub parameters: Vec<LoweredInstanceParameter>,
    /// Captures in declaration order with their concrete types.
    pub captures: Vec<LoweredInstanceCapture>,
    /// The instance-local root block; absent for body-less templates.
    pub root: Option<BlockId>,
    /// The global Stage 3.3 plan this body owns, when the template is a
    /// coroutine body thunk. The local plan is `plans[0]`.
    pub plan_template: Option<LoweredCoroutinePlanId>,
    /// The instance-local providers seeded from the concrete effect row, in
    /// row order. They are ownership roots for the body.
    pub function_providers: Vec<LoweredResourceProviderId>,
    /// Concrete bindings for every dispatch/construction site.
    pub bindings: BTreeMap<LoweredBindingSite, LoweredBoundTarget>,
    /// Resolved evidence for every trait-dependent site.
    pub evidence: BTreeMap<LoweredBindingSite, TraitEvidence>,
    /// Generated-artifact uses recorded by Stage 4.2 scanners, in scan order.
    /// The validator proves these agree one-to-one with the instance's
    /// closure-phase artifact edges.
    pub artifact_uses: Vec<LoweredArtifactUse>,
    /// Source-function instance uses recorded by Stage 4.2 scanners, in scan
    /// order. The validator proves these agree one-to-one with the instance's
    /// closure-phase instance edges.
    pub instance_uses: Vec<LoweredInstanceUse>,
    /// The symbols the legacy backend tracks for scope-exit cleanup, in
    /// registration order (pattern traversal order).
    pub owned_bindings: Vec<LoweredOwnedBinding>,
    // Instance-local arenas. IDs are meaningful only inside this body.
    pub(super) blocks: Arena<LoweredBlock, BlockId>,
    pub(super) items: Arena<LoweredItem, ItemId>,
    pub(super) expressions: Arena<LoweredExpression, ExpressionId>,
    pub(super) patterns: Arena<LoweredPattern, PatternId>,
    pub(super) places: Arena<LoweredPlace, PlaceId>,
    pub(super) calls: Arena<LoweredCall, LoweredCallId>,
    pub(super) callable_values: Arena<LoweredCallableValue, LoweredCallableValueId>,
    pub(super) resource_providers: Arena<LoweredResourceProvider, LoweredResourceProviderId>,
    pub(super) resource_uses: Arena<LoweredResourceUse, LoweredResourceUseId>,
    pub(super) withs: Arena<LoweredWith, LoweredWithId>,
    pub(super) reactive_operations:
        Arena<super::LoweredReactiveOperation, LoweredReactiveOperationId>,
    pub(super) reactive_callbacks: Arena<super::LoweredReactiveCallback, LoweredReactiveCallbackId>,
    pub(super) plans: Arena<LoweredCoroutinePlan, LoweredCoroutinePlanId>,
    pub(super) coros: Arena<LoweredCoro, LoweredCoroId>,
    pub(super) awaits: Arena<LoweredAwait, LoweredAwaitId>,
}

impl Default for LoweredInstanceBody {
    fn default() -> Self {
        LoweredInstanceBody::empty(FunctionId(0), Origin::compiler())
    }
}

impl LoweredInstanceBody {
    fn empty(template: FunctionId, origin: Origin) -> Self {
        LoweredInstanceBody {
            template,
            origin,
            signature: CheckedFunctionType {
                parameter: Box::new(CheckedType::Never),
                parameter_style: FunctionParameterStyle::Single,
                default: None,
                mutations: Vec::new(),
                moves: Vec::new(),
                effects: CheckedEffectSet::default(),
                result: Box::new(CheckedType::Never),
            },
            bounds: Vec::new(),
            parameter_style: FunctionParameterStyle::Single,
            parameter_pattern: PatternId::from_index(0),
            parameters: Vec::new(),
            captures: Vec::new(),
            root: None,
            plan_template: None,
            function_providers: Vec::new(),
            bindings: BTreeMap::new(),
            evidence: BTreeMap::new(),
            artifact_uses: Vec::new(),
            instance_uses: Vec::new(),
            owned_bindings: Vec::new(),
            blocks: Arena::default(),
            items: Arena::default(),
            expressions: Arena::default(),
            patterns: Arena::default(),
            places: Arena::default(),
            calls: Arena::default(),
            callable_values: Arena::default(),
            resource_providers: Arena::default(),
            resource_uses: Arena::default(),
            withs: Arena::default(),
            reactive_operations: Arena::default(),
            reactive_callbacks: Arena::default(),
            plans: Arena::default(),
            coros: Arena::default(),
            awaits: Arena::default(),
        }
    }

    /// The body's concrete signature.
    pub(crate) fn signature(&self) -> &CheckedFunctionType {
        &self.signature
    }

    pub(crate) fn block(&self, id: BlockId) -> Option<&LoweredBlock> {
        self.blocks.get(id)
    }

    pub(crate) fn item(&self, id: ItemId) -> Option<&LoweredItem> {
        self.items.get(id)
    }

    pub(crate) fn expression(&self, id: ExpressionId) -> Option<&LoweredExpression> {
        self.expressions.get(id)
    }

    pub(crate) fn pattern(&self, id: PatternId) -> Option<&LoweredPattern> {
        self.patterns.get(id)
    }

    pub(crate) fn place(&self, id: PlaceId) -> Option<&LoweredPlace> {
        self.places.get(id)
    }

    pub(crate) fn call(&self, id: LoweredCallId) -> Option<&LoweredCall> {
        self.calls.get(id)
    }

    pub(crate) fn callable_value(
        &self,
        id: LoweredCallableValueId,
    ) -> Option<&LoweredCallableValue> {
        self.callable_values.get(id)
    }

    pub(crate) fn plan(&self, id: LoweredCoroutinePlanId) -> Option<&LoweredCoroutinePlan> {
        self.plans.get(id)
    }

    /// The body's ordered captures with their concrete types and the cleanup
    /// facts a closure-environment finalizer mirrors.
    pub(crate) fn captures(&self) -> &[LoweredInstanceCapture] {
        &self.captures
    }

    /// The concrete binding at a site, when the body has one.
    pub(crate) fn binding(&self, site: LoweredBindingSite) -> Option<&LoweredBoundTarget> {
        self.bindings.get(&site)
    }

    /// The resolved evidence at a trait-dependent site.
    pub(crate) fn resolved_evidence(&self, site: LoweredBindingSite) -> Option<&TraitEvidence> {
        self.evidence.get(&site)
    }
}

impl LoweredProgram {
    /// Materializes one concrete body per reachable instance, in Stage 3.3
    /// ordinal order. Bodies are installed on their instances only when every
    /// site bound to the existing graph; otherwise the diagnostics are
    /// returned and the caller discards lowering.
    pub(super) fn materialize_instance_bodies(&mut self) -> Vec<Diagnostic> {
        self.materialize_pending_instance_bodies()
    }

    /// Materializes one concrete body per instance that has none yet, in
    /// ordinal order. The Stage 3 entry point calls this once; the Stage 4.2
    /// closure loop calls it after each resumed worklist pass, so an instance
    /// requested by a generated artifact gets a body without re-cloning the
    /// bodies already installed. Installation is all-or-nothing per call:
    /// when any body fails, the caller discards lowering and nothing changes.
    pub(super) fn materialize_pending_instance_bodies(&mut self) -> Vec<Diagnostic> {
        let (bodies, diagnostics) = {
            let materializer = BodyMaterializer::new(self);
            materializer.build_pending()
        };
        if !diagnostics.is_empty() {
            return diagnostics;
        }
        for (instance, body) in bodies {
            if let Some(record) = self.instances.get_mut(instance) {
                record.body = Some(body);
            }
        }
        Vec::new()
    }

    /// Validates every installed instance body: local-arena reachability and
    /// bounds, concrete checked metadata, complete target bindings, and
    /// agreement with the recorded Stage 3.3 dependency and artifact edges.
    pub(super) fn validate_instance_bodies(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (id, instance) in self.instances.iter() {
            let Some(body) = &instance.body else {
                if let Some(function) = self.functions.get(instance.template)
                    && function.body.is_some()
                {
                    diagnostics.push(Diagnostic::new(
                        instance.origin.span.clone(),
                        format!("function instance {} has no concrete body", id.index()),
                    ));
                }
                continue;
            };
            diagnostics.extend(self.validate_instance_body(id, instance, body));
        }
        diagnostics
    }

    fn validate_instance_body(
        &self,
        id: FunctionInstanceId,
        instance: &super::LoweredFunctionInstance,
        body: &LoweredInstanceBody,
    ) -> Vec<Diagnostic> {
        let mut validator = BodyValidator::new(self, id, instance, body);
        validator.run();
        validator.diagnostics
    }
}

/// Builds every body from immutable program state and hands the results back
/// for installation.
struct BodyMaterializer<'a> {
    program: &'a LoweredProgram,
    instances_by_key: HashMap<InstanceKey, FunctionInstanceId>,
    artifacts_by_key: HashMap<ArtifactRequestKey, ArtifactOrdinal>,
}

impl<'a> BodyMaterializer<'a> {
    fn new(program: &'a LoweredProgram) -> Self {
        let instances_by_key = program
            .specializations
            .instances()
            .map(|(ordinal, key)| (key.clone(), FunctionInstanceId::from_index(ordinal.index())))
            .collect();
        let artifacts_by_key = program
            .specializations
            .artifacts()
            .map(|(ordinal, key)| (key.clone(), ordinal))
            .collect();
        BodyMaterializer {
            program,
            instances_by_key,
            artifacts_by_key,
        }
    }

    /// Clones a body for every instance that does not have one yet. Correctness
    /// requires that a pending instance's nested requests are all interned
    /// before this call; the resumed worklist guarantees it, and the existing
    /// "missing function instance" diagnostic reports a traversal bug
    /// otherwise. A template with no body still yields its empty body record,
    /// exactly as the one-shot Stage 3.4 entry point always did.
    fn build_pending(
        &self,
    ) -> (
        Vec<(FunctionInstanceId, LoweredInstanceBody)>,
        Vec<Diagnostic>,
    ) {
        let ids = self
            .program
            .instances
            .iter()
            .filter(|(_, instance)| instance.body.is_none())
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let mut bodies = Vec::with_capacity(ids.len());
        let mut diagnostics = Vec::new();
        for id in ids {
            let mut cloner = BodyCloner::new(
                self.program,
                id,
                &self.instances_by_key,
                &self.artifacts_by_key,
            );
            let (body, mut problems) = cloner.run();
            diagnostics.append(&mut problems);
            bodies.push((id, body));
        }
        (bodies, diagnostics)
    }
}

/// Clones one template body into an instance-owned body.
struct BodyCloner<'a> {
    program: &'a LoweredProgram,
    owner: FunctionInstanceId,
    enclosing: ResolvedInstanceRequest,
    instances_by_key: &'a HashMap<InstanceKey, FunctionInstanceId>,
    artifacts_by_key: &'a HashMap<ArtifactRequestKey, ArtifactOrdinal>,
    substitution: HashMap<TypeParameterId, CheckedType>,
    body: LoweredInstanceBody,
    active_providers: Vec<LoweredResourceProviderId>,
    diagnostics: Vec<Diagnostic>,
    blocks: HashMap<BlockId, BlockId>,
    items: HashMap<ItemId, ItemId>,
    expressions: HashMap<ExpressionId, ExpressionId>,
    patterns: HashMap<PatternId, PatternId>,
    places: HashMap<PlaceId, PlaceId>,
    calls: HashMap<LoweredCallId, LoweredCallId>,
    callable_values: HashMap<LoweredCallableValueId, LoweredCallableValueId>,
    resource_providers: HashMap<LoweredResourceProviderId, LoweredResourceProviderId>,
    resource_uses: HashMap<LoweredResourceUseId, LoweredResourceUseId>,
    withs: HashMap<LoweredWithId, LoweredWithId>,
    operations: HashMap<LoweredReactiveOperationId, LoweredReactiveOperationId>,
    callbacks: HashMap<LoweredReactiveCallbackId, LoweredReactiveCallbackId>,
    plans: HashMap<LoweredCoroutinePlanId, LoweredCoroutinePlanId>,
    coros: HashMap<LoweredCoroId, LoweredCoroId>,
    awaits: HashMap<LoweredAwaitId, LoweredAwaitId>,
}

impl<'a> BodyCloner<'a> {
    fn new(
        program: &'a LoweredProgram,
        owner: FunctionInstanceId,
        instances_by_key: &'a HashMap<InstanceKey, FunctionInstanceId>,
        artifacts_by_key: &'a HashMap<ArtifactRequestKey, ArtifactOrdinal>,
    ) -> Self {
        let enclosing = enclosing_request(program, owner);
        let substitution = enclosing.environment.substitution_map();
        let template = program
            .instances
            .get(owner)
            .map(|record| record.template)
            .unwrap_or(FunctionId(0));
        let origin = program
            .functions
            .get(template)
            .map(|function| function.origin.clone())
            .unwrap_or_else(Origin::compiler);
        BodyCloner {
            program,
            owner,
            enclosing,
            instances_by_key,
            artifacts_by_key,
            substitution,
            body: LoweredInstanceBody::empty(template, origin),
            active_providers: Vec::new(),
            diagnostics: Vec::new(),
            blocks: HashMap::new(),
            items: HashMap::new(),
            expressions: HashMap::new(),
            patterns: HashMap::new(),
            places: HashMap::new(),
            calls: HashMap::new(),
            callable_values: HashMap::new(),
            resource_providers: HashMap::new(),
            resource_uses: HashMap::new(),
            withs: HashMap::new(),
            operations: HashMap::new(),
            callbacks: HashMap::new(),
            plans: HashMap::new(),
            coros: HashMap::new(),
            awaits: HashMap::new(),
        }
    }

    fn run(&mut self) -> (LoweredInstanceBody, Vec<Diagnostic>) {
        let template = self.body.template;
        let Some(function) = self.program.functions.get(template).cloned() else {
            self.diagnostics.push(Diagnostic::new(
                self.body.origin.span.clone(),
                format!(
                    "instance {} has no lowered function template {}",
                    self.owner.index(),
                    template.0
                ),
            ));
            return (
                std::mem::replace(
                    &mut self.body,
                    LoweredInstanceBody::empty(template, Origin::compiler()),
                ),
                std::mem::take(&mut self.diagnostics),
            );
        };
        self.body.origin = function.origin.clone();
        self.body.signature = self.function_type(&function.signature);
        self.body.bounds = function
            .bounds
            .iter()
            .filter_map(|bound| {
                // A bound whose parameter is absent from the resolved
                // environment is irrelevant to this instance. Stage 3.2
                // deliberately excludes such parameters from its key, so
                // there is no concrete bound to store in the body.
                if bound
                    .arguments
                    .iter()
                    .any(|argument| contains_type_parameter(&self.ty(argument)))
                {
                    return None;
                }
                match self.program.complete_declared_bound(
                    &function.origin,
                    bound,
                    &self.enclosing.environment,
                ) {
                    Ok(bound) => Some(bound),
                    Err(diagnostic) => {
                        self.diagnostics.push(diagnostic);
                        None
                    }
                }
            })
            .collect();
        self.body.parameter_style = function.parameter_style;
        self.body.parameters = function
            .parameters
            .iter()
            .map(|symbol| LoweredInstanceParameter {
                symbol: *symbol,
                value_type: self.symbol_type(*symbol),
            })
            .collect();
        self.body.captures = function
            .captures
            .iter()
            .map(|capture| {
                let symbol = self.program.symbols.get(capture.symbol);
                LoweredInstanceCapture {
                    capture: capture.clone(),
                    value_type: self.symbol_type(capture.symbol),
                    requires_initialization_state: symbol
                        .is_some_and(|symbol| symbol.requires_initialization_check),
                    mutable_storage: symbol.is_some_and(|symbol| symbol.mutable_storage),
                    derived: symbol.is_some_and(|symbol| symbol.derived),
                }
            })
            .collect();
        self.body.parameter_pattern = self.clone_pattern(function.parameter_pattern);
        self.seed_function_providers(template);
        if let Some(plan) = self.program.coroutine_plan_by_thunk.get(&template).copied() {
            self.clone_plan(plan);
        }
        self.body.root = function.body.map(|block| self.clone_block(block));
        (
            std::mem::take(&mut self.body),
            std::mem::take(&mut self.diagnostics),
        )
    }

    // ------------------------------------------------------------------
    // Substitution helpers.
    // ------------------------------------------------------------------

    fn ty(&self, value_type: &CheckedType) -> CheckedType {
        substitute_type(value_type.clone(), &self.substitution)
    }

    fn effects(&self, effects: &CheckedEffectSet) -> CheckedEffectSet {
        substitute_effect_set(effects.clone(), &self.substitution)
    }

    fn function_type(&self, function_type: &CheckedFunctionType) -> CheckedFunctionType {
        match self.ty(&CheckedType::Function(function_type.clone())) {
            CheckedType::Function(function_type) => function_type,
            other => CheckedFunctionType {
                parameter: Box::new(other),
                parameter_style: function_type.parameter_style,
                default: function_type.default.clone(),
                mutations: function_type.mutations.clone(),
                moves: function_type.moves.clone(),
                effects: self.effects(&function_type.effects),
                result: Box::new(CheckedType::Error),
            },
        }
    }

    fn bounds(&self, bounds: &[CheckedTraitBound]) -> Vec<CheckedTraitBound> {
        bounds
            .iter()
            .map(|bound| CheckedTraitBound {
                trait_id: bound.trait_id,
                arguments: bound
                    .arguments
                    .iter()
                    .map(|argument| self.ty(argument))
                    .collect(),
            })
            .collect()
    }

    fn coercion(&self, coercion: &CheckedCoercion) -> CheckedCoercion {
        CheckedCoercion {
            source: self.ty(&coercion.source),
            target: self.ty(&coercion.target),
        }
    }

    fn propagation(&self, propagation: &CheckedPropagation) -> CheckedPropagation {
        CheckedPropagation {
            source: self.ty(&propagation.source),
            success_index: propagation.success_index,
            result: self.ty(&propagation.result),
        }
    }

    fn dispatch(&self, dispatch: &CheckedTraitDispatch) -> CheckedTraitDispatch {
        CheckedTraitDispatch {
            method: dispatch.method,
            arguments: dispatch
                .arguments
                .iter()
                .map(|argument| self.ty(argument))
                .collect(),
        }
    }

    fn resource(&self, resource: &CheckedResource) -> CheckedResource {
        CheckedResource {
            value_type: self.ty(&resource.value_type),
            mutable: resource.mutable,
        }
    }

    fn product_type(&self, product: &CheckedProductType) -> CheckedProductType {
        CheckedProductType {
            elements: product
                .elements
                .iter()
                .map(|element| crate::CheckedTypeElement {
                    name: element.name.clone(),
                    value_type: self.ty(&element.value_type),
                    default: None,
                })
                .collect(),
            variadic: product.variadic,
        }
    }

    fn substitutions(&self, substitutions: &CallSubstitutions) -> CallSubstitutions {
        CallSubstitutions {
            types: substitutions
                .types
                .iter()
                .map(|entry| crate::CallTypeSubstitution {
                    parameter: entry.parameter,
                    value_type: self.ty(&entry.value_type),
                })
                .collect(),
            effects: substitutions
                .effects
                .iter()
                .map(|entry| crate::CallEffectSubstitution {
                    parameter: entry.parameter,
                    effects: self.effects(&entry.effects),
                })
                .collect(),
        }
    }

    fn evidence(&self, evidence: &TraitEvidence) -> TraitEvidence {
        match evidence {
            TraitEvidence::ExplicitImplementation {
                trait_id,
                implementation,
                method,
                function,
                arguments,
            } => TraitEvidence::ExplicitImplementation {
                trait_id: *trait_id,
                implementation: *implementation,
                method: *method,
                function: *function,
                arguments: arguments.iter().map(|argument| self.ty(argument)).collect(),
            },
            TraitEvidence::Structural {
                trait_id,
                method,
                structural,
                arguments,
            } => TraitEvidence::Structural {
                trait_id: *trait_id,
                method: *method,
                structural: *structural,
                arguments: arguments.iter().map(|argument| self.ty(argument)).collect(),
            },
            TraitEvidence::DeclaredBound {
                trait_id,
                method,
                arguments,
                prerequisites,
            } => TraitEvidence::DeclaredBound {
                trait_id: *trait_id,
                method: *method,
                arguments: arguments.iter().map(|argument| self.ty(argument)).collect(),
                prerequisites: self.bounds(prerequisites),
            },
            TraitEvidence::RejectedImplementation {
                trait_id,
                implementation,
                arguments,
            } => TraitEvidence::RejectedImplementation {
                trait_id: *trait_id,
                implementation: *implementation,
                arguments: arguments.iter().map(|argument| self.ty(argument)).collect(),
            },
        }
    }

    /// The concrete type of one global symbol, substituted for this instance.
    fn symbol_type(&self, symbol: SymbolId) -> CheckedType {
        self.program
            .symbols
            .get(symbol)
            .map(|record| self.ty(&record.value_type))
            .unwrap_or(CheckedType::Error)
    }

    // ------------------------------------------------------------------
    // Clone functions, in dependency order.
    // ------------------------------------------------------------------

    fn clone_block(&mut self, id: BlockId) -> BlockId {
        if let Some(new) = self.blocks.get(&id) {
            return *new;
        }
        let Some(block) = self.program.blocks.get(id).cloned() else {
            self.missing("block", id.index());
            return self.error_block();
        };
        let items = block
            .items
            .iter()
            .map(|item| self.clone_item(*item))
            .collect();
        let result = block.result.map(|result| self.clone_expression(result));
        let new = self.body.blocks.push(LoweredBlock {
            origin: block.origin,
            items,
            result,
        });
        self.blocks.insert(id, new);
        new
    }

    fn clone_item(&mut self, id: ItemId) -> ItemId {
        if let Some(new) = self.items.get(&id) {
            return *new;
        }
        let Some(item) = self.program.items.get(id).cloned() else {
            self.missing("item", id.index());
            return self.body.items.push(LoweredItem {
                origin: Origin::compiler(),
                kind: LoweredItemKind::Continue(super::LoweredContinueItem { loop_depth: 0 }),
            });
        };
        let mut assignment_site = None;
        let new_kind = match item.kind {
            LoweredItemKind::Binding(mut binding) => {
                if binding.generic {
                    // A generic local binding is a compile-time template
                    // declaration: its generic value is never emitted.
                    binding.value = None;
                    binding.reactive = None;
                } else {
                    binding.value = binding.value.map(|value| self.clone_expression(value));
                    binding.reactive = binding
                        .reactive
                        .map(|operation| self.clone_operation(operation));
                }
                LoweredItemKind::Binding(binding)
            }
            LoweredItemKind::PatternBinding(mut binding) => {
                binding.pattern = self.clone_pattern(binding.pattern);
                binding.value = self.clone_expression(binding.value);
                binding.propagation = binding.propagation.as_ref().map(|p| self.propagation(p));
                LoweredItemKind::PatternBinding(binding)
            }
            LoweredItemKind::Assignment(mut assignment) => {
                assignment_site = assignment
                    .mutate_index
                    .clone()
                    .zip(assignment.evidence.clone());
                assignment.target = self.clone_place(assignment.target);
                assignment.value = self.clone_expression(assignment.value);
                assignment.mutate_index = assignment
                    .mutate_index
                    .as_ref()
                    .map(|dispatch| self.dispatch(dispatch));
                assignment.evidence = assignment
                    .evidence
                    .as_ref()
                    .map(|evidence| self.evidence(evidence));
                assignment.signal_notify = assignment
                    .signal_notify
                    .map(|operation| self.clone_operation(operation));
                let concrete_target = self
                    .body
                    .places
                    .get(assignment.target)
                    .map(|place| place.value_type.clone())
                    .unwrap_or(CheckedType::Error);
                assignment.drop_previous = self.program.concrete_needs_drop(&concrete_target);
                LoweredItemKind::Assignment(assignment)
            }
            LoweredItemKind::Return(mut item) => {
                item.value = self.clone_expression(item.value);
                LoweredItemKind::Return(item)
            }
            LoweredItemKind::Break(mut item) => {
                item.value = item.value.map(|value| self.clone_expression(value));
                LoweredItemKind::Break(item)
            }
            LoweredItemKind::Continue(item) => LoweredItemKind::Continue(item),
            LoweredItemKind::Expression(mut item) => {
                item.expression = self.clone_expression(item.expression);
                let concrete = self
                    .body
                    .expressions
                    .get(item.expression)
                    .map(|expression| expression.value_type.clone())
                    .unwrap_or(CheckedType::Error);
                item.drop_result = self.program.concrete_needs_drop(&concrete);
                LoweredItemKind::Expression(item)
            }
        };
        let origin = item.origin;
        let new = self.body.items.push(LoweredItem {
            origin: origin.clone(),
            kind: new_kind,
        });
        self.items.insert(id, new);
        if let Some((dispatch, evidence)) = assignment_site {
            let trait_id = evidence_trait_id(&evidence);
            let substitutions =
                trait_site_substitutions(self.program, trait_id, &dispatch.arguments);
            self.bind_trait_site(
                LoweredBindingSite::IndexedAssignment(new),
                &origin,
                trait_id,
                dispatch.method,
                &evidence,
                None,
                substitutions,
            );
        }
        new
    }

    // ------------------------------------------------------------------
    // Expressions and every function-owned record family.
    // ------------------------------------------------------------------

    fn clone_expression(&mut self, id: ExpressionId) -> ExpressionId {
        if let Some(new) = self.expressions.get(&id) {
            return *new;
        }
        let Some(expression) = self.program.expressions.get(id).cloned() else {
            self.missing("expression", id.index());
            return self.error_expression();
        };
        let origin = expression.origin.clone();
        let original_kind = expression.kind.clone();
        let kind = match expression.kind {
            LoweredExpressionKind::Deferred(family) => LoweredExpressionKind::Deferred(family),
            LoweredExpressionKind::Stage26Deferred(route) => {
                LoweredExpressionKind::Stage26Deferred(route)
            }
            LoweredExpressionKind::Block(block) => {
                LoweredExpressionKind::Block(self.clone_block(block))
            }
            LoweredExpressionKind::Name(mut name) => {
                name.reactive = name
                    .reactive
                    .map(|operation| self.clone_operation(operation));
                LoweredExpressionKind::Name(self.concretize_name(name))
            }
            LoweredExpressionKind::Integer(integer) => LoweredExpressionKind::Integer(integer),
            LoweredExpressionKind::Float(float) => LoweredExpressionKind::Float(float),
            LoweredExpressionKind::String(string) => LoweredExpressionKind::String(string),
            LoweredExpressionKind::CString(c_string) => LoweredExpressionKind::CString(c_string),
            LoweredExpressionKind::Access(mut access) => {
                access.base = self.clone_expression(access.base);
                access.kind = match access.kind {
                    super::LoweredAccessKind::Representation { dereference } => {
                        super::LoweredAccessKind::Representation {
                            dereference: dereference.iter().map(|ty| self.ty(ty)).collect(),
                        }
                    }
                    super::LoweredAccessKind::Product { index, dereference } => {
                        super::LoweredAccessKind::Product {
                            index,
                            dereference: dereference.iter().map(|ty| self.ty(ty)).collect(),
                        }
                    }
                    super::LoweredAccessKind::Slice { index, dereference } => {
                        super::LoweredAccessKind::Slice {
                            index,
                            dereference: dereference.iter().map(|ty| self.ty(ty)).collect(),
                        }
                    }
                    super::LoweredAccessKind::Scalar { dereference } => {
                        super::LoweredAccessKind::Scalar {
                            dereference: dereference.iter().map(|ty| self.ty(ty)).collect(),
                        }
                    }
                };
                LoweredExpressionKind::Access(access)
            }
            LoweredExpressionKind::Product(mut product) => {
                product.final_type = self.product_type(&product.final_type);
                product.steps = product
                    .steps
                    .into_iter()
                    .map(|step| match step {
                        super::LoweredProductStep::Positional { expression, slot } => {
                            super::LoweredProductStep::Positional {
                                expression: self.clone_expression(expression),
                                slot,
                            }
                        }
                        super::LoweredProductStep::Designated {
                            name,
                            expression,
                            slot,
                        } => super::LoweredProductStep::Designated {
                            name,
                            expression: self.clone_expression(expression),
                            slot,
                        },
                        super::LoweredProductStep::PositionalSpread {
                            expression,
                            mappings,
                        } => super::LoweredProductStep::PositionalSpread {
                            expression: self.clone_expression(expression),
                            mappings,
                        },
                        super::LoweredProductStep::NamedSpread {
                            expression,
                            mappings,
                        } => super::LoweredProductStep::NamedSpread {
                            expression: self.clone_expression(expression),
                            mappings,
                        },
                        super::LoweredProductStep::Default {
                            slot,
                            expression,
                            expected,
                        } => super::LoweredProductStep::Default {
                            slot,
                            expression: self.clone_expression(expression),
                            expected: self.ty(&expected),
                        },
                    })
                    .collect();
                product.fields = product
                    .fields
                    .into_iter()
                    .map(|field| self.clone_expression(field))
                    .collect();
                LoweredExpressionKind::Product(product)
            }
            LoweredExpressionKind::RepeatedProduct(mut product) => {
                product.expression = self.clone_expression(product.expression);
                product.count = match product.count {
                    super::LoweredRepeatCount::Fixed(count) => {
                        super::LoweredRepeatCount::Fixed(count)
                    }
                    super::LoweredRepeatCount::Symbolic(value_type) => {
                        super::LoweredRepeatCount::Symbolic(self.ty(&value_type))
                    }
                };
                LoweredExpressionKind::RepeatedProduct(product)
            }
            LoweredExpressionKind::Satisfies(mut satisfies) => {
                satisfies.value = self.clone_expression(satisfies.value);
                LoweredExpressionKind::Satisfies(satisfies)
            }
            LoweredExpressionKind::Logical(mut logical) => {
                logical.left = self.clone_expression(logical.left);
                logical.right = self.clone_expression(logical.right);
                logical.bool_type = self.ty(&logical.bool_type);
                LoweredExpressionKind::Logical(logical)
            }
            LoweredExpressionKind::Loop(mut loop_) => {
                loop_.body = self.clone_block(loop_.body);
                loop_.result_type = self.ty(&loop_.result_type);
                let body_result = self
                    .body
                    .blocks
                    .get(loop_.body)
                    .and_then(|block| block.result)
                    .and_then(|result| self.body.expressions.get(result))
                    .map(|expression| expression.value_type.clone());
                if let Some(body_result) = body_result {
                    loop_.drops_body_result = self.program.concrete_needs_drop(&body_result);
                }
                LoweredExpressionKind::Loop(loop_)
            }
            LoweredExpressionKind::Match(mut match_) => {
                match_.subject = self.clone_expression(match_.subject);
                match_.source = self.ty(&match_.source);
                for arm in &mut match_.arms {
                    arm.pattern = self.clone_pattern(arm.pattern);
                    arm.body = self.clone_expression(arm.body);
                }
                LoweredExpressionKind::Match(match_)
            }
            LoweredExpressionKind::Index(mut index) => {
                index.base = self.clone_expression(index.base);
                index.index = self.clone_expression(index.index);
                index.dispatch = self.dispatch(&index.dispatch);
                index.arguments = index.arguments.iter().map(|ty| self.ty(ty)).collect();
                index.method_type = index.method_type.as_ref().map(|ty| self.function_type(ty));
                index.evidence = self.evidence(&index.evidence);
                LoweredExpressionKind::Index(index)
            }
            LoweredExpressionKind::StringTemplate(mut template) => {
                for part in &mut template.parts {
                    if let LoweredStringTemplatePart::Interpolation(interpolation) = part {
                        interpolation.expression = self.clone_expression(interpolation.expression);
                        interpolation.value_type = self.ty(&interpolation.value_type);
                        interpolation.evidence = self.evidence(&interpolation.evidence);
                    }
                }
                LoweredExpressionKind::StringTemplate(template)
            }
            LoweredExpressionKind::Call(call) => LoweredExpressionKind::Call(self.clone_call(call)),
            LoweredExpressionKind::CallableValue(value) => {
                LoweredExpressionKind::CallableValue(self.clone_callable_value(value))
            }
            LoweredExpressionKind::Resource(use_) => {
                LoweredExpressionKind::Resource(self.clone_resource_use(use_))
            }
            LoweredExpressionKind::With(with) => LoweredExpressionKind::With(self.clone_with(with)),
            LoweredExpressionKind::Coro(coro) => LoweredExpressionKind::Coro(self.clone_coro(coro)),
            LoweredExpressionKind::Await(await_) => {
                LoweredExpressionKind::Await(self.clone_await(await_))
            }
        };
        let new = self.body.expressions.push(LoweredExpression {
            key: expression.key,
            origin: origin.clone(),
            value_type: self.ty(&expression.value_type),
            effects: self.effects(&expression.effects),
            coercion: expression
                .coercion
                .as_ref()
                .map(|coercion| self.coercion(coercion)),
            moved_symbols: expression.moved_symbols,
            kind,
        });
        self.expressions.insert(id, new);
        match &original_kind {
            LoweredExpressionKind::Index(original_index) => {
                let trait_id = original_index.trait_id;
                let substitutions =
                    trait_site_substitutions(self.program, trait_id, &original_index.arguments);
                self.bind_trait_site(
                    LoweredBindingSite::Index(new),
                    &origin,
                    trait_id,
                    original_index.dispatch.method,
                    &original_index.evidence,
                    original_index.method_type.clone(),
                    substitutions,
                );
            }
            LoweredExpressionKind::StringTemplate(original_template) => {
                let has_literal = original_template
                    .parts
                    .iter()
                    .any(|part| matches!(part, LoweredStringTemplatePart::Literal(_)));
                self.bind_formatting_helpers(new, has_literal, &origin);
                for (part_index, part) in original_template.parts.iter().enumerate() {
                    let LoweredStringTemplatePart::Interpolation(interpolation) = part else {
                        continue;
                    };
                    let substitutions = trait_site_substitutions(
                        self.program,
                        interpolation.trait_id,
                        std::slice::from_ref(&interpolation.value_type),
                    );
                    self.bind_trait_site(
                        LoweredBindingSite::Interpolation {
                            template: new,
                            part: part_index,
                        },
                        &origin,
                        interpolation.trait_id,
                        interpolation.method,
                        &interpolation.evidence,
                        None,
                        substitutions,
                    );
                }
            }
            _ => {}
        }
        new
    }

    /// A name's storage classification is concrete-sensitive only through the
    /// symbol catalog, which stays global; the reactive operation is
    /// instance-local.
    fn concretize_name(&self, name: LoweredName) -> LoweredName {
        name
    }

    fn clone_place(&mut self, id: PlaceId) -> PlaceId {
        if let Some(new) = self.places.get(&id) {
            return *new;
        }
        let Some(place) = self.program.places.get(id).cloned() else {
            self.missing("place", id.index());
            let expression = self.error_expression();
            return self.body.places.push(super::LoweredPlace {
                origin: Origin::compiler(),
                value_type: CheckedType::Error,
                kind: LoweredPlaceKind::Temporary { expression },
            });
        };
        let kind = match place.kind {
            LoweredPlaceKind::Symbol { symbol } => LoweredPlaceKind::Symbol { symbol },
            LoweredPlaceKind::CapturedCell { symbol } => LoweredPlaceKind::CapturedCell { symbol },
            LoweredPlaceKind::Temporary { expression } => LoweredPlaceKind::Temporary {
                expression: self.clone_expression(expression),
            },
            LoweredPlaceKind::Resource { use_ } => LoweredPlaceKind::Resource {
                use_: self.clone_resource_use(use_),
            },
            LoweredPlaceKind::Dereference {
                reference,
                dereference,
            } => LoweredPlaceKind::Dereference {
                reference: self.clone_expression(reference),
                dereference: dereference.iter().map(|value| self.ty(value)).collect(),
            },
            LoweredPlaceKind::ProductElement { base, index, slice } => {
                LoweredPlaceKind::ProductElement {
                    base: self.clone_place(base),
                    index,
                    slice,
                }
            }
            LoweredPlaceKind::Representation { base } => LoweredPlaceKind::Representation {
                base: self.clone_place(base),
            },
            LoweredPlaceKind::Indexed { base, index } => LoweredPlaceKind::Indexed {
                base: self.clone_place(base),
                index: self.clone_expression(index),
            },
        };
        let new = self.body.places.push(super::LoweredPlace {
            origin: place.origin,
            value_type: self.ty(&place.value_type),
            kind,
        });
        self.places.insert(id, new);
        new
    }

    fn clone_pattern(&mut self, id: PatternId) -> PatternId {
        if let Some(new) = self.patterns.get(&id) {
            return *new;
        }
        let Some(pattern) = self.program.patterns.get(id).cloned() else {
            self.missing("pattern", id.index());
            return self.body.patterns.push(LoweredPattern {
                origin: Origin::compiler(),
                value_type: CheckedType::Error,
                kind: LoweredPatternKind::Wildcard,
            });
        };
        let kind = match pattern.kind {
            LoweredPatternKind::Wildcard => LoweredPatternKind::Wildcard,
            LoweredPatternKind::Binding {
                symbol,
                singleton,
                mutable,
                moved,
            } => LoweredPatternKind::Binding {
                symbol,
                singleton,
                mutable,
                moved,
            },
            LoweredPatternKind::Product {
                elements,
                mutable,
                moved,
            } => LoweredPatternKind::Product {
                elements: elements
                    .iter()
                    .map(|element| self.clone_pattern(*element))
                    .collect(),
                mutable,
                moved,
            },
            LoweredPatternKind::Nominal {
                target,
                name,
                moved,
                argument,
            } => LoweredPatternKind::Nominal {
                target,
                name,
                moved,
                argument: self.clone_pattern(argument),
            },
            LoweredPatternKind::Literal { literal } => LoweredPatternKind::Literal { literal },
            LoweredPatternKind::At { binding, pattern } => LoweredPatternKind::At {
                binding: self.clone_pattern(binding),
                pattern: self.clone_pattern(pattern),
            },
        };
        let new = self.body.patterns.push(LoweredPattern {
            origin: pattern.origin,
            value_type: self.ty(&pattern.value_type),
            kind,
        });
        self.patterns.insert(id, new);
        new
    }

    // ------------------------------------------------------------------
    // Diagnostics and fallbacks. A missing source node or remap is a
    // diagnostic at its recorded origin, never a panic.
    // ------------------------------------------------------------------

    fn missing(&mut self, family: &str, index: usize) {
        self.diagnostics.push(Diagnostic::new(
            self.body.origin.span.clone(),
            format!(
                "instance {} body references missing template {family} {index}",
                self.owner.index()
            ),
        ));
    }

    fn error_expression(&mut self) -> ExpressionId {
        self.body.expressions.push(LoweredExpression {
            key: super::ExpressionKey {
                syntax: staple_syntax::SyntaxId::COMPILER,
                owner: super::ExpressionOwner::Function(self.body.template),
                context: super::ExpressionContext::Primary,
            },
            origin: Origin::compiler(),
            value_type: CheckedType::Error,
            effects: CheckedEffectSet::default(),
            coercion: None,
            moved_symbols: Vec::new(),
            kind: LoweredExpressionKind::Deferred(super::DeferredExpressionFamily::Callable),
        })
    }

    fn error_block(&mut self) -> BlockId {
        self.body.blocks.push(LoweredBlock {
            origin: Origin::compiler(),
            items: Vec::new(),
            result: None,
        })
    }

    fn error_operation(&mut self) -> LoweredReactiveOperationId {
        self.body
            .reactive_operations
            .push(super::LoweredReactiveOperation {
                origin: Origin::compiler(),
                kind: LoweredReactiveOperationKind::Snapshot,
            })
    }

    /// Seeds the instance-local providers from the concrete effect row, in
    /// row order. A template provider whose substituted resource matches is
    /// cloned (with its position updated); a resource that only exists after
    /// substitution of a generic effect variable gets a fresh provider. They
    /// are body ownership roots.
    fn seed_function_providers(&mut self, template: FunctionId) {
        let concrete = self.body.signature.effects.resources.clone();
        let template_providers = self
            .program
            .resource_providers
            .iter()
            .filter(|(_, provider)| {
                provider.kind == super::LoweredProviderOriginKind::FunctionParameter
                    && provider.owner == super::ExpressionOwner::Function(template)
            })
            .map(|(id, provider)| {
                (
                    id,
                    self.ty(&provider.resource.value_type),
                    provider.resource.mutable,
                )
            })
            .collect::<Vec<_>>();
        let mut consumed = vec![false; template_providers.len()];
        for (position, resource) in concrete.iter().enumerate() {
            let matched = template_providers
                .iter()
                .enumerate()
                .find(|(index, (_, value_type, mutable))| {
                    !consumed[*index]
                        && *value_type == resource.value_type
                        && *mutable == resource.mutable
                })
                .map(|(index, (id, _, _))| {
                    consumed[index] = true;
                    *id
                });
            let cloned = match matched {
                Some(id) => {
                    let cloned = self.clone_resource_provider(id);
                    if let Some(provider) = self.body.resource_providers.get_mut(cloned) {
                        provider.target =
                            super::LoweredProviderTarget::EffectParameter { position };
                    }
                    cloned
                }
                None => self.create_function_provider(position, resource),
            };
            self.body.function_providers.push(cloned);
            self.active_providers.push(cloned);
        }
    }

    /// Creates a fresh function-parameter provider for a concrete effect-row
    /// position that the generic template did not materialize.
    fn create_function_provider(
        &mut self,
        position: usize,
        resource: &CheckedResource,
    ) -> LoweredResourceProviderId {
        let indirect = resource.mutable || !self.program.concrete_is_copy(&resource.value_type);
        let provider = LoweredResourceProvider {
            origin: self.body.origin.clone(),
            resource: self.resource(resource),
            kind: super::LoweredProviderOriginKind::FunctionParameter,
            target: super::LoweredProviderTarget::EffectParameter { position },
            parent: None,
            owner: super::ExpressionOwner::Function(self.body.template),
            indirect,
            borrow: indirect,
            storage: super::LoweredProviderStorage::Materialized,
            scope_exit: super::LoweredScopeExit::Ordinary,
        };
        self.body.resource_providers.push(provider)
    }

    /// Rebuilds a call's hidden effect-row bindings when substitution changed
    /// the row (a generic effect row kept no providers, or an expanded row
    /// changed its order). Existing bindings whose concrete resource type
    /// still matches are preserved; the rest select the innermost active
    /// provider with the exact concrete type, matching the backend's rule.
    fn rebind_hidden_resources(&mut self, call: &mut LoweredCall) {
        let passes_hidden = !matches!(
            call.target.category(),
            LoweredCallableCategory::ExternalFunction
                | LoweredCallableCategory::Intrinsic
                | LoweredCallableCategory::Constructor
        );
        if !passes_hidden {
            return;
        }
        if call.resource_bindings.len() == call.function_type.effects.resources.len()
            && call
                .resource_bindings
                .iter()
                .zip(&call.function_type.effects.resources)
                .all(|(id, resource)| {
                    self.body
                        .resource_uses
                        .get(*id)
                        .is_some_and(|use_| use_.resource == *resource)
                })
        {
            return;
        }
        let existing = call
            .resource_bindings
            .iter()
            .map(|use_| {
                (
                    *use_,
                    self.body
                        .resource_uses
                        .get(*use_)
                        .map(|use_| use_.resource.clone()),
                )
            })
            .collect::<Vec<_>>();
        let mut consumed = vec![false; existing.len()];
        let mut bindings = Vec::new();
        for resource in &call.function_type.effects.resources {
            let reused = existing
                .iter()
                .enumerate()
                .find(|(index, (_, existing_resource))| {
                    !consumed[*index] && existing_resource.as_ref() == Some(resource)
                })
                .map(|(index, (use_, _))| {
                    consumed[index] = true;
                    *use_
                });
            if let Some(use_) = reused {
                bindings.push(use_);
                continue;
            }
            let provider = self
                .active_providers
                .iter()
                .rev()
                .find(|provider| {
                    self.body
                        .resource_providers
                        .get(**provider)
                        .is_some_and(|provider| provider.resource.value_type == resource.value_type)
                })
                .copied();
            match provider {
                Some(provider) => {
                    let borrow =
                        resource.mutable || !self.program.concrete_is_copy(&resource.value_type);
                    let pass_mode = if borrow {
                        super::LoweredArgumentPassMode::BorrowedPointer
                    } else {
                        super::LoweredArgumentPassMode::Value
                    };
                    let indirect = self
                        .body
                        .resource_providers
                        .get(provider)
                        .is_some_and(|provider| provider.indirect);
                    let use_ = self.body.resource_uses.push(LoweredResourceUse {
                        origin: call.origin.clone(),
                        resource: resource.clone(),
                        provider: Some(provider),
                        kind: super::LoweredResourceUseKind::HiddenArgument,
                        pass_mode,
                        indirect,
                    });
                    bindings.push(use_);
                }
                None => self.diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    format!(
                        "resource `{}` is not available for the concrete instance",
                        resource.value_type
                    ),
                )),
            }
        }
        call.resource_bindings = bindings;
    }

    /// Recomputes concrete-sensitive call-argument pass decisions from the
    /// substituted parameter types, mirroring the lowering rule.
    fn recompute_call_arguments(&self, call: &mut LoweredCall) {
        let whole_mutation = call
            .function_type
            .mutations
            .contains(&CheckedMutation::Whole);
        let whole_move = call.function_type.moves.contains(&CheckedMutation::Whole);
        for argument in &mut call.arguments {
            let expected = argument.expected.clone();
            let mutation = whole_mutation
                || argument.slot.is_some_and(|slot| {
                    call.function_type
                        .mutations
                        .contains(&CheckedMutation::Element(slot))
                });
            let moves = whole_move
                || argument.slot.is_some_and(|slot| {
                    call.function_type
                        .moves
                        .contains(&CheckedMutation::Element(slot))
                });
            let indirect = mutation || (!moves && !self.program.concrete_is_copy(&expected));
            argument.pass_mode = if mutation {
                super::LoweredArgumentPassMode::MutablePlace
            } else if indirect {
                if argument.place.is_some() {
                    super::LoweredArgumentPassMode::BorrowedPointer
                } else {
                    super::LoweredArgumentPassMode::MaterializedTemporary
                }
            } else {
                super::LoweredArgumentPassMode::Value
            };
            argument.temporary = (mutation || indirect) && argument.place.is_none();
            argument.drops_after_call =
                mutation && argument.place.is_none() && self.program.concrete_needs_drop(&expected);
        }
    }

    /// Recomputes a `with` provider's borrow and storage from the concrete
    /// resource type and the provider value's place.
    fn recompute_with_provider(
        &mut self,
        provider: LoweredResourceProviderId,
        value: ExpressionId,
    ) {
        let Some(resource) = self
            .body
            .resource_providers
            .get(provider)
            .map(|provider| provider.resource.clone())
        else {
            return;
        };
        let borrow = resource.mutable || !self.program.concrete_is_copy(&resource.value_type);
        let has_place = self.body_value_has_place(value);
        if let Some(provider) = self.body.resource_providers.get_mut(provider) {
            provider.borrow = borrow;
            provider.storage = if borrow && has_place {
                super::LoweredProviderStorage::Place
            } else {
                super::LoweredProviderStorage::Materialized
            };
        }
    }

    /// Whether an instance-local expression is rooted at addressable storage,
    /// mirroring the checked `provider_value_has_place` rule.
    fn body_value_has_place(&self, expression: ExpressionId) -> bool {
        let Some(expression) = self.body.expressions.get(expression) else {
            return false;
        };
        match &expression.kind {
            LoweredExpressionKind::Name(_) => true,
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.body_value_has_place(satisfies.value)
            }
            LoweredExpressionKind::Product(product) if product.fields.len() == 1 => {
                self.body_value_has_place(product.fields[0])
            }
            LoweredExpressionKind::Resource(use_) => self
                .body
                .resource_uses
                .get(*use_)
                .and_then(|use_| use_.provider)
                .and_then(|provider| self.body.resource_providers.get(provider))
                .is_some_and(|provider| provider.indirect),
            LoweredExpressionKind::Access(access) => match &access.kind {
                super::LoweredAccessKind::Representation { dereference } => {
                    !dereference.is_empty() || self.body_value_has_place(access.base)
                }
                super::LoweredAccessKind::Product { dereference, .. } => {
                    !dereference.is_empty() || self.body_value_has_place(access.base)
                }
                super::LoweredAccessKind::Slice { .. } => true,
                super::LoweredAccessKind::Scalar { .. } => false,
            },
            _ => false,
        }
    }

    fn error_use(&mut self) -> LoweredResourceUseId {
        self.body.resource_uses.push(LoweredResourceUse {
            origin: Origin::compiler(),
            resource: CheckedResource {
                value_type: CheckedType::Error,
                mutable: false,
            },
            provider: None,
            kind: super::LoweredResourceUseKind::Read,
            pass_mode: super::LoweredArgumentPassMode::Value,
            indirect: false,
        })
    }

    fn error_provider(&mut self) -> LoweredResourceProviderId {
        self.body.resource_providers.push(LoweredResourceProvider {
            origin: Origin::compiler(),
            resource: CheckedResource {
                value_type: CheckedType::Error,
                mutable: false,
            },
            kind: super::LoweredProviderOriginKind::Source,
            target: super::LoweredProviderTarget::Entry,
            parent: None,
            owner: super::ExpressionOwner::Function(self.body.template),
            indirect: false,
            borrow: false,
            storage: super::LoweredProviderStorage::Materialized,
            scope_exit: super::LoweredScopeExit::Ordinary,
        })
    }

    fn error_callback(&mut self) -> LoweredReactiveCallbackId {
        self.body
            .reactive_callbacks
            .push(super::LoweredReactiveCallback {
                origin: Origin::compiler(),
                thunk: None,
                callable: None,
                function_type: self.error_signature(),
                captures: Vec::new(),
                resources: Vec::new(),
            })
    }

    fn error_signature(&self) -> CheckedFunctionType {
        CheckedFunctionType {
            parameter: Box::new(CheckedType::Error),
            parameter_style: FunctionParameterStyle::Single,
            default: None,
            mutations: Vec::new(),
            moves: Vec::new(),
            effects: CheckedEffectSet::default(),
            result: Box::new(CheckedType::Error),
        }
    }

    fn error_plan(&mut self) -> LoweredCoroutinePlanId {
        self.body.plans.push(LoweredCoroutinePlan {
            origin: Origin::compiler(),
            body_syntax: staple_syntax::SyntaxId::COMPILER,
            body: None,
            thunk: self.body.template,
            captures: Vec::new(),
            result_type: CheckedType::Error,
            deferred_effects: CheckedEffectSet::default(),
            resume_points: 0,
            frame_bindings: Vec::new(),
            await_result_types: Vec::new(),
            wait_await_states: Vec::new(),
            until_await_states: Vec::new(),
            awaits: Vec::new(),
        })
    }

    fn error_coro(&mut self) -> LoweredCoroId {
        let plan = self.error_plan();
        self.body.coros.push(LoweredCoro {
            origin: Origin::compiler(),
            plan,
            environment: LoweredClosureEnvironment::None,
        })
    }
}

/// Reconstructs the enclosing resolver view of one interned instance, matching
/// the Stage 3.3 worklist's `resolved_request`.
fn enclosing_request(
    program: &LoweredProgram,
    instance: FunctionInstanceId,
) -> ResolvedInstanceRequest {
    let record = program
        .instances
        .get(instance)
        .expect("instance body materialization starts from interned instances");
    let key = program
        .specializations
        .instance(record.ordinal)
        .expect("interned instance has a catalog key")
        .clone();
    let origin = record.request.origin(&record.origin);
    ResolvedInstanceRequest {
        key,
        environment: record.environment.clone(),
        relevant: record.relevant.clone(),
        evidence: record.evidence.clone(),
        origin,
    }
}

/// The trait a recipe selects, used by evidence-only sites.
fn evidence_trait_id(evidence: &TraitEvidence) -> crate::TraitId {
    match evidence {
        TraitEvidence::ExplicitImplementation { trait_id, .. }
        | TraitEvidence::Structural { trait_id, .. }
        | TraitEvidence::DeclaredBound { trait_id, .. }
        | TraitEvidence::RejectedImplementation { trait_id, .. } => *trait_id,
    }
}

impl<'a> BodyCloner<'a> {
    fn clone_call(&mut self, id: LoweredCallId) -> LoweredCallId {
        if let Some(new) = self.calls.get(&id) {
            return *new;
        }
        let Some(original) = self.program.calls.get(id).cloned() else {
            self.missing("call", id.index());
            return self.error_call();
        };
        let mut call = original.clone();
        call.callee = call.callee.map(|callee| self.clone_expression(callee));
        call.function_type = self.function_type(&call.function_type);
        for argument in &mut call.arguments {
            argument.expression = argument
                .expression
                .map(|expression| self.clone_expression(expression));
            argument.place = argument.place.map(|place| self.clone_place(place));
            argument.expected = self.ty(&argument.expected);
        }
        call.resource_bindings = call
            .resource_bindings
            .iter()
            .map(|use_| self.clone_resource_use(*use_))
            .collect();
        call.steps = call
            .steps
            .into_iter()
            .map(|step| match step {
                LoweredCallStep::Callee { expression } => LoweredCallStep::Callee {
                    expression: self.clone_expression(expression),
                },
                LoweredCallStep::Argument { argument } => LoweredCallStep::Argument { argument },
                LoweredCallStep::ProductElement {
                    argument,
                    slot,
                    expression,
                } => LoweredCallStep::ProductElement {
                    argument,
                    slot,
                    expression: self.clone_expression(expression),
                },
                LoweredCallStep::ProductSpread {
                    argument,
                    expression,
                    mappings,
                } => LoweredCallStep::ProductSpread {
                    argument,
                    expression: self.clone_expression(expression),
                    mappings,
                },
                LoweredCallStep::NamedProductSpread {
                    argument,
                    expression,
                    mappings,
                } => LoweredCallStep::NamedProductSpread {
                    argument,
                    expression: self.clone_expression(expression),
                    mappings,
                },
                LoweredCallStep::Default {
                    argument,
                    slot,
                    expression,
                    expected,
                } => LoweredCallStep::Default {
                    argument,
                    slot,
                    expression: self.clone_expression(expression),
                    expected: self.ty(&expected),
                },
                LoweredCallStep::Resource { resource } => LoweredCallStep::Resource { resource },
                LoweredCallStep::Invoke => LoweredCallStep::Invoke,
            })
            .collect();
        call.result_type = self.ty(&call.result_type);
        call.substitutions = self.substitutions(&call.substitutions);
        call.evidence = call
            .evidence
            .as_ref()
            .map(|evidence| self.evidence(evidence));
        call.reactive = call
            .reactive
            .map(|operation| self.clone_operation(operation));
        self.recompute_call_arguments(&mut call);
        self.rebind_hidden_resources(&mut call);
        let new = self.body.calls.push(call);
        self.calls.insert(id, new);
        for (index, argument) in original.arguments.iter().enumerate() {
            if argument.thunk.is_some() {
                self.bind_thunk(
                    LoweredBindingSite::CallArgumentThunk {
                        call: new,
                        argument: index,
                    },
                    argument.thunk,
                    &original.origin,
                    LoweredInstanceDependencyKind::ImplicitThunkArgument,
                );
            }
        }
        self.bind_call_target(new, &original);
        new
    }

    fn clone_callable_value(&mut self, id: LoweredCallableValueId) -> LoweredCallableValueId {
        if let Some(new) = self.callable_values.get(&id) {
            return *new;
        }
        let Some(original) = self.program.callable_values.get(id).cloned() else {
            self.missing("callable value", id.index());
            return error_callable_value(self);
        };
        let mut value = original.clone();
        value.function_type = self.function_type(&value.function_type);
        let concrete_function_type = value.function_type.clone();
        value.closure = value
            .closure
            .as_ref()
            .map(|closure| LoweredClosureConstruction {
                function: closure.function,
                captures: closure
                    .captures
                    .iter()
                    .map(|capture| {
                        let mut value_type = self.ty(&capture.value_type);
                        // A recursive closure captures its own generic function
                        // binding; the instance's concrete callable type is the
                        // authoritative capture type.
                        if contains_type_parameter(&value_type)
                            && self
                                .program
                                .symbols
                                .get(capture.capture.symbol)
                                .and_then(|symbol| symbol.function)
                                == Some(closure.function)
                        {
                            value_type = CheckedType::Function(concrete_function_type.clone());
                        }
                        let symbol = self.program.symbols.get(capture.capture.symbol);
                        LoweredClosureCapture {
                            capture: capture.capture.clone(),
                            drops_value: capture.owns_value
                                && self.program.concrete_needs_drop(&value_type),
                            value_type,
                            access: capture.access,
                            owns_value: capture.owns_value,
                            requires_initialization_state: symbol
                                .is_some_and(|symbol| symbol.requires_initialization_check),
                            mutable_storage: symbol.is_some_and(|symbol| symbol.mutable_storage),
                            derived: symbol.is_some_and(|symbol| symbol.derived),
                        }
                    })
                    .collect(),
                environment: closure.environment,
                adapter: closure.adapter,
                substitutions: self.substitutions(&closure.substitutions),
            });
        value.substitutions = self.substitutions(&value.substitutions);
        value.evidence = value
            .evidence
            .as_ref()
            .map(|evidence| self.evidence(evidence));
        let new = self.body.callable_values.push(value);
        self.callable_values.insert(id, new);
        self.bind_callable_target(new, &original);
        new
    }

    fn clone_resource_use(&mut self, id: LoweredResourceUseId) -> LoweredResourceUseId {
        if let Some(new) = self.resource_uses.get(&id) {
            return *new;
        }
        let Some(original) = self.program.resource_uses.get(id).cloned() else {
            self.missing("resource use", id.index());
            return self.error_use();
        };
        let mut use_ = original.clone();
        use_.resource = self.resource(&use_.resource);
        use_.provider = use_
            .provider
            .map(|provider| self.clone_resource_provider(provider));
        if let Some(provider) = use_
            .provider
            .and_then(|provider| self.body.resource_providers.get(provider))
        {
            use_.indirect = provider.indirect;
        }
        use_.pass_mode = match use_.kind {
            super::LoweredResourceUseKind::Read => super::LoweredArgumentPassMode::Value,
            super::LoweredResourceUseKind::MutablePlace => {
                super::LoweredArgumentPassMode::MutablePlace
            }
            super::LoweredResourceUseKind::HiddenArgument => {
                if use_.resource.mutable
                    || !self.program.concrete_is_copy(&use_.resource.value_type)
                {
                    super::LoweredArgumentPassMode::BorrowedPointer
                } else {
                    super::LoweredArgumentPassMode::Value
                }
            }
        };
        let new = self.body.resource_uses.push(use_);
        self.resource_uses.insert(id, new);
        new
    }

    fn clone_resource_provider(
        &mut self,
        id: LoweredResourceProviderId,
    ) -> LoweredResourceProviderId {
        if let Some(new) = self.resource_providers.get(&id) {
            return *new;
        }
        let Some(original) = self.program.resource_providers.get(id).cloned() else {
            self.missing("resource provider", id.index());
            return self.error_provider();
        };
        let parent = original
            .parent
            .map(|parent| self.clone_resource_provider(parent));
        let mut provider = original.clone();
        provider.resource = self.resource(&provider.resource);
        provider.parent = parent;
        provider.target = match provider.target {
            super::LoweredProviderTarget::Expression(expression) => {
                super::LoweredProviderTarget::Expression(self.clone_expression(expression))
            }
            other => other,
        };
        if provider.kind == super::LoweredProviderOriginKind::FunctionParameter {
            let indirect = provider.resource.mutable
                || !self.program.concrete_is_copy(&provider.resource.value_type);
            provider.indirect = indirect;
            provider.borrow = indirect;
        }
        let new = self.body.resource_providers.push(provider);
        self.resource_providers.insert(id, new);
        new
    }

    fn clone_with(&mut self, id: LoweredWithId) -> LoweredWithId {
        if let Some(new) = self.withs.get(&id) {
            return *new;
        }
        let Some(original) = self.program.withs.get(id).cloned() else {
            self.missing("with", id.index());
            return error_with(self);
        };
        let value = self.clone_expression(original.value);
        let provider = self.clone_resource_provider(original.provider);
        self.recompute_with_provider(provider, value);
        self.active_providers.push(provider);
        let body = self.clone_block(original.body);
        self.active_providers.pop();
        let new = self.body.withs.push(LoweredWith {
            origin: original.origin,
            provider,
            value,
            body,
            scope_exit: original.scope_exit,
        });
        self.withs.insert(id, new);
        new
    }

    fn clone_operation(&mut self, id: LoweredReactiveOperationId) -> LoweredReactiveOperationId {
        if let Some(new) = self.operations.get(&id) {
            return *new;
        }
        let Some(original) = self.program.reactive_operations.get(id).cloned() else {
            self.missing("reactive operation", id.index());
            return self.error_operation();
        };
        let callbacks = match &original.kind {
            LoweredReactiveOperationKind::Reaction { callback, .. } => Some(*callback),
            LoweredReactiveOperationKind::Until { predicate, .. } => Some(*predicate),
            LoweredReactiveOperationKind::Batch { callback } => Some(*callback),
            _ => None,
        };
        let mut operation = original.clone();
        match &mut operation.kind {
            LoweredReactiveOperationKind::DerivedCreate { function_type, .. } => {
                *function_type = self.function_type(function_type);
            }
            LoweredReactiveOperationKind::Reaction {
                reactive_provider, ..
            }
            | LoweredReactiveOperationKind::Until {
                reactive_provider, ..
            } => {
                *reactive_provider =
                    reactive_provider.map(|provider| self.clone_resource_provider(provider));
            }
            LoweredReactiveOperationKind::SignalCreate { .. }
            | LoweredReactiveOperationKind::SignalRead { .. }
            | LoweredReactiveOperationKind::SignalNotify { .. }
            | LoweredReactiveOperationKind::DerivedRead { .. }
            | LoweredReactiveOperationKind::Scope
            | LoweredReactiveOperationKind::Batch { .. }
            | LoweredReactiveOperationKind::Snapshot => {}
        }
        if let (Some(original_callback), LoweredReactiveOperationKind::Reaction { callback, .. }) =
            (callbacks, &mut operation.kind)
        {
            *callback = self.clone_callback(original_callback);
        }
        if let (Some(original_callback), LoweredReactiveOperationKind::Until { predicate, .. }) =
            (callbacks, &mut operation.kind)
        {
            *predicate = self.clone_callback(original_callback);
        }
        if let (Some(original_callback), LoweredReactiveOperationKind::Batch { callback }) =
            (callbacks, &mut operation.kind)
        {
            *callback = self.clone_callback(original_callback);
        }
        let new = self.body.reactive_operations.push(operation);
        self.operations.insert(id, new);
        if let LoweredReactiveOperationKind::DerivedCreate {
            evaluator,
            function_type,
            ..
        } = &original.kind
        {
            if let Some(instance) = self.request_function(
                *evaluator,
                &original.origin,
                function_type.clone(),
                CallSubstitutions::default(),
                None,
                false,
            ) {
                self.body.bindings.insert(
                    LoweredBindingSite::DerivedEvaluator(new),
                    LoweredBoundTarget::Instance(instance),
                );
            }
        }
        new
    }

    fn clone_callback(&mut self, id: LoweredReactiveCallbackId) -> LoweredReactiveCallbackId {
        if let Some(new) = self.callbacks.get(&id) {
            return *new;
        }
        let Some(original) = self.program.reactive_callbacks.get(id).cloned() else {
            self.missing("reactive callback", id.index());
            return self.error_callback();
        };
        let mut callback = original.clone();
        callback.callable = callback
            .callable
            .map(|callable| self.clone_expression(callable));
        callback.function_type = self.function_type(&callback.function_type);
        callback.resources = callback
            .resources
            .iter()
            .map(|use_| self.clone_resource_use(*use_))
            .collect();
        let new = self.body.reactive_callbacks.push(callback);
        self.callbacks.insert(id, new);
        self.bind_thunk(
            LoweredBindingSite::ReactiveCallback(new),
            original.thunk,
            &original.origin,
            LoweredInstanceDependencyKind::ReactiveCallback,
        );
        new
    }

    fn clone_plan(&mut self, id: LoweredCoroutinePlanId) {
        if self.plans.contains_key(&id) {
            return;
        }
        let Some(original) = self.program.coroutine_plans.get(id).cloned() else {
            self.missing("coroutine plan", id.index());
            return;
        };
        let local = self.body.plans.push(LoweredCoroutinePlan {
            origin: original.origin.clone(),
            body_syntax: original.body_syntax,
            body: None,
            thunk: original.thunk,
            captures: original.captures.clone(),
            result_type: self.ty(&original.result_type),
            deferred_effects: self.effects(&original.deferred_effects),
            resume_points: original.resume_points,
            frame_bindings: original.frame_bindings.clone(),
            await_result_types: original
                .await_result_types
                .iter()
                .map(|value| self.ty(value))
                .collect(),
            wait_await_states: original.wait_await_states.clone(),
            until_await_states: original.until_await_states.clone(),
            awaits: Vec::new(),
        });
        self.plans.insert(id, local);
        self.body.plan_template = Some(local);
        let awaits = original
            .awaits
            .iter()
            .map(|await_| self.clone_await(*await_))
            .collect::<Vec<_>>();
        let body = original.body.map(|block| self.clone_block(block));
        if let Some(plan) = self.body.plans.get_mut(local) {
            plan.awaits = awaits;
            plan.body = body;
        }
    }

    fn clone_coro(&mut self, id: LoweredCoroId) -> LoweredCoroId {
        if let Some(new) = self.coros.get(&id) {
            return *new;
        }
        let Some(original) = self.program.coros.get(id).cloned() else {
            self.missing("coro", id.index());
            return self.error_coro();
        };
        let new = self.body.coros.push(LoweredCoro {
            origin: original.origin.clone(),
            plan: original.plan,
            environment: original.environment,
        });
        self.coros.insert(id, new);
        if let Some(plan) = self.program.coroutine_plans.get(original.plan).cloned() {
            let Some(mut function_type) = self
                .program
                .functions
                .get(plan.thunk)
                .map(|function| function.signature.clone())
            else {
                self.diagnostics.push(Diagnostic::new(
                    original.origin.span.clone(),
                    "coroutine body thunk has no lowered template",
                ));
                return new;
            };
            function_type.effects = plan.deferred_effects.clone();
            if let Some(instance) = self.request_function(
                plan.thunk,
                &original.origin,
                function_type,
                CallSubstitutions::default(),
                None,
                false,
            ) {
                self.body.bindings.insert(
                    LoweredBindingSite::Coro(new),
                    LoweredBoundTarget::Instance(instance),
                );
            }
        }
        new
    }

    fn clone_await(&mut self, id: LoweredAwaitId) -> LoweredAwaitId {
        if let Some(new) = self.awaits.get(&id) {
            return *new;
        }
        let Some(original) = self.program.awaits.get(id).cloned() else {
            self.missing("await", id.index());
            return self.error_await();
        };
        let operand = self.clone_expression(original.operand);
        let child_plan = match &original.kind {
            LoweredAwaitKind::ChildCoroutine { plan, .. } => *plan,
            _ => None,
        };
        let deferred_resources = match &original.kind {
            LoweredAwaitKind::ChildCoroutine {
                deferred_resources, ..
            } => deferred_resources
                .iter()
                .map(|use_| self.clone_resource_use(*use_))
                .collect(),
            _ => Vec::new(),
        };
        let kind = match &original.kind {
            LoweredAwaitKind::ChildCoroutine {
                plan,
                child_result,
                until,
                ..
            } => LoweredAwaitKind::ChildCoroutine {
                plan: *plan,
                child_result: self.ty(child_result),
                deferred_resources,
                until: *until,
            },
            LoweredAwaitKind::Task { result } => LoweredAwaitKind::Task {
                result: self.ty(result),
            },
            LoweredAwaitKind::Wait { result } => LoweredAwaitKind::Wait {
                result: self.ty(result),
            },
        };
        let owning_plan = self.local_plan(original.owning_plan, &original.origin);
        let new = self.body.awaits.push(LoweredAwait {
            origin: original.origin.clone(),
            operand,
            result_type: self.ty(&original.result_type),
            owning_plan,
            resume_state: original.resume_state,
            kind,
        });
        self.awaits.insert(id, new);
        if let Some(plan) = child_plan {
            let owner = self
                .body
                .expressions
                .get(operand)
                .and_then(|expression| match &expression.kind {
                    LoweredExpressionKind::Coro(coro) => {
                        self.body.bindings.get(&LoweredBindingSite::Coro(*coro))
                    }
                    _ => None,
                })
                .and_then(LoweredBoundTarget::instance_id);
            match owner {
                Some(instance) => {
                    self.body.bindings.insert(
                        LoweredBindingSite::AwaitChildPlan(new),
                        LoweredBoundTarget::Instance(instance),
                    );
                }
                None => self.diagnostics.push(Diagnostic::new(
                    original.origin.span.clone(),
                    format!(
                        "await child plan {} is not bound by a coroutine creation in this body",
                        plan.index()
                    ),
                )),
            }
        }
        new
    }

    fn local_plan(
        &mut self,
        global: LoweredCoroutinePlanId,
        origin: &Origin,
    ) -> LoweredCoroutinePlanId {
        match self.plans.get(&global) {
            Some(local) => *local,
            None => {
                self.diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "await references plan {} outside its owning body",
                        global.index()
                    ),
                ));
                self.error_plan()
            }
        }
    }

    // ------------------------------------------------------------------
    // Binding sites to the Stage 3.3 graph.
    // ------------------------------------------------------------------

    fn request_function(
        &mut self,
        function: FunctionId,
        origin: &Origin,
        function_type: CheckedFunctionType,
        substitutions: CallSubstitutions,
        evidence: Option<TraitEvidence>,
        current: bool,
    ) -> Option<FunctionInstanceId> {
        let (function_type, substitutions, evidence) =
            if self.program.relevant_parameters(function).is_empty() {
                match self.program.functions.get(function) {
                    Some(template) => (
                        template.signature.clone(),
                        CallSubstitutions::default(),
                        None,
                    ),
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
            target: InstanceResolutionTarget::Nested(&self.enclosing),
        };
        let resolved = if current {
            let request = InstanceResolutionRequest {
                target: InstanceResolutionTarget::Current(&self.enclosing),
                ..request
            };
            match self.program.resolve_instance_request(&request) {
                Ok(resolved) => resolved,
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    return None;
                }
            }
        } else {
            match self.program.resolve_instance_request(&request) {
                Ok(resolved) => resolved,
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    return None;
                }
            }
        };
        match self.instances_by_key.get(&resolved.key) {
            Some(instance) => Some(*instance),
            None => {
                self.diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    format!(
                        "instance body requests a missing function instance for function {}",
                        function.0
                    ),
                ));
                None
            }
        }
    }

    fn bind_thunk(
        &mut self,
        site: LoweredBindingSite,
        thunk: Option<FunctionId>,
        origin: &Origin,
        _kind: LoweredInstanceDependencyKind,
    ) {
        let Some(function) = thunk else {
            return;
        };
        let Some(function_type) = self
            .program
            .functions
            .get(function)
            .map(|template| template.signature.clone())
        else {
            self.diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                format!("implicit thunk {} has no lowered template", function.0),
            ));
            return;
        };
        if let Some(instance) = self.request_function(
            function,
            origin,
            function_type,
            CallSubstitutions::default(),
            None,
            false,
        ) {
            self.body
                .bindings
                .insert(site, LoweredBoundTarget::Instance(instance));
        }
    }

    fn bind_formatting_helpers(
        &mut self,
        template: ExpressionId,
        has_literal: bool,
        origin: &Origin,
    ) {
        // Literal parts go through `Formatter.write`; a template with no
        // literal part never calls it and records no instance edge.
        let write = if has_literal {
            self.program.string_formatting.write
        } else {
            None
        };
        for (function, site) in [
            (
                self.program.string_formatting.constructor,
                LoweredBindingSite::FormattingConstructor(template),
            ),
            (write, LoweredBindingSite::FormattingWrite(template)),
            (
                self.program.string_formatting.finish,
                LoweredBindingSite::FormattingFinish(template),
            ),
        ] {
            let Some(function) = function else {
                continue;
            };
            let Some(function_type) = self
                .program
                .functions
                .get(function)
                .map(|template| template.signature.clone())
            else {
                self.diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    format!("formatter helper {} has no lowered template", function.0),
                ));
                continue;
            };
            if let Some(instance) = self.request_function(
                function,
                origin,
                function_type,
                CallSubstitutions::default(),
                None,
                false,
            ) {
                self.body
                    .bindings
                    .insert(site, LoweredBoundTarget::Instance(instance));
            }
        }
    }

    fn bind_call_target(&mut self, site_id: LoweredCallId, original: &LoweredCall) {
        let site = LoweredBindingSite::Call(site_id);
        match &original.target {
            LoweredCallableTarget::DirectFunction {
                function,
                environment,
            } => {
                let current = *environment == LoweredCallEnvironment::Current;
                if let Some(instance) = self.request_function(
                    *function,
                    &original.origin,
                    original.function_type.clone(),
                    original.substitutions.clone(),
                    original.evidence.clone(),
                    current,
                ) {
                    self.body
                        .bindings
                        .insert(site, LoweredBoundTarget::Instance(instance));
                }
            }
            LoweredCallableTarget::TraitImplementation {
                trait_id, method, ..
            }
            | LoweredCallableTarget::StructuralTraitMethod {
                trait_id, method, ..
            } => {
                let Some(evidence) = &original.evidence else {
                    self.diagnostics.push(Diagnostic::new(
                        original.origin.span.clone(),
                        "trait call has no evidence recipe",
                    ));
                    return;
                };
                self.bind_trait_site(
                    site,
                    &original.origin,
                    *trait_id,
                    *method,
                    evidence,
                    Some(original.function_type.clone()),
                    original.substitutions.clone(),
                );
            }
            LoweredCallableTarget::CompilerHelper { function } => {
                self.diagnostics.push(Diagnostic::new(
                    original.origin.span.clone(),
                    format!(
                        "compiler-helper target function {} has no generated artifact",
                        function.0
                    ),
                ));
            }
            LoweredCallableTarget::IndirectClosure { .. }
            | LoweredCallableTarget::ExternalFunction { .. }
            | LoweredCallableTarget::Intrinsic { .. }
            | LoweredCallableTarget::Constructor { .. } => {
                self.body
                    .bindings
                    .insert(site, LoweredBoundTarget::Route(original.target.category()));
            }
        }
    }

    fn bind_callable_target(
        &mut self,
        site_id: LoweredCallableValueId,
        original: &LoweredCallableValue,
    ) {
        let site = LoweredBindingSite::CallableValue(site_id);
        match &original.target {
            LoweredCallableTarget::DirectFunction {
                function,
                environment,
            } => {
                let current = *environment == LoweredCallEnvironment::Current
                    || original.closure.as_ref().is_some_and(|closure| {
                        closure.environment == LoweredClosureEnvironment::Current
                    });
                if let Some(instance) = self.request_function(
                    *function,
                    &original.origin,
                    original.function_type.clone(),
                    original.substitutions.clone(),
                    original.evidence.clone(),
                    current,
                ) {
                    self.body
                        .bindings
                        .insert(site, LoweredBoundTarget::Instance(instance));
                }
            }
            LoweredCallableTarget::TraitImplementation {
                trait_id, method, ..
            }
            | LoweredCallableTarget::StructuralTraitMethod {
                trait_id, method, ..
            } => {
                let Some(evidence) = &original.evidence else {
                    self.diagnostics.push(Diagnostic::new(
                        original.origin.span.clone(),
                        "trait-method value has no evidence recipe",
                    ));
                    return;
                };
                self.bind_trait_site(
                    site,
                    &original.origin,
                    *trait_id,
                    *method,
                    evidence,
                    Some(original.function_type.clone()),
                    original.substitutions.clone(),
                );
            }
            LoweredCallableTarget::Constructor {
                symbol, type_id, ..
            } => {
                self.bind_constructor_adapter(
                    site,
                    *symbol,
                    *type_id,
                    original.adapter,
                    &original.function_type,
                    &original.origin,
                );
            }
            LoweredCallableTarget::CompilerHelper { function } => {
                self.diagnostics.push(Diagnostic::new(
                    original.origin.span.clone(),
                    format!(
                        "compiler-helper target function {} has no generated artifact",
                        function.0
                    ),
                ));
            }
            LoweredCallableTarget::IndirectClosure { .. }
            | LoweredCallableTarget::ExternalFunction { .. }
            | LoweredCallableTarget::Intrinsic { .. } => {
                self.body
                    .bindings
                    .insert(site, LoweredBoundTarget::Route(original.target.category()));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_trait_site(
        &mut self,
        site: LoweredBindingSite,
        origin: &Origin,
        trait_id: crate::TraitId,
        method: crate::TraitMethodId,
        evidence: &TraitEvidence,
        recorded_type: Option<CheckedFunctionType>,
        substitutions: CallSubstitutions,
    ) {
        let site_environment = match self.program.site_environment(
            origin,
            &substitutions,
            Some(&self.enclosing.environment),
        ) {
            Ok(environment) => environment,
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                return;
            }
        };
        let resolved =
            match self
                .program
                .resolve_trait_evidence(origin, Some(evidence), &site_environment)
            {
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
                    None => match instantiate_method_type(
                        self.program,
                        origin,
                        trait_id,
                        method,
                        arguments,
                    ) {
                        Ok(function_type) => function_type,
                        Err(diagnostic) => {
                            self.diagnostics.push(diagnostic);
                            return;
                        }
                    },
                };
                let evidence = if self.program.relevant_parameters(*function).is_empty() {
                    None
                } else {
                    Some(resolved.clone())
                };
                if let Some(instance) = self.request_function(
                    *function,
                    origin,
                    function_type,
                    substitutions,
                    evidence,
                    false,
                ) {
                    self.store_resolved_evidence(site, &resolved);
                    self.body
                        .bindings
                        .insert(site, LoweredBoundTarget::Instance(instance));
                }
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
                            self.program,
                            origin,
                            *trait_id,
                            *method,
                            arguments,
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
                    Ok(key) => {
                        let key = ArtifactRequestKey::StructuralMethod(key);
                        match self.artifacts_by_key.get(&key) {
                            Some(ordinal) => {
                                self.store_resolved_evidence(site, &resolved);
                                self.body
                                    .bindings
                                    .insert(site, LoweredBoundTarget::Artifact(*ordinal));
                            }
                            None => self.diagnostics.push(Diagnostic::new(
                                origin.span.clone(),
                                "instance body requests a structural method Stage 3.3 did not reserve",
                            )),
                        }
                    }
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

    /// Stores the concrete selection in the body's evidence table and folds it
    /// back onto the site node, so a resolved instance body never retains a
    /// deferred `DeclaredBound` recipe as its dispatch representation.
    fn store_resolved_evidence(&mut self, site: LoweredBindingSite, resolved: &TraitEvidence) {
        self.body.evidence.insert(site, resolved.clone());
        match site {
            LoweredBindingSite::Call(id) => {
                if let Some(call) = self.body.calls.get_mut(id) {
                    call.evidence = Some(resolved.clone());
                }
            }
            LoweredBindingSite::CallableValue(id) => {
                if let Some(value) = self.body.callable_values.get_mut(id) {
                    value.evidence = Some(resolved.clone());
                }
            }
            LoweredBindingSite::Index(id) => {
                if let Some(expression) = self.body.expressions.get_mut(id)
                    && let LoweredExpressionKind::Index(index) = &mut expression.kind
                {
                    index.evidence = resolved.clone();
                }
            }
            LoweredBindingSite::Interpolation { template, part } => {
                if let Some(expression) = self.body.expressions.get_mut(template)
                    && let LoweredExpressionKind::StringTemplate(template) = &mut expression.kind
                    && let Some(LoweredStringTemplatePart::Interpolation(interpolation)) =
                        template.parts.get_mut(part)
                {
                    interpolation.evidence = resolved.clone();
                }
            }
            LoweredBindingSite::IndexedAssignment(id) => {
                if let Some(item) = self.body.items.get_mut(id)
                    && let LoweredItemKind::Assignment(assignment) = &mut item.kind
                {
                    assignment.evidence = Some(resolved.clone());
                }
            }
            _ => {}
        }
    }

    fn bind_constructor_adapter(
        &mut self,
        site: LoweredBindingSite,
        symbol: SymbolId,
        type_id: TypeId,
        adapter: super::LoweredCallableAdapter,
        callable_type: &CheckedFunctionType,
        origin: &Origin,
    ) {
        let concrete = match concretize_function_type(
            callable_type,
            Some(&self.enclosing.environment),
            origin,
        ) {
            Ok(concrete) => concrete,
            Err(diagnostic) => {
                self.diagnostics.push(diagnostic);
                return;
            }
        };
        match ConstructorAdapterKey::new(symbol, type_id, adapter, &concrete, origin) {
            Ok(key) => {
                let key = ArtifactRequestKey::ConstructorAdapter(key);
                match self.artifacts_by_key.get(&key) {
                    Some(ordinal) => {
                        self.body
                            .bindings
                            .insert(site, LoweredBoundTarget::Artifact(*ordinal));
                    }
                    None => self.diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "instance body requests a constructor adapter Stage 3.3 did not reserve",
                    )),
                }
            }
            Err(diagnostic) => self.diagnostics.push(diagnostic),
        }
    }

    fn error_call(&mut self) -> LoweredCallId {
        let callee = self.error_expression();
        self.body.calls.push(LoweredCall {
            origin: Origin::compiler(),
            target: LoweredCallableTarget::IndirectClosure { callee },
            callee: Some(callee),
            function_type: self.error_signature(),
            arguments: Vec::new(),
            resource_bindings: Vec::new(),
            mutations: Vec::new(),
            moves: Vec::new(),
            initialization_checks: Vec::new(),
            steps: Vec::new(),
            result_type: CheckedType::Error,
            substitutions: CallSubstitutions::default(),
            evidence: None,
            c_string_temporary: false,
            reactive: None,
        })
    }

    fn error_await(&mut self) -> LoweredAwaitId {
        let operand = self.error_expression();
        let plan = self.error_plan();
        self.body.awaits.push(LoweredAwait {
            origin: Origin::compiler(),
            operand,
            result_type: CheckedType::Error,
            owning_plan: plan,
            resume_state: 0,
            kind: LoweredAwaitKind::Wait {
                result: CheckedType::Error,
            },
        })
    }
}

/// Small fallbacks for arena families whose entries must stay in range even
/// while diagnostics are already recorded.
fn error_callable_value(cloner: &mut BodyCloner<'_>) -> LoweredCallableValueId {
    let callee = cloner.error_expression();
    let function_type = cloner.error_signature();
    cloner.body.callable_values.push(LoweredCallableValue {
        origin: Origin::compiler(),
        target: LoweredCallableTarget::IndirectClosure { callee },
        function_type,
        adapter: super::LoweredCallableAdapter::None,
        closure: None,
        substitutions: CallSubstitutions::default(),
        evidence: None,
        requires_initialization_check: false,
    })
}

fn error_with(cloner: &mut BodyCloner<'_>) -> LoweredWithId {
    let provider = cloner.error_provider();
    let value = cloner.error_expression();
    let body = cloner.error_block();
    cloner.body.withs.push(LoweredWith {
        origin: Origin::compiler(),
        provider,
        value,
        body,
        scope_exit: super::LoweredScopeExit::Ordinary,
    })
}

/// Walks one instance body and verifies its local invariants: every local ID
/// exists, every checked value is concrete, every arena node is reachable, and
/// every bound target agrees with the recorded Stage 3.3 graph.
struct BodyValidator<'a> {
    program: &'a LoweredProgram,
    owner: FunctionInstanceId,
    instance: &'a super::LoweredFunctionInstance,
    body: &'a LoweredInstanceBody,
    diagnostics: Vec<Diagnostic>,
    blocks: HashSet<BlockId>,
    items: HashSet<ItemId>,
    expressions: HashSet<ExpressionId>,
    patterns: HashSet<PatternId>,
    places: HashSet<PlaceId>,
    calls: HashSet<LoweredCallId>,
    callable_values: HashSet<LoweredCallableValueId>,
    providers: HashSet<LoweredResourceProviderId>,
    uses: HashSet<LoweredResourceUseId>,
    withs: HashSet<LoweredWithId>,
    operations: HashSet<LoweredReactiveOperationId>,
    callbacks: HashSet<LoweredReactiveCallbackId>,
    plan_visits: HashSet<LoweredCoroutinePlanId>,
    coros: HashSet<LoweredCoroId>,
    awaits: HashSet<LoweredAwaitId>,
    unresolved: bool,
}

impl<'a> BodyValidator<'a> {
    fn new(
        program: &'a LoweredProgram,
        owner: FunctionInstanceId,
        instance: &'a super::LoweredFunctionInstance,
        body: &'a LoweredInstanceBody,
    ) -> Self {
        BodyValidator {
            program,
            owner,
            instance,
            body,
            diagnostics: Vec::new(),
            blocks: HashSet::new(),
            items: HashSet::new(),
            expressions: HashSet::new(),
            patterns: HashSet::new(),
            places: HashSet::new(),
            calls: HashSet::new(),
            callable_values: HashSet::new(),
            providers: HashSet::new(),
            uses: HashSet::new(),
            withs: HashSet::new(),
            operations: HashSet::new(),
            callbacks: HashSet::new(),
            plan_visits: HashSet::new(),
            coros: HashSet::new(),
            awaits: HashSet::new(),
            unresolved: false,
        }
    }

    fn run(&mut self) {
        if self.body.template != self.instance.template {
            self.report(
                self.body.origin.span.clone(),
                "instance body template disagrees with its instance",
            );
        }
        if let Some(template) = self.program.functions.get(self.instance.template) {
            let template_parameters = template.parameters.iter().copied().collect::<Vec<_>>();
            let body_parameters = self
                .body
                .parameters
                .iter()
                .map(|parameter| parameter.symbol)
                .collect::<Vec<_>>();
            if template_parameters != body_parameters {
                self.report(
                    self.body.origin.span.clone(),
                    "instance body parameter order disagrees with its template",
                );
            }
            let template_captures = template
                .captures
                .iter()
                .map(|capture| {
                    (
                        capture.symbol,
                        capture.borrowed,
                        capture.non_owning,
                        capture.requires_cell,
                    )
                })
                .collect::<Vec<_>>();
            let body_captures = self
                .body
                .captures
                .iter()
                .map(|capture| {
                    (
                        capture.capture.symbol,
                        capture.capture.borrowed,
                        capture.capture.non_owning,
                        capture.capture.requires_cell,
                    )
                })
                .collect::<Vec<_>>();
            if template_captures != body_captures {
                self.report(
                    self.body.origin.span.clone(),
                    "instance body capture order disagrees with its template",
                );
            }
            if template.body.is_some() != self.body.root.is_some() {
                self.report(
                    self.body.origin.span.clone(),
                    "instance body root presence disagrees with its template",
                );
            }
        }
        self.check_concrete_type(
            &self.body.origin,
            &CheckedType::Function(self.body.signature.clone()),
            "signature",
        );
        for bound in &self.body.bounds {
            self.check_bound(&self.body.origin, bound);
        }
        for parameter in &self.body.parameters {
            self.check_concrete_type(&self.body.origin, &parameter.value_type, "parameter type");
        }
        for capture in &self.body.captures {
            self.check_concrete_type(&self.body.origin, &capture.value_type, "capture type");
        }
        if self
            .body
            .patterns
            .get(self.body.parameter_pattern)
            .is_none()
        {
            self.report(
                self.body.origin.span.clone(),
                "instance body parameter pattern is missing",
            );
        } else {
            self.visit_pattern(self.body.parameter_pattern);
        }
        if let Some(root) = self.body.root {
            self.visit_block(root);
        } else if self
            .program
            .functions
            .get(self.instance.template)
            .is_some_and(|function| function.body.is_some())
        {
            self.report(
                self.instance.origin.span.clone(),
                "instance body has no root for a template with a body",
            );
        }
        for provider in &self.body.function_providers {
            self.visit_provider(*provider);
        }
        if let Some(plan) = self.body.plan_template {
            self.visit_plan(plan);
        }
        self.check_arena_density();
        self.check_bindings();
    }

    fn report(&mut self, span: Span, message: impl Into<String>) {
        self.unresolved = true;
        self.diagnostics.push(Diagnostic::new(span, message.into()));
    }

    fn check_concrete_type(&mut self, origin: &Origin, value_type: &CheckedType, what: &str) {
        if contains_type_parameter(value_type) {
            self.report(
                origin.span.clone(),
                format!(
                    "instance body {what} is not concrete: its value still contains a declared parameter: {value_type}"
                ),
            );
        }
    }

    fn check_effects(&mut self, origin: &Origin, effects: &CheckedEffectSet, what: &str) {
        if let Some(variable) = &effects.variable {
            self.report(
                origin.span.clone(),
                format!(
                    "instance body {what} still names effect variable `{}`",
                    variable.name
                ),
            );
        }
        for resource in &effects.resources {
            self.check_concrete_type(origin, &resource.value_type, what);
        }
        if effects.state.is_some() {
            // State effects are concrete markers with no parameter payload.
        }
    }

    fn check_bound(&mut self, origin: &Origin, bound: &CheckedTraitBound) {
        for argument in &bound.arguments {
            self.check_concrete_type(origin, argument, "trait bound argument");
        }
    }

    fn visit_block(&mut self, id: BlockId) {
        if !self.blocks.insert(id) {
            return;
        }
        let Some(block) = self.body.blocks.get(id) else {
            self.report(
                self.instance.origin.span.clone(),
                format!("instance body references missing block {}", id.index()),
            );
            return;
        };
        for item in &block.items {
            if !self.body.items.contains(*item) {
                self.report(
                    block.origin.span.clone(),
                    format!("block references missing item {}", item.index()),
                );
                continue;
            }
            self.visit_item(*item);
        }
        if let Some(result) = block.result {
            self.visit_expression(result);
        }
    }

    fn visit_item(&mut self, id: ItemId) {
        if !self.items.insert(id) {
            return;
        }
        let Some(item) = self.body.items.get(id) else {
            return;
        };
        match &item.kind {
            LoweredItemKind::Binding(binding) => {
                if let Some(value) = binding.value {
                    self.visit_expression(value);
                }
                if let Some(operation) = binding.reactive {
                    self.visit_operation(operation);
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                self.visit_pattern(binding.pattern);
                self.visit_expression(binding.value);
                if let Some(propagation) = &binding.propagation {
                    self.check_concrete_type(
                        &item.origin,
                        &propagation.source,
                        "propagation source",
                    );
                    self.check_concrete_type(
                        &item.origin,
                        &propagation.result,
                        "propagation result",
                    );
                }
            }
            LoweredItemKind::Assignment(assignment) => {
                self.visit_place(assignment.target);
                self.visit_expression(assignment.value);
                if let Some(dispatch) = &assignment.mutate_index {
                    for argument in &dispatch.arguments {
                        self.check_concrete_type(
                            &item.origin,
                            argument,
                            "mutation dispatch argument",
                        );
                    }
                }
                if let Some(operation) = assignment.signal_notify {
                    self.visit_operation(operation);
                }
            }
            LoweredItemKind::Return(item) => self.visit_expression(item.value),
            LoweredItemKind::Break(item) => {
                if let Some(value) = item.value {
                    self.visit_expression(value);
                }
            }
            LoweredItemKind::Continue(_) => {}
            LoweredItemKind::Expression(item) => self.visit_expression(item.expression),
        }
    }

    fn visit_expression(&mut self, id: ExpressionId) {
        if !self.expressions.insert(id) {
            return;
        }
        let Some(expression) = self.body.expressions.get(id) else {
            return;
        };
        let origin = expression.origin.clone();
        self.check_concrete_type(&origin, &expression.value_type, "expression type");
        self.check_effects(&origin, &expression.effects, "expression effects");
        if let Some(coercion) = &expression.coercion {
            self.check_concrete_type(&origin, &coercion.source, "coercion source");
            self.check_concrete_type(&origin, &coercion.target, "coercion target");
        }
        match &expression.kind {
            LoweredExpressionKind::Deferred(_) | LoweredExpressionKind::Stage26Deferred(_) => {
                self.report(
                    origin.span.clone(),
                    "instance body retains a template-only deferred expression",
                );
            }
            LoweredExpressionKind::Block(block) => self.visit_block(*block),
            LoweredExpressionKind::Name(name) => {
                if let Some(operation) = name.reactive {
                    self.visit_operation(operation);
                }
            }
            LoweredExpressionKind::Integer(_)
            | LoweredExpressionKind::Float(_)
            | LoweredExpressionKind::String(_)
            | LoweredExpressionKind::CString(_) => {}
            LoweredExpressionKind::Access(access) => {
                self.visit_expression(access.base);
                match &access.kind {
                    super::LoweredAccessKind::Representation { dereference }
                    | super::LoweredAccessKind::Product { dereference, .. }
                    | super::LoweredAccessKind::Slice { dereference, .. }
                    | super::LoweredAccessKind::Scalar { dereference } => {
                        for value_type in dereference {
                            self.check_concrete_type(&origin, value_type, "dereference type");
                        }
                    }
                }
            }
            LoweredExpressionKind::Product(product) => {
                for element in &product.final_type.elements {
                    self.check_concrete_type(&origin, &element.value_type, "product element");
                }
                for step in &product.steps {
                    match step {
                        super::LoweredProductStep::Positional { expression, .. }
                        | super::LoweredProductStep::Designated { expression, .. }
                        | super::LoweredProductStep::PositionalSpread { expression, .. }
                        | super::LoweredProductStep::NamedSpread { expression, .. } => {
                            self.visit_expression(*expression);
                        }
                        super::LoweredProductStep::Default {
                            expression,
                            expected,
                            ..
                        } => {
                            self.visit_expression(*expression);
                            self.check_concrete_type(&origin, expected, "default expected type");
                        }
                    }
                }
                for field in &product.fields {
                    self.visit_expression(*field);
                }
            }
            LoweredExpressionKind::RepeatedProduct(product) => {
                self.visit_expression(product.expression);
                if let super::LoweredRepeatCount::Symbolic(value_type) = &product.count {
                    self.check_concrete_type(&origin, value_type, "repeat count");
                }
            }
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.visit_expression(satisfies.value);
            }
            LoweredExpressionKind::Logical(logical) => {
                self.check_concrete_type(&origin, &logical.bool_type, "logical type");
                self.visit_expression(logical.left);
                self.visit_expression(logical.right);
            }
            LoweredExpressionKind::Loop(loop_) => {
                self.check_concrete_type(&origin, &loop_.result_type, "loop result");
                self.visit_block(loop_.body);
            }
            LoweredExpressionKind::Match(match_) => {
                self.check_concrete_type(&origin, &match_.source, "match subject");
                self.visit_expression(match_.subject);
                for arm in &match_.arms {
                    self.visit_pattern(arm.pattern);
                    self.visit_expression(arm.body);
                }
            }
            LoweredExpressionKind::Index(index) => {
                self.visit_expression(index.base);
                self.visit_expression(index.index);
                for argument in &index.arguments {
                    self.check_concrete_type(&origin, argument, "index argument");
                }
                if let Some(method_type) = &index.method_type {
                    self.check_concrete_type(
                        &origin,
                        &CheckedType::Function(method_type.clone()),
                        "index method type",
                    );
                }
                self.check_effects_of_evidence(&origin, &index.evidence);
            }
            LoweredExpressionKind::StringTemplate(template) => {
                for part in &template.parts {
                    if let LoweredStringTemplatePart::Interpolation(interpolation) = part {
                        self.check_concrete_type(
                            &origin,
                            &interpolation.value_type,
                            "interpolation value",
                        );
                        self.check_effects_of_evidence(&origin, &interpolation.evidence);
                        self.visit_expression(interpolation.expression);
                    }
                }
            }
            LoweredExpressionKind::Call(call) => self.visit_call(*call),
            LoweredExpressionKind::CallableValue(value) => {
                self.visit_callable_value(*value);
            }
            LoweredExpressionKind::Resource(use_) => self.visit_use(*use_),
            LoweredExpressionKind::With(with) => {
                self.visit_with(*with);
            }
            LoweredExpressionKind::Coro(coro) => {
                self.visit_coro(*coro);
            }
            LoweredExpressionKind::Await(await_) => {
                self.visit_await(*await_);
            }
        }
    }

    fn check_effects_of_evidence(&mut self, origin: &Origin, evidence: &TraitEvidence) {
        match evidence {
            TraitEvidence::ExplicitImplementation { arguments, .. }
            | TraitEvidence::Structural { arguments, .. }
            | TraitEvidence::RejectedImplementation { arguments, .. } => {
                for argument in arguments {
                    self.check_concrete_type(origin, argument, "evidence argument");
                }
            }
            TraitEvidence::DeclaredBound {
                arguments,
                prerequisites,
                ..
            } => {
                for argument in arguments {
                    self.check_concrete_type(origin, argument, "evidence argument");
                }
                for bound in prerequisites {
                    self.check_bound(origin, bound);
                }
            }
        }
    }

    fn visit_pattern(&mut self, id: PatternId) {
        if !self.patterns.insert(id) {
            return;
        }
        let Some(pattern) = self.body.patterns.get(id) else {
            return;
        };
        self.check_concrete_type(&pattern.origin, &pattern.value_type, "pattern type");
        match &pattern.kind {
            LoweredPatternKind::Wildcard
            | LoweredPatternKind::Binding { .. }
            | LoweredPatternKind::Literal { .. } => {}
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.visit_pattern(*element);
                }
            }
            LoweredPatternKind::Nominal { argument, .. } => self.visit_pattern(*argument),
            LoweredPatternKind::At { binding, pattern } => {
                self.visit_pattern(*binding);
                self.visit_pattern(*pattern);
            }
        }
    }

    fn visit_place(&mut self, id: PlaceId) {
        if !self.places.insert(id) {
            return;
        }
        let Some(place) = self.body.places.get(id) else {
            return;
        };
        self.check_concrete_type(&place.origin, &place.value_type, "place type");
        match &place.kind {
            LoweredPlaceKind::Symbol { .. } | LoweredPlaceKind::CapturedCell { .. } => {}
            LoweredPlaceKind::Temporary { expression } => self.visit_expression(*expression),
            LoweredPlaceKind::Resource { use_ } => self.visit_use(*use_),
            LoweredPlaceKind::Dereference {
                reference,
                dereference,
            } => {
                for value_type in dereference {
                    self.check_concrete_type(&place.origin, value_type, "dereference type");
                }
                self.visit_expression(*reference);
            }
            LoweredPlaceKind::ProductElement { base, .. }
            | LoweredPlaceKind::Representation { base } => self.visit_place(*base),
            LoweredPlaceKind::Indexed { base, index } => {
                self.visit_place(*base);
                self.visit_expression(*index);
            }
        }
    }

    fn visit_call(&mut self, id: LoweredCallId) {
        if !self.calls.insert(id) {
            return;
        }
        let Some(call) = self.body.calls.get(id) else {
            return;
        };
        let origin = call.origin.clone();
        self.check_concrete_type(
            &origin,
            &CheckedType::Function(call.function_type.clone()),
            "call function type",
        );
        self.check_concrete_type(&origin, &call.result_type, "call result type");
        if let Some(evidence) = &call.evidence {
            self.check_effects_of_evidence(&origin, evidence);
        }
        if let Some(callee) = call.callee {
            self.visit_expression(callee);
        }
        for argument in &call.arguments {
            self.check_concrete_type(&origin, &argument.expected, "argument expected type");
            if let Some(expression) = argument.expression {
                self.visit_expression(expression);
            }
            if let Some(place) = argument.place {
                self.visit_place(place);
            }
        }
        for use_ in &call.resource_bindings {
            self.visit_use(*use_);
        }
        for step in &call.steps {
            match step {
                LoweredCallStep::Callee { expression } => self.visit_expression(*expression),
                LoweredCallStep::Argument { .. }
                | LoweredCallStep::Resource { .. }
                | LoweredCallStep::Invoke => {}
                LoweredCallStep::ProductElement { expression, .. }
                | LoweredCallStep::ProductSpread { expression, .. }
                | LoweredCallStep::NamedProductSpread { expression, .. } => {
                    self.visit_expression(*expression);
                }
                LoweredCallStep::Default {
                    expression,
                    expected,
                    ..
                } => {
                    self.check_concrete_type(&origin, expected, "default expected type");
                    self.visit_expression(*expression);
                }
            }
        }
        let passes_hidden = !matches!(
            call.target.category(),
            LoweredCallableCategory::ExternalFunction
                | LoweredCallableCategory::Intrinsic
                | LoweredCallableCategory::Constructor
        );
        if passes_hidden
            && call.resource_bindings.len() != call.function_type.effects.resources.len()
        {
            self.report(
                origin.span.clone(),
                format!(
                    "call hidden-resource bindings disagree with the concrete effect row ({} bindings for {} resources, category {:?})",
                    call.resource_bindings.len(),
                    call.function_type.effects.resources.len(),
                    call.target.category()
                ),
            );
        }
        for (id, required) in call
            .resource_bindings
            .iter()
            .zip(&call.function_type.effects.resources)
        {
            if self
                .body
                .resource_uses
                .get(*id)
                .is_some_and(|use_| use_.resource != *required)
            {
                self.report(
                    origin.span.clone(),
                    "call hidden-resource binding order disagrees with the concrete effect row",
                );
            }
        }
        if let Some(operation) = call.reactive {
            self.visit_operation(operation);
        }
    }

    fn visit_callable_value(&mut self, id: LoweredCallableValueId) {
        if !self.callable_values.insert(id) {
            return;
        }
        let Some(value) = self.body.callable_values.get(id) else {
            return;
        };
        self.check_concrete_type(
            &value.origin,
            &CheckedType::Function(value.function_type.clone()),
            "callable function type",
        );
        if let Some(closure) = &value.closure {
            for capture in &closure.captures {
                self.check_concrete_type(&value.origin, &capture.value_type, "capture type");
            }
        }
        if let Some(evidence) = &value.evidence {
            self.check_effects_of_evidence(&value.origin, evidence);
        }
    }

    fn visit_use(&mut self, id: LoweredResourceUseId) {
        if !self.uses.insert(id) {
            return;
        }
        let Some(use_) = self.body.resource_uses.get(id).cloned() else {
            return;
        };
        self.check_concrete_type(&use_.origin, &use_.resource.value_type, "resource type");
        let expected = match use_.kind {
            super::LoweredResourceUseKind::Read => super::LoweredArgumentPassMode::Value,
            super::LoweredResourceUseKind::MutablePlace => {
                super::LoweredArgumentPassMode::MutablePlace
            }
            super::LoweredResourceUseKind::HiddenArgument => {
                if use_.resource.mutable
                    || !self.program.concrete_is_copy(&use_.resource.value_type)
                {
                    super::LoweredArgumentPassMode::BorrowedPointer
                } else {
                    super::LoweredArgumentPassMode::Value
                }
            }
        };
        if use_.pass_mode != expected {
            self.report(
                use_.origin.span.clone(),
                "resource use pass mode disagrees with its concrete type",
            );
        }
        if let Some(provider) = use_.provider {
            if let Some(provider_record) = self.body.resource_providers.get(provider) {
                if use_.indirect != provider_record.indirect {
                    self.report(
                        use_.origin.span.clone(),
                        "resource use indirectness disagrees with its concrete provider",
                    );
                }
            }
            self.visit_provider(provider);
        }
    }

    fn visit_provider(&mut self, id: LoweredResourceProviderId) {
        if !self.providers.insert(id) {
            return;
        }
        let Some(provider) = self.body.resource_providers.get(id) else {
            return;
        };
        self.check_concrete_type(
            &provider.origin,
            &provider.resource.value_type,
            "resource type",
        );
        if let Some(parent) = provider.parent {
            self.visit_provider(parent);
        }
        if let super::LoweredProviderTarget::Expression(expression) = provider.target {
            self.visit_expression(expression);
        }
    }

    fn visit_with(&mut self, id: LoweredWithId) {
        if !self.withs.insert(id) {
            return;
        }
        let Some(with) = self.body.withs.get(id) else {
            return;
        };
        self.visit_provider(with.provider);
        self.visit_expression(with.value);
        self.visit_block(with.body);
    }

    fn visit_operation(&mut self, id: LoweredReactiveOperationId) {
        if !self.operations.insert(id) {
            return;
        }
        let Some(operation) = self.body.reactive_operations.get(id) else {
            return;
        };
        match &operation.kind {
            LoweredReactiveOperationKind::DerivedCreate { function_type, .. } => {
                self.check_concrete_type(
                    &operation.origin,
                    &CheckedType::Function(function_type.clone()),
                    "derived evaluator type",
                );
            }
            LoweredReactiveOperationKind::Reaction {
                callback,
                reactive_provider,
            }
            | LoweredReactiveOperationKind::Until {
                predicate: callback,
                reactive_provider,
            } => {
                self.visit_callback(*callback);
                if let Some(provider) = reactive_provider {
                    self.visit_provider(*provider);
                }
            }
            LoweredReactiveOperationKind::Batch { callback } => self.visit_callback(*callback),
            LoweredReactiveOperationKind::SignalCreate { .. }
            | LoweredReactiveOperationKind::SignalRead { .. }
            | LoweredReactiveOperationKind::SignalNotify { .. }
            | LoweredReactiveOperationKind::DerivedRead { .. }
            | LoweredReactiveOperationKind::Scope
            | LoweredReactiveOperationKind::Snapshot => {}
        }
    }

    fn visit_callback(&mut self, id: LoweredReactiveCallbackId) {
        if !self.callbacks.insert(id) {
            return;
        }
        let Some(callback) = self.body.reactive_callbacks.get(id) else {
            return;
        };
        self.check_concrete_type(
            &callback.origin,
            &CheckedType::Function(callback.function_type.clone()),
            "callback type",
        );
        if let Some(callable) = callback.callable {
            self.visit_expression(callable);
        }
        for use_ in &callback.resources {
            self.visit_use(*use_);
        }
    }

    fn visit_plan(&mut self, id: LoweredCoroutinePlanId) {
        if !self.plan_visits.insert(id) {
            return;
        }
        let Some(plan) = self.body.plans.get(id) else {
            self.report(
                self.instance.origin.span.clone(),
                "instance body references a missing coroutine plan",
            );
            return;
        };
        if plan.thunk != self.instance.template {
            self.report(
                plan.origin.span.clone(),
                "coroutine plan is owned by a different function template",
            );
        }
        self.check_concrete_type(&plan.origin, &plan.result_type, "coroutine result type");
        self.check_effects(
            &plan.origin,
            &plan.deferred_effects,
            "coroutine deferred effects",
        );
        for value_type in &plan.await_result_types {
            self.check_concrete_type(&plan.origin, value_type, "awaited result type");
        }
        if let Some(body) = plan.body {
            self.visit_block(body);
        }
        let mut states = Vec::new();
        for await_ in &plan.awaits {
            if let Some(record) = self.body.awaits.get(*await_) {
                if record.owning_plan != id {
                    self.report(
                        record.origin.span.clone(),
                        "await is listed by a plan it does not belong to",
                    );
                }
                if record.resume_state == 0 || record.resume_state > plan.resume_points {
                    self.report(
                        record.origin.span.clone(),
                        "await resume state is outside the plan's resume points",
                    );
                }
                states.push(record.resume_state);
            } else {
                self.report(
                    plan.origin.span.clone(),
                    "coroutine plan lists a missing await",
                );
            }
            self.visit_await(*await_);
        }
        let expected = (1..=states.len()).collect::<Vec<_>>();
        if states != expected {
            self.report(
                plan.origin.span.clone(),
                "coroutine plan await order disagrees with its resume states",
            );
        }
        for state in plan
            .wait_await_states
            .iter()
            .chain(&plan.until_await_states)
        {
            if *state == 0 || *state > plan.resume_points {
                self.report(
                    plan.origin.span.clone(),
                    "coroutine cancellation state is outside the plan's resume points",
                );
            }
        }
    }

    fn visit_coro(&mut self, id: LoweredCoroId) {
        if !self.coros.insert(id) {
            return;
        }
        let Some(coro) = self.body.coros.get(id) else {
            return;
        };
        let _ = coro;
    }

    fn visit_await(&mut self, id: LoweredAwaitId) {
        if !self.awaits.insert(id) {
            return;
        }
        let Some(await_) = self.body.awaits.get(id) else {
            return;
        };
        self.check_concrete_type(&await_.origin, &await_.result_type, "await result type");
        if self.body.plans.get(await_.owning_plan).is_none() {
            self.report(
                await_.origin.span.clone(),
                "await is owned by a missing coroutine plan",
            );
        }
        match &await_.kind {
            LoweredAwaitKind::ChildCoroutine {
                child_result,
                deferred_resources,
                ..
            } => {
                self.check_concrete_type(&await_.origin, child_result, "child result type");
                for use_ in deferred_resources {
                    self.visit_use(*use_);
                }
            }
            LoweredAwaitKind::Task { result } | LoweredAwaitKind::Wait { result } => {
                self.check_concrete_type(&await_.origin, result, "await result type");
            }
        }
        self.visit_expression(await_.operand);
    }

    fn check_arena_density(&mut self) {
        self.check_density("block", self.blocks.len(), self.body.blocks.len());
        self.check_density("item", self.items.len(), self.body.items.len());
        self.check_density(
            "expression",
            self.expressions.len(),
            self.body.expressions.len(),
        );
        self.check_density("pattern", self.patterns.len(), self.body.patterns.len());
        self.check_density("place", self.places.len(), self.body.places.len());
        self.check_density("call", self.calls.len(), self.body.calls.len());
        self.check_density(
            "callable value",
            self.callable_values.len(),
            self.body.callable_values.len(),
        );
        self.check_density(
            "resource provider",
            self.providers.len(),
            self.body.resource_providers.len(),
        );
        self.check_density(
            "resource use",
            self.uses.len(),
            self.body.resource_uses.len(),
        );
        self.check_density("with", self.withs.len(), self.body.withs.len());
        self.check_density(
            "reactive operation",
            self.operations.len(),
            self.body.reactive_operations.len(),
        );
        self.check_density(
            "reactive callback",
            self.callbacks.len(),
            self.body.reactive_callbacks.len(),
        );
        self.check_density("coro", self.coros.len(), self.body.coros.len());
        self.check_density("await", self.awaits.len(), self.body.awaits.len());
    }

    fn check_density(&mut self, family: &str, visited: usize, total: usize) {
        if visited != total {
            self.report(
                self.instance.origin.span.clone(),
                format!(
                    "instance body has {} unreachable {family} nodes",
                    total.saturating_sub(visited)
                ),
            );
        }
    }

    fn check_bindings(&mut self) {
        let mut matched_dependencies = HashSet::new();
        let mut matched_artifacts = HashSet::new();
        for (site, target) in &self.body.bindings {
            let site = *site;
            if !self.site_exists(site) {
                self.report(
                    self.instance.origin.span.clone(),
                    "instance body binds a site that does not exist",
                );
                continue;
            }
            let origin = self.site_origin(site);
            match target {
                LoweredBoundTarget::Instance(instance) => {
                    if !self.program.instances.contains(*instance) {
                        self.report(
                            origin
                                .as_ref()
                                .map(|origin| origin.span.clone())
                                .unwrap_or(Span::Compiler),
                            "instance body binds a missing function instance",
                        );
                        continue;
                    }
                    let kinds = self.site_dependency_kinds(site);
                    let matched = match origin.as_ref() {
                        Some(origin) => self.match_dependency_edge(
                            *instance,
                            &kinds,
                            origin,
                            &mut matched_dependencies,
                        ),
                        None => false,
                    };
                    if !matched && !kinds.is_empty() {
                        self.report(
                            origin
                                .as_ref()
                                .map(|origin| origin.span.clone())
                                .unwrap_or(Span::Compiler),
                            "instance body binding does not match a Stage 3.3 dependency",
                        );
                    }
                }
                LoweredBoundTarget::Artifact(ordinal) => {
                    if ordinal.index() >= self.program.artifacts.len() {
                        self.report(
                            origin
                                .as_ref()
                                .map(|origin| origin.span.clone())
                                .unwrap_or(Span::Compiler),
                            "instance body binds a missing generated artifact",
                        );
                        continue;
                    }
                    let Some(kind) = self.site_artifact_kind(site) else {
                        self.report(
                            origin
                                .as_ref()
                                .map(|origin| origin.span.clone())
                                .unwrap_or(Span::Compiler),
                            "instance body binds an artifact at a non-artifact site",
                        );
                        continue;
                    };
                    let matched = match origin.as_ref() {
                        Some(origin) => {
                            self.match_artifact_edge(*ordinal, kind, origin, &mut matched_artifacts)
                        }
                        None => false,
                    };
                    if !matched {
                        self.report(
                            origin
                                .as_ref()
                                .map(|origin| origin.span.clone())
                                .unwrap_or(Span::Compiler),
                            "instance body artifact binding does not match a Stage 3.3 artifact request",
                        );
                    }
                }
                LoweredBoundTarget::Route(category) => {
                    if let Some(actual) = self.site_category(site)
                        && actual != *category
                    {
                        self.report(
                            origin
                                .as_ref()
                                .map(|origin| origin.span.clone())
                                .unwrap_or(Span::Compiler),
                            "instance body route binding disagrees with its site",
                        );
                    }
                }
            }
        }
        for (index, dependency) in self.instance.dependencies.iter().enumerate() {
            // Closure-phase edges are scanner discoveries; the closure
            // validator checks them against use records instead.
            if !dependency.closure_phase && !matched_dependencies.contains(&index) {
                self.report(
                    dependency.origin.span.clone(),
                    format!(
                        "Stage 3.3 {} dependency has no instance-body binding",
                        dependency.kind.description()
                    ),
                );
            }
        }
        for (index, artifact) in self.instance.artifacts.iter().enumerate() {
            if !artifact.closure_phase && !matched_artifacts.contains(&index) {
                self.report(
                    artifact.origin.span.clone(),
                    "Stage 3.3 artifact request has no instance-body binding",
                );
            }
        }
    }

    fn match_dependency_edge(
        &self,
        instance: FunctionInstanceId,
        kinds: &[LoweredInstanceDependencyKind],
        origin: &Origin,
        matched: &mut HashSet<usize>,
    ) -> bool {
        let found = self
            .instance
            .dependencies
            .iter()
            .enumerate()
            .find(|(index, dependency)| {
                !matched.contains(index)
                    && !dependency.closure_phase
                    && dependency.instance == instance
                    && kinds.contains(&dependency.kind)
                    && dependency.origin.syntax == origin.syntax
            })
            .map(|(index, _)| index);
        match found {
            Some(index) => {
                matched.insert(index);
                true
            }
            None => false,
        }
    }

    fn match_artifact_edge(
        &self,
        ordinal: ArtifactOrdinal,
        kind: LoweredArtifactDependencyKind,
        origin: &Origin,
        matched: &mut HashSet<usize>,
    ) -> bool {
        let found = self
            .instance
            .artifacts
            .iter()
            .enumerate()
            .find(|(index, artifact)| {
                !matched.contains(index)
                    && !artifact.closure_phase
                    && artifact.artifact == ordinal
                    && artifact.kind == kind
                    && artifact.origin.syntax == origin.syntax
            })
            .map(|(index, _)| index);
        match found {
            Some(index) => {
                matched.insert(index);
                true
            }
            None => false,
        }
    }

    /// The Stage 3.3 dependency kinds a source-function binding at this site
    /// may agree with. Route-only sites (indirect, external, intrinsic,
    /// ordinary constructor) have no dependency edge.
    fn site_dependency_kinds(
        &self,
        site: LoweredBindingSite,
    ) -> Vec<LoweredInstanceDependencyKind> {
        match site {
            LoweredBindingSite::CallArgumentThunk { .. } => {
                vec![LoweredInstanceDependencyKind::ImplicitThunkArgument]
            }
            LoweredBindingSite::Index(_)
            | LoweredBindingSite::Interpolation { .. }
            | LoweredBindingSite::IndexedAssignment(_) => {
                vec![LoweredInstanceDependencyKind::TraitMethod]
            }
            LoweredBindingSite::FormattingConstructor(_) => {
                vec![LoweredInstanceDependencyKind::FormattingConstructor]
            }
            LoweredBindingSite::FormattingFinish(_) => {
                vec![LoweredInstanceDependencyKind::FormattingFinish]
            }
            LoweredBindingSite::FormattingWrite(_) => {
                vec![LoweredInstanceDependencyKind::FormattingWrite]
            }
            LoweredBindingSite::DerivedEvaluator(_) => {
                vec![LoweredInstanceDependencyKind::DerivedEvaluator]
            }
            LoweredBindingSite::ReactiveCallback(_) => {
                vec![LoweredInstanceDependencyKind::ReactiveCallback]
            }
            LoweredBindingSite::Coro(_) => vec![LoweredInstanceDependencyKind::CoroutineBody],
            LoweredBindingSite::AwaitChildPlan(_) => Vec::new(),
            LoweredBindingSite::Call(_) => match self.site_category(site) {
                Some(LoweredCallableCategory::DirectKnownFunction) => {
                    vec![LoweredInstanceDependencyKind::DirectCall]
                }
                Some(LoweredCallableCategory::TraitImplementation)
                | Some(LoweredCallableCategory::StructuralTraitMethod) => {
                    vec![LoweredInstanceDependencyKind::TraitMethod]
                }
                _ => Vec::new(),
            },
            LoweredBindingSite::CallableValue(_) => match self.site_category(site) {
                Some(LoweredCallableCategory::DirectKnownFunction) => {
                    vec![LoweredInstanceDependencyKind::CallableValue]
                }
                Some(LoweredCallableCategory::TraitImplementation)
                | Some(LoweredCallableCategory::StructuralTraitMethod) => {
                    vec![LoweredInstanceDependencyKind::TraitMethod]
                }
                _ => Vec::new(),
            },
        }
    }

    fn site_artifact_kind(
        &self,
        site: LoweredBindingSite,
    ) -> Option<LoweredArtifactDependencyKind> {
        match site {
            LoweredBindingSite::Call(_) | LoweredBindingSite::CallableValue(_) => {
                match self.site_category(site) {
                    Some(LoweredCallableCategory::Constructor) => {
                        Some(LoweredArtifactDependencyKind::ConstructorAdapter)
                    }
                    Some(LoweredCallableCategory::TraitImplementation)
                    | Some(LoweredCallableCategory::StructuralTraitMethod) => {
                        Some(LoweredArtifactDependencyKind::StructuralMethod)
                    }
                    _ => None,
                }
            }
            LoweredBindingSite::Index(_)
            | LoweredBindingSite::Interpolation { .. }
            | LoweredBindingSite::IndexedAssignment(_) => {
                Some(LoweredArtifactDependencyKind::StructuralMethod)
            }
            _ => None,
        }
    }

    fn site_origin(&self, site: LoweredBindingSite) -> Option<&Origin> {
        match site {
            LoweredBindingSite::Call(id) => self.body.calls.get(id).map(|call| &call.origin),
            LoweredBindingSite::CallableValue(id) => {
                self.body.callable_values.get(id).map(|value| &value.origin)
            }
            LoweredBindingSite::CallArgumentThunk { call, .. } => {
                self.body.calls.get(call).map(|call| &call.origin)
            }
            LoweredBindingSite::Index(id)
            | LoweredBindingSite::FormattingConstructor(id)
            | LoweredBindingSite::FormattingFinish(id)
            | LoweredBindingSite::FormattingWrite(id)
            | LoweredBindingSite::Interpolation { template: id, .. } => self
                .body
                .expressions
                .get(id)
                .map(|expression| &expression.origin),
            LoweredBindingSite::IndexedAssignment(id) => {
                self.body.items.get(id).map(|item| &item.origin)
            }
            LoweredBindingSite::DerivedEvaluator(id) => self
                .body
                .reactive_operations
                .get(id)
                .map(|operation| &operation.origin),
            LoweredBindingSite::ReactiveCallback(id) => self
                .body
                .reactive_callbacks
                .get(id)
                .map(|callback| &callback.origin),
            LoweredBindingSite::Coro(id) => self.body.coros.get(id).map(|coro| &coro.origin),
            LoweredBindingSite::AwaitChildPlan(id) => {
                self.body.awaits.get(id).map(|await_| &await_.origin)
            }
        }
    }

    fn site_exists(&self, site: LoweredBindingSite) -> bool {
        match site {
            LoweredBindingSite::Call(id) => self.body.calls.contains(id),
            LoweredBindingSite::CallableValue(id) => self.body.callable_values.contains(id),
            LoweredBindingSite::CallArgumentThunk { call, argument } => self
                .body
                .calls
                .get(call)
                .is_some_and(|call| argument < call.arguments.len()),
            LoweredBindingSite::Index(id)
            | LoweredBindingSite::FormattingConstructor(id)
            | LoweredBindingSite::FormattingFinish(id)
            | LoweredBindingSite::FormattingWrite(id)
            | LoweredBindingSite::Interpolation { template: id, .. } => {
                self.body.expressions.contains(id)
            }
            LoweredBindingSite::IndexedAssignment(id) => self.body.items.contains(id),
            LoweredBindingSite::DerivedEvaluator(id) => self.body.reactive_operations.contains(id),
            LoweredBindingSite::ReactiveCallback(id) => self.body.reactive_callbacks.contains(id),
            LoweredBindingSite::Coro(id) => self.body.coros.contains(id),
            LoweredBindingSite::AwaitChildPlan(id) => self.body.awaits.contains(id),
        }
    }

    fn site_category(&self, site: LoweredBindingSite) -> Option<LoweredCallableCategory> {
        match site {
            LoweredBindingSite::Call(id) => {
                self.body.calls.get(id).map(|call| call.target.category())
            }
            LoweredBindingSite::CallableValue(id) => self
                .body
                .callable_values
                .get(id)
                .map(|value| value.target.category()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{NameResolver, ProgramLoader, TypeChecker, TypedModule};

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

    /// Lowers a program through the Stage 3.3 worklist without materializing
    /// bodies, so a test can observe the before/after boundary.
    fn lower_with_worklist(source: &str) -> (TypedModule, LoweredProgram) {
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

    fn materialize(program: &mut LoweredProgram) {
        let diagnostics = program.materialize_instance_bodies();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = program.validate_instance_bodies();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    fn function_id(program: &LoweredProgram, name: &str) -> FunctionId {
        program
            .functions
            .iter()
            .find(|(_, _, function)| function.name == name)
            .or_else(|| {
                program
                    .functions
                    .iter()
                    .find(|(_, _, function)| function.name.ends_with(&format!(".{name}")))
            })
            .map(|(_, id, _)| id)
            .unwrap_or_else(|| panic!("no lowered function named {name}"))
    }

    fn instance_of<'a>(
        program: &'a LoweredProgram,
        function: FunctionId,
    ) -> (
        FunctionInstanceId,
        &'a super::super::LoweredFunctionInstance,
    ) {
        program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == function)
            .unwrap_or_else(|| panic!("no instance for function {}", function.0))
    }

    /// Every checked value carried by a body must be free of declared
    /// template parameters after substitution.
    fn assert_body_has_no_parameters(
        program: &LoweredProgram,
        instance: &super::super::LoweredFunctionInstance,
    ) {
        let body = instance.body.as_ref().expect("instance body");
        assert!(
            !contains_type_parameter(&CheckedType::Function(body.signature.clone())),
            "body signature still contains a declared parameter"
        );
        for bound in &body.bounds {
            for argument in &bound.arguments {
                assert!(!contains_type_parameter(argument));
            }
        }
        for parameter in &body.parameters {
            assert!(!contains_type_parameter(&parameter.value_type));
        }
        for capture in &body.captures {
            assert!(!contains_type_parameter(&capture.value_type));
        }
        for (_, expression) in body.expressions.iter() {
            assert!(
                !contains_type_parameter(&expression.value_type),
                "expression {:?} keeps a declared parameter",
                expression.origin.span
            );
            for resource in &expression.effects.resources {
                assert!(!contains_type_parameter(&resource.value_type));
            }
        }
        for (_, call) in body.calls.iter() {
            assert!(!contains_type_parameter(&CheckedType::Function(
                call.function_type.clone()
            )));
            assert!(!contains_type_parameter(&call.result_type));
            for argument in &call.arguments {
                assert!(!contains_type_parameter(&argument.expected));
            }
        }
        for (_, pattern) in body.patterns.iter() {
            assert!(!contains_type_parameter(&pattern.value_type));
        }
        for (_, place) in body.places.iter() {
            assert!(!contains_type_parameter(&place.value_type));
        }
        let _ = program;
    }

    #[test]
    fn generic_instances_substitute_signatures_separately() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "let second: U8 = identity (1 satisfies U8)\n",
        ));
        materialize(&mut program);
        let identity = function_id(&program, "identity");
        let instances = program
            .instances
            .iter()
            .filter(|(_, instance)| instance.template == identity)
            .map(|(_, instance)| instance)
            .collect::<Vec<_>>();
        assert_eq!(instances.len(), 2, "one body per distinct substitution");
        let mut parameters = instances
            .iter()
            .map(
                |instance| match instance.body.as_ref().unwrap().signature.parameter.as_ref() {
                    CheckedType::Parameter { .. } => {
                        panic!("a materialized body cannot keep a declared parameter")
                    }
                    concrete => concrete.clone(),
                },
            )
            .collect::<Vec<_>>();
        parameters.sort_by_key(|value| format!("{value:?}"));
        assert_eq!(parameters, vec![CheckedType::I32, CheckedType::U8]);
        for instance in &instances {
            assert_body_has_no_parameters(&program, instance);
        }
        let template = program
            .functions
            .get(identity)
            .expect("the template stays generic");
        assert!(
            matches!(
                template.signature.parameter.as_ref(),
                CheckedType::Parameter { .. }
            ),
            "the source template remains unchanged"
        );
    }

    #[test]
    fn unused_generic_bounds_do_not_require_an_instance_substitution() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def irrelevant: <T where Copy T> I32 -> I32 = value => value\n",
            "let result = irrelevant 1\n",
        ));
        materialize(&mut program);
        let (_, instance) = instance_of(&program, function_id(&program, "irrelevant"));
        assert!(instance.environment.is_empty());
        assert!(instance.body.as_ref().unwrap().bounds.is_empty());
    }

    #[test]
    fn owned_capture_drop_is_derived_from_its_concrete_type() {
        let (_, mut program) = lower_with_worklist(concat!(
            "use std.cinterop.*\n",
            "extern \"c\" { inspect: CString -> I32 }\n",
            "def make = (move value: CString) => { let callback = () => inspect value; callback }\n",
            "let callback = make (c_string \"owned\")\n",
        ));
        for value in &mut program.callable_values.values {
            if let Some(closure) = &mut value.closure {
                for capture in &mut closure.captures {
                    if capture.value_type == CheckedType::CString {
                        capture.drops_value = false;
                    }
                }
            }
        }
        materialize(&mut program);
        let (_, instance) = instance_of(&program, function_id(&program, "make"));
        let body = instance.body.as_ref().unwrap();
        let capture = body
            .callable_values
            .iter()
            .filter_map(|(_, value)| value.closure.as_ref())
            .flat_map(|closure| &closure.captures)
            .find(|capture| capture.value_type == CheckedType::CString)
            .expect("closure captures the concrete CString");
        assert!(capture.owns_value);
        assert!(capture.drops_value);
    }

    #[test]
    fn resource_uses_recompute_pass_mode_and_provider_indirectness() {
        let (_, mut program) = lower_with_worklist(concat!(
            "type A = ctor (value: I32)\n",
            "def read_a: () ->{A} I32 = () => (resource A).value\n",
            "def forward: () ->{A} I32 = () => read_a ()\n",
            "let result = with A = A (value: 1) { forward () }\n",
        ));
        let mut changed = 0;
        for use_ in &mut program.resource_uses.values {
            if matches!(
                use_.kind,
                super::super::LoweredResourceUseKind::Read
                    | super::super::LoweredResourceUseKind::HiddenArgument
            ) {
                use_.pass_mode = super::super::LoweredArgumentPassMode::BorrowedPointer;
                use_.indirect = true;
                changed += 1;
            }
        }
        assert!(changed > 0);
        materialize(&mut program);
        let (_, instance) = instance_of(&program, function_id(&program, "forward"));
        let body = instance.body.as_ref().unwrap();
        let mut saw_hidden = false;
        for (_, use_) in body.resource_uses.iter() {
            if use_.kind != super::super::LoweredResourceUseKind::HiddenArgument {
                continue;
            }
            let provider = body.resource_providers.get(use_.provider.unwrap()).unwrap();
            assert!(!provider.indirect, "the concrete A provider is Copy");
            assert!(!use_.indirect);
            assert_eq!(use_.pass_mode, super::super::LoweredArgumentPassMode::Value);
            saw_hidden = true;
        }
        assert!(saw_hidden);
    }

    #[test]
    fn hidden_resource_bindings_recover_order_without_a_count_change() {
        let (_, mut program) = lower_with_worklist(concat!(
            "type A = ctor (value: I32)\n",
            "type B = ctor (value: I32)\n",
            "def sum: () ->{A, B} I32 = () => (resource A).value + (resource B).value\n",
            "def forward: () ->{A, B} I32 = () => sum ()\n",
            "let result = with A = A (value: 1) { with B = B (value: 2) { forward () } }\n",
        ));
        let call = program
            .calls
            .values
            .iter_mut()
            .find(|call| {
                matches!(&call.origin.span, Span::User { location: Some(location), .. } if location.line == 4)
                    && call.resource_bindings.len() == 2
            })
            .expect("forward calls sum");
        assert_eq!(call.resource_bindings.len(), 2);
        call.resource_bindings.reverse();
        materialize(&mut program);
        let (_, instance) = instance_of(&program, function_id(&program, "forward"));
        let body = instance.body.as_ref().unwrap();
        let call = body
            .calls
            .iter()
            .map(|(_, call)| call)
            .find(|call| call.resource_bindings.len() == 2)
            .expect("concrete forward calls sum");
        for (id, required) in call
            .resource_bindings
            .iter()
            .zip(&call.function_type.effects.resources)
        {
            assert_eq!(&body.resource_uses.get(*id).unwrap().resource, required);
        }
    }

    #[test]
    fn repeated_product_counts_substitute_in_bodies() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T; N) = value => n => (value; N)\n",
            "let repeated: (I32; 3) = repeat 7 3\n",
        ));
        materialize(&mut program);
        let mut found = 0;
        for (_, instance) in program.instances.iter() {
            let body = instance.body.as_ref().expect("instance body");
            assert_body_has_no_parameters(&program, instance);
            for (_, expression) in body.expressions.iter() {
                if let LoweredExpressionKind::RepeatedProduct(repeated) = &expression.kind {
                    found += 1;
                    match &repeated.count {
                        super::super::LoweredRepeatCount::Symbolic(count) => {
                            assert!(!contains_type_parameter(count));
                        }
                        super::super::LoweredRepeatCount::Fixed(count) => {
                            assert_eq!(*count, 3);
                        }
                    }
                }
            }
        }
        assert_eq!(
            found, 1,
            "the repeated product survives into exactly one concrete body"
        );
    }

    #[test]
    fn nested_closures_and_captures_materialize_concretely() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def make: <T where Copy T> T -> (() -> T) = value => () => value\n",
            "let produced: () -> I32 = make 7\n",
        ));
        materialize(&mut program);
        let make = function_id(&program, "make");
        let instance = program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == make)
            .map(|(_, instance)| instance)
            .expect("make instance");
        assert_body_has_no_parameters(&program, instance);
        let body = instance.body.as_ref().unwrap();
        assert!(
            body.callable_values
                .iter()
                .any(|(_, value)| value.closure.is_some()),
            "the nested closure construction is cloned into the body"
        );
    }

    #[test]
    fn identical_requests_share_one_body() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
            "let second: I32 = identity 1\n",
        ));
        materialize(&mut program);
        let identity = function_id(&program, "identity");
        let instances = program
            .instances
            .iter()
            .filter(|(_, instance)| instance.template == identity)
            .collect::<Vec<_>>();
        assert_eq!(instances.len(), 1, "identical requests share one instance");
        assert!(instances[0].1.body.is_some());
    }

    /// Every trait-dependent site in every body must carry resolved evidence,
    /// and its binding must point at a Stage 3.3 instance or artifact.
    fn assert_trait_sites_are_resolved(program: &LoweredProgram) {
        for (_, instance) in program.instances.iter() {
            let body = instance.body.as_ref().expect("instance body");
            for (site, evidence) in &body.evidence {
                assert!(
                    !matches!(
                        evidence,
                        TraitEvidence::DeclaredBound { .. }
                            | TraitEvidence::RejectedImplementation { .. }
                    ),
                    "body evidence at {site:?} is not a concrete selection"
                );
                assert!(
                    body.binding(*site).is_some(),
                    "body evidence at {site:?} has no binding"
                );
            }
            for (_, call) in body.calls.iter() {
                if let Some(evidence) = &call.evidence {
                    if matches!(
                        call.target.category(),
                        LoweredCallableCategory::TraitImplementation
                            | LoweredCallableCategory::StructuralTraitMethod
                    ) {
                        assert!(
                            !matches!(
                                evidence,
                                TraitEvidence::DeclaredBound { .. }
                                    | TraitEvidence::RejectedImplementation { .. }
                            ),
                            "a trait call keeps a deferred recipe"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn recursive_calls_bind_to_their_own_instance() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def recursive: <T where Copy T> T -> T = value => recursive value\n",
            "let result: I32 = recursive 1\n",
        ));
        materialize(&mut program);
        let recursive = function_id(&program, "recursive");
        let (id, instance) = program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == recursive)
            .expect("recursive instance");
        let body = instance.body.as_ref().unwrap();
        let mut saw_back_edge = false;
        for (call_id, _) in body.calls.iter() {
            let site = LoweredBindingSite::Call(call_id);
            if let Some(LoweredBoundTarget::Instance(target)) = body.binding(site) {
                assert_eq!(*target, id, "recursion reuses the interned instance");
                saw_back_edge = true;
            }
        }
        assert!(saw_back_edge, "the recursive call is a concrete back-edge");
        assert_trait_sites_are_resolved(&program);
    }

    #[test]
    fn constructor_and_structural_sites_bind_to_artifacts() {
        let (_, mut program) = lower_with_worklist(concat!(
            "type Point = ctor (I32, I32)\n",
            "def make: () -> ((I32, I32) -> Point) = () => Point\n",
            "def describe: (I32, I32) -> String = value => \"${value:?}\"\n",
            "let text: String = describe (1, 2)\n",
            "let built: (I32, I32) -> Point = make ()\n",
        ));
        materialize(&mut program);
        let mut constructor = 0;
        let mut structural = 0;
        for (_, instance) in program.instances.iter() {
            let body = instance.body.as_ref().unwrap();
            for (_, target) in body.bindings.iter() {
                if let LoweredBoundTarget::Artifact(ordinal) = target {
                    match program.specializations.artifact(*ordinal) {
                        Some(ArtifactRequestKey::ConstructorAdapter(_)) => constructor += 1,
                        Some(ArtifactRequestKey::StructuralMethod(_)) => structural += 1,
                        _ => panic!("unexpected artifact family"),
                    }
                }
            }
        }
        assert!(constructor >= 1, "a constructor value binds an adapter");
        assert!(structural >= 1, "a structural selection binds an artifact");
        assert_trait_sites_are_resolved(&program);
    }

    #[test]
    fn indirect_calls_bind_as_routes_without_new_instances() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def apply: (() -> I32) -> I32 = f => f ()\n",
            "let result = apply (() => 1)\n",
        ));
        materialize(&mut program);
        let apply = function_id(&program, "apply");
        let instance = program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == apply)
            .map(|(_, instance)| instance)
            .expect("apply instance");
        let body = instance.body.as_ref().unwrap();
        let routes = body
            .bindings
            .iter()
            .filter_map(|(site, target)| match (site, target) {
                (_, LoweredBoundTarget::Route(category)) => Some(*category),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            routes.contains(&LoweredCallableCategory::IndirectClosure),
            "the call through the parameter stays indirect"
        );
        assert!(
            !program.instances.iter().any(|(_, instance)| program
                .functions
                .get(instance.template)
                .is_some_and(|function| function.name.contains("anonymous"))),
            "an indirect invocation creates no anonymous instance"
        );
        assert_trait_sites_are_resolved(&program);
    }

    #[test]
    fn formatting_sites_store_concrete_evidence() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def render: <T where Display T> move T -> String = move value => \"value=$value\"\n",
            "let result: String = render 1\n",
        ));
        materialize(&mut program);
        let render = function_id(&program, "render");
        let instance = program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == render)
            .map(|(_, instance)| instance)
            .expect("render instance");
        let body = instance.body.as_ref().unwrap();
        let interpolation = body
            .evidence
            .keys()
            .find(|site| matches!(site, LoweredBindingSite::Interpolation { .. }))
            .copied()
            .expect("the interpolation records evidence");
        assert!(matches!(
            body.binding(interpolation),
            Some(LoweredBoundTarget::Instance(_))
        ));
        assert!(
            body.bindings.iter().any(|(site, target)| {
                matches!(
                    site,
                    LoweredBindingSite::FormattingConstructor(_)
                        | LoweredBindingSite::FormattingFinish(_)
                ) && matches!(target, LoweredBoundTarget::Instance(_))
            }),
            "the formatter helpers bind to their instances"
        );
        assert_trait_sites_are_resolved(&program);
    }

    #[test]
    fn copy_and_drop_facts_agree_with_checking() {
        let (module, mut program) = lower_with_worklist(concat!(
            "use std.cinterop.(CString, c_string)\n",
            "def discard: <T> move T -> () = move value => { value; () }\n",
            "def make_string: () -> (() -> String) = () => {\n",
            "  let held = \"captured\"\n",
            "  () => held\n",
            "}\n",
            "def looped: () -> I32 = () => {\n",
            "  let mut total = 0\n",
            "  while (total < 1) { total = total + 1 }\n",
            "  total\n",
            "}\n",
            "let first: () = discard 1\n",
            "let second: () = discard (c_string \"x\")\n",
            "let closure: () -> String = make_string ()\n",
            "let counted: I32 = looped ()\n",
        ));
        materialize(&mut program);
        let mut saw_dropped_statement = false;
        let mut saw_live_statement = false;
        let mut saw_capture = false;
        for (_, instance) in program.instances.iter() {
            let body = instance.body.as_ref().unwrap();
            for (_, item) in body.items.iter() {
                match &item.kind {
                    LoweredItemKind::Expression(statement) => {
                        let value_type = body
                            .expressions
                            .get(statement.expression)
                            .map(|expression| expression.value_type.clone())
                            .unwrap_or(CheckedType::Error);
                        assert_eq!(
                            statement.drop_result,
                            module.type_needs_drop(&value_type),
                            "discarded value drop disagrees with checking"
                        );
                        saw_dropped_statement |= statement.drop_result;
                        saw_live_statement |= !statement.drop_result;
                    }
                    LoweredItemKind::Assignment(assignment) => {
                        let value_type = body
                            .places
                            .get(assignment.target)
                            .map(|place| place.value_type.clone())
                            .unwrap_or(CheckedType::Error);
                        assert_eq!(
                            assignment.drop_previous,
                            module.type_needs_drop(&value_type),
                            "assignment previous-value drop disagrees with checking"
                        );
                    }
                    _ => {}
                }
            }
            for (_, expression) in body.expressions.iter() {
                if let LoweredExpressionKind::Loop(loop_) = &expression.kind {
                    let body_result = body
                        .blocks
                        .get(loop_.body)
                        .and_then(|block| block.result)
                        .and_then(|result| body.expressions.get(result))
                        .map(|result| result.value_type.clone());
                    if let Some(body_result) = body_result {
                        assert_eq!(
                            loop_.drops_body_result,
                            module.type_needs_drop(&body_result),
                            "loop body-result drop disagrees with checking"
                        );
                    }
                }
            }
            for (_, value) in body.callable_values.iter() {
                if let Some(closure) = &value.closure {
                    for capture in &closure.captures {
                        assert_eq!(
                            capture.drops_value,
                            capture.owns_value && module.type_needs_drop(&capture.value_type),
                            "capture drop disagrees with checking"
                        );
                        saw_capture = true;
                    }
                }
            }
            for (_, use_) in body.resource_uses.iter() {
                if use_.kind == super::super::LoweredResourceUseKind::HiddenArgument {
                    let borrow =
                        use_.resource.mutable || !module.is_copy_type(&use_.resource.value_type);
                    assert_eq!(
                        use_.pass_mode == super::super::LoweredArgumentPassMode::BorrowedPointer,
                        borrow,
                        "resource pass mode disagrees with checking"
                    );
                }
            }
        }
        assert!(
            saw_dropped_statement,
            "a move-only discarded value needs a drop"
        );
        assert!(saw_live_statement, "a copy discarded value does not");
        assert!(saw_capture, "the closure capture check ran");
    }

    #[test]
    fn call_step_layout_is_preserved() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def combine: (I32, (I32, I32)) -> I32 = arguments => {\n",
            "  let (first, (second, third)) = arguments\n",
            "  first + second + third\n",
            "}\n",
            "combine (1, (2, 3))\n",
        ));
        materialize(&mut program);
        let combine = function_id(&program, "combine");
        let instance = program
            .instances
            .iter()
            .find(|(_, instance)| instance.template == combine)
            .map(|(_, instance)| instance)
            .expect("combine instance");
        let body = instance.body.as_ref().unwrap();
        let template_call = program
            .calls
            .iter()
            .find(|(_, call)| call.origin.syntax == body.origin.syntax)
            .map(|(_, call)| call);
        if let Some(template_call) = template_call {
            let body_calls = body.calls.iter().map(|(_, call)| call).collect::<Vec<_>>();
            let body_call = body_calls
                .iter()
                .find(|call| call.origin.syntax == template_call.origin.syntax)
                .expect("the call is cloned with its origin");
            assert_eq!(body_call.arguments.len(), template_call.arguments.len());
            assert_eq!(body_call.steps.len(), template_call.steps.len());
            for (cloned, original) in body_call.steps.iter().zip(&template_call.steps) {
                assert_eq!(
                    std::mem::discriminant(cloned),
                    std::mem::discriminant(original)
                );
            }
            for (cloned, original) in body_call.arguments.iter().zip(&template_call.arguments) {
                assert_eq!(cloned.slot, original.slot);
                assert_eq!(cloned.temporary, original.temporary);
            }
        }
    }

    fn instance_body_snapshot(program: &LoweredProgram) -> Vec<String> {
        program
            .instances
            .iter()
            .map(|(id, instance)| {
                format!(
                    "instance {} {} {:?}",
                    id.index(),
                    instance.name,
                    instance.body
                )
            })
            .collect()
    }

    #[test]
    fn repeated_lowering_produces_stable_bodies() {
        let source = concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "def discard: <T> move T -> () = move value => { value; () }\n",
            "let first: I32 = identity 1\n",
            "let second: U8 = identity (1 satisfies U8)\n",
            "let third: () = discard 2\n",
        );
        let (_, mut first) = lower_with_worklist(source);
        let (_, mut second) = lower_with_worklist(source);
        materialize(&mut first);
        materialize(&mut second);
        assert_eq!(
            instance_body_snapshot(&first),
            instance_body_snapshot(&second),
            "repeated lowering must produce byte-identical instance bodies"
        );
    }

    #[test]
    fn materialization_cannot_mutate_templates() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def identity: <T where Copy T> T -> T = value => value\n",
            "let first: I32 = identity 1\n",
        ));
        let template_expressions = program
            .expressions
            .iter()
            .map(|(_, expression)| format!("{expression:?}"))
            .collect::<Vec<_>>();
        let template_calls = program
            .calls
            .iter()
            .map(|(_, call)| format!("{call:?}"))
            .collect::<Vec<_>>();
        materialize(&mut program);
        assert_eq!(
            template_expressions,
            program
                .expressions
                .iter()
                .map(|(_, expression)| format!("{expression:?}"))
                .collect::<Vec<_>>(),
            "materialization must not mutate template expressions"
        );
        assert_eq!(
            template_calls,
            program
                .calls
                .iter()
                .map(|(_, call)| format!("{call:?}"))
                .collect::<Vec<_>>(),
            "materialization must not mutate template calls"
        );
    }

    #[test]
    fn coroutine_plans_and_await_links_materialize() {
        let (_, mut program) = lower_with_worklist(concat!(
            "use std.coroutine.*\n",
            "use std.io.(IO, println)\n",
            "def worker: () -> Coroutine{IO} I32 = () => coro { println \"work\"; 7 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro { let v = await (coro { 7 }); v + 1 }\n",
            "let sched = scheduler ()\n",
            "with Tasks = task_scope (sched) {\n",
            "  let _ = spawn (driver ())\n",
            "  let _ = pump (sched, 4)\n",
            "}\n",
        ));
        materialize(&mut program);
        let mut saw_plan = false;
        let mut saw_creation_link = false;
        for (_, instance) in program.instances.iter() {
            let body = instance.body.as_ref().unwrap();
            if body.plan_template.is_some() {
                saw_plan = true;
            }
            for (coro_id, _) in body.coros.iter() {
                if let Some(LoweredBoundTarget::Instance(owner)) =
                    body.binding(LoweredBindingSite::Coro(coro_id))
                {
                    saw_creation_link = true;
                    assert!(
                        program
                            .instances
                            .get(*owner)
                            .and_then(|instance| instance.body.as_ref())
                            .is_some_and(|body| body.plan_template.is_some()),
                        "a coroutine creation binds its body thunk's plan"
                    );
                }
            }
            for (await_id, await_) in body.awaits.iter() {
                if let LoweredAwaitKind::ChildCoroutine { plan: Some(_), .. } = &await_.kind {
                    assert!(
                        matches!(
                            body.binding(LoweredBindingSite::AwaitChildPlan(await_id)),
                            Some(LoweredBoundTarget::Instance(_))
                        ),
                        "an identified child plan binds to its thunk instance"
                    );
                }
            }
        }
        assert!(saw_plan, "a coroutine body thunk owns its plan");
        assert!(
            saw_creation_link,
            "a coroutine creation binds to its body thunk instance"
        );
    }

    #[test]
    fn reactive_callbacks_bind_in_bodies() {
        let (_, mut program) = lower_with_worklist(concat!(
            "let signal count = 0\n",
            "def install: () ->{state} () = () => {\n",
            "  with Reactive = reactive_scope () {\n",
            "    reaction { let current = count; () }\n",
            "    count = 1\n",
            "  }\n",
            "}\n",
            "install ()\n",
        ));
        materialize(&mut program);
        let mut saw_callback = false;
        for (_, instance) in program.instances.iter() {
            let body = instance.body.as_ref().unwrap();
            for (callback_id, _) in body.reactive_callbacks.iter() {
                if let Some(LoweredBoundTarget::Instance(_)) =
                    body.binding(LoweredBindingSite::ReactiveCallback(callback_id))
                {
                    saw_callback = true;
                }
            }
        }
        assert!(
            saw_callback,
            "a reaction callback thunk binds to its instance"
        );
    }

    #[test]
    fn nongeneric_instance_body_matches_its_template() {
        let (_, mut program) =
            lower_with_worklist("def identity: I32 -> I32 = value => value\nidentity 1\n");
        let template_count = program.expressions.len();
        let template_blocks = program.blocks.len();
        let answer = function_id(&program, "identity");
        materialize(&mut program);
        let (id, instance) = instance_of(&program, answer);
        let body = instance
            .body
            .as_ref()
            .expect("a nongeneric instance with a body materializes one");
        let template = program
            .functions
            .get(answer)
            .expect("the template stays in the catalog");
        assert_eq!(body.template, answer);
        assert_eq!(body.origin.syntax, template.origin.syntax);
        assert_eq!(body.signature, template.signature);
        assert_eq!(body.parameters.len(), template.parameters.len());
        assert!(body.root.is_some(), "the body has a local root block");
        assert!(body.blocks.len() > 0, "instance {id:?} owns local blocks");
        assert!(
            body.expressions.len() > 0,
            "the body owns instance-local expressions"
        );
        // Materialization never mutates the template arenas.
        assert_eq!(program.expressions.len(), template_count);
        assert_eq!(program.blocks.len(), template_blocks);
    }

    #[test]
    fn bodyless_templates_materialize_empty_bodies() {
        let (_, mut program) =
            lower_with_worklist("def identity: I32 -> I32 = value => value\nidentity 1\n");
        materialize(&mut program);
        for (_, instance) in program.instances.iter() {
            let template = program
                .functions
                .get(instance.template)
                .expect("instance templates stay in the catalog");
            let body = instance.body.as_ref().expect("every instance has a body");
            if template.body.is_none() {
                assert!(body.root.is_none());
                assert_eq!(body.blocks.len(), 0);
                assert_eq!(body.expressions.len(), 0);
            } else {
                assert!(body.root.is_some());
            }
        }
    }

    #[test]
    fn instance_body_traversal_uses_only_local_ids() {
        let (_, mut program) = lower_with_worklist(concat!(
            "def double: I32 -> I32 = value => value\n",
            "def outer: () -> I32 = () => {\n",
            "  let local = double 21\n",
            "  local\n",
            "}\n",
            "outer ()\n",
        ));
        materialize(&mut program);
        for (id, instance) in program.instances.iter() {
            let body = instance.body.as_ref().expect("instance body");
            let Some(root) = body.root else {
                continue;
            };
            // Every local reference is in range: walk the local arenas and
            // resolve each child ID against the body itself.
            let mut expressions = std::collections::HashSet::new();
            walk_blocks(body, root, &mut expressions);
            for expression in expressions {
                let node = body
                    .expression(expression)
                    .unwrap_or_else(|| panic!("instance {id:?} has a dangling expression"));
                let _ = node;
            }
        }
    }

    fn walk_blocks(
        body: &LoweredInstanceBody,
        block: BlockId,
        expressions: &mut std::collections::HashSet<ExpressionId>,
    ) {
        let Some(block) = body.block(block) else {
            return;
        };
        for item in &block.items {
            if let Some(item) = body.item(*item) {
                match &item.kind {
                    LoweredItemKind::Binding(binding) => {
                        if let Some(value) = binding.value {
                            walk_expressions(body, value, expressions);
                        }
                    }
                    LoweredItemKind::Expression(statement) => {
                        walk_expressions(body, statement.expression, expressions)
                    }
                    LoweredItemKind::Return(item) => {
                        walk_expressions(body, item.value, expressions)
                    }
                    _ => {}
                }
            }
        }
        if let Some(result) = block.result {
            walk_expressions(body, result, expressions);
        }
    }

    fn walk_expressions(
        body: &LoweredInstanceBody,
        expression: ExpressionId,
        found: &mut std::collections::HashSet<ExpressionId>,
    ) {
        if !found.insert(expression) {
            return;
        }
        let Some(expression) = body.expression(expression) else {
            return;
        };
        match &expression.kind {
            LoweredExpressionKind::Block(block) => walk_blocks(body, *block, found),
            LoweredExpressionKind::Satisfies(satisfies) => {
                walk_expressions(body, satisfies.value, found)
            }
            LoweredExpressionKind::Logical(logical) => {
                walk_expressions(body, logical.left, found);
                walk_expressions(body, logical.right, found);
            }
            LoweredExpressionKind::Access(access) => walk_expressions(body, access.base, found),
            LoweredExpressionKind::Product(product) => {
                for step in &product.steps {
                    let expression = match step {
                        super::super::LoweredProductStep::Positional { expression, .. }
                        | super::super::LoweredProductStep::Designated { expression, .. }
                        | super::super::LoweredProductStep::PositionalSpread {
                            expression, ..
                        }
                        | super::super::LoweredProductStep::NamedSpread { expression, .. }
                        | super::super::LoweredProductStep::Default { expression, .. } => {
                            Some(*expression)
                        }
                    };
                    if let Some(expression) = expression {
                        walk_expressions(body, expression, found);
                    }
                }
                for field in &product.fields {
                    walk_expressions(body, *field, found);
                }
            }
            LoweredExpressionKind::Call(call) => {
                if let Some(call) = body.call(*call) {
                    for step in &call.steps {
                        match step {
                            LoweredCallStep::Callee { expression }
                            | LoweredCallStep::ProductElement { expression, .. }
                            | LoweredCallStep::ProductSpread { expression, .. }
                            | LoweredCallStep::NamedProductSpread { expression, .. }
                            | LoweredCallStep::Default { expression, .. } => {
                                walk_expressions(body, *expression, found)
                            }
                            _ => {}
                        }
                    }
                    for argument in &call.arguments {
                        if let Some(expression) = argument.expression {
                            walk_expressions(body, expression, found);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
