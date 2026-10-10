# Stage 5 Breakdown: Effect Polymorphism at Run Time

Part of [HYBRID_POLYMORPHISM_PLAN.md](HYBRID_POLYMORPHISM_PLAN.md). Builds
on Stage 4 ([4a](STAGE_4A_REMAINDER_BREAKDOWN.md),
[4b](STAGE_4B_POLYMORPHIC_VALUES_BREAKDOWN.md), and the mixed-layout
follow-up `a47627a`). The Stage 4 decisions (1–11) and the 4a remainder
decisions (R-A to R-F) still apply unless this plan changes them.

The main plan marked this stage "breakdown not required". Surveying the code
showed more work than its three bullets suggest, so this plan replaces them.

## Goal

Effect-polymorphic functions run in dictionary-passed code and can be used
as values. The main plan's exit program works as a stored value and as a
parameter, with resource and empty instantiations:

```staple
use std.io.(IO, println)

def twice: <effect E> (() ->{E} ()) ->{E} () = f => { f (); f () }

let stored: <effect E> (() ->{E} ()) ->{E} () = twice
def run: (<effect E> (() ->{E} ()) ->{E} ()) ->{IO} () = repeat => {
    let mut count = 0
    repeat (() => println "hi")          // E := {IO}
    repeat (() => { count = count + 1 })  // E := {} (state effects are erased)
    println "${count}"
}
run stored
```

The 4b diagnostic "effect-polymorphic functions cannot be used as values
yet" goes away, and `Staple.md` no longer says that effect parameters "have
no runtime representation, and are concrete before code generation".

**Behavior for existing programs is preserved.** A program that uses no
effect-polymorphic value and no type-and-effect generic from a dynamic body
takes no new path. Default-mode IR must match the release build of this
stage's base commit for every existing input.

## Current state

**Stage 5 is complete** on `worktree-hybrid-polymorphism` (base `a47627a`),
including the fixes from the review of `c823212` below.
Effect-polymorphic functions run as stored and passed values, in the Goal
program and with user, `mut`, open-row, empty-row, and state instantiations.

- **Checker.**
  - Calls erase the callee's own quantified parameters inside function-typed
    tuple slots, so `apply (a, g)` infers, also through a polymorphic local. A
    parameter that is itself a function type keeps its parameters, so the
    callback-versus-thunk reading of existing programs is unchanged.
  - `refresh_effect_generic_call` re-solves a call's effect parameters once
    lambda effects are inferred, for generic `def`s and polymorphic locals.
    Before it, `twice (() => bump 1)` with a `mut` resource solved
    `E := {}` and crashed (`IO` is zero-sized and hid it). State effects of
    captured cells stay out of the solved row, as before.
  - A tail call to an effect-generic function from another one keeps the
    argument-determined effect variable in its expected callee type, so
    `def four = f => twice f` checks.
- **Lowering.** Instance keys carry `runtime_effects` beside `runtime`
  (E-C); requests resolve in a `RuntimeScope` of both. Templates with effect
  parameters run in dictionary-passed code; polymorphic effect schemes
  build entries. The support analysis no longer rejects effect variables.
- **ABI.** A row naming a variable passes one bundle whose resources are
  found by key (E-D, `rows.rs`), so equal function types share one ABI.
  Concrete callers, dynamic callers, direct dynamic calls, instantiation and
  scheme adapters, instance prologues, and the two concrete↔dynamic
  reabstraction thunks build or read bundles the same way.
- **Tests.** Corpus programs `effect_generic_*` and `effect_polymorphic_*`;
  `stage5_effect_generic_differential`, `stage5_effect_row_differential`
  (effect rows at run time, E-H), a reachability test, and the ignored
  full-corpus run with types and effects at run time
  (`codegen_corpus_effect_row_differential_run_matches_default`). The coverage
  report counts effect-parameter templates (6 in the corpus, none rejected).
- **Exit gate.** `cargo test --workspace`, the full effect-row differential
  run, and IR against the `a47627a` release build (all 11 examples and 121
  existing corpus programs identical).
- **Review of `c823212`.** The review found three defects that the corpus
  and both differential modes missed, all of which compiled into crashes or
  invalid modules. All are fixed:
  - **Captures in effect-only dynamic closures.** A closure or implicit
    thunk whose row names a run-time effect variable, but which has no
    run-time type parameter, was built with a concrete environment while its
    body read a dynamic one (R-D), so any capture crashed. Construction now
    follows `instance_is_dynamic`, and lowering plans the thunk's dynamic
    finalizer the same way.
  - **Instantiation and scheme adapters.** `emit_instantiation_adapter`
    still forwarded the target's resources one by one. With the empty row or
    two or more resources the module was invalid; with one real resource it
    crashed. A scheme adapter that reorders an effect parameter
    (`<effect G, U>` from `<T, effect E>`) failed the same way. The adapter
    now passes the row through `push_dynamic_row_arguments`, reading a
    dynamic target's own bundle when it has one.
  - **Nested rows.** The positional bundle of the first E-D did not give
    equal function types one ABI. A callee row `{F, Label}` at
    `F := {E, IO}` was nested, `[&Label, [&IO, E]]`, while the closure of
    type `{E, IO, Label}` expected the flat `[&IO, &Label, E]`; the closure
    crashed. No positional layout can work, because a callee must find its
    fixed resources at known positions while canonical order interleaves
    them with the variable's. Bundles now hold `(key, address)` entries and
    code finds resources by key (E-D).

  The review also fixed three problems outside the new ABI:
  - **Implicit thunks.** The source-coverage validator decomposed a
    parenthesized argument such as `(g ())` before checking whether the
    whole argument was an implicit thunk. Thunked arguments to
    effect-generic functions were rejected ("has no lowered counterpart").
    This predates Stage 5; the checker fixes above first let such calls
    check.
  - **Module bindings.** Dynamic bodies resolved names only through local
    and captured bindings, so a polymorphic value that read or assigned a
    module-level `let` failed with "missing or moved dynamic binding". This
    predates Stage 5 (4b). Reads and writes now use the module global, as
    the concrete emitter does.
  - **Crashes under `staple run`.** A program killed by a signal exited
    with a silent status 1. It now reports the signal and exits with
    128 + the signal, as a shell does.

  New corpus programs `effect_polymorphic_captures`,
  `effect_polymorphic_instantiation`, `effect_polymorphic_nested_rows`,
  `effect_generic_thunk_argument`, and `polymorphic_module_bindings` cover
  each case, so both differential modes now reach them.

  Verified after the review fixes: `cargo test --workspace` (1,505 passed,
  3 ignored); the full type differential run (134 programs, 133 with
  dynamic calls, 15,619 call sites); the full effect-row differential run
  (133 programs); and IR against the `c823212` release build over 147
  inputs: 137 identical, the 6 earlier effect-polymorphic programs differ
  only in how bundles are built and read, and the 4 new programs that
  `c823212` rejected do not compile there (`effect_polymorphic_nested_rows`
  compiled there and crashed).
- **Known limitation (withdrawn).** An earlier version listed `wrap` types
  with an effect parameter as unconstructible (`Task (run: f)`). It does not
  reproduce, annotated or not, inside or outside a generic function.

## Decisions

- **E-A. Two substages.** 5a runs type-and-effect generics in dynamic code
  with every row fixed at compile time; it needs no ABI change. 5b adds
  effect parameters that stay unknown until run time: effect-polymorphic
  values and everything they reach. 5a is useful on its own and shrinks
  5b's test surface.
- **E-B. Which effect parameters are run-time.** As Decision 2 does for
  types: inside a dynamic body, a callee's effect parameter is run-time
  exactly when its row mentions a run-time effect variable of the caller.
  Otherwise its row is substituted and keys the instance as today. A
  polymorphic value's own quantified effect parameters are run-time in its
  entry. So in 5a, where no run-time effect variable exists yet, every
  effect parameter is compile-time.

  The alternative of keying a callee by the fixed part of the row plus "some
  run-time variable" avoids building bundles in dynamic-to-dynamic calls,
  but calls through polymorphic values need nested bundles and repacking
  anyway (E-F). One rule is simpler.
- **E-C. Run-time effect variables.** An instance key has a separate
  `runtime_effects` set beside `runtime`, so every use of `runtime()` still
  means "has a descriptor". `CanonicalEffectSet` gains `runtime_variable`.
  Effect parameters get no descriptor and no hidden argument. `is_dynamic()`
  (either set non-empty) selects the dynamic ABI.
- **E-D. A row's run-time representation.** A row without a variable
  passes each resource by address, in canonical order. A row that names a
  variable passes one *bundle* pointer instead. A bundle is a chain of nodes
  `{ next, count, [(key, address)] }`, where a key is the address of a
  per-resource-type global, and code finds a resource by walking the chain
  for its key (`rows.rs`). An empty row's bundle is null.
  - **Equal function types share one ABI.** A bundle stands for the set of
    resources it holds, not for the way the row was written, so
    `() ->{F, Label} ()` at `F := {E, IO}` and `() ->{E, IO, Label} ()`
    receive the same bundle.
  - **Extra resources are harmless.** A caller passes its own bundle on
    unchanged when the call row adds nothing, and otherwise adds one node in
    front holding the call row's fixed resources.
  - **A missed lookup traps.** The checker guarantees it never happens.

  This replaces the main plan's "one resource bundle pointer per effect
  variable" among the hidden arguments, and two earlier drafts of this
  decision. The first passed fixed resources beside a bundle. The second
  was a positional bundle, in which a row with fixed resources nested its
  variable's bundle. Neither gave equal types one ABI, because a callee must
  find its fixed resources at known positions while canonical order
  interleaves them with the variable's (see the review above).
- **E-E. Bundles are stack data.** A caller builds a bundle in a fixed-size
  entry-block slot: the instantiated row's shape is known at the call. A
  bundle lives for one call. No closure retains one, because a function's
  resources are supplied on each call. The exception is coroutine spawning,
  which captures resources and stays restricted until Stage 8.
- **E-F. Row reabstraction.** A concrete function type reaching a dynamic
  type with a run-time variable converts through the existing reabstraction
  thunks, in both directions. The thunk looks each concrete resource up in
  the bundle by key, or builds a one-node bundle from the concrete
  resources. Dynamic function types never need a thunk for their rows (E-D).
- **E-G. Effects never affect layout.** A function value is `{fn*, env*}`
  whatever its row. A type that mentions run-time effect variables but no
  run-time type parameters has the descriptor of the same type with each
  such variable replaced by the empty row. Examples are `() ->{E} ()`,
  `Handler{E}`, and `Coroutine{E} T` passed as an opaque value. No
  descriptor shape changes.
- **E-H. Effect-row differential mode.** The hidden differential option
  gains a variant that makes every effect parameter of a generic call
  run-time. Every effect-generic call then goes through bundles and row
  thunks, and its output is compared with the default mode, the way
  Decision 11 tests type parameters. The corpus has little effect-generic
  code, so 5a and 5b add programs before relying on this mode.

## Steps

All steps are done. In short:

- **S0.** Coverage report for effect templates, corpus programs, and the
  checker fixes above.
- **5a.** Type-and-effect generics in dictionary-passed code; effect
  parameters stay compile-time substitutions (no ABI change).
- **5b.** Run-time effect sets in keys and resolution; the bundle ABI and
  thunk reading; effect-polymorphic values, calls, and instantiation through
  the existing Stage 4b machinery (an effect-only scheme needs no descriptors);
  `Staple.md` and the restriction tables.
- **Exit.** The Goal and corpus programs pass in default, type-differential,
  and effect-row-differential modes; effect-generic `def`s called only with
  concrete rows build no bundles, and an unused effect-polymorphic value
  builds no entry.

## Restrictions

After Stage 5:

| Restriction | Message | Lifted in |
|---|---|---|
| Trait bound other than `Copy`/`Sized` | "polymorphic values with trait bounds are not supported yet" | Lifted in Stage 6 |
| Coroutine, async, or reactive body, transitively, including `block_on`, `spawn`, `reaction`, and `batch` | the existing coroutine, async, and reactive messages | Stage 8 |
| Trait member used as a polymorphic value | "trait members cannot be used as polymorphic values yet" | Stage 7 |
| Run-time `Natural` count; function values behind references | unchanged | Not planned |

Effect-generic intrinsics still can't be used as values, like every
intrinsic.

## Intended Differences

- Type-and-effect generics run in dictionary-passed code (5a).
- Effect-polymorphic values compile and run (5b).
- No existing program changes behavior or IR (verified at the exit gate).

## Risks

- **Thin corpus.** The corpus programs and the effect-row differential mode
  carry the coverage. They missed three defects at `c823212` because no
  program captured in an effect-only dynamic body, instantiated an
  effect-polymorphic value at a function type, reordered an effect
  parameter, or nested fixed resources; the differential modes only reroute
  generic calls, so they cannot reach shapes the corpus lacks. Each of those
  shapes now has a program. New shapes need a program, not just a probe.
- **Resource order.** Bundles no longer depend on order (E-D). Rows without
  a variable still pass resources in the instantiated row's canonical order
  on both sides. A resource type cannot mention a type parameter (resources
  must be fully concrete nominal types), so its sort position never changes
  on instantiation.
- **Two kinds of run-time parameter.** `runtime_effects` is a separate set,
  so no use of `runtime()` sees an effect id. The converse is the trap:
  code that decides "dynamic" from `runtime()` alone misses effect-only
  dynamic instances. Closure construction and thunk planning did, and are
  fixed; decide dynamism with `is_dynamic()` / `instance_is_dynamic`.
