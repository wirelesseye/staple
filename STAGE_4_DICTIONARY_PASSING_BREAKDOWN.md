# Stage 4 Breakdown: Core Dictionary Passing

Part of [HYBRID_POLYMORPHISM_PLAN.md](HYBRID_POLYMORPHISM_PLAN.md). Builds on
Stage 3 (`50565d8`).

## Goal

Polymorphic function values run. Every Goal program from Stage 3 compiles
and prints the expected output:

```staple
def identity: <T> move T -> T = move value => value

def consume: (<T> move T -> T) -> (I32, String) =
    f => (f 42, f "hello")

type Functions = wrap (identity: <T> move T -> T,)

let mut selected: <T> move T -> T = identity
let fs = Functions (identity: selected)
let pair = consume fs.identity
```

The work splits into two substages that each end with the full suite
passing:

- **4a — Dynamic implementations.** Run-time type descriptors, instances
  whose type parameters arrive at run time, their ABI and dynamic layout,
  and reabstraction between concrete and dynamic function values. Nothing
  changes for users: a hidden lowering option sends ordinary generic calls
  through these implementations so the whole existing corpus can test them.
- **4b — Polymorphic values.** Building closures from `Forall` values,
  calling through them, scheme and instantiation adapters, the narrowed
  lowering gate, and `Staple.md`.

**Behavior for existing programs is preserved.** Polymorphic values were
rejected before this stage, so no existing program uses the new paths.
Later stages compare default IR with `scripts/compare-llvm-ir.py` against the
release reference tagged **`unpacked-products-ir-baseline`** (the final U5
commit). Its preserved binary is `target/ir-baselines/unpacked-products/staple`;
`commit.txt` beside it records the exact commit. This supersedes `50565d8` after
the intentional layout changes in [Unpacked Products](UNPACKED_PRODUCTS_PLAN.md).
The unpacking behavior gate matches the pre-U1 release on all 119 runnable
corpus programs and examples.

## Scope

**In scope:**
- schemes over unbounded type parameters;
- marker bounds that need no dictionary: `Copy` and `Sized` (Decision 10);
- type parameters at any position by value, in products, sums, arrays with
  a concrete count, wrappers, function types, and behind `Ref`, `Slice`,
  and `Buffer`.

**Out of scope, each with a targeted diagnostic at the polymorphic-value
site (see [Restrictions](#restrictions)):**
- other trait bounds (Stage 6);
- effect variables (Stage 5);
- coroutine, async, and reactive bodies (Decision 5 of the main plan,
  lifted in Stage 8);
- run-time `Natural` count parameters;
- the intrinsics listed as unsupported in Decision 9;
- function values behind references, slices, buffers, or shared cells,
  where they would need reabstraction (Decision 7).

## Current Shape

- **Instances are concrete.** `InstanceKey` (`specialization.rs`) is a
  `FunctionId`, a concrete substitution, and evidence. Specialization
  substitutes every parameter in the body, and validation rejects any
  instance that still contains one.
- **Artifacts.** Each generated artifact family is a variant of
  `ArtifactRequestKey` and `LoweredArtifactPlan`: constructor adapters,
  structural methods, drop glue, GC finalizers, coroutine pairs, reactive
  runners, and extern adapters. They are requested through the worklist
  (`lower/worklist.rs`) and the artifact closure
  (`lower/artifact_closure.rs`), so only reachable ones are emitted.
- **Callables.** A callable value is `LoweredCallableValue` with a
  `LoweredCallableTarget` (direct function, indirect closure, extern,
  intrinsic, constructor) and a `LoweredCallableAdapter`
  (`None`, `Constructor`, `External`, `NestedClosure`).
- **Codegen is type-directed.**
  - `LayoutContext::compile_type` (`codegen/layout.rs`) maps every concrete
    `CheckedType` to an LLVM type and rejects `Parameter` and `Forall`.
    Values are SSA values of those types.
  - `compile_closure_function_type` (`codegen/abi.rs`) builds a closure's
    signature: the environment, then effect resources (by pointer when `mut`
    or non-`Copy`), then flattened value parameters (by pointer when
    mutated or indirect). A whole-product mutation passes one pointer.
- **Layout.**
  - Products use natural struct alignment: align each field's previous end
    to its ABI alignment, and round total size/stride to the largest field
    alignment. Field order is unchanged.
  - A sum has an `i32` tag and carrier-aligned payload. The carrier starts as
    `i8` and changes only for an alternative with strictly larger ABI alignment;
    ties keep the first carrier. Its length is
    `n = ceil(max(1, max store size) / max(1, carrier store size))`. Payload
    offset rounds tag size up to carrier alignment; total size rounds up to
    the maximum of tag and carrier alignment. Scalar carriers use
    `[n x carrier]`; aggregate carriers use
    `{ [0 x carrier], [n * carrier stride x i8] }` and expose every
    payload byte explicitly, so LLVM cannot discard another alternative's data
    in padding during SSA transport.
  - `Ref`, `Buffer`, `CPointer`, closures, and coroutine handles are
    pointers, and `Slice` is a fixed struct, whatever their type arguments.
- **Drop glue** is expanded inline per concrete type from `DropGluePlan`
  (`codegen/lowered/mod.rs`, `emit_drop_glue`). GC allocations carry a
  per-allocation finalizer pointer in the GC header (`gc.ll`).
- **Stack slots** all come from `entry_builder()` (`codegen/ir.rs`).
- **Stage 3's lowering gate** (`TypedModule::polymorphic_value_diagnostics`)
  stops lowering at every reachable polymorphic value.
- **Generic intrinsics** (23): `__slice_*` (3), `__buffer_*` (9),
  `__completion*`, `__task_*`, and `__resolver_*` (6), pointer conversions
  (4), and `__ref_replace`.

## Decisions

Breakdown-level decisions, in addition to the main plan's. Decisions 10 and
11 change the main plan's text; see [Main plan changes](#main-plan-changes).

1. **Dynamic instances keep run-time parameters as parameters.** An instance
   key gains `runtime: BTreeSet<TypeParameterId>`. Specialization
   substitutes every other parameter as today, and leaves each run-time
   parameter as `CheckedType::Parameter` in the body. Validation accepts a
   parameter exactly when it is in the instance's run-time set. Concrete
   instances have an empty set, so their keys, names, and IR are unchanged.
2. **Which callee parameters are run-time.** Inside a dynamic body, a
   callee's type parameter is run-time exactly when its argument mentions a
   run-time parameter of the caller. Other arguments are substituted
   concretely, so `pair.<A, I32>` keys `pair` with `X` at run time and
   `Y := I32`. Calls whose arguments are all concrete are monomorphized, as
   the main plan's strategy table requires. This makes keys canonical: one
   call site always produces the same key.
3. **Dynamic values live in memory.** A type is *dynamic* when it mentions a
   run-time parameter. Codegen represents a value of dynamic type as a
   pointer to its storage and never as an SSA aggregate:
   - copy and move are `memcpy` of the descriptor's store size;
   - drop calls the descriptor's drop function;
   - field, alternative, and element access is pointer arithmetic from
     dynamic offsets (Decision 5).

   Values of concrete type inside a dynamic body are unchanged. Pointer-like
   types (`Ref T`, `Buffer T`, `Slice T`, closures) have static layout even
   when dynamic, so only their element operations become dynamic.
4. **Descriptor layout.** A descriptor is
   `{ store_size: usize, stride: usize, align: usize, flags: u32,
   drop: ptr, count: u32, components: [count x ptr] }`.
   - `flags` bit 0 is "has no drop", which every `Copy` type sets.
   - `drop` has the signature `(descriptor*, value*) -> ()` and may be null.
     It receives its own descriptor so a compound type's drop can reach its
     components.
   - `components` holds the descriptors a compound type was built from.
   - Concrete types get static constants. A dynamic compound type gets a
     descriptor built at run time into a fixed-size entry-block slot (its
     size depends only on the shape).
5. **Dynamic layout mirrors the concrete rules exactly**, so a value built
   in dynamic code is bit-compatible with the same value built
   concretely:
   - products: each offset is the previous end rounded up to the field's
     descriptor alignment; alignment is the maximum field alignment (at least
     1), and total store size/stride round up to it;
   - sums: carrier and payload length follow `compile_sum_type`'s rule
     (an initial byte carrier upgraded only for strictly larger alignment,
     length rounded up by carrier store size with a minimum divisor/size of 1,
     elements of carrier `stride`). Aggregate carriers expose every payload
     byte explicitly rather than losing data in padding.
     The payload offset rounds tag size up to carrier alignment; total size
     rounds up to the maximum of tag and carrier alignment;
   - arrays: `count * stride`;
   - wrappers: their representation.

   A test checks the descriptor of every concrete type in the corpus
   against LLVM's `TargetData` (Step 4a.2).
6. **Three function ABIs.**
   - *Concrete* (unchanged): a fully concrete function type.
   - *Dynamic*: a function type that mentions run-time parameters. Its
     signature is the environment, then the result out-pointer, then one
     pointer per effect resource, then one pointer per flattened value
     parameter. Parameter products can't occur, because Stage 3 forbids
     capable parameters in schemes used as values, so flattening depends
     only on the written shape.
   - *Polymorphic*: a `Forall` value's entry. Like dynamic, with one
     descriptor per scheme type parameter, in scheme order, after the
     environment.

   Passing every value by pointer, even concrete ones, makes the abstract
   shape depend only on the function's written structure. Adapter keys
   include that symbolic shape as well as the concrete type. Ownership follows
   the existing modes: a `move` parameter transfers the pointed-to value (the caller
   does not drop it), a borrowed one stays the caller's, and a `mut` one is
   updated in place.
7. **Reabstraction at concrete/dynamic boundaries.** When a concrete
   instance calls a dynamic implementation, every argument and result whose
   dynamic type contains a function type is converted:
   - a function value converts through a generated reabstraction thunk,
     keyed by the concrete function type, symbolic dynamic function type,
     and direction, that captures the original closure. Nested function
     types convert recursively;
   - a by-value product, sum, array, or wrapper converts element by element
     into new storage through a generated conversion function, keyed by
     `(concrete type, symbolic dynamic type, direction)`;
   - a function value reached through `Ref`, `Slice`, `Buffer`, or a shared
     capture cell can't be converted without copying shared storage. That
     is rejected with a diagnostic (see [Restrictions](#restrictions)).
8. **GC allocations of dynamic payloads.** `Ref`, `Buffer`, and closure
   environment allocations made by dynamic code append one descriptor word
   after the concrete layout and install a dynamic finalizer that reads it.
   - Concrete code never reads past the concrete layout, so values can flow
     both ways: a `Buffer I32` built concretely keeps its concrete
     finalizer when dynamic code at `T := I32` pushes to it.
   - Closure environments built by dynamic code also store the descriptors
     their body needs, ahead of the captures, so a nested dynamic lambda
     receives its enclosing run-time parameters through its environment.
9. **Intrinsics with run-time type arguments.** Supported, implemented
   from descriptor sizes: `__ref_replace`, `__slice_length`,
   `__slice_get_ref`, `__buffer_with_capacity`, `__buffer_length`,
   `__buffer_capacity`, `__buffer_push`, `__buffer_pop`, `__buffer_get`,
   `__buffer_freeze`, `__buffer_transfer`, and the four pointer
   conversions. Unsupported in Stage 4: `__buffer_clone` (needs a `Clone`
   dictionary, Stage 6), `__slice_from_ref` (run-time count), and the task,
   completion, and resolver intrinsics (async, Stage 8). An unsupported
   intrinsic is a transitive restriction reported at the polymorphic-value
   site, like Decision 5.
10. **`Copy` and `Sized` bounds join Stage 4.** Neither needs a dictionary:
    a run-time type is always sized, and `Copy` only promises that copying
    is a `memcpy` and that there is no drop. Supporting them here lets the
    differential mode (Decision 11) cover the many standard-library
    generics bounded by `Copy`. The main plan puts `Copy` in Stage 6.
    `Drop` stays there: it is covered by descriptor drop glue and is never
    written as a bound.
11. **Differential testing mode.** A hidden `Lowerer` option,
    `dictionary_pass_generic_calls`, lowers every direct call of a generic
    `def` from a concrete instance through a dynamic implementation with all
    of the callee's parameters at run time. Calls that a restriction
    excludes fall back to monomorphization. Because existing programs make
    heavy use of generics, running the whole corpus and every example in
    both modes and comparing output tests 4a long before 4b exists. The
    option is never exposed through the CLI.

## Stage 4a — Dynamic implementations

Stage 4a's exit gate and resolved review problems are recorded in
[STAGE_4A_REMAINDER_BREAKDOWN.md](STAGE_4A_REMAINDER_BREAKDOWN.md).

### Implementation status

**Stage 4a is complete.** Dynamic implementations support ordinary implicit
thunks, managed references, concrete structural dispatch and mutation, generic
constructor values, recursive function reabstraction in by-value aggregates,
and runtime captures with owned descriptor graphs. Concrete child closures
receive native environments; shared cells retain initialization state.
Concrete indexing, spreads, defaults, string patterns, propagating/asserting
bindings, and nested generic declarations have dynamic emission. Both emitters
share String/CString kernels, and known dynamic storage loads use natural
alignment. Runtime-only generic byte copies retain a conservative alignment
bound. GC payload pointer words, including trailing descriptors, are aligned,
and scanning advances by one pointer word.
The explicit later-stage restrictions and remaining rejection guards are audited.

The complete workspace suite, zero-baseline coverage, and full differential
execution pass. Before unpacking, default IR matched the `50565d8` release
build for 109 of 112 inputs; the three differences are intended fixes of
programs `50565d8` miscompiles (a local recursive generic `def`, and the phantom-parameter case).
Detailed final results, and the problems the review of `89de0eb` found and
fixed, live in the remainder plan's
[Current state](STAGE_4A_REMAINDER_BREAKDOWN.md#current-state).

**Decision R-F. The dynamic emitter stays separate.** It is too large to fold
into the concrete emitter at this point. Consistency comes from three rules:

- semantic decisions (cleanup order, drop flags, coercions, pattern tests)
  come only from lowered records, which both emitters read;
- both the dynamic emitter's and the support scanner's matches over lowered
  kinds are exhaustive, so a new form must be classified in both;
- the differential run compares the two emitters' behavior on the whole
  corpus.

Status is kept in the remainder plan's "Current state" section, rewritten
in place, not appended to. Validation details live in commit messages, and
commit references must be reachable from the branch.

### Step 4a.1 — Keys, specialization, and validation

- Add the `runtime` set to `InstanceKey` (Decision 1), with stable name
  planning: dynamic instances get a distinct suffix, and concrete names are
  unchanged.
- Specialization substitutes only non-run-time parameters. Relax
  `validate_checked_module`, graph validation, and artifact validation to
  accept run-time parameters inside dynamic instances only.
- Worklist: a dynamic instance's body discovers callees with Decision 2.
  Instances and artifacts stay demand-driven through the existing closure.
- Implement Decision 11's option in the worklist's call resolution, and the
  transitive restriction analysis that decides fallbacks. The same analysis
  produces 4b's diagnostics.

### Step 4a.2 — Descriptors

- New artifact families: `type-descriptor` (a static constant, keyed by the
  concrete `CanonicalType`) and `descriptor-drop` (a drop function wrapping
  the existing drop glue for one concrete type). Both are emitted only when
  requested.
- A dynamic descriptor builder per compound shape (product, sum, array,
  wrapper), as a generated function taking component descriptors and
  filling a caller-provided slot.
- Layout tests: for every concrete type in the corpus, the static
  descriptor's size, stride, and alignment equal LLVM's `TargetData`
  values, and the sum payload offset matches `compile_sum_type`.

### Step 4a.3 — Dynamic codegen

- `LayoutContext` reports whether a type is dynamic. `compile_type` keeps
  rejecting dynamic types, so a missed path fails loudly instead of
  emitting a wrong layout.
- Dynamic value operations (Decision 3) for every lowered expression form
  that can carry a dynamic value: bindings, moves, copies, products, sums
  and `match`, arrays, wrappers, field access, calls, returns, and cleanup.
- Dynamic stack slots: one entry-block `alloca` per slot, sized from
  descriptors computed at entry (main plan Decision 6). Descriptor slots
  for built compound descriptors are also allocated and filled at entry.
- Cleanup: lowering's cleanup decisions for a dynamic type always emit a
  descriptor drop call, with a null check, because whether the type has a
  drop is only known at run time.
- Dynamic slots need no GC registration: the collector scans the whole
  stack region (`__staple_gc_scan_region` in `gc.ll`), and dynamic slots
  are ordinary entry-frame allocas.

### Step 4a.4 — Dynamic ABI and calls

- Emit dynamic instances with the dynamic or polymorphic signature
  (Decision 6). A dynamic instance requested by Decision 11 uses the
  polymorphic signature, with descriptors for its run-time parameters.
- Call sequences: from a concrete instance into a dynamic implementation
  (static descriptors, spilled arguments, out-pointer, conversion of results
  back), and from a dynamic body into another dynamic implementation
  (descriptors passed through or built), or into a concrete one.
- Closures whose function type is dynamic use the dynamic ABI. Indirect
  calls choose the ABI from the callee value's type.

### Step 4a.5 — Reabstraction

- New artifact families `reabstraction-thunk` and `value-conversion`,
  keyed by `(concrete type, symbolic dynamic type, direction)` (Decision 7).
- Insert conversions at concrete-to-dynamic call boundaries for arguments
  and results, including nested function types and by-value aggregates.
- Report the restriction for function values behind references.

### Step 4a.6 — GC payloads and intrinsics

- Trailing descriptor words, the dynamic finalizer, and dynamic closure
  environments (Decision 8).
- Supported intrinsics from Decision 9, and the restriction for the rest.

### Step 4a.7 — Exit gate for 4a

- `cargo test --workspace --quiet` passes.
- **Differential run.** A CLI test runs every `MustRun` corpus program and
  every runnable example in both modes and requires identical stdout and
  exit status. It also prints, per program, how many calls the option sent
  through dynamic implementations; that count is non-zero for most
  programs.
- **IR comparison** without the option: every program `same` against the
  `unpacked-products-ir-baseline` release build for later work.

## Stage 4b — Polymorphic values

Stage 4b is planned in
[STAGE_4B_POLYMORPHIC_VALUES_BREAKDOWN.md](STAGE_4B_POLYMORPHIC_VALUES_BREAKDOWN.md),
which replaces Steps 4b.1–4b.5 below with decisions B-A to B-J and steps
B1–B7. The steps below are kept as the original outline.

### Step 4b.1 — Constructing polymorphic closures

- A generic `def` used as a value builds `{polymorphic entry, null env}`.
  The entry is the def's dynamic instance with every scheme parameter at
  run time, keyed with the enclosing concrete substitution.
- A lambda checked against a scheme (`TypedModule::scheme_of_function`)
  builds a closure whose entry is its dynamic instance. Captures may be
  dynamic when the lambda is inside a dynamic body (Decision 8).
- Values that hold or pass a `Forall` (fields, `let mut`, products,
  arguments, results, top-level `let` initializers) need nothing new: a
  polymorphic closure is an ordinary closure value.

### Step 4b.2 — Using polymorphic values

- **Calls through a `Forall` value**, from concrete and dynamic code: pass
  one descriptor per scheme parameter (static, passed through, or built),
  spill the arguments, and read the result from an out slot. No adapter.
- **Instantiation adapters.** A `Forall` value checked against a
  non-`Forall` type (Stage 3 rule 2), or given explicit `.<…>` arguments
  without being applied, becomes a closure in the target's ABI. The
  adapter's environment holds the polymorphic closure and the descriptors.
  New family `instantiation-adapter`, keyed by
  `(scheme shape, target function type)`.
- **Scheme adapters.** Subsumption between two schemes needs no code when
  the value's parameters map one-to-one, in order, onto the target's.
  Otherwise, for example a reordered or compound instantiation, a
  `scheme-adapter` receives the target's descriptors, builds the value's
  descriptors from them, and forwards the call.
- A generic `def` instantiated to a concrete type is still monomorphized,
  exactly as today.

### Step 4b.3 — The lowering gate becomes targeted

- Replace Stage 3's blanket gate with the restriction analysis from
  Step 4a.1, which reports each [restriction](#restrictions) at the
  polymorphic-value site with its own message.
- Keep "polymorphic function values are not supported yet" only as an
  internal-invariant fallback, and add a test that no supported program
  reaches it.

### Step 4b.4 — `Staple.md`

As listed in the main plan's Stage 4:
- rewrite "Generic functions" around `Forall` types: polymorphic values,
  scope, instantiation, shallow scheme matching, mutable storage with a
  fixed scheme, and the restrictions below, with bounded (other than
  `Copy` and `Sized`) and effect-polymorphic values noted as not yet
  supported;
- extend the `.<…>` rules to `Forall`-typed values, with parameter order
  from the scheme;
- update "Where a parameter product may appear" and the capable-parameter
  rules for nested schemes.

### Step 4b.5 — Exit gate for 4b

- `cargo test --workspace --quiet` passes, including the differential run.
- **Goal programs run.** Each Goal program, and one program per value
  position (binding, field, parameter, result, explicit type argument,
  `let mut`), is a corpus entry with `expected_stdout`, run from a concrete
  context and from inside a dynamic body.
- **Reachability**, asserted on lowered artifact families:
  - a generic `def` called only concretely has no dynamic instance;
  - a polymorphic value constructed only in an unreachable function yields
    no dynamic instance, descriptor, or adapter;
  - `let concrete: I32 -> I32 = identity` monomorphizes and builds no
    adapter.
- **IR comparison**: every existing program `same` against the
  `unpacked-products-ir-baseline` release build.

## Restrictions

Each restriction has a positive test (the same function works when called
directly) and a negative test (the polymorphic-value use is rejected).

| Restriction | Message at the polymorphic-value site | Lifted in |
|---|---|---|
| Trait bound other than `Copy`/`Sized` | "polymorphic values with trait bounds are not supported yet" | Stage 6 |
| Coroutine, async, or reactive body, transitively | "functions with coroutine bodies cannot be used as polymorphic values yet" (and the reactive form) | Stage 8 |
| Unsupported intrinsic (Decision 9), transitively | "`{intrinsic}` cannot be used in a polymorphic value's implementation yet" | Stage 6 / 8 |
| Run-time `Natural` count | "array lengths cannot depend on a polymorphic value's type parameters yet" | Not planned |
| Function value behind `Ref`, `Slice`, `Buffer`, or a shared cell crossing into a dynamic implementation | "function values inside references cannot be passed to a polymorphic value's implementation" | Not planned |

## Intended Differences

- Programs that use polymorphic values now compile and run, except where a
  restriction applies.
- Stage 3's "polymorphic function values are not supported yet" is replaced
  by the targeted messages above.
- `Staple.md` describes polymorphic values.
- No existing program changes: the IR comparison must report `same` for all
  of them.

## Main plan changes

Made in the same commit as this breakdown:
- Stage 4 links here and records the split: 4a now includes reabstraction
  between concrete and dynamic function values, because Decision 11's
  differential mode needs it before polymorphic closures exist.
- `Copy` and `Sized` bounds move from Stage 6 to Stage 4 (Decision 10).

## Risks

- **Layout divergence.** If a dynamic layout differs from LLVM's concrete
  layout in any case, values corrupt silently at the boundary. Decision 5's
  descriptor-versus-`TargetData` test and the differential run are the
  guards. Sums with mixed alignments and nested sums need explicit corpus
  programs.
- **Missed dynamic paths in codegen.** Every expression form that can carry
  a dynamic value needs a dynamic path. `compile_type` rejecting dynamic
  types turns a missed path into a compile error instead of wrong code, and
  the differential run exercises the forms the corpus uses.
- **Reabstraction completeness.** A function type nested where no
  conversion is inserted would be called with the wrong ABI and crash. The
  conversion pass works from the dynamic type of every boundary value, so
  coverage is structural, not case by case. The differential run includes
  `map`-style higher-order generics.
- **Drop and ownership.** Dynamic drops are decided at run time. A missed
  drop flag on a moved dynamic value double-drops. Corpus programs with
  non-`Copy` payloads (strings, `Ref`s, `Drop` types) must run in the
  differential mode, and GC finalizer counts should match between modes
  where the corpus already observes them.
- **Size of 4a.** Codegen's type-directed emission is the largest change.
  Steps 4a.3 and 4a.4 should land incrementally behind Decision 11's
  option, one expression family at a time, with the differential run as
  the progress measure (programs that fall back are counted, not failed).
- **Performance of the differential run.** Running every program twice
  roughly doubles the CLI suite time. If it becomes too slow, run the full
  differential set in a dedicated test and a representative subset in the
  default suite.
