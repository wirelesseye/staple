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
    CheckedFunctionalDependency, CheckedMutation, CheckedProductType, CheckedPropagation,
    CheckedResource, CheckedTraitBound, CheckedTraitDispatch, CheckedType, DefinitionId, FloatType,
    FunctionId, IntegerType, IntrinsicFunction, ModuleId, RecursiveConstruction, ResolvedFunction,
    ResolvedModule, SourceModule, StructuralTraitMethod, SymbolId, TraitId, TraitMethodId, TypeId,
    TypeParameterId, TypedModule, contains_type_parameter,
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
arena_id!(LoweredCallId);
arena_id!(LoweredCallableValueId);

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
/// exactly one family, and every family has a concrete lowered payload.
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
    Product(LoweredProduct),
    RepeatedProduct(LoweredRepeatedProduct),
    /// `value satisfies Type`: a transparent wrapper. The parent expression
    /// header remains authoritative for the checked coercion.
    Satisfies(LoweredSatisfies),
    Logical(LoweredLogical),
    Loop(LoweredLoop),
    Match(LoweredMatch),
    Index(LoweredIndex),
    StringTemplate(LoweredStringTemplate),
    /// An owned call with its explicit target and argument plan.
    Call(LoweredCallId),
    /// An owned first-class callable value with its construction plan.
    CallableValue(LoweredCallableValueId),
}

/// A string template with its ordered parts and checked formatting
/// selections. Literal text is retained exactly after source decoding, and
/// interpolations keep the selected formatting trait/method and value type.
/// Helper instantiation and artifact deduplication remain Stage 2.5/4 work.
#[derive(Debug, Clone)]
pub(crate) struct LoweredStringTemplate {
    pub parts: Vec<LoweredStringTemplatePart>,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredStringTemplatePart {
    Literal(String),
    Interpolation(LoweredInterpolation),
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredInterpolation {
    pub expression: ExpressionId,
    pub format: staple_syntax::StringInterpolationFormat,
    pub value_type: CheckedType,
    pub trait_id: TraitId,
    pub method: TraitMethodId,
}

/// Standard formatter helper selections copied from checked metadata so later
/// emission never discovers them by name.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoweredStringFormatting {
    pub constructor: Option<FunctionId>,
    pub write: Option<FunctionId>,
    pub finish: Option<FunctionId>,
}

/// The eight explicit callable categories. Every call and callable value has
/// exactly one category; there is no unknown/fallback variant. Variants carry
/// semantic IDs only, never backend symbol strings or debug-formatted keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LoweredCallableCategory {
    DirectKnownFunction,
    IndirectClosure,
    ExternalFunction,
    Intrinsic,
    Constructor,
    TraitImplementation,
    StructuralTraitMethod,
    CompilerHelper,
}

impl LoweredCallableCategory {
    /// Every category, used by the decision-table test to prove that each one
    /// has at least one explicit route and target representation.
    pub(crate) const ALL: [LoweredCallableCategory; 8] = [
        LoweredCallableCategory::DirectKnownFunction,
        LoweredCallableCategory::IndirectClosure,
        LoweredCallableCategory::ExternalFunction,
        LoweredCallableCategory::Intrinsic,
        LoweredCallableCategory::Constructor,
        LoweredCallableCategory::TraitImplementation,
        LoweredCallableCategory::StructuralTraitMethod,
        LoweredCallableCategory::CompilerHelper,
    ];
}

/// A typed callable target. This is the semantic identity a call or callable
/// value invokes; concrete instances and generated adapters stay in Stage 3/4.
#[derive(Debug, Clone)]
pub(crate) enum LoweredCallableTarget {
    /// A known function invoked directly. The environment is `None` for a
    /// null environment and `Current` for same-function recursion, so the
    /// category alone does not imply a null environment.
    DirectFunction {
        function: FunctionId,
        environment: LoweredCallEnvironment,
    },
    /// A callee value evaluated at the call site and invoked through its
    /// closure representation.
    IndirectClosure { callee: ExpressionId },
    /// An `extern` binding.
    ExternalFunction { symbol: SymbolId },
    /// A compiler intrinsic selected by the checker.
    Intrinsic {
        symbol: SymbolId,
        intrinsic: IntrinsicFunction,
    },
    /// A nominal constructor. `recursive` distinguishes managed-reference
    /// construction from ordinary nominal wrapping.
    Constructor {
        symbol: SymbolId,
        type_id: TypeId,
        recursive: Option<RecursiveConstruction>,
    },
    /// An explicit trait implementation method. `function` is absent while
    /// the selection depends on Stage 3 substitution; the call's evidence
    /// recipe retains the declared obligation.
    TraitImplementation {
        trait_id: TraitId,
        method: TraitMethodId,
        function: Option<FunctionId>,
    },
    /// A structural trait method generated by the compiler.
    StructuralTraitMethod {
        trait_id: TraitId,
        method: TraitMethodId,
        structural: StructuralTraitMethod,
    },
    /// A compiler helper function selected by checked operations. Complete
    /// helper discovery and deduplication remain Stage 4 work.
    CompilerHelper { function: FunctionId },
}

impl LoweredCallableTarget {
    pub(crate) fn category(&self) -> LoweredCallableCategory {
        match self {
            LoweredCallableTarget::DirectFunction { .. } => {
                LoweredCallableCategory::DirectKnownFunction
            }
            LoweredCallableTarget::IndirectClosure { .. } => {
                LoweredCallableCategory::IndirectClosure
            }
            LoweredCallableTarget::ExternalFunction { .. } => {
                LoweredCallableCategory::ExternalFunction
            }
            LoweredCallableTarget::Intrinsic { .. } => LoweredCallableCategory::Intrinsic,
            LoweredCallableTarget::Constructor { .. } => LoweredCallableCategory::Constructor,
            LoweredCallableTarget::TraitImplementation { .. } => {
                LoweredCallableCategory::TraitImplementation
            }
            LoweredCallableTarget::StructuralTraitMethod { .. } => {
                LoweredCallableCategory::StructuralTraitMethod
            }
            LoweredCallableTarget::CompilerHelper { .. } => LoweredCallableCategory::CompilerHelper,
        }
    }
}

/// How a direct call obtains its closure environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredCallEnvironment {
    /// The target takes a null environment.
    None,
    /// The call reuses the enclosing closure environment (same-function
    /// recursion).
    Current,
}

/// The adapter a first-class callable value needs before it can be invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredCallableAdapter {
    None,
    Constructor,
    External,
    Curried,
    NestedClosure,
    ImplicitThunk,
}

/// How a closure environment holds one capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredCaptureAccess {
    ByValue,
    Borrowed,
    SharedCell,
}

/// The environment a closure construction allocates or reuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredClosureEnvironment {
    /// Construction allocates a fresh environment.
    Fresh,
    /// The value needs no environment.
    None,
    /// Construction reuses the enclosing environment (recursion).
    Current,
}

/// One capture of a closure construction, copied from the function catalog in
/// capture order with its ownership and access facts.
#[derive(Debug, Clone)]
pub(crate) struct LoweredClosureCapture {
    /// The catalog capture record (symbol, borrowing, non-owning, cell facts).
    pub capture: LoweredCapture,
    pub value_type: CheckedType,
    pub access: LoweredCaptureAccess,
    /// The constructed environment owns the capture value.
    pub owns_value: bool,
    /// The capture requires a drop when the environment is destroyed.
    pub drops_value: bool,
}

/// A closure construction plan: the target function, its ordered captures,
/// environment reuse, and adapter requirement. Target-specific environment
/// layout stays in LLVM.
#[derive(Debug, Clone)]
pub(crate) struct LoweredClosureConstruction {
    pub function: FunctionId,
    pub captures: Vec<LoweredClosureCapture>,
    pub environment: LoweredClosureEnvironment,
    pub adapter: LoweredCallableAdapter,
    pub substitutions: CallSubstitutions,
}

/// Compile-time substitutions recorded at a call or closure use site. The
/// mapping retains unresolved declared template parameters instead of
/// inventing a concrete instance; Stage 3 owns instance interning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CallSubstitutions {
    pub types: Vec<CallTypeSubstitution>,
    pub effects: Vec<CallEffectSubstitution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallTypeSubstitution {
    pub parameter: TypeParameterId,
    pub value_type: CheckedType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallEffectSubstitution {
    pub parameter: TypeParameterId,
    pub effects: CheckedEffectSet,
}

/// Trait selection evidence for a call, index read, indexed mutation, or
/// formatting interpolation. Negative implementations stay rejection data and
/// never become callable targets.
#[derive(Debug, Clone)]
pub(crate) enum TraitEvidence {
    /// A selected explicit implementation and its method function.
    ExplicitImplementation {
        trait_id: TraitId,
        implementation: LoweredTraitImplementationId,
        method: TraitMethodId,
        function: FunctionId,
        arguments: Vec<CheckedType>,
    },
    /// A selected structural method with its completed trait arguments.
    Structural {
        trait_id: TraitId,
        method: TraitMethodId,
        structural: StructuralTraitMethod,
        arguments: Vec<CheckedType>,
    },
    /// A declared bound or implementation prerequisite that Stage 3 must
    /// realize after substitution; no implementation is chosen yet.
    DeclaredBound {
        trait_id: TraitId,
        method: Option<TraitMethodId>,
        arguments: Vec<CheckedType>,
        prerequisites: Vec<CheckedTraitBound>,
    },
    /// Rejection data from a negative implementation.
    RejectedImplementation {
        trait_id: TraitId,
        implementation: LoweredTraitImplementationId,
        arguments: Vec<CheckedType>,
    },
}

/// How one visible call argument is passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweredArgumentPassMode {
    /// Passed by value in its ABI slot.
    Value,
    /// Passed as a borrowed pointer to the caller's value.
    BorrowedPointer,
    /// Passed as a mutable place pointer.
    MutablePlace,
    /// Materialized into a temporary and passed by pointer.
    MaterializedTemporary,
}

/// One visible call argument with its final ABI slot and temporary/writeback
/// facts. Final slot mapping is stored here, separately from evaluation order.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCallArgument {
    pub expression: ExpressionId,
    /// The final ABI slot, absent when the argument is materialized.
    pub slot: Option<usize>,
    pub pass_mode: LoweredArgumentPassMode,
    /// The checked type expected for this argument.
    pub expected: CheckedType,
    /// The argument's source place, when it has one.
    pub place: Option<PlaceId>,
    /// The value is materialized into a temporary for the pass.
    pub temporary: bool,
    /// The temporary's updated value is written back after the call.
    pub writeback: bool,
    /// The temporary must be dropped after the call.
    pub drops_after_call: bool,
}

/// An ordered call step. Explicit source arguments execute in source order;
/// defaults execute in the order the checked plan selected.
#[derive(Debug, Clone)]
pub(crate) enum LoweredCallStep {
    /// Evaluate the callee occurrence of an indirect call.
    Callee { expression: ExpressionId },
    /// Evaluate a visible argument in source order.
    Argument { argument: usize },
    /// Place one element of a product argument into a final slot.
    ProductElement {
        argument: usize,
        slot: usize,
        expression: ExpressionId,
    },
    /// Expand a positional spread into an argument's final slots.
    ProductSpread {
        argument: usize,
        expression: ExpressionId,
        mappings: Vec<LoweredSpreadMapping>,
    },
    /// Expand a named spread into an argument's final slots.
    NamedProductSpread {
        argument: usize,
        expression: ExpressionId,
        mappings: Vec<LoweredNamedSpreadMapping>,
    },
    /// Evaluate a contextual default for one argument slot.
    Default {
        argument: usize,
        slot: usize,
        expression: ExpressionId,
        expected: CheckedType,
    },
    /// Look up the provider for a hidden resource requirement.
    Resource { resource: usize },
    /// Perform the invocation.
    Invoke,
}

/// A complete call: explicit target, ordered argument plan, hidden resource
/// requirements, and the site-specific substitution/evidence recipe.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCall {
    pub origin: Origin,
    pub target: LoweredCallableTarget,
    /// The callee occurrence for an indirect call; direct targets reference
    /// their semantic ID instead.
    pub callee: Option<ExpressionId>,
    /// The checked function type after call-site inference.
    pub function_type: CheckedFunctionType,
    /// Ordered visible arguments with their final ABI slots.
    pub arguments: Vec<LoweredCallArgument>,
    /// Ordered hidden resource requirements from the checked effect row.
    /// Lexical provider resolution is Stage 2.6 work.
    pub resources: Vec<CheckedResource>,
    /// Checked mutation and move markers for the parameter slots.
    pub mutations: Vec<CheckedMutation>,
    pub moves: Vec<CheckedMutation>,
    /// Symbols whose initialization state the call must check.
    pub initialization_checks: Vec<SymbolId>,
    /// Ordered evaluation steps.
    pub steps: Vec<LoweredCallStep>,
    /// The call's checked result type.
    pub result_type: CheckedType,
    /// Compile-time substitutions known at the call site.
    pub substitutions: CallSubstitutions,
    /// Trait evidence for trait-dispatched calls.
    pub evidence: Option<TraitEvidence>,
}

/// A first-class callable value with its explicit target and construction
/// plan.
#[derive(Debug, Clone)]
pub(crate) struct LoweredCallableValue {
    pub origin: Origin,
    pub target: LoweredCallableTarget,
    /// The checked function type of the value after call-site inference.
    pub function_type: CheckedFunctionType,
    pub adapter: LoweredCallableAdapter,
    /// Closure construction for values that build an environment.
    pub closure: Option<LoweredClosureConstruction>,
    /// Compile-time substitutions known at the use site.
    pub substitutions: CallSubstitutions,
    /// Trait evidence for trait-method values.
    pub evidence: Option<TraitEvidence>,
}

/// The runtime call route a checked call follows, mirroring the backend's
/// decision order. Every call has exactly one route; the indirect closure
/// route is the fallback, never an unknown-callable case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CallRoute {
    /// A completed juxtaposed call chain through a closure value.
    Juxtaposed,
    /// A juxtaposed chain whose callee is a compiler intrinsic.
    JuxtaposedIntrinsic,
    /// A curried call with checked defaults or a `_` partial application.
    CurriedDefault,
    /// A trait-dispatched call with a selected explicit implementation.
    TraitImplementation,
    /// A trait-dispatched call whose implementation depends on Stage 3
    /// substitution.
    DeclaredTraitBound,
    /// A trait-dispatched call resolved structurally.
    StructuralTraitMethod,
    /// A compiler intrinsic.
    Intrinsic,
    /// A direct call to a captureless generic function template.
    GenericDirect,
    /// An `extern` binding.
    External,
    /// A call through a first-class closure value.
    Indirect,
    /// A nominal constructor.
    Constructor,
    /// A primitive macro call (`c_string`).
    PrimitiveMacro,
    /// A compiler helper selected by checked operations.
    CompilerHelper,
}

impl CallRoute {
    /// Every route, checked by the decision-table test against the exhaustive
    /// category mapping.
    pub(crate) const ALL: [CallRoute; 13] = [
        CallRoute::Juxtaposed,
        CallRoute::JuxtaposedIntrinsic,
        CallRoute::CurriedDefault,
        CallRoute::TraitImplementation,
        CallRoute::DeclaredTraitBound,
        CallRoute::StructuralTraitMethod,
        CallRoute::Intrinsic,
        CallRoute::GenericDirect,
        CallRoute::External,
        CallRoute::Indirect,
        CallRoute::Constructor,
        CallRoute::PrimitiveMacro,
        CallRoute::CompilerHelper,
    ];

    /// The single explicit callable category this route produces. The
    /// exhaustive match is the compile-time half of the decision table.
    pub(crate) fn category(self) -> LoweredCallableCategory {
        use LoweredCallableCategory::*;
        match self {
            CallRoute::Juxtaposed | CallRoute::CurriedDefault | CallRoute::Indirect => {
                IndirectClosure
            }
            CallRoute::JuxtaposedIntrinsic | CallRoute::Intrinsic | CallRoute::PrimitiveMacro => {
                Intrinsic
            }
            CallRoute::TraitImplementation | CallRoute::DeclaredTraitBound => TraitImplementation,
            CallRoute::StructuralTraitMethod => StructuralTraitMethod,
            CallRoute::GenericDirect => DirectKnownFunction,
            CallRoute::External => ExternalFunction,
            CallRoute::Constructor => Constructor,
            CallRoute::CompilerHelper => CompilerHelper,
        }
    }
}

/// A checked index read: `base[index]`. The complete checked `Index` dispatch
/// recipe is copied here; Stage 2.5 converts it into explicit callable
/// evidence without consulting the type checker. The operands are lowered in
/// source evaluation order (base before index).
#[derive(Debug, Clone)]
pub(crate) struct LoweredIndex {
    pub base: ExpressionId,
    pub index: ExpressionId,
    /// The checked `Index` dispatch as recorded by the type checker.
    pub dispatch: CheckedTraitDispatch,
    /// Owning trait of the dispatched method.
    pub trait_id: TraitId,
    /// Functional-dependency-completed trait arguments.
    pub arguments: Vec<CheckedType>,
    /// Instantiated method type, carrying mutation/move masks, effects,
    /// resources, and the result type.
    pub method_type: Option<CheckedFunctionType>,
    /// The whole `(base, index)` argument is materialized into a temporary so
    /// a whole-argument mutation/move can be passed by address.
    pub whole_temporary: bool,
    /// The base operand is materialized into a temporary for a mutation.
    pub base_temporary: bool,
    /// The index operand is materialized into a temporary for a mutation.
    pub index_temporary: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredSatisfies {
    pub value: ExpressionId,
}

/// A short-circuiting logical operator with its checked `Bool` selection.
/// Operands are always evaluated left-to-right and `right` only when the
/// short-circuit does not determine the result.
#[derive(Debug, Clone)]
pub(crate) struct LoweredLogical {
    pub operator: staple_syntax::LogicalOperator,
    pub left: ExpressionId,
    pub right: ExpressionId,
    pub bool_type: CheckedType,
    /// Resolved `True` alternative of the checked `Bool` sum.
    pub true_index: usize,
}

/// A loop with its lowered body block and cleanup metadata. `break`/`continue`
/// items reference the enclosing loop through the recorded nesting depth.
#[derive(Debug, Clone)]
pub(crate) struct LoweredLoop {
    pub body: BlockId,
    pub result_type: CheckedType,
    /// The body result must be dropped before the back edge.
    pub drops_body_result: bool,
    /// The body can complete normally; a body whose tail diverges exits only
    /// through `break` or an enclosing return.
    pub body_falls_through: bool,
    /// One-based nesting depth of this loop, matching the `loop_depth`
    /// recorded on the break/continue items it owns.
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredMatch {
    pub subject: ExpressionId,
    /// Checked subject type copied from `CheckedMatch`.
    pub source: CheckedType,
    pub arms: Vec<LoweredMatchArm>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredMatchArm {
    pub origin: Origin,
    pub pattern: PatternId,
    pub body: ExpressionId,
    /// Symbols the pattern binds, in source order. The arm's cleanup boundary
    /// is the enclosing ownership state plus these bindings; a divergent arm
    /// (a `Never` body) never contributes a runtime value.
    pub bound_symbols: Vec<SymbolId>,
}

/// A product construction plan. `steps` preserve source evaluation order
/// (including spread expansion and default evaluation); `fields` is the final
/// checked layout, where later steps override earlier values for a slot.
#[derive(Debug, Clone)]
pub(crate) struct LoweredProduct {
    /// The final checked product shape, defaults cleared.
    pub final_type: CheckedProductType,
    /// Ordered source evaluation steps.
    pub steps: Vec<LoweredProductStep>,
    /// Final layout: exactly one expression per slot.
    pub fields: Vec<ExpressionId>,
}

#[derive(Debug, Clone)]
pub(crate) enum LoweredProductStep {
    /// A positional element placed at the next positional slot.
    Positional {
        expression: ExpressionId,
        slot: usize,
    },
    /// A named element (designator or named-spread entry).
    Designated {
        name: String,
        expression: ExpressionId,
        slot: usize,
    },
    /// A positional spread expanded against the operand's checked product.
    PositionalSpread {
        expression: ExpressionId,
        mappings: Vec<LoweredSpreadMapping>,
    },
    /// A named spread expanded against the operand's checked product.
    NamedSpread {
        expression: ExpressionId,
        mappings: Vec<LoweredNamedSpreadMapping>,
    },
    /// A missing contextual default evaluated in final slot order.
    Default {
        slot: usize,
        expression: ExpressionId,
        expected: CheckedType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoweredSpreadMapping {
    /// Element position inside the spread operand.
    pub source: usize,
    /// Destination slot in the final product layout.
    pub slot: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoweredNamedSpreadMapping {
    pub name: String,
    /// Element position inside the spread operand.
    pub source: usize,
    /// Destination slot in the final product layout.
    pub slot: usize,
}

/// A repeated product `(value; count)`. The element is evaluated exactly once;
/// `collapsed` marks the fixed `count == 1` representation that is the element
/// itself rather than a single-field product.
#[derive(Debug, Clone)]
pub(crate) struct LoweredRepeatedProduct {
    pub expression: ExpressionId,
    pub count: LoweredRepeatCount,
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoweredRepeatCount {
    Fixed(usize),
    Symbolic(CheckedType),
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    Continue(LoweredContinueItem),
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
    /// One-based nesting depth of the loop this break targets.
    pub loop_depth: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct LoweredContinueItem {
    /// One-based nesting depth of the loop this continue targets.
    pub loop_depth: usize,
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
    calls: Arena<LoweredCall, LoweredCallId>,
    callable_values: Arena<LoweredCallableValue, LoweredCallableValueId>,
    initializers: Arena<LoweredInitializer, InitializerId>,
    semantic_ids: LoweredSemanticIds,
    string_formatting: LoweredStringFormatting,
    /// Transient lowering state: the number of currently enclosing loops,
    /// recorded on break/continue items and loop nodes so validation can tie
    /// exits to the loop that owns them. Not part of the lowered program.
    loop_depth: usize,
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
        diagnostics.extend(self.validate_runtime_metadata(module));
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
        let formatting = module.string_formatting();
        self.string_formatting = LoweredStringFormatting {
            constructor: formatting.formatter_new,
            write: formatting.formatter_write,
            finish: formatting.formatter_finish,
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
                loop_depth: self.loop_depth,
            }),
            Item::Continue(_) => LoweredItemKind::Continue(LoweredContinueItem {
                loop_depth: self.loop_depth,
            }),
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
        self.lower_expression_occurrence(module, owner, context, expression, None)
    }

    /// Lowers one expression occurrence with an optional root type override.
    /// Contextual product defaults use it because the shared default syntax
    /// node may have been checked last for a different instantiation; the
    /// consumer's checked slot type is authoritative for the root.
    fn lower_expression_occurrence(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        expression: &Expression,
        type_override: Option<CheckedType>,
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
        let value_type = match type_override {
            Some(value_type) => value_type,
            None => match module.type_of_expression(syntax.id).cloned() {
                Some(value_type) => value_type,
                // The checker stops recording a type once control flow
                // diverges (`return`, `break`, `continue`, or a `Never`
                // sub-expression), so an expression with no recorded type is
                // unreachable and its value is `Never`.
                None => CheckedType::Never,
            },
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
    /// family. Every family has a concrete payload; a family/expression
    /// mismatch is a defensive diagnostic.
    fn lower_ordinary_expression(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        family: Stage24Family,
        expression: &Expression,
    ) -> Result<LoweredExpressionKind, Diagnostic> {
        match (family, expression) {
            (Stage24Family::Block, Expression::Block(block)) => Ok(LoweredExpressionKind::Block(
                self.lower_block(module, owner, context, block)?,
            )),
            (Stage24Family::Satisfies, Expression::Satisfies(satisfies)) => {
                let value = self.lower_expression(module, owner, context, &satisfies.value)?;
                Ok(LoweredExpressionKind::Satisfies(LoweredSatisfies { value }))
            }
            (Stage24Family::Match, Expression::Match(match_)) => self
                .lower_match(module, owner, context, match_)
                .map(LoweredExpressionKind::Match),
            (Stage24Family::Loop, Expression::Loop(loop_)) => self
                .lower_loop(module, owner, context, loop_)
                .map(LoweredExpressionKind::Loop),
            (Stage24Family::Product, Expression::Product(product)) => self
                .lower_product(module, owner, context, product)
                .map(LoweredExpressionKind::Product),
            (Stage24Family::RepeatedProduct, Expression::RepeatedProduct(repeated)) => self
                .lower_repeated_product(module, owner, context, repeated)
                .map(LoweredExpressionKind::RepeatedProduct),
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
            (Stage24Family::Index, Expression::Index(index)) => self
                .lower_index(module, owner, context, index)
                .map(LoweredExpressionKind::Index),
            (Stage24Family::Logical, Expression::Logical(logical)) => self
                .lower_logical(module, owner, context, logical)
                .map(LoweredExpressionKind::Logical),
            (Stage24Family::StringTemplate, Expression::StringTemplate(template)) => self
                .lower_string_template(module, owner, context, template)
                .map(LoweredExpressionKind::StringTemplate),
            _ => Err(Diagnostic::new(
                expression.syntax().span.clone(),
                format!(
                    "lowered expression family {} does not match its syntax variant",
                    family_name(family)
                ),
            )),
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

    /// Lowers a product construction into ordered evaluation steps plus a
    /// final layout. Designated fields resolve to checked slots, spreads
    /// expand to explicit source/destination mappings, and contextual defaults
    /// fill the remaining slots in final order. Explicit values override
    /// earlier spread values exactly as the checked plan permits.
    fn lower_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        product: &staple_syntax::ProductExpression,
    ) -> Result<LoweredProduct, Diagnostic> {
        let checked = module.type_of_expression(product.syntax.id).cloned();
        let plan = module.product_default_plan(product.syntax.id).cloned();
        let final_type = match &plan {
            Some(plan) => Some(plan.final_type.clone()),
            None => match &checked {
                Some(CheckedType::Product(product_type)) if !product_type.variadic => {
                    Some(product_type.clone())
                }
                _ => None,
            },
        };
        let Some(final_type) = final_type else {
            // The checker diverged before recording a product shape, so the
            // product is unreachable. Lower every element in source order and
            // record it positionally; later stages never emit this value.
            let mut steps = Vec::new();
            let mut elements = Vec::new();
            for element in &product.elements {
                let expression = self.lower_expression(module, owner, context, &element.value)?;
                let value_type = self
                    .expressions
                    .get(expression)
                    .map(|expression| expression.value_type.clone())
                    .unwrap_or(CheckedType::Never);
                elements.push(crate::CheckedTypeElement {
                    name: element.name.clone(),
                    value_type,
                    default: None,
                });
                steps.push(LoweredProductStep::Positional {
                    expression,
                    slot: elements.len() - 1,
                });
            }
            return Ok(LoweredProduct {
                final_type: CheckedProductType {
                    elements,
                    variadic: false,
                },
                fields: steps
                    .iter()
                    .map(|step| match step {
                        LoweredProductStep::Positional { expression, .. } => *expression,
                        _ => unreachable!("fallback products are positional"),
                    })
                    .collect(),
                steps,
            });
        };

        if product.elements.iter().any(|element| element.designated) {
            self.lower_designated_product(module, owner, context, product, &final_type)
        } else if product.elements.iter().any(|element| element.named_spread) {
            self.lower_named_spread_product(module, owner, context, product, &final_type)
        } else {
            self.lower_positional_product(module, owner, context, product, &final_type)
        }
    }

    fn lower_positional_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
    ) -> Result<LoweredProduct, Diagnostic> {
        let mut steps = Vec::new();
        let mut fields = vec![None; final_type.elements.len()];
        let mut positioned = 0usize;
        for element in &product.elements {
            let expression = self.lower_expression(module, owner, context, &element.value)?;
            if element.spread {
                let mappings =
                    self.expand_positional_spread(element, expression, positioned, final_type)?;
                for mapping in &mappings {
                    fields[mapping.slot] = Some(expression);
                }
                positioned += mappings.len();
                steps.push(LoweredProductStep::PositionalSpread {
                    expression,
                    mappings,
                });
            } else {
                if positioned >= final_type.elements.len() {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "too many positional elements in product",
                    ));
                }
                fields[positioned] = Some(expression);
                steps.push(LoweredProductStep::Positional {
                    expression,
                    slot: positioned,
                });
                positioned += 1;
            }
        }
        self.fill_product_defaults(module, owner, product, final_type, &mut steps, &mut fields)?;
        self.finish_product(product, final_type, steps, fields)
    }

    fn lower_designated_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
    ) -> Result<LoweredProduct, Diagnostic> {
        let mut steps = Vec::new();
        let mut fields = vec![None; final_type.elements.len()];
        let mut positioned = 0usize;
        for element in &product.elements {
            let expression = self.lower_expression(module, owner, context, &element.value)?;
            if element.designated {
                let name = element
                    .name
                    .clone()
                    .expect("designators always have a name");
                let Some(slot) = final_type
                    .elements
                    .iter()
                    .position(|field| field.name.as_deref() == Some(name.as_str()))
                else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        format!("unknown designated product field `{name}`"),
                    ));
                };
                fields[slot] = Some(expression);
                steps.push(LoweredProductStep::Designated {
                    name,
                    expression,
                    slot,
                });
                continue;
            }
            if element.spread {
                let mappings =
                    self.expand_positional_spread(element, expression, positioned, final_type)?;
                for mapping in &mappings {
                    fields[mapping.slot] = Some(expression);
                }
                positioned += mappings.len();
                steps.push(LoweredProductStep::PositionalSpread {
                    expression,
                    mappings,
                });
                continue;
            }
            if positioned >= final_type.elements.len() {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    "too many positional elements in designated product initializer",
                ));
            }
            fields[positioned] = Some(expression);
            steps.push(LoweredProductStep::Positional {
                expression,
                slot: positioned,
            });
            positioned += 1;
        }
        self.fill_product_defaults(module, owner, product, final_type, &mut steps, &mut fields)?;
        self.finish_product(product, final_type, steps, fields)
    }

    fn lower_named_spread_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
    ) -> Result<LoweredProduct, Diagnostic> {
        let mut steps = Vec::new();
        let mut fields = vec![None; final_type.elements.len()];
        for element in &product.elements {
            let expression = self.lower_expression(module, owner, context, &element.value)?;
            if element.spread {
                let operand = self
                    .expressions
                    .get(expression)
                    .map(|expression| expression.value_type.clone())
                    .unwrap_or(CheckedType::Never);
                let CheckedType::Product(operand_type) = operand else {
                    return Err(Diagnostic::new(
                        element.syntax.span.clone(),
                        "product spread operand does not have a fixed product type",
                    ));
                };
                let mut mappings = Vec::new();
                for (source, field) in operand_type.elements.iter().enumerate() {
                    let Some(name) = field.name.clone() else {
                        return Err(Diagnostic::new(
                            element.syntax.span.clone(),
                            "a named spread operand must have every element named",
                        ));
                    };
                    let Some(slot) = final_type
                        .elements
                        .iter()
                        .position(|final_field| final_field.name.as_deref() == Some(name.as_str()))
                    else {
                        return Err(Diagnostic::new(
                            element.syntax.span.clone(),
                            format!("unknown field `{name}` in named product spread"),
                        ));
                    };
                    fields[slot] = Some(expression);
                    mappings.push(LoweredNamedSpreadMapping { name, source, slot });
                }
                steps.push(LoweredProductStep::NamedSpread {
                    expression,
                    mappings,
                });
                continue;
            }
            let Some(name) = element.name.clone() else {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    "every element must be named when the product contains a named spread",
                ));
            };
            let Some(slot) = final_type
                .elements
                .iter()
                .position(|field| field.name.as_deref() == Some(name.as_str()))
            else {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    format!("unknown field `{name}` in named product spread"),
                ));
            };
            fields[slot] = Some(expression);
            steps.push(LoweredProductStep::Designated {
                name,
                expression,
                slot,
            });
        }
        // Named spreads are checked complete: every final field must be
        // provided, and defaults are not part of this construction path.
        for (slot, field) in fields.iter().enumerate() {
            if field.is_none() {
                return Err(missing_product_slot_error(
                    final_type,
                    slot,
                    &product.syntax.span,
                ));
            }
        }
        self.finish_product(product, final_type, steps, fields)
    }

    /// Expands a positional spread operand into explicit source-index to
    /// destination-slot mappings, rejecting operands the checker should have
    /// rejected and destinations outside the final shape.
    fn expand_positional_spread(
        &self,
        element: &staple_syntax::ProductElement,
        expression: ExpressionId,
        positioned: usize,
        final_type: &CheckedProductType,
    ) -> Result<Vec<LoweredSpreadMapping>, Diagnostic> {
        let operand = self
            .expressions
            .get(expression)
            .map(|expression| expression.value_type.clone())
            .unwrap_or(CheckedType::Never);
        let CheckedType::Product(operand_type) = operand else {
            return Err(Diagnostic::new(
                element.syntax.span.clone(),
                "product spread operand does not have a fixed product type",
            ));
        };
        if operand_type.variadic {
            return Err(Diagnostic::new(
                element.syntax.span.clone(),
                "cannot spread a variadic product",
            ));
        }
        let mut mappings = Vec::new();
        for source in 0..operand_type.elements.len() {
            let Some(_) = final_type.elements.get(positioned + source) else {
                return Err(Diagnostic::new(
                    element.syntax.span.clone(),
                    "too many positional elements in product",
                ));
            };
            mappings.push(LoweredSpreadMapping {
                source,
                slot: positioned + source,
            });
        }
        Ok(mappings)
    }

    /// Evaluates missing contextual defaults in final slot order using
    /// occurrence-aware keys and the plan's contextual element type.
    fn fill_product_defaults(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
        steps: &mut Vec<LoweredProductStep>,
        fields: &mut [Option<ExpressionId>],
    ) -> Result<(), Diagnostic> {
        let plan = module.product_default_plan(product.syntax.id).cloned();
        for slot in 0..final_type.elements.len() {
            if fields[slot].is_some() {
                continue;
            }
            let default = plan
                .as_ref()
                .and_then(|plan| plan.defaults.get(slot))
                .and_then(|default| default.as_ref());
            let Some(default) = default else {
                return Err(missing_product_slot_error(
                    final_type,
                    slot,
                    &product.syntax.span,
                ));
            };
            let expected = final_type.elements[slot].value_type.clone();
            let default_context = ExpressionContext::ContextualDefault {
                consumer: product.syntax.id,
                slot,
            };
            let expression = self.lower_expression_occurrence(
                module,
                owner,
                default_context,
                default,
                Some(expected.clone()),
            )?;
            fields[slot] = Some(expression);
            steps.push(LoweredProductStep::Default {
                slot,
                expression,
                expected,
            });
        }
        Ok(())
    }

    fn finish_product(
        &self,
        product: &staple_syntax::ProductExpression,
        final_type: &CheckedProductType,
        steps: Vec<LoweredProductStep>,
        fields: Vec<Option<ExpressionId>>,
    ) -> Result<LoweredProduct, Diagnostic> {
        let mut final_fields = Vec::with_capacity(fields.len());
        for (slot, field) in fields.into_iter().enumerate() {
            match field {
                Some(field) => final_fields.push(field),
                None => {
                    return Err(missing_product_slot_error(
                        final_type,
                        slot,
                        &product.syntax.span,
                    ));
                }
            }
        }
        Ok(LoweredProduct {
            final_type: final_type.clone(),
            steps,
            fields: final_fields,
        })
    }

    /// Lowers `&&`/`||` operands left-to-right and copies the checked `Bool`
    /// selection, resolving the `True` alternative index the backend needs.
    fn lower_logical(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        logical: &staple_syntax::LogicalExpression,
    ) -> Result<LoweredLogical, Diagnostic> {
        let left = self.lower_expression(module, owner, context, &logical.left)?;
        let right = self.lower_expression(module, owner, context, &logical.right)?;
        let checked = module.logical_for(logical.syntax.id).cloned();
        // Missing metadata means checking diverged before the logical was
        // recorded, so the expression is unreachable and never emitted.
        let bool_type = match checked {
            Some(checked) => checked.bool_type,
            None => module
                .type_of_expression(logical.syntax.id)
                .cloned()
                .unwrap_or(CheckedType::Never),
        };
        let true_index = match &bool_type {
            CheckedType::Sum(sum) => sum
                .alternatives
                .iter()
                .position(|alternative| {
                    matches!(alternative, CheckedType::Distinct { name, .. } if name == "True")
                })
                .ok_or_else(|| {
                    Diagnostic::new(
                        logical.syntax.span.clone(),
                        "`Bool` has no `True` alternative",
                    )
                })?,
            CheckedType::Never => 0,
            _ => {
                return Err(Diagnostic::new(
                    logical.syntax.span.clone(),
                    "`&&`/`||` require `Bool` to be a sum type",
                ));
            }
        };
        Ok(LoweredLogical {
            operator: logical.operator,
            left,
            right,
            bool_type,
            true_index,
        })
    }

    /// Lowers a string template: parts keep their order and decoded literal
    /// text, interpolations lower left-to-right, and each interpolation keeps
    /// its checked value type plus the selected formatting trait and method.
    /// Missing formatter helpers or trait selections diagnose here instead of
    /// being rediscovered by name during emission.
    fn lower_string_template(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        template: &staple_syntax::StringTemplateExpression,
    ) -> Result<LoweredStringTemplate, Diagnostic> {
        let formatting = module.string_formatting();
        if formatting.formatter_new.is_none()
            || formatting.formatter_write.is_none()
            || formatting.formatter_finish.is_none()
        {
            return Err(Diagnostic::new(
                template.syntax.span.clone(),
                "string formatting helpers are unavailable",
            ));
        }
        let mut parts = Vec::with_capacity(template.parts.len());
        for part in &template.parts {
            match part {
                staple_syntax::StringTemplatePart::Literal(literal) => {
                    parts.push(LoweredStringTemplatePart::Literal(literal.clone()));
                }
                staple_syntax::StringTemplatePart::Interpolation(interpolation) => {
                    let expression =
                        self.lower_expression(module, owner, context, &interpolation.expression)?;
                    let value_type = self
                        .expressions
                        .get(expression)
                        .map(|expression| expression.value_type.clone())
                        .unwrap_or(CheckedType::Never);
                    let checked = formatting
                        .interpolations
                        .get(&interpolation.expression.syntax().id)
                        .cloned();
                    let (trait_id, method, value_type) = match checked {
                        Some(checked) => (checked.trait_id, checked.method, checked.value_type),
                        None => {
                            // Checking diverged before recording this
                            // interpolation; the template is unreachable. Use
                            // the checker-selected formatting trait, never a
                            // name lookup.
                            let trait_id = match interpolation.format {
                                staple_syntax::StringInterpolationFormat::Display => {
                                    module.semantic_ids().display_trait
                                }
                                staple_syntax::StringInterpolationFormat::Debug => {
                                    module.semantic_ids().debug_trait
                                }
                            }
                            .ok_or_else(|| {
                                Diagnostic::new(
                                    interpolation.expression.syntax().span.clone(),
                                    "standard formatting trait is unavailable",
                                )
                            })?;
                            let method = self
                                .traits
                                .get(trait_id)
                                .and_then(|trait_| trait_.methods.first().copied())
                                .ok_or_else(|| {
                                    Diagnostic::new(
                                        interpolation.expression.syntax().span.clone(),
                                        "formatting trait has no method",
                                    )
                                })?;
                            (trait_id, method, value_type)
                        }
                    };
                    parts.push(LoweredStringTemplatePart::Interpolation(
                        LoweredInterpolation {
                            expression,
                            format: interpolation.format,
                            value_type,
                            trait_id,
                            method,
                        },
                    ));
                }
            }
        }
        Ok(LoweredStringTemplate { parts })
    }

    /// Lowers `base[index]` in evaluation order and copies the checked `Index`
    /// dispatch recipe: owning trait, completed argument types, instantiated
    /// method type (mutation/move masks, effects, resources), and the
    /// temporary-cleanup facts for mutation operands that are not places.
    fn lower_index(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        index: &staple_syntax::IndexExpression,
    ) -> Result<LoweredIndex, Diagnostic> {
        let base = self.lower_expression(module, owner, context, &index.value)?;
        let position = self.lower_expression(module, owner, context, &index.index)?;
        let dispatch = module
            .trait_dispatch_for(index.syntax.id)
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(index.syntax.span.clone(), "missing `Index` dispatch")
            })?;
        let Some(trait_id) = module.resolved().trait_for_method(dispatch.method) else {
            return Err(Diagnostic::new(
                index.syntax.span.clone(),
                "`Index` dispatch method has no owning trait",
            ));
        };
        let arguments = module
            .complete_trait_arguments(trait_id, &dispatch.arguments)
            .ok_or_else(|| {
                Diagnostic::new(index.syntax.span.clone(), "incomplete `Index` dispatch")
            })?;
        let method_type =
            module.instantiated_trait_method_type(trait_id, &arguments, dispatch.method);
        let base_place = expression_has_place_root(module.resolved(), &index.value);
        let index_place = expression_has_place_root(module.resolved(), &index.index);
        let mut whole_temporary = false;
        let mut base_temporary = false;
        let mut index_temporary = false;
        if let Some(method_type) = &method_type {
            for target in method_type.mutations.iter().chain(&method_type.moves) {
                match target {
                    CheckedMutation::Whole => whole_temporary = true,
                    CheckedMutation::Element(0) => base_temporary |= !base_place,
                    CheckedMutation::Element(1) => index_temporary |= !index_place,
                    CheckedMutation::Element(_) => {}
                }
            }
        }
        Ok(LoweredIndex {
            base,
            index: position,
            dispatch,
            trait_id,
            arguments,
            method_type,
            whole_temporary,
            base_temporary,
            index_temporary,
        })
    }

    /// Lowers a loop body into the existing block/item arenas, tracks loop
    /// nesting for break/continue validation, and records the body-result drop
    /// requirement and fall-through fact.
    fn lower_loop(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        loop_: &staple_syntax::LoopExpression,
    ) -> Result<LoweredLoop, Diagnostic> {
        let body_type = module.type_of_expression(loop_.body.syntax.id).cloned();
        let result_type = module
            .type_of_expression(loop_.syntax.id)
            .cloned()
            .unwrap_or(CheckedType::Never);
        self.loop_depth += 1;
        let body = self.lower_block(module, owner, context, &loop_.body);
        self.loop_depth -= 1;
        let body = body?;
        let drops_body_result = body_type
            .as_ref()
            .is_some_and(|body_type| module.type_needs_drop(body_type));
        let body_falls_through = self.block_falls_through(body);
        Ok(LoweredLoop {
            body,
            result_type,
            drops_body_result,
            body_falls_through,
            depth: self.loop_depth + 1,
        })
    }

    /// Lowers a match subject first, then arms in source order, reusing the
    /// Stage 2.3 pattern lowering and copying the checked subject type.
    fn lower_match(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        match_: &staple_syntax::MatchExpression,
    ) -> Result<LoweredMatch, Diagnostic> {
        let subject = self.lower_expression(module, owner, context, &match_.subject)?;
        let Some(checked) = module.match_for(match_.syntax.id).cloned() else {
            // The checker diverged before recording the match (for example
            // when the subject returns), so the whole match is unreachable and
            // code generation never emits its arms.
            let source = self
                .expressions
                .get(subject)
                .map(|subject| subject.value_type.clone())
                .unwrap_or(CheckedType::Never);
            return Ok(LoweredMatch {
                subject,
                source,
                arms: Vec::new(),
            });
        };
        let mut arms = Vec::with_capacity(match_.arms.len());
        for arm in &match_.arms {
            let pattern = self.lower_pattern(module, &arm.pattern)?;
            let body = self.lower_expression(module, owner, context, &arm.body)?;
            arms.push(LoweredMatchArm {
                origin: Origin {
                    syntax: arm.syntax.id,
                    span: arm.syntax.span.clone(),
                },
                pattern,
                body,
                bound_symbols: pattern_symbols(module.resolved(), &arm.pattern),
            });
        }
        Ok(LoweredMatch {
            subject,
            source: checked.source,
            arms,
        })
    }

    /// Lowers `(value; count)`: the checked result type gives the repetition
    /// count and whether the single-element representation collapses.
    fn lower_repeated_product(
        &mut self,
        module: &TypedModule,
        owner: ExpressionOwner,
        context: ExpressionContext,
        repeated: &staple_syntax::RepeatedProductExpression,
    ) -> Result<LoweredRepeatedProduct, Diagnostic> {
        let expression = self.lower_expression(module, owner, context, &repeated.value)?;
        let count = match module.type_of_expression(repeated.syntax.id) {
            Some(CheckedType::Product(product)) if !product.variadic => {
                LoweredRepeatCount::Fixed(product.elements.len())
            }
            Some(CheckedType::Array { count, .. }) => {
                LoweredRepeatCount::Symbolic(count.as_ref().clone())
            }
            _ => LoweredRepeatCount::Fixed(1),
        };
        let collapsed = count == LoweredRepeatCount::Fixed(1);
        Ok(LoweredRepeatedProduct {
            expression,
            count,
            collapsed,
        })
    }

    /// Whether control can reach the end of a lowered block: a `return`,
    /// `break`, `continue`, or unconditionally diverging (`Never`) expression
    /// stops the sequence, matching the backend's `did_return` handling.
    fn block_falls_through(&self, block: BlockId) -> bool {
        let Some(block) = self.blocks.get(block) else {
            return false;
        };
        for item in &block.items {
            let Some(item) = self.items.get(*item) else {
                return false;
            };
            match &item.kind {
                LoweredItemKind::Return(_)
                | LoweredItemKind::Break(_)
                | LoweredItemKind::Continue(_) => return false,
                LoweredItemKind::Binding(binding) => {
                    if binding
                        .value
                        .is_some_and(|value| self.expression_diverges(value))
                    {
                        return false;
                    }
                }
                LoweredItemKind::PatternBinding(binding) => {
                    if self.expression_diverges(binding.value) {
                        return false;
                    }
                }
                LoweredItemKind::Assignment(assignment) => {
                    if self.expression_diverges(assignment.value) {
                        return false;
                    }
                }
                LoweredItemKind::Expression(statement) => {
                    if self.expression_diverges(statement.expression) {
                        return false;
                    }
                }
            }
        }
        block
            .result
            .is_none_or(|result| !self.expression_diverges(result))
    }

    fn expression_diverges(&self, expression: ExpressionId) -> bool {
        self.expressions
            .get(expression)
            .is_some_and(|expression| expression.value_type == CheckedType::Never)
    }

    fn expression_needs_drop(&self, module: &TypedModule, expression: ExpressionId) -> bool {
        self.expressions
            .get(expression)
            .is_some_and(|expression| module.type_needs_drop(&expression.value_type))
    }

    /// Classifies one checked call into its single explicit route, mirroring
    /// the backend's decision order: juxtaposed chain, curried defaults, trait
    /// dispatch, intrinsic, generic direct, external, then the indirect
    /// closure fallback. There is no unknown-callable outcome.
    fn classify_call_route(
        &self,
        module: &TypedModule,
        owner: ExpressionOwner,
        call: &staple_syntax::CallExpression,
    ) -> Result<CallRoute, Diagnostic> {
        let resolved = module.resolved();
        if resolved.primitive_macro_for(call.syntax.id).is_some() {
            return Ok(CallRoute::PrimitiveMacro);
        }
        if let Some(symbol) = module.symbol_for(call.callee.syntax().id)
            && resolved.constructor_type(symbol).is_some()
        {
            return Ok(CallRoute::Constructor);
        }
        if let Some(plan) = module.juxtaposed_call_plan(call.syntax.id) {
            let expected = match plan.function.parameter.as_ref() {
                CheckedType::Product(product) => product.elements.len(),
                _ => 0,
            };
            if plan.arguments.len() == expected {
                let mut callee = call.callee.as_ref();
                for _ in 1..plan.consumed_calls {
                    let Expression::Call(previous) = callee else {
                        break;
                    };
                    callee = previous.callee.as_ref();
                }
                let intrinsic = module
                    .symbol_for(callee.syntax().id)
                    .and_then(|symbol| resolved.intrinsic_function(symbol));
                return Ok(match intrinsic {
                    Some(_) => CallRoute::JuxtaposedIntrinsic,
                    None => CallRoute::Juxtaposed,
                });
            }
        }
        if module.curried_default_plan(call.syntax.id).is_some() {
            return Ok(CallRoute::CurriedDefault);
        }
        if let Some(dispatch) = module.trait_dispatch_for(call.callee.syntax().id) {
            return self.classify_trait_call_route(module, dispatch);
        }
        if let Some(symbol) = module.symbol_for(call.callee.syntax().id) {
            if resolved.intrinsic_function(symbol).is_some() {
                return Ok(CallRoute::Intrinsic);
            }
            if !self.call_callee_is_local(owner, symbol)
                && let Some(function) = module.function_for_symbol(symbol)
                && self
                    .functions
                    .get(function)
                    .is_some_and(|function| function.captures.is_empty())
                && module
                    .type_of_function(function)
                    .is_some_and(|function_type| {
                        contains_type_parameter(&CheckedType::Function(function_type.clone()))
                    })
            {
                return Ok(CallRoute::GenericDirect);
            }
            if !self.call_callee_is_local(owner, symbol) && resolved.is_external_symbol(symbol) {
                return Ok(CallRoute::External);
            }
        }
        Ok(CallRoute::Indirect)
    }

    /// Resolves the trait-dispatch route into explicit evidence: a selected
    /// implementation, a structural method, or a declared bound whose
    /// selection waits for Stage 3 substitution.
    fn classify_trait_call_route(
        &self,
        module: &TypedModule,
        dispatch: &CheckedTraitDispatch,
    ) -> Result<CallRoute, Diagnostic> {
        let Some(trait_id) = module.resolved().trait_for_method(dispatch.method) else {
            return Err(Diagnostic::new(
                Span::Compiler,
                format!("trait method {} has no owning trait", dispatch.method.0),
            ));
        };
        let Some(arguments) = module.complete_trait_arguments(trait_id, &dispatch.arguments) else {
            return Ok(CallRoute::DeclaredTraitBound);
        };
        if module
            .trait_impl_method(trait_id, &arguments, dispatch.method)
            .is_some()
        {
            return Ok(CallRoute::TraitImplementation);
        }
        if module
            .structural_trait_method(trait_id, &arguments)
            .is_some()
        {
            return Ok(CallRoute::StructuralTraitMethod);
        }
        // A generic argument's implementation may only be selectable after
        // Stage 3 has canonical substitutions.
        Ok(CallRoute::DeclaredTraitBound)
    }

    /// Whether a callee symbol is local at this call site: owned by the
    /// enclosing function or captured by it. Local bindings stay indirect.
    fn call_callee_is_local(&self, owner: ExpressionOwner, symbol: SymbolId) -> bool {
        let ExpressionOwner::Function(function) = owner else {
            return false;
        };
        let owned = self
            .symbols
            .get(symbol)
            .is_some_and(|symbol| symbol.owner == Some(function));
        let captured = self.functions.get(function).is_some_and(|function| {
            function
                .captures
                .iter()
                .any(|capture| capture.symbol == symbol)
        });
        owned || captured
    }

    fn validate(&self) -> Vec<Diagnostic> {
        let mut diagnostics = self.modules.validate("module");
        diagnostics.extend(self.functions.validate("function"));
        diagnostics.extend(self.symbols.validate("symbol"));
        diagnostics.extend(self.types.validate("type"));
        diagnostics.extend(self.traits.validate("trait"));
        diagnostics.extend(self.trait_methods.validate("trait method"));
        diagnostics.extend(self.validate_occurrence_lookups());
        diagnostics.extend(self.validate_arena_references());
        diagnostics.extend(self.validate_ownership());
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
        diagnostics.extend(self.validate_string_formatting());
        diagnostics.extend(self.validate_calls_and_callable_values());
        diagnostics
    }

    /// Checks every call and callable value against the semantic catalogs and
    /// the arena children they own: targets, callee/argument occurrences,
    /// ABI slots, ordered steps, closure captures, and trait evidence.
    fn validate_calls_and_callable_values(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, call) in self.calls.iter() {
            self.validate_callable_target(&call.origin, &call.target, &mut diagnostics);
            if let Some(callee) = call.callee
                && !self.expressions.contains(callee)
            {
                diagnostics.push(invalid_reference(
                    &call.origin,
                    "call",
                    "expression",
                    callee.index(),
                ));
            }
            let mut slots = HashSet::new();
            for argument in &call.arguments {
                if !self.expressions.contains(argument.expression) {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call argument",
                        "expression",
                        argument.expression.index(),
                    ));
                }
                if let Some(place) = argument.place
                    && !self.places.contains(place)
                {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call argument",
                        "place",
                        place.index(),
                    ));
                }
                if let Some(slot) = argument.slot
                    && !slots.insert(slot)
                {
                    diagnostics.push(Diagnostic::new(
                        call.origin.span.clone(),
                        format!("call argument slot {slot} is filled more than once"),
                    ));
                }
            }
            for step in &call.steps {
                let expression = match step {
                    LoweredCallStep::Callee { expression } => Some(*expression),
                    LoweredCallStep::Argument { argument } => {
                        self.check_call_step_argument(
                            &call.origin,
                            *argument,
                            call.arguments.len(),
                            &mut diagnostics,
                        );
                        None
                    }
                    LoweredCallStep::ProductElement {
                        argument,
                        expression,
                        ..
                    }
                    | LoweredCallStep::ProductSpread {
                        argument,
                        expression,
                        ..
                    }
                    | LoweredCallStep::NamedProductSpread {
                        argument,
                        expression,
                        ..
                    }
                    | LoweredCallStep::Default {
                        argument,
                        expression,
                        ..
                    } => {
                        self.check_call_step_argument(
                            &call.origin,
                            *argument,
                            call.arguments.len(),
                            &mut diagnostics,
                        );
                        Some(*expression)
                    }
                    LoweredCallStep::Resource { resource } => {
                        if *resource >= call.resources.len() {
                            diagnostics.push(Diagnostic::new(
                                call.origin.span.clone(),
                                format!(
                                    "call step targets out-of-range resource {resource} of {}",
                                    call.resources.len()
                                ),
                            ));
                        }
                        None
                    }
                    LoweredCallStep::Invoke => None,
                };
                if let Some(expression) = expression
                    && !self.expressions.contains(expression)
                {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call step",
                        "expression",
                        expression.index(),
                    ));
                }
            }
            for symbol in &call.initialization_checks {
                if self.symbols.get(*symbol).is_none() {
                    diagnostics.push(invalid_reference(
                        &call.origin,
                        "call initialization check",
                        "symbol",
                        symbol.0,
                    ));
                }
            }
            if let Some(evidence) = &call.evidence {
                self.validate_trait_evidence(&call.origin, evidence, &mut diagnostics);
            }
        }
        for (_, value) in self.callable_values.iter() {
            self.validate_callable_target(&value.origin, &value.target, &mut diagnostics);
            if let Some(closure) = &value.closure {
                match self.functions.get(closure.function) {
                    Some(function) => {
                        if function.captures.len() != closure.captures.len() {
                            diagnostics.push(Diagnostic::new(
                                value.origin.span.clone(),
                                format!(
                                    "closure construction has {} captures for a function with {}",
                                    closure.captures.len(),
                                    function.captures.len()
                                ),
                            ));
                        }
                        for (catalog, capture) in function.captures.iter().zip(&closure.captures) {
                            if catalog.symbol != capture.capture.symbol {
                                diagnostics.push(Diagnostic::new(
                                    value.origin.span.clone(),
                                    format!(
                                        "closure capture {} disagrees with catalog capture {}",
                                        capture.capture.symbol.0, catalog.symbol.0
                                    ),
                                ));
                            }
                            if self.symbols.get(capture.capture.symbol).is_none() {
                                diagnostics.push(invalid_reference(
                                    &value.origin,
                                    "closure capture",
                                    "symbol",
                                    capture.capture.symbol.0,
                                ));
                            }
                        }
                    }
                    None => diagnostics.push(invalid_reference(
                        &value.origin,
                        "closure construction",
                        "function",
                        closure.function.0,
                    )),
                }
            }
            if let Some(evidence) = &value.evidence {
                self.validate_trait_evidence(&value.origin, evidence, &mut diagnostics);
            }
        }
        diagnostics
    }

    fn check_call_step_argument(
        &self,
        origin: &Origin,
        argument: usize,
        arguments: usize,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        if argument >= arguments {
            diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                format!("call step targets out-of-range argument {argument} of {arguments}"),
            ));
        }
    }

    fn validate_callable_target(
        &self,
        origin: &Origin,
        target: &LoweredCallableTarget,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match target {
            LoweredCallableTarget::DirectFunction { function, .. }
            | LoweredCallableTarget::CompilerHelper { function } => {
                if self.functions.get(*function).is_none() {
                    diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "function",
                        function.0,
                    ));
                }
            }
            LoweredCallableTarget::IndirectClosure { callee } => {
                if !self.expressions.contains(*callee) {
                    diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "expression",
                        callee.index(),
                    ));
                }
            }
            LoweredCallableTarget::ExternalFunction { symbol } => match self.symbols.get(*symbol) {
                Some(symbol) if symbol.external => {}
                Some(_) => diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    "external callable target is not an external symbol",
                )),
                None => diagnostics.push(invalid_reference(
                    origin,
                    "callable target",
                    "symbol",
                    symbol.0,
                )),
            },
            LoweredCallableTarget::Intrinsic { symbol, intrinsic } => {
                match self.symbols.get(*symbol) {
                    Some(symbol) if symbol.intrinsic == Some(*intrinsic) => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "intrinsic callable target disagrees with its symbol",
                    )),
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "symbol",
                        symbol.0,
                    )),
                }
            }
            LoweredCallableTarget::Constructor {
                symbol,
                type_id,
                recursive,
            } => {
                match self.symbols.get(*symbol) {
                    Some(symbol) if symbol.constructor == Some(*type_id) => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "constructor callable target disagrees with its symbol",
                    )),
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "symbol",
                        symbol.0,
                    )),
                }
                match self.types.get(*type_id) {
                    Some(metadata) if metadata.recursive_construction == *recursive => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "constructor callable target recursive classification disagrees with its type",
                    )),
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "type",
                        type_id.0,
                    )),
                }
            }
            LoweredCallableTarget::TraitImplementation {
                trait_id,
                method,
                function,
            } => {
                self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
                if let Some(function) = function
                    && self.functions.get(*function).is_none()
                {
                    diagnostics.push(invalid_reference(
                        origin,
                        "callable target",
                        "function",
                        function.0,
                    ));
                }
            }
            LoweredCallableTarget::StructuralTraitMethod {
                trait_id, method, ..
            } => {
                self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
            }
        }
    }

    fn validate_trait_method_reference(
        &self,
        origin: &Origin,
        trait_id: TraitId,
        method: TraitMethodId,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        if self.traits.get(trait_id).is_none() {
            diagnostics.push(invalid_reference(
                origin,
                "trait evidence",
                "trait",
                trait_id.0,
            ));
        }
        match self.trait_methods.get(method) {
            Some(metadata) if metadata.trait_id == trait_id => {}
            Some(_) => diagnostics.push(Diagnostic::new(
                origin.span.clone(),
                "trait evidence method does not belong to its recorded trait",
            )),
            None => diagnostics.push(invalid_reference(
                origin,
                "trait evidence",
                "trait method",
                method.0,
            )),
        }
    }

    fn validate_trait_evidence(
        &self,
        origin: &Origin,
        evidence: &TraitEvidence,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        match evidence {
            TraitEvidence::ExplicitImplementation {
                trait_id,
                implementation,
                method,
                function,
                ..
            } => {
                self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
                if self.functions.get(*function).is_none() {
                    diagnostics.push(invalid_reference(
                        origin,
                        "trait evidence",
                        "function",
                        function.0,
                    ));
                }
                match self.trait_implementations.get(*implementation) {
                    Some(metadata)
                        if metadata.trait_id == *trait_id
                            && metadata.methods.iter().any(|(candidate, selected)| {
                                candidate == method && selected == function
                            }) => {}
                    Some(_) => diagnostics.push(Diagnostic::new(
                        origin.span.clone(),
                        "trait evidence implementation does not provide its selected method",
                    )),
                    None => diagnostics.push(invalid_reference(
                        origin,
                        "trait evidence",
                        "trait implementation",
                        implementation.index(),
                    )),
                }
            }
            TraitEvidence::Structural {
                trait_id, method, ..
            } => {
                self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
            }
            TraitEvidence::DeclaredBound {
                trait_id, method, ..
            } => {
                if self.traits.get(*trait_id).is_none() {
                    diagnostics.push(invalid_reference(
                        origin,
                        "trait evidence",
                        "trait",
                        trait_id.0,
                    ));
                }
                if let Some(method) = method {
                    self.validate_trait_method_reference(origin, *trait_id, *method, diagnostics);
                }
            }
            TraitEvidence::RejectedImplementation {
                trait_id,
                implementation,
                ..
            } => match self.trait_implementations.get(*implementation) {
                Some(metadata) if metadata.trait_id == *trait_id && metadata.negative => {}
                Some(_) => diagnostics.push(Diagnostic::new(
                    origin.span.clone(),
                    "rejected trait evidence is not a negative implementation of its trait",
                )),
                None => diagnostics.push(invalid_reference(
                    origin,
                    "trait evidence",
                    "trait implementation",
                    implementation.index(),
                )),
            },
        }
    }

    /// Checks that checked formatter helper selections resolve to the
    /// function catalog. Absent helpers are valid for `no_prelude` programs
    /// that never construct a template.
    fn validate_string_formatting(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (role, function) in [
            ("formatter constructor", self.string_formatting.constructor),
            ("formatter write", self.string_formatting.write),
            ("formatter finish", self.string_formatting.finish),
        ] {
            let Some(function) = function else {
                continue;
            };
            if self.functions.get(function).is_none() {
                diagnostics.push(Diagnostic::new(
                    Span::Compiler,
                    format!(
                        "lowered {role} function reference {} is missing from the function catalog",
                        function.0
                    ),
                ));
            }
        }
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
                LoweredExpressionKind::Deferred(_) | LoweredExpressionKind::String(_) => {}
                LoweredExpressionKind::Integer(integer) => {
                    let width = integer_literal_bit_width(integer.integer_type);
                    let value_bits = if integer.integer_type.is_signed() {
                        width - 1
                    } else {
                        width
                    };
                    if value_bits < 64 && integer.value > ((1_u64 << value_bits) - 1) {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            format!(
                                "lowered integer payload {} does not fit in `{}`",
                                integer.value,
                                integer.integer_type.name()
                            ),
                        ));
                    }
                }
                LoweredExpressionKind::Float(float) => {
                    if !float.value.is_finite() {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "lowered float payload is not finite",
                        ));
                    }
                    if float.float_type == FloatType::F32
                        && f64::from(float.value as f32) != float.value
                    {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "lowered F32 payload is not exactly representable",
                        ));
                    }
                }
                LoweredExpressionKind::CString(c_string) => {
                    let trailing_nul = c_string.bytes.last() == Some(&0);
                    if !trailing_nul {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "lowered C string payload has no trailing NUL",
                        ));
                    }
                    if c_string.bytes[..c_string.bytes.len().saturating_sub(1)].contains(&0) {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "lowered C string payload contains an interior NUL",
                        ));
                    }
                }
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
                LoweredExpressionKind::Product(product) => {
                    for step in &product.steps {
                        let child = match step {
                            LoweredProductStep::Positional { expression, .. }
                            | LoweredProductStep::Designated { expression, .. }
                            | LoweredProductStep::PositionalSpread { expression, .. }
                            | LoweredProductStep::NamedSpread { expression, .. }
                            | LoweredProductStep::Default { expression, .. } => *expression,
                        };
                        if !self.expressions.contains(child) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "product step",
                                "expression",
                                child.index(),
                            ));
                        }
                    }
                    for field in &product.fields {
                        if !self.expressions.contains(*field) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "product field",
                                "expression",
                                field.index(),
                            ));
                        }
                    }
                    self.validate_product_plan(expression, product, &mut diagnostics);
                }
                LoweredExpressionKind::RepeatedProduct(repeated) => {
                    if !self.expressions.contains(repeated.expression) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "repeated product",
                            "expression",
                            repeated.expression.index(),
                        ));
                    }
                    if repeated.collapsed != (repeated.count == LoweredRepeatCount::Fixed(1)) {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "repeated product collapse marker disagrees with its count",
                        ));
                    }
                    match (&repeated.count, &expression.value_type) {
                        (LoweredRepeatCount::Fixed(count), CheckedType::Product(product_type))
                            if !product_type.variadic && product_type.elements.len() != *count =>
                        {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                format!(
                                    "repeated product count {count} disagrees with its {} element result type",
                                    product_type.elements.len()
                                ),
                            ));
                        }
                        (LoweredRepeatCount::Fixed(_), CheckedType::Array { .. }) => {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                "repeated product fixed count disagrees with its symbolic array result type",
                            ));
                        }
                        (
                            LoweredRepeatCount::Symbolic(count),
                            CheckedType::Array { count: checked, .. },
                        ) if count != checked.as_ref() => {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                "repeated product symbolic count disagrees with its result type",
                            ));
                        }
                        (LoweredRepeatCount::Symbolic(_), ty)
                            if !matches!(ty, CheckedType::Array { .. }) =>
                        {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                "repeated product symbolic count requires an array result type",
                            ));
                        }
                        _ => {}
                    }
                }
                LoweredExpressionKind::Satisfies(satisfies) => {
                    if !self.expressions.contains(satisfies.value) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "satisfies",
                            "expression",
                            satisfies.value.index(),
                        ));
                    }
                }
                LoweredExpressionKind::Logical(logical) => {
                    for child in [logical.left, logical.right] {
                        if !self.expressions.contains(child) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "logical",
                                "expression",
                                child.index(),
                            ));
                        }
                    }
                    if let CheckedType::Sum(sum) = &logical.bool_type
                        && logical.true_index >= sum.alternatives.len()
                    {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            format!(
                                "logical true alternative index {} is out of range for {} alternatives",
                                logical.true_index,
                                sum.alternatives.len()
                            ),
                        ));
                    }
                }
                LoweredExpressionKind::StringTemplate(template) => {
                    for part in &template.parts {
                        let LoweredStringTemplatePart::Interpolation(interpolation) = part else {
                            continue;
                        };
                        if !self.expressions.contains(interpolation.expression) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "string template",
                                "expression",
                                interpolation.expression.index(),
                            ));
                        }
                        if self.traits.get(interpolation.trait_id).is_none() {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "string template",
                                "trait",
                                interpolation.trait_id.0,
                            ));
                        }
                        match self.trait_methods.get(interpolation.method) {
                            Some(method) if method.trait_id == interpolation.trait_id => {}
                            Some(_) => diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                "interpolation formatting method does not belong to its trait",
                            )),
                            None => diagnostics.push(invalid_reference(
                                &expression.origin,
                                "string template",
                                "trait method",
                                interpolation.method.0,
                            )),
                        }
                    }
                }
                LoweredExpressionKind::Index(index) => {
                    for child in [index.base, index.index] {
                        if !self.expressions.contains(child) {
                            diagnostics.push(invalid_reference(
                                &expression.origin,
                                "index",
                                "expression",
                                child.index(),
                            ));
                        }
                    }
                    if self
                        .trait_methods
                        .get(index.dispatch.method)
                        .map(|method| method.trait_id)
                        != Some(index.trait_id)
                    {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "index dispatch method does not belong to its recorded trait",
                        ));
                    }
                    if index.method_type.is_none() {
                        diagnostics.push(Diagnostic::new(
                            expression.origin.span.clone(),
                            "index dispatch has no instantiated method type for Stage 2.5",
                        ));
                    }
                    let base_type = self
                        .expressions
                        .get(index.base)
                        .map(|base| &base.value_type);
                    let position_type = self
                        .expressions
                        .get(index.index)
                        .map(|position| &position.value_type);
                    for (position, actual) in [
                        (0usize, base_type),
                        (1, position_type),
                        (2, Some(&expression.value_type)),
                    ] {
                        if let (Some(expected), Some(actual)) =
                            (index.arguments.get(position), actual)
                            && !types_agree(expected, actual)
                        {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                format!(
                                    "index dispatch argument {position} is `{expected}` but the lowered operand is `{actual}`"
                                ),
                            ));
                        }
                    }
                }
                LoweredExpressionKind::Loop(loop_) => {
                    if !self.blocks.contains(loop_.body) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "loop",
                            "block",
                            loop_.body.index(),
                        ));
                    }
                }
                LoweredExpressionKind::Match(match_) => {
                    if !self.expressions.contains(match_.subject) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "match",
                            "expression",
                            match_.subject.index(),
                        ));
                    }
                    for arm in &match_.arms {
                        if !self.patterns.contains(arm.pattern) {
                            diagnostics.push(invalid_reference(
                                &arm.origin,
                                "match arm",
                                "pattern",
                                arm.pattern.index(),
                            ));
                        }
                        if !self.expressions.contains(arm.body) {
                            diagnostics.push(invalid_reference(
                                &arm.origin,
                                "match arm",
                                "expression",
                                arm.body.index(),
                            ));
                        }
                        for symbol in &arm.bound_symbols {
                            if self.symbols.get(*symbol).is_none() {
                                diagnostics.push(invalid_reference(
                                    &arm.origin,
                                    "match arm",
                                    "symbol",
                                    symbol.0,
                                ));
                            }
                        }
                    }
                }
                LoweredExpressionKind::Call(call) => {
                    if !self.calls.contains(*call) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "expression",
                            "call",
                            call.index(),
                        ));
                    }
                }
                LoweredExpressionKind::CallableValue(value) => {
                    if !self.callable_values.contains(*value) {
                        diagnostics.push(invalid_reference(
                            &expression.origin,
                            "expression",
                            "callable value",
                            value.index(),
                        ));
                    }
                }
            }
            self.validate_expression_coercion(expression, &mut diagnostics);
        }
        diagnostics.extend(self.validate_loop_exits());
        diagnostics.extend(self.validate_mutation_dispatch_arguments());
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
                LoweredItemKind::Continue(_) => {}
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

    /// Checks that the occurrence lookup has exactly one entry per expression
    /// node, so no two nodes alias the same occurrence key.
    fn validate_occurrence_lookups(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        let nodes = self.expressions.iter().count();
        if self.expression_lookup.len() != nodes {
            diagnostics.push(Diagnostic::new(
                Span::Compiler,
                format!(
                    "expression occurrence lookup has {} entries for {nodes} nodes",
                    self.expression_lookup.len()
                ),
            ));
        }
        diagnostics
    }

    /// Walks every arena node through typed arena edges starting from module
    /// initializers and function bodies, and reports nodes that are not
    /// reachable from any root. Deliberate sharing through the occurrence
    /// memo is expected; the traversal visits each node once.
    fn validate_ownership(&self) -> Vec<Diagnostic> {
        let mut reached = Reachability::default();
        for (_, initializer) in self.initializers.iter() {
            self.visit_owned_block(initializer.body, &mut reached);
        }
        for (_, _, function) in self.functions.iter() {
            self.visit_owned_pattern(function.parameter_pattern, &mut reached);
            if let Some(body) = function.body {
                self.visit_owned_block(body, &mut reached);
            }
            // A function body that is itself an expression (for example a
            // block) was allocated through the dispatcher; the expression node
            // is a root in its own right even though `LoweredFunction` only
            // records the body block.
            let key = ExpressionKey {
                syntax: function.body_syntax,
                owner: ExpressionOwner::Function(function.semantic_id),
                context: ExpressionContext::Primary,
            };
            if let Some(body) = self.expression_lookup.get(&key) {
                self.visit_owned_expression(*body, &mut reached);
            }
        }
        let mut diagnostics = Vec::new();
        for (id, expression) in self.expressions.iter() {
            if !reached.expressions.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    expression.origin.span.clone(),
                    format!(
                        "lowered expression {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, pattern) in self.patterns.iter() {
            if !reached.patterns.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    pattern.origin.span.clone(),
                    format!(
                        "lowered pattern {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, place) in self.places.iter() {
            if !reached.places.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    place.origin.span.clone(),
                    format!(
                        "lowered place {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, block) in self.blocks.iter() {
            if !reached.blocks.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    block.origin.span.clone(),
                    format!(
                        "lowered block {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, item) in self.items.iter() {
            if !reached.items.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    format!(
                        "lowered item {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, call) in self.calls.iter() {
            if !reached.calls.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    call.origin.span.clone(),
                    format!(
                        "lowered call {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        for (id, value) in self.callable_values.iter() {
            if !reached.callable_values.contains(&id) {
                diagnostics.push(Diagnostic::new(
                    value.origin.span.clone(),
                    format!(
                        "lowered callable value {} is not reachable from any runtime root",
                        id.index()
                    ),
                ));
            }
        }
        diagnostics
    }

    fn visit_owned_block(&self, id: BlockId, reached: &mut Reachability) {
        if !reached.blocks.insert(id) {
            return;
        }
        let Some(block) = self.blocks.get(id) else {
            return;
        };
        for item in &block.items {
            self.visit_owned_item(*item, reached);
        }
        if let Some(result) = block.result {
            self.visit_owned_expression(result, reached);
        }
    }

    fn visit_owned_item(&self, id: ItemId, reached: &mut Reachability) {
        if !reached.items.insert(id) {
            return;
        }
        let Some(item) = self.items.get(id) else {
            return;
        };
        match &item.kind {
            LoweredItemKind::Binding(binding) => {
                if let Some(value) = binding.value {
                    self.visit_owned_expression(value, reached);
                }
            }
            LoweredItemKind::PatternBinding(binding) => {
                self.visit_owned_pattern(binding.pattern, reached);
                self.visit_owned_expression(binding.value, reached);
            }
            LoweredItemKind::Assignment(assignment) => {
                self.visit_owned_place(assignment.target, reached);
                self.visit_owned_expression(assignment.value, reached);
            }
            LoweredItemKind::Return(item) => self.visit_owned_expression(item.value, reached),
            LoweredItemKind::Break(item) => {
                if let Some(value) = item.value {
                    self.visit_owned_expression(value, reached);
                }
            }
            LoweredItemKind::Continue(_) => {}
            LoweredItemKind::Expression(statement) => {
                self.visit_owned_expression(statement.expression, reached);
            }
        }
    }

    fn visit_owned_expression(&self, id: ExpressionId, reached: &mut Reachability) {
        if !reached.expressions.insert(id) {
            return;
        }
        let Some(expression) = self.expressions.get(id) else {
            return;
        };
        match &expression.kind {
            LoweredExpressionKind::Block(block) => self.visit_owned_block(*block, reached),
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.visit_owned_expression(satisfies.value, reached);
            }
            LoweredExpressionKind::Logical(logical) => {
                self.visit_owned_expression(logical.left, reached);
                self.visit_owned_expression(logical.right, reached);
            }
            LoweredExpressionKind::Loop(loop_) => self.visit_owned_block(loop_.body, reached),
            LoweredExpressionKind::Match(match_) => {
                self.visit_owned_expression(match_.subject, reached);
                for arm in &match_.arms {
                    self.visit_owned_pattern(arm.pattern, reached);
                    self.visit_owned_expression(arm.body, reached);
                }
            }
            LoweredExpressionKind::Product(product) => {
                for step in &product.steps {
                    let child = match step {
                        LoweredProductStep::Positional { expression, .. }
                        | LoweredProductStep::Designated { expression, .. }
                        | LoweredProductStep::PositionalSpread { expression, .. }
                        | LoweredProductStep::NamedSpread { expression, .. }
                        | LoweredProductStep::Default { expression, .. } => *expression,
                    };
                    self.visit_owned_expression(child, reached);
                }
            }
            LoweredExpressionKind::RepeatedProduct(repeated) => {
                self.visit_owned_expression(repeated.expression, reached);
            }
            LoweredExpressionKind::Access(access) => {
                self.visit_owned_expression(access.base, reached);
            }
            LoweredExpressionKind::Index(index) => {
                self.visit_owned_expression(index.base, reached);
                self.visit_owned_expression(index.index, reached);
            }
            LoweredExpressionKind::StringTemplate(template) => {
                for part in &template.parts {
                    if let LoweredStringTemplatePart::Interpolation(interpolation) = part {
                        self.visit_owned_expression(interpolation.expression, reached);
                    }
                }
            }
            LoweredExpressionKind::Call(call) => self.visit_owned_call(*call, reached),
            LoweredExpressionKind::CallableValue(value) => {
                self.visit_owned_callable_value(*value, reached)
            }
            LoweredExpressionKind::Deferred(_)
            | LoweredExpressionKind::Name(_)
            | LoweredExpressionKind::Integer(_)
            | LoweredExpressionKind::Float(_)
            | LoweredExpressionKind::String(_)
            | LoweredExpressionKind::CString(_) => {}
        }
    }

    fn visit_owned_call(&self, id: LoweredCallId, reached: &mut Reachability) {
        if !reached.calls.insert(id) {
            return;
        }
        let Some(call) = self.calls.get(id) else {
            return;
        };
        if let Some(callee) = call.callee {
            self.visit_owned_expression(callee, reached);
        }
        for argument in &call.arguments {
            self.visit_owned_expression(argument.expression, reached);
            if let Some(place) = argument.place {
                self.visit_owned_place(place, reached);
            }
        }
        for step in &call.steps {
            let expression = match step {
                LoweredCallStep::Callee { expression } => Some(*expression),
                LoweredCallStep::Argument { .. } | LoweredCallStep::Resource { .. } => None,
                LoweredCallStep::ProductElement { expression, .. }
                | LoweredCallStep::ProductSpread { expression, .. }
                | LoweredCallStep::NamedProductSpread { expression, .. }
                | LoweredCallStep::Default { expression, .. } => Some(*expression),
                LoweredCallStep::Invoke => None,
            };
            if let Some(expression) = expression {
                self.visit_owned_expression(expression, reached);
            }
        }
    }

    fn visit_owned_callable_value(&self, id: LoweredCallableValueId, reached: &mut Reachability) {
        if !reached.callable_values.insert(id) {
            return;
        }
        let Some(value) = self.callable_values.get(id) else {
            return;
        };
        if let LoweredCallableTarget::IndirectClosure { callee } = &value.target {
            self.visit_owned_expression(*callee, reached);
        }
    }

    fn visit_owned_pattern(&self, id: PatternId, reached: &mut Reachability) {
        if !reached.patterns.insert(id) {
            return;
        }
        let Some(pattern) = self.patterns.get(id) else {
            return;
        };
        match &pattern.kind {
            LoweredPatternKind::Wildcard | LoweredPatternKind::Literal { .. } => {}
            LoweredPatternKind::Binding { .. } => {}
            LoweredPatternKind::Product { elements, .. } => {
                for element in elements {
                    self.visit_owned_pattern(*element, reached);
                }
            }
            LoweredPatternKind::Nominal { argument, .. } => {
                self.visit_owned_pattern(*argument, reached);
            }
            LoweredPatternKind::At { binding, pattern } => {
                self.visit_owned_pattern(*binding, reached);
                self.visit_owned_pattern(*pattern, reached);
            }
        }
    }

    fn visit_owned_place(&self, id: PlaceId, reached: &mut Reachability) {
        if !reached.places.insert(id) {
            return;
        }
        let Some(place) = self.places.get(id) else {
            return;
        };
        match &place.kind {
            LoweredPlaceKind::Temporary { expression } => {
                self.visit_owned_expression(*expression, reached);
            }
            LoweredPlaceKind::Dereference { reference, .. } => {
                self.visit_owned_expression(*reference, reached);
            }
            LoweredPlaceKind::ProductElement { base, .. }
            | LoweredPlaceKind::Representation { base } => {
                self.visit_owned_place(*base, reached);
            }
            LoweredPlaceKind::Indexed { base, index } => {
                self.visit_owned_place(*base, reached);
                self.visit_owned_expression(*index, reached);
            }
            LoweredPlaceKind::Symbol { .. }
            | LoweredPlaceKind::CapturedCell { .. }
            | LoweredPlaceKind::Resource { .. } => {}
        }
    }

    /// Replays a product's evaluation steps and checks that they fill exactly
    /// the final checked slots with slot/name agreement.
    fn validate_product_plan(
        &self,
        expression: &LoweredExpression,
        product: &LoweredProduct,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let slots = product.final_type.elements.len();
        if product.fields.len() != slots {
            diagnostics.push(Diagnostic::new(
                expression.origin.span.clone(),
                format!(
                    "product final layout has {} fields for {slots} slots",
                    product.fields.len()
                ),
            ));
        }
        let mut replayed = vec![None; slots];
        let check_slot = |diagnostics: &mut Vec<Diagnostic>, slot: usize| -> bool {
            if slot >= slots {
                diagnostics.push(Diagnostic::new(
                    expression.origin.span.clone(),
                    format!("product step targets out-of-range slot {slot} of {slots}"),
                ));
                false
            } else {
                true
            }
        };
        for step in &product.steps {
            match step {
                LoweredProductStep::Positional {
                    expression: child,
                    slot,
                } => {
                    if check_slot(diagnostics, *slot) {
                        replayed[*slot] = Some(*child);
                    }
                }
                LoweredProductStep::Designated {
                    name,
                    expression: child,
                    slot,
                } => {
                    if check_slot(diagnostics, *slot) {
                        if product.final_type.elements[*slot].name.as_deref() != Some(name.as_str())
                        {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                format!(
                                    "designated product step `{name}` targets a slot named `{:?}`",
                                    product.final_type.elements[*slot].name
                                ),
                            ));
                        }
                        replayed[*slot] = Some(*child);
                    }
                }
                LoweredProductStep::PositionalSpread {
                    expression: child,
                    mappings,
                } => {
                    for mapping in mappings {
                        if check_slot(diagnostics, mapping.slot) {
                            replayed[mapping.slot] = Some(*child);
                        }
                    }
                }
                LoweredProductStep::NamedSpread {
                    expression: child,
                    mappings,
                } => {
                    for mapping in mappings {
                        if check_slot(diagnostics, mapping.slot) {
                            if product.final_type.elements[mapping.slot].name.as_deref()
                                != Some(mapping.name.as_str())
                            {
                                diagnostics.push(Diagnostic::new(
                                    expression.origin.span.clone(),
                                    format!(
                                        "named product spread `{}` targets a slot named `{:?}`",
                                        mapping.name,
                                        product.final_type.elements[mapping.slot].name
                                    ),
                                ));
                            }
                            replayed[mapping.slot] = Some(*child);
                        }
                    }
                }
                LoweredProductStep::Default {
                    slot,
                    expression: child,
                    expected,
                } => {
                    if check_slot(diagnostics, *slot) {
                        let slot_type = &product.final_type.elements[*slot].value_type;
                        if !types_agree(expected, slot_type) {
                            diagnostics.push(Diagnostic::new(
                                expression.origin.span.clone(),
                                format!(
                                    "product default for slot {slot} expects `{expected}` but the slot is `{slot_type}`"
                                ),
                            ));
                        }
                        replayed[*slot] = Some(*child);
                    }
                }
            }
        }
        if replayed.len() == product.fields.len()
            && replayed
                .iter()
                .zip(&product.fields)
                .any(|(replayed, field)| replayed != &Some(*field))
        {
            diagnostics.push(Diagnostic::new(
                expression.origin.span.clone(),
                "product final layout disagrees with its replayed evaluation steps",
            ));
        }
    }

    /// Checks that an expression's checked coercion agrees with the node and,
    /// where statically available, with the child it coerces. Contextual
    /// default occurrences may carry a checked type from a different
    /// instantiation, so the child relation is only enforced for primary
    /// occurrences.
    fn validate_expression_coercion(
        &self,
        expression: &LoweredExpression,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(coercion) = &expression.coercion else {
            return;
        };
        if !matches!(expression.key.context, ExpressionContext::Primary) {
            return;
        }
        // A block can be checked more than once while inference converges, so
        // a recorded coercion may target a type the final expression pass
        // replaced. `satisfies` is single-pass, so its target is authoritative.
        if let LoweredExpressionKind::Satisfies(_) = expression.kind
            && coercion.target != expression.value_type
        {
            diagnostics.push(Diagnostic::new(
                expression.origin.span.clone(),
                format!(
                    "expression coercion target `{}` disagrees with its checked type `{}`",
                    coercion.target, expression.value_type
                ),
            ));
        }
        let coerced_child = match &expression.kind {
            LoweredExpressionKind::Satisfies(satisfies) => Some(satisfies.value),
            LoweredExpressionKind::Block(block) => {
                self.blocks.get(*block).and_then(|block| block.result)
            }
            _ => None,
        };
        if let Some(child) = coerced_child
            && let Some(child) = self.expressions.get(child)
            && child.value_type != coercion.source
            && child.value_type != CheckedType::Never
            && coercion.source != CheckedType::Never
        {
            diagnostics.push(Diagnostic::new(
                expression.origin.span.clone(),
                format!(
                    "expression coercion source `{}` disagrees with its child type `{}`",
                    coercion.source, child.value_type
                ),
            ));
        }
    }

    /// Checks runtime metadata that requires the checked module: block results
    /// are never also statement items, and expression-statement discard/drop
    /// facts agree with the checked value type.
    fn validate_runtime_metadata(&self, module: &TypedModule) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, block) in self.blocks.iter() {
            let Some(result) = block.result else {
                continue;
            };
            let duplicated = block.items.iter().any(|item| {
                matches!(
                    self.items.get(*item).map(|item| &item.kind),
                    Some(LoweredItemKind::Expression(statement))
                        if statement.expression == result
                )
            });
            if duplicated {
                diagnostics.push(Diagnostic::new(
                    block.origin.span.clone(),
                    "lowered block result is also present as a statement item",
                ));
            }
        }
        for (_, item) in self.items.iter() {
            let LoweredItemKind::Expression(statement) = &item.kind else {
                continue;
            };
            let Some(expression) = self.expressions.get(statement.expression) else {
                continue;
            };
            if statement.drop_result != module.type_needs_drop(&expression.value_type) {
                diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    "expression statement discard/drop fact disagrees with its checked type",
                ));
            }
        }
        diagnostics
    }

    /// Cross-checks `MutateIndex` assignment dispatches against the lowered
    /// indexed place: the checked argument types must agree with the base,
    /// the position expression, and the element type.
    fn validate_mutation_dispatch_arguments(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        for (_, item) in self.items.iter() {
            let LoweredItemKind::Assignment(assignment) = &item.kind else {
                continue;
            };
            let Some(dispatch) = &assignment.mutate_index else {
                continue;
            };
            if self.trait_methods.get(dispatch.method).is_none() {
                diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    "assignment `MutateIndex` dispatch method is missing from the trait method catalog",
                ));
            }
            let Some(place) = self.places.get(assignment.target) else {
                continue;
            };
            let LoweredPlaceKind::Indexed { base, index } = &place.kind else {
                continue;
            };
            let base_type = self.places.get(*base).map(|base| &base.value_type);
            let position_type = self.expressions.get(*index).map(|index| &index.value_type);
            for (position, actual) in [
                (0usize, base_type),
                (1, position_type),
                (2, Some(&place.value_type)),
            ] {
                if let (Some(expected), Some(actual)) = (dispatch.arguments.get(position), actual)
                    && !types_agree(expected, actual)
                {
                    diagnostics.push(Diagnostic::new(
                        item.origin.span.clone(),
                        format!(
                            "`MutateIndex` dispatch argument {position} is `{expected}` but the lowered place operand is `{actual}`"
                        ),
                    ));
                }
            }
        }
        diagnostics
    }

    /// Traverses every lowered loop body once and checks that each
    /// break/continue item is owned by an enclosing loop at the depth it
    /// recorded. Orphaned exits and inconsistent nested depths diagnose.
    fn validate_loop_exits(&self) -> Vec<Diagnostic> {
        let mut reached = HashMap::<ItemId, usize>::new();
        let mut diagnostics = Vec::new();
        for (_, expression) in self.expressions.iter() {
            if let LoweredExpressionKind::Loop(loop_) = &expression.kind {
                self.collect_loop_items(loop_.body, loop_.depth, &mut reached, &mut diagnostics);
            }
        }
        for (item_id, item) in self.items.iter() {
            let recorded = match &item.kind {
                LoweredItemKind::Break(item) => Some(item.loop_depth),
                LoweredItemKind::Continue(item) => Some(item.loop_depth),
                _ => None,
            };
            let Some(recorded) = recorded else {
                continue;
            };
            match reached.get(&item_id) {
                Some(depth) if *depth == recorded => {}
                Some(depth) => diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    format!(
                        "break/continue targets loop depth {recorded} but is inside loop depth {depth}"
                    ),
                )),
                None => diagnostics.push(Diagnostic::new(
                    item.origin.span.clone(),
                    "break/continue item is not owned by an enclosing lowered loop",
                )),
            }
        }
        diagnostics
    }

    fn collect_loop_items(
        &self,
        block: BlockId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(block) = self.blocks.get(block) else {
            return;
        };
        for item in &block.items {
            let item_id = *item;
            let Some(lowered) = self.items.get(item_id) else {
                continue;
            };
            match &lowered.kind {
                LoweredItemKind::Break(_) | LoweredItemKind::Continue(_) => {
                    reached.entry(item_id).or_insert(depth);
                }
                LoweredItemKind::Binding(binding) => {
                    if let Some(value) = binding.value {
                        self.collect_loop_expression(value, depth, reached, diagnostics);
                    }
                }
                LoweredItemKind::PatternBinding(binding) => {
                    self.collect_loop_expression(binding.value, depth, reached, diagnostics);
                }
                LoweredItemKind::Assignment(assignment) => {
                    self.collect_loop_place(assignment.target, depth, reached, diagnostics);
                    self.collect_loop_expression(assignment.value, depth, reached, diagnostics);
                }
                LoweredItemKind::Return(item) => {
                    self.collect_loop_expression(item.value, depth, reached, diagnostics);
                }
                LoweredItemKind::Expression(statement) => {
                    self.collect_loop_expression(statement.expression, depth, reached, diagnostics);
                }
            }
        }
        if let Some(result) = block.result {
            self.collect_loop_expression(result, depth, reached, diagnostics);
        }
    }

    fn collect_loop_expression(
        &self,
        expression: ExpressionId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(expression) = self.expressions.get(expression) else {
            return;
        };
        match &expression.kind {
            LoweredExpressionKind::Loop(inner) => {
                if inner.depth != depth + 1 {
                    diagnostics.push(Diagnostic::new(
                        expression.origin.span.clone(),
                        format!(
                            "nested loop records depth {} instead of {}",
                            inner.depth,
                            depth + 1
                        ),
                    ));
                }
                self.collect_loop_items(inner.body, inner.depth, reached, diagnostics);
            }
            LoweredExpressionKind::Block(block) => {
                self.collect_loop_items(*block, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Satisfies(satisfies) => {
                self.collect_loop_expression(satisfies.value, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Logical(logical) => {
                self.collect_loop_expression(logical.left, depth, reached, diagnostics);
                self.collect_loop_expression(logical.right, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Match(match_) => {
                self.collect_loop_expression(match_.subject, depth, reached, diagnostics);
                for arm in &match_.arms {
                    self.collect_loop_expression(arm.body, depth, reached, diagnostics);
                }
            }
            LoweredExpressionKind::Product(product) => {
                for step in &product.steps {
                    let child = match step {
                        LoweredProductStep::Positional { expression, .. }
                        | LoweredProductStep::Designated { expression, .. }
                        | LoweredProductStep::PositionalSpread { expression, .. }
                        | LoweredProductStep::NamedSpread { expression, .. }
                        | LoweredProductStep::Default { expression, .. } => *expression,
                    };
                    self.collect_loop_expression(child, depth, reached, diagnostics);
                }
            }
            LoweredExpressionKind::RepeatedProduct(repeated) => {
                self.collect_loop_expression(repeated.expression, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Access(access) => {
                self.collect_loop_expression(access.base, depth, reached, diagnostics);
            }
            LoweredExpressionKind::Index(index) => {
                self.collect_loop_expression(index.base, depth, reached, diagnostics);
                self.collect_loop_expression(index.index, depth, reached, diagnostics);
            }
            LoweredExpressionKind::StringTemplate(template) => {
                for part in &template.parts {
                    if let LoweredStringTemplatePart::Interpolation(interpolation) = part {
                        self.collect_loop_expression(
                            interpolation.expression,
                            depth,
                            reached,
                            diagnostics,
                        );
                    }
                }
            }
            LoweredExpressionKind::Call(call) => {
                self.collect_loop_call(*call, depth, reached, diagnostics)
            }
            LoweredExpressionKind::CallableValue(value) => {
                self.collect_loop_callable_value(*value, depth, reached, diagnostics)
            }
            LoweredExpressionKind::Deferred(_)
            | LoweredExpressionKind::Name(_)
            | LoweredExpressionKind::Integer(_)
            | LoweredExpressionKind::Float(_)
            | LoweredExpressionKind::String(_)
            | LoweredExpressionKind::CString(_) => {}
        }
    }

    fn collect_loop_call(
        &self,
        id: LoweredCallId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(call) = self.calls.get(id) else {
            return;
        };
        if let Some(callee) = call.callee {
            self.collect_loop_expression(callee, depth, reached, diagnostics);
        }
        for argument in &call.arguments {
            self.collect_loop_expression(argument.expression, depth, reached, diagnostics);
            if let Some(place) = argument.place {
                self.collect_loop_place(place, depth, reached, diagnostics);
            }
        }
        for step in &call.steps {
            let expression = match step {
                LoweredCallStep::Callee { expression } => Some(*expression),
                LoweredCallStep::Argument { .. } | LoweredCallStep::Resource { .. } => None,
                LoweredCallStep::ProductElement { expression, .. }
                | LoweredCallStep::ProductSpread { expression, .. }
                | LoweredCallStep::NamedProductSpread { expression, .. }
                | LoweredCallStep::Default { expression, .. } => Some(*expression),
                LoweredCallStep::Invoke => None,
            };
            if let Some(expression) = expression {
                self.collect_loop_expression(expression, depth, reached, diagnostics);
            }
        }
    }

    fn collect_loop_callable_value(
        &self,
        id: LoweredCallableValueId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(value) = self.callable_values.get(id) else {
            return;
        };
        if let LoweredCallableTarget::IndirectClosure { callee } = &value.target {
            self.collect_loop_expression(*callee, depth, reached, diagnostics);
        }
    }

    fn collect_loop_place(
        &self,
        place: PlaceId,
        depth: usize,
        reached: &mut HashMap<ItemId, usize>,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(place) = self.places.get(place) else {
            return;
        };
        match &place.kind {
            LoweredPlaceKind::Temporary { expression } => {
                self.collect_loop_expression(*expression, depth, reached, diagnostics);
            }
            LoweredPlaceKind::Dereference { reference, .. } => {
                self.collect_loop_expression(*reference, depth, reached, diagnostics);
            }
            LoweredPlaceKind::ProductElement { base, .. }
            | LoweredPlaceKind::Representation { base } => {
                self.collect_loop_place(*base, depth, reached, diagnostics);
            }
            LoweredPlaceKind::Indexed { base, index } => {
                self.collect_loop_place(*base, depth, reached, diagnostics);
                self.collect_loop_expression(*index, depth, reached, diagnostics);
            }
            LoweredPlaceKind::Symbol { .. }
            | LoweredPlaceKind::CapturedCell { .. }
            | LoweredPlaceKind::Resource { .. } => {}
        }
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

/// The stable name of a Stage 2.4 expression family.
fn family_name(family: Stage24Family) -> &'static str {
    match family {
        Stage24Family::Block => "Block",
        Stage24Family::Satisfies => "Satisfies",
        Stage24Family::Match => "Match",
        Stage24Family::Loop => "Loop",
        Stage24Family::Product => "Product",
        Stage24Family::RepeatedProduct => "RepeatedProduct",
        Stage24Family::Access => "Access",
        Stage24Family::Index => "Index",
        Stage24Family::Logical => "Logical",
        Stage24Family::Name => "Name",
        Stage24Family::String => "String",
        Stage24Family::StringTemplate => "StringTemplate",
        Stage24Family::CString => "CString",
        Stage24Family::Integer => "Integer",
        Stage24Family::Float => "Float",
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

/// Nodes reached by the ownership traversal from runtime roots.
#[derive(Default)]
struct Reachability {
    expressions: HashSet<ExpressionId>,
    patterns: HashSet<PatternId>,
    places: HashSet<PlaceId>,
    blocks: HashSet<BlockId>,
    items: HashSet<ItemId>,
    calls: HashSet<LoweredCallId>,
    callable_values: HashSet<LoweredCallableValueId>,
}

/// Whether a checked dispatch argument and a lowered operand type agree well
/// enough for validation. `Inferred`, `Error`, and `Never` occurrences are
/// placeholders rather than real operand types.
fn types_agree(expected: &CheckedType, actual: &CheckedType) -> bool {
    expected == actual
        || matches!(
            expected,
            CheckedType::Inferred | CheckedType::Error | CheckedType::Never
        )
        || matches!(
            actual,
            CheckedType::Inferred | CheckedType::Error | CheckedType::Never
        )
}

/// A source diagnostic for a final product slot that no explicit element or
/// contextual default filled.
fn missing_product_slot_error(
    final_type: &CheckedProductType,
    slot: usize,
    span: &Span,
) -> Diagnostic {
    Diagnostic::new(
        span.clone(),
        match final_type.elements.get(slot) {
            Some(field) => match &field.name {
                Some(name) => format!("missing product field `{name}`"),
                None => format!("missing product element at position {slot}"),
            },
            None => format!("missing product element at position {slot}"),
        },
    )
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
        let validation = program.validate();
        assert!(validation.is_empty(), "{validation:?}");
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
            LoweredItemKind::Continue(_) => "continue",
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
        for (_, call) in program.calls.iter() {
            lines.push(format!("call {call:?}"));
        }
        for (_, value) in program.callable_values.iter() {
            lines.push(format!("callable value {value:?}"));
        }
        lines.push(format!("semantic ids {:?}", program.semantic_ids));
        lines.push(format!("string formatting {:?}", program.string_formatting));
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
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dangling block reference 4")),
            "unexpected diagnostics: {diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("not reachable")),
            "the hand-built expression is unreachable"
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
    }

    #[test]
    fn call_route_decision_table_covers_every_callable_category() {
        let mut routes = HashSet::new();
        let mut categories = HashSet::new();
        for route in CallRoute::ALL {
            assert!(routes.insert(route), "route {route:?} appears twice");
            categories.insert(route.category());
        }
        for category in LoweredCallableCategory::ALL {
            assert!(
                categories.contains(&category),
                "no call route produces callable category {category:?}"
            );
        }

        let targets = [
            LoweredCallableTarget::DirectFunction {
                function: FunctionId(0),
                environment: LoweredCallEnvironment::None,
            },
            LoweredCallableTarget::IndirectClosure {
                callee: ExpressionId(0),
            },
            LoweredCallableTarget::ExternalFunction {
                symbol: SymbolId(0),
            },
            LoweredCallableTarget::Intrinsic {
                symbol: SymbolId(0),
                intrinsic: IntrinsicFunction::StringAdd,
            },
            LoweredCallableTarget::Constructor {
                symbol: SymbolId(0),
                type_id: TypeId(0),
                recursive: None,
            },
            LoweredCallableTarget::TraitImplementation {
                trait_id: TraitId(0),
                method: TraitMethodId(0),
                function: None,
            },
            LoweredCallableTarget::StructuralTraitMethod {
                trait_id: TraitId(0),
                method: TraitMethodId(0),
                structural: StructuralTraitMethod::Debug,
            },
            LoweredCallableTarget::CompilerHelper {
                function: FunctionId(0),
            },
        ];
        let target_categories = targets
            .iter()
            .map(LoweredCallableTarget::category)
            .collect::<HashSet<_>>();
        assert_eq!(
            target_categories.len(),
            LoweredCallableCategory::ALL.len(),
            "each callable category needs exactly one target representation"
        );
        for category in LoweredCallableCategory::ALL {
            assert!(
                target_categories.contains(&category),
                "no callable target represents category {category:?}"
            );
        }
    }

    #[test]
    fn classifies_every_source_call_route_without_a_fallback() {
        let module = checked_program(concat!(
            "use std.cinterop.*\n",
            "extern \"c\" { external_identity: I32 -> I32 }\n",
            "trait TestShow T { test_show: T -> Bool }\n",
            "impl TestShow I32 { def test_show = _ => True }\n",
            "def generic_identity: <T where Copy T> T -> T = value => value\n",
            "let juxtaposed: x: I32 * y: I32 -> I32 = x * y => x + y\n",
            "type TestBox = ctor (value: I32)\n",
            "def sum_product: (I32, I32) -> I32 = pair => {\n",
            "  let mut total: I32 = 0\n",
            "  for value in pair { total = total + value }\n",
            "  total\n",
            "}\n",
            "def route_samples: () -> I32 = () => {\n",
            "  let closure = (value: I32) => value\n",
            "  let indirect: I32 = closure (1)\n",
            "  let direct: I32 = generic_identity 1\n",
            "  let arithmetic: I32 = 1 + 2\n",
            "  let juxtaposed_result: I32 = juxtaposed 1 2\n",
            "  let external_result: I32 = external_identity (1)\n",
            "  let built: TestBox = TestBox (value: 1)\n",
            "  let ctext = c_string \"hi\"\n",
            "  let shown: Bool = test_show 1\n",
            "  direct\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let mut routes = Vec::new();
        for function in module.functions().iter().chain(module.implicit_thunks()) {
            collect_call_routes(
                &program,
                &module,
                ExpressionOwner::Function(function.id),
                &function.body,
                &mut routes,
            );
        }
        // `CurriedDefault` is unreachable from accepted source (curried
        // defaults are rejected during resolution), `PrimitiveMacro` calls are
        // normalized to `Expression::CString` by macro expansion and remain a
        // defensive route, and `CompilerHelper` is selected by checked
        // operations rather than source calls. The route/category table test
        // covers all three; every source-reachable route is asserted here.
        for route in [
            CallRoute::Juxtaposed,
            CallRoute::JuxtaposedIntrinsic,
            CallRoute::TraitImplementation,
            CallRoute::DeclaredTraitBound,
            CallRoute::StructuralTraitMethod,
            CallRoute::Intrinsic,
            CallRoute::GenericDirect,
            CallRoute::External,
            CallRoute::Indirect,
            CallRoute::Constructor,
        ] {
            assert!(
                routes.contains(&route),
                "expected the source decision table to contain route {route:?}, found {routes:?}"
            );
        }
    }

    fn collect_call_routes(
        program: &LoweredProgram,
        module: &TypedModule,
        owner: ExpressionOwner,
        expression: &Expression,
        routes: &mut Vec<CallRoute>,
    ) {
        if let Expression::Call(call) = expression {
            let route = program
                .classify_call_route(module, owner, call)
                .unwrap_or_else(|diagnostic| {
                    panic!("call route classification failed: {}", diagnostic.message)
                });
            routes.push(route);
        }
        match expression {
            Expression::Function(function) => {
                collect_call_routes(program, module, owner, &function.body, routes);
            }
            Expression::Satisfies(satisfies) => {
                collect_call_routes(program, module, owner, &satisfies.value, routes);
            }
            Expression::Match(match_) => {
                collect_call_routes(program, module, owner, &match_.subject, routes);
                for arm in &match_.arms {
                    collect_call_routes(program, module, owner, &arm.body, routes);
                }
            }
            Expression::Loop(loop_) => {
                collect_block_call_routes(program, module, owner, &loop_.body, routes);
            }
            Expression::Coro(coro) => {
                collect_block_call_routes(program, module, owner, &coro.body, routes);
            }
            Expression::Await(await_) => {
                collect_call_routes(program, module, owner, &await_.operand, routes);
            }
            Expression::Resource(_) => {}
            Expression::With(with) => {
                collect_call_routes(program, module, owner, &with.value, routes);
                collect_block_call_routes(program, module, owner, &with.body, routes);
            }
            Expression::Block(block) => {
                collect_block_call_routes(program, module, owner, block, routes);
            }
            Expression::Product(product) => {
                for element in &product.elements {
                    collect_call_routes(program, module, owner, &element.value, routes);
                }
            }
            Expression::RepeatedProduct(repeated) => {
                collect_call_routes(program, module, owner, &repeated.value, routes);
            }
            Expression::Call(call) => {
                collect_call_routes(program, module, owner, &call.callee, routes);
                collect_call_routes(program, module, owner, &call.argument, routes);
            }
            Expression::Access(access) => {
                collect_call_routes(program, module, owner, &access.value, routes);
            }
            Expression::Index(index) => {
                collect_call_routes(program, module, owner, &index.value, routes);
                collect_call_routes(program, module, owner, &index.index, routes);
            }
            Expression::Unary(unary) => {
                collect_call_routes(program, module, owner, &unary.operand, routes);
            }
            Expression::Binary(binary) => {
                collect_call_routes(program, module, owner, &binary.left, routes);
                collect_call_routes(program, module, owner, &binary.right, routes);
            }
            Expression::Logical(logical) => {
                collect_call_routes(program, module, owner, &logical.left, routes);
                collect_call_routes(program, module, owner, &logical.right, routes);
            }
            Expression::StringTemplate(template) => {
                for part in &template.parts {
                    if let staple_syntax::StringTemplatePart::Interpolation(interpolation) = part {
                        collect_call_routes(
                            program,
                            module,
                            owner,
                            &interpolation.expression,
                            routes,
                        );
                    }
                }
            }
            Expression::SyntaxArgument(_)
            | Expression::VisibilityArgument(_)
            | Expression::Quote(_)
            | Expression::Splice(_)
            | Expression::Name(_)
            | Expression::String(_)
            | Expression::CString(_)
            | Expression::Integer(_)
            | Expression::Float(_) => {}
        }
    }

    fn collect_block_call_routes(
        program: &LoweredProgram,
        module: &TypedModule,
        owner: ExpressionOwner,
        block: &staple_syntax::BlockExpression,
        routes: &mut Vec<CallRoute>,
    ) {
        for item in &block.items {
            match item {
                Item::Binding(binding) => {
                    if let Some(value) = &binding.value {
                        collect_call_routes(program, module, owner, value, routes);
                    }
                }
                Item::PatternBinding(binding) => {
                    collect_call_routes(program, module, owner, &binding.value, routes);
                }
                Item::Assignment(assignment) => {
                    collect_call_routes(program, module, owner, &assignment.target, routes);
                    collect_call_routes(program, module, owner, &assignment.value, routes);
                }
                Item::Return(item) => {
                    collect_call_routes(program, module, owner, &item.value, routes);
                }
                Item::Break(item) => {
                    if let Some(value) = &item.value {
                        collect_call_routes(program, module, owner, value, routes);
                    }
                }
                Item::Continue(_) => {}
                Item::Expression(expression) => {
                    collect_call_routes(program, module, owner, expression, routes);
                }
                _ => {}
            }
        }
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

    fn replay_product_steps(product: &LoweredProduct) -> Vec<ExpressionId> {
        let mut fields = vec![None; product.final_type.elements.len()];
        for step in &product.steps {
            match step {
                LoweredProductStep::Positional { expression, slot } => {
                    fields[*slot] = Some(*expression)
                }
                LoweredProductStep::Designated {
                    expression, slot, ..
                } => fields[*slot] = Some(*expression),
                LoweredProductStep::PositionalSpread {
                    expression,
                    mappings,
                } => {
                    for mapping in mappings {
                        fields[mapping.slot] = Some(*expression);
                    }
                }
                LoweredProductStep::NamedSpread {
                    expression,
                    mappings,
                } => {
                    for mapping in mappings {
                        fields[mapping.slot] = Some(*expression);
                    }
                }
                LoweredProductStep::Default {
                    slot, expression, ..
                } => fields[*slot] = Some(*expression),
            }
        }
        fields
            .into_iter()
            .map(|field| field.expect("every slot is filled"))
            .collect()
    }

    #[test]
    fn positional_products_expand_spreads_and_defaults() {
        let module = checked_program(concat!(
            "let pair = (left: 2, right: 3)\n",
            "let expanded = (prefix: \"value\", ...pair, suffix: False)\n",
            "let point: (x: I32 = 1, y: I32 = 2) = ()\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let products = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Product(product) => Some(product),
                _ => None,
            })
            .collect::<Vec<_>>();

        let spread = products
            .iter()
            .find(|product| {
                product
                    .steps
                    .iter()
                    .any(|step| matches!(step, LoweredProductStep::PositionalSpread { .. }))
            })
            .expect("the spread product should lower");
        assert_eq!(spread.final_type.elements.len(), 4);
        assert_eq!(spread.fields.len(), 4);
        let mut kinds = spread
            .steps
            .iter()
            .map(|step| match step {
                LoweredProductStep::Positional { .. } => "positional",
                LoweredProductStep::Designated { .. } => "designated",
                LoweredProductStep::PositionalSpread { .. } => "spread",
                LoweredProductStep::NamedSpread { .. } => "named-spread",
                LoweredProductStep::Default { .. } => "default",
            })
            .collect::<Vec<_>>();
        kinds.sort_unstable();
        assert_eq!(kinds, vec!["positional", "positional", "spread"]);
        let LoweredProductStep::PositionalSpread { mappings, .. } = spread
            .steps
            .iter()
            .find(|step| matches!(step, LoweredProductStep::PositionalSpread { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(
            mappings,
            &vec![
                LoweredSpreadMapping { source: 0, slot: 1 },
                LoweredSpreadMapping { source: 1, slot: 2 },
            ]
        );
        assert_eq!(replay_product_steps(spread), spread.fields);

        let defaulted = products
            .iter()
            .find(|product| {
                product
                    .steps
                    .iter()
                    .any(|step| matches!(step, LoweredProductStep::Default { .. }))
            })
            .expect("the default product should lower");
        assert_eq!(defaulted.final_type.elements.len(), 2);
        let defaults = defaulted
            .steps
            .iter()
            .filter_map(|step| match step {
                LoweredProductStep::Default {
                    slot,
                    expression,
                    expected,
                } => Some((*slot, *expression, expected.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(defaults.len(), 2);
        assert_eq!(defaults[0].0, 0);
        assert_eq!(defaults[1].0, 1);
        for (slot, expression, expected) in &defaults {
            assert_eq!(expected, &CheckedType::I32);
            let lowered = program.expressions.get(*expression).expect("default value");
            assert_eq!(lowered.value_type, CheckedType::I32);
            assert!(
                matches!(
                    lowered.key.context,
                    ExpressionContext::ContextualDefault { slot: lowered_slot, .. }
                        if lowered_slot == *slot
                ),
                "defaults use occurrence-aware keys"
            );
        }
        assert_ne!(
            defaults[0].1, defaults[1].1,
            "one default declaration used twice must not alias"
        );
        assert_eq!(replay_product_steps(defaulted), defaulted.fields);
    }

    #[test]
    fn designated_products_resolve_slots_and_overrides() {
        let module = checked_program(concat!(
            "let value: (I32, I32, a: I32, b: I32) = (1, 2, .b: 4, .a: 3)\n",
            "let overridden: (a: I32, b: I32) = (1, 2, .a: 10, .a: 30)\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let designated = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Product(product)
                    if product
                        .steps
                        .iter()
                        .any(|step| matches!(step, LoweredProductStep::Designated { .. })) =>
                {
                    Some(product)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(designated.len(), 2);

        let slots = |product: &LoweredProduct| {
            product
                .steps
                .iter()
                .filter_map(|step| match step {
                    LoweredProductStep::Designated { name, slot, .. } => {
                        Some((name.clone(), *slot))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            slots(designated[0]),
            vec![("b".to_owned(), 3), ("a".to_owned(), 2)]
        );
        for product in &designated {
            assert_eq!(replay_product_steps(product), product.fields);
        }

        // `(1, 2, .a: 10, .a: 30)` keeps the last designator in the final
        // layout while evaluating every source element in order.
        let overridden = &designated[1];
        assert_eq!(overridden.steps.len(), 4);
        assert_eq!(overridden.fields.len(), 2);
        let last = overridden
            .steps
            .iter()
            .rev()
            .find_map(|step| match step {
                LoweredProductStep::Designated {
                    expression, name, ..
                } if name == "a" => Some(*expression),
                _ => None,
            })
            .expect("last `a` designator");
        assert_eq!(overridden.fields[0], last);
    }

    #[test]
    fn named_spreads_expand_fields_and_override_in_source_order() {
        let module = checked_program(concat!(
            "let dimensions = (height: 600, width: 800)\n",
            "let config: (width: I32, height: I32, title: String) = (\n",
            "    ...=dimensions,\n",
            "    title: \"Staple\",\n",
            ")\n",
            "let overridden: (width: I32, height: I32) = (\n",
            "    ...=dimensions,\n",
            "    width: 900,\n",
            ")\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let named = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Product(product)
                    if product
                        .steps
                        .iter()
                        .any(|step| matches!(step, LoweredProductStep::NamedSpread { .. })) =>
                {
                    Some(product)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(named.len(), 2);

        let config = &named[0];
        assert_eq!(config.final_type.elements.len(), 3);
        let LoweredProductStep::NamedSpread { mappings, .. } = &config.steps[0] else {
            panic!("the named spread is the first step");
        };
        assert_eq!(
            mappings,
            &vec![
                LoweredNamedSpreadMapping {
                    name: "height".to_owned(),
                    source: 0,
                    slot: 1,
                },
                LoweredNamedSpreadMapping {
                    name: "width".to_owned(),
                    source: 1,
                    slot: 0,
                },
            ]
        );
        assert!(matches!(
            config.steps[1],
            LoweredProductStep::Designated { ref name, slot: 2, .. } if name == "title"
        ));
        assert_eq!(replay_product_steps(config), config.fields);

        // The later `width: 900` overrides the spread's `width` value.
        let overridden = &named[1];
        assert!(matches!(
            overridden.steps[1],
            LoweredProductStep::Designated { ref name, slot: 0, .. } if name == "width"
        ));
        assert_eq!(replay_product_steps(overridden), overridden.fields);
        assert_eq!(
            overridden.fields[0],
            match overridden.steps[1] {
                LoweredProductStep::Designated { expression, .. } => expression,
                _ => unreachable!(),
            }
        );
    }

    #[test]
    fn repeated_products_record_count_and_collapse() {
        let module = checked_program(concat!(
            "type Count = alias 3\n",
            "let repeated: (I32; 3) = (9; Count)\n",
            "let single: (I32; 1) = (9; 1)\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let repeated = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::RepeatedProduct(repeated) => Some(repeated),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(repeated.len(), 2);
        let three = repeated
            .iter()
            .find(|repeated| repeated.count == LoweredRepeatCount::Fixed(3))
            .expect("count-three repeated product");
        assert!(!three.collapsed);
        let single = repeated
            .iter()
            .find(|repeated| repeated.count == LoweredRepeatCount::Fixed(1))
            .expect("count-one repeated product");
        assert!(single.collapsed);
        for repeated in &repeated {
            assert!(program.expressions.contains(repeated.expression));
        }
    }

    #[test]
    fn generic_repeated_product_retains_symbolic_count() {
        let module = checked_program(concat!(
            "def repeat: <T, N where Copy T, Natural N> T -> N -> (T; N) = value => n => (value; N)\n",
            "let repeated: (I32; 3) = repeat 7 3\n",
        ));
        let mut program = LoweredProgram::default();
        assert!(program.snapshot(&module).is_empty());
        assert!(program.validate().is_empty());

        let (symbolic_id, symbolic) = program
            .expressions
            .iter()
            .filter_map(|(id, expression)| match &expression.kind {
                LoweredExpressionKind::RepeatedProduct(repeated) => {
                    Some((id, expression, repeated))
                }
                _ => None,
            })
            .find(|(_, expression, _)| matches!(expression.value_type, CheckedType::Array { .. }))
            .map(|(id, expression, repeated)| (id, (expression, repeated)))
            .expect("generic repeated product");
        let CheckedType::Array { count, .. } = &symbolic.0.value_type else {
            unreachable!()
        };
        assert_eq!(
            symbolic.1.count,
            LoweredRepeatCount::Symbolic(count.as_ref().clone())
        );
        assert!(!symbolic.1.collapsed);

        let expression = program
            .expressions
            .values
            .get_mut(symbolic_id.index())
            .unwrap();
        let LoweredExpressionKind::RepeatedProduct(repeated) = &mut expression.kind else {
            unreachable!()
        };
        repeated.count = LoweredRepeatCount::Fixed(1);
        repeated.collapsed = true;
        assert!(program.validate().iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("fixed count disagrees with its symbolic array result type")
        }));
    }

    #[test]
    fn satisfies_lowers_as_wrapper_and_keeps_checked_coercions() {
        let module = checked_program(concat!(
            "use std.slice.Slice\n",
            "let widened = 42 satisfies I8\n",
            "let text: String = \"literal\"\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok (42)\n",
            "let sum: Ok I32 | IOError = Ok (41)\n",
            "let slice: Slice I32 = Ref 8\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (expression, satisfies) = program
            .expressions
            .iter()
            .find_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Satisfies(satisfies) => program
                    .expressions
                    .get(satisfies.value)
                    .filter(|child| child.value_type == CheckedType::I8)
                    .map(|_| (expression, satisfies)),
                _ => None,
            })
            .expect("`42 satisfies I8` lowers as a wrapper");
        assert_eq!(expression.value_type, CheckedType::I8);
        let child = program
            .expressions
            .get(satisfies.value)
            .expect("satisfies child");
        assert_eq!(child.value_type, CheckedType::I8);
        assert_eq!(
            child.value_type,
            module
                .type_of_expression(child.key.syntax)
                .cloned()
                .expect("checked child type")
        );

        // Coercions recorded on deferred call headers (sum injection and
        // `Ref` to `Slice`) are retained for Stage 2.5.
        let coercions = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| expression.coercion.clone())
            .collect::<Vec<_>>();
        assert!(coercions.iter().any(|coercion| matches!(
            &coercion.target,
            CheckedType::Sum(sum) if sum.alternatives.len() == 2
        )));
        assert!(coercions.iter().any(|coercion| matches!(
            (&coercion.source, &coercion.target),
            (CheckedType::Ref(_), CheckedType::Slice(_))
        )));
    }

    #[test]
    fn blocks_preserve_nested_results_divergence_and_drop_facts() {
        let module = checked_program(concat!(
            "type Handle = ctor I32\n",
            "impl Drop Handle { def drop = Handle value => () }\n",
            "def nested = () => {\n",
            "  let outer: I32 = { let inner: I32 = 1; inner + 2 }\n",
            "  outer\n",
            "}\n",
            "def early = () => { return 1; 0 }\n",
            "def discards = () => {\n",
            "  Handle 1\n",
            "  ()\n",
            "}\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "def read: () -> Ok I32 | IOError = () => Ok (42)\n",
            "def propagates = () => {\n",
            "  let Ok(value)? = read()\n",
            "  value\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        // A nested block is a reachable block expression whose items and
        // result were lowered through the same arena.
        let nested_blocks = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Block(block) => program
                    .blocks
                    .get(*block)
                    .filter(|block| block.items.len() == 1 && block.result.is_some())
                    .map(|_| ()),
                _ => None,
            })
            .count();
        assert!(nested_blocks >= 2, "function and nested blocks lower");

        let (_, early) = lowered_function(&program, "early");
        let block = body_block(&program, early);
        assert!(block.items.iter().any(|item| matches!(
            program.items.get(*item).map(|item| &item.kind),
            Some(LoweredItemKind::Return(_))
        )));
        let result = block.result.expect("unreachable tail keeps its result");
        assert!(!block.items.iter().any(|item| matches!(
            program.items.get(*item).map(|item| &item.kind),
            Some(LoweredItemKind::Expression(statement)) if statement.expression == result
        )));

        let (_, discards) = lowered_function(&program, "discards");
        let block = body_block(&program, discards);
        let discard = block
            .items
            .iter()
            .find_map(
                |item| match program.items.get(*item).map(|item| &item.kind) {
                    Some(LoweredItemKind::Expression(statement)) => Some(statement),
                    _ => None,
                },
            )
            .expect("the discarded value lowers as an expression statement");
        assert!(discard.drop_result, "owned discarded values need a drop");
        let discarded = program
            .expressions
            .get(discard.expression)
            .expect("discarded expression");
        assert!(module.type_needs_drop(&discarded.value_type));

        let (_, propagates) = lowered_function(&program, "propagates");
        let block = body_block(&program, propagates);
        let binding = block
            .items
            .iter()
            .find_map(
                |item| match program.items.get(*item).map(|item| &item.kind) {
                    Some(LoweredItemKind::PatternBinding(binding)) => Some(binding),
                    _ => None,
                },
            )
            .expect("the propagation binding lowers");
        assert!(binding.propagating);
        assert!(binding.propagation.is_some());

        // Every expression statement's discard/drop fact agrees with its
        // checked value type.
        for (_, item) in program.items.iter() {
            let LoweredItemKind::Expression(statement) = &item.kind else {
                continue;
            };
            let expression = program
                .expressions
                .get(statement.expression)
                .expect("statement expression");
            assert_eq!(
                statement.drop_result,
                module.type_needs_drop(&expression.value_type)
            );
        }
    }

    fn expression_body<'a>(program: &'a LoweredProgram, name: &str) -> &'a LoweredExpression {
        let (_, function) = lowered_function(program, name);
        let block = body_block(program, function);
        program
            .expressions
            .get(block.result.expect("function body result"))
            .expect("lowered body expression")
    }

    #[test]
    fn logical_operators_copy_checked_bool_and_operand_order() {
        let module = checked_program(concat!(
            "def both = (left: Bool, right: Bool) => left && right\n",
            "def either = (left: Bool, right: Bool) => left || right\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let both = expression_body(&program, "both");
        let LoweredExpressionKind::Logical(logical) = &both.kind else {
            panic!("`&&` lowers to a logical node");
        };
        assert_eq!(logical.operator, staple_syntax::LogicalOperator::And);
        assert!(program.expressions.contains(logical.left));
        assert!(program.expressions.contains(logical.right));
        let CheckedType::Sum(sum) = &logical.bool_type else {
            panic!("`Bool` is a sum type");
        };
        let expected_true = sum
            .alternatives
            .iter()
            .position(|alternative| {
                matches!(alternative, CheckedType::Distinct { name, .. } if name == "True")
            })
            .expect("`Bool` has a `True` alternative");
        assert_eq!(logical.true_index, expected_true);
        for operand in [logical.left, logical.right] {
            assert_eq!(
                program.expressions.get(operand).unwrap().value_type,
                logical.bool_type
            );
        }

        let either = expression_body(&program, "either");
        let LoweredExpressionKind::Logical(logical) = &either.kind else {
            panic!("`||` lowers to a logical node");
        };
        assert_eq!(logical.operator, staple_syntax::LogicalOperator::Or);
    }

    #[test]
    fn loops_record_drop_facts_depth_and_owned_exits() {
        let module = checked_program(concat!(
            "type Handle = ctor I32\n",
            "impl Drop Handle { def drop = Handle value => () }\n",
            "def select: Bool -> I32 = condition => loop {\n",
            "  match condition { True() => { break 9 }, False() => { continue } }\n",
            "}\n",
            "def nested = () => loop { loop { break () }; break 7 }\n",
            "def forever: () -> () = () => loop { continue }\n",
            "def drop_body: () -> Never = () => loop { Handle 1 }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let select = expression_body(&program, "select");
        let LoweredExpressionKind::Loop(loop_) = &select.kind else {
            panic!("`loop` lowers to a loop node");
        };
        assert_eq!(loop_.depth, 1);
        assert_eq!(loop_.result_type, CheckedType::I32);
        assert!(program.blocks.contains(loop_.body));
        let (break_depth, continue_depth) =
            program
                .items
                .iter()
                .fold(
                    (None, None),
                    |(break_depth, continue_depth), (_, item)| match &item.kind {
                        LoweredItemKind::Break(item) => (Some(item.loop_depth), continue_depth),
                        LoweredItemKind::Continue(item) => (break_depth, Some(item.loop_depth)),
                        _ => (break_depth, continue_depth),
                    },
                );
        assert_eq!(break_depth, Some(1));
        assert_eq!(continue_depth, Some(1));

        let nested = expression_body(&program, "nested");
        let LoweredExpressionKind::Loop(outer) = &nested.kind else {
            panic!("`loop` lowers to a loop node");
        };
        assert_eq!(outer.depth, 1);
        let loops = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Loop(loop_) => Some(loop_),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            loops.iter().any(|loop_| loop_.depth == 2),
            "the nested loop records depth 2"
        );
        let depths = program
            .items
            .iter()
            .filter_map(|(_, item)| match &item.kind {
                LoweredItemKind::Break(item) => Some(item.loop_depth),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(depths.contains(&1), "outer `break 7` targets depth 1");
        assert!(depths.contains(&2), "inner `break ()` targets depth 2");

        let drop_body = expression_body(&program, "drop_body");
        let LoweredExpressionKind::Loop(loop_) = &drop_body.kind else {
            panic!("`loop` lowers to a loop node");
        };
        assert!(
            loop_.drops_body_result,
            "owned loop bodies drop their result"
        );
        assert!(loop_.body_falls_through);
        assert_eq!(loop_.result_type, CheckedType::Never);

        let forever = expression_body(&program, "forever");
        let LoweredExpressionKind::Loop(loop_) = &forever.kind else {
            panic!("`loop` lowers to a loop node");
        };
        assert!(
            !loop_.body_falls_through,
            "a diverging body has no back edge"
        );
    }

    #[test]
    fn matches_lower_subject_arms_patterns_and_bound_symbols() {
        let module = checked_program(concat!(
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "def pick = (value: Ok I32 | IOError) => match value {\n",
            "  Ok payload => payload,\n",
            "  other => 0,\n",
            "}\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let pick = expression_body(&program, "pick");
        let LoweredExpressionKind::Match(match_) = &pick.kind else {
            panic!("`match` lowers to a match node");
        };
        assert!(program.expressions.contains(match_.subject));
        assert_eq!(match_.arms.len(), 2);
        for arm in &match_.arms {
            assert!(program.patterns.contains(arm.pattern));
            assert!(program.expressions.contains(arm.body));
            for symbol in &arm.bound_symbols {
                assert!(program.symbols.get(*symbol).is_some());
            }
        }
        assert_eq!(match_.arms[0].bound_symbols.len(), 1);
        assert_eq!(match_.arms[1].bound_symbols.len(), 1);
        let subject_type = program
            .expressions
            .get(match_.subject)
            .expect("match subject")
            .value_type
            .clone();
        assert_eq!(match_.source, subject_type);
    }

    #[test]
    fn validator_rejects_orphaned_and_misdepth_loop_exits() {
        let module = checked_program("def value: () -> () = () => { loop { break }; () }\n");
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        for entry in program.items.values.iter_mut() {
            if let LoweredItemKind::Break(item) = &mut entry.kind {
                item.loop_depth = 2;
            }
        }
        let diagnostics = program.validate();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("targets loop depth 2")),
            "unexpected diagnostics: {diagnostics:?}"
        );

        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        program.items.push(LoweredItem {
            origin: Origin::compiler(),
            kind: LoweredItemKind::Break(LoweredBreakItem {
                value: None,
                loop_depth: 1,
            }),
        });
        let diagnostics = program.validate();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("not owned by an enclosing lowered loop")),
            "unexpected diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn index_reads_copy_checked_dispatch_recipes_and_temporaries() {
        let module = checked_program(concat!(
            "use std.slice.Slice\n",
            "type Counter = ctor I32\n",
            "impl Index Counter String I32 { def index = (counter, key) => 0 }\n",
            "impl MutateIndex Counter String I32 { def mutate_index = (mut counter, key, move value) => () }\n",
            "def make_counter = () => Counter 0\n",
            "let mut counter = Counter 0\n",
            "let read = counter[\"key\"]\n",
            "let temporary = (make_counter())[\"key\"]\n",
            "counter[\"key\"] = 1\n",
            "(make_counter())[\"key\"] = 1\n",
            "let values: (I32; 2) = (10, 20)\n",
            "let element = values[0]\n",
            "def slice_read: (Slice I32, USize) -> I32 = (values, position) => values[position]\n",
            "def ref_read: (Ref (I32; 2), USize) -> I32 = (values, position) => values[position]\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let indexes = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::Index(index) => Some((expression, index)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(indexes.len() >= 5, "stdlib and test indexes both lower");

        let counter_type = module
            .declared_type_of_symbol(binding_symbol(&module, "counter"))
            .expect("counter type");
        let counter_reads = indexes
            .iter()
            .filter(|(_, index)| {
                program
                    .expressions
                    .get(index.base)
                    .is_some_and(|base| base.value_type == counter_type)
            })
            .collect::<Vec<_>>();
        assert_eq!(counter_reads.len(), 2, "direct and temporary reads lower");
        for (expression, index) in &counter_reads {
            assert!(program.expressions.contains(index.base));
            assert!(program.expressions.contains(index.index));
            assert_eq!(
                Some(index.trait_id),
                module.semantic_ids().index_trait,
                "the dispatch's owning trait is copied"
            );
            assert!(index.method_type.is_some(), "instantiated method type");
            assert!(matches!(
                index.dispatch.arguments.first(),
                Some(argument) if *argument == counter_type
            ));
            let base = program.expressions.get(index.base).unwrap();
            let position = program.expressions.get(index.index).unwrap();
            for (position, expected, actual) in [
                (0usize, &index.arguments[0], &base.value_type),
                (1, &index.arguments[1], &position.value_type),
                (2, &index.arguments[2], &expression.value_type),
            ] {
                assert!(
                    types_agree(expected, actual),
                    "dispatch argument {position} `{expected}` vs operand `{actual}`"
                );
            }
            // The read-only `Index` method declares no mutations, so no
            // operand temporary is required.
            assert!(!index.whole_temporary && !index.base_temporary && !index.index_temporary);
        }

        // A mutation of a non-place base materializes the base into a
        // temporary place; the assignment keeps its checked `MutateIndex`
        // dispatch and argument agreement.
        let temporary_targets = program
            .items
            .iter()
            .filter_map(|(_, item)| match &item.kind {
                LoweredItemKind::Assignment(assignment) => Some(assignment),
                _ => None,
            })
            .filter(|assignment| {
                matches!(
                    program.places.get(assignment.target).map(|place| &place.kind),
                    Some(LoweredPlaceKind::Indexed { base, .. })
                        if matches!(
                            program.places.get(*base).map(|place| &place.kind),
                            Some(LoweredPlaceKind::Temporary { .. })
                        )
                )
            })
            .collect::<Vec<_>>();
        assert!(
            !temporary_targets.is_empty(),
            "`(make_counter())[\"key\"] = 1` needs a temporary place"
        );
        for assignment in temporary_targets {
            assert!(assignment.mutate_index.is_some());
        }

        // Structural product, slice, and ref reads all lower through the same
        // checked dispatch recipe rather than a source-level access.
        for name in ["slice_read", "ref_read"] {
            let body = expression_body(&program, name);
            assert!(
                matches!(body.kind, LoweredExpressionKind::Index(_)),
                "`{name}` lowers through the index dispatcher"
            );
        }
        assert!(
            indexes.iter().any(
                |(_, index)| program
                    .expressions
                    .get(index.base)
                    .is_some_and(|base| matches!(
                        &base.value_type,
                        CheckedType::Product(product) if product.elements.len() == 2
                    ))
            ),
            "the fixed-product read lowers through the index dispatcher"
        );

        // The indexed assignment's `MutateIndex` dispatch agrees with the
        // lowered place operands (validated during snapshot).
        let assignments = program
            .items
            .iter()
            .filter_map(|(_, item)| match &item.kind {
                LoweredItemKind::Assignment(assignment) if assignment.mutate_index.is_some() => {
                    Some(assignment)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!assignments.is_empty());
    }

    #[test]
    fn string_templates_record_parts_and_formatting_selections() {
        let module = checked_program(concat!(
            "use std.fmt.Formatter\n",
            "type Label = ctor String\n",
            "impl Display Label {\n",
            "  def fmt = (Label value, mut formatter) => Formatter.write formatter value\n",
            "}\n",
            "def render: <T where Display T> move T -> String = move value => \"value=$value\"\n",
            "let name: String = \"world\"\n",
            "let answer: I32 = 42\n",
            "let product = (answer, name)\n",
            "let message: String = \"hello $name: ${answer}; ${product:?}; \\$5\"\n",
            "let generic: String = render answer\n",
            "let nominal: String = \"label=${Label \"tag\"}\"\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let formatting = program.string_formatting.clone();
        for function in [formatting.constructor, formatting.write, formatting.finish] {
            assert!(
                function.is_some_and(|id| program.functions.get(id).is_some()),
                "checked formatter helper selections resolve in the function catalog"
            );
        }

        let templates = program
            .expressions
            .iter()
            .filter_map(|(_, expression)| match &expression.kind {
                LoweredExpressionKind::StringTemplate(template) => Some(template),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            templates.len() >= 3,
            "message, nominal, and render templates"
        );

        let display_trait = module.semantic_ids().display_trait.expect("Display");
        let debug_trait = module.semantic_ids().debug_trait.expect("Debug");
        let message = templates
            .iter()
            .find(|template| {
                template.parts.iter().any(|part| {
                    matches!(part, LoweredStringTemplatePart::Literal(text) if text == "hello ")
                })
            })
            .expect("the message template lowers");
        let literals = message
            .parts
            .iter()
            .filter_map(|part| match part {
                LoweredStringTemplatePart::Literal(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(literals, vec!["hello ", ": ", "; ", "; $5"]);
        let interpolations = message
            .parts
            .iter()
            .filter_map(|part| match part {
                LoweredStringTemplatePart::Interpolation(interpolation) => Some(interpolation),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(interpolations.len(), 3);
        assert_eq!(
            interpolations[0].format,
            staple_syntax::StringInterpolationFormat::Display
        );
        assert_eq!(interpolations[0].trait_id, display_trait);
        assert_eq!(
            interpolations[1].format,
            staple_syntax::StringInterpolationFormat::Display
        );
        assert_eq!(interpolations[1].trait_id, display_trait);
        assert_eq!(
            interpolations[2].format,
            staple_syntax::StringInterpolationFormat::Debug
        );
        assert_eq!(interpolations[2].trait_id, debug_trait);
        for interpolation in &interpolations {
            assert!(program.expressions.contains(interpolation.expression));
            let value = program
                .expressions
                .get(interpolation.expression)
                .expect("interpolation value");
            assert_eq!(interpolation.value_type, value.value_type);
            let method = program
                .trait_methods
                .get(interpolation.method)
                .expect("formatting method");
            assert_eq!(method.trait_id, interpolation.trait_id);
        }

        // A generic interpolation keeps the declared type parameter and the
        // prelude's `Display` selection; nested interpolation expressions are
        // lowered left-to-right through the same arena.
        let generic = templates
            .iter()
            .find(|template| {
                template.parts.iter().any(|part| {
                    matches!(
                        part,
                        LoweredStringTemplatePart::Interpolation(interpolation)
                            if matches!(interpolation.value_type, CheckedType::Parameter { .. })
                    )
                })
            })
            .expect("the generic template keeps its parameter type");
        let nominal = templates
            .iter()
            .find(|template| {
                template.parts.iter().any(|part| {
                    matches!(
                        part,
                        LoweredStringTemplatePart::Literal(text) if text == "label="
                    )
                })
            })
            .expect("the nominal template lowers");
        let LoweredStringTemplatePart::Interpolation(interpolation) = &nominal.parts[1] else {
            panic!("the nominal interpolation is the second part");
        };
        assert!(matches!(
            program
                .expressions
                .get(interpolation.expression)
                .unwrap()
                .kind,
            LoweredExpressionKind::Deferred(DeferredExpressionFamily::Callable)
        ));
    }

    #[test]
    fn unreachable_string_templates_still_lower_their_parts() {
        let module = checked_program(concat!(
            "use std.fmt.Formatter\n",
            "let answer: I32 = 42\n",
            "def early = () => { return \"early\"; \"value=${answer}\" }\n",
        ));
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let (_, early) = lowered_function(&program, "early");
        let block = body_block(&program, early);
        let result = block.result.expect("unreachable tail is still the result");
        let LoweredExpressionKind::StringTemplate(template) =
            &program.expressions.get(result).unwrap().kind
        else {
            panic!("the unreachable tail template still lowers");
        };
        assert!(template.parts.iter().any(|part| matches!(
            part,
            LoweredStringTemplatePart::Interpolation(interpolation)
                if interpolation.value_type == CheckedType::I32
        )));
    }

    /// A fixture exercising every Stage 2.4-owned family, every later-stage
    /// deferral, and the shared checked metadata the transition comparisons
    /// read back.
    fn complete_coverage_source() -> &'static str {
        concat!(
            "use std.cinterop.*\n",
            "use std.coroutine.(Coroutine)\n",
            "use std.fmt.Formatter\n",
            "type Counter = ctor (value: I32)\n",
            "type Ok T = ctor T\n",
            "type IOError = ctor String\n",
            "let integer: I32 = 42\n",
            "let float: F64 = 1.5\n",
            "let string: String = \"text\"\n",
            "let cstring = c_string \"c\"\n",
            "let named: I32 = integer\n",
            "let product = (left: 1, right: 2)\n",
            "let left = product.left\n",
            "let values: (I32; 2) = (1, 2)\n",
            "let element = values[0]\n",
            "def logical = (flag: Bool) => flag && flag\n",
            "def looping = () => loop { break 1 }\n",
            "def matching = (value: Ok I32 | IOError) => match value {\n",
            "  Ok inner => inner,\n",
            "  other => 0,\n",
            "}\n",
            "def blocked = () => { let local: I32 = 1; local }\n",
            "let coerced: I8 = 42 satisfies I8\n",
            "let repeated: (I32; 3) = (7; 3)\n",
            "let template: String = \"value=${integer}\"\n",
            "def callable = (value: I32) => value\n",
            "let applied = callable 1\n",
            "let closure = callable\n",
            "def task: () -> Coroutine{} I32 = () => coro { 7 }\n",
            "def driver: () -> Coroutine{} I32 = () => coro { await (task ()) }\n",
            "def increment: () ->{mut Counter} () = () => {\n",
            "  (resource Counter).value = (resource Counter).value + 1\n",
            "}\n",
            "let mut counter = Counter (value: 0)\n",
            "with mut Counter = counter { increment () }\n",
        )
    }

    fn expression_kind_name(kind: &LoweredExpressionKind) -> String {
        match kind {
            LoweredExpressionKind::Deferred(DeferredExpressionFamily::Callable) => {
                "deferred.callable".to_owned()
            }
            LoweredExpressionKind::Deferred(DeferredExpressionFamily::Resource) => {
                "deferred.resource".to_owned()
            }
            LoweredExpressionKind::Deferred(DeferredExpressionFamily::Coroutine) => {
                "deferred.coroutine".to_owned()
            }
            LoweredExpressionKind::Block(_) => "block".to_owned(),
            LoweredExpressionKind::Name(_) => "name".to_owned(),
            LoweredExpressionKind::Integer(_) => "integer".to_owned(),
            LoweredExpressionKind::Float(_) => "float".to_owned(),
            LoweredExpressionKind::String(_) => "string".to_owned(),
            LoweredExpressionKind::CString(_) => "cstring".to_owned(),
            LoweredExpressionKind::Access(_) => "access".to_owned(),
            LoweredExpressionKind::Product(_) => "product".to_owned(),
            LoweredExpressionKind::RepeatedProduct(_) => "repeated-product".to_owned(),
            LoweredExpressionKind::Satisfies(_) => "satisfies".to_owned(),
            LoweredExpressionKind::Logical(_) => "logical".to_owned(),
            LoweredExpressionKind::Loop(_) => "loop".to_owned(),
            LoweredExpressionKind::Match(_) => "match".to_owned(),
            LoweredExpressionKind::Index(_) => "index".to_owned(),
            LoweredExpressionKind::StringTemplate(_) => "string-template".to_owned(),
            LoweredExpressionKind::Call(_) => "call".to_owned(),
            LoweredExpressionKind::CallableValue(_) => "callable-value".to_owned(),
        }
    }

    #[test]
    fn coverage_fixture_lowers_every_family_concretely() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let mut kinds = program
            .expressions
            .iter()
            .map(|(_, expression)| expression_kind_name(&expression.kind))
            .collect::<Vec<_>>();
        kinds.sort_unstable();
        kinds.dedup();
        for expected in [
            "access",
            "block",
            "cstring",
            "deferred.callable",
            "deferred.coroutine",
            "deferred.resource",
            "float",
            "index",
            "integer",
            "logical",
            "loop",
            "match",
            "name",
            "product",
            "repeated-product",
            "satisfies",
            "string",
            "string-template",
        ] {
            assert!(
                kinds.iter().any(|kind| kind == expected),
                "the coverage fixture should lower a `{expected}` expression; have {kinds:?}"
            );
        }
    }

    #[test]
    fn lowered_payloads_agree_with_checked_side_tables() {
        let module = checked_program(complete_coverage_source());
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let formatting = module.string_formatting();
        for (_, expression) in program.expressions.iter() {
            let syntax = expression.key.syntax;
            match &expression.kind {
                LoweredExpressionKind::Integer(integer) => assert_eq!(
                    module
                        .type_of_expression(syntax)
                        .and_then(CheckedType::integer_type),
                    Some(integer.integer_type)
                ),
                LoweredExpressionKind::Float(float) => assert_eq!(
                    module
                        .type_of_expression(syntax)
                        .and_then(CheckedType::float_type),
                    Some(float.float_type)
                ),
                LoweredExpressionKind::Name(name) => assert_eq!(
                    name.requires_initialization_check,
                    module.resolved().requires_initialization_check(syntax)
                ),
                LoweredExpressionKind::Access(access) => {
                    let checked = module
                        .access_for(syntax)
                        .expect("checked access metadata")
                        .clone();
                    let expected = match checked {
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
                    assert_eq!(access.kind, expected);
                }
                LoweredExpressionKind::Logical(logical) => assert_eq!(
                    module.logical_for(syntax).map(|checked| &checked.bool_type),
                    Some(&logical.bool_type)
                ),
                LoweredExpressionKind::Match(match_) => assert_eq!(
                    module.match_for(syntax).map(|checked| &checked.source),
                    Some(&match_.source)
                ),
                LoweredExpressionKind::Index(index) => {
                    assert_eq!(module.trait_dispatch_for(syntax), Some(&index.dispatch))
                }
                LoweredExpressionKind::StringTemplate(template) => {
                    for part in &template.parts {
                        let LoweredStringTemplatePart::Interpolation(interpolation) = part else {
                            continue;
                        };
                        let interpolation_syntax = program
                            .expressions
                            .get(interpolation.expression)
                            .expect("interpolation value")
                            .key
                            .syntax;
                        let checked = formatting
                            .interpolations
                            .get(&interpolation_syntax)
                            .unwrap_or_else(|| {
                                panic!(
                                    "interpolation {} has checked formatting metadata",
                                    interpolation.expression.index()
                                )
                            });
                        assert_eq!(interpolation.trait_id, checked.trait_id);
                        assert_eq!(interpolation.method, checked.method);
                        assert_eq!(interpolation.value_type, checked.value_type);
                    }
                }
                LoweredExpressionKind::Product(product) => {
                    if let Some(CheckedType::Product(checked)) = module.type_of_expression(syntax) {
                        if !checked.variadic {
                            assert_eq!(product.final_type.elements.len(), checked.elements.len());
                        }
                    }
                }
                LoweredExpressionKind::RepeatedProduct(repeated) => {
                    let expected = match module.type_of_expression(syntax) {
                        Some(CheckedType::Product(product)) if !product.variadic => {
                            LoweredRepeatCount::Fixed(product.elements.len())
                        }
                        Some(CheckedType::Array { count, .. }) => {
                            LoweredRepeatCount::Symbolic(count.as_ref().clone())
                        }
                        _ => LoweredRepeatCount::Fixed(1),
                    };
                    assert_eq!(repeated.count, expected);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn validator_rejects_orphaned_arena_nodes() {
        let module = checked_program("let value: I32 = 1\n");
        let mut program = LoweredProgram::default();
        let diagnostics = program.snapshot(&module);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(program.validate().is_empty());

        let key = ExpressionKey {
            syntax: SyntaxId(99_999),
            owner: ExpressionOwner::Module(ModuleId(99_999)),
            context: ExpressionContext::Primary,
        };
        let id = program.expressions.push(LoweredExpression {
            key,
            origin: Origin::compiler(),
            value_type: CheckedType::I32,
            effects: CheckedEffectSet::default(),
            coercion: None,
            moved_symbols: Vec::new(),
            kind: LoweredExpressionKind::Name(LoweredName {
                symbol: SymbolId(0),
                storage: SymbolStorage::ImmutableValue,
                requires_initialization_check: false,
                mutable: false,
                captured_cell: false,
                moved: false,
                move_parameter: false,
                singleton: None,
            }),
        });
        program.expression_lookup.insert(key, id);
        let diagnostics = program.validate();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .message
                .contains("is not reachable from any runtime root")),
            "unexpected diagnostics: {diagnostics:?}"
        );
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
        let validation = program.validate();
        assert!(validation.is_empty(), "{validation:?}");

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
