# Staged macro compilation, Phase 5: switch over

Breakdown of Phase 5 in [`staged-macro-compilation.md`](staged-macro-compilation.md).
Line references are against `docs/staged-macro-plan` at `3534a88f`
(2026-11-25), the end of Phase 4.

Phase 4 made compiled macros work end to end behind a hidden
`--macro-engine=compiled`, and the engines agree on 1902 of 2034 inputs. Phase 5
makes compiled the default everywhere `staple` expands macros:

- the command line;
- the language server;
- the compiler's own test suite.

The interpreter stays reachable through `--macro-engine=interp` until Phase 6
deletes it.

Before the switch:

- every difference between the engines is either fixed or written down as an
  intended language change;
- the compiled engine is about as fast as the interpreter on a warm cache;
- the language server can build libraries without blocking analysis.

## Rules for the implementor

- **Keep the status section of this file up to date.** Each time you finish a
  task, find a blocker or measure something, update
  [Implementation status](#implementation-status) in the same commit. Mark the
  task done, partly done or blocked, add the commit, and write down what you
  found. Anything that changes the plan goes in [Findings](#findings). When the
  phase ends, update the Phase 5 section of the umbrella plan with a short
  status paragraph, and change Phase 6 where the findings disagree with it.
- Commit everything to `docs/staged-macro-plan`, with no pull requests. A task
  can take several commits, but the tests pass after each one.
- Don't refer to the plan files or phase numbers in the code and
  documentation, except from the plan files themselves.
- Don't run `staple fmt` (see `AGENTS.md`), and don't use its output in tests.
- Until task 11 switches the default, interpreter expansion must not change.
  Run `scripts/compare-expansion.py` against the start of the phase after every
  task that touches the expander or the standard library, and explain every
  difference in Findings.
- `scripts/check-macro-closures.py --mode all` stays clean after every commit.
- Run `scripts/compare-engines.py` after every task that touches either engine,
  the standard library or test sources, and keep its count in the status
  table. Run the standard library's entries one at a time until task 9 fixes
  concurrent builds.
- Each language decision below is a proposal for the user to confirm before it
  is implemented. If the user changes one, update the decision here first.
- Until task 6 restores them, don't add `if`, `when`, `for` or `while` to code in
  `std.syntax`, or to code the syntax runtime reaches. Write it with `match` and
  `loop`.
- Work on macOS and Linux. Other hosts keep the interpreter as their default,
  and `staple` says so when `--macro-engine=compiled` is given there.

## What already exists

All of these were read from the source at `3534a88f`.

- **The engine flag.** `--macro-engine=interp|compiled` is parsed in
  `staple-cli/src/compile.rs` and `package.rs`, and defaults to `Interp`
  (`package.rs:142`, `compile.rs:624`). `run_with_macro_engine`
  (`compile.rs:1019`) installs `CliMacroBackend` (`staple-cli/src/macro_backend.rs`)
  for the duration of a command.
- **The backend lives in the CLI.** The `MacroBackend` trait is in
  `macro_expand/compiled_call.rs:111`. Its only real implementation, with the
  cache (`staple-cli/src/cache.rs`), the linker and the server client, is in
  `staple-cli`. Code that calls `NameResolver` directly installs no backend, so
  it expands with the interpreter. That includes the compiler's own tests and
  the language server (`staple-cli/src/lsp/mod.rs:686`).
- **Strict mode.** With the compiled engine, a call the compiled code couldn't
  take reports ``internal error: macro `m` could not be run as compiled code``
  (`refuse_interpreter`, `macro_expand.rs:5858`). A macro with an omitted
  parameter type gets no entry function (`check_program.rs:1259`), so it
  always falls back to the interpreter. `Staple.md` says an omitted parameter
  type means `SyntaxNode`, and its `choose` example (`Staple.md:93`) omits
  them. About 13 of the integration tests' sources do too.
- **The comparison.** `scripts/compare-engines.py` expands and checks each input
  with both engines (`staple expand --tree-dump`, `staple check`). With
  `--dump-tests` it also covers every source the compiler's integration tests
  load. The standard library, examples, fixtures and corpus match (232/232).
  1857 of 1989 test sources match.
- **The remaining differences (Phase 4, task 12).** None is an engine bug:
  - 25 macros with a `parse_quote` that nothing gives a type, such as an
    unannotated binding. The interpreter takes the type from the call, and
    compiled code can't.
  - 11 ownership errors, which compiled builds enforce by design.
  - Macro code that is only valid dynamically: refutable `let Ok x = …`, `move`
    markers that don't match the annotation, `std.syntax` types used without an
    import, match arms with different result types, a juxtaposed `error_at`.
  - Different wording: a missing trait reads `no trait implementation …`, a
    panic adds the macro's source position, and stage cycles can be reached in
    another order.
  - Four that only the interpreter rejects: a bare `Sequence` or `Separated`
    splice, and `Syntax` as a `parse_quote` context.
- **Cost.** Phase 4's measurements, macOS on an Apple M2:

  | | Interpreter | Compiled |
  |---|---|---|
  | B1, warm | 0.33 s | 0.56 s |
  | B2, warm | 0.34 s | 0.61 s |
  | B3, warm | 0.81 s | 1.27 s |
  | One `if` call (561 calls) | 110 µs | 383 µs |
  | One closure library | – | 3.6–4.3 s (type check about 1.2 s, lower about 1.7 s) |
  | Cold standard library | – | about 57 s |

  About a third of an `if` call is the server's work, and half of that is
  rebuilding the fragment's position: `stream_prefix` (`wire.rs:780`) pads the
  slice with filler tokens, and the parser intrinsics then rebuild text and
  line tables from the start of the file.
- **Known faults.**
  - An `O1` package library lost a `Sequence`'s elements in a later call,
    probably through derived pointers the conservative stack scan misses.
    Libraries are built at `O0`.
  - Two builds of one package prune each other's libraries
    (`prune_stale`, `cache.rs:460`).
  - A crashing macro's report ends in an empty forwarded line.
  - A cycle between two type definitions names either type from run to run.
  - `derive_eq::derives_compose_in_every_order_and_preserve_docs` fails under the
    interpreter with a `when` stage cycle in `std.string`.
- **What the language server needs (Phase 4, task 14).**
  - Closure libraries are built synchronously inside expansion.
  - A server never unloads a library.
  - Manifests key entries by package, module path, macro and overload, so an
    older library can still be found by key.
  - A failed build leaves that macro's invocations unexpanded, and the rest of
    the file is still analyzed.
- **`if` and `when` are not templates.** Phase 1 kept them as ordinary macros
  because the self-calling template form needed sequence splices in expression
  position. Plain `$rest` now splices a sequence anywhere in compiled code, and
  in the interpreter outside item and type-parameter lists only through raw
  quotations.

## Decisions for this phase

1. **The backend moves into `staple-compiler`.** Phase 6 needs every
   `NameResolver` user to expand without the interpreter, including the
   compiler's tests and the language server. So the library build, its cache
   and the server client move from `staple-cli` into a compiler module
   (`macro_build`), and the engine becomes an option of `NameResolver`.
   - The CLI only configures it: cache roots, the server executable and the
     engine.
   - The server stays `staple __macro-server`. The compiler's tests find it
     through a small binary target in `staple-compiler` that calls the same
     `macro_runtime::server` code, or through `STAPLE_MACRO_SERVER`.
   - The fingerprint code in `cache.rs` moves with the backend. The object
     cache that normal incremental builds use moves too, rather than being
     split across two crates.
2. **A macro's result type is inferred from its body, as a function's is, and
   `parse_quote` needs a contextual type** (language decision). An unannotated
   macro stays valid wherever an unannotated function would be. Only a
   `parse_quote`, or a `Parse.parse` or `Parse.parse_all`, whose target type
   nothing determines is rejected. The interpreter's fallback that takes the
   type from the call site goes.
   - These stay valid, and pass under both engines at `3534a88f`:
     - `macro identity = expr: Expr => expr`, whose result is `Expr`, as for the
       same function;
     - `macro bare = expr: Expr => parse_quote { $expr + 1 }`, where the
       `parse_quote` is the macro's result.
   - These are rejected:
     - `let result = parse_quote { $expr + 1 }; result`. Nothing gives the
       binding a type. The compiled engine rejects it at `3534a88f`, and the
       interpreter accepts it.
     - `Parse.parse_all (quote { … })` with no target. Both engines reject it
       today, with different wording.
   - `Staple.md` already says this ("a `parse_quote` reached with no contextual
     type at all … is rejected"). Only the interpreter's call-site fallback
     made the 25 sources work.
   - The diagnostic names the fixes it already lists for `parse_quote`: annotate
     the binding, use `T :: expression`, or declare the macro's result type. It
     reads the same under both engines.

3. **Macro code follows the ordinary rules, and the sources that relied on the
   interpreter are fixed.** Refutable `let`, `move` markers that disagree with
   the annotation, unimported `std.syntax` types and arms with different types
   are errors in run-time code, and they become errors in macro code too. This
   isn't a new decision: Phase 2 already held closures to the ordinary rules,
   behind an opt-in check. Fix each affected test source. A test that exists
   to show the interpreter accepting such code changes to expect the error.
4. **A macro parameter must have a type** (language change). The type comes
   from the macro's annotation or from the parameter itself:
   - `macro identity: Expr -> Expr = expr => expr` and
     `macro identity = expr: Expr => expr` are valid;
   - `macro identity = expr => expr` is rejected under both engines.

   This replaces `Staple.md`'s rule that an omitted parameter type means
   `SyntaxNode`. Every macro then has an entry function, so strict mode's
   ``could not be run as compiled code`` is a compiler bug wherever it
   appears, and the comparison checks that no input produces it.
5. **The compiled engine's wording is the wording** (language decision). Macro
   code is checked by the ordinary type checker, so its diagnostics read like
   any other code's.
   - Tests that assert the interpreter's wording change to the compiled
     wording.
   - A panic keeps the macro's source position, since it locates the panic in
     the macro.
   - Stage-cycle diagnostics must be deterministic. Which item a cycle names
     follows a fixed order, such as source order, not hash order.
6. **Performance gates the switch.** Before task 11 changes the default, warm
   B1–B3 with the compiled engine must be within 25% of the interpreter, and a
   closure library must build in under 2 s on the development machine.
   - If a target can't be reached, record why in Findings and ask the user
     whether to switch anyway. Don't loosen the target silently.
   - Cold standard-library builds are not gated, because the install script
     prebuilds them. A build without prebuilt objects
     (`STAPLE_INSTALL_OBJECTS=0`) prints one line saying it is building the
     standard library's macros.
7. **`if` and `when` become templates if the interpreter can expand them the same
   way.** They are 561 and 119 of the calls in Phase 4's measurements, and a
   template expands in the parent with no server call. Phase 1's preferred form
   is an overloaded template that expands to a call of itself on the remaining
   arguments.
   - Try it first. If the interpreter can't splice the remaining arguments in
     expression position, extend its raw-quotation path to do so (Phase 3,
     task 7, did the same for item lists). If that isn't small, keep them
     compiled and record why.
   - Their diagnostics, such as a missing branch after `else`, must not
     change.
8. **Pruning keeps libraries another process may be using.** A build deletes a
   library only if it is neither one of the current fingerprints nor used in
   the last 24 hours. Loading a library updates its modification time.
   - This avoids lock files, which the command line and an editor would both
     have to honor.
   - Replacing a library is already an atomic rename, so a reader never sees a
     half-written file.
9. **The language server never builds a library during analysis.**
   - When a macro's library is missing or stale, analysis uses the newest
     library for that macro's stable key, if there is one. Otherwise the macro's
     invocations stay unexpanded, with an information diagnostic saying the
     macro is being compiled.
   - A background worker builds the missing libraries one at a time. It starts
     500 ms after the last edit and drops builds a newer edit has made stale.
     When a build finishes, the open documents are analyzed again.
   - A build failure is reported as a diagnostic at the macro, and its
     invocations stay unexpanded, as on the command line.
   - The server process is restarted after 64 library loads, instead of adding
     an unload message. A restart costs 10–13 ms, plus about 1 ms per library
     reloaded.
10. **`--macro-engine=interp` stays, hidden, until Phase 6.** The umbrella plan
    keeps the interpreter for one release cycle as an escape hatch. The flag
    is mentioned in the release notes, not in `staple --help`. The language
    server takes the same choice from its initialization options.
11. **The `O1` fault is investigated but doesn't block the switch.** No
    optimization level made calls faster (Phase 4, task 11). So libraries stay
    at `O0` until the fault is found. The fault may be a collector bug that
    optimized run-time code would hit too, so it gets a time-boxed
    investigation of its own (task 13).

12. **The standard library uses `if` and `when` again, and its derives stay
    hand-written** (decided). This branch removed both to break stage cycles:
    - Phase 1 hand-wrote the `Eq`, `Hash` and `Debug` impls of `True`, `False`,
      `None`, `Some`, `Ok` and `Ordering`, and wrote `Ordering` and
      `ConversionError` out without `typegroup`.
    - Phase 4 rewrote `if` and `when`, and `for` and `while` next to them, with
      `match` and `loop` in 15 files. These include `std.syntax`, `char`,
      `number` and `string`.

    **`if`, `when`, `for` and `while` come back once `if` and `when` are
    templates** (Decision 7). A template has no closure and is never compiled,
    so a use of it can't close a cycle, wherever it is. Then every rewritten
    use is restored, and the rule against flow macros in the syntax runtime
    goes. If `if` and `when` stay compiled, a use inside their own closure is a
    real cycle. Only uses outside it come back, and the rule stays.

    **The derives and `typegroup` stay off the types Phase 1 wrote by hand.**
    - A derive can't go on a type in its own closure, since the macro can't run
      before the item it modifies exists. The derive code uses `True`, `False`,
      `None`, `Some` and `Ok`.
    - `Ordering` and `ConversionError` aren't used by the derive code, but a
      closure takes every impl of a trait it holds for every type it holds. So
      restoring `@all(@derive_eq, @derive_hash) typegroup Ordering` still makes
      `Ordering` part of `@derive_eq`'s closure, and expansion reports a cycle.
    - Leaving such generated impls out of a closure optimistically was
      considered and dropped. It would make closures stop being a safe
      over-approximation, and it needs a cycle diagnostic mapped from a type
      error in both engines, for the sake of two types. Computing closures from
      checked code, with only the impls the type checker selects, would remove
      the over-reach properly. It is left for later (umbrella plan, open
      questions).

## Tasks

Tasks 1 to 5 close the differences between the engines. Task 6 gives the
standard library `if` and `when` back. Tasks 7 to 9 make the compiled engine
fast and safe to share. Task 10 moves the backend into the compiler. Task 11
switches the default. Task 12 is the language server, and tasks 13 to 15 finish
the phase.

### 1. A clean baseline

- Fix `derive_eq::derives_compose_in_every_order_and_preserve_docs`, which fails
  under the interpreter with a `when` stage cycle in `std.string`. Find which
  change caused it (Phase 4 rewrote `std` modules without `if` in task 6) and
  record it in Findings.
- Make the cycle between two type definitions name the same type on every run.
- Drop the empty forwarded line from a crashing macro's report.
- Done when `cargo test --release --workspace` passes with no failures, and
  `compare-engines.py` gives the same result on two runs.

### 2. Every macro has an entry function

- Implement Decision 4. A macro parameter with no type, from neither the
  macro's annotation nor the parameter, is an error under both engines, at the
  parameter. The diagnostic names both fixes: annotate the macro, or give the
  parameter a type.
- Update every source the error finds: `Staple.md`'s `choose` example, the
  integration tests' sources, and any standard-library or example macro. Use
  the type the parameter actually reads, usually `Expr`, not `SyntaxNode` by
  rote. A test that exists to show an untyped parameter changes to expect the
  error. Record the count in Findings.
- Remove the interpreter's `SyntaxNode` default for untyped parameters, and the
  entry-function exception for them (`check_program.rs:1259`).
- Make `compare-engines.py` count ``could not be run as compiled code`` and fail
  when it is non-zero. Fix every case it finds.
- Tests: `identity` with an annotation and with a typed parameter runs compiled;
  an untyped parameter, alone or mixed with typed ones, is rejected by both
  engines with the same diagnostic.

### 3. `parse_quote` without a contextual type

- Implement Decision 2. Remove the interpreter's call-site fallback, so the
  interpreter rejects what the compiled engine rejects. Use one diagnostic for
  both engines, worded as `parse_quote`'s existing one.
- Update the 25 test sources, and any standard-library or example macro the
  error finds, by annotating the binding or declaring the macro's result type.
  Record which fix each needed in Findings.
- Tests:
  - an unannotated macro returning a parameter (`identity`) and one ending in
    `parse_quote` are accepted by both engines;
  - an unannotated binding of `parse_quote`, and `Parse.parse_all` with no
    target, are rejected by both engines with the same diagnostic;
  - each of the three fixes the diagnostic names makes the macro valid.

### 4. Macro code under the ordinary rules

- Under the interpreter, run the closure check by default, as an error. This
  removes the last place where an invalid macro is accepted, and makes the
  engines agree before the switch. If the check makes interpreter builds
  noticeably slower, record the cost and run it only with the compiled engine
  instead.
- Fix the test sources that relied on the interpreter (Decision 3). Group the
  changes by kind in Findings.
- Enforce syntax ownership under both engines. Phase 3 made it part of the
  check.
- Change test expectations to the compiled wording (Decision 5), and make stage
  cycles deterministic.
- Done when `compare-engines.py` reports only the four inputs that only the
  interpreter rejects (a bare `Sequence` or `Separated` splice, and `Syntax` as a
  `parse_quote` context), each listed in Findings as intended.

### 5. `if` and `when` as templates

- Follow Decision 7. Write each as overloads that expand to a call of themselves
  on the remaining arguments. Keep `compile_error` for the missing-branch cases.
- Check that every diagnostic the two macros give is unchanged, and that the
  standard library still has no stage cycles.
- Measure B1 and B3 under both engines before and after.

### 6. `if` and `when` back in the standard library

- This needs task 5. If `if` and `when` became templates, restore every use this
  branch rewrote.
  - Take the old text from `main`, as it was before the rewrite (`git diff
    $(git merge-base HEAD main) HEAD -- stdlib`), and keep any later change to
    the same lines.
  - Then drop the implementor rule about flow macros in the syntax runtime,
    in this plan and in any code comment that states it.
- If they stayed compiled, restore only the uses outside their closures. Record
  which uses stayed rewritten and why.
- Check three things:
  - the standard library has no stage cycles under either engine;
  - its expansion is the same under both engines;
  - B1 and B3 don't get slower.

  The expansion comparison against the start of the phase will show the
  restored files. Explain them in Findings.

### 7. Call cost

- Let the parser start at a given offset, line and column, instead of padding a
  fragment with `stream_prefix`. Make the parser intrinsics keep the fragment's
  own line table, rather than rebuilding text from the start of the file.
- Profile what is left of a warm call on both sides, and remove the largest
  costs: argument trees, encoding, result reading.
- Profile the fixed cost of a warm run. B1 builds nothing and still takes 0.56 s
  against 0.33 s.
- Record per-call and B1–B3 figures after each change, against Decision 6's
  target.

### 8. Closure-library build cost

- A closure library spends about 1.2 s type-checking and 1.7 s lowering, most of
  it on code the closure doesn't own. Find what lowering seeds beyond the closure
  (Phase 0 found it seeds every non-generic template), and lower only what the
  entry functions reach.
- Check whether the type check repeats work its dependencies' macro-mode objects
  already did, and record what could be cached.
- Target: under 2 s per closure library (Decision 6). Record each step's time
  before and after, using `STAPLE_MACRO_STATS`.

### 9. Pruning and concurrent builds

- Implement Decision 8. A build doesn't delete a library that is current, or that
  was loaded in the last 24 hours. Loading a library updates its modification
  time.
- Tests:
  - two processes build and load the same package at once;
  - a library loaded by one process survives another process's prune;
  - a stale library older than the limit is removed.
- Then run `compare-engines.py` on the standard library's entries in parallel.

### 10. The backend in the compiler

- Move `CliMacroBackend`, the cache, the linker invocation and the server client
  into `staple-compiler`, as Decision 1 says. Give `NameResolver` an engine option.
  The CLI keeps argument parsing and passes its configuration.
- Make the server reachable from the compiler's tests (Decision 1). Tests share
  one cache directory per run, under `target/`, so the standard library's
  package library is built once per test run, or taken from the prebuilt
  installation.
- Measure the compiler's test suite with the compiled engine. If it takes more
  than twice as long as with the interpreter, find which tests build libraries
  and why, and record it. Many tests may share one macro, and a library is
  cached by its closure.

### 11. Switch the default

- Check Decision 6's targets, and that tasks 1 to 10 are done. If a target isn't
  met, stop and ask the user (Decision 6).
- Make `compiled` the default for every command, for `NameResolver` and for the
  compiler's tests. Keep `--macro-engine=interp` hidden (Decision 10).
- `scripts/compare-expansion.py` and `scripts/check-macro-closures.py` now run
  with the compiled engine. Record a new baseline for the comparison, and keep a
  way to run it with the interpreter until Phase 6.
- Run the full comparison, the closure check and the workspace tests with the
  new default, and record the results.

### 12. The language server

- Implement Decision 9:
  - analysis uses the newest library for each macro's stable key;
  - the background worker is debounced and drops stale builds;
  - open documents are analyzed again when a build finishes;
  - the server process restarts after 64 loads.
- The expander needs a way to ask the backend for "the newest library for this
  key" instead of "the library for this fingerprint". Add it to `MacroBackend`.
- Tests:
  - an edit to a macro shows its old expansion, then the new one after the
    build;
  - a macro that fails to build shows its diagnostic and leaves the rest of
    the document analyzed;
  - a stale build is dropped;
  - the server restarts after its load limit.

### 13. The `O1` fault

- Time-box to two days. Reproduce Phase 4's case: two top-level `if`s, then a
  later `if` in `std.number.types` whose `Sequence` argument loses its elements,
  with the standard library's package library at `O1`.
- Check the derived-pointer theory: does optimized code keep only an interior
  pointer to a live object across a collection? If so, the collector's stack scan
  needs to accept interior pointers. Normal optimized builds would need that too,
  so fix it as its own change, with a run-time test.
- Record the result. If it is fixed, measure `O1` libraries again, and choose the
  levels from those measurements.

### 14. Language reference

- Describe the compiled engine as how macros run. Remove or rewrite statements
  that only describe the interpreter. The "Compile-time evaluation supports …"
  paragraph and the interpreter's limits go in Phase 6, with the interpreter.
- Document that a macro's result type is inferred from its body as a
  function's is, and that `parse_quote` takes its type only from context,
  never from the call site (Decision 2). Also document that every macro
  parameter needs a type, from the annotation or the parameter (Decision 4).
- Say that the first build without a prebuilt standard library compiles its
  macros, and how long that roughly takes.
- Don't document `--macro-engine`.

### 15. Close out

- Run the expansion comparison, the closure check, the engine comparison and the
  workspace tests, and record the results.
- Update the umbrella plan: the Phase 5 status paragraph, and what Phase 6
  inherits. That includes every place the interpreter is still reached with the
  compiled engine as the default, and how `compare-engines.py` can be retired.

## Out of scope

- Deleting the interpreter, its static check and `--macro-engine` (Phase 6).
- Producing `ArgTree`s directly from argument matching, and removing the
  `@interpreted_only` items and the `ordinary_syntax_types` gates (Phase 6).
- Hosts other than macOS and Linux.
- An in-process mode for trusted libraries (umbrella Decision 3).
- Compile-time file reads such as `include_str` (umbrella open question 3).

## Done when

- `compiled` is the default for the command line, the language server and the
  compiler's tests, and `--macro-engine=interp` still works.
- `compare-engines.py` reports no ``could not be run as compiled code``, and only
  the differences listed in Findings as intended language changes.
- Warm B1–B3 are within 25% of the interpreter and a closure library builds in
  under 2 s, or the user agreed to switch without that, as recorded in
  Findings.
- Two builds of one package can run at once without breaking each other.
- The standard library uses `if`, `when`, `for` and `while` again, except where
  Findings records a real cycle.
- The language server builds libraries in the background, keeps analyzing with
  the newest library it has, and restarts its server after its load limit.
- The `O1` investigation has a recorded result.
- The closure check is clean, and the workspace tests pass with the compiled
  default.

## Implementation status

Update this section as you work (see "Rules for the implementor").

| Task | Status | Commit | Notes |
|---|---|---|---|
| 1. A clean baseline | done | `fa5f0b99` | Closure lookup falls back to the use site for names a macro wrote (the `derive_eq` cycle); constructors, singleton values and macro definitions are checked in declaration order; a dead server's forwarded output loses its blank lines. Tests: workspace 2378 passed, 0 failed. `compare-expansion.py` against `3534a88f`: 232/232. Closure check clean (232 inputs). `compare-engines.py`: the standard library 45/45 (one at a time), the rest 1904/2036, the same 132 differences on two runs. See Findings. |
| 2. Every macro has an entry function | not started | | |
| 3. `parse_quote` without a contextual type | not started | | |
| 4. Macro code under the ordinary rules | not started | | |
| 5. `if` and `when` as templates | not started | | |
| 6. `if` and `when` back in the standard library | not started | | |
| 7. Call cost | not started | | |
| 8. Closure-library build cost | not started | | |
| 9. Pruning and concurrent builds | not started | | |
| 10. The backend in the compiler | not started | | |
| 11. Switch the default | not started | | |
| 12. The language server | not started | | |
| 13. The `O1` fault | not started | | |
| 14. Language reference | not started | | |
| 15. Close out | not started | | |

## Measurements

_None yet._ For each one, record what was measured, the result, the machine, the
commit and the command. Include:

- warm B1–B3 and per-call cost under both engines, before and after tasks 5,
  6 and 7;
- closure-library build time per step, before and after task 8;
- the compiler's test suite under each engine (task 10);
- `O1` against `O0`, if task 13 fixes the fault.

## Findings

### Task 1

- **The `derive_eq` failure.** In `@all(@derive_eq, @derive_debug) typegroup
  Switch { … }`, the derives write `impl Eq Switch` and `impl Debug Switch`.
  The `Switch` in those impls carries the derive's module (`std.cmp`) as its
  definition module, and the closure lookup searched only that module, so the
  impls' self types resolved to nothing. An impl with no resolved self type
  joins any closure that holds its trait, as if it were for a builtin type. So
  `when`'s closure, which holds `Eq` and `Debug`, took the user's whole
  generated group, whose bodies reach `std.string`'s `Debug` and `decode_at`.
  Both use `when`, so `when` needed itself.
  - Fix (`macro_expand/closure.rs`): as in name resolution
    (`lookup_unqualified_type`), a name its definition module doesn't declare
    is looked up at the use site. A self type may also resolve to a macro
    call's unit, such as the `typegroup` call, because that unit declares the
    types its expansion does.
  - **The cause.** Bisecting from `88fdea78` to `686679d2` gives `1d473565`
    (Phase 4 task 6). The test passed before it. That commit made every quoting
    macro's closure include `__quote`, `Parse`, `Sequence` and `Splice` with
    their impls, which brought `Eq` and `Debug` into `when`'s closure. The
    lookup fault dates from `5da38086` (Phase 1), and the closure rules
    haven't changed since. The same commit rewrote `std` modules without `if`;
    the cycle doesn't pass through a rewritten item, but the two halves of the
    commit weren't tested apart.
- **The type-definition cycle.** `seed_constructors` and
  `seed_singleton_values` (`typecheck.rs`) walked hash maps, so the first
  declaration resolved, and the one the cycle is reported at, changed from run
  to run under either engine. They now go in declaration order:
  `type A wrap B` / `type B from (I32 | A)` always reports `B`.
- **The blank output line.** It came from stack overflows, not crashes in
  general: the runtime prints an empty line before its overflow message, and
  only the message was filtered. Blank lines at either end of a dead server's
  standard error are now dropped too (`crash_output` in `client.rs`).
- **Another ordering fault.** The first two `compare-engines.py` runs differed
  by one integration-test source: two macro definition errors came out in
  either order under both engines, at `3534a88f` too. `validate_definitions`
  (`macro_expand.rs`) walked the definitions' hash map; it now goes in
  declaration order.
- **The comparison.** The 132 remaining differences are the kinds Phase 4
  listed (macros without a declared result type, ownership differences, macro
  code valid only when interpreted, wording and ordering). The cycle-naming
  difference is gone. The integration tests' sources grew by two inputs, the
  new tests, which match.

### To record

Record:

- the cause of the `derive_eq` failure (task 1);
- the test sources changed for each language decision, grouped by kind (tasks 3
  and 4);
- whether `if` and `when` became templates, and if not, why (task 5);
- which flow-macro uses went back, and which stayed rewritten and why (task 6);
- whether Decision 6's targets were met, and the user's answer if not (task 11);
- the result of the `O1` investigation (task 13);
- every remaining difference between the engines, with its reason;
- anything that changes Phase 6.
