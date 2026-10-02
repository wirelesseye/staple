//! the backend read view over the lowered program.
//!
//! The emitter receives only lowered IR , but the
//! lowering arenas are private to this module tree and mix two owner shapes:
//! instance bodies own instance-local arenas, while module initializers index
//! the program's template arenas. This module is the one read-only surface the
//! backend may use:
//!
//! - [`EmissionView`] exposes catalog iteration, metadata, planned names,
//!   binding tables, and the per-owner artifact uses, instance uses, and owned
//!   bindings (uniformly for both owner shapes) without granting mutation or
//!   arena access;
//! - [`OwnerArenas`] resolves one owner-local or program-local ID (block, item,
//!   expression, pattern, place, call, callable value, provider, use, with,
//!   reactive operation/callback, plan, `coro`, await) against either owner
//!   shape, so the emitter never needs to know which arena family holds a node.
//!
//! No arena is made public and no accessor returns `&mut`.

use std::collections::BTreeMap;

use crate::specialization::ArtifactOrdinal;
use crate::{CheckedFunctionType, FunctionId, ModuleId, SymbolId};

use super::artifact_closure::{LoweredArtifactUse, LoweredInstanceUse};
use super::instance_body::{
    LoweredBindingSite, LoweredBoundTarget, LoweredInstanceBody, LoweredOwnedBinding,
};
#[cfg(test)]
use super::instance_body::{LoweredInstanceCapture, LoweredInstanceParameter};
use super::{
    ArenaId, BlockId, ExpressionId, FunctionInstanceId, InitializerId, ItemId,
    LoweredArtifactRequest, LoweredArtifactRequestId, LoweredAwait, LoweredBlock, LoweredCall,
    LoweredCallableValue, LoweredCoro, LoweredExpression, LoweredFunction, LoweredFunctionInstance,
    LoweredInitializer, LoweredModuleInfo, LoweredPattern, LoweredProgram, LoweredReactiveCallback,
    LoweredReactiveOperation, LoweredResourceProvider, LoweredResourceProviderId,
    LoweredResourceUse, LoweredResourceUseId, LoweredRuntimeRequirements, LoweredSemanticIds,
    LoweredSymbol, LoweredWith, PatternId, PlaceId,
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
    /// The emission backend read view. Read-only by construction: every
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

    /// Resolve an owner's runtime block without exposing either arena.
    pub(crate) fn block(&self, owner: EmissionOwner, id: BlockId) -> Option<&'a LoweredBlock> {
        self.owner(owner)?.block(self.program, id)
    }

    pub(crate) fn item(&self, owner: EmissionOwner, id: ItemId) -> Option<&'a super::LoweredItem> {
        self.owner(owner)?.item(self.program, id)
    }

    pub(crate) fn expression(
        &self,
        owner: EmissionOwner,
        id: ExpressionId,
    ) -> Option<&'a LoweredExpression> {
        self.owner(owner)?.expression(self.program, id)
    }

    pub(crate) fn call(
        &self,
        owner: EmissionOwner,
        id: super::LoweredCallId,
    ) -> Option<&'a LoweredCall> {
        self.owner(owner)?.call(self.program, id)
    }

    /// The reactive operation one call performs, resolved through the owned
    /// arenas so the emitter can name the operation's construct family.
    pub(crate) fn reactive_operation(
        &self,
        owner: EmissionOwner,
        id: super::LoweredReactiveOperationId,
    ) -> Option<&'a super::LoweredReactiveOperation> {
        self.owner(owner)?.reactive_operation(self.program, id)
    }

    pub(crate) fn pattern(
        &self,
        owner: EmissionOwner,
        id: PatternId,
    ) -> Option<&'a LoweredPattern> {
        self.owner(owner)?.pattern(self.program, id)
    }

    /// Resolve an owner's place (emission's `emit_place_pointer`).
    pub(crate) fn place(
        &self,
        owner: EmissionOwner,
        id: PlaceId,
    ) -> Option<&'a super::LoweredPlace> {
        self.owner(owner)?.place(self.program, id)
    }

    /// Resolve an owner's `with` .
    pub(crate) fn with(
        &self,
        owner: EmissionOwner,
        id: super::LoweredWithId,
    ) -> Option<&'a LoweredWith> {
        self.owner(owner)?.with(self.program, id)
    }

    pub(crate) fn callable_value(
        &self,
        owner: EmissionOwner,
        id: super::LoweredCallableValueId,
    ) -> Option<&'a LoweredCallableValue> {
        self.owner(owner)?.callable_value(self.program, id)
    }

    pub(crate) fn coro(
        &self,
        owner: EmissionOwner,
        id: super::LoweredCoroId,
    ) -> Option<&'a super::LoweredCoro> {
        self.owner(owner)?.coro(self.program, id)
    }

    /// Resolve an owner's resource provider without exposing either arena
    /// (emission reads a body's `function_providers` through this).
    pub(crate) fn resource_provider(
        &self,
        owner: EmissionOwner,
        id: LoweredResourceProviderId,
    ) -> Option<&'a LoweredResourceProvider> {
        self.owner(owner)?.resource_provider(self.program, id)
    }

    /// The entry providers one module initializer installs, in the order of
    /// `LoweredInitializer::resources`. Lowering seeds them into the program
    /// arena as `EntryParameter` providers owned by the initializer's module,
    /// in exactly that order, so the emitter binds each entry resource to the
    /// provider its `LoweredResourceUse` records name. `None` when the count
    /// disagrees with the initializer's resources.
    pub(crate) fn initializer_entry_providers(
        &self,
        initializer: InitializerId,
    ) -> Option<Vec<LoweredResourceProviderId>> {
        let record = self.program.initializers.get(initializer)?;
        let owner = super::ExpressionOwner::Module(record.module);
        let providers = self
            .program
            .resource_providers
            .iter()
            .filter(|(_, provider)| {
                provider.owner == owner
                    && provider.kind == super::LoweredProviderOriginKind::EntryParameter
            })
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        (providers.len() == record.resources.len()).then_some(providers)
    }

    /// Resolve an owner's resource use without exposing either arena.
    pub(crate) fn resource_use(
        &self,
        owner: EmissionOwner,
        id: LoweredResourceUseId,
    ) -> Option<&'a LoweredResourceUse> {
        self.owner(owner)?.resource_use(self.program, id)
    }

    /// Resolve an owner's await suspension record .
    pub(crate) fn await_record(
        &self,
        owner: EmissionOwner,
        id: super::LoweredAwaitId,
    ) -> Option<&'a LoweredAwait> {
        self.owner(owner)?.await_record(self.program, id)
    }

    /// Resolve an owner's reactive callback record .
    pub(crate) fn reactive_callback(
        &self,
        owner: EmissionOwner,
        id: super::LoweredReactiveCallbackId,
    ) -> Option<&'a LoweredReactiveCallback> {
        self.owner(owner)?.reactive_callback(self.program, id)
    }

    pub(crate) fn binding(
        &self,
        owner: EmissionOwner,
        site: LoweredBindingSite,
    ) -> Option<&'a LoweredBoundTarget> {
        match owner {
            EmissionOwner::Instance(id) => self.instance_bindings(id)?.get(&site),
            EmissionOwner::Initializer(id) => self.initializer_bindings(id)?.get(&site),
        }
    }

    #[cfg(test)]
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

    /// The `LoweredSemanticIds` layout context (`runtime_opaque_kind`, string
    /// representation, entry resources, standard trait IDs).
    pub(crate) fn semantic_ids(&self) -> &'a LoweredSemanticIds {
        &self.program.semantic_ids
    }

    /// The catalog's concrete `Copy` decision: the shared layout/ABI layer's
    /// `is_copy` predicate for fully substituted types .
    pub(crate) fn concrete_is_copy(&self, value_type: &crate::CheckedType) -> bool {
        self.program.concrete_is_copy(value_type)
    }

    #[cfg(test)]
    /// The catalog's concrete needs-drop decision: the shared predicate the
    /// checker also uses .
    pub(crate) fn concrete_needs_drop(&self, value_type: &crate::CheckedType) -> bool {
        self.program.concrete_needs_drop(value_type)
    }

    /// The opaque runtime type identity of a fully substituted type, the same
    /// selection `LoweredProgram::runtime_opaque_kind` makes (emission layout
    /// context).
    pub(crate) fn runtime_opaque_kind(
        &self,
        value_type: &crate::CheckedType,
    ) -> Option<super::instance_resolution::RuntimeOpaqueKind> {
        self.program.runtime_opaque_kind(value_type)
    }

    /// The ordered runtime surfaces the closed catalog needs.
    pub(crate) fn runtime_requirements(&self) -> &'a LoweredRuntimeRequirements {
        &self.program.runtime_requirements
    }

    /// The planned emitted name of one interned source-function instance.
    pub(crate) fn planned_name(&self, instance: FunctionInstanceId) -> Option<&'a str> {
        self.program.planned_name(instance)
    }

    /// The planned emitted name of one interned generated artifact.
    pub(crate) fn planned_artifact_name(&self, ordinal: ArtifactOrdinal) -> Option<&'a str> {
        self.program.planned_artifact_name(ordinal)
    }

    /// The two planned names of a coroutine pair : derived by the
    /// catalog from the pair artifact's planned name, which collision-checks
    /// both against every other planned name.
    pub(crate) fn planned_coroutine_pair_names(
        &self,
        ordinal: ArtifactOrdinal,
    ) -> Option<(String, String)> {
        self.program.planned_coroutine_pair_names(ordinal)
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

    /// The concrete dispatch bindings of one module initializer.
    pub(crate) fn initializer_bindings(
        &self,
        initializer: InitializerId,
    ) -> Option<&'a BTreeMap<LoweredBindingSite, LoweredBoundTarget>> {
        self.program.initializer_bindings.get(initializer.index())
    }

    #[cfg(test)]
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

    /// Recorded state-access layout under this owner's substitutions.
    pub(crate) fn initialization_state_only(&self, owner: EmissionOwner, symbol: SymbolId) -> bool {
        if let EmissionOwner::Instance(id) = owner {
            if let Some(body) = self.instance_body(id) {
                for (_, item) in body.items.iter() {
                    if let super::LoweredItemKind::Binding(binding) = &item.kind
                        && binding.symbol == Some(symbol)
                    {
                        return binding.initialization_state_only;
                    }
                }
                for (_, pattern) in body.patterns.iter() {
                    if let super::LoweredPatternKind::Binding {
                        symbol: Some(bound),
                        initialization_state_only,
                        ..
                    } = &pattern.kind
                        && *bound == symbol
                    {
                        return *initialization_state_only;
                    }
                }
                if let Some(parameter) = body
                    .parameters
                    .iter()
                    .find(|parameter| parameter.symbol == symbol)
                {
                    return parameter.initialization_state_only;
                }
                if let Some(capture) = body
                    .captures
                    .iter()
                    .find(|capture| capture.capture.symbol == symbol)
                {
                    return capture.initialization_state_only;
                }
            }
        }
        self.symbol(symbol)
            .is_some_and(|symbol| symbol.initialization_state_only)
    }

    /// The concrete type of one owner-local symbol under the owner's
    /// substitutions: a binding item or binding pattern, a parameter, or a
    /// capture of the owner's body. An initializer's symbols take the program
    /// catalog's declared type, which is concrete. An instance never falls
    /// back to the catalog: a generic template's symbol keeps its template
    /// type there, so a symbol missing from the body is `None`, not a type
    /// that may still hold a parameter.
    pub(crate) fn owner_symbol_type(
        &self,
        owner: EmissionOwner,
        symbol: SymbolId,
    ) -> Option<&'a crate::CheckedType> {
        if let EmissionOwner::Instance(id) = owner {
            let body = self.instance_body(id)?;
            if let Some(value_type) = body.binding_symbol_type(symbol) {
                return Some(value_type);
            }
            if let Some(parameter) = body
                .parameters
                .iter()
                .find(|parameter| parameter.symbol == symbol)
            {
                return Some(&parameter.value_type);
            }
            return body
                .captures
                .iter()
                .find(|capture| capture.capture.symbol == symbol)
                .map(|capture| &capture.value_type);
        }
        self.symbol(symbol).map(|symbol| &symbol.value_type)
    }

    #[cfg(test)]
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

    /// The closure-phase artifact uses of one owner, in scan order: the drop
    /// glue, finalizers, coroutine pairs, runners, and adapters each site
    /// emits. `None` for an unknown owner or a body-less instance.
    pub(crate) fn artifact_uses(&self, owner: EmissionOwner) -> Option<&'a [LoweredArtifactUse]> {
        match owner {
            EmissionOwner::Instance(instance) => self
                .instance_body(instance)
                .map(|body| body.artifact_uses.as_slice()),
            EmissionOwner::Initializer(initializer) => {
                self.program.initializers.get(initializer)?;
                Some(
                    self.program
                        .initializer_artifact_uses
                        .get(initializer.index())
                        .map_or(&[][..], Vec::as_slice),
                )
            }
        }
    }

    /// The closure-phase source-function instance uses of one owner, in scan
    /// order (for example a buffer clone's element `Clone` instance).
    pub(crate) fn instance_uses(&self, owner: EmissionOwner) -> Option<&'a [LoweredInstanceUse]> {
        match owner {
            EmissionOwner::Instance(instance) => self
                .instance_body(instance)
                .map(|body| body.instance_uses.as_slice()),
            EmissionOwner::Initializer(initializer) => {
                self.program.initializers.get(initializer)?;
                Some(
                    self.program
                        .initializer_instance_uses
                        .get(initializer.index())
                        .map_or(&[][..], Vec::as_slice),
                )
            }
        }
    }

    /// The owned bindings of one owner in registration order, each with the
    /// drop glue its scope-exit cleanup calls. An initializer's records cover
    /// nested block locals only; module globals are never owned.
    pub(crate) fn owned_bindings(&self, owner: EmissionOwner) -> Option<&'a [LoweredOwnedBinding]> {
        match owner {
            EmissionOwner::Instance(instance) => self
                .instance_body(instance)
                .map(|body| body.owned_bindings.as_slice()),
            EmissionOwner::Initializer(initializer) => {
                self.program.initializers.get(initializer)?;
                Some(
                    self.program
                        .initializer_owned_bindings
                        .get(initializer.index())
                        .map_or(&[][..], Vec::as_slice),
                )
            }
        }
    }

    fn instance_body(&self, instance: FunctionInstanceId) -> Option<&'a LoweredInstanceBody> {
        self.program
            .instances
            .get(instance)
            .and_then(|record| record.body.as_ref())
    }
}

/// The owner whose cleanup sites a scan reads: a materialized instance body or
/// a module initializer. Instance bodies own private arenas; initializer sites
/// index the program's template arenas.
///
/// Artifact planning defined this resolver for its scanner; emission promotes it to
/// the shared read-only owner view the emitter and every scanner use.
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
    /// lends the emission binding tables without exposing an arena.
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
        assert_eq!(view.planned_name(instance), Some(record.name.as_str()));
    }

    /// The per-owner cleanup and use records are readable for both owner
    /// shapes through one accessor: an initializer `coro` creation's pair
    /// and an instance's owned block local, each with its use record.
    #[test]
    fn emission_view_lends_owner_uses_and_owned_bindings() {
        let lowered = lower(concat!(
            "use std.coroutine.*\n",
            "use std.cinterop.(CString, c_string)\n",
            "def keep: I32 -> I32 = value => { let text = c_string \"y\"; value }\n",
            "let started = coro { 1 }\n",
            "let kept = keep 1\n",
        ));
        let program = &lowered.program;
        let view = lowered.program();
        let entry = view
            .modules()
            .find(|(_, module)| module.executable_entry)
            .map(|(_, module)| module.initializer)
            .expect("an entry initializer");
        let owner = EmissionOwner::Initializer(entry);
        let uses = view
            .artifact_uses(owner)
            .expect("initializer artifact uses");
        assert!(
            uses.iter()
                .any(|use_| matches!(use_.site, super::super::ArtifactUseSite::CoroCreation(_))),
            "the initializer coroutine pair use is readable: {uses:?}"
        );
        assert_eq!(
            view.owned_bindings(owner).map(<[_]>::len),
            program
                .initializer_owned_bindings
                .get(entry.index())
                .map(Vec::len)
        );
        assert_eq!(
            view.instance_uses(owner).map(<[_]>::len),
            program
                .initializer_instance_uses
                .get(entry.index())
                .map(Vec::len)
        );

        let keep = view
            .instances()
            .find(|(_, instance)| {
                view.function(instance.template)
                    .is_some_and(|function| function.name == "keep")
            })
            .map(|(id, _)| EmissionOwner::Instance(id))
            .expect("keep has an instance");
        let owned = view.owned_bindings(keep).expect("instance owned bindings");
        let glue = owned
            .iter()
            .find_map(|binding| binding.glue)
            .expect("the droppable block local is owned with its glue");
        assert!(
            view.artifact_uses(keep)
                .is_some_and(|uses| uses.iter().any(|use_| use_.artifact == glue)),
            "the owned binding's glue has its use record"
        );
        assert!(
            view.owned_bindings(EmissionOwner::Initializer(InitializerId::from_index(
                10_000
            )))
            .is_none(),
            "an unknown owner has no records"
        );
    }

    /// Entry resources bind to the exact `EntryParameter` providers lowering
    /// seeded, in resource order, so the emitter never selects a provider by
    /// type.
    #[test]
    fn initializer_entry_providers_follow_the_entry_resources() {
        let lowered = lower("use std.io.println\nprintln \"hi\"\n");
        let view = lowered.program();
        let (entry, record) = view
            .initializers()
            .find(|(_, initializer)| initializer.executable_entry)
            .expect("an entry initializer");
        assert!(!record.resources.is_empty(), "the entry installs resources");
        let providers = view
            .initializer_entry_providers(entry)
            .expect("one provider per entry resource");
        for (resource, provider) in record.resources.iter().zip(&providers) {
            let provider = view
                .resource_provider(EmissionOwner::Initializer(entry), *provider)
                .expect("the provider resolves in the initializer's arena");
            assert_eq!(provider.resource, resource.resource);
            assert_eq!(
                provider.kind,
                super::super::LoweredProviderOriginKind::EntryParameter
            );
        }
        for (id, _) in view.initializers() {
            if id != entry {
                assert_eq!(
                    view.initializer_entry_providers(id)
                        .map(|providers| providers.len()),
                    view.initializer(id).map(|record| record.resources.len()),
                );
            }
        }
    }
}
