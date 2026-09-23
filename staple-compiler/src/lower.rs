//! Typed lowering boundary and owned lowered representation.
//!
//! Lowering owns the transition from a successfully checked program to the
//! representation consumed by code generation. The explicit arenas are being
//! populated incrementally during Stage 2. The existing typed module remains
//! a temporary, private backend bridge until code generation is migrated.

#![allow(dead_code)] // Stage 2 populates and consumes this schema incrementally.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;

use staple_syntax::{Diagnostic, Expression, Item, Pattern, Span, SyntaxId};

use crate::{
    BuiltinType, CheckedAccess, CheckedCoercion, CheckedEffectSet, CheckedFunctionType,
    CheckedFunctionalDependency, CheckedPropagation, CheckedResource, CheckedTraitBound,
    CheckedTraitDispatch, CheckedType, DefinitionId, FloatType, FunctionId, IntegerType, ModuleId,
    RecursiveConstruction, ResolvedFunction, ResolvedModule, SourceModule, SymbolId, TraitId,
    TraitMethodId, TypeId, TypeParameterId, TypedModule,
};

macro_rules! arena_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $name(usize);

        impl ArenaId for $name {
            fn from_index(index: usize) -> Self {
                Self(index)
            }

            fn index(self) -> usize {
                self.0
            }
        }
    };
}

trait ArenaId: Copy {
    fn from_index(index: usize) -> Self;
    fn index(self) -> usize;
}

arena_id!(ExpressionId);
arena_id!(PatternId);
arena_id!(PlaceId);
arena_id!(BlockId);
arena_id!(ItemId);
arena_id!(LoweredFunctionId);
arena_id!(InitializerId);
arena_id!(LoweredModuleId);
arena_id!(LoweredSymbolId);
arena_id!(LoweredTypeId);
arena_id!(LoweredTraitId);
arena_id!(LoweredTraitMethodId);
arena_id!(LoweredTraitImplementationId);

/// Deterministic, append-only storage whose handles cannot be mixed with
/// handles from another lowered-node family.
#[derive(Debug, Clone)]
struct Arena<T, I> {
    values: Vec<T>,
    id: PhantomData<fn() -> I>,
}

impl<T, I> Default for Arena<T, I> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            id: PhantomData,
        }
    }
}

impl<T, I: ArenaId> Arena<T, I> {
    fn push(&mut self, value: T) -> I {
        let id = I::from_index(self.values.len());
        self.values.push(value);
        id
    }

    fn contains(&self, id: I) -> bool {
        id.index() < self.values.len()
    }

    fn get(&self, id: I) -> Option<&T> {
        self.values.get(id.index())
    }

    fn iter(&self) -> impl Iterator<Item = (I, &T)> {
        self.values
            .iter()
            .enumerate()
            .map(|(index, value)| (I::from_index(index), value))
    }
}

#[derive(Debug, Clone)]
struct CatalogEntry<K, T> {
    key: K,
    origin: Origin,
    value: T,
}

/// An insertion-ordered semantic catalog with a separate lookup index.
///
/// The ordered arena is authoritative for traversal. The map is deliberately
/// validated in both directions so later phases cannot observe a stale or
/// overwritten semantic-ID lookup.
#[derive(Debug, Clone)]
struct Catalog<K, T, I> {
    entries: Arena<CatalogEntry<K, T>, I>,
    by_key: HashMap<K, I>,
}

impl<K, T, I> Default for Catalog<K, T, I> {
    fn default() -> Self {
        Self {
            entries: Arena::default(),
            by_key: HashMap::new(),
        }
    }
}

impl<K, T, I> Catalog<K, T, I>
where
    K: Copy + Debug + Eq + Hash,
    I: ArenaId + Eq,
{
    fn insert(&mut self, kind: &str, key: K, origin: Origin, value: T) -> Result<I, Diagnostic> {
        if self.by_key.contains_key(&key) {
            return Err(Diagnostic::new(
                origin.span,
                format!("duplicate lowered {kind} semantic id {key:?}"),
            ));
        }
        let id = self.entries.push(CatalogEntry { key, origin, value });
        self.by_key.insert(key, id);
        Ok(id)
    }

    fn get(&self, key: K) -> Option<&T> {
        self.by_key
            .get(&key)
            .and_then(|id| self.entries.get(*id))
            .map(|entry| &entry.value)
    }

    fn iter(&self) -> impl Iterator<Item = (I, K, &T)> {
        self.entries
            .iter()
            .map(|(id, entry)| (id, entry.key, &entry.value))
    }

    fn validate(&self, kind: &str) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut keys = HashSet::new();
        for (id, entry) in self.entries.iter() {
            if !keys.insert(entry.key) {
                diagnostics.push(Diagnostic::new(
                    entry.origin.span.clone(),
                    format!("lowered {kind} catalog repeats semantic id {:?}", entry.key),
                ));
            }
            if self.by_key.get(&entry.key) != Some(&id) {
                diagnostics.push(Diagnostic::new(
                    entry.origin.span.clone(),
                    format!(
                        "lowered {kind} catalog lookup disagrees for semantic id {:?}",
                        entry.key
                    ),
                ));
            }
        }
        for (key, id) in &self.by_key {
            let Some(entry) = self.entries.get(*id) else {
                diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!("lowered {kind} catalog has dangling lookup for semantic id {key:?}"),
                ));
                continue;
            };
            if entry.key != *key {
                diagnostics.push(Diagnostic::new(
                    entry.origin.span.clone(),
                    format!(
                        "lowered {kind} catalog lookup points to semantic id {:?} instead of {key:?}",
                        entry.key
                    ),
                ));
            }
        }
        diagnostics
    }
}

/// Source identity retained on every lowered node for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Origin {
    pub syntax: SyntaxId,
    pub span: Span,
}

impl Origin {
    fn compiler() -> Self {
        Self {
            syntax: SyntaxId::COMPILER,
            span: Span::Compiler,
        }
    }
}

/// The runtime owner of a lowered expression occurrence. Every reachable
/// runtime expression belongs to exactly one function template or module
/// initializer, and the owner participates in the expression memo key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ExpressionOwner {
    Module(ModuleId),
    Function(FunctionId),
}

/// Additional occurrence identity for contextual expressions. A product
/// type's declared default expression is one shared AST node that may be
/// evaluated at many construction sites with different checked types, so the
/// consuming product expression and destination slot must distinguish those
/// occurrences. Ordinary source occurrences are `Primary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ExpressionContext {
    Primary,
    ContextualDefault { consumer: SyntaxId, slot: usize },
}

/// Occurrence-aware expression memo key. Ordinary occurrences deduplicate by
/// source syntax and owner; contextual defaults additionally carry the
/// consuming product and destination slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ExpressionKey {
    pub syntax: SyntaxId,
    pub owner: ExpressionOwner,
    pub context: ExpressionContext,
}

/// A Stage 2.4-owned expression family. Every ordinary syntax variant maps to
/// exactly one family, and a family reaches its concrete lowered form no later
/// than the step that owns it. `Pending` is the transitional marker for
/// families whose concrete payload has not landed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage24Family {
    Block,
    Satisfies,
    Match,
    Loop,
    Product,
    RepeatedProduct,
    Access,
    Index,
    Logical,
    Name,
    String,
    StringTemplate,
    CString,
    Integer,
    Float,
}

/// An expression family explicitly deferred to a later lowering stage.
/// Deferred nodes are not silently unlowered: later stages own them whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferredExpressionFamily {
    /// Stage 2.5 owns function values and calls.
    Callable,
    /// Stage 2.6 owns `with` and resource access.
    Resource,
    /// Stage 2.6 owns coroutine construction and `await`.
    Coroutine,
}

/// The single lowering decision for a syntax variant. Exhaustiveness is
/// enforced by a match, and the coverage classifier test fails when a new
/// variant is missing from the enumerated list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpressionDisposition {
    Ordinary(Stage24Family),
    Deferred(DeferredExpressionFamily),
    /// Compile-time-only survivors that earlier phases must eliminate.
    Rejected,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredExpression {
    pub key: ExpressionKey,
    pub origin: Origin,
    pub value_type: CheckedType,
    pub effects: CheckedEffectSet,
    pub coercion: Option<CheckedCoercion>,
    pub moved_symbols: Vec<SymbolId>,
    pub kind: LoweredExpressionKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredExpressionKind {
    /// A Stage 2.4-owned family awaiting its concrete payload. Every ordinary
    /// expression is classified explicitly, so no ambiguous `Unlowered`
    /// placeholder exists; validation rejects leftovers once Stage 2.4 ends.
    Pending(Stage24Family),
    /// Explicitly deferred to Stage 2.5 or Stage 2.6 with its owning family.
    Deferred(DeferredExpressionFamily),
    Block(BlockId),
    /// An ordinary value read: a local, parameter, global, mutable cell,
    /// captured cell, or singleton. Callable-valued names and constructors are
    /// deferred instead of appearing here.
    Name(LoweredName),
    Integer(LoweredInteger),
    Float(LoweredFloat),
    String(LoweredString),
    CString(LoweredCString),
    /// A structural representation, product, slice, or scalar access.
    Access(LoweredAccess),
}

/// An ordinary value read. The symbol catalog supplies the storage
/// classification; initialization checking, movement, and singleton identity
/// are copied from the checked occurrence.
#[derive(Debug, Clone)]
pub(crate) struct LoweredName {
    pub symbol: SymbolId,
    pub storage: SymbolStorage,
    pub requires_initialization_check: bool,
    /// The symbol's storage is mutable and reads must go through its cell.
    pub mutable: bool,
    /// The symbol is reached through a shared capture cell.
    pub captured_cell: bool,
    /// This occurrence transfers the symbol's value.
    pub moved: bool,
    /// The symbol is a `move`-marked parameter.
    pub move_parameter: bool,
    /// Singleton type identity when the name denotes a singleton value.
    pub singleton: Option<TypeId>,
}

/// A checked integer literal. The magnitude is parsed once and validated
/// against the selected scalar type at lowering time.
#[derive(Debug, Clone)]
pub(crate) struct LoweredInteger {
    pub value: u64,
    pub integer_type: IntegerType,
}

/// A checked float literal with the exact finite value selected by checking.
/// `F32` literals are stored widened to `f64` exactly as the backend does.
#[derive(Debug, Clone)]
pub(crate) struct LoweredFloat {
    pub value: f64,
    pub float_type: FloatType,
}

/// A string literal decoded to UTF-8 once during lowering.
#[derive(Debug, Clone)]
pub(crate) struct LoweredString {
    pub value: String,
}

/// A C string literal decoded once. `bytes` includes the required trailing
/// NUL and never contains an interior NUL.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCString {
    pub bytes: Vec<u8>,
}

/// A structural access with its base expression and checked dereference path.
#[derive(Debug, Clone)]
pub(crate) struct LoweredAccess {
    pub base: ExpressionId,
    pub kind: LoweredAccessKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredAccessKind {
    /// Distinct representation or single-element distinct access.
    Representation { dereference: Vec<CheckedType> },
    /// Element `index` of a fixed product, optionally behind `Ref` payloads.
    Product {
        index: usize,
        dereference: Vec<CheckedType>,
    },
    /// Element `index` of a `Slice`, bounds-checked at runtime.
    Slice {
        index: usize,
        dereference: Vec<CheckedType>,
    },
    /// The single-element shortcut, read directly after any dereference.
    Scalar { dereference: Vec<CheckedType> },
}

/// A source pattern with its checked type and lowered children. Patterns are
/// shared by function parameter templates, runtime pattern bindings, and
/// (from Stage 2.4 on) match arms.
#[derive(Debug, Clone)]
pub(crate) struct LoweredPattern {
    pub origin: Origin,
    pub value_type: CheckedType,
    pub kind: LoweredPatternKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredPatternKind {
    Wildcard,
    Binding {
        /// The symbol this pattern binds. Absent for singleton patterns such
        /// as `True`, which name an existing value instead of binding one.
        symbol: Option<SymbolId>,
        /// The singleton type a name-like pattern selects, when it is not a
        /// binding.
        singleton: Option<TypeId>,
        mutable: bool,
        moved: bool,
    },
    Product {
        elements: Vec<PatternId>,
        mutable: bool,
        moved: bool,
    },
    /// A nominal destructuring pattern (`Ref inner`, `Some value`, ...) with
    /// the named type it selects and its argument pattern.
    Nominal {
        target: Option<TypeId>,
        name: String,
        /// Whether the whole nominal destructure transfers ownership.
        moved: bool,
        argument: PatternId,
    },
    /// A string literal pattern; the literal is retained exactly as written.
    Literal {
        literal: String,
    },
    At {
        binding: PatternId,
        pattern: PatternId,
    },
}

/// A normalized assignment target. Places exist only as mutation destinations;
/// code generation turns them back into storage pointers without consulting
/// the source AST.
#[derive(Debug, Clone)]
pub(crate) struct LoweredPlace {
    pub origin: Origin,
    pub value_type: CheckedType,
    pub kind: LoweredPlaceKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredPlaceKind {
    /// Direct storage of a local, parameter, or global symbol.
    Symbol { symbol: SymbolId },
    /// Symbol storage reached through a shared capture cell.
    CapturedCell { symbol: SymbolId },
    /// A non-place base materialized into temporary storage so it can be
    /// mutated (`x[i] = v` where `x` is not itself a place root).
    Temporary { expression: ExpressionId },
    /// An ambient resource value in scope.
    Resource { resource: CheckedResource },
    /// A pointer into a reference value: `reference` evaluates the `Ref`
    /// container and `dereference` records the crossed payloads
    /// outermost-first, matching `CheckedAccess`.
    Dereference {
        reference: ExpressionId,
        dereference: Vec<CheckedType>,
    },
    /// Element `index` of a product place. `slice` marks a `Slice` base,
    /// whose pointer is loaded from the slice representation and bounds
    /// checked rather than projected directly.
    ProductElement {
        base: PlaceId,
        index: usize,
        slice: bool,
    },
    /// The representation pointer of a distinct value, covering both `.*`
    /// and single-element distinct access.
    Representation { base: PlaceId },
    /// `base[index]` mutation, dispatched through the `MutateIndex` trait.
    Indexed { base: PlaceId, index: ExpressionId },
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredBlock {
    pub origin: Origin,
    pub items: Vec<ItemId>,
    pub result: Option<ExpressionId>,
}

/// One runtime item inside a module initializer or a runtime block.
#[derive(Debug, Clone)]
pub(crate) struct LoweredItem {
    pub origin: Origin,
    pub kind: LoweredItemKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredItemKind {
    Binding(LoweredBindingItem),
    PatternBinding(LoweredPatternBindingItem),
    Assignment(LoweredAssignmentItem),
    Return(LoweredReturnItem),
    Break(LoweredBreakItem),
    Continue,
    Expression(LoweredExpressionStatementItem),
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredBindingItem {
    /// The bound runtime symbol. Absent for compile-time-only `const`
    /// bindings, which stay outside the runtime symbol catalog.
    pub symbol: Option<SymbolId>,
    pub value: Option<ExpressionId>,
    pub compile_time_only: bool,
    /// The binding declares compile-time parameters, so code generation only
    /// records its initialization state.
    pub generic: bool,
    pub derived: bool,
    pub signal: bool,
    /// The binding's value lives in a local binding cell rather than an SSA
    /// value or module global storage.
    pub cell: bool,
    pub requires_initialization_check: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredPatternBindingItem {
    pub pattern: PatternId,
    pub value: ExpressionId,
    /// `true` for `let pattern? = value` propagation.
    pub propagating: bool,
    /// Checked propagation metadata, present exactly for propagating
    /// bindings.
    pub propagation: Option<CheckedPropagation>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredAssignmentItem {
    pub target: PlaceId,
    pub value: ExpressionId,
    /// Selected `MutateIndex` dispatch for an indexed target.
    pub mutate_index: Option<CheckedTraitDispatch>,
    /// The place's root symbol, whose initialization state is written back.
    pub initialization_symbol: Option<SymbolId>,
    /// Whether the place's previous value must be dropped before the store.
    pub drop_previous: bool,
    /// Whether the assignment must notify the symbol's signal metadata.
    pub signal: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredReturnItem {
    pub value: ExpressionId,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredBreakItem {
    pub value: Option<ExpressionId>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredExpressionStatementItem {
    pub expression: ExpressionId,
    /// Whether the discarded result needs a drop after evaluation.
    pub drop_result: bool,
}

/// One capture of a function template, carrying the ownership facts code
/// generation currently reads from the type checker.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCapture {
    pub symbol: SymbolId,
    pub borrowed: bool,
    pub non_owning: bool,
    /// The capture must be reached through a shared cell (mutable storage,
    /// derived binding, or initialization state) rather than by value.
    pub requires_cell: bool,
}

/// Orthogonal function-template classifications. A thunk can be a derived
/// evaluator and a resource helper at the same time, so these are explicit
/// flags rather than a single enum.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LoweredFunctionClass {
    pub declared: bool,
    pub implicit_thunk: bool,
    pub derived_evaluator: bool,
    pub coroutine_body: bool,
    pub resource_helper: bool,
    pub external: bool,
    pub intrinsic: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredFunction {
    pub origin: Origin,
    pub semantic_id: FunctionId,
    pub name: String,
    pub module: ModuleId,
    pub binding_symbol: Option<SymbolId>,
    pub signature: CheckedFunctionType,
    pub bounds: Vec<CheckedTraitBound>,
    pub parameter_style: staple_syntax::FunctionParameterStyle,
    /// The lowered parameter pattern, with bound symbols in source order.
    pub parameter_pattern: PatternId,
    pub parameters: Vec<SymbolId>,
    pub captures: Vec<LoweredCapture>,
    pub body_origin: Origin,
    pub body_syntax: SyntaxId,
    pub body: Option<BlockId>,
    pub class: LoweredFunctionClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredEntryResourceKind {
    Io,
    Reactive,
}

/// An IO or reactive resource the executable entry installs before it runs
/// its initializer items. Recorded as initializer metadata rather than as
/// synthetic runtime AST nodes.
#[derive(Debug, Clone)]
pub(crate) struct LoweredEntryResource {
    pub kind: LoweredEntryResourceKind,
    pub resource: CheckedResource,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredInitializer {
    pub origin: Origin,
    pub module: ModuleId,
    pub executable_entry: bool,
    pub resources: Vec<LoweredEntryResource>,
    /// The module's ordered runtime items, lowered into `body`'s item list.
    pub body: BlockId,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredModuleInfo {
    pub origin: Origin,
    pub semantic_id: ModuleId,
    pub qualified_name: String,
    pub parent: Option<ModuleId>,
    pub companion: bool,
    pub initialization_index: usize,
    pub executable_entry: bool,
    pub initializer: InitializerId,
}

/// The primary storage category of a symbol. Independent facts (mutation,
/// moves, initialization checking, capture-cell use) stay in explicit
/// `LoweredSymbol` flags rather than overloading this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolStorage {
    ImmutableValue,
    MutableCell,
    GlobalStorage,
    FunctionBinding,
    DerivedBinding,
    Signal,
    CapturedCell,
    ExternalSymbol,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredSymbol {
    pub origin: Origin,
    pub semantic_id: SymbolId,
    pub module: ModuleId,
    pub owner: Option<FunctionId>,
    pub value_type: CheckedType,
    pub storage: SymbolStorage,
    pub requires_initialization_check: bool,
    pub derived: bool,
    pub signal: bool,
    pub mutated_parameter: bool,
    pub move_parameter: bool,
    pub captured_cell: bool,
    pub function: Option<FunctionId>,
    pub constructor: Option<TypeId>,
    pub singleton: Option<TypeId>,
    pub intrinsic: Option<crate::IntrinsicFunction>,
    pub external: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredTypeKind {
    Alias,
    Distinct,
    Opaque,
    Singleton,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredTypeMetadata {
    pub origin: Origin,
    pub semantic_id: TypeId,
    pub name: String,
    pub module: ModuleId,
    pub kind: LoweredTypeKind,
    pub builtin: Option<BuiltinType>,
    pub recursive_construction: Option<RecursiveConstruction>,
    /// Checked parameter templates in declaration order.
    pub parameters: Vec<CheckedType>,
    /// Compact representation template; nested nominal types are references.
    /// Absent for opaque types without a representation.
    pub representation: Option<CheckedType>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredTraitMetadata {
    pub origin: Origin,
    pub semantic_id: TraitId,
    pub name: String,
    pub module: ModuleId,
    pub parameters: Vec<CheckedType>,
    pub prerequisites: Vec<CheckedTraitBound>,
    pub functional_dependencies: Vec<CheckedFunctionalDependency>,
    /// Declared method order.
    pub methods: Vec<TraitMethodId>,
    /// Default implementations, in declared method order.
    pub default_methods: Vec<(TraitMethodId, FunctionId)>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredTraitMethodMetadata {
    pub origin: Origin,
    pub semantic_id: TraitMethodId,
    pub name: String,
    pub trait_id: TraitId,
    pub value_type: CheckedType,
    pub default_function: Option<FunctionId>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredTraitImplementationMetadata {
    pub origin: Origin,
    pub trait_id: TraitId,
    /// Implementation-declared type parameters, ascending.
    pub parameters: Vec<TypeParameterId>,
    pub arguments: Vec<CheckedType>,
    pub bounds: Vec<CheckedTraitBound>,
    pub negative: bool,
    /// Selected method functions, ordered by semantic method ID.
    pub methods: Vec<(TraitMethodId, FunctionId)>,
}

/// Type-checker-owned standard and runtime subsystem identities. Optional
/// slots are absent in library-only or `no_prelude` programs, and every
/// present ID must resolve to the matching catalog family.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoweredSemanticIds {
    pub natural_trait: Option<TraitId>,
    pub sized_trait: Option<TraitId>,
    pub copy_trait: Option<TraitId>,
    pub drop_trait: Option<TraitId>,
    pub default_trait: Option<TraitId>,
    pub debug_trait: Option<TraitId>,
    pub display_trait: Option<TraitId>,
    pub index_trait: Option<TraitId>,
    pub mutate_index_trait: Option<TraitId>,
    pub into_iterator_trait: Option<TraitId>,
    pub iterator_trait: Option<TraitId>,
    pub io_type: Option<TypeId>,
    pub reactive_type: Option<TypeId>,
    pub coroutine_type: Option<TypeId>,
    pub task_type: Option<TypeId>,
    pub completed_type: Option<TypeId>,
    pub cancelled_type: Option<TypeId>,
    pub tasks_type: Option<TypeId>,
    pub scheduler_type: Option<TypeId>,
    pub wait_type: Option<TypeId>,
    pub resolver_type: Option<TypeId>,
    pub completion_token_type: Option<TypeId>,
    pub io_resource: Option<CheckedResource>,
    pub reactive_resource: Option<CheckedResource>,
    pub string_representation: Option<CheckedType>,
    pub entry_reactive_required: bool,
}

/// The complete owned Stage 2 representation. Arena order is insertion order,
/// which lowering defines to be deterministic program/source order.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoweredProgram {
    modules: Catalog<ModuleId, LoweredModuleInfo, LoweredModuleId>,
    expressions: Arena<LoweredExpression, ExpressionId>,
    /// Lookup only; traversal always uses the expression arena. Repeated
    /// lowering of the same occurrence key returns the first allocated ID.
    /// Contextual defaults use distinct keys so shared default syntax does not
    /// alias incompatible node types.
    expression_lookup: HashMap<ExpressionKey, ExpressionId>,
    patterns: Arena<LoweredPattern, PatternId>,
    places: Arena<LoweredPlace, PlaceId>,
    blocks: Arena<LoweredBlock, BlockId>,
    /// Lookup only; traversal always uses the block arena. Repeated lowering
    /// of the same block occurrence returns the first allocated ID.
    block_lookup: HashMap<ExpressionKey, BlockId>,
    items: Arena<LoweredItem, ItemId>,
    functions: Catalog<FunctionId, LoweredFunction, LoweredFunctionId>,
    symbols: Catalog<SymbolId, LoweredSymbol, LoweredSymbolId>,
    types: Catalog<TypeId, LoweredTypeMetadata, LoweredTypeId>,
    traits: Catalog<TraitId, LoweredTraitMetadata, LoweredTraitId>,
    trait_methods: Catalog<TraitMethodId, LoweredTraitMethodMetadata, LoweredTraitMethodId>,
    trait_implementations: Arena<LoweredTraitImplementationMetadata, LoweredTraitImplementationId>,
    initializers: Arena<LoweredInitializer, InitializerId>,
    semantic_ids: LoweredSemanticIds,
}

impl LoweredProgram {
    /// Copies deterministic declaration metadata out of checked compiler state.
    /// Symbols are snapshotted first so expression lowering can read storage
    /// facts from the catalog instead of the resolver.
    fn snapshot(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let mut diagnostics = self.snapshot_symbols(module);
        diagnostics.extend(self.snapshot_modules(module));
        diagnostics.extend(self.snapshot_functions(module));
        diagnostics.extend(self.snapshot_types(module));
        diagnostics.extend(self.snapshot_traits(module));
        diagnostics.extend(self.snapshot_semantic_ids(module));
        diagnostics
    }

    /// Copies type-checker-owned standard and runtime subsystem identities.
    /// No name lookup happens here; absent prelude or library-only programs
    /// keep whichever IDs the checker selected.
    fn snapshot_semantic_ids(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let ids = module.semantic_ids();
        self.semantic_ids = LoweredSemanticIds {
            natural_trait: ids.natural_trait,
            sized_trait: ids.sized_trait,
            copy_trait: ids.copy_trait,
            drop_trait: ids.drop_trait,
            default_trait: ids.default_trait,
            debug_trait: ids.debug_trait,
            display_trait: ids.display_trait,
            index_trait: ids.index_trait,
            mutate_index_trait: ids.mutate_index_trait,
            into_iterator_trait: ids.into_iterator_trait,
            iterator_trait: ids.iterator_trait,
            io_type: ids.io_type,
            reactive_type: ids.reactive_type,
            coroutine_type: ids.coroutine_type,
            task_type: ids.task_type,
            completed_type: ids.completed_type,
            cancelled_type: ids.cancelled_type,
            tasks_type: ids.tasks_type,
            scheduler_type: ids.scheduler_type,
            wait_type: ids.wait_type,
            resolver_type: ids.resolver_type,
            completion_token_type: ids.completion_token_type,
            io_resource: module.io_resource(),
            reactive_resource: module.reactive_resource(),
            string_representation: module.string_representation().cloned(),
            entry_reactive_required: module.entry_reactive_required(),
        };
        Vec::new()
    }

    /// Inserts type metadata in ascending `TypeId`, retaining origin, module,
    /// declaration kind, builtin/recursive classification, checked parameter
    /// templates, and the compact representation template.
    fn snapshot_types(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let resolved = module.resolved();
        let mut diagnostics = Vec::new();
        for (id, declaration) in resolved.types_in_id_order() {
            let origin = Origin {
                syntax: declaration.syntax.id,
                span: declaration.syntax.span.clone(),
            };
            let Some(module_id) = resolved.definition_module(DefinitionId::Type(id)) else {
                diagnostics.push(Diagnostic::new(
                    origin.span,
                    format!(
                        "cannot lower type `{}` (type id {}) without an owning module",
                        declaration.name, id.0
                    ),
                ));
                continue;
            };
            let kind = match declaration.kind() {
                staple_syntax::TypeDeclarationKind::Alias => LoweredTypeKind::Alias,
                staple_syntax::TypeDeclarationKind::Distinct => LoweredTypeKind::Distinct,
                staple_syntax::TypeDeclarationKind::Opaque => LoweredTypeKind::Opaque,
                staple_syntax::TypeDeclarationKind::Singleton => LoweredTypeKind::Singleton,
            };
            let value = LoweredTypeMetadata {
                origin: origin.clone(),
                semantic_id: id,
                name: declaration.name.clone(),
                module: module_id,
                kind,
                builtin: resolved.builtin_type(id),
                recursive_construction: resolved.recursive_construction(id),
                parameters: module.type_parameter_templates(id).to_vec(),
                representation: module.type_representation(id).cloned(),
            };
            if let Err(diagnostic) = self.types.insert("type", id, origin, value) {
                diagnostics.push(diagnostic);
            }
        }
        diagnostics
    }

    /// Inserts traits and their methods by semantic ID, preserving declared
    /// method order inside each trait, then records checked implementations in
    /// resolver declaration order.
    fn snapshot_traits(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let resolved = module.resolved();
        let mut diagnostics = Vec::new();
        let parameter_arguments = module
            .trait_parameter_arguments_in_id_order()
            .into_iter()
            .map(|(id, arguments)| (id, arguments.to_vec()))
            .collect::<HashMap<_, _>>();
        let method_types = module
            .trait_method_types_in_id_order()
            .into_iter()
            .map(|(id, value_type)| (id, value_type.clone()))
            .collect::<HashMap<_, _>>();
        let method_origins = resolved
            .trait_methods_in_id_order()
            .into_iter()
            .map(|(id, member)| {
                (
                    id,
                    (
                        Origin {
                            syntax: member.syntax.id,
                            span: member.syntax.span.clone(),
                        },
                        member.name.clone(),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        for (trait_id, trait_) in resolved.traits_in_id_order() {
            let origin = Origin {
                syntax: trait_.declaration.syntax.id,
                span: trait_.declaration.syntax.span.clone(),
            };
            let Some(module_id) = resolved.definition_module(DefinitionId::Trait(trait_id)) else {
                diagnostics.push(Diagnostic::new(
                    origin.span,
                    format!(
                        "cannot lower trait `{}` (trait id {}) without an owning module",
                        trait_.declaration.name, trait_id.0
                    ),
                ));
                continue;
            };
            let default_methods = trait_
                .methods
                .iter()
                .filter_map(|method| {
                    trait_
                        .default_methods
                        .get(method)
                        .map(|function| (*method, *function))
                })
                .collect();
            let value = LoweredTraitMetadata {
                origin: origin.clone(),
                semantic_id: trait_id,
                name: trait_.declaration.name.clone(),
                module: module_id,
                parameters: parameter_arguments
                    .get(&trait_id)
                    .cloned()
                    .unwrap_or_default(),
                prerequisites: module.trait_prerequisites(trait_id).to_vec(),
                functional_dependencies: module.trait_functional_dependencies(trait_id).to_vec(),
                methods: trait_.methods.clone(),
                default_methods,
            };
            if let Err(diagnostic) = self.traits.insert("trait", trait_id, origin, value) {
                diagnostics.push(diagnostic);
            }
            for method in &trait_.methods {
                let Some((origin, name)) = method_origins.get(method).cloned() else {
                    diagnostics.push(Diagnostic::new(
                        Span::Compiler,
                        format!(
                            "cannot lower trait method {method:?} without a declaration origin"
                        ),
                    ));
                    continue;
                };
                let Some(value_type) = method_types.get(method).cloned() else {
                    diagnostics.push(Diagnostic::new(
                        origin.span,
                        format!("cannot lower trait method {method:?} without a checked type"),
                    ));
                    continue;
                };
                let value = LoweredTraitMethodMetadata {
                    origin: origin.clone(),
                    semantic_id: *method,
                    name,
                    trait_id,
                    value_type,
                    default_function: trait_.default_methods.get(method).copied(),
                };
                if let Err(diagnostic) =
                    self.trait_methods
                        .insert("trait method", *method, origin, value)
                {
                    diagnostics.push(diagnostic);
                }
            }
        }

        let resolved_implementations = resolved.trait_implementations();
        let checked_implementations = module.checked_trait_implementations();
        if resolved_implementations.len() != checked_implementations.len() {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "lowered trait implementation count {} does not match resolver count {}",
                    checked_implementations.len(),
                    resolved_implementations.len()
                ),
            ));
        }
        for (declared, checked) in resolved_implementations.iter().zip(checked_implementations) {
            let mut parameters = checked.parameters.iter().copied().collect::<Vec<_>>();
            parameters.sort_by_key(|parameter| parameter.0);
            let mut methods = checked
                .methods
                .iter()
                .map(|(method, function)| (*method, *function))
                .collect::<Vec<_>>();
            methods.sort_by_key(|(method, _)| method.0);
            self.trait_implementations
                .push(LoweredTraitImplementationMetadata {
                    origin: Origin {
                        syntax: declared.syntax,
                        span: declared.span.clone(),
                    },
                    trait_id: checked.trait_id,
                    parameters,
                    arguments: checked.arguments.clone(),
                    bounds: checked.bounds.clone(),
                    negative: checked.negative,
                    methods,
                });
        }
        diagnostics
    }

    /// Enumerates resolver-declared runtime symbols in ascending `SymbolId`,
    /// supplementing any referenced but undeclared symbol with a compiler
    /// origin. Compile-time-only consts and macro quote placeholders stay out
    /// of the catalog.
    fn snapshot_symbols(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let resolved = module.resolved();
        let mut diagnostics = Vec::new();
        let declared = resolved.symbols_in_id_order();
        let origins = declared
            .iter()
            .map(|info| {
                (
                    info.id,
                    Origin {
                        syntax: info.declaration,
                        span: info.span.clone(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let mut captured = HashSet::new();
        let mut referenced = HashSet::new();
        for function in module
            .functions()
            .iter()
            .chain(module.implicit_thunks_in_id_order())
        {
            captured.extend(function.captures.iter().copied());
            referenced.extend(pattern_symbols(resolved, &function.pattern));
            referenced.extend(function.captures.iter().copied());
        }
        for info in &declared {
            if compile_time_only_symbol(module, info.id)
                || (!info.module_symbol && info.owner.is_none())
            {
                continue;
            }
            referenced.remove(&info.id);
            let origin = origins[&info.id].clone();
            if let Some(diagnostic) =
                self.snapshot_symbol(module, info.id, origin, info.module, info.owner, &captured)
            {
                diagnostics.push(diagnostic);
            }
        }
        for symbol in referenced {
            if self.symbols.get(symbol).is_some() || compile_time_only_symbol(module, symbol) {
                continue;
            }
            let Some(module_id) = resolved.symbol_module(symbol) else {
                diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!("referenced lowered symbol {symbol:?} has no owning module"),
                ));
                continue;
            };
            let origin = origins
                .get(&symbol)
                .cloned()
                .unwrap_or_else(Origin::compiler);
            if let Some(diagnostic) =
                self.snapshot_symbol(module, symbol, origin, module_id, None, &captured)
            {
                diagnostics.push(diagnostic);
            }
        }
        diagnostics.extend(initializer_symbol_diagnostics(
            module,
            &origins,
            &self.symbols,
        ));
        diagnostics
    }

    #[allow(clippy::too_many_arguments)]
    fn snapshot_symbol(
        &mut self,
        module: &TypedModule,
        symbol: SymbolId,
        origin: Origin,
        module_id: ModuleId,
        owner: Option<FunctionId>,
        captured: &HashSet<SymbolId>,
    ) -> Option<Diagnostic> {
        let resolved = module.resolved();
        let Some(value_type) = module.declared_type_of_symbol(symbol) else {
            return Some(Diagnostic::new(
                origin.span,
                format!("cannot lower symbol {symbol:?} without a declared checked type"),
            ));
        };
        let function = module.function_for_symbol(symbol);
        let constructor = resolved.constructor_type(symbol);
        let singleton = resolved.singleton_type(symbol);
        let intrinsic = resolved.intrinsic_function(symbol);
        let external = resolved.is_external_symbol(symbol);
        let derived = module.is_derived_symbol(symbol);
        let signal = resolved.is_signal_symbol(symbol);
        let mutable = module.has_mutable_storage(symbol);
        let module_symbol = resolved.is_module_symbol(symbol);
        let storage = symbol_storage(
            external,
            function.is_some() || constructor.is_some() || singleton.is_some(),
            derived,
            signal,
            captured.contains(&symbol) && mutable,
            module_symbol,
            mutable,
        );
        let value = LoweredSymbol {
            origin: origin.clone(),
            semantic_id: symbol,
            module: module_id,
            owner,
            value_type,
            storage,
            requires_initialization_check: resolved.requires_initialization_state(symbol),
            derived,
            signal,
            mutated_parameter: module.is_mutated_parameter(symbol),
            move_parameter: module.is_move_parameter(symbol),
            captured_cell: capture_requires_cell(module, symbol),
            function,
            constructor,
            singleton,
            intrinsic,
            external,
        };
        self.symbols.insert("symbol", symbol, origin, value).err()
    }

    /// Inserts declared functions in resolver order, then implicit thunks in
    /// stable semantic-ID order.
    fn snapshot_functions(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let derived_evaluators = module
            .derived_evaluators_in_symbol_order()
            .into_iter()
            .map(|(_, function)| function)
            .collect::<HashSet<_>>();
        let mut diagnostics = Vec::new();
        for function in module.functions() {
            self.snapshot_function(
                module,
                function,
                false,
                &derived_evaluators,
                &mut diagnostics,
            );
        }
        for function in module.implicit_thunks_in_id_order() {
            self.snapshot_function(
                module,
                function,
                true,
                &derived_evaluators,
                &mut diagnostics,
            );
        }
        diagnostics
    }

    fn snapshot_function(
        &mut self,
        module: &TypedModule,
        function: &ResolvedFunction,
        implicit_thunk: bool,
        derived_evaluators: &HashSet<FunctionId>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let resolved = module.resolved();
        let body_syntax = function.body.syntax();
        let origin = Origin {
            syntax: body_syntax.id,
            span: body_syntax.span.clone(),
        };
        let Some(signature) = module.type_of_function(function.id).cloned() else {
            diagnostics.push(Diagnostic::new(
                origin.span,
                format!(
                    "cannot lower function `{}` (function id {}) without a checked function type",
                    function.name, function.id.0
                ),
            ));
            return;
        };
        let Some(module_id) = resolved.module_for_syntax(body_syntax.id) else {
            diagnostics.push(Diagnostic::new(
                origin.span,
                format!(
                    "cannot lower function `{}` (function id {}) without an owning module",
                    function.name, function.id.0
                ),
            ));
            return;
        };
        let binding_symbol = function
            .binding_syntax
            .and_then(|syntax| resolved.symbol_for(syntax));
        let derived_evaluator = derived_evaluators.contains(&function.id);
        let coroutine_body = implicit_thunk && module.coroutine_plan(body_syntax.id).is_some();
        let effectful = !signature.effects.resources.is_empty()
            || signature.effects.state.is_some()
            || signature.effects.variable.is_some();
        let resource_helper = implicit_thunk && !derived_evaluator && !coroutine_body && effectful;
        let class = LoweredFunctionClass {
            declared: !implicit_thunk,
            implicit_thunk,
            derived_evaluator,
            coroutine_body,
            resource_helper,
            external: binding_symbol.is_some_and(|symbol| resolved.is_external_symbol(symbol)),
            intrinsic: binding_symbol
                .is_some_and(|symbol| resolved.intrinsic_function(symbol).is_some()),
        };
        let parameter_pattern = match self.lower_function_pattern(module, function, &signature) {
            Ok(pattern) => pattern,
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                return;
            }
        };
        let captures = function
            .captures
            .iter()
            .map(|symbol| LoweredCapture {
                symbol: *symbol,
                borrowed: module.is_borrowed_capture(function.id, *symbol),
                non_owning: module.is_non_owning_symbol(*symbol),
                requires_cell: capture_requires_cell(module, *symbol),
            })
            .collect();
        let body = match self.lower_function_body(module, function) {
            Ok(body) => Some(body),
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                return;
            }
        };
        let value = LoweredFunction {
            origin: origin.clone(),
            semantic_id: function.id,
            name: function.name.clone(),
            module: module_id,
            binding_symbol,
            signature,
            bounds: module.bounds_of_function(function.id).to_vec(),
            parameter_style: function.parameter_style,
            parameter_pattern,
            parameters: pattern_symbols(resolved, &function.pattern),
            captures,
            body_origin: origin.clone(),
            body_syntax: body_syntax.id,
            body,
            class,
        };
        if let Err(diagnostic) = self
            .functions
            .insert("function", function.id, origin, value)
        {
            diagnostics.push(diagnostic);
        }
    }

    fn snapshot_modules(&mut self, module: &TypedModule) -> Vec<Diagnostic> {
        let resolved = module.resolved();
        let program = resolved.program();
        let sources = program.modules();
        let executable_entry = program.executable_entry();
        let mut diagnostics =
            validate_initialization_order(sources, program.initialization_order());
        let mut seen = vec![false; sources.len()];
        for (index, module_id) in program.initialization_order().iter().copied().enumerate() {
            if module_id.0 >= sources.len() {
                continue;
            }
            let source = &sources[module_id.0];
            let origin = module_origin(source);
            if std::mem::replace(&mut seen[module_id.0], true) {
                continue;
            }
            let is_entry = executable_entry == Some(module_id);
            let items = self.lower_items(
                module,
                ExpressionOwner::Module(module_id),
                &source.syntax.items,
                &mut diagnostics,
            );
            let body = self.blocks.push(LoweredBlock {
                origin: origin.clone(),
                items,
                result: None,
            });
            let initializer = self.initializers.push(LoweredInitializer {
                origin: origin.clone(),
                module: module_id,
                executable_entry: is_entry,
                resources: if is_entry {
                    entry_resources(module)
                } else {
                    Vec::new()
                },
                body,
            });
            let info = LoweredModuleInfo {
                origin: origin.clone(),
                semantic_id: module_id,
                qualified_name: source.qualified_name.clone(),
                parent: source.parent,
                companion: source.companion,
                initialization_index: index,
                executable_entry: is_entry,
                initializer,
            };
            if let Err(diagnostic) = self.modules.insert("module", module_id, origin, info) {
                diagnostics.push(diagnostic);
            }
        }
        diagnostics
    }

    /// Lowers one runtime block's item sequence, preserving source order. The
    /// trailing expression becomes the block result rather than an item, so a
    /// block's value is never also a statement. The block occurrence is
    /// memoized so loop bodies and function bodies reached again later reuse
    /// the same arena node.
    fn lower_block(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        block: &staple_syntax::BlockExpression,
    ) -> Result<BlockId, Diagnostic> {
        let key = ExpressionKey {
            syntax: block.syntax.id,
            owner,
            context,
        };
        if let Some(existing) = self.block_lookup.get(&key) {
            return Ok(*existing);
        }
        let origin = Origin {
            syntax: block.syntax.id,
            span: block.syntax.span.clone(),
        };
        let mut items = Vec::new();
        let mut result = None;
        let last = block.items.len().checked_sub(1);
        for (index, item) in block.items.iter().enumerate() {
            if Some(index) == last
                && let Item::Expression(expression) = item
            {
                result = Some(self.lower_expression(module, owner, context, expression)?);
                continue;
            }
            if let Some(item) = self.lower_item(module, owner, context, item)? {
                items.push(item);
            }
        }
        let id = self.blocks.push(LoweredBlock {
            origin,
            items,
            result,
        });
        self.block_lookup.insert(key, id);
        Ok(id)
    }

    /// Lowers a function template's body into exactly one block. A non-block
    /// body expression becomes the result of a synthetic single-result block,
    /// matching how code generation returns the body expression directly.
    fn lower_function_body(
        &mut self,
        module: &TypedModule,
        function: &ResolvedFunction,
    ) -> Result<BlockId, Diagnostic> {
        let owner = ExpressionOwner::Function(function.id);
        let body =
            self.lower_expression(module, owner, ExpressionContext::Primary, &function.body)?;
        if let Some(LoweredExpressionKind::Block(block)) = self
            .expressions
            .get(body)
            .map(|expression| &expression.kind)
        {
            return Ok(*block);
        }
        let syntax = function.body.syntax();
        Ok(self.blocks.push(LoweredBlock {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            items: Vec::new(),
            result: Some(body),
        }))
    }

    /// Lowers every runtime item of a module initializer in source order.
    /// Declaration-only and other compile-time-only source items are omitted,
    /// matching how resolution and code generation treat them.
    fn lower_items(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        items: &[Item],
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Vec<ItemId> {
        let mut lowered = Vec::new();
        for item in items {
            match self.lower_item(module, owner, ExpressionContext::Primary, item) {
                Ok(Some(item)) => lowered.push(item),
                Ok(None) => {}
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        lowered
    }

    /// Lowers one runtime item. Returns `None` for compile-time-only source
    /// items that resolution and code generation omit. Unexpanded macro,
    /// splice, and operator nodes are lowering diagnostics instead of being
    /// silently dropped.
    fn lower_item(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        item: &Item,
    ) -> Result<Option<ItemId>, Diagnostic> {
        let syntax = item.syntax();
        let origin = Origin {
            syntax: syntax.id,
            span: syntax.span.clone(),
        };
        let kind = match item {
            Item::Binding(binding) => {
                LoweredItemKind::Binding(self.lower_binding_item(module, owner, context, binding)?)
            }
            Item::PatternBinding(binding) => LoweredItemKind::PatternBinding(
                self.lower_pattern_binding_item(module, owner, context, binding)?,
            ),
            Item::Assignment(assignment) => LoweredItemKind::Assignment(
                self.lower_assignment_item(module, owner, context, assignment)?,
            ),
            Item::Return(item) => LoweredItemKind::Return(LoweredReturnItem {
                value: self.lower_expression(module, owner, context, &item.value)?,
            }),
            Item::Break(item) => LoweredItemKind::Break(LoweredBreakItem {
                value: match &item.value {
                    Some(value) => Some(self.lower_expression(module, owner, context, value)?),
                    None => None,
                },
            }),
            Item::Continue(_) => LoweredItemKind::Continue,
            Item::Expression(expression) => {
                let expression = self.lower_expression(module, owner, context, expression)?;
                let drop_result = self.expression_needs_drop(module, expression);
                LoweredItemKind::Expression(LoweredExpressionStatementItem {
                    expression,
                    drop_result,
                })
            }
            Item::VisibilitySplice(splice) => {
                return Err(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "unexpanded visibility splice reached lowering",
                ));
            }
            Item::RepeatedItemSplice(splice) => {
                return Err(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "unexpanded repeated item splice reached lowering",
                ));
            }
            // A surviving visibility-macro invocation is the marker left by a
            // successfully expanded item-producing macro; its generated items
            // are lowered separately. Resolution and code generation both
            // treat it as a compile-time-only no-op.
            Item::VisibilityMacroInvocation(_)
            | Item::Modified(_)
            | Item::UseDeclaration(_)
            | Item::Submodule(_)
            | Item::ExternBlock(_)
            | Item::TypeDeclaration(_)
            | Item::MacroDeclaration(_)
            | Item::TraitDeclaration(_)
            | Item::TraitImplementation(_) => return Ok(None),
        };
        Ok(Some(self.items.push(LoweredItem { origin, kind })))
    }

    fn lower_binding_item(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        binding: &staple_syntax::Binding,
    ) -> Result<LoweredBindingItem, Diagnostic> {
        let resolved = module.resolved();
        let Some(symbol) = module.symbol_for(binding.syntax.id) else {
            return Err(Diagnostic::new(
                binding.syntax.span.clone(),
                format!(
                    "cannot lower binding `{}` without a resolved symbol",
                    binding.name
                ),
            ));
        };
        let value = match &binding.value {
            Some(value) => Some(self.lower_expression(module, owner, context, value)?),
            None => None,
        };
        Ok(LoweredBindingItem {
            symbol: Some(symbol),
            value,
            compile_time_only: compile_time_only_symbol(module, symbol),
            generic: !binding.type_parameters.is_empty(),
            derived: module.is_derived_symbol(symbol),
            signal: resolved.is_signal_symbol(symbol),
            cell: symbol_requires_cell(module, symbol),
            requires_initialization_check: resolved.requires_initialization_state(symbol),
        })
    }

    fn lower_pattern_binding_item(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        binding: &staple_syntax::PatternBinding,
    ) -> Result<LoweredPatternBindingItem, Diagnostic> {
        let pattern = self.lower_pattern(module, &binding.pattern)?;
        let value = self.lower_expression(module, owner, context, &binding.value)?;
        let propagating = binding.kind == staple_syntax::PatternBindingKind::Propagating;
        let propagation = module.propagation_for(binding.syntax.id).cloned();
        if propagating && propagation.is_none() {
            return Err(Diagnostic::new(
                binding.syntax.span.clone(),
                "cannot lower a propagating binding without checked propagation metadata",
            ));
        }
        Ok(LoweredPatternBindingItem {
            pattern,
            value,
            propagating,
            propagation,
        })
    }

    fn lower_assignment_item(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        assignment: &staple_syntax::Assignment,
    ) -> Result<LoweredAssignmentItem, Diagnostic> {
        if let Expression::Index(index) = &assignment.target {
            let target = self.lower_indexed_place(module, owner, context, index)?;
            let value = self.lower_expression(module, owner, context, &assignment.value)?;
            let mutate_index = module.trait_dispatch_for(assignment.syntax.id).cloned();
            if mutate_index.is_none() {
                return Err(Diagnostic::new(
                    assignment.syntax.span.clone(),
                    "cannot lower an indexed assignment without a checked MutateIndex dispatch",
                ));
            }
            return Ok(LoweredAssignmentItem {
                target,
                value,
                mutate_index,
                initialization_symbol: None,
                drop_previous: false,
                signal: false,
            });
        }
        let target = self.lower_place(module, owner, context, &assignment.target)?;
        let value = self.lower_expression(module, owner, context, &assignment.value)?;
        let target_type = self
            .places
            .get(target)
            .map(|place| place.value_type.clone())
            .unwrap_or(CheckedType::Error);
        let initialization_symbol = self.place_root_symbol(target);
        let signal =
            initialization_symbol.is_some_and(|symbol| module.resolved().is_signal_symbol(symbol));
        Ok(LoweredAssignmentItem {
            target,
            value,
            mutate_index: None,
            initialization_symbol,
            drop_previous: module.type_needs_drop(&target_type),
            signal,
        })
    }

    /// The symbol whose initialization state an assignment writes back. Mirrors
    /// code generation's place-pointer result: direct storage keeps its symbol,
    /// slice and dereference places do not.
    fn place_root_symbol(&self, place: PlaceId) -> Option<SymbolId> {
        match &self.places.get(place)?.kind {
            LoweredPlaceKind::Symbol { symbol } | LoweredPlaceKind::CapturedCell { symbol } => {
                Some(*symbol)
            }
            LoweredPlaceKind::ProductElement { base, slice, .. } => {
                if *slice {
                    None
                } else {
                    self.place_root_symbol(*base)
                }
            }
            LoweredPlaceKind::Representation { base } => self.place_root_symbol(*base),
            LoweredPlaceKind::Temporary { .. }
            | LoweredPlaceKind::Resource { .. }
            | LoweredPlaceKind::Dereference { .. }
            | LoweredPlaceKind::Indexed { .. } => None,
        }
    }

    fn lower_indexed_place(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        index: &staple_syntax::IndexExpression,
    ) -> Result<PlaceId, Diagnostic> {
        let syntax = &index.syntax;
        let Some(value_type) = module.type_of_expression(syntax.id).cloned() else {
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "cannot lower an indexed place without a checked element type",
            ));
        };
        let base = if expression_has_place_root(module.resolved(), &index.value) {
            self.lower_place(module, owner, context, &index.value)?
        } else {
            let value_syntax = index.value.syntax();
            let Some(base_type) = module.type_of_expression(value_syntax.id).cloned() else {
                return Err(Diagnostic::new(
                    value_syntax.span.clone(),
                    "cannot lower an indexed base without a checked type",
                ));
            };
            let expression = self.lower_expression(module, owner, context, &index.value)?;
            self.places.push(LoweredPlace {
                origin: Origin {
                    syntax: value_syntax.id,
                    span: value_syntax.span.clone(),
                },
                value_type: base_type,
                kind: LoweredPlaceKind::Temporary { expression },
            })
        };
        let position = self.lower_expression(module, owner, context, &index.index)?;
        Ok(self.places.push(LoweredPlace {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type,
            kind: LoweredPlaceKind::Indexed {
                base,
                index: position,
            },
        }))
    }

    /// Lowers an assignment target into an explicit place tree.
    fn lower_place(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        expression: &Expression,
    ) -> Result<PlaceId, Diagnostic> {
        let syntax = expression.syntax();
        let origin = Origin {
            syntax: syntax.id,
            span: syntax.span.clone(),
        };
        let Some(value_type) = module.type_of_expression(syntax.id).cloned() else {
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "cannot lower a place without a checked type",
            ));
        };
        if let Some(symbol) = module.symbol_for(syntax.id) {
            let kind = if symbol_requires_cell(module, symbol) {
                LoweredPlaceKind::CapturedCell { symbol }
            } else {
                LoweredPlaceKind::Symbol { symbol }
            };
            return Ok(self.places.push(LoweredPlace {
                origin,
                value_type,
                kind,
            }));
        }
        let kind = match expression {
            Expression::Product(product) if product.elements.len() == 1 => {
                return self.lower_place(module, owner, context, &product.elements[0].value);
            }
            Expression::Satisfies(satisfies) => {
                return self.lower_place(module, owner, context, &satisfies.value);
            }
            Expression::Resource(resource) => {
                let Some(resource) = module.resource_for_expression(resource.syntax.id).cloned()
                else {
                    return Err(Diagnostic::new(
                        resource.syntax.span.clone(),
                        "cannot lower a resource place without checked resource metadata",
                    ));
                };
                LoweredPlaceKind::Resource { resource }
            }
            Expression::Access(access) => {
                self.lower_access_place(module, owner, context, access)?
            }
            Expression::Index(index) => {
                return self.lower_indexed_place(module, owner, context, index);
            }
            other => {
                return Err(Diagnostic::new(
                    other.syntax().span.clone(),
                    "assignment target is not a lowerable place",
                ));
            }
        };
        Ok(self.places.push(LoweredPlace {
            origin,
            value_type,
            kind,
        }))
    }

    fn lower_access_place(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        access: &staple_syntax::AccessExpression,
    ) -> Result<LoweredPlaceKind, Diagnostic> {
        let Some(checked) = module.access_for(access.syntax.id).cloned() else {
            return Err(Diagnostic::new(
                access.syntax.span.clone(),
                "cannot lower an access place without checked access metadata",
            ));
        };
        match checked {
            CheckedAccess::Representation { dereference } => {
                let base =
                    self.lower_access_base(module, owner, context, &access.value, dereference)?;
                Ok(LoweredPlaceKind::Representation { base })
            }
            CheckedAccess::Product {
                index,
                dereference,
                slice,
                scalar,
            } => {
                let base =
                    self.lower_access_base(module, owner, context, &access.value, dereference)?;
                if scalar {
                    Ok(LoweredPlaceKind::Representation { base })
                } else {
                    Ok(LoweredPlaceKind::ProductElement { base, index, slice })
                }
            }
        }
    }

    /// The base of an access place: a recursive place when no `Ref` payloads
    /// are crossed, or a dereference of the evaluated value expression.
    fn lower_access_base(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        value: &Expression,
        dereference: Vec<CheckedType>,
    ) -> Result<PlaceId, Diagnostic> {
        if dereference.is_empty() {
            return self.lower_place(module, owner, context, value);
        }
        let syntax = value.syntax();
        let Some(value_type) = dereference.last().cloned() else {
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "cannot lower an empty dereference chain",
            ));
        };
        let reference = self.lower_expression(module, owner, context, value)?;
        Ok(self.places.push(LoweredPlace {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type,
            kind: LoweredPlaceKind::Dereference {
                reference,
                dereference,
            },
        }))
    }

    /// Lowers a function template's parameter pattern. Compiler-synthesized
    /// implicit-thunk parameters carry no source pattern type, so their
    /// checked signature parameter and body origin stand in.
    fn lower_function_pattern(
        &mut self,
        module: &TypedModule,
        function: &ResolvedFunction,
        signature: &CheckedFunctionType,
    ) -> Result<PatternId, Diagnostic> {
        if module
            .type_of_pattern(function.pattern.syntax().id)
            .is_some()
        {
            return self.lower_pattern(module, &function.pattern);
        }
        let Pattern::Product(product) = &function.pattern else {
            return Err(Diagnostic::new(
                Span::Compiler,
                "cannot lower a compiler-synthesized function parameter pattern",
            ));
        };
        if !product.elements.is_empty() {
            return Err(Diagnostic::new(
                Span::Compiler,
                "compiler-synthesized function parameter patterns must be empty products",
            ));
        }
        let syntax = function.body.syntax();
        Ok(self.patterns.push(LoweredPattern {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type: signature.parameter.as_ref().clone(),
            kind: LoweredPatternKind::Product {
                elements: Vec::new(),
                mutable: product.mutable,
                moved: product.moved,
            },
        }))
    }

    /// Lowers a checked pattern recursively, recording bound symbols and
    /// singleton targets.
    fn lower_pattern(
        &mut self,
        module: &TypedModule,
        pattern: &Pattern,
    ) -> Result<PatternId, Diagnostic> {
        let syntax = pattern.syntax();
        let Some(value_type) = module.type_of_pattern(syntax.id).cloned() else {
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "cannot lower a pattern without a checked type",
            ));
        };
        let resolved = module.resolved();
        let kind = match pattern {
            Pattern::Wildcard(_) => LoweredPatternKind::Wildcard,
            Pattern::Binding(binding) => LoweredPatternKind::Binding {
                symbol: resolved.symbol_for(binding.syntax.id),
                singleton: resolved.type_for_pattern(binding.syntax.id),
                mutable: binding.mutable,
                moved: binding.moved,
            },
            Pattern::Product(product) => {
                let mut elements = Vec::with_capacity(product.elements.len());
                for element in &product.elements {
                    elements.push(self.lower_pattern(module, element)?);
                }
                LoweredPatternKind::Product {
                    elements,
                    mutable: product.mutable,
                    moved: product.moved,
                }
            }
            Pattern::Nominal(nominal) => LoweredPatternKind::Nominal {
                target: resolved.type_for_pattern(syntax.id),
                name: nominal.name.clone(),
                moved: nominal.moved,
                argument: self.lower_pattern(module, &nominal.argument)?,
            },
            Pattern::StringLiteral(literal) => LoweredPatternKind::Literal {
                literal: literal.literal.clone(),
            },
            Pattern::At(at) => {
                let binding =
                    self.lower_pattern(module, &Pattern::Binding(at.binding.as_ref().clone()))?;
                let pattern = self.lower_pattern(module, &at.pattern)?;
                LoweredPatternKind::At { binding, pattern }
            }
            Pattern::Splice(splice) => {
                return Err(Diagnostic::new(
                    splice.syntax.span.clone(),
                    "unexpanded pattern splice reached lowering",
                ));
            }
        };
        Ok(self.patterns.push(LoweredPattern {
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type,
            kind,
        }))
    }

    /// Lowers one runtime expression occurrence. The occurrence key memoizes
    /// repeated visits; children are lowered before the node is allocated so a
    /// diagnostic never leaves a partially initialized arena node.
    fn lower_expression(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        expression: &Expression,
    ) -> Result<ExpressionId, Diagnostic> {
        let syntax = expression.syntax();
        let key = ExpressionKey {
            syntax: syntax.id,
            owner,
            context,
        };
        if let Some(existing) = self.expression_lookup.get(&key) {
            return Ok(*existing);
        }
        let disposition = classify_expression(expression);
        if disposition == ExpressionDisposition::Rejected {
            reject_compile_time_expression(expression)?;
            return Err(Diagnostic::new(
                syntax.span.clone(),
                "compile-time-only expression reached lowering",
            ));
        }
        let value_type = match module.type_of_expression(syntax.id).cloned() {
            Some(value_type) => value_type,
            // The checker stops recording a type once control flow diverges
            // (`return`, `break`, `continue`, or a `Never` sub-expression), so
            // an expression with no recorded type is unreachable and its
            // value is `Never`.
            None => CheckedType::Never,
        };
        let effects = module
            .effects_of_expression(syntax.id)
            .cloned()
            .unwrap_or_default();
        let coercion = module.coercion_for(syntax.id).cloned();
        let mut moved_symbols = module.moved_symbols(syntax.id).collect::<Vec<_>>();
        moved_symbols.sort_by_key(|symbol| symbol.0);
        let kind = match disposition {
            ExpressionDisposition::Ordinary(family) => {
                self.lower_ordinary_expression(module, owner, context, family, expression)?
            }
            ExpressionDisposition::Deferred(family) => LoweredExpressionKind::Deferred(family),
            ExpressionDisposition::Rejected => unreachable!("rejected above"),
        };
        let id = self.expressions.push(LoweredExpression {
            key,
            origin: Origin {
                syntax: syntax.id,
                span: syntax.span.clone(),
            },
            value_type,
            effects,
            coercion,
            moved_symbols,
            kind,
        });
        self.expression_lookup.insert(key, id);
        Ok(id)
    }

    /// Lowers the children and payload of one Stage 2.4-owned expression
    /// family. Families whose concrete payload has not landed yet lower their
    /// children and then stay `Pending` so traversal still visits every
    /// reachable ordinary occurrence.
    fn lower_ordinary_expression(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        family: Stage24Family,
        expression: &Expression,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        let pending = |family| LoweredExpressionKind::Pending(family);
        match (family, expression) {
            (Stage24Family::Block, Expression::Block(block)) => Ok(LoweredExpressionKind::Block(
                self.lower_block(module, owner, context, block)?,
            )),
            (Stage24Family::Satisfies, Expression::Satisfies(satisfies)) => {
                self.lower_expression(module, owner, context, &satisfies.value)?;
                Ok(pending(Stage24Family::Satisfies))
            }
            (Stage24Family::Match, Expression::Match(match_)) => {
                self.lower_expression(module, owner, context, &match_.subject)?;
                for arm in &match_.arms {
                    self.lower_expression(module, owner, context, &arm.body)?;
                }
                Ok(pending(Stage24Family::Match))
            }
            (Stage24Family::Loop, Expression::Loop(loop_)) => {
                self.lower_block(module, owner, context, &loop_.body)?;
                Ok(pending(Stage24Family::Loop))
            }
            (Stage24Family::Product, Expression::Product(product)) => {
                for element in &product.elements {
                    self.lower_expression(module, owner, context, &element.value)?;
                }
                Ok(pending(Stage24Family::Product))
            }
            (Stage24Family::RepeatedProduct, Expression::RepeatedProduct(repeated)) => {
                self.lower_expression(module, owner, context, &repeated.value)?;
                Ok(pending(Stage24Family::RepeatedProduct))
            }
            (Stage24Family::Access, Expression::Access(access)) => {
                self.lower_access(module, owner, context, access)
            }
            (Stage24Family::Name, Expression::Name(name)) => {
                let Some(symbol) = module.symbol_for(name.syntax.id) else {
                    return Err(Diagnostic::new(
                        name.syntax.span.clone(),
                        format!(
                            "cannot lower name `{}` without a resolved symbol",
                            name.name
                        ),
                    ));
                };
                self.lower_name(module, name.syntax.id, name.syntax.span.clone(), symbol)
            }
            (Stage24Family::Integer, Expression::Integer(integer)) => self
                .lower_integer(module, integer)
                .map(LoweredExpressionKind::Integer),
            (Stage24Family::Float, Expression::Float(float)) => self
                .lower_float(module, float)
                .map(LoweredExpressionKind::Float),
            (Stage24Family::String, Expression::String(string)) => {
                let value = staple_syntax::string_literal::decode(&string.literal)
                    .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
                Ok(LoweredExpressionKind::String(LoweredString { value }))
            }
            (Stage24Family::CString, Expression::CString(string)) => self
                .lower_c_string(string)
                .map(LoweredExpressionKind::CString),
            (Stage24Family::Index, Expression::Index(index)) => {
                self.lower_expression(module, owner, context, &index.value)?;
                self.lower_expression(module, owner, context, &index.index)?;
                Ok(pending(Stage24Family::Index))
            }
            (Stage24Family::Logical, Expression::Logical(logical)) => {
                self.lower_expression(module, owner, context, &logical.left)?;
                self.lower_expression(module, owner, context, &logical.right)?;
                Ok(pending(Stage24Family::Logical))
            }
            (Stage24Family::StringTemplate, Expression::StringTemplate(template)) => {
                for part in &template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        self.lower_expression(module, owner, context, &interpolation.expression)?;
                    }
                }
                Ok(pending(Stage24Family::StringTemplate))
            }
            (family, _) => Ok(pending(family)),
        }
    }

    /// Lowers a symbol-selected name occurrence. Functions, constructors, and
    /// other callable values are explicitly deferred to Stage 2.5; singleton
    /// values record their identity and remain ordinary reads.
    fn lower_name(
        &mut self,
        module: &TypedModule,
        syntax: SyntaxId,
        span: Span,
        symbol: SymbolId,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        let resolved = module.resolved();
        // A name or companion selector that the checker resolved to a trait
        // method is a first-class callable value; Stage 2.5 owns its closure
        // construction and evidence.
        if module.trait_dispatch_for(syntax).is_some() {
            return Ok(LoweredExpressionKind::Deferred(
                DeferredExpressionFamily::Callable,
            ));
        }
        if compile_time_only_symbol(module, symbol) {
            return Err(Diagnostic::new(
                span,
                format!("compile-time-only symbol {symbol:?} reached lowering as a runtime value"),
            ));
        }
        if resolved.constructor_type(symbol).is_some()
            || module.function_for_symbol(symbol).is_some()
        {
            return Ok(LoweredExpressionKind::Deferred(
                DeferredExpressionFamily::Callable,
            ));
        }
        let Some(catalog) = self.symbols.get(symbol) else {
            return Err(Diagnostic::new(
                span,
                format!("symbol {symbol:?} is missing from the lowered symbol catalog"),
            ));
        };
        Ok(LoweredExpressionKind::Name(LoweredName {
            symbol,
            storage: catalog.storage,
            requires_initialization_check: resolved.requires_initialization_check(syntax),
            mutable: module.has_mutable_storage(symbol),
            captured_cell: catalog.captured_cell,
            moved: module.moved_symbols(syntax).any(|moved| moved == symbol),
            move_parameter: catalog.move_parameter,
            singleton: resolved.singleton_type(symbol),
        }))
    }

    /// Lowers a structural access. A symbol-selected access (a singleton or a
    /// companion/function value) lowers like a name instead of reading a base
    /// expression, matching the checked access metadata.
    fn lower_access(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        access: &staple_syntax::AccessExpression,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        if module.trait_dispatch_for(access.syntax.id).is_some() {
            return Ok(LoweredExpressionKind::Deferred(
                DeferredExpressionFamily::Callable,
            ));
        }
        if let Some(symbol) = module.symbol_for(access.syntax.id) {
            return self.lower_name(module, access.syntax.id, access.syntax.span.clone(), symbol);
        }
        let Some(checked) = module.access_for(access.syntax.id).cloned() else {
            return Err(Diagnostic::new(
                access.syntax.span.clone(),
                "cannot lower an access without checked access metadata",
            ));
        };
        let base = self.lower_expression(module, owner, context, &access.value)?;
        let kind = match checked {
            CheckedAccess::Representation { dereference } => {
                LoweredAccessKind::Representation { dereference }
            }
            CheckedAccess::Product {
                index,
                dereference,
                slice,
                scalar,
            } => {
                if scalar {
                    LoweredAccessKind::Scalar { dereference }
                } else if slice {
                    LoweredAccessKind::Slice { index, dereference }
                } else {
                    LoweredAccessKind::Product { index, dereference }
                }
            }
        };
        Ok(LoweredExpressionKind::Access(LoweredAccess { base, kind }))
    }

    fn lower_integer(
        &self,
        module: &TypedModule,
        integer: &staple_syntax::IntegerExpression,
    ) -> Result<LoweredInteger, Diagnostic> {
        let value = integer.literal.parse::<u64>().map_err(|_| {
            Diagnostic::new(
                integer.syntax.span.clone(),
                format!("integer literal `{}` is too large", integer.literal),
            )
        })?;
        let integer_type = module
            .type_of_expression(integer.syntax.id)
            .and_then(CheckedType::integer_type)
            .unwrap_or(IntegerType::I32);
        let width = integer_literal_bit_width(integer_type);
        let value_bits = if integer_type.is_signed() {
            width - 1
        } else {
            width
        };
        if value_bits < 64 && value > ((1_u64 << value_bits) - 1) {
            return Err(Diagnostic::new(
                integer.syntax.span.clone(),
                format!(
                    "integer literal `{}` does not fit in `{}`",
                    integer.literal,
                    integer_type.name()
                ),
            ));
        }
        Ok(LoweredInteger {
            value,
            integer_type,
        })
    }

    fn lower_float(
        &self,
        module: &TypedModule,
        float: &staple_syntax::FloatExpression,
    ) -> Result<LoweredFloat, Diagnostic> {
        let float_type = module
            .type_of_expression(float.syntax.id)
            .and_then(CheckedType::float_type)
            .unwrap_or(FloatType::F64);
        let value = match float_type {
            FloatType::F32 => float.literal.parse::<f32>().map(f64::from),
            FloatType::F64 => float.literal.parse::<f64>(),
        }
        .map_err(|_| Diagnostic::new(float.syntax.span.clone(), "invalid float literal"))?;
        if !value.is_finite() {
            return Err(Diagnostic::new(
                float.syntax.span.clone(),
                format!(
                    "float literal `{}` does not fit in `{}`",
                    float.literal,
                    float_type.name()
                ),
            ));
        }
        Ok(LoweredFloat { value, float_type })
    }

    fn lower_c_string(
        &self,
        string: &staple_syntax::CStringExpression,
    ) -> Result<LoweredCString, Diagnostic> {
        let value = staple_syntax::string_literal::decode(&string.literal)
            .map_err(|message| Diagnostic::new(string.syntax.span.clone(), message))?;
        if value.as_bytes().contains(&0) {
            return Err(Diagnostic::new(
                string.syntax.span.clone(),
                "C string literals cannot contain an interior NUL byte",
            ));
        }
        let mut bytes = value.into_bytes();
        bytes.push(0);
        Ok(LoweredCString { bytes })
    }

    fn expression_needs_drop(&self, module: &TypedModule, expression: ExpressionId) -> bool {
        self.expressions
            .get(expression)
            .is_some_and(|expression| module.type_needs_drop(&expression.value_type))
    }

    fn validate(&self) -> Vec<Diagnostic> {
        let mut diagnostics = self.modules.validate("module");
        diagnostics.extend(self.functions.validate("function"));
        diagnostics.extend(self.symbols.validate("symbol"));
        diagnostics.extend(self.types.validate("type"));
        diagnostics.extend(self.traits.validate("trait"));
        diagnostics.extend(self.trait_methods.validate("trait method"));
        diagnostics.extend(self.validate_arena_references());
        diagnostics.extend(self.validate_modules_and_initializers());
        diagnostics.extend(self.validate_functions());
        diagnostics.extend(self.validate_symbols());
        diagnostics.extend(self.validate_types());
        diagnostics.extend(self.validate_traits());
        diagnostics.extend(validate_semantic_ids(
            &self.semantic_ids,
            &self.types,
            &self.traits,
        ));
        diagnostics
    }

    fn validate_arena_references(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (id, expression) in self.expressions.iter() {
            if self.expression_lookup.get(&expression.key) != Some(&id) {
                diagnostics.push(Diagnostic::new(
                    expression.origin.span.clone(),
                    format!(
                        "expression occurrence {:?} disagrees with its lookup entry",
                        expression.key
                    ),
                ));
            }
            match &expression.kind {
                LoweredExpressionKind::Pending(_)
                | LoweredExpressionKind::Deferred(_)
                | LoweredExpressionKind::Integer(_)
                | LoweredExpressionKind::Float(_)
                | LoweredExpressionKind::String(_)
                | LoweredExpressionKind::CString(_) => {}
                LoweredExpressionKind::Block(id) if !self.blocks.contains(*id) => diagnostics.push(
                    invalid_reference(&expression.origin, "expression", "block", id.index()),
                ),
                LoweredExpressionKind::Block(_) => {}
                LoweredExpressionKind::Name(name) => {
                    if self.symbols.get(name.symbol).is_none() {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "name",
                            "symbol",
                            name.symbol.0,
                        ));
                    }
                    if let Some(singleton) = name.singleton
                        && self.types.get(singleton).is_none()
                    {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "name",
                            "type",
                            singleton.0,
                        ));
                    }
                }
                LoweredExpressionKind::Access(access) => {
                    if !self.expressions.contains(access.base) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "access",
                            "expression",
                            access.base.index(),
                        ));
                    }
                }
            }
        }
        for (_, pattern) in self.patterns.iter() {
            match &pattern.kind {
                LoweredPatternKind::Wildcard | LoweredPatternKind::Literal { .. } => {}
                LoweredPatternKind::Binding {
                    symbol, singleton, ..
                } => {
                    if let Some(symbol) = symbol
                        && self.symbols.get(*symbol).is_none()
                    {
                        diagnostics.push(invalid_reference(
                            &pattern.origin,
                            "pattern",
                            "symbol",
                            symbol.0,
                        ));
                    }
                    if let Some(singleton) = singleton
                        && self.types.get(*singleton).is_none()
                    {
                        diagnostics.push(invalid_reference(
                            &pattern.origin,
                            "pattern",
                            "type",
                            singleton.0,
                        ));
                    }
                }
                LoweredPatternKind::Product { elements, .. } => {
                    for element in elements {
                        if !self.patterns.contains(*element) {
                            diagnostics.push(invalid_reference(
                                &pattern.origin,
                                "pattern",
                                "pattern",
                                element.index(),
                            ));
                        }
                    }
                }
                LoweredPatternKind::Nominal {
                    target, argument, ..
                } => {
                    if let Some(target) = target
                        && self.types.get(*target).is_none()
                    {
                        diagnostics.push(invalid_reference(
                            &pattern.origin,
                            "pattern",
                            "type",
                            target.0,
                        ));
                    }
                    if !self.patterns.contains(*argument) {
                        diagnostics.push(invalid_reference(
                            &pattern.origin,
                            "pattern",
                            "pattern",
                            argument.index(),
                        ));
                    }
                }
                LoweredPatternKind::At {
                    binding,
                    pattern: nested,
                } => {
                    for child in [*binding, *nested] {
                        if !self.patterns.contains(child) {
                            diagnostics.push(invalid_reference(
                                &pattern.origin,
                                "at pattern",
                                "pattern",
                                child.index(),
                            ));
                        }
                    }
                }
            }
        }
        for (_, place) in self.places.iter() {
            match &place.kind {
                LoweredPlaceKind::Symbol { symbol } | LoweredPlaceKind::CapturedCell { symbol } => {
                    if self.symbols.get(*symbol).is_none() {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "symbol",
                            symbol.0,
                        ));
                    }
                }
                LoweredPlaceKind::Temporary { expression } => {
                    if !self.expressions.contains(*expression) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "expression",
                            expression.index(),
                        ));
                    }
                }
                LoweredPlaceKind::Resource { .. } => {}
                LoweredPlaceKind::Dereference { reference, .. } => {
                    if !self.expressions.contains(*reference) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "expression",
                            reference.index(),
                        ));
                    }
                }
                LoweredPlaceKind::ProductElement { base, .. }
                | LoweredPlaceKind::Representation { base } => {
                    if !self.places.contains(*base) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "place",
                            base.index(),
                        ));
                    }
                }
                LoweredPlaceKind::Indexed { base, index } => {
                    if !self.places.contains(*base) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "place",
                            base.index(),
                        ));
                    }
                    if !self.expressions.contains(*index) {
                        diagnostics.push(invalid_reference(
                            &place.origin,
                            "place",
                            "expression",
                            index.index(),
                        ));
                    }
                }
            }
        }
        for (_, block) in self.blocks.iter() {
            for item in &block.items {
                if !self.items.contains(*item) {
                    diagnostics.push(invalid_reference(
                        &block.origin,
                        "block",
                        "item",
                        item.index(),
                    ));
                }
            }
            if let Some(result) = block.result
                && !self.expressions.contains(result)
            {
                diagnostics.push(invalid_reference(
                    &block.origin,
                    "block",
                    "expression",
                    result.index(),
                ));
            }
        }
        for (_, item) in self.items.iter() {
            let mut check = |target: &str, index: usize, valid: bool| {
                if !valid {
                    diagnostics.push(invalid_reference(&item.origin, "item", target, index));
                }
            };
            match &item.kind {
                LoweredItemKind::Binding(binding) => {
                    if let Some(symbol) = binding.symbol
                        && !binding.compile_time_only
                    {
                        check("symbol", symbol.0, self.symbols.get(symbol).is_some());
                    }
                    if let Some(value) = binding.value {
                        check(
                            "expression",
                            value.index(),
                            self.expressions.contains(value),
                        );
                    }
                }
                LoweredItemKind::PatternBinding(binding) => {
                    check(
                        "pattern",
                        binding.pattern.index(),
                        self.patterns.contains(binding.pattern),
                    );
                    check(
                        "expression",
                        binding.value.index(),
                        self.expressions.contains(binding.value),
                    );
                }
                LoweredItemKind::Assignment(assignment) => {
                    check(
                        "place",
                        assignment.target.index(),
                        self.places.contains(assignment.target),
                    );
                    check(
                        "expression",
                        assignment.value.index(),
                        self.expressions.contains(assignment.value),
                    );
                    if let Some(symbol) = assignment.initialization_symbol {
                        check("symbol", symbol.0, self.symbols.get(symbol).is_some());
                    }
                }
                LoweredItemKind::Return(item) => {
                    check(
                        "expression",
                        item.value.index(),
                        self.expressions.contains(item.value),
                    );
                }
                LoweredItemKind::Break(item) => {
                    if let Some(value) = item.value {
                        check(
                            "expression",
                            value.index(),
                            self.expressions.contains(value),
                        );
                    }
                }
                LoweredItemKind::Continue => {}
                LoweredItemKind::Expression(item) => {
                    check(
                        "expression",
                        item.expression.index(),
                        self.expressions.contains(item.expression),
                    );
                }
            }
        }
        diagnostics
    }

    fn validate_modules_and_initializers(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let mut initializer_counts = HashMap::<ModuleId, usize>::new();
        for (_, initializer) in self.initializers.iter() {
            *initializer_counts.entry(initializer.module).or_default() += 1;
            if !self.blocks.contains(initializer.body) {
                diagnostics.push(invalid_reference(
                    &initializer.origin,
                    "initializer",
                    "block",
                    initializer.body.index(),
                ));
            }
            if self.modules.get(initializer.module).is_none() {
                diagnostics.push(invalid_reference(
                    &initializer.origin,
                    "initializer",
                    "module",
                    initializer.module.0,
                ));
            }
        }
        for (position, (_, key, info)) in self.modules.iter().enumerate() {
            if info.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "module catalog key {key:?} disagrees with stored semantic id {:?}",
                        info.semantic_id
                    ),
                ));
            }
            if info.initialization_index != position {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "module {key:?} has initialization index {} instead of {position}",
                        info.initialization_index
                    ),
                ));
            }
            if let Some(parent) = info.parent
                && self.modules.get(parent).is_none()
            {
                diagnostics.push(invalid_reference(
                    &info.origin,
                    "module",
                    "module",
                    parent.0,
                ));
            }
            if !self.initializers.contains(info.initializer) {
                diagnostics.push(invalid_reference(
                    &info.origin,
                    "module",
                    "initializer",
                    info.initializer.index(),
                ));
            } else if let Some(initializer) = self.initializers.get(info.initializer)
                && initializer.module != key
            {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "module {key:?} points at an initializer for module {:?}",
                        initializer.module
                    ),
                ));
            }
            match initializer_counts.get(&key).copied() {
                Some(1) => {}
                Some(count) => diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!("module {key:?} has {count} initializers instead of one"),
                )),
                None => diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!("module {key:?} has no initializer"),
                )),
            }
        }
        diagnostics
    }

    fn validate_functions(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, key, function) in self.functions.iter() {
            if function.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    function.origin.span.clone(),
                    format!(
                        "function catalog key {key:?} disagrees with stored semantic id {:?}",
                        function.semantic_id
                    ),
                ));
            }
            if self.modules.get(function.module).is_none() {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "module",
                    function.module.0,
                ));
            }
            if let Some(body) = function.body
                && !self.blocks.contains(body)
            {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "block",
                    body.index(),
                ));
            }
            if !self.patterns.contains(function.parameter_pattern) {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "pattern",
                    function.parameter_pattern.index(),
                ));
            }
            if function.body_origin.syntax != function.body_syntax {
                diagnostics.push(Diagnostic::new(
                    function.origin.span.clone(),
                    format!(
                        "function {key:?} body origin syntax does not match its body syntax id"
                    ),
                ));
            }
            for symbol in function
                .parameters
                .iter()
                .chain(function.captures.iter().map(|capture| &capture.symbol))
            {
                if self.symbols.get(*symbol).is_none() {
                    diagnostics.push(invalid_reference(
                        &function.origin,
                        "function",
                        "symbol",
                        symbol.0,
                    ));
                }
            }
            if let Some(binding) = function.binding_symbol
                && self.symbols.get(binding).is_none()
            {
                diagnostics.push(invalid_reference(
                    &function.origin,
                    "function",
                    "symbol",
                    binding.0,
                ));
            }
        }
        diagnostics
    }

    fn validate_symbols(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, key, symbol) in self.symbols.iter() {
            if symbol.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!(
                        "symbol catalog key {key:?} disagrees with stored semantic id {:?}",
                        symbol.semantic_id
                    ),
                ));
            }
            if self.modules.get(symbol.module).is_none() {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "module",
                    symbol.module.0,
                ));
            }
            if let Some(owner) = symbol.owner
                && self.functions.get(owner).is_none()
            {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "function",
                    owner.0,
                ));
            }
            if let Some(function) = symbol.function
                && self.functions.get(function).is_none()
            {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "function",
                    function.0,
                ));
            }
            if let Some(constructor) = symbol.constructor
                && self.types.get(constructor).is_none()
            {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "type",
                    constructor.0,
                ));
            }
            if let Some(singleton) = symbol.singleton
                && self.types.get(singleton).is_none()
            {
                diagnostics.push(invalid_reference(
                    &symbol.origin,
                    "symbol",
                    "type",
                    singleton.0,
                ));
            }
            let binding = symbol.function.is_some()
                || symbol.constructor.is_some()
                || symbol.singleton.is_some();
            let consistent = match symbol.storage {
                SymbolStorage::ExternalSymbol => symbol.external,
                SymbolStorage::FunctionBinding => binding,
                SymbolStorage::DerivedBinding => symbol.derived,
                SymbolStorage::Signal => symbol.signal,
                SymbolStorage::CapturedCell => symbol.captured_cell,
                SymbolStorage::GlobalStorage
                | SymbolStorage::MutableCell
                | SymbolStorage::ImmutableValue => true,
            };
            if !consistent {
                diagnostics.push(Diagnostic::new(
                    symbol.origin.span.clone(),
                    format!(
                        "symbol {key:?} storage {:?} disagrees with its classification flags",
                        symbol.storage
                    ),
                ));
            }
        }
        diagnostics
    }

    fn validate_types(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, key, info) in self.types.iter() {
            if info.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "type catalog key {key:?} disagrees with stored semantic id {:?}",
                        info.semantic_id
                    ),
                ));
            }
            if self.modules.get(info.module).is_none() {
                diagnostics.push(invalid_reference(
                    &info.origin,
                    "type",
                    "module",
                    info.module.0,
                ));
            }
        }
        diagnostics
    }

    fn validate_traits(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, key, info) in self.traits.iter() {
            if info.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    info.origin.span.clone(),
                    format!(
                        "trait catalog key {key:?} disagrees with stored semantic id {:?}",
                        info.semantic_id
                    ),
                ));
            }
            if self.modules.get(info.module).is_none() {
                diagnostics.push(invalid_reference(
                    &info.origin,
                    "trait",
                    "module",
                    info.module.0,
                ));
            }
            for method in &info.methods {
                match self.trait_methods.get(*method) {
                    Some(record) if record.trait_id == key => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        info.origin.span.clone(),
                        format!("trait {key:?} lists method {method:?} owned by another trait"),
                    )),
                    None => diagnostics.push(invalid_reference(
                        &info.origin,
                        "trait",
                        "trait method",
                        method.0,
                    )),
                }
            }
            for (method, function) in &info.default_methods {
                if !info.methods.contains(method) {
                    diagnostics.push(Diagnostic::new(
                        info.origin.span.clone(),
                        format!(
                            "trait {key:?} has a default function for undeclared method {method:?}"
                        ),
                    ));
                }
                if self.functions.get(*function).is_none() {
                    diagnostics.push(invalid_reference(
                        &info.origin,
                        "trait",
                        "function",
                        function.0,
                    ));
                }
            }
        }
        for (_, key, method) in self.trait_methods.iter() {
            if method.semantic_id != key {
                diagnostics.push(Diagnostic::new(
                    method.origin.span.clone(),
                    format!(
                        "trait method catalog key {key:?} disagrees with stored semantic id {:?}",
                        method.semantic_id
                    ),
                ));
            }
            if self.traits.get(method.trait_id).is_none() {
                diagnostics.push(invalid_reference(
                    &method.origin,
                    "trait method",
                    "trait",
                    method.trait_id.0,
                ));
            }
            if let Some(default) = method.default_function
                && self.functions.get(default).is_none()
            {
                diagnostics.push(invalid_reference(
                    &method.origin,
                    "trait method",
                    "function",
                    default.0,
                ));
            }
        }
        for (_, implementation) in self.trait_implementations.iter() {
            if self.traits.get(implementation.trait_id).is_none() {
                diagnostics.push(invalid_reference(
                    &implementation.origin,
                    "trait implementation",
                    "trait",
                    implementation.trait_id.0,
                ));
            }
            for (method, function) in &implementation.methods {
                match self.trait_methods.get(*method) {
                    Some(record) if record.trait_id == implementation.trait_id => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        implementation.origin.span.clone(),
                        format!(
                            "implementation of trait {:?} selects a method from another trait",
                            implementation.trait_id
                        ),
                    )),
                    None => diagnostics.push(invalid_reference(
                        &implementation.origin,
                        "trait implementation",
                        "trait method",
                        method.0,
                    )),
                }
                if self.functions.get(*function).is_none() {
                    diagnostics.push(invalid_reference(
                        &implementation.origin,
                        "trait implementation",
                        "function",
                        function.0,
                    ));
                }
            }
        }
        diagnostics
    }
}

/// Checks that every present standard/runtime ID resolves to the matching
/// catalog family and that resource selections agree with their type IDs.
fn validate_semantic_ids(
    ids: &LoweredSemanticIds,
    types: &Catalog<TypeId, LoweredTypeMetadata, LoweredTypeId>,
    traits: &Catalog<TraitId, LoweredTraitMetadata, LoweredTraitId>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (slot, id) in [
        ("natural", ids.natural_trait),
        ("sized", ids.sized_trait),
        ("copy", ids.copy_trait),
        ("drop", ids.drop_trait),
        ("default", ids.default_trait),
        ("debug", ids.debug_trait),
        ("display", ids.display_trait),
        ("index", ids.index_trait),
        ("mutate_index", ids.mutate_index_trait),
        ("into_iterator", ids.into_iterator_trait),
        ("iterator", ids.iterator_trait),
    ] {
        if let Some(id) = id
            && traits.get(id).is_none()
        {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("lowered semantic {slot} trait {id:?} has no trait catalog record"),
            ));
        }
    }
    for (slot, id) in [
        ("io", ids.io_type),
        ("reactive", ids.reactive_type),
        ("coroutine", ids.coroutine_type),
        ("task", ids.task_type),
        ("completed", ids.completed_type),
        ("cancelled", ids.cancelled_type),
        ("tasks", ids.tasks_type),
        ("scheduler", ids.scheduler_type),
        ("wait", ids.wait_type),
        ("resolver", ids.resolver_type),
        ("completion token", ids.completion_token_type),
    ] {
        if let Some(id) = id
            && types.get(id).is_none()
        {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("lowered semantic {slot} type {id:?} has no type catalog record"),
            ));
        }
    }
    for (slot, resource, expected) in [
        ("io", &ids.io_resource, ids.io_type),
        ("reactive", &ids.reactive_resource, ids.reactive_type),
    ] {
        if let Some(resource) = resource
            && nominal_type_id(&resource.value_type) != expected
        {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!("lowered semantic {slot} resource does not match its selected type id"),
            ));
        }
    }
    diagnostics
}

/// Checks that the initialization order lists every loaded module exactly
/// once, diagnosing unknown, duplicate, and missing entries.
fn validate_initialization_order(sources: &[SourceModule], order: &[ModuleId]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen = vec![false; sources.len()];
    for module_id in order {
        if module_id.0 >= sources.len() {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "initialization order references unknown module id {}",
                    module_id.0
                ),
            ));
            continue;
        }
        let source = &sources[module_id.0];
        if std::mem::replace(&mut seen[module_id.0], true) {
            diagnostics.push(Diagnostic::new(
                module_origin(source).span,
                format!(
                    "module `{}` appears more than once in initialization order",
                    source.qualified_name
                ),
            ));
        }
    }
    for (index, was_seen) in seen.iter().enumerate() {
        if !was_seen {
            let source = &sources[index];
            diagnostics.push(Diagnostic::new(
                module_origin(source).span,
                format!(
                    "module `{}` is missing from initialization order",
                    source.qualified_name
                ),
            ));
        }
    }
    diagnostics
}

/// A module's declaration syntax when it has one; file-backed modules without
/// a `mod` declaration use their module syntax origin instead.
fn module_origin(module: &SourceModule) -> Origin {
    let syntax = module
        .syntax
        .declaration_syntax
        .as_ref()
        .unwrap_or(&module.syntax.syntax);
    Origin {
        syntax: syntax.id,
        span: syntax.span.clone(),
    }
}

/// The single, exhaustive lowering decision for a syntax variant. A new
/// `Expression` variant fails to compile here until it has an explicit
/// owned/deferred/rejected decision, and the coverage classifier test keeps
/// the enumerated variant list in agreement.
fn classify_expression(expression: &Expression) -> ExpressionDisposition {
    use DeferredExpressionFamily::{Callable, Coroutine, Resource};
    use ExpressionDisposition::{Deferred, Ordinary, Rejected};
    use Stage24Family as Family;
    match expression {
        Expression::Function(_) | Expression::Call(_) => Deferred(Callable),
        Expression::Satisfies(_) => Ordinary(Family::Satisfies),
        Expression::Match(_) => Ordinary(Family::Match),
        Expression::Loop(_) => Ordinary(Family::Loop),
        Expression::Coro(_) | Expression::Await(_) => Deferred(Coroutine),
        Expression::Resource(_) | Expression::With(_) => Deferred(Resource),
        Expression::Block(_) => Ordinary(Family::Block),
        Expression::Product(_) => Ordinary(Family::Product),
        Expression::RepeatedProduct(_) => Ordinary(Family::RepeatedProduct),
        Expression::Access(_) => Ordinary(Family::Access),
        Expression::Index(_) => Ordinary(Family::Index),
        Expression::Unary(_) | Expression::Binary(_) => Rejected,
        Expression::Logical(_) => Ordinary(Family::Logical),
        Expression::SyntaxArgument(_)
        | Expression::VisibilityArgument(_)
        | Expression::Quote(_)
        | Expression::Splice(_) => Rejected,
        Expression::Name(_) => Ordinary(Family::Name),
        Expression::String(_) => Ordinary(Family::String),
        Expression::StringTemplate(_) => Ordinary(Family::StringTemplate),
        Expression::CString(_) => Ordinary(Family::CString),
        Expression::Integer(_) => Ordinary(Family::Integer),
        Expression::Float(_) => Ordinary(Family::Float),
    }
}

/// The stable name of an expression variant, used by coverage tests and
/// diagnostics. The exhaustive match is the compile-time half of the
/// coverage gate: adding an `Expression` variant fails to compile until it
/// is named here and classified by `classify_expression`.
fn expression_variant_name(expression: &Expression) -> &'static str {
    match expression {
        Expression::Function(_) => "Function",
        Expression::Satisfies(_) => "Satisfies",
        Expression::Match(_) => "Match",
        Expression::Loop(_) => "Loop",
        Expression::Coro(_) => "Coro",
        Expression::Await(_) => "Await",
        Expression::Resource(_) => "Resource",
        Expression::With(_) => "With",
        Expression::Block(_) => "Block",
        Expression::Product(_) => "Product",
        Expression::RepeatedProduct(_) => "RepeatedProduct",
        Expression::Call(_) => "Call",
        Expression::Access(_) => "Access",
        Expression::Index(_) => "Index",
        Expression::Unary(_) => "Unary",
        Expression::Binary(_) => "Binary",
        Expression::Logical(_) => "Logical",
        Expression::SyntaxArgument(_) => "SyntaxArgument",
        Expression::VisibilityArgument(_) => "VisibilityArgument",
        Expression::Quote(_) => "Quote",
        Expression::Splice(_) => "Splice",
        Expression::Name(_) => "Name",
        Expression::String(_) => "String",
        Expression::StringTemplate(_) => "StringTemplate",
        Expression::CString(_) => "CString",
        Expression::Integer(_) => "Integer",
        Expression::Float(_) => "Float",
    }
}

/// Rejects compile-time-only expression nodes that earlier phases must have
/// eliminated. These are lowering diagnostics rather than backend panics.
fn reject_compile_time_expression(expression: &Expression) -> Result<(), Diagnostic> {
    let message = match expression {
        Expression::Unary(unary) => format!(
            "unresolved `{}` operator expression reached lowering",
            unary.operator.text()
        ),
        Expression::Binary(binary) => format!(
            "unresolved `{}` operator expression reached lowering",
            binary.operator.text()
        ),
        Expression::Quote(quote) => {
            format!(
                "unexpanded `{}` expression reached lowering",
                quote.kind.name()
            )
        }
        Expression::Splice(_) => "unexpanded splice expression reached lowering".to_owned(),
        Expression::SyntaxArgument(_) => {
            "unexpanded grouped syntax argument reached lowering".to_owned()
        }
        Expression::VisibilityArgument(_) => "visibility syntax reached lowering".to_owned(),
        _ => return Ok(()),
    };
    Err(Diagnostic::new(expression.syntax().span.clone(), message))
}

/// Whether an expression has a place root, mirroring code generation's
/// mutation-argument test: a direct symbol or an access chain ending in one.
fn expression_has_place_root(module: &ResolvedModule, expression: &Expression) -> bool {
    if module.symbol_for(expression.syntax().id).is_some() {
        return true;
    }
    match expression {
        Expression::Access(access) => expression_has_place_root(module, &access.value),
        _ => false,
    }
}

fn entry_resources(module: &TypedModule) -> Vec<LoweredEntryResource> {
    let mut resources = Vec::new();
    if let Some(resource) = module.io_resource() {
        resources.push(LoweredEntryResource {
            kind: LoweredEntryResourceKind::Io,
            resource,
        });
    }
    if module.entry_reactive_required()
        && let Some(resource) = module.reactive_resource()
    {
        resources.push(LoweredEntryResource {
            kind: LoweredEntryResourceKind::Reactive,
            resource,
        });
    }
    resources
}

fn capture_requires_cell(module: &TypedModule, symbol: SymbolId) -> bool {
    module.resolved().requires_initialization_state(symbol)
        || module.has_mutable_storage(symbol)
        || module.is_derived_symbol(symbol)
}

/// Whether a symbol's place is reached through a shared binding cell rather
/// than direct storage. Mutable/derived/initialization-checked locals and
/// captures use cells; module symbols live in global storage and mutated
/// parameters arrive as caller-provided pointers.
fn symbol_requires_cell(module: &TypedModule, symbol: SymbolId) -> bool {
    if module.resolved().is_module_symbol(symbol) || module.is_mutated_parameter(symbol) {
        return false;
    }
    capture_requires_cell(module, symbol)
}

/// Collects binding symbols from a resolved pattern in source order, matching
/// how destructuring patterns bind symbols. Used for function parameters and
/// top-level pattern bindings.
fn pattern_symbols(module: &ResolvedModule, pattern: &Pattern) -> Vec<SymbolId> {
    fn collect(module: &ResolvedModule, pattern: &Pattern, symbols: &mut Vec<SymbolId>) {
        match pattern {
            Pattern::Binding(binding) => {
                if let Some(symbol) = module.symbol_for(binding.syntax.id) {
                    symbols.push(symbol);
                }
            }
            Pattern::At(at) => {
                if let Some(symbol) = module.symbol_for(at.binding.syntax.id) {
                    symbols.push(symbol);
                }
                collect(module, &at.pattern, symbols);
            }
            Pattern::Product(product) => {
                for element in &product.elements {
                    collect(module, element, symbols);
                }
            }
            Pattern::Nominal(nominal) => collect(module, &nominal.argument, symbols),
            Pattern::Wildcard(_) | Pattern::StringLiteral(_) | Pattern::Splice(_) => {}
        }
    }
    let mut symbols = Vec::new();
    collect(module, pattern, &mut symbols);
    symbols
}

fn nominal_type_id(value_type: &CheckedType) -> Option<TypeId> {
    match value_type {
        CheckedType::TypeConstructor { id, .. }
        | CheckedType::Opaque { id, .. }
        | CheckedType::Distinct { id, .. } => Some(*id),
        _ => None,
    }
}

/// Compile-time-only symbols stay out of the runtime catalog: constructors of
/// compiler-owned syntax types. `const` bindings are *not* compile-time-only:
/// their values are folded during checking but the backend still materializes
/// and reads a module global for every runtime reference, so they are ordinary
/// global-storage symbols here.
fn compile_time_only_symbol(module: &TypedModule, symbol: SymbolId) -> bool {
    let resolved = module.resolved();
    resolved.constructor_type(symbol).is_some_and(|id| {
        resolved.recursive_construction(id) == Some(crate::RecursiveConstruction::Syntax)
    })
}

/// Primary storage classification, in documented precedence order. Facts
/// that overlap (mutation, moves, initialization checks, capture cells) stay
/// as independent `LoweredSymbol` flags.
#[allow(clippy::too_many_arguments)]
fn symbol_storage(
    external: bool,
    binding: bool,
    derived: bool,
    signal: bool,
    captured_mutable_cell: bool,
    module_symbol: bool,
    mutable: bool,
) -> SymbolStorage {
    if external {
        SymbolStorage::ExternalSymbol
    } else if binding {
        SymbolStorage::FunctionBinding
    } else if derived {
        SymbolStorage::DerivedBinding
    } else if signal {
        SymbolStorage::Signal
    } else if captured_mutable_cell {
        SymbolStorage::CapturedCell
    } else if module_symbol {
        SymbolStorage::GlobalStorage
    } else if mutable {
        SymbolStorage::MutableCell
    } else {
        SymbolStorage::ImmutableValue
    }
}

/// The storage width used to validate integer literal magnitudes. `ISize`
/// and `USize` match the backend's pointer-sized integers.
fn integer_literal_bit_width(integer_type: IntegerType) -> u32 {
    match integer_type {
        IntegerType::I8 | IntegerType::U8 => 8,
        IntegerType::I16 | IntegerType::U16 => 16,
        IntegerType::I32 | IntegerType::U32 => 32,
        IntegerType::I64 | IntegerType::U64 | IntegerType::ISize | IntegerType::USize => 64,
    }
}

/// Checks that every top-level runtime binding symbol reached the catalog.
fn initializer_symbol_diagnostics(
    module: &TypedModule,
    origins: &HashMap<SymbolId, Origin>,
    symbols: &Catalog<SymbolId, LoweredSymbol, LoweredSymbolId>,
) -> Vec<Diagnostic> {
    let resolved = module.resolved();
    let mut diagnostics = Vec::new();
    let mut check = |symbol: SymbolId| {
        if compile_time_only_symbol(module, symbol) {
            return;
        }
        if symbols.get(symbol).is_none() {
            let origin = origins
                .get(&symbol)
                .cloned()
                .unwrap_or_else(Origin::compiler);
            diagnostics.push(Diagnostic::new(
                origin.span,
                format!(
                    "runtime initializer symbol {symbol:?} is missing from the lowered symbol catalog"
                ),
            ));
        }
    };
    for source in resolved.program().modules() {
        for item in &source.syntax.items {
            match item {
                Item::Binding(binding) => {
                    if let Some(symbol) = resolved.symbol_for(binding.syntax.id) {
                        check(symbol);
                    }
                }
                Item::PatternBinding(binding) => {
                    for symbol in pattern_symbols(resolved, &binding.pattern) {
                        check(symbol);
                    }
                }
                _ => {}
            }
        }
    }
    diagnostics
}

fn invalid_reference(origin: &Origin, owner: &str, target: &str, index: usize) -> Diagnostic {
    Diagnostic::new(
        origin.span.clone(),
        format!("lowered {owner} has dangling {target} reference {index}"),
    )
}

/// A program accepted by the lowering phase and ready for code generation.
///
/// The fields are intentionally private: callers may pass this value to later
/// compiler phases, but cannot depend on the transitional representation.
#[derive(Debug, Clone)]
pub struct LoweredModule {
    program: LoweredProgram,
    typed: Box<TypedModule>,
}

impl LoweredModule {
    pub(crate) fn typed(&self) -> &TypedModule {
        self.typed.as_ref()
    }
}

/// Converts checked compiler state into the code-generation input.
#[derive(Debug, Default, Clone, Copy)]
pub struct Lowerer;

impl Lowerer {
    pub fn new() -> Self {
        Self
    }

    pub fn lower(&self, module: &TypedModule) -> Result<LoweredModule, Vec<Diagnostic>> {
        let mut program = LoweredProgram::default();
        let mut diagnostics = validate_checked_module(module);
        if diagnostics.is_empty() {
            diagnostics.extend(program.snapshot(module));
        }
        diagnostics.extend(program.validate());
        if diagnostics.is_empty() {
            Ok(LoweredModule {
                program,
                typed: Box::new(module.clone()),
            })
        } else {
            Err(diagnostics)
        }
    }
}

fn validate_checked_module(module: &TypedModule) -> Vec<Diagnostic> {
    module
        .functions()
        .iter()
        .chain(module.implicit_thunks())
        .filter(|function| module.type_of_function(function.id).is_none())
        .map(|function| {
            Diagnostic::new(
                function.body.syntax().span.clone(),
                format!(
                    "cannot lower unchecked function `{}` (function id {})",
                    function.name, function.id.0
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use staple_syntax::{Syntax, Type};

    use crate::{NameResolver, ProgramLoader, TypeChecker};

    fn standard_library_root() -> std::path::PathBuf {
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

    fn checked_program_at(entry: &Path, source: &str, root: &Path) -> TypedModule {
        let program = ProgramLoader::new()
            .with_standard_library_root(standard_library_root())
            .with_module_root(root)
            .load_source_at(entry, source)
            .expect("test source should load");
        let resolved = NameResolver::new()
            .resolve_program(program)
            .expect("test source should resolve");
        TypeChecker::new()
            .check(resolved)
            .expect("test source should type check")
    }

    #[test]
    fn empty_program_has_valid_deterministic_arenas() {
        let program = LoweredProgram::default();
        assert!(program.validate().is_empty());
        assert_eq!(program.expressions.iter().count(), 0);
        assert_eq!(program.functions.iter().count(), 0);
    }

    #[test]
    fn arena_ids_are_dense_and_in_insertion_order() {
        let mut patterns = Arena::<LoweredPattern, PatternId>::default();
        let first = patterns.push(LoweredPattern {
            origin: Origin::compiler(),
            value_type: CheckedType::I32,
            kind: LoweredPatternKind::Wildcard,
        });
        let second = patterns.push(LoweredPattern {
            origin: Origin::compiler(),
            value_type: CheckedType::I64,
            kind: LoweredPatternKind::Wildcard,
        });
        assert_eq!(first.index(), 0);
        assert_eq!(second.index(), 1);
        assert_eq!(
            patterns
                .iter()
                .map(|(id, _)| id.index())
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn catalog_rejects_duplicate_semantic_ids_without_overwriting() {
        let mut catalog = Catalog::<ModuleId, &'static str, LoweredModuleId>::default();
        let id = catalog
            .insert("module", ModuleId(7), Origin::compiler(), "first")
            .expect("first semantic ID should be accepted");
        let diagnostic = catalog
            .insert("module", ModuleId(7), Origin::compiler(), "second")
            .expect_err("duplicate semantic ID should be rejected");
        assert_eq!(id.index(), 0);
        assert_eq!(catalog.get(ModuleId(7)), Some(&"first"));
        assert!(diagnostic.message.contains("duplicate lowered module"));
        assert_eq!(catalog.iter().count(), 1);
    }

    #[test]
    fn catalog_validator_checks_lookup_and_ordered_entries_both_ways() {
        let mut catalog = Catalog::<ModuleId, (), LoweredModuleId>::default();
        let first = catalog
            .insert("module", ModuleId(2), Origin::compiler(), ())
            .expect("first insertion");
        let second = catalog
            .insert("module", ModuleId(9), Origin::compiler(), ())
            .expect("second insertion");
        assert_eq!(
            catalog.iter().map(|(_, key, _)| key).collect::<Vec<_>>(),
            vec![ModuleId(2), ModuleId(9)]
        );

        catalog.by_key.insert(ModuleId(2), second);
        catalog.by_key.insert(ModuleId(11), first);
        let diagnostics = catalog.validate("module");
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("lookup disagrees for semantic id ModuleId(2)")
        }));
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("instead of ModuleId(11)"))
        );
    }

    #[test]
    fn checked_inventory_apis_are_complete_and_semantically_ordered() {
        let module = checked_program("let answer = 42\n");
        let resolved = module.resolved();

        let symbol_ids = resolved
            .symbols_in_id_order()
            .into_iter()
            .map(|symbol| symbol.id.0)
            .collect::<Vec<_>>();
        assert!(symbol_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let parameter_ids = resolved
            .type_parameters_in_id_order()
            .into_iter()
            .map(|parameter| parameter.id.0)
            .collect::<Vec<_>>();
        assert!(parameter_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let type_ids = resolved
            .types_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(type_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let trait_ids = resolved
            .traits_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(trait_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let method_ids = resolved
            .trait_methods_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(method_ids.windows(2).all(|ids| ids[0] < ids[1]));

        let thunk_ids = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .map(|function| function.id.0)
            .collect::<Vec<_>>();
        assert!(thunk_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let evaluator_symbols = module
            .derived_evaluators_in_symbol_order()
            .into_iter()
            .map(|(symbol, _)| symbol.0)
            .collect::<Vec<_>>();
        assert!(
            evaluator_symbols
                .windows(2)
                .all(|symbols| symbols[0] < symbols[1])
        );
        let checked_method_ids = module
            .trait_method_types_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(checked_method_ids.windows(2).all(|ids| ids[0] < ids[1]));
        let checked_trait_ids = module
            .trait_parameter_arguments_in_id_order()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>();
        assert!(checked_trait_ids.windows(2).all(|ids| ids[0] < ids[1]));
        for (trait_id, _) in module.trait_parameter_arguments_in_id_order() {
            let _ = module.trait_functional_dependencies(trait_id);
        }
        let _ = module.checked_trait_implementations();
        let semantic_ids = module.semantic_ids();
        assert!(semantic_ids.copy_trait.is_some());
        assert!(semantic_ids.io_type.is_some());
    }

    fn snapshot(source: &str) -> LoweredProgram {
        let module = checked_program(source);
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());
        program
    }

    fn entry_module(program: &LoweredProgram) -> (ModuleId, &LoweredModuleInfo) {
        program
            .modules
            .iter()
            .find_map(|(_, key, info)| info.executable_entry.then_some((key, info)))
            .expect("a lowered program should have one executable entry")
    }

    fn item_category(kind: &LoweredItemKind) -> &'static str {
        match kind {
            LoweredItemKind::Binding(_) => "binding",
            LoweredItemKind::PatternBinding(_) => "pattern-binding",
            LoweredItemKind::Assignment(_) => "assignment",
            LoweredItemKind::Return(_) => "return",
            LoweredItemKind::Break(_) => "break",
            LoweredItemKind::Continue => "continue",
            LoweredItemKind::Expression(_) => "expression",
        }
    }

    fn runtime_categories(program: &LoweredProgram, module: ModuleId) -> Vec<&'static str> {
        let initializer = program
            .modules
            .get(module)
            .and_then(|info| program.initializers.get(info.initializer))
            .expect("module should have an initializer");
        program
            .blocks
            .get(initializer.body)
            .expect("initializer body")
            .items
            .iter()
            .map(|item| {
                item_category(&program.items.get(*item).expect("lowered runtime item").kind)
            })
            .collect()
    }

    fn normalized_modules(
        program: &LoweredProgram,
    ) -> Vec<(
        ModuleId,
        String,
        Option<ModuleId>,
        bool,
        usize,
        Vec<&'static str>,
    )> {
        program
            .modules
            .iter()
            .map(|(_, key, info)| {
                (
                    key,
                    info.qualified_name.clone(),
                    info.parent,
                    info.companion,
                    info.initialization_index,
                    runtime_categories(program, key),
                )
            })
            .collect()
    }

    #[test]
    fn module_catalog_matches_initialization_order() {
        let module = checked_program(
            "use dependency.answer\nmod dependency { pub let answer: I32 = 42 }\nlet copy: I32 = answer\n",
        );
        let order = module.resolved().program().initialization_order().to_vec();
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let lowered = program
            .modules
            .iter()
            .map(|(_, key, info)| (key, info))
            .collect::<Vec<_>>();
        assert_eq!(lowered.len(), order.len());
        assert_eq!(
            lowered.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            order
        );
        let (entry_id, _) = entry_module(&program);
        for (index, (key, info)) in lowered.iter().enumerate() {
            assert_eq!(info.semantic_id, *key);
            assert_eq!(info.initialization_index, index);
            assert_eq!(info.executable_entry, *key == entry_id);
            let initializer = program
                .initializers
                .get(info.initializer)
                .expect("every module should have an initializer");
            assert_eq!(initializer.module, *key);
            assert_eq!(initializer.origin, info.origin);
            assert_eq!(initializer.executable_entry, info.executable_entry);
            assert!(program.blocks.contains(initializer.body));
            if let Some(parent) = info.parent {
                assert!(program.modules.get(parent).is_some());
            }
        }

        let entry = program.modules.get(entry_id).expect("entry module");
        assert!(entry.parent.is_none());
        let sources = module.resolved().program();
        assert_eq!(
            entry.origin.syntax,
            sources.module(entry_id).syntax.syntax.id,
            "the entry module has no declaration node"
        );
        assert_eq!(
            entry.initialization_index,
            order.len() - 1,
            "the imported dependency initializes first"
        );
    }

    #[test]
    fn declaration_only_modules_have_empty_initializer_roots() {
        let program = snapshot("type Wrapper = alias I32\n");
        let (entry_id, _) = entry_module(&program);
        assert!(runtime_categories(&program, entry_id).is_empty());
    }

    #[test]
    fn runtime_items_follow_source_order_and_category() {
        let program = snapshot("let first: I32 = 1\nlet second: I32 = first\nfirst == second\n");
        let (entry_id, _) = entry_module(&program);
        assert_eq!(
            runtime_categories(&program, entry_id),
            vec!["binding", "binding", "expression"]
        );
    }

    #[test]
    fn companion_modules_keep_parent_and_companion_metadata() {
        let program =
            snapshot("pub type User = alias I32\ncompanion User { pub let id: I32 = 42 }\n");
        let (entry_id, _) = entry_module(&program);
        let companion = program
            .modules
            .iter()
            .find(|(_, _, info)| info.companion && info.parent == Some(entry_id))
            .expect("the companion module should be lowered");
        assert_eq!(companion.2.parent, Some(entry_id));
        assert_eq!(runtime_categories(&program, companion.1), vec!["binding"]);
    }

    #[test]
    fn entry_initializer_records_the_io_resource() {
        let program = snapshot("let answer: I32 = 42\n");
        let (entry_id, _) = entry_module(&program);
        let entry = program
            .initializers
            .get(program.modules.get(entry_id).unwrap().initializer)
            .expect("entry initializer");
        assert!(entry.executable_entry);
        assert_eq!(entry.resources.len(), 1);
        assert_eq!(entry.resources[0].kind, LoweredEntryResourceKind::Io);
        assert!(matches!(
            entry.resources[0].resource.value_type,
            CheckedType::Opaque { .. }
        ));
        let dependency = program
            .initializers
            .iter()
            .find(|(_, initializer)| initializer.module != entry_id)
            .map(|(_, initializer)| initializer);
        assert!(dependency.is_none() || dependency.unwrap().resources.is_empty());
    }

    #[test]
    fn file_modules_with_declarations_use_their_declaration_origin() {
        let root =
            std::env::temp_dir().join(format!("staple-lower-module-origin-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temp root");
        std::fs::write(
            root.join("tools.sta"),
            "pub mod\npub let answer: I32 = 42\n",
        )
        .expect("write module");
        let entry = root.join("main.sta");
        let module =
            checked_program_at(&entry, "use tools.answer\nlet copy: I32 = answer\n", &root);
        let tools_id = module
            .resolved()
            .program()
            .modules()
            .iter()
            .find(|source| source.path.ends_with("tools.sta"))
            .map(|source| source.id)
            .expect("the file module should be loaded");
        let declaration = module
            .resolved()
            .program()
            .module(tools_id)
            .syntax
            .declaration_syntax
            .clone()
            .expect("the file module declares itself");
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let tools = program.modules.get(tools_id).expect("lowered file module");
        assert_eq!(tools.origin.syntax, declaration.id);
        assert_eq!(runtime_categories(&program, tools_id), vec!["binding"]);
        std::fs::remove_dir_all(root).expect("clean temp root");
    }

    #[test]
    fn module_catalog_is_stable_across_repeated_lowering() {
        let module = checked_program(
            "mod dependency { pub let answer: I32 = 42 }\nlet copy: I32 = dependency.answer\n",
        );
        let mut first = LoweredProgram::default();
        let mut second = LoweredProgram::default();
        assert!(first.snapshot(&module).is_empty());
        assert!(second.snapshot(&module).is_empty());
        assert_eq!(normalized_modules(&first), normalized_modules(&second));
    }

    #[test]
    fn initialization_order_diagnostics_report_unknown_duplicate_and_missing_modules() {
        let source = |id: usize| SourceModule {
            id: ModuleId(id),
            path: std::path::PathBuf::from(format!("module{id}.sta")),
            syntax: staple_syntax::parse("").expect("empty module should parse"),
            parent: None,
            name: None,
            visibility: staple_syntax::Visibility::Public,
            qualified_name: format!("module{id}"),
            companion: false,
        };
        let sources = vec![source(0), source(1)];
        let diagnostics =
            validate_initialization_order(&sources, &[ModuleId(0), ModuleId(0), ModuleId(7)]);
        assert_eq!(diagnostics.len(), 3);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("unknown module id 7"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("appears more than once"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("`module1` is missing"))
        );
    }

    fn lowered_function<'a>(
        program: &'a LoweredProgram,
        name: &str,
    ) -> (FunctionId, &'a LoweredFunction) {
        program
            .functions
            .iter()
            .find_map(|(_, key, function)| function.name.contains(name).then_some((key, function)))
            .unwrap_or_else(|| panic!("`{name}` should have a lowered function"))
    }

    fn binding_symbol(module: &TypedModule, name: &str) -> SymbolId {
        module
            .syntax()
            .items
            .iter()
            .find_map(|item| match item {
                Item::Binding(binding) if binding.name == name => {
                    module.symbol_for(binding.syntax.id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("`{name}` should have a symbol"))
    }

    #[test]
    fn function_catalog_lists_declared_functions_before_implicit_thunks() {
        let module = checked_program(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "let mut count = 0\n",
            "let first = evaluate { count = count + 1; count }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let expected = module
            .functions()
            .iter()
            .map(|function| function.id)
            .chain(
                module
                    .implicit_thunks_in_id_order()
                    .into_iter()
                    .map(|function| function.id),
            )
            .collect::<Vec<_>>();
        let actual = program
            .functions
            .iter()
            .map(|(_, key, _)| key)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(
            actual.iter().collect::<HashSet<_>>().len(),
            actual.len(),
            "every function id should appear exactly once"
        );
        assert!(
            actual.len() > module.functions().len(),
            "the callback thunk should be lowered"
        );

        let declared = module.functions().len();
        assert!(
            program
                .functions
                .iter()
                .take(declared)
                .all(|(_, _, function)| function.class.declared)
        );
        assert!(
            program
                .functions
                .iter()
                .skip(declared)
                .all(|(_, _, function)| function.class.implicit_thunk)
        );
    }

    #[test]
    fn function_templates_copy_checked_signatures_parameters_and_bounds() {
        let module = checked_program(concat!(
            "trait Increment T { increment: T -> T }\n",
            "impl Increment I32 { def increment = value => value + 1 }\n",
            "def increment_twice: <T where Increment T> T -> T = value => increment (increment value)\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let source = module
            .functions()
            .iter()
            .find(|function| function.name.contains("increment_twice"))
            .expect("declared function");
        let (key, lowered) = lowered_function(&program, "increment_twice");
        assert_eq!(key, source.id);
        assert_eq!(
            lowered.signature,
            *module
                .type_of_function(source.id)
                .expect("checked signature")
        );
        assert_eq!(
            lowered.bounds,
            module.bounds_of_function(source.id).to_vec()
        );
        assert!(!lowered.bounds.is_empty());
        assert_eq!(lowered.parameter_style, source.parameter_style);
        assert_eq!(
            lowered.binding_symbol,
            source
                .binding_syntax
                .and_then(|syntax| module.resolved().symbol_for(syntax))
        );
        assert_eq!(
            lowered.parameters,
            pattern_symbols(module.resolved(), &source.pattern)
        );
        assert_eq!(lowered.body_syntax, source.body.syntax().id);
        assert_eq!(lowered.body_origin.syntax, source.body.syntax().id);
        let body = lowered
            .body
            .and_then(|body| program.blocks.get(body))
            .expect("every function template should have a lowered body");
        assert_eq!(body.origin, lowered.body_origin);
        assert!(program.patterns.contains(lowered.parameter_pattern));
        assert_eq!(
            lowered.module,
            module
                .resolved()
                .module_for_syntax(source.body.syntax().id)
                .expect("owning module")
        );
        assert!(lowered.class.declared && !lowered.class.implicit_thunk);
    }

    #[test]
    fn implicit_thunk_captures_match_transition_ownership_facts() {
        let module = checked_program(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "def run: () ->{state} I32 = () => {\n",
            "  let mut count = 0\n",
            "  evaluate { count = count + 1; count }\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let thunk = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .next()
            .expect("a callback thunk");
        let lowered = program
            .functions
            .get(thunk.id)
            .expect("lowered implicit thunk");
        assert!(lowered.class.implicit_thunk && !lowered.class.declared);
        assert!(lowered.binding_symbol.is_none());
        assert_eq!(lowered.captures.len(), thunk.captures.len());
        for (capture, symbol) in lowered.captures.iter().zip(&thunk.captures) {
            assert_eq!(capture.symbol, *symbol);
            assert_eq!(
                capture.borrowed,
                module.is_borrowed_capture(thunk.id, *symbol)
            );
            assert_eq!(capture.non_owning, module.is_non_owning_symbol(*symbol));
            assert_eq!(
                capture.requires_cell,
                capture_requires_cell(&module, *symbol)
            );
        }
        assert!(
            lowered
                .captures
                .iter()
                .any(|capture| capture.requires_cell && module.has_mutable_storage(capture.symbol)),
            "the mutable local `count` capture should require a shared cell"
        );
    }

    #[test]
    fn derived_evaluators_and_coroutine_bodies_are_classified() {
        let module = checked_program(concat!(
            "let signal count = 1\n",
            "let doubled = count + count\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let doubled = binding_symbol(&module, "doubled");
        let evaluator = module
            .derived_evaluator(doubled)
            .expect("derived evaluator thunk");
        let lowered = program
            .functions
            .get(evaluator.id)
            .expect("lowered derived evaluator");
        assert!(lowered.class.derived_evaluator);
        assert!(lowered.class.implicit_thunk);
        assert!(!lowered.class.coroutine_body);
        assert_eq!(
            lowered
                .captures
                .iter()
                .map(|capture| capture.symbol)
                .collect::<Vec<_>>(),
            evaluator.captures
        );

        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "def f: () -> Coroutine{} I32 = () => coro { 42 }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let body = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .find(|function| module.coroutine_plan(function.body.syntax().id).is_some())
            .expect("coroutine body thunk");
        let lowered = program
            .functions
            .get(body.id)
            .expect("lowered coroutine body");
        assert!(lowered.class.coroutine_body);
        assert!(lowered.class.implicit_thunk);
        assert!(!lowered.class.derived_evaluator);
        assert_eq!(lowered.body_syntax, body.body.syntax().id);
    }

    #[test]
    fn callback_thunks_classify_effectful_resource_helpers() {
        let module = checked_program(concat!(
            "let signal count = 0\n",
            "with Reactive = reactive_scope () {\n",
            "  reaction { let current = count; () }\n",
            "  count = 1\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let callback = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .find(|thunk| {
                let class = program.functions.get(thunk.id).expect("thunk").class;
                !class.derived_evaluator && !class.coroutine_body
            })
            .expect("the reaction callback should be an implicit thunk");
        let lowered = program
            .functions
            .get(callback.id)
            .expect("lowered implicit thunk");
        assert!(lowered.class.resource_helper);
        assert!(lowered.signature.effects.state.is_some());

        let module = checked_program(concat!(
            "def evaluate: <T, effect E> (() ->{E} T) ->{E} T = callback => callback ()\n",
            "let answer = evaluate { 42 }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let pure = module
            .implicit_thunks_in_id_order()
            .into_iter()
            .next()
            .expect("the callback should be an implicit thunk");
        let lowered = program
            .functions
            .get(pure.id)
            .expect("lowered implicit thunk");
        assert!(!lowered.class.resource_helper);
        assert!(lowered.signature.effects == CheckedEffectSet::default());
    }

    fn lowered_symbol(program: &LoweredProgram, symbol: SymbolId) -> &LoweredSymbol {
        program
            .symbols
            .get(symbol)
            .unwrap_or_else(|| panic!("symbol {symbol:?} should be lowered"))
    }

    fn body_block<'a>(program: &'a LoweredProgram, function: &LoweredFunction) -> &'a LoweredBlock {
        program
            .blocks
            .get(function.body.expect("every lowered function has a body"))
            .expect("lowered body block")
    }

    fn body_items<'a>(
        program: &'a LoweredProgram,
        function: &LoweredFunction,
    ) -> Vec<&'a LoweredItem> {
        body_block(program, function)
            .items
            .iter()
            .map(|item| program.items.get(*item).expect("lowered item"))
            .collect()
    }

    fn lowered_pattern(program: &LoweredProgram, pattern: PatternId) -> &LoweredPattern {
        program
            .patterns
            .get(pattern)
            .unwrap_or_else(|| panic!("pattern {} should be lowered", pattern.index()))
    }

    fn lowered_place(program: &LoweredProgram, place: PlaceId) -> &LoweredPlace {
        program
            .places
            .get(place)
            .unwrap_or_else(|| panic!("place {} should be lowered", place.index()))
    }

    #[test]
    fn symbol_catalog_covers_declared_runtime_symbols_in_id_order() {
        let module = checked_program(concat!(
            "const answer: I32 = 42\n",
            "let value: I32 = answer\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let resolved = module.resolved();
        let const_symbol = binding_symbol(&module, "answer");
        assert!(resolved.is_const_symbol(const_symbol));
        // Non-generic `const` bindings materialize a module global, so they
        // stay in the runtime symbol catalog as global storage.
        assert_eq!(
            program
                .symbols
                .get(const_symbol)
                .map(|symbol| symbol.storage),
            Some(SymbolStorage::GlobalStorage)
        );
        let expected = resolved
            .symbols_in_id_order()
            .into_iter()
            .filter(|info| !compile_time_only_symbol(&module, info.id))
            .filter(|info| info.module_symbol || info.owner.is_some())
            .map(|info| info.id)
            .collect::<Vec<_>>();
        let actual = program
            .symbols
            .iter()
            .map(|(_, key, _)| key)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(actual.windows(2).all(|ids| ids[0].0 < ids[1].0));
        for (_, key, symbol) in program.symbols.iter() {
            assert_eq!(symbol.semantic_id, key);
            assert!(module.declared_type_of_symbol(key).is_some());
        }
    }

    #[test]
    fn symbol_storage_classifies_globals_locals_and_parameters() {
        let module = checked_program(concat!(
            "let global: I32 = 1\n",
            "let mut mutable_global: I32 = 2\n",
            "def update: mut I32 -> I32 = mut value: I32 => { value = value + 1; value }\n",
            "def local_values: () -> I32 = () => {\n",
            "  let local: I32 = 3\n",
            "  let mut mutable_local: I32 = 4\n",
            "  local + mutable_local\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let global = lowered_symbol(&program, binding_symbol(&module, "global"));
        assert_eq!(global.storage, SymbolStorage::GlobalStorage);
        assert!(!global.captured_cell && !global.mutated_parameter && !global.move_parameter);
        let mutable_global = lowered_symbol(&program, binding_symbol(&module, "mutable_global"));
        assert_eq!(mutable_global.storage, SymbolStorage::GlobalStorage);

        let update = module
            .functions()
            .iter()
            .find(|function| function.name.contains("update"))
            .expect("update function");
        let parameter = pattern_symbols(module.resolved(), &update.pattern)[0];
        let lowered = lowered_symbol(&program, parameter);
        assert_eq!(lowered.storage, SymbolStorage::MutableCell);
        assert!(lowered.mutated_parameter);
        assert!(module.is_mutated_parameter(parameter));

        let local_values = module
            .functions()
            .iter()
            .find(|function| function.name.contains("local_values"))
            .expect("local_values function");
        let parameters = pattern_symbols(module.resolved(), &local_values.pattern);
        let locals = module
            .resolved()
            .symbols_in_id_order()
            .into_iter()
            .filter(|info| info.owner == Some(local_values.id))
            .filter(|info| !parameters.contains(&info.id))
            .collect::<Vec<_>>();
        assert_eq!(locals.len(), 2);
        assert_eq!(
            lowered_symbol(&program, locals[0].id).storage,
            SymbolStorage::ImmutableValue
        );
        assert_eq!(
            lowered_symbol(&program, locals[1].id).storage,
            SymbolStorage::MutableCell
        );
        assert!(module.has_mutable_storage(locals[1].id));
    }

    #[test]
    fn symbol_storage_classifies_captured_cells_and_borrowed_captures() {
        let module = checked_program(concat!(
            "def counter: () -> () -> I32 = () => {\n",
            "  let mut count: I32 = 0\n",
            "  () => { count = count + 1; count }\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let inner = module
            .functions()
            .iter()
            .find(|function| !function.captures.is_empty())
            .expect("capturing closure");
        let captured = inner.captures[0];
        let lowered = lowered_symbol(&program, captured);
        assert_eq!(lowered.storage, SymbolStorage::CapturedCell);
        assert!(lowered.captured_cell);
        assert!(
            lowered.requires_initialization_check
                == module.resolved().requires_initialization_state(captured)
        );
        assert_eq!(
            lowered.captured_cell,
            capture_requires_cell(&module, captured)
        );

        let module = checked_program(concat!(
            "type MyString = ctor String\n",
            "impl !Copy MyString {}\n",
            "companion MyString {\n",
            "  pub def concat = a: MyString => b: MyString => MyString (a.* + b.*)\n",
            "}\n",
            "def local = (left: MyString, right: MyString) => {\n",
            "  let append = MyString.concat left\n",
            "  append right\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let (_function, borrowed) = module
            .functions()
            .iter()
            .chain(module.implicit_thunks_in_id_order())
            .flat_map(|function| {
                function
                    .captures
                    .iter()
                    .map(move |symbol| (function, *symbol))
            })
            .find(|(function, symbol)| module.is_borrowed_capture(function.id, *symbol))
            .expect("a borrowed capture");
        assert!(!module.is_move_parameter(borrowed));
        let lowered = lowered_symbol(&program, borrowed);
        assert_eq!(lowered.storage, SymbolStorage::ImmutableValue);
        assert_eq!(
            lowered.captured_cell,
            capture_requires_cell(&module, borrowed)
        );
        assert!(!lowered.captured_cell);
        assert_eq!(lowered.owner, module.resolved().symbol_owner(borrowed));
        assert!(lowered.owner.is_some());
    }

    #[test]
    fn symbol_storage_classifies_functions_signals_derived_and_singletons() {
        let module = checked_program(concat!(
            "let signal count: I32 = 1\n",
            "let doubled: I32 = count + count\n",
            "def double: I32 -> I32 = value => value + value\n",
            "type Wrapper = ctor I32\n",
            "type Enabled\n",
            "let enabled: Enabled = Enabled\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let count = lowered_symbol(&program, binding_symbol(&module, "count"));
        assert_eq!(count.storage, SymbolStorage::Signal);
        assert!(count.signal);
        let doubled = lowered_symbol(&program, binding_symbol(&module, "doubled"));
        assert_eq!(doubled.storage, SymbolStorage::DerivedBinding);
        assert!(doubled.derived);

        let double_symbol = binding_symbol(&module, "double");
        let double = lowered_symbol(&program, double_symbol);
        assert_eq!(double.storage, SymbolStorage::FunctionBinding);
        assert_eq!(double.function, module.function_for_symbol(double_symbol));
        assert!(double.function.is_some());

        let wrapper_id = module
            .resolved()
            .type_declarations()
            .iter()
            .find_map(|(id, _)| {
                (module.resolved().type_name(*id) == Some("Wrapper")).then_some(*id)
            })
            .expect("Wrapper type");
        let (constructor, constructor_type) = module
            .resolved()
            .constructors()
            .iter()
            .find(|(_, id)| **id == wrapper_id)
            .expect("Wrapper constructor");
        let lowered = lowered_symbol(&program, *constructor);
        assert_eq!(lowered.storage, SymbolStorage::FunctionBinding);
        assert_eq!(lowered.constructor, Some(*constructor_type));

        let enabled_id = module
            .resolved()
            .type_declarations()
            .iter()
            .find_map(|(id, _)| {
                (module.resolved().type_name(*id) == Some("Enabled")).then_some(*id)
            })
            .expect("Enabled type");
        let (singleton, singleton_type) = module
            .resolved()
            .singleton_values()
            .iter()
            .find(|(_, id)| **id == enabled_id)
            .expect("Enabled singleton");
        let lowered = lowered_symbol(&program, *singleton);
        assert_eq!(lowered.storage, SymbolStorage::FunctionBinding);
        assert_eq!(lowered.singleton, Some(*singleton_type));
    }

    #[test]
    fn symbol_storage_classifies_extern_and_intrinsic_symbols() {
        let module = checked_program(concat!(
            "use std.cinterop.*\n",
            "extern \"c\" {\n",
            "  my_puts: (CPointer CChar) -> I32\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let external = module
            .resolved()
            .symbols_in_id_order()
            .into_iter()
            .find(|info| module.resolved().is_external_symbol(info.id))
            .expect("external symbol");
        let lowered = lowered_symbol(&program, external.id);
        assert_eq!(lowered.storage, SymbolStorage::ExternalSymbol);
        assert!(lowered.external);

        let intrinsic = module
            .resolved()
            .symbols_in_id_order()
            .into_iter()
            .find(|info| module.resolved().intrinsic_function(info.id).is_some())
            .expect("intrinsic symbol");
        let lowered = lowered_symbol(&program, intrinsic.id);
        assert_eq!(lowered.storage, SymbolStorage::GlobalStorage);
        assert!(lowered.intrinsic.is_some());
        assert!(!lowered.external);
    }

    #[test]
    fn symbol_catalog_excludes_macro_quote_placeholders() {
        let module = checked_program(concat!(
            "use std.syntax.(Expr, parse_quote)\n",
            "macro double: Expr -> Expr = value => parse_quote { $value + $value }\n",
            "let answer: I32 = double 21\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let resolved = module.resolved();
        for (_, key, _) in program.symbols.iter() {
            let info = resolved
                .symbols_in_id_order()
                .into_iter()
                .find(|info| info.id == key)
                .expect("catalogued symbol should be declared");
            assert!(info.module_symbol || info.owner.is_some());
            assert!(!resolved.is_const_symbol(key));
        }
    }

    fn lowered_type<'a>(program: &'a LoweredProgram, name: &str) -> &'a LoweredTypeMetadata {
        program
            .types
            .iter()
            .find_map(|(_, _, info)| (info.name == name).then_some(info))
            .unwrap_or_else(|| panic!("`{name}` should be lowered"))
    }

    fn lowered_trait<'a>(program: &'a LoweredProgram, name: &str) -> &'a LoweredTraitMetadata {
        program
            .traits
            .iter()
            .find_map(|(_, _, info)| (info.name == name).then_some(info))
            .unwrap_or_else(|| panic!("`{name}` should be lowered"))
    }

    #[test]
    fn type_catalog_matches_resolver_order_and_keeps_compact_templates() {
        let module = checked_program(concat!(
            "type TestPair T = ctor (T, T)\n",
            "type TestInner = ctor I32\n",
            "type TestOuter = ctor (TestInner, TestInner)\n",
            "type TestAlias = alias TestOuter\n",
            "type TestCallback{E} = alias () ->{E} ()\n",
            "type TestHidden = opaque\n",
            "type TestEnabled\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let expected = module
            .resolved()
            .types_in_id_order()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let actual = program
            .types
            .iter()
            .map(|(_, key, _)| key)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(actual.windows(2).all(|ids| ids[0].0 < ids[1].0));

        let pair = lowered_type(&program, "TestPair");
        assert_eq!(pair.kind, LoweredTypeKind::Distinct);
        assert_eq!(pair.parameters.len(), 1);
        assert!(matches!(pair.parameters[0], CheckedType::Parameter { .. }));
        let Some(CheckedType::Product(product)) = pair.representation.as_ref() else {
            panic!("TestPair representation should be a product");
        };
        assert_eq!(product.elements.len(), 2);
        assert!(
            product
                .elements
                .iter()
                .all(|element| matches!(element.value_type, CheckedType::Parameter { .. }))
        );

        let inner = lowered_type(&program, "TestInner");
        assert_eq!(inner.kind, LoweredTypeKind::Distinct);
        assert_eq!(
            Some(inner.module),
            module
                .resolved()
                .definition_module(DefinitionId::Type(inner.semantic_id))
        );
        assert!(inner.parameters.is_empty());

        let outer = lowered_type(&program, "TestOuter");
        let Some(CheckedType::Product(product)) = outer.representation.as_ref() else {
            panic!("TestOuter representation should be a product");
        };
        assert_eq!(product.elements.len(), 2);
        for element in &product.elements {
            assert!(
                matches!(&element.value_type, CheckedType::Opaque { id, .. } if *id == inner.semantic_id),
                "nested nominal representations should stay compact references"
            );
        }

        let alias = lowered_type(&program, "TestAlias");
        assert_eq!(alias.kind, LoweredTypeKind::Alias);
        assert_eq!(
            alias.representation, outer.representation,
            "an alias expands to its target's compact representation"
        );

        let callback = lowered_type(&program, "TestCallback");
        let Some(CheckedType::Function(parameter_template)) = callback.parameters.first() else {
            panic!("effect parameter should use an effect-substitution template");
        };
        let parameter = parameter_template
            .effects
            .variable
            .as_ref()
            .expect("effect parameter template should retain its variable");
        let Some(CheckedType::Function(representation)) = callback.representation.as_ref() else {
            panic!("effect-parameterized alias should retain its representation");
        };
        assert_eq!(representation.effects.variable.as_ref(), Some(parameter));

        let hidden = lowered_type(&program, "TestHidden");
        assert_eq!(hidden.kind, LoweredTypeKind::Opaque);
        assert!(hidden.representation.is_none());

        let enabled = lowered_type(&program, "TestEnabled");
        assert_eq!(enabled.kind, LoweredTypeKind::Singleton);
        assert_eq!(enabled.representation, Some(CheckedType::empty_product()));
    }

    #[test]
    fn trait_catalog_preserves_parameters_dependencies_methods_and_defaults() {
        let module = checked_program(concat!(
            "trait TestBase T { test_base: T -> T }\n",
            "trait TestConvert Target Position Output where {Target, Position} ~> Output {\n",
            "  test_convert: (Target, Position) -> Output\n",
            "}\n",
            "trait TestOrdered T where TestBase T {\n",
            "  test_first: T -> Bool\n",
            "  test_second: T -> Bool = value => test_first value\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let convert = lowered_trait(&program, "TestConvert");
        assert_eq!(convert.parameters.len(), 3);
        assert_eq!(convert.functional_dependencies.len(), 1);
        let dependency = &convert.functional_dependencies[0];
        assert_eq!(dependency.determinants.len(), 2);
        assert_eq!(convert.methods.len(), 1);
        let method = program
            .trait_methods
            .get(convert.methods[0])
            .expect("converted method");
        assert_eq!(method.name, "test_convert");
        assert_eq!(method.trait_id, convert.semantic_id);
        assert!(method.default_function.is_none());

        let ordered = lowered_trait(&program, "TestOrdered");
        assert!(!ordered.prerequisites.is_empty());
        assert_eq!(ordered.methods.len(), 2);
        assert_eq!(ordered.default_methods.len(), 1);
        assert_eq!(ordered.default_methods[0].0, ordered.methods[1]);
        let names = ordered
            .methods
            .iter()
            .map(|id| {
                program
                    .trait_methods
                    .get(*id)
                    .expect("ordered method")
                    .name
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["test_first", "test_second"]);
        let second = program
            .trait_methods
            .get(ordered.methods[1])
            .expect("second method");
        assert_eq!(second.name, "test_second");
        assert_eq!(second.default_function, Some(ordered.default_methods[0].1));
        assert!(
            program
                .functions
                .get(ordered.default_methods[0].1)
                .is_some()
        );
    }

    #[test]
    fn trait_implementation_catalog_records_arguments_bounds_and_negation() {
        let module = checked_program(concat!(
            "trait TestEq T { test_eq: (T, T) -> Bool }\n",
            "impl TestEq I32 { def test_eq = (left, right) => left == right }\n",
            "type TestHandle = ctor I32\n",
            "impl !Copy TestHandle {}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let eq = lowered_trait(&program, "TestEq");
        let implementation = program
            .trait_implementations
            .iter()
            .map(|(_, value)| value)
            .find(|implementation| implementation.trait_id == eq.semantic_id)
            .expect("TestEq implementation");
        assert_eq!(implementation.arguments, vec![CheckedType::I32]);
        assert!(!implementation.negative);
        assert!(implementation.bounds.is_empty());
        assert_eq!(implementation.methods.len(), 1);
        assert_eq!(implementation.methods[0].0, eq.methods[0]);
        assert!(program.functions.get(implementation.methods[0].1).is_some());

        let copy_trait = module.semantic_ids().copy_trait.expect("Copy trait");
        let negative = program
            .trait_implementations
            .iter()
            .map(|(_, value)| value)
            .find(|implementation| implementation.negative)
            .expect("negative implementation");
        assert_eq!(negative.trait_id, copy_trait);
        assert_eq!(negative.arguments.len(), 1);
        assert!(matches!(
            negative.arguments[0],
            CheckedType::Distinct { .. }
        ));
        assert!(negative.methods.is_empty());
    }

    #[test]
    fn semantic_ids_capture_standard_traits_and_runtime_types() {
        let program = snapshot("let answer: I32 = 42\n");
        let ids = &program.semantic_ids;
        for (slot, trait_id) in [
            ("natural", ids.natural_trait),
            ("sized", ids.sized_trait),
            ("copy", ids.copy_trait),
            ("drop", ids.drop_trait),
            ("default", ids.default_trait),
            ("debug", ids.debug_trait),
            ("display", ids.display_trait),
            ("index", ids.index_trait),
            ("mutate_index", ids.mutate_index_trait),
            ("into_iterator", ids.into_iterator_trait),
            ("iterator", ids.iterator_trait),
        ] {
            let trait_id = trait_id.unwrap_or_else(|| panic!("`{slot}` trait should be selected"));
            assert!(
                program.traits.get(trait_id).is_some(),
                "`{slot}` trait should have a catalog record"
            );
        }
        for (slot, type_id) in [("io", ids.io_type), ("reactive", ids.reactive_type)] {
            let type_id = type_id.unwrap_or_else(|| panic!("`{slot}` type should be selected"));
            assert!(
                program.types.get(type_id).is_some(),
                "`{slot}` type should have a catalog record"
            );
        }
        assert!(ids.string_representation.is_some());
        assert!(ids.io_resource.is_some());
        assert!(ids.reactive_resource.is_some());
        assert!(!ids.entry_reactive_required);
        assert!(program.validate().is_empty());

        let program = snapshot(concat!("use std.coroutine.*\n", "let answer: I32 = 42\n",));
        let ids = &program.semantic_ids;
        for (slot, type_id) in [
            ("coroutine", ids.coroutine_type),
            ("task", ids.task_type),
            ("completed", ids.completed_type),
            ("cancelled", ids.cancelled_type),
            ("tasks", ids.tasks_type),
            ("scheduler", ids.scheduler_type),
            ("wait", ids.wait_type),
            ("resolver", ids.resolver_type),
            ("completion token", ids.completion_token_type),
        ] {
            let type_id = type_id.unwrap_or_else(|| panic!("`{slot}` type should be selected"));
            assert!(
                program.types.get(type_id).is_some(),
                "`{slot}` type should have a catalog record"
            );
        }
        assert!(program.validate().is_empty());
    }

    #[test]
    fn entry_reactive_requirement_is_recorded() {
        let program = snapshot(concat!(
            "let signal count = 0\n",
            "reaction { let current = count; () }\n",
            "count = 1\n",
        ));
        let ids = &program.semantic_ids;
        assert!(ids.entry_reactive_required);
        assert!(ids.reactive_resource.is_some());
        assert_eq!(
            nominal_type_id(&ids.reactive_resource.as_ref().unwrap().value_type),
            ids.reactive_type
        );
        let (entry_id, _) = entry_module(&program);
        let entry = program
            .initializers
            .get(program.modules.get(entry_id).unwrap().initializer)
            .expect("entry initializer");
        assert!(
            entry
                .resources
                .iter()
                .any(|resource| resource.kind == LoweredEntryResourceKind::Reactive)
        );
    }

    #[test]
    fn no_prelude_programs_keep_standard_semantic_ids() {
        let program = snapshot("@no_prelude\npub mod\nlet answer: I32 = 1 + 2\n");
        let ids = &program.semantic_ids;
        assert!(ids.copy_trait.is_some());
        assert!(ids.io_type.is_some());
        assert!(ids.io_resource.is_some());
        assert!(program.validate().is_empty());
    }

    #[test]
    fn validator_rejects_semantic_ids_without_catalog_records() {
        let mut program = LoweredProgram::default();
        program.semantic_ids.copy_trait = Some(TraitId(3));
        program.semantic_ids.io_type = Some(TypeId(4));
        let diagnostics = program.validate();
        assert_eq!(diagnostics.len(), 2);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("copy trait TraitId(3)"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("io type TypeId(4)"))
        );
    }

    fn normalized_program_snapshot(program: &LoweredProgram) -> Vec<String> {
        let mut lines = Vec::new();
        for (_, key, info) in program.modules.iter() {
            lines.push(format!("module {key:?} {info:?}"));
        }
        for (_, key, function) in program.functions.iter() {
            lines.push(format!("function {key:?} {function:?}"));
        }
        for (_, key, symbol) in program.symbols.iter() {
            lines.push(format!("symbol {key:?} {symbol:?}"));
        }
        for (_, key, info) in program.types.iter() {
            lines.push(format!("type {key:?} {info:?}"));
        }
        for (_, key, info) in program.traits.iter() {
            lines.push(format!("trait {key:?} {info:?}"));
        }
        for (_, key, info) in program.trait_methods.iter() {
            lines.push(format!("trait method {key:?} {info:?}"));
        }
        for (_, info) in program.trait_implementations.iter() {
            lines.push(format!("trait implementation {info:?}"));
        }
        for (_, initializer) in program.initializers.iter() {
            lines.push(format!("initializer {initializer:?}"));
        }
        for (_, expression) in program.expressions.iter() {
            lines.push(format!("expression {expression:?}"));
        }
        for (_, pattern) in program.patterns.iter() {
            lines.push(format!("pattern {pattern:?}"));
        }
        for (_, place) in program.places.iter() {
            lines.push(format!("place {place:?}"));
        }
        for (_, block) in program.blocks.iter() {
            lines.push(format!("block {block:?}"));
        }
        for (_, item) in program.items.iter() {
            lines.push(format!("item {item:?}"));
        }
        lines.push(format!("semantic ids {:?}", program.semantic_ids));
        lines
    }

    #[test]
    fn stage_2_2_catalogs_are_stable_across_repeated_lowering() {
        let module = checked_program(concat!(
            "use std.coroutine.*\n",
            "let signal count: I32 = 1\n",
            "let doubled: I32 = count + count\n",
            "def add: (I32, I32) -> I32 = (left, right) => left + right\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "type TestBox T = ctor (value: T)\n",
            "type TestHidden = opaque\n",
            "type TestEnabled\n",
            "let enabled: TestEnabled = TestEnabled\n",
            "let boxed: TestBox I32 = TestBox 3\n",
            "def captured: () ->{state} I32 = () => { let mut local = 0; local = local + 1; local }\n",
        ));
        let mut first = LoweredProgram::default();
        let mut second = LoweredProgram::default();
        let diagnostics = first.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(first.validate().is_empty());
        assert!(second.snapshot(&module).is_empty());
        assert!(second.validate().is_empty());

        let first_snapshot = normalized_program_snapshot(&first);
        assert!(!first_snapshot.is_empty());
        assert_eq!(first_snapshot, normalized_program_snapshot(&second));
    }

    #[test]
    fn semantic_ids_match_transition_typed_module_selections() {
        let module = checked_program(concat!("use std.coroutine.*\n", "let answer: I32 = 42\n",));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let checked = module.semantic_ids();
        let ids = &program.semantic_ids;
        assert_eq!(ids.natural_trait, checked.natural_trait);
        assert_eq!(ids.sized_trait, checked.sized_trait);
        assert_eq!(ids.copy_trait, checked.copy_trait);
        assert_eq!(ids.drop_trait, checked.drop_trait);
        assert_eq!(ids.default_trait, checked.default_trait);
        assert_eq!(ids.debug_trait, checked.debug_trait);
        assert_eq!(ids.display_trait, checked.display_trait);
        assert_eq!(ids.index_trait, checked.index_trait);
        assert_eq!(ids.mutate_index_trait, checked.mutate_index_trait);
        assert_eq!(ids.into_iterator_trait, checked.into_iterator_trait);
        assert_eq!(ids.iterator_trait, checked.iterator_trait);
        assert_eq!(ids.io_type, checked.io_type);
        assert_eq!(ids.reactive_type, checked.reactive_type);
        assert_eq!(ids.coroutine_type, checked.coroutine_type);
        assert_eq!(ids.task_type, checked.task_type);
        assert_eq!(ids.completed_type, checked.completed_type);
        assert_eq!(ids.cancelled_type, checked.cancelled_type);
        assert_eq!(ids.tasks_type, checked.tasks_type);
        assert_eq!(ids.scheduler_type, checked.scheduler_type);
        assert_eq!(ids.wait_type, checked.wait_type);
        assert_eq!(ids.resolver_type, checked.resolver_type);
        assert_eq!(ids.completion_token_type, checked.completion_token_type);
        assert_eq!(ids.io_resource, module.io_resource());
        assert_eq!(ids.reactive_resource, module.reactive_resource());
        assert_eq!(
            ids.string_representation.as_ref(),
            module.string_representation()
        );
        assert_eq!(
            ids.entry_reactive_required,
            module.entry_reactive_required()
        );
    }

    #[test]
    fn validator_rejects_inconsistent_catalogs() {
        let module = checked_program("let answer: I32 = 42\n");
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        program.modules.entries.values[0].value.parent = Some(ModuleId(999));
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling module reference 999")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let entry_id = program.modules.entries.values[0].key;
        let body = program.initializers.values[0].body;
        program.initializers.push(LoweredInitializer {
            origin: Origin::compiler(),
            module: entry_id,
            executable_entry: false,
            resources: Vec::new(),
            body,
        });
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("initializers instead of one")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        program.symbols.entries.values[0].value.module = ModuleId(999);
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling module reference 999")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn validator_rejects_dangling_arena_references() {
        let mut program = LoweredProgram::default();
        let key = ExpressionKey {
            syntax: SyntaxId(4),
            owner: ExpressionOwner::Module(ModuleId(0)),
            context: ExpressionContext::Primary,
        };
        let id = program.expressions.push(LoweredExpression {
            key,
            origin: Origin::compiler(),
            value_type: CheckedType::I32,
            effects: CheckedEffectSet::default(),
            coercion: None,
            moved_symbols: Vec::new(),
            kind: LoweredExpressionKind::Block(BlockId::from_index(4)),
        });
        program.expression_lookup.insert(key, id);
        let diagnostics = program.validate();
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("dangling block reference 4")
        );
    }

    fn compiler_syntax() -> Syntax {
        Syntax::compiler()
    }

    fn wildcard_pattern() -> Pattern {
        Pattern::Wildcard(staple_syntax::WildcardPattern {
            syntax: compiler_syntax(),
            ty: Type::Inferred(staple_syntax::InferredType::new()),
        })
    }

    fn name_expression(name: &str) -> Expression {
        Expression::Name(staple_syntax::NameExpression {
            syntax: compiler_syntax(),
            name: name.to_owned(),
        })
    }

    fn empty_block() -> staple_syntax::BlockExpression {
        staple_syntax::BlockExpression {
            syntax: compiler_syntax(),
            items: Vec::new(),
        }
    }

    fn inferred_type() -> Type {
        Type::Inferred(staple_syntax::InferredType::new())
    }

    fn representative_expressions() -> Vec<(&'static str, Expression)> {
        use staple_syntax::*;
        let syntax = compiler_syntax();
        let mut expressions = Vec::new();
        expressions.push((
            "Function",
            Expression::Function(Box::new(FunctionExpression {
                syntax: syntax.clone(),
                parameter_style: FunctionParameterStyle::Single,
                pattern: wildcard_pattern(),
                body: Box::new(name_expression("value")),
            })),
        ));
        expressions.push((
            "Satisfies",
            Expression::Satisfies(Box::new(SatisfiesExpression {
                syntax: syntax.clone(),
                value: Box::new(name_expression("value")),
                ty: inferred_type(),
            })),
        ));
        expressions.push((
            "Match",
            Expression::Match(MatchExpression {
                syntax: syntax.clone(),
                subject: Box::new(name_expression("value")),
                arms: vec![MatchArm {
                    syntax: syntax.clone(),
                    pattern: wildcard_pattern(),
                    body: name_expression("value"),
                }],
            }),
        ));
        expressions.push((
            "Loop",
            Expression::Loop(LoopExpression {
                syntax: syntax.clone(),
                body: empty_block(),
            }),
        ));
        expressions.push((
            "Coro",
            Expression::Coro(CoroExpression {
                syntax: syntax.clone(),
                body: empty_block(),
            }),
        ));
        expressions.push((
            "Await",
            Expression::Await(AwaitExpression {
                syntax: syntax.clone(),
                operand: Box::new(name_expression("value")),
            }),
        ));
        expressions.push((
            "Resource",
            Expression::Resource(Box::new(ResourceExpression {
                syntax: syntax.clone(),
                resource: inferred_type(),
            })),
        ));
        expressions.push((
            "With",
            Expression::With(Box::new(WithResourceExpression {
                syntax: syntax.clone(),
                resource: inferred_type(),
                mutable: false,
                value: Box::new(name_expression("value")),
                body: empty_block(),
            })),
        ));
        expressions.push(("Block", Expression::Block(empty_block())));
        expressions.push((
            "Product",
            Expression::Product(ProductExpression {
                syntax: syntax.clone(),
                elements: vec![ProductElement {
                    syntax: syntax.clone(),
                    name: None,
                    designated: false,
                    value: name_expression("value"),
                    spread: false,
                    named_spread: false,
                }],
            }),
        ));
        expressions.push((
            "RepeatedProduct",
            Expression::RepeatedProduct(RepeatedProductExpression {
                syntax: syntax.clone(),
                value: Box::new(name_expression("value")),
                count: Box::new(inferred_type()),
            }),
        ));
        expressions.push((
            "Call",
            Expression::Call(CallExpression {
                syntax: syntax.clone(),
                callee: Box::new(name_expression("value")),
                argument: Box::new(name_expression("value")),
            }),
        ));
        expressions.push((
            "Access",
            Expression::Access(AccessExpression {
                syntax: syntax.clone(),
                value: Box::new(name_expression("value")),
                accessor: Accessor::Name("field".to_owned()),
            }),
        ));
        expressions.push((
            "Index",
            Expression::Index(IndexExpression {
                syntax: syntax.clone(),
                value: Box::new(name_expression("value")),
                index: Box::new(name_expression("position")),
            }),
        ));
        expressions.push((
            "Unary",
            Expression::Unary(UnaryExpression {
                syntax: syntax.clone(),
                operator_syntax: syntax.clone(),
                operator: UnaryOperator::Negate,
                operand: Box::new(name_expression("value")),
            }),
        ));
        expressions.push((
            "Binary",
            Expression::Binary(BinaryExpression {
                syntax: syntax.clone(),
                operator_syntax: syntax.clone(),
                operator: BinaryOperator::Add,
                left: Box::new(name_expression("value")),
                right: Box::new(name_expression("value")),
            }),
        ));
        expressions.push((
            "Logical",
            Expression::Logical(LogicalExpression {
                syntax: syntax.clone(),
                operator: LogicalOperator::And,
                left: Box::new(name_expression("value")),
                right: Box::new(name_expression("value")),
                bool_type: inferred_type(),
            }),
        ));
        expressions.push((
            "SyntaxArgument",
            Expression::SyntaxArgument(SyntaxArgumentExpression {
                syntax: syntax.clone(),
            }),
        ));
        expressions.push((
            "VisibilityArgument",
            Expression::VisibilityArgument(VisibilitySyntax {
                syntax: syntax.clone(),
                kind: VisibilityKind::Private,
            }),
        ));
        expressions.push((
            "Quote",
            Expression::Quote(QuoteExpression {
                syntax: syntax.clone(),
                kind: QuoteKind::Quote,
                path: Vec::new(),
                contents: syntax.clone(),
                template: QuoteTemplate::Raw,
            }),
        ));
        expressions.push((
            "Splice",
            Expression::Splice(SpliceExpression {
                syntax: syntax.clone(),
                name: "value".to_owned(),
                repeated: false,
            }),
        ));
        expressions.push(("Name", name_expression("value")));
        expressions.push((
            "String",
            Expression::String(StringExpression {
                syntax: syntax.clone(),
                literal: "\"value\"".to_owned(),
            }),
        ));
        expressions.push((
            "StringTemplate",
            Expression::StringTemplate(StringTemplateExpression {
                syntax: syntax.clone(),
                parts: vec![
                    StringTemplatePart::Literal("value ".to_owned()),
                    StringTemplatePart::Interpolation(StringInterpolation {
                        expression: Box::new(name_expression("value")),
                        format: StringInterpolationFormat::Display,
                    }),
                ],
            }),
        ));
        expressions.push((
            "CString",
            Expression::CString(CStringExpression {
                syntax: syntax.clone(),
                literal: "c\"value\"".to_owned(),
            }),
        ));
        expressions.push((
            "Integer",
            Expression::Integer(IntegerExpression {
                syntax: syntax.clone(),
                literal: "1".to_owned(),
            }),
        ));
        expressions.push((
            "Float",
            Expression::Float(FloatExpression {
                syntax,
                literal: "1.0".to_owned(),
            }),
        ));
        expressions
    }

    #[test]
    fn coverage_classifier_decides_every_expression_variant() {
        use ExpressionDisposition::{Deferred, Ordinary, Rejected};
        let representatives = representative_expressions();
        let mut names = representatives
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            representatives.len(),
            "representative expressions must cover each variant exactly once"
        );

        for (name, expression) in &representatives {
            assert_eq!(expression_variant_name(expression), *name);
            let expected = match *name {
                "Function" | "Call" => Deferred(DeferredExpressionFamily::Callable),
                "Resource" | "With" => Deferred(DeferredExpressionFamily::Resource),
                "Coro" | "Await" => Deferred(DeferredExpressionFamily::Coroutine),
                "Unary" | "Binary" | "SyntaxArgument" | "VisibilityArgument" | "Quote"
                | "Splice" => Rejected,
                "Satisfies" => Ordinary(Stage24Family::Satisfies),
                "Match" => Ordinary(Stage24Family::Match),
                "Loop" => Ordinary(Stage24Family::Loop),
                "Block" => Ordinary(Stage24Family::Block),
                "Product" => Ordinary(Stage24Family::Product),
                "RepeatedProduct" => Ordinary(Stage24Family::RepeatedProduct),
                "Access" => Ordinary(Stage24Family::Access),
                "Index" => Ordinary(Stage24Family::Index),
                "Logical" => Ordinary(Stage24Family::Logical),
                "Name" => Ordinary(Stage24Family::Name),
                "String" => Ordinary(Stage24Family::String),
                "StringTemplate" => Ordinary(Stage24Family::StringTemplate),
                "CString" => Ordinary(Stage24Family::CString),
                "Integer" => Ordinary(Stage24Family::Integer),
                "Float" => Ordinary(Stage24Family::Float),
                other => panic!("unclassified expression variant {other}"),
            };
            assert_eq!(classify_expression(expression), expected, "{name}");
        }
    }

    fn deferred_families(program: &LoweredProgram) -> Vec<DeferredExpressionFamily> {
        program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match expression.kind {
                LoweredExpressionKind::Deferred(family) => Some(family),
                _ => None,
            })
            .collect()
    }

    fn pending_families(program: &LoweredProgram) -> Vec<Stage24Family> {
        program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match expression.kind {
                LoweredExpressionKind::Pending(family) => Some(family),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn dispatcher_defers_later_stage_families_explicitly() {
        let module = checked_program(concat!(
            "use std.coroutine.(Coroutine)\n",
            "type Counter = ctor (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "def task: () -> Coroutine{} I32 = () => coro { 42 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro {\n",
            "  let value = await (task ()); value\n",
            "}\n",
            "let closure = (value: I32) => value\n",
            "let applied = task ()\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let deferred = deferred_families(&program);
        assert!(deferred.contains(&DeferredExpressionFamily::Callable));
        assert!(deferred.contains(&DeferredExpressionFamily::Resource));
        assert!(deferred.contains(&DeferredExpressionFamily::Coroutine));
        assert!(
            !pending_families(&program)
                .iter()
                .any(|family| matches!(family, Stage24Family::Block)),
            "blocks are lowered, not pending"
        );
    }

    #[test]
    fn occurrence_keys_deduplicate_ordinary_expressions_and_blocks() {
        let module = checked_program("def block_body = () => { let value: I32 = 1; value }\n");
        let function = module
            .functions()
            .iter()
            .find(|function| function.name == "block_body")
            .expect("block_body function");
        let Expression::Block(block) = &function.body else {
            panic!("expected a block body");
        };
        let owner = ExpressionOwner::Function(function.id);
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let lowered_blocks = program.blocks.iter().count();
        let lowered_expressions = program.expressions.iter().count();

        let first = program
            .lower_block(&module, owner, ExpressionContext::Primary, block)
            .expect("block should lower");
        let second = program
            .lower_block(&module, owner, ExpressionContext::Primary, block)
            .expect("block should lower again");
        assert_eq!(first, second);
        assert_eq!(program.blocks.iter().count(), lowered_blocks);
        assert_eq!(program.expressions.iter().count(), lowered_expressions);

        let first_expression = program
            .lower_expression(&module, owner, ExpressionContext::Primary, &function.body)
            .expect("block expression should lower");
        let second_expression = program
            .lower_expression(&module, owner, ExpressionContext::Primary, &function.body)
            .expect("block expression should lower again");
        assert_eq!(first_expression, second_expression);
        assert!(matches!(
            program.expressions.get(first_expression).unwrap().kind,
            LoweredExpressionKind::Block(id) if id == first
        ));
    }

    #[test]
    fn scalar_literals_decode_once_with_checked_payloads() {
        let module = checked_program(concat!(
            "use std.cinterop.*\n",
            "let small: U8 = 200\n",
            "let wide: I64 = 42\n",
            "let ratio: F32 = 1.5\n",
            "let precise: F64 = 2.25\n",
            "let text = \"hello\\n\"\n",
            "let ctext = c_string \"ok\"\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let integers = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Integer(integer) => {
                    Some((integer.value, integer.integer_type))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(integers.contains(&(200, IntegerType::U8)));
        assert!(integers.contains(&(42, IntegerType::I64)));

        let floats = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Float(float) => Some((float.value, float.float_type)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(floats.iter().any(|(value, ty)| *ty == FloatType::F32
            && (*value - f64::from(1.5_f32)).abs() < f64::EPSILON));
        assert!(floats.contains(&(2.25, FloatType::F64)));

        let strings = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::String(string) => Some(string.value.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(strings.contains(&"hello\n".to_owned()));

        let c_strings = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::CString(string) => Some(string.bytes.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(c_strings.contains(&b"ok\0".to_vec()));

        // Transition comparison: every scalar payload agrees with the checked
        // scalar type recorded for the same syntax occurrence.
        for (_, expression) in program.expressions.iter() {
            match &expression.kind {
                LoweredExpressionKind::Integer(integer) => assert_eq!(
                    module
                        .type_of_expression(expression.key.syntax)
                        .and_then(CheckedType::integer_type),
                    Some(integer.integer_type)
                ),
                LoweredExpressionKind::Float(float) => assert_eq!(
                    module
                        .type_of_expression(expression.key.syntax)
                        .and_then(CheckedType::float_type),
                    Some(float.float_type)
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn ordinary_names_record_storage_and_defer_callable_values() {
        let module = checked_program(concat!(
            "let global: I32 = 7\n",
            "let mut counter: I32 = 0\n",
            "let yes: Bool = True\n",
            "let copy: I32 = global\n",
            "let current: I32 = counter\n",
            "let truth: Bool = yes\n",
            "def identity = (value: I32) => value\n",
            "type Wrapper = ctor (value: I32)\n",
            "type MyString = ctor String\n",
            "companion MyString { pub def make = value: String => MyString (value) }\n",
            "let built = Wrapper (value: 1)\n",
            "let callable = identity\n",
            "let constructor = Wrapper\n",
            "let maker = MyString.make\n",
            "def capture = () => {\n",
            "  let mut count: I32 = 0\n",
            "  let reader = () => { count = count + 1; count }\n",
            "  reader\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let names = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Name(name) => Some(name),
                _ => None,
            })
            .collect::<Vec<_>>();
        let global = binding_symbol(&module, "global");
        let counter = binding_symbol(&module, "counter");
        assert!(names.iter().any(|name| {
            name.symbol == global && name.storage == SymbolStorage::GlobalStorage && !name.mutable
        }));
        assert!(names.iter().any(|name| {
            name.symbol == counter && name.storage == SymbolStorage::GlobalStorage && name.mutable
        }));
        assert!(
            names.iter().any(|name| name.singleton.is_some()),
            "singleton values keep their identity on the lowered name"
        );
        assert!(
            names.iter().any(|name| name.captured_cell),
            "mutable captures lower as captured-cell reads"
        );
        for (_, expression) in program.expressions.iter() {
            if let LoweredExpressionKind::Name(name) = &expression.kind {
                assert!(program.symbols.get(name.symbol).is_some());
                assert_eq!(
                    name.requires_initialization_check,
                    module
                        .resolved()
                        .requires_initialization_check(expression.key.syntax)
                );
            }
        }

        let deferred = program
            .expressions
            .iter()
            .filter(|(_, expression)| {
                matches!(
                    expression.kind,
                    LoweredExpressionKind::Deferred(DeferredExpressionFamily::Callable)
                )
            })
            .count();
        assert!(
            deferred >= 3,
            "function, constructor, and companion-method values defer to Stage 2.5"
        );
    }

    #[test]
    fn structural_access_lowers_representation_product_slice_and_scalar() {
        let module = checked_program(concat!(
            "use std.slice.Slice\n",
            "type MyString = ctor String\n",
            "type Pair = ctor (left: I32, right: I32)\n",
            "type Scalar = ctor (value: I32)\n",
            "let pair = Pair (left: 1, right: 2)\n",
            "let named = pair.left\n",
            "let other = pair.right\n",
            "let indexed = pair.0\n",
            "let text: MyString = MyString \"x\"\n",
            "let inner = text.*\n",
            "let scalar = Scalar (value: 5)\n",
            "let shortcut = scalar.value\n",
            "def slice_field: Ref (Slice I32) -> I32 = values => values.0\n",
            "def through_ref: Ref Pair -> I32 = reference => reference.left\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let accesses = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Access(access) => Some(access),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Representation { .. }))
        );
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Product { index: 0, .. }))
        );
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Product { index: 1, .. }))
        );
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Slice { index: 0, .. }))
        );
        assert!(
            accesses
                .iter()
                .any(|access| matches!(access.kind, LoweredAccessKind::Scalar { .. }))
        );
        assert!(
            accesses.iter().any(|access| match &access.kind {
                LoweredAccessKind::Product { dereference, .. }
                | LoweredAccessKind::Slice { dereference, .. }
                | LoweredAccessKind::Scalar { dereference } => !dereference.is_empty(),
                LoweredAccessKind::Representation { dereference } => !dereference.is_empty(),
            }),
            "access through `Ref` records its crossed payloads"
        );

        // Transition comparison: every access agrees with `CheckedAccess`.
        for (_, expression) in program.expressions.iter() {
            let LoweredExpressionKind::Access(access) = &expression.kind else {
                continue;
            };
            let checked = module
                .access_for(expression.key.syntax)
                .expect("lowered access has checked metadata");
            match (&access.kind, checked) {
                (
                    LoweredAccessKind::Representation { dereference },
                    CheckedAccess::Representation {
                        dereference: checked,
                    },
                ) => assert_eq!(dereference, checked),
                (
                    LoweredAccessKind::Product { index, dereference },
                    CheckedAccess::Product {
                        index: checked_index,
                        dereference: checked_dereference,
                        slice: false,
                        scalar: false,
                    },
                ) => {
                    assert_eq!(index, checked_index);
                    assert_eq!(dereference, checked_dereference);
                }
                (
                    LoweredAccessKind::Slice { index, dereference },
                    CheckedAccess::Product {
                        index: checked_index,
                        dereference: checked_dereference,
                        slice: true,
                        scalar: false,
                    },
                ) => {
                    assert_eq!(index, checked_index);
                    assert_eq!(dereference, checked_dereference);
                }
                (
                    LoweredAccessKind::Scalar { dereference },
                    CheckedAccess::Product {
                        dereference: checked_dereference,
                        scalar: true,
                        ..
                    },
                ) => assert_eq!(dereference, checked_dereference),
                (kind, checked) => panic!("access mismatch: {kind:?} vs {checked:?}"),
            }
        }
    }

    #[test]
    fn invalid_literal_payloads_are_lowering_diagnostics() {
        let module = checked_program("use std.cinterop.*\nlet value = c_string \"bad\\0value\"\n");
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("interior NUL byte")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn function_parameter_patterns_lower_every_source_form() {
        let module = checked_program(concat!(
            "type TestOwned = ctor String\n",
            "def pair = (left: I32, right: I32) => left + right\n",
            "def wildcard = (_: I32) => 0\n",
            "def moved: move String -> String = move value => value\n",
            "def moved_nominal: move TestOwned -> String = move TestOwned value => value\n",
            "def singleton: True -> I32 = True => 0\n",
            "def borrowed: Ref I32 -> I32 = (Ref inner) => inner\n",
            "def literal: \"literal\" -> I32 = \"literal\" => 0\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, pair) = lowered_function(&program, "pair");
        let product = lowered_pattern(&program, pair.parameter_pattern);
        assert!(matches!(product.value_type, CheckedType::Product(_)));
        let LoweredPatternKind::Product { elements, .. } = &product.kind else {
            panic!("pair should lower to a product pattern");
        };
        assert_eq!(elements.len(), 2);
        for element in elements {
            let pattern = lowered_pattern(&program, *element);
            let LoweredPatternKind::Binding {
                symbol: Some(symbol),
                singleton: None,
                ..
            } = &pattern.kind
            else {
                panic!("pair elements should bind symbols");
            };
            assert_eq!(pattern.value_type, CheckedType::I32);
            assert!(program.symbols.get(*symbol).is_some());
        }

        let (_, wildcard) = lowered_function(&program, "wildcard");
        let pattern = lowered_pattern(&program, wildcard.parameter_pattern);
        let LoweredPatternKind::Product { elements, .. } = &pattern.kind else {
            panic!("`(_: I32)` should lower to a single-element product");
        };
        assert_eq!(elements.len(), 1);
        let element = lowered_pattern(&program, elements[0]);
        assert!(matches!(element.kind, LoweredPatternKind::Wildcard));
        assert_eq!(element.value_type, CheckedType::I32);

        let (_, moved) = lowered_function(&program, "moved");
        let pattern = lowered_pattern(&program, moved.parameter_pattern);
        let LoweredPatternKind::Binding {
            symbol: Some(_),
            moved: true,
            mutable: false,
            ..
        } = &pattern.kind
        else {
            panic!("a `move` parameter should lower to a moved binding");
        };
        assert_eq!(pattern.value_type, CheckedType::String);

        let (_, moved_nominal) = lowered_function(&program, "moved_nominal");
        let pattern = lowered_pattern(&program, moved_nominal.parameter_pattern);
        let LoweredPatternKind::Nominal {
            moved: true,
            argument,
            ..
        } = &pattern.kind
        else {
            panic!("a `move` nominal parameter should retain its ownership marker");
        };
        assert!(matches!(
            lowered_pattern(&program, *argument).kind,
            LoweredPatternKind::Binding {
                symbol: Some(_),
                ..
            }
        ));

        let (_, singleton) = lowered_function(&program, "singleton");
        let pattern = lowered_pattern(&program, singleton.parameter_pattern);
        let LoweredPatternKind::Binding {
            symbol: None,
            singleton: Some(target),
            ..
        } = &pattern.kind
        else {
            panic!("a singleton parameter should lower to a name-like binding");
        };
        let source = module
            .functions()
            .iter()
            .find(|function| function.name.contains("singleton"))
            .expect("singleton function");
        assert_eq!(
            Some(*target),
            module
                .resolved()
                .type_for_pattern(source.pattern.syntax().id)
        );

        let (_, borrowed) = lowered_function(&program, "borrowed");
        let pattern = lowered_pattern(&program, borrowed.parameter_pattern);
        let LoweredPatternKind::Product { elements, .. } = &pattern.kind else {
            panic!("`(Ref inner)` should lower to a single-element product");
        };
        assert_eq!(elements.len(), 1);
        let pattern = lowered_pattern(&program, elements[0]);
        let LoweredPatternKind::Nominal {
            target: Some(target),
            name,
            moved: false,
            argument,
        } = &pattern.kind
        else {
            panic!("`Ref inner` should lower to a nominal pattern");
        };
        assert_eq!(name, "Ref");
        assert_eq!(
            module.resolved().builtin_type(*target),
            Some(BuiltinType::Ref)
        );
        let argument = lowered_pattern(&program, *argument);
        assert!(matches!(
            argument.kind,
            LoweredPatternKind::Binding {
                symbol: Some(_),
                ..
            }
        ));
        assert_eq!(argument.value_type, CheckedType::I32);

        let (_, literal) = lowered_function(&program, "literal");
        let pattern = lowered_pattern(&program, literal.parameter_pattern);
        let LoweredPatternKind::Literal { literal } = &pattern.kind else {
            panic!("a string literal parameter should lower to a literal pattern");
        };
        assert_eq!(literal, "\"literal\"");
    }

    #[test]
    fn pattern_binding_items_lower_patterns_and_propagation_metadata() {
        let module = checked_program(concat!(
            "pub type Wrapper = pub ctor (value: I32)\n",
            "pub type Ok T = pub ctor T\n",
            "pub type IOError = pub ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok(42)\n",
            "def patterns = () => {\n",
            "  let (first, second) = (1, 2)\n",
            "  let whole@(third, fourth) = (3, 4)\n",
            "  let Wrapper inner = Wrapper (value: 5)\n",
            "  let Ref payload = Ref 6\n",
            "  let Ok(value)? = read()\n",
            "  first + second + third + fourth + whole.0 + inner + payload + value\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, patterns) = lowered_function(&program, "patterns");
        let items = body_items(&program, patterns);
        assert_eq!(items.len(), 5);

        let LoweredItemKind::PatternBinding(binding) = &items[0].kind else {
            panic!("the product pattern binding should lower");
        };
        assert!(!binding.propagating && binding.propagation.is_none());
        assert!(matches!(
            lowered_pattern(&program, binding.pattern).kind,
            LoweredPatternKind::Product { .. }
        ));

        let LoweredItemKind::PatternBinding(binding) = &items[1].kind else {
            panic!("the at pattern binding should lower");
        };
        let LoweredPatternKind::At {
            binding: at_binding,
            pattern: nested,
        } = &lowered_pattern(&program, binding.pattern).kind
        else {
            panic!("`whole@(third, fourth)` should lower to an at pattern");
        };
        assert!(matches!(
            lowered_pattern(&program, *at_binding).kind,
            LoweredPatternKind::Binding {
                symbol: Some(_),
                ..
            }
        ));
        let LoweredPatternKind::Product { elements, .. } = &lowered_pattern(&program, *nested).kind
        else {
            panic!("the at pattern's nested pattern should be a product");
        };
        assert_eq!(elements.len(), 2);

        let LoweredItemKind::PatternBinding(binding) = &items[2].kind else {
            panic!("the nominal pattern binding should lower");
        };
        let LoweredPatternKind::Nominal {
            name,
            target: Some(target),
            ..
        } = &lowered_pattern(&program, binding.pattern).kind
        else {
            panic!("`Wrapper inner` should lower to a nominal pattern");
        };
        assert_eq!(name, "Wrapper");
        assert_eq!(
            program.types.get(*target).map(|info| info.name.as_str()),
            Some("Wrapper")
        );

        let LoweredItemKind::PatternBinding(binding) = &items[3].kind else {
            panic!("the reference pattern binding should lower");
        };
        assert!(matches!(
            lowered_pattern(&program, binding.pattern).kind,
            LoweredPatternKind::Nominal { .. }
        ));

        let LoweredItemKind::PatternBinding(binding) = &items[4].kind else {
            panic!("the propagating pattern binding should lower");
        };
        assert!(binding.propagating);
        let propagation = binding.propagation.as_ref().expect("checked propagation");
        assert_eq!(propagation.success_index, 0);
        let pattern = lowered_pattern(&program, binding.pattern);
        assert!(matches!(
            &pattern.kind,
            LoweredPatternKind::Nominal { name, .. } if name == "Ok"
        ));
        assert_eq!(pattern.value_type, propagation.source);
    }

    #[test]
    fn assignment_targets_lower_to_explicit_places() {
        let module = checked_program(concat!(
            "type Wrapper = ctor (value: I32)\n",
            "type Counter = ctor I32\n",
            "def places = (mut direct: I32, mut pair: (I32, I32), mut wrapper: Wrapper, mut counter: Counter, mut values: (I32; 2)) => {\n",
            "  direct = 1\n",
            "  pair.0 = 2\n",
            "  wrapper.value = 3\n",
            "  counter.* = 4\n",
            "  values[0] = 5\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, places) = lowered_function(&program, "places");
        let items = body_items(&program, places);
        assert_eq!(items.len(), 5);

        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("`direct = 1` should lower to an assignment item");
        };
        let LoweredPlaceKind::Symbol { symbol } = &lowered_place(&program, assignment.target).kind
        else {
            panic!("a parameter target should lower to symbol storage");
        };
        assert_eq!(assignment.initialization_symbol, Some(*symbol));
        assert!(assignment.mutate_index.is_none());
        assert!(!assignment.drop_previous && !assignment.signal);

        let LoweredItemKind::Assignment(assignment) = &items[1].kind else {
            panic!("`pair.0 = 2` should lower to an assignment item");
        };
        let LoweredPlaceKind::ProductElement {
            base,
            index: 0,
            slice: false,
        } = &lowered_place(&program, assignment.target).kind
        else {
            panic!("`pair.0` should lower to a product element place");
        };
        assert_eq!(
            assignment.initialization_symbol,
            program.place_root_symbol(*base)
        );

        let LoweredItemKind::Assignment(assignment) = &items[2].kind else {
            panic!("`wrapper.value = 3` should lower to an assignment item");
        };
        assert!(matches!(
            &lowered_place(&program, assignment.target).kind,
            LoweredPlaceKind::Representation { .. }
        ));

        let LoweredItemKind::Assignment(assignment) = &items[3].kind else {
            panic!("`counter.* = 4` should lower to an assignment item");
        };
        let LoweredPlaceKind::Representation { base } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`counter.*` should lower to a representation place");
        };
        assert!(matches!(
            &lowered_place(&program, *base).kind,
            LoweredPlaceKind::Symbol { .. }
        ));

        let LoweredItemKind::Assignment(assignment) = &items[4].kind else {
            panic!("`values[0] = 5` should lower to an assignment item");
        };
        let LoweredPlaceKind::Indexed { base, .. } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`values[0]` should lower to an indexed place");
        };
        assert!(matches!(
            &lowered_place(&program, *base).kind,
            LoweredPlaceKind::Symbol { .. }
        ));
        assert!(assignment.mutate_index.is_some());
        assert!(assignment.initialization_symbol.is_none());
    }

    #[test]
    fn assignment_places_cross_references_and_materialize_temporaries() {
        let module = checked_program(concat!(
            "def make_ref: () -> Ref (I32, I32) = () => Ref (1, 2)\n",
            "def through_ref = (mut reference: Ref (I32, I32)) => { reference.0 = 9 }\n",
            "def through_call = () => { (make_ref())[0] = 9 }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, through_ref) = lowered_function(&program, "through_ref");
        let items = body_items(&program, through_ref);
        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("the reference assignment should lower");
        };
        let LoweredPlaceKind::ProductElement { base, index: 0, .. } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`reference.0` should lower to a product element");
        };
        let deref_place = lowered_place(&program, *base);
        let LoweredPlaceKind::Dereference {
            reference,
            dereference: payloads,
        } = &deref_place.kind
        else {
            panic!("crossing `Ref` should lower to a dereference place");
        };
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads.last(), Some(&deref_place.value_type));
        assert!(program.expressions.contains(*reference));
        assert!(assignment.initialization_symbol.is_none());

        let (_, through_call) = lowered_function(&program, "through_call");
        let items = body_items(&program, through_call);
        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("the call-rooted assignment should lower");
        };
        let LoweredPlaceKind::Indexed { base, .. } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`(make_ref())[0]` should lower to an indexed place");
        };
        let LoweredPlaceKind::Temporary { expression } = &lowered_place(&program, *base).kind
        else {
            panic!("a non-place indexed base should materialize a temporary");
        };
        assert!(program.expressions.contains(*expression));
    }

    #[test]
    fn captured_cell_and_resource_places_are_explicit() {
        let module = checked_program(concat!(
            "def counter = () => {\n",
            "  let mut count: I32 = 0\n",
            "  let bump = () => { count = count + 1; count }\n",
            "  bump ()\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let (_, bump) = lowered_function(&program, "bump");
        let items = body_items(&program, bump);
        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("the captured assignment should lower");
        };
        let LoweredPlaceKind::CapturedCell { symbol } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("a captured mutable local should lower to a captured cell");
        };
        assert!(lowered_symbol(&program, *symbol).captured_cell);

        let module = checked_program(concat!(
            "type Counter = ctor (value: I32)\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, increment) = lowered_function(&program, "increment");
        let items = body_items(&program, increment);
        let LoweredItemKind::Assignment(assignment) = &items[0].kind else {
            panic!("the resource assignment should lower");
        };
        let LoweredPlaceKind::Representation { base } =
            &lowered_place(&program, assignment.target).kind
        else {
            panic!("`(resource Counter).value` should lower to a representation place");
        };
        let resource = lowered_place(&program, *base);
        let LoweredPlaceKind::Resource { resource: checked } = &resource.kind else {
            panic!("the representation base should be a resource place");
        };
        assert_eq!(resource.value_type, checked.value_type);
        let Some(id) = nominal_type_id(&resource.value_type) else {
            panic!("a resource type should be nominal");
        };
        assert_eq!(
            program.types.get(id).map(|info| info.name.as_str()),
            Some("Counter")
        );
    }

    #[test]
    fn module_items_record_binding_assignment_and_statement_metadata() {
        let program = snapshot(concat!(
            "const answer: I32 = 42\n",
            "let signal count: I32 = 0\n",
            "let doubled: I32 = count + count\n",
            "def generic: <T> move T -> T = move value => value\n",
            "let mut mutable: I32 = 1\n",
            "mutable = 2\n",
            "count = 1\n",
            "type Handle = ctor I32\n",
            "impl Drop Handle { def drop = Handle value => () }\n",
            "def discard = () => {\n",
            "  let owned: Handle = Handle 1\n",
            "  owned\n",
            "  ()\n",
            "}\n",
        ));
        let (entry_id, _) = entry_module(&program);
        let initializer = program
            .initializers
            .get(program.modules.get(entry_id).unwrap().initializer)
            .expect("entry initializer");
        let items = program
            .blocks
            .get(initializer.body)
            .expect("initializer body")
            .items
            .iter()
            .map(|item| program.items.get(*item).expect("item"))
            .collect::<Vec<_>>();

        let LoweredItemKind::Binding(binding) = &items[0].kind else {
            panic!("the const binding should lower");
        };
        // `const` values are folded, but every runtime reference still reads a
        // module global, so the binding is an ordinary runtime binding.
        assert!(!binding.compile_time_only);
        let const_symbol = binding.symbol.expect("the const binding symbol");
        assert!(binding.value.is_some());
        assert_eq!(
            program
                .symbols
                .get(const_symbol)
                .map(|symbol| symbol.storage),
            Some(SymbolStorage::GlobalStorage)
        );

        let LoweredItemKind::Binding(binding) = &items[1].kind else {
            panic!("the signal binding should lower");
        };
        assert!(binding.signal && !binding.derived);
        assert!(!binding.cell, "module bindings use global storage");
        assert!(binding.value.is_some());
        let signal_symbol = binding.symbol.expect("the signal binding symbol");

        let LoweredItemKind::Binding(binding) = &items[2].kind else {
            panic!("the derived binding should lower");
        };
        assert!(binding.derived && !binding.signal);

        let LoweredItemKind::Binding(binding) = &items[3].kind else {
            panic!("the generic binding should lower");
        };
        assert!(binding.generic);
        assert!(binding.value.is_some());

        let LoweredItemKind::Binding(binding) = &items[4].kind else {
            panic!("the mutable binding should lower");
        };
        assert!(!binding.cell);

        let LoweredItemKind::Assignment(assignment) = &items[5].kind else {
            panic!("the mutable global assignment should lower");
        };
        assert!(assignment.mutate_index.is_none());
        assert!(assignment.initialization_symbol.is_some());
        assert!(!assignment.signal && !assignment.drop_previous);

        let LoweredItemKind::Assignment(assignment) = &items[6].kind else {
            panic!("the signal assignment should lower");
        };
        assert!(assignment.signal);
        assert_eq!(assignment.initialization_symbol, Some(signal_symbol));

        let (_, discard) = lowered_function(&program, "discard");
        let items = body_items(&program, discard);
        let LoweredItemKind::Expression(statement) = &items[1].kind else {
            panic!("the discarded handle should lower to a statement");
        };
        assert!(statement.drop_result);
        assert!(matches!(
            program
                .expressions
                .get(statement.expression)
                .expect("statement expression")
                .value_type,
            CheckedType::Distinct { .. }
        ));
    }

    #[test]
    fn function_bodies_lower_into_blocks_with_separate_results() {
        let module = checked_program(concat!(
            "def expression_body = () => 42\n",
            "def block_body = () => { let value: I32 = 1; value }\n",
            "def early = () => { return 1; 0 }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let (_, expression_body) = lowered_function(&program, "expression_body");
        let block = body_block(&program, expression_body);
        assert!(block.items.is_empty());
        let result = block.result.expect("the body expression is the result");
        assert_eq!(
            program.expressions.get(result).unwrap().value_type,
            CheckedType::I32
        );

        let (_, block_body) = lowered_function(&program, "block_body");
        let block = body_block(&program, block_body);
        assert_eq!(block.items.len(), 1);
        assert!(matches!(
            program.items.get(block.items[0]).unwrap().kind,
            LoweredItemKind::Binding(_)
        ));
        let result = block.result.expect("the tail expression is the result");
        assert!(program.expressions.contains(result));
        assert!(
            !block.items.iter().any(|item| matches!(
                &program.items.get(*item).unwrap().kind,
                LoweredItemKind::Expression(statement) if statement.expression == result
            )),
            "a block result must not also be an item"
        );

        let (_, early) = lowered_function(&program, "early");
        let block = body_block(&program, early);
        assert_eq!(block.items.len(), 1);
        let LoweredItemKind::Return(item) = &program.items.get(block.items[0]).unwrap().kind else {
            panic!("`return` should lower to a return item");
        };
        assert!(program.expressions.contains(item.value));
        let result = block
            .result
            .expect("the unreachable tail is still the result");
        assert_eq!(
            program.expressions.get(result).unwrap().value_type,
            CheckedType::I32
        );
    }

    #[test]
    fn lowering_rejects_compile_time_only_runtime_nodes() {
        let module = checked_program("let answer: I32 = 1\n");
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());

        let splice = Item::RepeatedItemSplice(staple_syntax::RepeatedItemSplice {
            syntax: staple_syntax::Syntax::compiler(),
            name: "items".to_owned(),
        });
        let owner = ExpressionOwner::Module(ModuleId(0));
        let diagnostic = program
            .lower_item(&module, owner, ExpressionContext::Primary, &splice)
            .expect_err("a repeated item splice should be rejected");
        assert!(diagnostic.message.contains("repeated item splice"));

        let splice = Item::VisibilitySplice(staple_syntax::VisibilitySplice {
            syntax: staple_syntax::Syntax::compiler(),
            name: "item".to_owned(),
            item: Box::new(Item::Expression(Expression::Integer(
                staple_syntax::IntegerExpression {
                    syntax: staple_syntax::Syntax::compiler(),
                    literal: "1".to_owned(),
                },
            ))),
        });
        let diagnostic = program
            .lower_item(&module, owner, ExpressionContext::Primary, &splice)
            .expect_err("a visibility splice should be rejected");
        assert!(diagnostic.message.contains("visibility splice"));

        let source = staple_syntax::parse("1 + 2").expect("binary source should parse");
        let Some(Item::Expression(expression)) = source.items.first() else {
            panic!("`1 + 2` should parse to an expression item");
        };
        let diagnostic = reject_compile_time_expression(expression)
            .expect_err("an unresolved binary expression should be rejected");
        assert!(diagnostic.message.contains("unresolved `+`"));

        let source = staple_syntax::parse("-1").expect("unary source should parse");
        let Some(Item::Expression(expression)) = source.items.first() else {
            panic!("`-1` should parse to an expression item");
        };
        let diagnostic = reject_compile_time_expression(expression)
            .expect_err("an unresolved unary expression should be rejected");
        assert!(diagnostic.message.contains("unresolved `-`"));
    }

    #[test]
    fn validator_rejects_dangling_pattern_place_and_item_references() {
        let module = checked_program(concat!(
            "def places = (mut value: I32) => { value = value + 1; value }\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        program.places.values[0].kind = LoweredPlaceKind::ProductElement {
            base: PlaceId::from_index(999_999),
            index: 0,
            slice: false,
        };
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling place reference")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        program.patterns.values[0].kind = LoweredPatternKind::At {
            binding: PatternId::from_index(999_999),
            pattern: PatternId::from_index(999_998),
        };
        let diagnostics = program.validate();
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.message.contains("at pattern"))
                .count(),
            2,
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        let (item_id, value) = program
            .items
            .iter()
            .find_map(|(id, item)| match &item.kind {
                LoweredItemKind::Assignment(assignment) => Some((id, assignment.value)),
                _ => None,
            })
            .expect("the assignment should lower");
        program.items.values[item_id.index()].kind =
            LoweredItemKind::Assignment(LoweredAssignmentItem {
                target: PlaceId::from_index(999_999),
                value,
                mutate_index: None,
                initialization_symbol: None,
                drop_previous: false,
                signal: false,
            });
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling place reference")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn stage_2_3_arenas_are_stable_across_repeated_lowering() {
        let module = checked_program(concat!(
            "pub type Wrapper = pub ctor (value: I32)\n",
            "pub type Ok T = pub ctor T\n",
            "pub type IOError = pub ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok(42)\n",
            "let signal count: I32 = 0\n",
            "let doubled: I32 = count + count\n",
            "count = 1\n",
            "def places = (mut pair: (I32, I32), mut values: (I32; 2)) => {\n",
            "  pair.0 = 1\n",
            "  values[0] = 2\n",
            "}\n",
            "def patterns = () => {\n",
            "  let (first, second) = (1, 2)\n",
            "  let Ok(value)? = read()\n",
            "  let Wrapper inner = Wrapper (value: first + second + value)\n",
            "  inner\n",
            "}\n",
        ));
        let mut first = LoweredProgram::default();
        let mut second = LoweredProgram::default();
        let diagnostics = first.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(first.validate().is_empty());
        assert!(second.snapshot(&module).is_empty());
        assert!(second.validate().is_empty());

        let first_snapshot = normalized_program_snapshot(&first);
        assert!(!first_snapshot.is_empty());
        assert_eq!(first_snapshot, normalized_program_snapshot(&second));
    }
}
