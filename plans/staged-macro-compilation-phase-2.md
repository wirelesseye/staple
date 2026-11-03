# Staged macro compilation, Phase 2: check macro code as ordinary code

Breakdown of Phase 2 in [`staged-macro-compilation.md`](staged-macro-compilation.md).
Line references are against `docs/staged-macro-plan` at `3c6e9dd5`
(2026-11-04), the end of Phase 1.

Phase 2 sends every macro's compile-time closure through the real front end
(`NameResolver` and `TypeChecker`) while the interpreter still runs the macros.
The point is to find every way macro code differs from ordinary code, and to
remove each difference, before Phase 3 and Phase 4 try to compile and run that
code. The main deliverable is the **gap inventory** in [Findings](#findings).
Each gap in it is either fixed in this phase, with tests, or explicitly moved
to a later phase with a reason.

## Rules for the implementor

- **Keep the status section of this file up to date.** Each time you finish a
  task, find a blocker or measure something, update
  [Implementation status](#implementation-status) in the same commit. Mark the
  task done, partly done or blocked, add the commit, and write down what you
  found. Anything that changes the plan goes in [Findings](#findings). When
  the phase ends, update the Phase 2 section of the umbrella plan with a short
  status paragraph (as Phases 0 and 1 have), and change later phases where the
  findings disagree with them.
- Commit everything to `docs/staged-macro-plan`, with no pull requests. A task
  can take several commits, but the tests pass after each one.
- Don't refer to the plan files or phase numbers in the code and
  documentation, except from the plan files themselves.
- Don't run `staple fmt` (see `AGENTS.md`), and don't use its output in tests.
- Expansion must not change. Run `scripts/compare-expansion.py` against the
  start of the phase after every task that touches the expander or the
  standard library, and explain every difference in Findings.

## What already exists

All of these were read from the source at `3c6e9dd5`.

- **Closures are computed on demand and exposed.** `MacroAnalysis` carries
  `closures: HashMap<SyntaxId, MacroClosure>`, `templates` and
  `closure_misses` (`staple-compiler/src/macro_expand.rs:1004`–`:1014`).
  `MacroClosure` (`macro_expand/closure.rs:88`) holds `items` and
  `macros_used` as sets of `ItemId`. Only macros that are actually invoked get
  a closure, because computing all of them cost about 80 ms.
- **Expanded items are memoized by syntax id** (`macro_expand/staged.rs`).
  Helpers and macro bodies are expanded in a compile-time mode: quotations and
  syntax constructors stay as written.
- **Compile-time items are dropped after expansion.** That covers helpers
  whose signature mentions a syntax type (`binding_is_compile_time_helper`)
  and `Parse` implementations. `Parse.parse_all` loses its default body in
  `std.syntax` (`expand_program_recovering`). The front end has never seen
  any of this code.
- **`staple-intrinsic` externs are already a normal front-end feature.** The
  resolver allows the ABI only in the standard library (`resolve.rs:3088`).
  The typechecker accepts generic intrinsics (`typecheck.rs:11071`), and many
  run-time intrinsics use it. The syntax intrinsics (`stream`, `is_empty`,
  `remaining`, `error_at`, `compile_error`, and the `__parse_primitive` and
  `__*_group` building blocks) are declared in `stdlib/std/syntax.sta`.
  Today, imports of them are skipped through `compile_time_items`
  (`resolve.rs:1877`).
- **String literal types exist in the typechecker** (`StringLiteralSet`,
  `typecheck.rs:596`, and `Pattern::StringLiteral` at `:10955`). Whether a
  value of type `Ident<String>` can be *matched* against `Ident<"else">` at
  run time is not known.
- **The interpreter's static check still expands lazily.**
  `validate_compile_time_code` runs before expansion and resolves macros in
  helper bodies as it meets them (`macro_expand.rs:3868`).
  Phase 1 removed the lazy path from evaluation, but not from this check.
- **The reference describes compile-time code as a separate dialect.**
  `Staple.md` says `impl Parse` items are "never type-checked as runtime code"
  and that the orphan rule for them "is applied by the macro expander"
  (`Staple.md:436`–`:438`). It says compile-time checking is "permissive", and
  it describes compile-time `?`, `return` and `mut` parameters in their own
  terms (the "Parsing syntax with `Parse`" section and the following
  paragraphs).
- **Phase 0, Finding 1:** a phase-1 library is built against the cached
  objects of its dependencies, and only the package being expanded is
  compiled from the closure. The check program below follows that split.
- **Phase 1 Findings** list two known limits that this phase should look at:
  loops through a generated item (the two `typegroup ConversionError` chains
  in `std.convert`), and closures inflated by the `impl` rule (`derive_eq` at
  861 items, most of them `Convert`/`TryConvert` impls).

## Decisions for this phase

1. **The check program mirrors what Phase 4 will compile.** For one package
   being expanded:
   - Every *dependency* package (including `std`) appears whole: every item,
     after expansion, **including its compile-time items**. Phase 4 links
     those packages as cached objects built with their compile-time items
     kept, so the check needs the same view.
   - The *package being expanded* contributes only the items in the closures
     being checked. This tests that closures are complete: a name the closure
     misses is a resolution error, not a silent success.
   - `use` declarations in the expanded package are kept, but an import whose
     target is not in the check program is dropped. The resolver rejects
     unresolved imports, and those imports belong to code outside the
     closure.

   For a single-file program, the file is the package being expanded and
   `std` is its only dependency. When the standard library itself is checked,
   `std` is the package being expanded.
2. **A macro overload becomes a function.** Each overload with a body becomes
   a private `def` whose parameter types come from its `MetaType` signature
   (an unannotated parameter is `SyntaxNode`) and whose result type is the
   declared or inferred macro result. A `MacroCallMetadata` first parameter
   stays as it is. Modifier macros take and return their declared item types.
   The function's name is compiler-generated (Phase 0, Finding 9: names must
   not depend on the user's module layout) and is reserved, so source code
   can't refer to it. Bodyless, compiler-provided macros are skipped.
3. **Quotations become typed calls.** This is the surface lowering Phase 3
   implements at run time, so it is fixed here:
   - A new trait in `std.syntax`, `Splice T { splice: T -> Syntax }`, has
     impls for every syntax type that may be spliced, and for
     `Sequence<T> where Splice T` for `$name...`. A splice `$x` becomes
     `Splice.splice x`. That way, splicing a value that isn't syntax is a type
     error.
   - `quote { … }` becomes `__quote (TEMPLATE_ID, splices)`, where `__quote:
     (U64, Sequence<Syntax>) -> Syntax` is a new syntax intrinsic, and
     `TEMPLATE_ID` indexes a per-program table of quotation fragments.
   - `parse_quote { … }` becomes `__parse_quote.<T> (TEMPLATE_ID, splices)`.
     `__parse_quote` is an ordinary generic Staple function in `std.syntax`:
     it calls `Parse.parse_all.<T>` on `__quote`'s result and turns an error
     into `compile_error`. `T` comes from normal inference. That replaces the
     expander's special rule for the contextual type, and the "requesting
     `Syntax` is rejected" rule becomes "`Syntax` has no `Parse` impl" (or a
     deliberately missing one).

   Hygiene is not modeled in this phase. Template ids and the table only have
   to be stable within one check.
4. **The interpreter keeps running the original syntax.** The lowering in
   Decisions 2 and 3 is applied to a copy for the check program. The
   interpreter, and so expansion results, don't change in this phase.
5. **The check is opt-in in the compiler and on in the test suite.** A
   standard-library check costs roughly a full front-end pass (Phase 0
   measured about 1.1 s). That is fine for tests, but not for every build
   until Phase 4 caches checked dependencies. The switch is the hidden
   environment variable `STAPLE_CHECK_MACRO_CLOSURES`, read by `staple check`,
   `compile`, `run` and `expand`: `1` checks the closures of invoked macros,
   and `all` also computes and checks the closure of every macro declared in
   the program. Like `STAPLE_MACRO_CYCLES`, it is not a public flag.
6. **Every gap is resolved toward run-time semantics.** When macro code relies
   on a behavior only the interpreter has, change the macro code or the
   language for both stages. Don't add a compile-time special case to the
   real front end. Each language change is recorded in Findings, documented
   in `Staple.md` and tested from run-time code.
7. **The interpreter's static check moves after expansion.**
   `validate_compile_time_code` runs per closure on expanded syntax, which
   removes the last lazy expansion. It stays, as the interpreter's own guard,
   until Phase 6 deletes the interpreter.

## Tasks

The tasks are in dependency order. Task 7 can be done at any point after
task 1.

### 1. Check-program builder

New `macro_expand/phase_program.rs` (choose a name that doesn't mention
phases if one reads better in code). Given the expanded program and a set of
closures, it builds a `Program`:

- Dependency packages whole, with compile-time items kept (Decision 1). That
  means capturing the expanded items *before* the `retain` that drops
  compile-time helpers and `Parse` impls, and before `Parse.parse_all` loses
  its default.
- The expanded package's closure items, with imports filtered (Decision 1).
- Macro overloads converted to functions (Decision 2).
- Module structure, visibility and package identity kept as they are, so that
  name resolution behaves as it does for the real program.
- Unit tests: a closure across two modules, a dropped import, a macro with
  `MacroCallMetadata`, a modifier macro, overloads, and a dependency package.

### 2. Typed quotation lowering

- Add `Splice`, its impls, `__quote` and `__parse_quote` to
  `stdlib/std/syntax.sta` (Decision 3). The impl bodies call intrinsics. In
  this phase, they only have to type-check.
- Lower `quote`, `parse_quote`, `$x` and `$x...` in the check program's copy
  of each closure item, and build the template table.
- Decide where `$x` is allowed to splice a non-syntax value today (for
  example a `String` into an identifier position, if the interpreter accepts
  that). Each such case either gets a `Splice` impl or becomes an error.
  Record which in Findings.
- Tests: a quotation of each result category, a repeated splice, a splice of
  the wrong type (type error), and a `parse_quote` whose type is only known
  from an ascription.

### 3. Check driver

- Under `STAPLE_CHECK_MACRO_CLOSURES` (Decision 5), after expansion, build the
  check program for the closures that were computed, then run
  `NameResolver` and `TypeChecker` on it.
- Report each diagnostic at its own span, followed by a note naming the macro
  whose closure contained the item ("while checking the compile-time code of
  macro `m`"). One item shared by several closures is checked once.
- Add `scripts/check-macro-closures.py`, which runs the check with `all` over
  the corpus that `scripts/compare-expansion.py` uses and prints the
  diagnostics grouped by message. That output is the raw material for task 4.
- Measure the cost of the check on B1–B3 (the Phase 0 inputs) with `1` and
  with `all`.

### 4. Gap inventory

Run task 3's script and write every distinct failure into Findings, grouped by
cause. For each group, record: an example, how many closures hit it, the
proposed resolution, and whether it is fixed in this phase (task 5) or moved
later. Expected groups, none of them confirmed yet:

- **Syntax types at run time.** The opaque syntax types and `ParseStream`
  being used as values, and their representation. They may already pass
  checking as `@sized_opaque` types.
- **Refinement patterns.** Matching an `Ident<String>` against
  `Ident<"else">`, and `Sequence (first, rest)` on `@recursive_constructor`
  types.
- **Semantic differences that `Staple.md` describes for compile-time code:**
  `?` on a value that isn't a `Result`, `return` from nested blocks, and `mut`
  parameters passed by address. Some of these may already match run-time
  semantics, in which case the compile-time wording is just removed.
- **Inference and dispatch that the interpreter left permissive:** integer
  literals with no width, unbound type arguments, and `collect` without an
  expected type.
- **`impl Parse` coherence.** The interpreter's own orphan rule against the
  real one.
- **Closure misses.** Names that resolve in the full program but not in the
  check program. Each one is a bug in the Phase 1 scan, so fix it there, with
  a unit test.
- **Generated-item loops.** Whether the two `typegroup ConversionError` chains
  hide a real dependency (Phase 1, "Known limits").

### 5. Close the gaps

Fix each group that the inventory assigns to this phase, following
Decision 6. Each fix gets its own commit with:

- the change (macro code, standard library, front end, or language),
- run-time tests when the language changed,
- the `Staple.md` change, when there is one,
- an updated inventory entry.

Groups whose fix needs the run-time representation of syntax (Phase 3) or
the restricted runtime (Phase 4) are moved there, with a note in the umbrella
plan. The phase is not done while a group is still open without a target
phase.

### 6. The interpreter's static check after expansion

- Run `validate_compile_time_code` per closure, on the expanded items, at the
  point where `ensure_ready` has finished (Decision 7).
- Remove its lazy macro resolution (`resolve_macro` in
  `check_compile_expression`). Like Phase 1's evaluation path, replace it with
  an error saying the closure scan missed an edge.
- The corpus comparison must show no difference, and the existing
  diagnostics tests must still pass. Diagnostics may now come later or in a
  different order. Fix tests that depend on the order, and write down which.

### 7. Compile-time effects (umbrella Decision 7)

Decide what compiled macro code may do. The umbrella plan proposes no
`std.fs`, no reading from `std.io`, no spawning processes and no OS seeding
for `std.rand`, with debug output to stderr allowed. Record the decision,
document it in the "Compile-time phases" section of `Staple.md`, and leave
enforcement to Phase 4, where lowering knows what is actually reachable. The
closure over-approximates, so enforcing it in this phase would report
functions that are never called.

### 8. Turn the check on in the test suite

- Make every compiler test that expands macros run with
  `STAPLE_CHECK_MACRO_CLOSURES=1`, or add one test that runs the check with
  `all` over the standard library and the macro test fixtures. Choose by cost
  and record why.
- A failing check must fail the test with the diagnostics, not just a count.

### 9. Language reference

- Replace the statements that compile-time code is never type-checked as
  run-time code, that its checking is permissive and that `impl Parse` has its
  own orphan rule, with the rule that macro code is ordinary code and is
  checked as such. Keep the interpreter's evaluation limits (the step budget
  and the run-time-only list), because those still hold until Phase 6.
- Document `Splice`, and how `parse_quote` gets its type through `Parse`.
- Document every language change from task 5.

### 10. Close out

- Run the corpus comparison and the timings one last time, and record both in
  Findings.
- Update the umbrella plan: write a status paragraph for Phase 2, and change
  Phases 3 to 6 where the inventory moved work to them or changed their
  assumptions. For example, Phase 3 must implement `__quote`, `Splice` and the
  template table exactly as lowered here.

## Out of scope

- Running anything compiled. That is Phase 3 and Phase 4.
- Narrowing closures with the typechecker's selected impls. Phase 1 suggests
  this would let derives come back on standard-library types, but closures
  have to be known before expansion, while checked code exists only after.
  Record any evidence for it in Findings, and leave the change for Phase 4,
  where the library build can use it.
- Turning the check on by default in the compiler.

## Done when

- With `STAPLE_CHECK_MACRO_CLOSURES=all`, the closures of every macro in the
  standard library and in the macro test fixtures pass `NameResolver` and
  `TypeChecker` with no diagnostics.
- Every gap in the inventory is fixed or assigned to a later phase with a
  reason, and the umbrella plan says so.
- The test suite runs the check (task 8).
- The interpreter's static check has no lazy macro expansion left.
- `staple expand` output for the corpus is unchanged, apart from differences
  listed in Findings with a reason.

## Implementation status

Update this section as you work (see "Rules for the implementor").

| Task | Status | Commit | Notes |
|---|---|---|---|
| 1. Check-program builder | not started | | |
| 2. Typed quotation lowering | not started | | |
| 3. Check driver | not started | | |
| 4. Gap inventory | not started | | |
| 5. Close the gaps | not started | | |
| 6. Static check after expansion | not started | | |
| 7. Compile-time effects | not started | | |
| 8. Check in the test suite | not started | | |
| 9. Language reference | not started | | |
| 10. Close out | not started | | |

## Measurements

_None yet._ For each one, record what was measured, the result, the machine,
the commit and the command. Use B1–B3 from the Phase 0 baseline, with the
check off, with `1` and with `all`.

## Findings

_None yet._ Record the gap inventory (task 4) and how each group was resolved,
splice cases that changed (task 2), the effects decision (task 7), every
expansion difference from the start of the phase with its reason, and
anything that changes later phases.
