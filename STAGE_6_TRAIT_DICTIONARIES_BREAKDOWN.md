# Stage 6 Breakdown: Trait Bounds via Dictionaries

Part of [HYBRID_POLYMORPHISM_PLAN.md](HYBRID_POLYMORPHISM_PLAN.md). Builds
on Stage 4 ([4a](STAGE_4A_REMAINDER_BREAKDOWN.md),
[4b](STAGE_4B_POLYMORPHIC_VALUES_BREAKDOWN.md)) and Stage 5
([breakdown](STAGE_5_EFFECT_POLYMORPHISM_BREAKDOWN.md)), at `1106ba8`. The
Stage 4 decisions (1–11), the 4a remainder decisions (R-A to R-F), and the
Stage 5 decisions (E-A to E-H) still apply unless this plan changes them.

## Goal

Code with trait bounds runs in dictionary-passed form, and bounded schemes
can be used as values. The main plan's exit program works as a stored value
and as a parameter, and a bounded value reaches a generic implementation:

```staple
use std.io.(IO, println)

def show: <T where Display T> move T ->{IO} () = move value => println "${value}"
let stored: <T where Display T> move T ->{IO} () = show

def twice: (<T where Display T> move T ->{IO} ()) ->{IO} () = print => {
    print 42                  // a static dictionary for `Display I32`
    print.<String> "text"     // and for `Display String`
}
twice stored

// `ToString T` comes from the blanket `impl<T where Display T> ToString T`.
def describe: <T where Display T> T -> String = value => ToString.to_string value
let described: <T where Display T> T -> String = describe
println (described 7)
```

The 4b diagnostic "polymorphic values with trait bounds are not supported
yet" goes away, except for the restrictions below. `Staple.md` stops saying
that "Traits use static dispatch … runtime dictionaries … are not currently
supported" and that prerequisites "do not add runtime dictionaries".

**Behavior for existing programs is preserved.** Concrete trait calls keep
static dispatch to a monomorphized implementation. A program that uses no
bounded polymorphic value takes no new path, and default-mode IR must match
the release build of this stage's base commit for every existing input.

## Current state

- **S0 done.** The trait fixtures plus the environment, builder, values,
  adapters and captures programs run in default and differential modes.
  Trait-only rejections fell from 1,819 occurrences / 46 templates to 0 outside
  the coroutine and reactive bodies of Stage 8.
- **6a.1–6a.5 done.** Static and built dictionaries (generic and blanket
  implementations in GC memory, structural `Debug`, `Index`, `MutateIndex` and
  iteration over products and sums),
  environment/static/builder/structural recipes selected uniquely at abstract
  types, hidden dictionary arguments in signatures, entries, calls, closure
  environments and adapters, and every T-K operation through slot calls.
- **6b.1 done.** Bounded schemes are values: construction, calls (dictionaries
  after descriptors), instantiation adapters (site-evaluated dictionaries stored
  beside the descriptors) and scheme adapters (entailment through prerequisite
  routes, static dictionaries and built ones). `Staple.md` and the restriction tables are updated.
- **Gates passed.** `cargo test --workspace`; the full type differential run
  and effect-row run over the corpus (including the Stage 6 programs, with
  structural `Index`/`MutateIndex`/iteration over tuples and `Ref` tuples, and
  subsumption through built dictionaries); IR identical to the `1106ba8`
  release build over 136 corpus programs and 11 examples; the coverage report
  lists no trait-only template; reachability tests (a concretely called bounded
  `def` and an unused bounded value build nothing).
- Checker workarounds remain: explicit `USize` indices, projected-result type
  arguments/result annotations, and callback parameter annotations.

## Decisions

- **T-A. Two substages.** 6a runs bounded generic templates in
  dictionary-passed code: dictionaries, their recipes, and every
  trait-dependent operation, tested through the hidden differential mode,
  which then reaches the 1,775 trait-only occurrences. 6b makes bounded
  schemes usable as values on top of 6a, as 4b did on top of 4a.
- **T-B. Which bounds are run-time.** A template's bound is run-time exactly
  when its arguments, after the instance's substitution, mention a run-time
  type parameter (including a projection parameter). Every other bound is
  resolved statically into evidence, as today. Run-time bounds are a
  function of the instance key's run-time set and the template's bounds, so
  instance keys do not change.
- **T-C. Hidden dictionary arguments.** A dynamic instance takes one
  dictionary pointer per run-time bound, after its descriptors. The order is
  the template's declared bound order, with an enclosing function's bounds
  before a nested lambda's own, mirroring R-A. A polymorphic entry takes its
  scheme's bounds in scheme order. As with descriptors (R-B, R-C, R-D),
  dynamic-shape closures take no dictionaries. Their environments store them
  after the descriptors, and descriptor-bound entries forward both.
- **T-D. Dictionary layout.** A dictionary is
  `{ slots: [ptr], prerequisites: [ptr], associated: [ptr], environment }`.
  - **Fixed prefix per trait.** One slot per trait member in declaration
    order, including defaulted members. One dictionary per prerequisite in
    declaration order, including bounds declared on associated types. One
    descriptor per associated type.
  - **Implementation-specific tail.** The `environment` holds the
    implementation's own descriptors and sub-dictionaries. Only that
    implementation's slots read it.
  - **Slot ABI.** A slot has the dynamic ABI with the dictionary in the
    environment position: `(dictionary, result, row…, arguments…)`, every
    value by pointer. A slot is therefore also a valid dynamic-shape closure
    entry, with the dictionary as its environment. That is how a trait
    member is used as a monomorphic value at a run-time type (T-L).
- **T-E. Recipes.** Lowering records, for every run-time obligation at a call,
  operation, closure, adapter, or builder, a *dictionary recipe* (as R-E does
  for conversions):
  - **Environment:** the instance's own hidden dictionary for an equal bound;
  - **Prerequisite:** a prerequisite slot of another recipe, transitively;
  - **Static:** an obligation with no run-time parameter, served by a static
    dictionary artifact keyed by trait, canonical arguments, and
    implementation;
  - **Builder:** a generic implementation selected at abstract types,
    applied to its parameters' descriptors and to recipes for its own
    `where` clause;
  - **Structural:** a structural trait method family (`Debug`, `Index`,
    `MutateIndex`, `IntoIterator`, `Iterator`, and their `Ref` forms) for a
    compound shape, applied to the components' descriptors and recipes.

  Codegen evaluates recipes and never selects implementations.
- **T-F. Selection at abstract types is unique, or rejected.** An environment
  bound wins: it is the caller's evidence, selected at its own types.
  Otherwise the candidates are the implementations whose headers unify with
  the obligation when run-time parameters are unknowns, minus those whose
  `where` clause is definitely unsatisfiable (for example
  `Copy (List A)`). Exactly one candidate must remain, with its `where`
  clause provable from the environment. Otherwise the construction site
  reports a restriction. The rule keeps dictionary passing equal to
  monomorphization: when it succeeds, every instantiation would select the
  same implementation, and an instantiation that could be ambiguous (a
  concrete `impl Clone (List I32)` beside the generic one) is rejected
  rather than silently resolved.
- **T-G. Dictionary lifetime.**
  - Static dictionaries are constants.
  - A built dictionary is allocated in GC memory with its descriptors
    deep-copied, so it may be stored in a closure environment or returned
    without a copy step. (A fixed-size stack slot would need self-describing
    dictionaries to copy on escape; the build cost is paid only where a
    dictionary is built.)
- **T-H. Associated types.**
  - **Unchanged path:** a projection parameter keeps flowing as an R-A
    descriptor.
  - **New source:** when a caller's projection cannot be normalized
    statically, its descriptor comes from the `associated` part of the
    obligation's dictionary.
  - **Normalization at abstract types** follows T-F: a unique implementation
    or a restriction.
- **T-I. Bounds that need no dictionary.**
  - `Copy` and `Sized` use the descriptor (Stage 4).
  - `Drop` behavior is the descriptor's drop glue, and user code cannot call
    `Drop.cleanup`.
  - `Natural` is a sealed marker for compile-time counts and stays under the
    run-time `Natural` restriction.
  - Negative implementations remain compile-time facts.
- **T-J. Slots of concrete and generic implementations.**
  - A static dictionary's slots are reabstraction thunks (R5) from the
    dynamic ABI to the concrete method, keyed by implementation and method.
  - A built dictionary's slots are descriptor-bound entries over its tail. The
    tail holds, per member, the descriptors and dictionaries that member's
    dynamic instance receives, in its ABI order, so an entry forwards them as a
    closure environment does (with a tail offset).
  - A defaulted member's instance receives the dictionary itself as its bound.
  - A member that needs no descriptors stays concrete and is not yet supported
    in built dictionaries.
- **T-K. Trait-dependent operations share one mechanism.** These all carry
  evidence today, and each becomes a slot call through a recipe:
  - method calls and operators (`==`, `<`, `+`);
  - `Index` reads, `MutateIndex` places, and indexed assignment;
  - `for` loops (`IntoIterator`, `Iterator`);
  - string interpolation (`Display`, `Debug`);
  - `Formatter.display`/`debug`.

  `__buffer_clone` gets a dynamic form that calls the element `Clone` slot,
  lifting its 4b restriction.
- **T-L. Trait members as values.** At a run-time type, `ToString.to_string`
  used as a value is a closure of the slot with the (GC-copied) dictionary as
  its environment (T-D). Using a trait member as a *polymorphic* value stays
  Stage 7.
- **T-M. Bounded schemes as values (6b).**
  - **Entries:** a bounded scheme's entry takes T-C's dictionaries.
  - **Calls:** concrete callers pass static dictionaries or built ones.
    Dynamic callers pass recipes.
  - **Instantiation adapters** store their dictionaries beside the
    descriptors, deep-copied.
  - **Scheme adapters** satisfy the source scheme's bounds from the target
    scheme's through recipes. This is the run-time form of the checker's
    bound entailment, for example `Eq T` from `Ord T` through the
    prerequisite slot.
- **T-N. Differential testing.** The hidden differential mode (Decision 11)
  dictionary-passes bounded generic calls once 6a lands. The Stage 5 lesson
  applies: the corpus must contain each shape before the code that handles
  it, because the differential modes only reroute calls the corpus already
  makes.

## Steps

### S0 — Measure and write the corpus first

- **Coverage report.** Report trait reasons per trait and per distinct
  template, not only per program, and record the baseline here.
- **Corpus programs.** Each runs in default mode before 6a; they are what
  the differential mode reroutes. Bounded generics called from generic
  bodies and polymorphic values, over:
  - an explicit concrete implementation, a generic implementation
    (`Eq (List T)`, `Clone (List T)`), and a blanket one (`ToString` over
    `Display`, `Clone T` over `Copy T`);
  - prerequisites (`Ord` used where `Eq` is needed), default methods, and
    overridden defaults;
  - associated types (`Iterator.Item`, `Index.Output`), and a projection
    that only a dictionary can supply;
  - structural `Debug` of `(A, I32)` and of a sum, `Index` and `for` over
    `List A`, interpolation of an `A`, operators on an `A`;
  - a trait member used as a value at a run-time type;
  - a captured dictionary that outlives its frame;
  - a mixed-layout argument whose callback field's type mentions a bounded
    parameter.
- **Restriction probes.** An ambiguous abstract selection (T-F) and a
  `Natural` bound, with the diagnostic each should get.
- **Checker check.** Any program above that the checker rejects is a
  checker gap; fix it here or record it with an explicit-argument
  workaround, as Stage 5's S0 did.

### 6a.1 — Dictionaries and static dictionaries

- **Layout and family.** The T-D layout, and a `trait-dictionary` artifact
  family for static dictionaries keyed by trait, canonical arguments, and
  implementation.
- **Slots.** Slot thunks from the dynamic ABI to concrete methods (T-J).
- **Tests.** A layout test against the trait's member, prerequisite, and
  associated-type order.

### 6a.2 — Hidden dictionary arguments

- **Run-time bounds.** Derive them from the key (T-B), in R-A order (T-C).
- **Signatures.** Add dictionaries to dynamic and polymorphic signatures,
  environments, descriptor-bound entries, and dynamic finalizers.
- **Audit.** Every place that counts hidden arguments from `runtime()` (the
  Stage 5 risk, in a new form).

### 6a.3 — Recipes and selection

- **Recipe kinds.** The five kinds (T-E), recorded per site, validated like
  conversions, and keyed by their canonical form.
- **Selection.** The uniqueness rule at abstract types (T-F), with
  three-valued `where` clauses (definitely true, definitely false, unknown),
  shared with projection normalization (T-H).
- **Diagnostics.** Each new restriction message reported at the
  polymorphic-value construction site.

### 6a.4 — Builders

- **Generic implementations.** Builder artifacts that take the
  implementation's descriptors and sub-dictionaries and fill the `environment`
  and slots.
- **Structural traits.** One builder per trait and compound shape.
- **Shared pieces.** Associated-type descriptors, default-method slots, and
  GC deep copy (T-G).

### 6a.5 — Trait-dependent operations

- **Operations.** Every T-K operation in the dynamic emitter, `__buffer_clone`,
  and trait members as values (T-L).
- **Support analysis.** It accepts `TraitBound` and `TraitCallRuntime`
  when each obligation has a recipe. A missing recipe is a restriction (T-F)
  or a bug.

### 6a.6 — Exit gate for 6a

- **Coverage.** The report shows no trait-only rejections. The in-scope
  baseline is updated, with every remaining trait occurrence explained.
- **Suites.** `cargo test --workspace`, the full type and effect-row
  differential runs, and the quick subset extended with a trait-heavy
  program.
- **IR.** Against the base commit's release build, every existing input
  `same`.

### 6b.1 — Bounded polymorphic values

- **Construction (B2).** Bounded schemes from generic `def`s, lambdas
  checked against them, and constructors.
- **Calls (B3).** From concrete and dynamic callers.
- **Adapters (B4, B5).** Instantiation adapters and scheme adapters with
  bound entailment (T-M), and R5 conversions alongside, unchanged.

### 6b.2 — Restrictions and `Staple.md`

- **Traits and bounded generic functions.** Describe the hybrid strategy:
  static dispatch for concrete uses, dictionaries for polymorphic values.
  Rewrite the "Traits use static dispatch" and the prerequisites
  paragraphs.
- **Generic functions.** Allow bounded polymorphic values, and list the
  remaining restrictions.
- **Restriction tables.** Update this plan's and the 4b plan's.

### 6b.3 — Exit gate

- **Programs.** The Goal and S0 programs pass in default and both
  differential modes.
- **Suites.** `cargo test --workspace`, both full differential runs, and IR
  against the base commit's release build (every existing input `same`).
- **Reachability.** A bounded `def` called only concretely builds no
  dictionary, and an unused bounded polymorphic value builds no entry,
  dictionary, or slot.

## Restrictions

After Stage 6:

| Restriction | Message | Lifted in |
|---|---|---|
| An obligation at run-time types that more than one implementation could satisfy (T-F) | "trait `X` for `Y` is ambiguous at run-time types: candidates …". The checker rejects overlapping implementations, so only an unknown type matching several concrete implementations reaches it | Not planned: it is the price of agreeing with monomorphization |
| An obligation no dictionary can satisfy: no bound in scope and no unique implementation | "a trait obligation of this polymorphic value's implementation has no dictionary at run time …" at the construction site | Not planned |
| `Natural` bound or run-time `Natural` count | "array lengths cannot depend on a polymorphic value's type parameters yet" | Not planned |
| Trait member used as a polymorphic value; independently generic trait members | "trait members cannot be used as polymorphic values yet" | Stage 7 |
| Coroutine, async, or reactive body, transitively | the existing messages | Stage 8 |
| Function values behind references | unchanged | Not planned |

## Intended Differences

- Bounded generic templates run in dictionary-passed code (6a).
- Bounded schemes can be used as values (6b).
- `__buffer_clone` no longer restricts polymorphic values.
- No existing program changes behavior or IR: the IR of 145 corpus programs and
  examples is identical to the `1106ba8` release build.

## Risks

- **Coherence.** Dictionary passing must select what monomorphization would.
  T-F's uniqueness rule is the safeguard; its three-valued `where` check must
  never call "unknown" true. Test it with overlapping generic and concrete
  implementations.
- **Hidden-argument counting.** Code that derives hidden arguments from
  `runtime()` alone will miss dictionaries, the way Stage 5 code that used
  `runtime()` missed effect-only instances. 6a.2 audits every such site
  before the first dictionary parameter exists.
- **Lifetime.** A built dictionary in an entry-block slot must not escape.
  Every capture path (closures, adapters, implicit thunks, dictionaries
  inside dictionaries) needs a test that outlives the frame.
- **Corpus blind spots.** Stage 5's review found three defects in shapes the
  corpus never exercised. S0 writes a program for every T-K operation and
  recipe kind before 6a starts.
- **Checker coupling.** Bounded code in generic bodies exercises inference
  paths that monomorphization never stressed. S0 surfaces them early.
