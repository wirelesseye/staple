//! Stage 5.2: the test-only legacy recorder types.
//!
//! The Stage 3.5/4.x transition tests compare legacy discoveries with the
//! lowered catalog. The recorder *state* stays on `ModuleEmitter`; these are
//! just the record shapes, moved here unchanged so `codegen/mod.rs` keeps only
//! the emitter. Production builds compile none of this.

// Shadow comparison consumes census facts; transition tests inspect the rest.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;

use crate::{CheckedFunctionType, CheckedType, FunctionId, ModuleId, SymbolId, TypeParameterId};

/// Stage 3.5 test-only view of the legacy LLVM backend's discoveries: the
/// source-function specialization queue (function, concrete type, recorded
/// substitutions), constructor adapters, and structural methods. Production
/// emission never reads this record.
/// Test-only: one emitted constructor adapter and the decisions its body
/// makes.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct LegacyConstructorAdapter {
    pub(crate) symbol: SymbolId,
    pub(crate) callable_type: CheckedFunctionType,
    /// The concrete result is a managed reference.
    pub(crate) managed_ref: bool,
    /// `build_ref_value` set a payload finalizer.
    pub(crate) finalizer_set: bool,
}

/// Test-only: one nested `trait_method_code` selection inside a structural
/// body: the selected instance template with its concrete method type, or the
/// nested structural kind with its completed arguments.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LegacyStructuralCallee {
    Instance(FunctionId, CheckedFunctionType),
    Structural(crate::StructuralTraitMethod, Vec<CheckedType>),
}

/// Test-only: one emitted structural-method body and the decisions it makes.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct LegacyStructuralMethod {
    pub(crate) structural: crate::StructuralTraitMethod,
    pub(crate) arguments: Vec<CheckedType>,
    pub(crate) function_type: CheckedFunctionType,
    /// `compile_formatter_write_literal` strings in emission order.
    pub(crate) debug_literals: Vec<String>,
    /// Nested trait-method selections in emission order.
    pub(crate) delegates: Vec<LegacyStructuralCallee>,
    /// `DerefIndex`: `Some(true)` took the direct-load fast path; `Some(false)`
    /// delegated; `None` for other kinds.
    pub(crate) deref_index_fast_path: Option<bool>,
    /// `Iterator.next`: the `Done` and `Yield` alternative indices.
    pub(crate) next_alternatives: Option<(usize, usize)>,
    /// `Index`: whether the direct homogeneous load path was taken.
    pub(crate) index_homogeneous: Option<bool>,
    /// `Index`/`MutateIndex`: the target product length.
    pub(crate) index_length: Option<usize>,
    /// `MutateIndex`: whether the replaced element's cleanup ran.
    pub(crate) mutate_drop_previous: Option<bool>,
    /// `IntoIterator`: the source product the body iterated.
    pub(crate) into_iterator_source: Option<CheckedType>,
}

/// Test-only: the cleanup branch one `compile_drop_value` call took.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LegacyDropBranch {
    UserDrop(FunctionId),
    CoroutineCleanup,
    RuntimeRelease(&'static str),
    CStringFree,
    Product,
    Sum,
    Distinct,
    NoOp,
}

/// Test-only: one `compile_drop_value` call with its nested calls in order.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegacyDropCall {
    pub(crate) value_type: CheckedType,
    pub(crate) branch: LegacyDropBranch,
    pub(crate) nested: Vec<LegacyDropCall>,
}

/// Test-only: one finalizer body the legacy backend created.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LegacyFinalizer {
    Payload(CheckedType),
    Cell(CheckedType),
    ClosureEnvironment {
        function: FunctionId,
        capture_types: Vec<CheckedType>,
        dropped: Vec<usize>,
    },
    Buffer(CheckedType),
}

/// Test-only: one legacy ownership registration in `owned_order`.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegacyOwned {
    pub(crate) function: FunctionId,
    pub(crate) function_type: CheckedFunctionType,
    pub(crate) substitutions: HashMap<TypeParameterId, CheckedType>,
    pub(crate) symbol: SymbolId,
    /// A binding cell (owned cell) versus an SSA value.
    pub(crate) cell: bool,
}

/// Test-only: one `compile_buffer_clone` element `Clone` selection.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LegacyBufferClone {
    pub(crate) element: CheckedType,
    pub(crate) function: FunctionId,
}

/// Test-only: one `ensure_coroutine_codes` pair *creation*, with the facts the
/// plan comparison records. Legacy's unwind drops follow `HashMap` order, so
/// the test compares them as a set.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct LegacyCoroutinePair {
    /// The `coro` body syntax the legacy cache key uses.
    pub(crate) body_syntax: staple_syntax::SyntaxId,
    /// The active substitutions at creation.
    pub(crate) substitutions: HashMap<TypeParameterId, CheckedType>,
    /// The substituted frame-binding types in plan order.
    pub(crate) frame_binding_types: Vec<CheckedType>,
    pub(crate) result_type: CheckedType,
    pub(crate) await_result_types: Vec<CheckedType>,
    pub(crate) resume_points: usize,
    pub(crate) wait_await_states: Vec<usize>,
    pub(crate) until_await_states: Vec<usize>,
    /// Each deferred resource's pass mode (`true` = loaded through a pointer).
    pub(crate) resource_slots: Vec<bool>,
    /// Whether the cancel/cleanup path calls the capture-environment
    /// finalizer; legacy gates it on non-empty captures.
    pub(crate) capture_finalizer: bool,
    /// The frame bindings the cancel unwind conditionally drops, as a set.
    pub(crate) unwind_drops: Vec<(SymbolId, CheckedType)>,
}

/// Test-only: one `compile_coro_expression` *request* for a pair, so the test
/// can show where the syntax-keyed cache aliased two instantiations.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct LegacyCoroutineRequest {
    pub(crate) body_syntax: staple_syntax::SyntaxId,
    pub(crate) substitutions: HashMap<TypeParameterId, CheckedType>,
}

/// Test-only: the three reactive runner families.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LegacyRunnerFamily {
    Reaction,
    Until,
    Derived,
}

/// Test-only: one legacy runner creation with its legacy key and call facts.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct LegacyReactiveRunner {
    pub(crate) family: LegacyRunnerFamily,
    /// The call `SyntaxId` for reaction and `until`.
    pub(crate) call_syntax: Option<staple_syntax::SyntaxId>,
    /// The evaluator `FunctionId` for derived.
    pub(crate) evaluator: Option<FunctionId>,
    /// The specialization being emitted, or `None` for module initializers.
    pub(crate) owner: Option<(
        FunctionId,
        CheckedFunctionType,
        HashMap<TypeParameterId, CheckedType>,
    )>,
    pub(crate) callback_type: CheckedFunctionType,
    /// Reaction resource pass modes (`true` = pointer).
    pub(crate) resource_slots: Vec<bool>,
    /// `until` only: the syntax-keyed name already existed, so the runner was
    /// reused across two instantiations.
    pub(crate) name_reused: bool,
}

/// Test-only: which legacy record one emitted function belongs to. Indexed
/// variants point into the matching `LegacyEmissions` vector, whose per-family
/// transition tests compare the record's contents with its plan.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) enum LegacyFunctionOrigin {
    /// A concrete template `declare_functions` emits eagerly.
    Declared {
        function: FunctionId,
        function_type: CheckedFunctionType,
    },
    /// A generic specialization; the substitutions are the ones legacy
    /// queued.
    Specialization {
        function: FunctionId,
        function_type: CheckedFunctionType,
        substitutions: HashMap<TypeParameterId, CheckedType>,
    },
    /// One source module's initializer.
    Initializer(ModuleId),
    /// The executable harness, which Stage 5 regenerates from entry metadata.
    Main,
    /// The UTF-8 validator, a fixed runtime helper.
    Utf8Validator,
    ConstructorAdapter(usize),
    StructuralMethod(usize),
    Finalizer(usize),
    CoroutineResume(usize),
    CoroutineCleanup(usize),
    Runner(usize),
    ExternAdapter(usize),
}

/// Test-only: one eager extern closure adapter and whether `compile_symbol_value`
/// read it as a first-class value.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct LegacyExternAdapter {
    pub(crate) symbol: SymbolId,
    pub(crate) callable_type: CheckedFunctionType,
    pub(crate) used: bool,
}

#[cfg(test)]
pub(crate) struct LegacyEmissions {
    pub(crate) specializations: Vec<(
        FunctionId,
        CheckedFunctionType,
        HashMap<TypeParameterId, CheckedType>,
    )>,
    pub(crate) constructor_adapters: Vec<LegacyConstructorAdapter>,
    pub(crate) structural_methods: Vec<LegacyStructuralMethod>,
    pub(crate) drop_calls: Vec<LegacyDropCall>,
    pub(crate) finalizers: Vec<LegacyFinalizer>,
    pub(crate) owned: Vec<LegacyOwned>,
    pub(crate) buffer_clones: Vec<LegacyBufferClone>,
    pub(crate) coroutine_pairs: Vec<LegacyCoroutinePair>,
    pub(crate) coroutine_requests: Vec<LegacyCoroutineRequest>,
    pub(crate) runners: Vec<LegacyReactiveRunner>,
    pub(crate) extern_adapters: Vec<LegacyExternAdapter>,
    /// Every runtime symbol some emitted function actually references, read
    /// from the finished LLVM module rather than from hand-placed hooks, in
    /// module function order. References from inside the installed runtime
    /// modules, the UTF-8 validator, and the `main` harness (which Stage 5
    /// regenerates from entry metadata) are excluded.
    pub(crate) runtime_surfaces: Vec<String>,
    /// The same references as `(runtime symbol, referencing function)` pairs,
    /// one per referencing function, for per-function comparison. A
    /// non-instruction user has no function and is named by an empty string.
    pub(crate) runtime_references: Vec<(String, String)>,
    /// Every function the emitter defined, excluding the runtime modules, the
    /// UTF-8 validator, and `main`, so a function with no runtime reference
    /// still compares.
    pub(crate) emitted_functions: Vec<String>,
    /// Every function the emitter created, with the legacy record it belongs
    /// to, in creation order.
    pub(crate) defined_functions: Vec<(String, LegacyFunctionOrigin)>,
    /// LLVM function types keyed by final legacy symbol name, for the Stage
    /// 5.3 catalog declaration comparison.
    pub(crate) function_types: HashMap<String, String>,
    /// LLVM linkage keyed by final legacy symbol name (`true` for `Internal`),
    /// for the Stage 5.3 F2 declaration comparison.
    pub(crate) function_linkages: HashMap<String, bool>,
    /// The finished legacy module's IR text, for body-level comparison
    /// (Stage 5.3 Step 4 `main`, Stage 5.3 Step 6 functions).
    pub(crate) module_ir: String,
}
