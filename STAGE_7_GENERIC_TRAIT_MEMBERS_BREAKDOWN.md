# Stage 7 Breakdown: Independently Generic Trait Members

Part of [HYBRID_POLYMORPHISM_PLAN.md](HYBRID_POLYMORPHISM_PLAN.md). Builds
on Stage 6 ([breakdown](STAGE_6_TRAIT_DICTIONARIES_BREAKDOWN.md)) at
`6b32a64`. The Stage 4 decisions (1–11), the 4a remainder decisions (R-A to
R-F), the Stage 5 decisions (E-A to E-H), and the Stage 6 decisions (T-A to
T-N) still apply unless this plan changes them.

## Goal

A trait member may quantify over parameters of its own. Such a member works
through static dispatch, through a bounded polymorphic value, and as a
polymorphic value itself. Every trait member, generic or not, can be used as a
polymorphic value:

```staple
use std.io.println

trait Visit T {
    // `A` belongs to the member, not the trait.
    visit: <A> (T, move A) -> A
}
impl Visit I32 { visit = (_, move value) => value }

// Concrete calls specialize both trait and member arguments.
println "${Visit.visit.<_, I32> (1, 42)}"

// A stored bounded function reads Visit's dictionary slot.
def relay: <T, A where Visit T> (T, move A) -> A =
    (item, move value) => Visit.visit (item, value)
let stored = relay
println (stored (1, "dynamic"))

// The member itself is a polymorphic value, with trait parameters first.
let member: <T, A where Visit T> (T, move A) -> A = Visit.visit
println (member (1, "member"))
let to_text: <T where ToString T> T -> String = ToString.to_string
println (to_text 7)
```

The static, dictionary, and value fixtures pin equivalent runnable examples,
including generic defaults, member bounds, effects, and nested schemes.

The resolver diagnostic "trait members cannot introduce their own
compile-time parameters yet" and the checker diagnostic "generic function
types are not supported in trait members yet" go away. So does the 6b
restriction "trait members cannot be used as polymorphic values yet".
`Staple.md` stops saying that "an individual trait member may not
independently introduce parameters".

**Behavior for existing programs is preserved.** One exception is deliberate:
explicit `.<…>` arguments on a member of a trait with associated types no
longer include the hidden associated-type parameters (Decision 7 of the main
plan). No existing program or test writes such arguments (S0 confirms this).
Default-mode IR must match the release build of this stage's base commit for
every existing input.

## Current state

7a and 7b.1–7b.3 are implemented. Stage 7 fixtures pass both differential
modes; checker regressions also cover phantom arguments, member bounds on
completed associated types, and direct-call projection handling. Schemes,
conformance, parameter order, and runtime dispatch are documented. The final
workspace run, both full differential suites, and corpus IR comparison remain
in progress; all 11 example IR comparisons pass.

## Decisions

- **G-A. Two substages.** 7a is checker plus static dispatch: member
  schemes, conformance, explicit arguments, and monomorphized calls. Nothing
  new runs in dictionary-passed code, except where the hidden differential
  mode reroutes ordinary generic calls. 7b adds dictionary slots for generic
  members and trait members as polymorphic values, as 4b and 6b did on top of
  4a and 6a.
- **G-B. The member scheme.** Every trait member gets a checked scheme:
  - **Parameters:** the trait's explicit parameters, then the member's own
    (type and effect parameters, with the member's `?Sized` relaxations).
    Hidden associated-type parameters are not scheme parameters. Each
    occurrence becomes the projection `Trait.Name parameters`, normalized
    as in `ad21973`.
  - **Bounds:** the trait bound `Trait parameters` first, then the member's
    own bounds in declared order. A member bound may mention trait
    parameters (`<B where Convert T B>`).
  - **Body:** the member's function type.

  `symbol_types`-style lookup returns this scheme for a member used as a
  value. Calls instantiate it like any scheme, and `trait_method_types` keeps
  the template for conformance and dispatch. This finishes what Stage 2 left
  for "until Stage 7".
- **G-C. Member rules.**
  - A member still must mention every explicit trait parameter. Its own
    parameters follow the `def` rule: one that the type does not mention
    needs explicit arguments.
  - The body of a member scheme must be a function type.
  - Parameter-product-capable member parameters are rejected: dictionary
    slots and polymorphic values cannot adapt an unknown arity (main plan
    §3).
  - Nested `Forall`s in member types (`T -> (<A> A -> A) -> T`) are allowed
    and need no new ABI: a scheme-typed parameter is an ordinary polymorphic
    closure argument (Stage 4b).
- **G-D. Implementation conformance.** An implementation member has no
  annotation. It is checked against the member scheme with the header
  substituted and the member's own parameters skolemized. This is the Stage 3
  check of a lambda against a scheme. The member's own parameter names are
  not in scope in implementation bodies, as with any lambda checked against a
  scheme. A body that must name them (to annotate a local `let`, say) would
  need implementation member annotations, which are new syntax and not part
  of this stage.
- **G-E. Default bodies.** A default body is written under the member's
  annotation. A top-level `Forall` there scopes its parameters over the body
  (main plan §2), so the default sees both the trait's parameters and the
  member's own. Its function template has the trait's parameters followed by
  the member's.
- **G-F. Static dispatch.** A call whose trait arguments are concrete
  selects the implementation as today. The instance of the implementation
  member is keyed by the header's parameters and the member's own
  parameters, from the call's instantiation. Concrete member arguments
  monomorphize. Member arguments that mention a dynamic body's run-time
  parameters take the dictionary-passed instance with those parameters at
  run time (Stage 4 key), with the header's parameters fixed. The dispatch
  record carries the member's own substitutions:
  `CheckedTraitDispatch { method, arguments, member_arguments }`.
- **G-G. Slots for generic members (7b).** A slot for a member with its own
  parameters is a *polymorphic entry* over the member's own scheme, with the
  dictionary as its environment:
  `(dictionary, member descriptors…, member dictionaries…, result, row…,
  arguments…)`. This is T-D's slot ABI with Stage 6's polymorphic entry
  hidden arguments inserted, in R-A and T-C order. Effect parameters of the
  member pass a bundle (E-D).
  - **Static dictionaries:** the slot is the implementation member's dynamic
    instance with the header fixed and the member's parameters at run time.
    It is not an R5 thunk: no concrete method exists to adapt to.
  - **Built dictionaries:** the slot is a descriptor-bound entry over the
    tail, as T-J describes. It forwards the header's descriptors and
    dictionaries from the tail, then passes the caller's member descriptors
    and dictionaries through. This is the polymorphic-closure environment rule
    (T-J), with the tail as the environment.
  - **Defaulted members:** the default's dynamic instance receives the
    dictionary as its trait bound (T-J), then the member's own hidden
    arguments.
  - **Slot calls** pass the member's descriptors and dictionaries from
    recipes, exactly as a call to a bounded polymorphic value does (T-M).
- **G-H. Trait members as polymorphic values (7b).** A trait member used at
  its scheme (G-B) is a polymorphic closure. Its entry is a generated
  *member entry*, keyed by trait and member. It takes the scheme's
  descriptors and dictionaries, reads the trait dictionary (the first
  dictionary, G-B), and calls the slot with the member's own hidden arguments.
  The environment is null. Instantiation adapters, scheme adapters, and
  subsumption (`<T where Ord T> T -> T -> Bool` from `Eq.eq` via a
  prerequisite route) come from 4b and 6b unchanged. A member whose scheme
  names an associated type (`Iterator.next`) stays under the T-H
  restriction.
- **G-I. Monomorphic member values at run-time types.** T-L's closure of the
  slot covers non-generic members. For a generic member, the slot closure is
  a polymorphic closure of the member's own scheme, and an instantiation at
  the use site goes through the 4b instantiation adapter.
- **G-J. Explicit arguments.** Order is the scheme's (G-B), which is the
  main plan's Decision 7: explicit trait parameters, then the member's.
  `_` skips any of them. `check_explicit_type_application` reads the scheme,
  so trait members leave the `trait_parameter_arguments` path. That table
  stays for implementation headers and bounds.
- **G-K. Polymorphic recursion through dispatch is rejected.** A generic
  member body that dispatches to the same member at a member argument that
  strictly contains the member's own parameter (`A` to `(A, A)`) would
  monomorphize without end. The existing check extends to trait dispatch:
  a call that reaches the same trait member, through the trait bound or a
  concrete implementation, is a self-use. If lowering has no instance-depth
  guard, one is added with a "polymorphic recursion" diagnostic, as a
  backstop for routes the checker cannot see (a default calling an
  implementation member that calls back).

## Steps

### S0 — Measure and write the corpus first

- **Probes.** Re-run the `p7/` probes and record each diagnostic. Confirm
  that no existing program, test, or standard library file writes `.<…>` on
  a member of a trait with associated types (G-J changes their meaning).
  A grep at `6b32a64` finds only `Eq.eq.<I32>` and `Eq.eq.<_>` in
  `tests/compiler.rs`, on a trait with no associated types.
- **Corpus programs.** They are written now and enabled as each step makes
  them pass. Each must be reached from concrete code, from dynamic bodies,
  and from nested closures (the Stage 6 lesson):
  - a generic member with a concrete implementation and a generic one,
    called concretely, from a bounded generic `def`, and from a bounded
    polymorphic value;
  - a generic member with its own bound, and one whose bound mentions a
    trait parameter;
  - a defaulted generic member, overridden in one implementation and not in
    another;
  - an effect-polymorphic member (`<effect E> F -> (A ->{E} ()) ->{E} ()`);
  - a member with a nested scheme parameter;
  - trait members as polymorphic values: a non-generic member
    (`ToString.to_string`), a generic member, one used through subsumption
    from a prerequisite, and one passed as an argument to a function taking a
    scheme;
  - explicit arguments, with `_` in trait and member positions, and on a trait
    with an associated type;
  - a generic member value captured in a closure that outlives its frame.
- **Restriction probes.** A member scheme naming an associated type used as a
  value (T-H), a parameter-product-capable member parameter, and polymorphic
  recursion through dispatch, each with its diagnostic.

### 7a.1 — Member schemes in the resolver and checker

- **Resolver.** Resolve a member's top-level `Forall` in a scope that holds
  the trait's parameters and associated types, then the member's own. Default
  bodies see both (G-E).
- **Checker.** Build the G-B scheme for every member. Apply the G-C rules,
  looking through the `Forall` to its body: the existing "must mention
  trait parameter" and "must have function types" checks currently inspect
  the raw annotation type. Remove the nested-scheme diagnostic.
- **Default templates.** Record the default's template with both parameter
  lists.

### 7a.2 — Conformance

- **Implementation members** are checked against the instantiated member
  scheme with the member's parameters skolemized (G-D). `impl_function_types`
  holds the scheme body for the implementation member's template, with the
  member's parameter ids. Members of different implementations share those
  ids, as companion members share their companion's.
- **Diagnostics** name the member and the scheme when a body is less
  general than the member (it fixes `B` to `I32`, for example).

### 7a.3 — Uses and dispatch

- **Calls.** `resolve_trait_method_use` instantiates the member scheme:
  infer trait and member arguments together, resolve the trait obligation
  (projections included), and check the member's own bounds at the use as
  for a generic `def`. Record `member_arguments` (G-F).
- **Explicit arguments.** Scheme order, with `_` (G-J).
- **Values.** A member used where a scheme is expected, or bound by an
  unannotated `let`, has the G-B scheme. Subsumption and alpha-equivalence
  are those of Stage 3.
- **Polymorphic recursion** through dispatch (G-K).
- **LSP.** Hover shows a member's scheme. Completion and signature help
  show explicit-argument order.

### 7a.4 — Static dispatch in lowering

- **Substitutions.** Lowering adds `member_arguments` to the header's
  substitutions for the implementation member's instance. The same applies to
  default members, whose template has both lists.
- **Dynamic bodies.** A call with concrete trait arguments and run-time
  member arguments takes the dictionary-passed instance with the member's
  parameters at run time (G-F). Its descriptors and dictionaries come from
  recipes like any dynamic call.
- **Census and coverage.** The coverage report counts generic member
  instances, and the hidden differential mode reroutes generic member calls
  from generic bodies.

### 7a.5 — Exit gate for 7a

- **Programs.** The S0 static-dispatch programs pass in default mode and in
  both differential modes.
- **Suites.** `cargo test --workspace`, both full differential runs, and IR
  against the base commit's release build, with every existing input
  `same`.

### 7b.1 — Slots for generic members

- **Slot ABI** (G-G), in the dictionary layout test, which gains a generic
  member.
- **Static, built, and default slots,** each with a corpus program reached
  from a bounded generic `def`, a bounded polymorphic value, and a nested
  closure.
- **Slot calls** pass member hidden arguments from recipes. Audit every
  place that assumes slots take no hidden arguments: slot call emission,
  slot closures (T-L), dictionary finalizers, and the slot-count arithmetic
  in built dictionaries' tails.

### 7b.2 — Trait members as polymorphic values

- **Member entries** (G-H) as an artifact family, requested through the
  worklist only where a member is used as a polymorphic value.
- **Generic member values at run-time types** (G-I).
- **Remove** the worklist restriction. A member whose scheme names an
  associated type reports the T-H message at the value.

### 7b.3 — `Staple.md` and restriction tables

- **Traits and bounded generic functions.** Remove "an individual trait
  member may not independently introduce parameters", and document member
  schemes, implementation conformance (own parameters not in scope), default
  bodies, and the explicit-argument order.
- **Generic functions.** Remove "a trait member used as a polymorphic value"
  from the restrictions, and add the associated-type case for members.
  Extend the call-site type argument rules to trait members.
- **Restriction tables.** Update this plan's, the Stage 6 plan's, and the 4b
  plan's.

### 7b.4 — Exit gate

- **Programs.** The Goal and S0 programs pass in default and both
  differential modes.
- **Suites.** `cargo test --workspace`, both full differential runs, and IR
  against the base commit's release build (every existing input `same`).
- **Reachability.** A generic member called only concretely produces no
  dictionary-passed instance, and an unused member value produces no member
  entry, slot, or adapter.

## Restrictions

After Stage 7:

| Restriction | Message | Lifted in |
|---|---|---|
| A trait member whose scheme names an associated type, used as a polymorphic value (T-H) | the Stage 6 associated-type message | Not planned yet: needs projection placeholders as run-time parameters of polymorphic entries |
| Parameter-product-capable member parameters (G-C) | "trait member parameters cannot be parameter-product capable" | Not planned |
| Implementation members naming the member's own parameters (G-D) | the ordinary unknown-type error | Not planned: needs implementation member annotations |
| Polymorphic recursion through dispatch (G-K) | "polymorphic recursion is not supported" | Not planned (main plan Decision 4) |
| Stage 6 restrictions (T-F ambiguity, missing dictionaries, `Natural`) | unchanged | as in Stage 6 |
| Coroutine, async, or reactive body, transitively | the existing messages | Stage 8 |

## Intended Differences

- Trait members may quantify over their own parameters and may contain
  nested schemes.
- Trait members have schemes: they can be stored, passed, and bound by an
  unannotated `let` as polymorphic values.
- Explicit `.<…>` arguments on members of traits with associated types omit
  the associated types.
- No other existing program changes behavior or IR.

## Risks

- **Dispatch records.** Every consumer of `CheckedTraitDispatch` and of the
  evidence arguments assumes the trait's arguments determine the
  implementation member's instance. Member arguments must reach instance
  keys, recipes, evidence validation, and specialization. A missed site
  monomorphizes two different instantiations into one instance.
- **Slot ABI drift.** Slots with and without hidden arguments coexist in
  one dictionary. Slot calls, slot closures (T-L), and built-dictionary
  tails must each read the member's scheme, not assume T-D's original slot
  shape. 7b.1's audit lists the sites before the first generic slot exists.
- **Default bodies.** A default with both parameter lists is generic over
  more than an implementation member. It is reached from static dispatch,
  from static dictionaries, and from built ones, and each path must
  substitute both lists.
- **Termination.** Polymorphic recursion through dispatch is easy to write
  by accident in generic members. G-K's checker rule and the lowering guard
  must both exist before generic members reach the corpus.
- **Corpus blind spots.** Stages 5 and 6 each shipped three defects in
  shapes the corpus did not exercise. Every S0 program is reached from
  concrete owners, dynamic bodies, and nested closures before its step is
  called done.
