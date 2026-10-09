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

Stage 5a is done and 5b's core works on `worktree-hybrid-polymorphism` (base `a47627a`). The Goal program and its resource variants run. Left: the effect-row differential mode (E-H), dynamic-to-dynamic row repacking, and the exit gate.

- **Checker fixes (done).**
  - Calls erase the callee's own quantified parameters inside function-typed
    slots, so `apply (a, g)` infers, also through a polymorphic local. The
    cause was not effects. Single-value diagnostics are kept.
  - `refresh_effect_generic_call` re-solves a call's effect parameters once
    lambda effects are inferred, for generic `def`s and polymorphic locals.
    Before it, `twice (() => bump 1)` with a `mut` resource solved
    `E := {}` and crashed (`IO` is zero-sized and hid it). State effects of
    captured cells stay out of the solved row, as before.
- **5a (done).** Type-and-effect generics run in dictionary-passed code;
  effect parameters are compile-time substitutions in the instance key.
- **5b core (done).**
  - `InstanceKey::runtime_effects` is a separate set from `runtime`
    (decision E-C changed: effect ids stay out of `runtime()`, so every
    descriptor-order use is unaffected). `is_dynamic()` replaces
    `runtime().is_empty()` where the ABI is chosen.
  - `CanonicalEffectSet::runtime_variable` is the run-time form. Resolution
    takes a `RuntimeScope` (types plus effects); a polymorphic entry's scope
    is `scheme_runtime_scope`, and a callee effect parameter is run-time when
    its row mentions a caller's run-time variable (E-B).
  - The dynamic signature gains the bundle slot after the fixed resources
    (E-D, flat form). `push_dynamic_row_arguments` builds it at concrete
    callers and at dynamic callers of polymorphic values; a call whose row
    is the caller's own variable forwards the caller's bundle.
  - Row reabstraction thunks (`concrete`↔`dynamic`) take each concrete
    resource from a fixed dynamic slot or the next bundle slot
    (`row_sources`).
  - The "effect-polymorphic functions cannot be used as values yet"
    restriction is gone, and `Staple.md` documents the feature.
  - Corpus: `effect_polymorphic_goal`, `_resources`, `_type_and_effect`.
- **Not done.**
  - Effect-row differential mode (E-H), which also exercises direct
    dynamic-to-dynamic calls whose callee row is run-time.
  - Rows mixing fixed resources with a nested variable, and the
    dynamic-to-dynamic repacking thunk (E-F's third direction).
  - Reachability tests and the exit gate.
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
- **E-C. Canonical run-time effect variables.** `CanonicalEffectSet` gains
  a run-time variable form beside the binder form, mirroring
  `CanonicalType::RuntimeParameter`. An `InstanceKey`'s run-time set may
  hold effect parameter ids. Descriptors stay one per run-time *type*
  parameter in R-A order: effect parameters get no descriptor and no hidden
  argument.
- **E-D. A row's run-time representation.** In the dynamic and polymorphic
  ABIs, a row passes its fixed resources by pointer in the existing resource
  slots (canonical order). If the row has a variable, one *bundle pointer*
  follows them. A bundle is an array of pointers: one per fixed resource of
  the row the variable stands for, in that row's canonical order, then one
  pointer to the nested variable's bundle when that row has a variable too.
  An empty row's bundle is a null pointer. State effects are erased.

  This replaces the main plan's "one resource bundle pointer per effect
  variable" among the hidden arguments. A body can use `E` only to call a
  `->{E}` function, which requires `E` in its own row, so the bundle always
  arrives with the row. Effect parameters that appear only in argument or
  result types need nothing at run time.
- **E-E. Bundles are stack data.** A caller builds a bundle in a fixed-size
  entry-block slot: the instantiated row's shape is known at the call. A
  bundle lives for one call. No closure retains one, because a function's
  resources are supplied on each call. The exception is coroutine spawning,
  which captures resources and stays restricted until Stage 8.
- **E-F. Row reabstraction extends R5.** Two function types whose rows have
  different representations convert through a reabstraction thunk. This
  happens when the expected type has a run-time variable standing for a row
  the actual type spells out, as with `() ->{IO} ()` reaching
  `() ->{E} ()` at `E := {IO}`, or `() ->{E, IO} ()` reaching `() ->{F} ()`
  at `F := {E, IO}`.
  - The thunk unpacks the bundle into the actual ABI's resource arguments,
    loading by-value ones, or packs them the other way.
  - Thunks exist in three directions: concrete↔dynamic as today, and
    dynamic→dynamic for repacking.
  - Keys use the canonical function types, including run-time effect
    variables.
  - Nested function types and by-value aggregates reuse the R5 and
    mixed-layout value conversions, whose `needs_conversion` gains the row
    criterion.
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

### S0 — Measure and fix the checker limitation

- **Coverage report.** Count templates with effect parameters, not just
  those with type parameters, and report them per reason. Record the
  baseline in this plan.
- **Corpus programs.** Add `<T, effect E>` and `<effect E>` generics:
  - with `IO`, a user resource, `mut` resources, an open row `{E, IO}`, an
    empty row, and state effects only;
  - called concretely and from generic bodies;
  - plus one effect-parameterized type, a `wrap` with a `->{E}` callback
    field.

  Default mode must run them all. In the hidden mode they are rejected
  until 5a.
- **Checker fix.** Fix inference for the call above, with a positive test
  and the explicit-argument form as a regression test. If the root cause
  turns out to be outside call inference, land it as its own commit before
  5a.

### 5a.1 — Compile-time rows in dynamic instances

- **Support analysis.** Replace the blanket `EffectVariable` rejection with
  a check that every effect parameter is compile-time under E-B. In 5a that
  always holds except for a polymorphic value whose scheme quantifies an
  effect, which keeps the existing message until 5b.
- **Specialization.** A dynamic instance's key mixes run-time type
  parameters with `Effect` substitutions; `InstanceKey` already allows this.
  Audit body validation, cleanup, and the emitter for rows that mention
  substituted effect parameters.
- **Values.** A polymorphic value built from an effect-generic `def` at a
  scheme that fixes the effect, as in `run` above, substitutes that effect
  in the construction (B2), so the value's entry is an ordinary dynamic
  instance.
- **Tests.** The S0 programs pass in hidden mode, and the coverage report
  has no effect rejections outside quantified schemes. The `apply`/`run`
  shapes from Current state run as polymorphic values.

### 5a.2 — Exit gate for 5a

- `cargo test --workspace --quiet`, the full differential run, and IR
  against the base commit's release build: every existing input `same`.
- Update this plan's Current state.

### 5b.1 — Keys and representation

- **Keys.** Run-time effect variables in `CanonicalEffectSet` (E-C), in
  instance, thunk, conversion, and adapter keys. The run-time set
  distinguishes effect from type parameters wherever descriptors are
  counted (`compile_dynamic_function_type`, R-A ordering, environments).
- **Lowering.** Body validation accepts a run-time effect variable exactly
  when it is in the instance's run-time set, as Decision 1 does for types.
- **Layout.** Descriptor selection erases run-time effect variables (E-G).

### 5b.2 — Bundle ABI

- **Signatures.** The dynamic and polymorphic signatures gain the bundle
  slot after the fixed resources when the row has a variable (E-D).
  Concrete signatures are unchanged.
- **Bodies.** A dynamic body receives its own bundle and forwards it
  unchanged to callees whose row is exactly its variable.
- **Callers.** For a callee row that adds fixed resources around a run-time
  variable, the caller builds a nested bundle (E-E). Concrete callers of a
  run-time effect parameter build a flat bundle from their concrete
  resources.

### 5b.3 — Row reabstraction

- **Planning.** Extend `needs_conversion` and the conversion plans with the
  row criterion (E-F), and record conversions in lowering (R-E) for:
  - concrete callers of polymorphic values;
  - dynamic callers of polymorphic values;
  - dynamic-to-dynamic calls whose callee effect parameter is run-time.
- **Thunks.** Pack and unpack bundles in all three directions, including
  `mut` writeback and results that are effect-polymorphic functions.
- **Composition.** Row conversion composes with the existing type
  conversions in one thunk. A function whose rows and types both differ is
  converted once.

### 5b.4 — Effect-polymorphic values

- **Construction (B2).** `<effect E>` and `<T, effect E>` closures from
  generic `def`s, lambdas checked against such schemes, and constructors.
  Effect-only schemes need no descriptors.
- **Calls (B3).** One bundle per quantified effect is built by the caller,
  per 5b.2.
- **Instantiation and scheme adapters (B4, B5).** Instantiation at a
  concrete or partly fixed row stores the bundle shape in the adapter.
  Scheme adapters cover renaming, fixing, and opening effect parameters.
- **Support analysis.** It treats a quantified effect as supported, so the
  "effect-polymorphic functions cannot be used as values yet" message
  goes away.

### 5b.5 — Restrictions and `Staple.md`

- **Generic functions.** Remove the "an effect parameter in the scheme"
  restriction and state that effect-polymorphic functions can be used as
  values, with an example.
- **Effect parameters paragraph.** Effect parameters can now be run-time
  inside polymorphic values. Rows still never affect a value's
  representation.
- **Restriction table.** Update this plan's table and the 4b plan's.

### 5b.6 — Exit gate

- The Goal program and the S0 programs pass in default mode, the type
  differential mode, and the effect-row differential mode (E-H).
- `cargo test --workspace --quiet` passes, as does the full differential
  run in both variants.
- IR against the release build of the base commit: every existing input
  `same`. New programs may differ only where the base rejected them.
- Reachability tests:
  - an effect-generic `def` called only with concrete rows produces no
    bundle code;
  - an unused effect-polymorphic value produces no thunks.

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
- No existing program changes behavior or IR.

## Risks

- **Thin corpus.** Effect-generic code barely appears in the corpus, so S0's
  programs and the effect-row differential mode carry most of the coverage.
  Write them before the implementation they test.
- **Resource order.** Concrete rows sort resources by type
  (`CheckedEffectSet::canonical`). Bundles and thunks must use the
  instantiated row's canonical order on both sides, never the template's.
  If a resource type mentions a run-time type parameter, its sort position
  may change on instantiation. Audit Stage 4's handling of such resources in
  5b.1 and restrict them if needed.
- **Checker coupling.** S0's fix may expose further inference gaps in
  generic bodies. Programs that hit them use explicit type arguments, and
  each gap gets a test and a note here.
- **Two kinds of run-time parameter.** Code that treats the run-time set as
  "one descriptor each" breaks silently when it holds effect ids. 5b.1 audits
  every use of `runtime()` before adding the first effect id.
