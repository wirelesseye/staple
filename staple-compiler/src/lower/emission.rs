//! Stage 5.1: the backend read view over the lowered program.
//!
//! The emitter receives only lowered IR (Migration Contract 1), but the
//! lowering arenas are private to this module tree and mix two owner shapes:
//! instance bodies own instance-local arenas, while module initializers index
//! the program's template arenas. This module is the one read-only surface the
//! backend may use:
//!
//! - [`EmissionView`] exposes catalog iteration, metadata, planned names, and
//!   binding tables without granting mutation or arena access;
//! - [`OwnerArenas`] resolves one owner-local or program-local ID (block, item,
//!   expression, pattern, place, call, callable value, provider, use, with,
//!   reactive operation/callback, plan, `coro`, await) against either owner
//!   shape, so the emitter never needs to know which arena family holds a node.
//!
//! No arena is made public and no accessor returns `&mut`.

use std::collections::BTreeMap;

use crate::specialization::ArtifactOrdinal;
use crate::{CheckedFunctionType, FunctionId, ModuleId, SymbolId, TraitId, TypeId};

use super::instance_body::{
    LoweredBindingSite, LoweredBoundTarget, LoweredInstanceBody, LoweredInstanceCapture,
    LoweredInstanceParameter,
};
use super::{
    ArenaId, BlockId, ExpressionId, FunctionInstanceId, InitializerId, ItemId,
    LoweredArtifactRequest, LoweredArtifactRequestId, LoweredAwait, LoweredBlock, LoweredCall,
    LoweredCallableValue, LoweredCoro, LoweredCoroutinePlan, LoweredCoroutinePlanId,
    LoweredExpression, LoweredFunction, LoweredFunctionInstance, LoweredInitializer,
    LoweredModuleInfo, LoweredPattern, LoweredProgram, LoweredReactiveCallback,
    LoweredReactiveOperation, LoweredResourceProvider, LoweredResourceProviderId,
    LoweredResourceUse, LoweredResourceUseId, LoweredRuntimeRequirements, LoweredSemanticIds,
    LoweredStringFormatting, LoweredSymbol, LoweredTraitMetadata, LoweredTypeMetadata, LoweredWith,
    PatternId, PlaceId, TraitEvidence,
};

/// The owner shape a backend node ID belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EmissionOwner {
    Instance(FunctionInstanceId),
    Initializer(InitializerId),
}

/// The read-only backend view of one lowered program. Construct with
/// [`LoweredProgram::emission_view`] or `LoweredModule::program`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EmissionView<'a> {
    program: &'a LoweredProgram,
}

impl LoweredProgram {
    /// The Stage 5.1 backend read view. Read-only by construction: every
    /// accessor lends program data for the program's lifetime.
    pub(crate) fn emission_view(&self) -> EmissionView<'_> {
        EmissionView { program: self }
    }
}

impl<'a> EmissionView<'a> {
    /// The owner-arena resolver for one instance body or module initializer.
    /// An instance without a materialized body has no view.
    pub(crate) fn owner(&self, owner: EmissionOwner) -> Option<OwnerArenas<'a>> {
        match owner {
            EmissionOwner::Instance(instance) => self
                .program
                .instances
                .get(instance)
                .and_then(|record| record.body.as_ref())
                .map(OwnerArenas::Instance),
            EmissionOwner::Initializer(initializer) => {
                self.program.initializers.get(initializer)?;
                Some(OwnerArenas::Initializer(initializer))
            }
        }
    }

    /// Every interned source-function instance in catalog order.
    pub(crate) fn instances(
        &self,
    ) -> impl Iterator<Item = (FunctionInstanceId, &'a LoweredFunctionInstance)> + 'a {
        self.program.instances.iter()
    }

    pub(crate) fn instance(&self, id: FunctionInstanceId) -> Option<&'a LoweredFunctionInstance> {
        self.program.instances.get(id)
    }

    /// Every interned generated artifact in ordinal order.
    pub(crate) fn artifacts(
        &self,
    ) -> impl Iterator<Item = (LoweredArtifactRequestId, &'a LoweredArtifactRequest)> + 'a {
        self.program.artifacts.iter()
    }

    pub(crate) fn artifact(&self, ordinal: ArtifactOrdinal) -> Option<&'a LoweredArtifactRequest> {
        self.program
            .artifacts
            .get(LoweredArtifactRequestId::from_index(ordinal.index()))
    }

    /// Every module initializer in lowering order (the program's
    /// initialization order).
    pub(crate) fn initializers(
        &self,
    ) -> impl Iterator<Item = (InitializerId, &'a LoweredInitializer)> + 'a {
        self.program.initializers.iter()
    }

    pub(crate) fn initializer(&self, id: InitializerId) -> Option<&'a LoweredInitializer> {
        self.program.initializers.get(id)
    }

    pub(crate) fn modules(&self) -> impl Iterator<Item = (ModuleId, &'a LoweredModuleInfo)> + 'a {
        self.program.modules.iter().map(|(_, id, info)| (id, info))
    }

    pub(crate) fn module(&self, id: ModuleId) -> Option<&'a LoweredModuleInfo> {
        self.program.modules.get(id)
    }

    /// Function templates by semantic ID, for naming only. The emitter never
    /// enumerates templates to emit bodies: bodies come from instances.
    pub(crate) fn functions(&self) -> impl Iterator<Item = (FunctionId, &'a LoweredFunction)> + 'a {
        self.program
            .functions
            .iter()
            .map(|(_, id, function)| (id, function))
    }

    pub(crate) fn function(&self, id: FunctionId) -> Option<&'a LoweredFunction> {
        self.program.functions.get(id)
    }

    pub(crate) fn symbols(&self) -> impl Iterator<Item = (SymbolId, &'a LoweredSymbol)> + 'a {
        self.program
            .symbols
            .iter()
            .map(|(_, id, symbol)| (id, symbol))
    }

    pub(crate) fn symbol(&self, id: SymbolId) -> Option<&'a LoweredSymbol> {
        self.program.symbols.get(id)
    }

    pub(crate) fn types(&self) -> impl Iterator<Item = (TypeId, &'a LoweredTypeMetadata)> + 'a {
        self.program
            .types
            .iter()
            .map(|(_, id, metadata)| (id, metadata))
    }

    pub(crate) fn type_metadata(&self, id: TypeId) -> Option<&'a LoweredTypeMetadata> {
        self.program.types.get(id)
    }

    pub(crate) fn traits(&self) -> impl Iterator<Item = (TraitId, &'a LoweredTraitMetadata)> + 'a {
        self.program
            .traits
            .iter()
            .map(|(_, id, metadata)| (id, metadata))
    }

    /// The `LoweredSemanticIds` layout context (`runtime_opaque_kind`, string
    /// representation, entry resources, standard trait IDs).
    pub(crate) fn semantic_ids(&self) -> &'a LoweredSemanticIds {
        &self.program.semantic_ids
    }

    pub(crate) fn string_formatting(&self) -> &'a LoweredStringFormatting {
        &self.program.string_formatting
    }

    /// The ordered runtime surfaces the closed catalog needs.
    pub(crate) fn runtime_requirements(&self) -> &'a LoweredRuntimeRequirements {
        &self.program.runtime_requirements
    }

    /// The planned emitted name of one interned source-function instance (D2).
    pub(crate) fn planned_name(&self, instance: FunctionInstanceId) -> Option<String> {
        self.program.planned_name(instance)
    }

    /// The planned emitted name of one interned generated artifact.
    pub(crate) fn planned_artifact_name(&self, ordinal: ArtifactOrdinal) -> Option<String> {
        self.program.planned_artifact_name(ordinal)
    }

    /// The concrete dispatch bindings of one instance body.
    pub(crate) fn instance_bindings(
        &self,
        instance: FunctionInstanceId,
    ) -> Option<&'a BTreeMap<LoweredBindingSite, LoweredBoundTarget>> {
        self.program
            .instances
            .get(instance)
            .and_then(|record| record.body.as_ref())
            .map(|body| &body.bindings)
    }

    /// The resolved evidence of one instance body.
    pub(crate) fn instance_evidence(
        &self,
        instance: FunctionInstanceId,
    ) -> Option<&'a BTreeMap<LoweredBindingSite, TraitEvidence>> {
        self.program
            .instances
            .get(instance)
            .and_then(|record| record.body.as_ref())
            .map(|body| &body.evidence)
    }

    /// The concrete dispatch bindings of one module initializer (D4).
    pub(crate) fn initializer_bindings(
        &self,
        initializer: InitializerId,
    ) -> Option<&'a BTreeMap<LoweredBindingSite, LoweredBoundTarget>> {
        self.program.initializer_bindings.get(initializer.index())
    }

    /// The resolved evidence of one module initializer (D4).
    pub(crate) fn initializer_evidence(
        &self,
        initializer: InitializerId,
    ) -> Option<&'a BTreeMap<LoweredBindingSite, TraitEvidence>> {
        self.program.initializer_evidence.get(initializer.index())
    }

    /// The ordered captures of one instance body.
    pub(crate) fn instance_captures(
        &self,
        instance: FunctionInstanceId,
    ) -> Option<&'a [LoweredInstanceCapture]> {
        self.program
            .instances
            .get(instance)
            .and_then(|record| record.body.as_ref())
            .map(LoweredInstanceBody::captures)
    }

    /// The ordered parameters of one instance body.
    pub(crate) fn instance_parameters(
        &self,
        instance: FunctionInstanceId,
    ) -> Option<&'a [LoweredInstanceParameter]> {
        self.program
            .instances
            .get(instance)
            .and_then(|record| record.body.as_ref())
            .map(|body| body.parameters.as_slice())
    }

    /// The concrete signature of one instance body.
    pub(crate) fn instance_signature(
        &self,
        instance: FunctionInstanceId,
    ) -> Option<&'a CheckedFunctionType> {
        self.program
            .instances
            .get(instance)
            .and_then(|record| record.body.as_ref())
            .map(LoweredInstanceBody::signature)
    }
}

/// The owner whose cleanup sites a scan reads: a materialized instance body or
/// a module initializer. Instance bodies own private arenas; initializer sites
/// index the program's template arenas.
///
/// Stage 4.4 defined this resolver for its scanner; Stage 5.1 promotes it to
/// the shared read-only owner view the Stage 5 emitter and every scanner use.
#[derive(Clone, Copy)]
pub(crate) enum OwnerArenas<'a> {
    Instance(&'a LoweredInstanceBody),
    Initializer(InitializerId),
}

impl<'a> OwnerArenas<'a> {
    pub(crate) fn block(
        self,
        program: &'a LoweredProgram,
        id: BlockId,
    ) -> Option<&'a LoweredBlock> {
        match self {
            OwnerArenas::Instance(body) => body.block(id),
            OwnerArenas::Initializer(_) => program.blocks.get(id),
        }
    }

    pub(crate) fn item(
        self,
        program: &'a LoweredProgram,
        id: ItemId,
    ) -> Option<&'a super::LoweredItem> {
        match self {
            OwnerArenas::Instance(body) => body.item(id),
            OwnerArenas::Initializer(_) => program.items.get(id),
        }
    }

    pub(crate) fn expression(
        self,
        program: &'a LoweredProgram,
        id: ExpressionId,
    ) -> Option<&'a LoweredExpression> {
        match self {
            OwnerArenas::Instance(body) => body.expression(id),
            OwnerArenas::Initializer(_) => program.expressions.get(id),
        }
    }

    pub(crate) fn pattern(
        self,
        program: &'a LoweredProgram,
        id: PatternId,
    ) -> Option<&'a LoweredPattern> {
        match self {
            OwnerArenas::Instance(body) => body.pattern(id),
            OwnerArenas::Initializer(_) => program.patterns.get(id),
        }
    }

    pub(crate) fn place(
        self,
        program: &'a LoweredProgram,
        id: PlaceId,
    ) -> Option<&'a super::LoweredPlace> {
        match self {
            OwnerArenas::Instance(body) => body.place(id),
            OwnerArenas::Initializer(_) => program.places.get(id),
        }
    }

    pub(crate) fn call(
        self,
        program: &'a LoweredProgram,
        id: super::LoweredCallId,
    ) -> Option<&'a LoweredCall> {
        match self {
            OwnerArenas::Instance(body) => body.call(id),
            OwnerArenas::Initializer(_) => program.calls.get(id),
        }
    }

    pub(crate) fn callable_value(
        self,
        program: &'a LoweredProgram,
        id: super::LoweredCallableValueId,
    ) -> Option<&'a LoweredCallableValue> {
        match self {
            OwnerArenas::Instance(body) => body.callable_value(id),
            OwnerArenas::Initializer(_) => program.callable_values.get(id),
        }
    }

    pub(crate) fn resource_provider(
        self,
        program: &'a LoweredProgram,
        id: LoweredResourceProviderId,
    ) -> Option<&'a LoweredResourceProvider> {
        match self {
            OwnerArenas::Instance(body) => body.resource_providers.get(id),
            OwnerArenas::Initializer(_) => program.resource_providers.get(id),
        }
    }

    pub(crate) fn resource_use(
        self,
        program: &'a LoweredProgram,
        id: LoweredResourceUseId,
    ) -> Option<&'a LoweredResourceUse> {
        match self {
            OwnerArenas::Instance(body) => body.resource_uses.get(id),
            OwnerArenas::Initializer(_) => program.resource_uses.get(id),
        }
    }

    pub(crate) fn with(
        self,
        program: &'a LoweredProgram,
        id: super::LoweredWithId,
    ) -> Option<&'a LoweredWith> {
        match self {
            OwnerArenas::Instance(body) => body.withs.get(id),
            OwnerArenas::Initializer(_) => program.withs.get(id),
        }
    }

    pub(crate) fn coro(
        self,
        program: &'a LoweredProgram,
        id: super::LoweredCoroId,
    ) -> Option<&'a LoweredCoro> {
        match self {
            OwnerArenas::Instance(body) => body.coro(id),
            OwnerArenas::Initializer(_) => program.coros.get(id),
        }
    }

    pub(crate) fn reactive_operation(
        self,
        program: &'a LoweredProgram,
        id: super::LoweredReactiveOperationId,
    ) -> Option<&'a LoweredReactiveOperation> {
        match self {
            OwnerArenas::Instance(body) => body.reactive_operation(id),
            OwnerArenas::Initializer(_) => program.reactive_operations.get(id),
        }
    }

    pub(crate) fn reactive_callback(
        self,
        program: &'a LoweredProgram,
        id: super::LoweredReactiveCallbackId,
    ) -> Option<&'a LoweredReactiveCallback> {
        match self {
            OwnerArenas::Instance(body) => body.reactive_callback(id),
            OwnerArenas::Initializer(_) => program.reactive_callbacks.get(id),
        }
    }

    pub(crate) fn plan(
        self,
        program: &'a LoweredProgram,
        id: LoweredCoroutinePlanId,
    ) -> Option<&'a LoweredCoroutinePlan> {
        match self {
            OwnerArenas::Instance(body) => body.plan(id),
            OwnerArenas::Initializer(_) => program.coroutine_plans.get(id),
        }
    }

    pub(crate) fn await_record(
        self,
        program: &'a LoweredProgram,
        id: super::LoweredAwaitId,
    ) -> Option<&'a LoweredAwait> {
        match self {
            OwnerArenas::Instance(body) => body.await_record(id),
            OwnerArenas::Initializer(_) => program.awaits.get(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::{LoweredModule, Lowerer, NameResolver, ProgramLoader, TypeChecker};

    use super::*;

    fn lower(source: &str) -> LoweredModule {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("compiler crate should have a workspace parent");
        let standard_library: PathBuf = root.join("stdlib");
        let program = ProgramLoader::new()
            .with_standard_library_root(standard_library)
            .load_source(source, root)
            .expect("test source should load");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("test source should resolve");
        let module = TypeChecker::new()
            .check(resolved)
            .expect("test source should type check");
        Lowerer::new()
            .lower(&module)
            .unwrap_or_else(|diagnostics| panic!("source should lower: {diagnostics:?}"))
    }

    /// The read view resolves both owner shapes, exposes planned names, and
    /// lends the Stage 5.1 binding tables without exposing an arena.
    #[test]
    fn emission_view_resolves_owners_and_tables() {
        let lowered = lower(concat!(
            "def increment: I32 -> I32 = value => value + 1\n",
            "let entry = increment (1)\n",
        ));
        let view = lowered.program();

        let entry = view
            .modules()
            .find(|(_, module)| module.executable_entry)
            .map(|(_, module)| module.initializer)
            .expect("an entry initializer");
        let owner = view
            .owner(EmissionOwner::Initializer(entry))
            .expect("the initializer has a view");
        assert!(
            !view
                .initializer_bindings(entry)
                .expect("entry bindings")
                .is_empty(),
            "the entry initializer binds its call sites"
        );
        let initializer = view.initializer(entry).expect("the initializer record");
        assert!(
            owner.block(&lowered.program, initializer.body).is_some(),
            "the owner view resolves the initializer root block"
        );

        let (instance, record) = view
            .instances()
            .find(|(_, instance)| instance.body.is_some())
            .expect("a materialized instance");
        let body = record.body.as_ref().expect("materialized body");
        let owner = view
            .owner(EmissionOwner::Instance(instance))
            .expect("the instance has a view");
        let root = body.root.expect("a body root");
        assert!(owner.block(&lowered.program, root).is_some());
        assert!(
            view.instance_bindings(instance)
                .is_some_and(|bindings| !bindings.is_empty()),
            "the instance table is readable"
        );
        assert_eq!(
            view.planned_name(instance).as_deref(),
            Some(record.name.as_str())
        );
    }
}
