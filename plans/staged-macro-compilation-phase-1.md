# Staged macro compilation, Phase 1: phase-separation analysis

Breakdown of Phase 1 in [`staged-macro-compilation.md`](staged-macro-compilation.md).
Line references are against `main` at `2c8bb69b` (2026-11-03).

Phase 1 makes the phase rules real while the interpreter still runs every
macro. When it is done, the expander knows the compile-time closure of each
macro and expands items in stage order. It reports cycles, and it instantiates
template macros without evaluating anything. Later phases rely on all four:

- Phase 2 checks each closure with the real front end, so it needs the closure
  sets.
- Phase 4 compiles closures and has to build them in stage order.
- Phase 5 compares the two engines, so the order the interpreter runs in has
  to match the compiled engine's.

Unlike the spike, this code is kept. Everything is committed to one branch,
with no pull requests.

## Rules for the implementor

- **Keep the status section of this file up to date.** Each time you finish a
  task, find a blocker or measure something, update
  [Implementation status](#implementation-status) in the same commit.
  Mark the task done, partly done or blocked, add the commit, and write
  down what you found. Anything that changes the plan goes in
  [Findings](#findings). When the phase ends, update the Phase 1 section of the
  umbrella plan with a short status paragraph (as Phase 0 has), and change
  later phases where the findings disagree with them.
- Don't refer to the plan files or phase numbers in the code and
  documentation, except from the plan files themselves.
- Don't run `staple fmt` (see `AGENTS.md`), and don't use its output in tests.
- Each task lands with tests. Behavior that is visible from Staple code (cycle
  errors, template expansion, stage order) is tested from Staple sources in
  `staple-compiler/tests`. Internal data (closure contents) is tested with unit
  tests next to the code.

## What already exists

All of these were read from the source.

- **Definitions and scopes are built up front from raw syntax.**
  `MacroExpander::new` (`staple-compiler/src/macro_expand.rs:2059`) fills one
  `ModuleScope` per module (`:267`), with `macros`, `modifiers`, `namespaces`
  and `helpers`. Helpers are any binding with a value, plus traits and
  `staple-intrinsic` externs (`:2276`–`:2350`). Type declarations and `impl`
  items are not indexed.
- **Name lookup on raw syntax already exists for macros.** `resolve_macro`
  (`:6074`) follows a name's definition module (hygiene), walks companion
  modules up to their parents and handles qualified access. Closure scanning
  needs the same lookup for every item kind.
- **Modules are expanded in program order, item by item, in place.**
  `expand_program_recovering` (`:1766`) loops over `program.modules_mut()`.
  It expands each item with `expand_item` (`:4425`), splices emitted items in,
  then drops compile-time helpers (`binding_is_compile_time_helper`,
  `:13602`) and `Parse` implementations.
- **Macros inside helper bodies are expanded lazily, during evaluation.**
  `eval_expression` (`:6684`) resolves and invokes a macro when it reaches one
  (`:7059`), and the static check does the same (`:3675`). So `if` inside
  `derived_eq_implementation` (`stdlib/std/cmp.sta:125`) is expanded every
  time the derive runs. There is no stage order: it works only because the
  interpreter evaluates raw syntax.
- **Recursion is caught per invocation.** `expansion_stack` reports
  "recursive macro expansion of `…`" (for example `:4697`; test at
  `staple-compiler/tests/compiler.rs:9854`). There is no notion of an item that
  depends on a macro.
- **`compile_time_items`** (`resolve.rs:1877`, built at
  `macro_expand.rs:2989`) is only the list of `std.syntax` intrinsics. Imports
  of them are accepted without binding anything (`resolve.rs:4447`, `:4485`).
  It is unrelated to closures.
- **The flow macros differ in shape** (`stdlib/std/flow.sta`):
  - `while`, `discard` and `for` are already templates: one `parse_quote`
    that splices only parameters.
  - `if` and `when` call recursive helpers (`if_branches`, `when_clauses`), and
    those helpers call `panic`.
  - `panic` (`stdlib/std/process.sta:41`) formats a `String` and calls into
    `std.io`. Today the interpreter recognizes `panic` by identity
    (`standard_library_process`) instead of evaluating it.
- **Derive helpers use `if`.** See `stdlib/std/cmp.sta:125` and `:138`.
- **Repeated splices exist only in some positions.** `$items...` works in item
  lists (`stdlib/std/typegroup.sta:235`) and in type-parameter lists
  (`stdlib/std/fmt.sta:48`). `Staple.md` lists "general repeated type and
  pattern splices" as future work.

## Decisions for this phase

1. **Closures include every item kind, not just what the interpreter needs.**
   Phase 2 sends closures through the real front end, so a closure has to hold
   the types, traits, `impl` items and `std.ops` operator traits that the code
   uses, not only helpers. An `impl` is included when its trait and its self
   type are both in the closure. A blanket `impl` (a type-parameter self type)
   is included when its trait is in the closure.
2. **Scans see through template expansions and rescan after real ones.**
   - When a closure item calls a template macro, the names in the template's
     quotation count as references. They are resolved in the template's
     definition module, by hygiene.
   - When it calls a non-template macro, its output can't be known before the
     macro runs. The closure is rescanned once the item is expanded. A
     closure is complete when a rescan adds nothing.
3. **The scanner tracks local scopes.** `let`, lambda parameters, match and
   `for` patterns introduce local names. A name that resolves to a local is not
   a reference. Without this, every local variable would look like an unknown
   item.
4. **An item is the unit of expansion, and its expansion is memoized.** An item
   expanded early because it is in a closure is not expanded again when the
   module walk reaches it. Its expansion (one item or several emitted items)
   is stored by the original item's `SyntaxId` and spliced in at the original
   position, so the order of the final module doesn't change.
5. **Cycles are found on demand, with in-progress marks** (the Racket
   approach). `ensure_ready(macro)` and `ensure_expanded(item)` call each
   other. Meeting a node that is still in progress is a cycle, and the stack
   of the two is the item chain shown to the user.
6. **Cycles are reported as data before they become errors.** First (task 5),
   cycles are collected and turned into a list for the standard library and the
   test corpus, while expansion falls back to the current lazy behavior.
   They become errors (task 7) only once that list is empty or every entry is
   understood. This keeps the branch's tests passing after every commit.
7. **`if` and `when` must not have a closure.** Every module that formats a
   `String` uses `if`, and `panic` reaches `String`. So as long as `if` or
   `when` calls a helper that can `panic`, both form a cycle. The fix depends
   on what the inventory shows (task 6):
   - **Preferred:** rewrite them as overloaded template macros that expand to
     a call of themselves on the remaining arguments. This needs a repeated
     splice of a `Sequence<Expr>` as juxtaposed call arguments (`if $rest...`)
     and, for `when`, a comma-separated repeated splice inside a braced group.
   - **Fallback**, if those splices turn out to be more than a small, general
     addition: make `if` and `when` compiler-provided macros with bodyless
     contracts, like `quote` and `line`. The expander then rewrites them in
     Rust.

   Record the choice and the reason in Findings.
8. **`compile_time_items` stays.** It is about importing intrinsics, not about
   closures, so the umbrella plan's bullet on replacing it moves to Phase 6,
   where the intrinsics change.

## Tasks

The tasks are in dependency order, and they are all committed to the same
branch. A task can take several commits, but the tests pass after each one.
Tasks 2 and 3 can run in parallel after task 1.

### 1. Baseline snapshots

Before changing behavior, record what expansion produces today, so later tasks
can show that they changed nothing:

- A script, `scripts/compare-expansion.py`, modeled on
  `scripts/compare-run-output.py`. It runs `staple expand` over the standard
  library entry points, every `.sta` input the macro tests use, and the
  examples. It stores the output under a directory given on the command line,
  then compares two such directories.
- Normalize anything that depends on allocation order (syntax ids, hygiene
  marks), if the rendered output contains any. The rendering comes from
  `render_expanded_module` (`staple-compiler/src/expansion_render.rs`), not
  from the formatter.
- Record the timings of the Phase 0 interpreter baseline (B1–B3 in the Phase 0
  plan) at the starting commit, to compare against at the end.

### 2. Template macros

- Detection (new `macro_expand/templates.rs`). An overload is a template when
  its body, after its parameter list, is exactly one of:
  - `parse_quote { … }` or `quote { … }` whose splices (`$name`, `$name...`)
    name only parameters, or
  - a parameter name on its own, as in `@identity = item => item`.

  Each overload is classified separately. A type ascription on the body is
  allowed. Anything else, including a `let` before the quote, is not a
  template.
- Instantiation: bind the matched arguments to the parameters and call the
  existing quotation code directly (the same code the interpreter uses for
  `parse_quote`/`quote`). Skip the general evaluator, its static check and
  its step budget. Hygiene marks, contextual result types and splice rules
  must behave exactly as before. That is the point of reusing the code.
- Record template-ness in `MacroDefinition` (`macro_expand.rs:93`) so that the
  closure scan (task 3) and later phases can query it.
- Tests: detection edge cases (ascription, a `let` before the quote, a splice
  of a non-parameter, a modifier returning its parameter), expansion results
  equal to before (task 1's comparison shows no difference), and hygiene
  through a template.
- Measure B1–B3 again. `while`, `for` and `discard` should get faster.

### 3. Item index and closure scan

- **Item index** (new `macro_expand/closure.rs`). Map `(module, name)` to the
  defining item for every item kind: bindings, types, traits, externs,
  submodules and macros. Also index `impl` items by trait and self type.
  Resolve `use` imports (selected, renamed, glob), re-exports and companion
  modules the way the expander does today. Share the lookup with
  `resolve_macro` instead of writing a second one.
- **Reference scan.** Walk an item's raw syntax and collect what it refers to:
  - names in expressions, types, patterns (constructor names), trait bounds,
    `Trait.member` access and pipes;
  - the `std.ops`, `std.cmp`, `std.try` and `std.convert` traits that
    operators desugar to (see `StdModules`, `macro_expand.rs:1000`, and
    `desugar_expression`);
  - macro invocations, recorded separately as macro dependencies, with their
    arguments scanned as ordinary syntax.

  Track local scopes (Decision 3). Inside `quote`/`parse_quote`, skip
  everything except names that are not spliced and not local, and resolve
  those in the definition module.
- **Closure.** The transitive set of items reached from a macro's body,
  following Decision 1 for `impl` items and Decision 2 for template calls.
  Expose it as `MacroClosure { items, macros_used }` from `MacroAnalysis`, so
  later phases can read it.
- **Generated items.** A name that doesn't resolve may be produced by an item
  macro that hasn't run yet. When a scan meets one in module `m`, the
  scheduler (task 4) first expands `m`'s top-level item-macro invocations,
  then looks the name up again. A name that still doesn't resolve is ignored:
  it may be a field or a member resolved later.
- Unit tests: closures across modules, packages, re-exports, companions,
  renamed imports, operators, blanket and concrete `impl` items, template
  calls, locals shadowing items, and quotations.

### 4. Stage-ordered expansion

- Restructure the module walk in `expand_program_recovering` around
  `ensure_expanded(item)` and `ensure_ready(macro)` (Decisions 4 and 5).
  Before an invocation of a non-template macro is expanded, its closure has
  to be ready. That means every item in it is expanded, and the closure has
  been rescanned until nothing new is added.
- After a closure item is expanded, point its `ModuleScope` helper entry
  (`HelperDefinition`) at the *expanded* binding. The interpreter then
  evaluates expanded syntax, and the lazy macro paths in `eval_expression` and
  the static check are no longer reached for closure items.
- Keep the module walk's final order. Emitted items go where their source
  item was (Decision 4).
- Recovery mode (the LSP) keeps working: an item whose expansion failed is
  left as it was, and the walk goes on.
- Run task 1's comparison: the output must match, apart from cases listed in
  Findings with a reason. Diagnostics may come out in a different order. If a
  test depends on the order, sort it or fix the test, and write it down.

### 5. Cycle detection and inventory

- When `ensure_ready` or `ensure_expanded` meets a node in progress, record
  the cycle as an item chain (for example `if` → `if_branches` → `panic` →
  `String` → … → `if`), and fall back to today's lazy expansion for that
  invocation.
- Add a hidden way to print the recorded cycles, for example the
  `STAPLE_MACRO_CYCLES` environment variable read by `staple expand`. Don't
  add a public flag yet (see the umbrella plan's open question 4).
- Run it over the standard library and the test corpus. Write the list of
  cycles in Findings, grouped by cause.

### 6. Break the standard library's cycles

- Resolve `if` and `when` as Decision 7 says.
- Resolve every other cycle in the inventory. Expected kinds:
  - **A derive applied to a type that the derive's own closure reaches.** For
    example, `@derive_eq` on a type that `@derive_eq`'s helpers use. Options:
    write that impl by hand, or narrow the closure if the reference is a false
    positive of the scan.
  - **False positives from over-approximation.** Fix them in the scanner, with
    a unit test each.
- Each fix is its own commit, and the inventory in Findings is updated.

### 7. Cycles become errors

- Replace the fallback with a diagnostic at the invocation that closed the
  cycle. For example: "macro `m` is needed to expand code that `m` itself
  depends on", followed by one note per link in the chain, each at its item.
- In recovery mode, the invocation stays unexpanded and the diagnostic is
  reported.
- Remove the lazy macro paths from evaluation (`macro_expand.rs:7059`) and from
  the static check (`:3675`) once no test reaches them. If something still
  does, that is a missing edge in the closure scan, so fix the scan.
- Tests: a direct cycle in one module, a cycle across modules, a cycle through
  an `impl`, a cycle through a generated item, and a non-cycle that looks like
  one (a macro used only inside a quotation of its own helper).

### 8. Language reference

Add a "Phases" section to `Staple.md` and link to it from the macro section.
It covers:

- what a compile-time closure is;
- that items in a closure are expanded before the macro runs;
- what a cycle is and how it is reported;
- what a template macro is, and that being one only changes speed, not
  meaning.

If `if` and `when` became compiler-provided macros (Decision 7), list them
with the other bodyless contracts. If the new repeated splices were added,
document them where `$items...` is documented.

### 9. Close out

- Run task 1's comparison and the timings one last time. Record both in
  Findings.
- Update the umbrella plan. Write a status paragraph for Phase 1, and move
  the `if`/`when` rewrite out of Phase 2 and the `compile_time_items` bullet
  to Phase 6, if that hasn't happened yet. Change anything else the findings
  affect.

## Done when

- Every non-template macro invocation is expanded only after its closure is
  ready, and the interpreter never expands a macro lazily.
- The standard library and all tests have no cycles. A cycle in user code is an
  error with an item chain.
- Template macros are instantiated without the evaluator, and `staple expand`
  output for the corpus matches the baseline.
- `MacroAnalysis` exposes each macro's closure and template-ness for Phase 2.
- B1–B3 are no slower than the baseline. B3 (1,000 `if`) should be much faster
  if `if` became a template or a compiler-provided macro.

## Implementation status

Update this section as you work (see "Rules for the implementor").

| Task | Status | Commit | Notes |
|---|---|---|---|
| 1. Baseline snapshots | not started | | |
| 2. Template macros | not started | | |
| 3. Item index and closure scan | not started | | |
| 4. Stage-ordered expansion | not started | | |
| 5. Cycle detection and inventory | not started | | |
| 6. Break the standard library's cycles | not started | | |
| 7. Cycles become errors | not started | | |
| 8. Language reference | not started | | |
| 9. Close out | not started | | |

## Measurements

_None yet._ For each one, record what was measured, the result, the machine,
the commit and the command. Use the same inputs as the Phase 0 interpreter
baseline (B1–B3) so the numbers are comparable.

## Findings

_None yet._ Record the cycle inventory (task 5), the choice for `if` and
`when` and its reason (Decision 7), every expansion difference from the
baseline with its reason, and anything that changes later phases.
