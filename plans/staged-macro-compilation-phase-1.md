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
| 1. Baseline snapshots | done | `de1666ef` | `scripts/compare-expansion.py` (`capture DIR`, `compare A B`) covers 232 inputs: the standard library's entry files, the corpus dump, the examples and the test fixtures. Two captures of the same binary compare equal. `scripts/bench-expansion.py` reproduces B1–B3. |
| 2. Template macros | done | `5da38086` | `macro_expand/templates.rs`: `template_parts` detects the curried body (quotation over parameters only, optionally ascribed, or a bare parameter; each overload separately) and `instantiate_template` binds the arguments and evaluates only the quotation. `MacroDefinition.is_template` records it and `MacroAnalysis.templates` exposes it. Argument conversion moved into `macro_argument_value`, shared with the general path. The corpus comparison shows no difference. |
| 3. Item index and closure scan | done | `5da38086` | `macro_expand/scan.rs` (reference scanner with local scopes, quotations kept apart, operators seen through `desugar_item`), `macro_expand/closure.rs` (`ClosureIndex`: units by syntax id, names by module with imports, renames, globs, re-exports, prelude and companion fallback, `impl` entries; `MacroClosure { items, macros_used }`). `SyntaxId` gained `Ord`. Closures are computed on demand, not for every macro on every run, because doing all of them cost about 80 ms on B1. |
| 4. Stage-ordered expansion | done | `b71059f7` | `macro_expand/staged.rs`: `ensure_ready` (a macro's closure and the macros it calls are expanded first) and `ensure_expanded` (one top-level item, remembered by syntax id). `invoke_macro` and `invoke_modifier` call `ensure_ready`; the module walk (`expand_in_walk`) reuses an item's earlier expansion and puts it back where it was written. Compile-time helpers and macro bodies are expanded in a compile-time mode (quotations and syntax constructors stay as written) and the scopes' `HelperDefinition` is pointed at the expanded binding. The index moves each module's items out of the program instead of copying them, so the extra cost is small. Corpus comparison: 232/232 identical apart from the three standard-library files edited in task 6. |
| 5. Cycle detection and inventory | done | `b71059f7` | A stack of macros and items in progress (`StageNode`). Meeting a node already on it records the chain; `STAPLE_MACRO_CYCLES=1 staple expand FILE` prints the chains on stderr. Inventory in Findings. |
| 6. Break the standard library's cycles | done | `b71059f7` | `compile_error` intrinsic so `if` and `when` do not reach `panic`; `extern` blocks indexed per binding; hand-written `Eq`/`Hash`/`Debug` for `True` and `False` and `Eq`/`Hash` for `None` and `Some`. Zero cycles across the corpus afterwards. |
| 7. Cycles become errors | done | `b71059f7` | The cycle is reported at the invocation that closed it ("macro `m` is needed to expand code that `m` itself depends on") with one `note:` diagnostic per link, each at its item; the invocation stays unexpanded. The lazy macro path in `eval_expression` is replaced by an error saying the closure scan missed an edge, and no corpus input reaches it. A name that resolves to nothing makes the scheduler expand the item-position macros and modified items of its module first (generated items); a loop through one is recorded but not reported, because names alone cannot say that the macro needs it. Tests: direct, across modules, through an `impl`, a generated item (recorded), a quotation that only looks like a cycle, and stage order. |
| 8. Language reference | done | `b71059f7` | "Compile-time phases" in `Staple.md`, linked from the metaprogramming introduction, plus `compile_error` in the `Parse` intrinsics listing. |
| 9. Close out | done | `b71059f7` | Final corpus comparison: 227/232 identical; the five that differ are the standard-library entry files that were edited (`boolean`, `cmp`, `flow`, `option`, `result`). No cycle is reported as an error; two chains through the generated `typegroup ConversionError` in `std.convert` are recorded. Timings are in Measurements: **B1–B3 are 8–20% slower than the baseline, so that done-when criterion is not met** (see Findings). Full test suite: 39 test binaries, no failures. |

## Measurements

For each one, record what was measured, the result, the machine, the commit
and the command. Use the same inputs as the Phase 0 interpreter baseline
(B1–B3) so the numbers are comparable.

### Starting baseline (task 1)

Apple M2, macOS 27.0, release build at `2c8bb69b` (the code of this branch is
identical to `main` there), 2026-11-03. Command:
`python3 scripts/bench-expansion.py` (best of five).

| # | Input | Time |
|---|---|---|
| B1 | `let x: I32 = 1` | 0.271 s |
| B2 | 20 × `@derive_eq` on a 50-field `wrap` | 0.284 s |
| B3 | One function with 1,000 `if … else …` | 0.724 s |

These agree with the Phase 0 numbers (0.278, 0.289 and 0.712 s).

### At the end of the phase (task 9)

Apple M2, macOS 27.0, release build of this branch (head of task 9) against
the baseline binary (the spike branch's `staple`, whose expansion code is the
same as `main`'s at `2c8bb69b`), run back to back, best of five. The machine
was shared, so single runs vary by about ±0.03 s; CPU time (user plus system)
for B1 was 0.25 s against 0.29 s.

| # | Input | Baseline | Now |
|---|---|---|---|
| B1 | `let x: I32 = 1` | 0.260 s | 0.31 s |
| B2 | 20 × `@derive_eq` on a 50-field `wrap` | 0.275 s | 0.32 s |
| B3 | One function with 1,000 `if … else …` | 0.713 s | 0.77 s |
| B4 | One function with 1,000 `while` and 1,000 `discard` (added) | 0.76 s | 0.82 s |

Commands: `python3 scripts/bench-expansion.py [--staple BINARY]`; B4 is
`staple expand` on a generated function (1,000 `while (y < i) { y = y + 1 }`
and 1,000 `discard (y + i)`).

Where the extra time goes (instrumented run of B1): scanning 1,364 items for
six macro closures takes about 25 ms, and building the item index, tracking
each expansion and the larger closures take the rest. The index no longer
copies the standard library (it moves each module's items out of the program),
which removed about 40 ms of an earlier version.

## Findings

- **Baseline corpus.** `python3 scripts/compare-expansion.py capture DIR`
  takes about 90 s. Hygiene names (`True__macro_18`) are renumbered by first
  appearance, and diagnostics have the input path replaced, so two runs of one
  binary are identical. One input exits non-zero in the baseline:
  `stdlib/boolean.sta` ("splices are only allowed inside `quote`"). That is
  expected when a standard-library file is expanded as an entry file, and its
  stderr is part of the snapshot, so it only counts as a difference if it
  changes.

- **Template measurements.** B1–B3 do not use `while`, `for` or `discard`, so
  they cannot show the template speed-up; they stayed within noise
  (0.262, 0.275 and 0.707 s after task 2).
- **Closure sizes (task 3).** On the real standard library the closure of
  `while` is 15 items and of `for` 19, with no macro dependencies beyond the
  built-in modifiers. `if` has 153 items and `when` 157, and both call `if`
  and the derive macros through `String`. `derive_eq` has 188 items and
  `typegroup` 308. That `if` reaches itself is the cycle Decision 7 expects.

- **Cycle inventory (task 5).** Over the 232 corpus inputs the first inventory
  had 152 distinct chains. They came from three causes:
  1. **`if` and `when` reaching themselves through `panic`.** `panic` formats
     a `String`, and `String`, the number types and `char` use `if` and `when`,
     so both macros formed a cycle with every item that uses them.
  2. **Derives applied to types the derives' own closures reach.** `True` and
     `False` (`std.boolean`) and `None` and `Some` (`std.option`) carry
     `@derive_eq`, `@derive_hash` and, for the booleans, `@derive_debug`. Each
     derive macro's closure reaches `Bool` and `Option`, so each derive needed
     itself, and the other two, expanded first. Every other chain in the
     inventory passed through one of those four types.
  3. **Over-approximation by block granularity.** The `std.syntax` intrinsics
     are one `extern` block, so `if` reached `Bool` and `Option` through the
     signatures of intrinsics it never calls, and the unit that stood for a
     whole block was credited with every name inside it.
- **Decision 7: neither the repeated splice nor the compiler-provided macros.**
  `if` and `when` stay ordinary macros. What put them in a cycle was only
  `panic`, so the helpers now report malformed input with a new
  `std.syntax.compile_error: String -> Never` intrinsic, which gives the same
  diagnostic as `panic` did ("macro `if` panicked: ...") and has no closure.
  With the extern block split per binding, the closure of `if` is the
  syntax types, `String` and its representation, and the two helpers: 19
  items, no macro dependencies except the built-in modifiers. The preferred
  design (templates that call themselves) needs a repeated splice of a
  `Sequence<Expr>` as call arguments, which is a language addition for no
  gain in the dependency structure, and the fallback (rewriting in Rust)
  duplicates `if_branches`. The cost is that `if` is still evaluated by the
  interpreter, so B3 does not get faster.
- **Hand-written impls (task 6).** `True`, `False`, `None` and `Some` no longer
  use the derive macros; their `Eq`, `Hash` and `Debug` impls are written out
  as the derives produce them. The other derived standard-library types
  (`Ordering`, `Result`'s variants) are not reached by any macro's closure and
  keep their derives.
- **Expansion differences from the baseline.** None for user programs, the
  examples and the fixtures. Five standard-library entry files differ, all
  because they were edited: `boolean.sta`, `option.sta`, `result.sta` and
  `cmp.sta` lost their derive and `typegroup` uses (`boolean.sta` used to fail
  when expanded alone, and now expands), and `flow.sta` uses `compile_error`.
- **The speed criterion is not met.** B1–B4 are 8–20% slower than the baseline
  (Measurements). B3 did not get faster because `if` stayed an interpreted
  macro, and the template speed-up for `while`, `for` and `discard` is smaller
  than the cost of the analysis, so B4 is slower too. The cost is the closure
  scan (about 25 ms for the closures a run needs) plus the index. Options for
  Phase 4, which wants the closures anyway: scan once and cache the result
  with the standard-library objects, or store the scan with each parsed module.

- **Task 7 findings.**
  - *Lazy paths.* The corpus (232 inputs) reaches the interpreter's lazy macro
    expansion zero times once the standard library is expanded in stage order,
    after two additions: `Parse` member bodies and the trait's default
    `parse_all` are expanded the first time they are used, and the walk keeps
    an `impl` readable while its own expansion runs. The lazy path in
    `eval_expression` is now an error, so a closure edge the scan misses fails
    loudly instead of expanding late.
  - *Static check.* The lazy expansion in the static check (`check` of
    compile-time code) stays. It runs in `validate_compile_time_code`, before
    any expansion, over raw syntax that still has macro calls in it, so it
    cannot wait for the stage order. Removing it needs the check to run per
    closure after expansion, which belongs with the front-end work of Phase 2.
  - *`Ordering` by hand.* Turning unresolved names into expansions of their
    module's generators exposed one more cycle: `typegroup`'s closure reaches
    `Eq`, `PartialOrd` and `Ord`, which mention `Ordering`, which `typegroup`
    itself makes in `std.cmp`. `Ordering` and its three variants are now written
    out as the macros produce them.
  - *Method calls and pipes.* A method call or pipe stage runs a companion
    function chosen by the receiver's type, which names cannot tell. Adding
    every companion function with that name to the closure grew the closure of
    `derive_eq` from 188 to over 800 items and brought new cycles, so the scan
    does not do it. Instead the interpreter expands the function it actually
    selects, the first time it calls it: companion functions, trait
    implementation members, trait default members and helpers reached by name
    all go through `expanded_binding`, `expanded_impl` and `expanded_trait`.
    These expand one item on demand and are subject to the same cycle
    detection, but the closures recorded in `MacroAnalysis` do not contain
    functions reached only through a method call. Phase 2 must add those when
    it has the receiver types.
  - *`const` initializers.* Folding a `const` runs arbitrary functions in the
    interpreter, so its initializer is a closure root as well
    (`ensure_expression_ready`). The scan resolves `Type.member` through the
    type's companion blocks for the same reason.
  - *`Ok` by hand.* Reaching companion members through type namespaces brought
    `Ok` (`std.result`) into the closure of `derive_eq`, so it is written out
    like `Some`; `result.sta` is the fourth standard-library file whose
    expansion differs from the baseline.
  - *Functions only.* The interpreter's tables point at the expanded form of
    functions (`def`), trait declarations and implementations. A `const` is not
    replaced: it expands to its folded value, and a folded NaN names the
    `from_bits` intrinsic relative to its own hygiene, which the interpreter
    cannot resolve from the module that refers to the constant.
  - *Generated names the language cannot use yet.* A compile-time helper that
    a macro generates is not visible to the interpreter at all (the helper
    tables are built from written items), so the closure of a macro cannot
    usefully depend on one; generated types and impls are what the rule is for.

Nothing else to record.
