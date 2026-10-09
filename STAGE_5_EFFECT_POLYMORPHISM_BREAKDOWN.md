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

**Stage 5 is complete** on `worktree-hybrid-polymorphism` (base `a47627a`).
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
- **ABI.** A row naming a variable passes one bundle (E-D, `rows.rs`),
  so equal function types share one ABI. Concrete callers, dynamic callers,
  direct dynamic calls, and the two concrete↔dynamic reabstraction thunks
  build or read bundles with the same layout.
- **Tests.** Corpus programs `effect_generic_*` and `effect_polymorphic_*`;
  `stage5_effect_generic_differential`, `stage5_effect_row_differential`
  (effect rows at run time, E-H), a reachability test, and the ignored
  full-corpus run with types and effects at run time
  (`codegen_corpus_effect_row_differential_run_matches_default`). The coverage
  report counts effect-parameter templates (6 in the corpus, none rejected).
- **Exit gate.** `cargo test --workspace`, the full effect-row differential
  run, and IR against the `a47627a` release build (all 11 examples and 121
  existing corpus programs identical).
- **Known limitation.** A `wrap` type with an effect parameter cannot be
  constructed: `type Task{E} = wrap (run: () ->{E} ())` and `Task (run: f)`
  reports the field as `() -> ()`. The corpus uses `alias` types instead.

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
- **E-D. A row's run-time representation.** The representation depends only
  on the row's type, so function types that are equal after substitution
  share one ABI. A row without a variable passes each resource by pointer in
  canonical order. A row that names a variable passes one *bundle* pointer
  instead, defined recursively:
  - with fixed resources `r0..rn`, an array of their addresses followed by
    the representation of the variable;
  - with none, the representation of the variable itself;
  - a variable instantiated at resources `s0..sk` (and possibly a variable of
    the caller's) is an array of their addresses, then the caller's
    representation; it is null when empty, and the caller's own
    representation when that is all there is.

  This replaces the main plan's "one resource bundle pointer per effect
  variable" among the hidden arguments, and the earlier draft of this
  decision (fixed resources beside a bundle), whose representation differed
  between `() ->{F} ()` at `F := {E, IO}` and `() ->{E, IO} ()` and so needed
  a dynamic-to-dynamic repacking thunk.
- **E-E. Bundles are stack data.** A caller builds a bundle in a fixed-size
  entry-block slot: the instantiated row's shape is known at the call. A
  bundle lives for one call. No closure retains one, because a function's
  resources are supplied on each call. The exception is coroutine spawning,
  which captures resources and stays restricted until Stage 8.
- **E-F. Row reabstraction.** A concrete function type reaching a dynamic
  type with a run-time variable converts through the existing reabstraction
  thunks, in both directions. The thunk reads each concrete resource from a
  fixed slot of the bundle or from the variable's representation, or builds
  the bundle from the concrete resources (`rows.rs`). Dynamic function types
  never need a thunk for their rows (E-D).
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
| Trait bound other than `Copy`/`Sized` | "polymorphic values with trait bounds are not supported yet" | Stage 6 |
| Coroutine, async, or reactive body, transitively, including `block_on`, `spawn`, `reaction`, and `batch` | the existing coroutine, async, and reactive messages | Stage 8 |
| Trait member used as a polymorphic value | "trait members cannot be used as polymorphic values yet" | Stage 7 |
| Run-time `Natural` count; function values behind references | unchanged | Not planned |

Effect-generic intrinsics still can't be used as values, like every
intrinsic.

## Intended Differences

- Type-and-effect generics run in dictionary-passed code (5a).
- Effect-polymorphic values compile and run (5b).
- No existing program changes behavior or IR (verified at the exit gate).

## Risks (resolved)

- **Thin corpus.** The new corpus programs and the effect-row differential
  mode carry the coverage, and were written before the code they test.
- **Resource order.** Bundles and thunks use the instantiated row's canonical
  order on both sides (`row_layout`). A resource type cannot mention a type
  parameter (resources must be fully concrete nominal types), so its sort
  position never changes on instantiation.
- **Two kinds of run-time parameter.** `runtime_effects` is a separate set,
  so no use of `runtime()` sees an effect id.
