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
| 1. Check-program builder | done | see git log | `macro_expand/check_program.rs`. Unit tests cover closures across modules, dropped imports, metadata, modifiers, overloads, dependency packages and package-rooted imports. |
| 2. Typed quotation lowering | done | see git log | `macro_expand/lower_quote.rs`, `Splice`, `__quote` and `Parse.parse_quote` in `std.syntax`. `parse_quote` is a `Parse` trait member, not a free function (Findings, group 7). No splice of a non-syntax value was found that needs a `Splice` impl. |
| 3. Check driver | done | see git log | `STAPLE_CHECK_MACRO_CLOSURES` and `scripts/check-macro-closures.py`. Cost measured on B1–B3 (Measurements). |
| 4. Gap inventory | done | see git log | Groups 1–9 below. The full corpus ran (232 inputs; see "Review of this phase"). |
| 5. Close the gaps | done | see git log | Groups 3–7 and 9 are fixed. Group 1 keeps only its handle representation, which belongs to Phase 3; group 2 is excluded until Phase 3 rewrites the variadic `Parse` impls; group 8 goes to Phase 4. |
| 6. Static check after expansion | done | see git log | A ready macro is checked on its expanded closure just before its first run, and does not run if that fails. What was not checked by the end of the walk is checked then: ready closures as expanded, and unused definitions as written, with their macro calls selected and typed but not expanded. A separate context pass fills the tables the interpreter reads, and `Parse` members are checked there before the interpreter mutates them; an error there stops expansion. The lazy diagnostic path is gone. Known limit: a `const` initializer can still run a helper with a static error, as it could before this phase. |
| 7. Compile-time effects | decided, not enforced | see git log | Documented in `Staple.md` ("Compile-time effects"). |
| 8. Check in the test suite | done | see git log | `check_program.rs` checks every standard-library macro, the fixture macros and the macro example with `all`. Per-test environment variables would race between parallel tests, so one dedicated test covers them instead. |
| 9. Language reference | done | see git log | `Staple.md` describes the check, the quotation lowering and `Splice`, and now describes `Parse` implementations and macro code as ordinary code with the normal orphan rule. The interpreter's evaluation limits remain. |
| 10. Close out | done | see git log | Umbrella plan updated with the Phase 2 status and the work moved to Phases 3, 4 and 6. Corpus comparison and final timings are in Findings. |

**Phase 2 is done.** The clean check, the test-suite coverage and the
interpreter's static check all meet the "Done when" conditions. The remaining
items are assigned to Phase 3 (syntax-type handles and variadic `Parse` impls),
Phase 4 (the quotation cycle, generated-item loops and dependency library
builds) and Phase 6 (delete the interpreter and its static check).

## Measurements

Release build, `STAPLE_CHECK_MACRO_CLOSURES=<mode> target/release/staple expand
--stdlib stdlib <input>`, min of two or three runs on the development machine
(Apple M2). The machine occasionally stalled a process for 25–29 s while the
host did other I/O; those runs were discarded. The inputs are the Phase 0
baselines:

- B1: `let x: I32 = 1`, so the time is loading and expanding the standard
  library.
- B2: 20 × `@derive_eq` on a 50-field `wrap` product.
- B3: one function with 1,000 `if … else …` calls.

| Input | off | `1` | `all` |
|---|---|---|---|
| B1 | 0.285 s | 3.54 s | 3.55 s |
| B2 | 0.29 s | 3.55 s | 3.56 s |
| B3 | 0.62 s | 3.88 s | 3.84 s |

The check costs about 3.2–3.3 s, roughly one front-end pass over the standard
library and the program. `1` and `all` cost the same here because an invoked
standard-library macro makes the check take the library whole; `1` only saves
work in a program whose macros are all in its own package.

## Findings

### Design changes made while implementing

- **A package other than the program's own is checked whole.** Decision 1 says
  the package being expanded contributes only its closure items. That cannot
  hold for the standard library when a user macro uses `if`: the closure of `if`
  lives in `std`, and checking it as closure-only needs `Splice` and `__quote`
  through quotations (group 8). So the check takes the program's own package
  (the package of the entry module) closure-only, and any other package, with
  its dependencies, whole, with all of its macros converted. This matches how
  Phase 4 would build a dependency's macros: with the dependency.
- **A macro stays declared in the check program, without its body**, next to
  its generated function, so `use` of the macro still resolves.
- **The check needs compile-time items expanded.** The module walk leaves the
  bodies of `Parse` implementations unexpanded (the interpreter runs them as
  written), and helpers and macro bodies of macros nothing invoked are not
  expanded at all. The snapshot expands both, on copies, so what the
  interpreter runs does not change. All macros are made ready under the check,
  which expands their closures.
- **`parse_quote` is a `Parse` trait member (`Parse.parse_quote`)**, not the
  free generic function Decision 3 names. The interpreter's static check
  accepts a `Parse` bound on a type parameter only in an `impl`, so a free
  `def __parse_quote = <T where Parse T> …` is rejected by it. This goes away
  when task 6 or Phase 6 removes that check.
- **The syntax intrinsics are declared, not implemented, for the resolver.** The
  resolver no longer reports `unknown Staple intrinsic` for `std.syntax`, whose
  functions the macro runtime will provide.
- **`Splice` impls are compile-time items** (dropped after expansion, like
  `Parse` impls).
- **The standard library is always checked whole**, even when the corpus input
  is a standard-library file. In a real build the library is a dependency, and
  a corpus file is not a package build of `std`; checking it closure-only made
  every import from another `std` module and every contextual extern block look
  missing. The package of the entry module is still closure-only for any other
  package, so closure completeness is still tested.
- **The static check runs after expansion.** A macro that became ready is
  checked on its expanded body and expanded closure helpers. A definition that
  nothing called is still checked as written, but its macro calls are not
  resolved or expanded, which keeps the cheap diagnostic for unused code
  without the lazy expansion path. The interpreter still reads contextual types
  from the static check, so a first pass fills those tables from the written
  syntax and discards its diagnostics; it does not expand anything. `Parse`
  members are checked in that first pass, because the interpreter replaces them
  with expanded bodies the first time it runs one.
- **A submodule or companion name is not an importable item.** The check's
  import filter used to count those names, so `use package.hash.(Hash, Hasher)`
  survived when `Hasher` was only the companion module and the type had been
  left outside the closure.

### Gap inventory

Counts are diagnostics from `STAPLE_CHECK_MACRO_CLOSURES=all staple check` on a
three-line program, which checks every standard-library macro, unless noted.
Most diagnostics in a group have one cause, so the counts show size, not the
number of defects.

1. **Syntax types are opaque in the real front end. Partly fixed; the rest is
   Phase 3.** The resolver registers the `std.syntax` types as
   `BuiltinType::Syntax`, which the checker treats as opaque handles, so
   patterns such as `Sequence (first, rest)` failed (about 800 diagnostics on
   the standard library). In the check program only the handle types (`Syntax`
   and the `Unstructured*` types) stay opaque; the data types are ordinary
   types, and three front-end changes make macro code on them check:
   a `@recursive_constructor` type may refer to itself directly (the
   self-reference is a stand-in, as for recursion through `Shared`); a product
   pattern written against a wrapper of a sum selects the alternative of its
   width (`Sequence (first, rest)`), in binding, checking and coverage; and a
   binding whose type is a stand-in is unfolded before it is matched. None of
   this runs for ordinary programs, which never see these types: they are gated
   on the mode, and tests in `tests/compiler.rs` hold ordinary programs to the
   old rules. Later work removed the remaining diagnostics, so the check of the
   standard library's macros is now empty. Phase 3 still has to give the types a
   handle representation.
2. **`impl` targets the front end rejects. Open; Phase 3.** `trait
   implementation target must be fully concrete` for the `Parse` impls on
   variadic products (`(H; ...T)`, `(H, ...T)`) and on `Ident<S>`. These impls
   were never seen by the front end. The check leaves the variadic ones out
   (`is_variadic_parse_impl`), so it cannot be clean about them: a call such as
   `Parse.parse.<(Expr; Expr)>` is reported as having no implementation. Phase 3 rewrites the `Parse` impls
   (Decision 4 of the umbrella plan), so it removes or reshapes them.
3. **Names macro code uses without importing. Fixed.** `String` in `std.cmp`,
   `std.default`, `std.hash` and `std.typegroup`, and `Ok` in `std.typegroup`.
   The interpreter resolved them without an import. Added the imports.
4. **Private representations. Fixed.** Patterns on `AliasBody`, `WrapperBody`
   and `FromBody` from other modules were rejected, because the types were
   declared `wrap` without `pub`. They are now `wrap pub`, which matches how
   the other syntax data types are declared.
5. **Compile-time code the snapshot had not expanded. Fixed** (see above).
   Without it, `if … else …` in `derive_eq` and `derive_hash` helpers showed
   up as `macro `if` must be invoked` and `unknown name `else``.
6. **Quotation and splice typing. Fixed for the cases found.** No macro
   splices a non-syntax value, so no case needed a `Splice` impl beyond the
   syntax types. A name that is spliced twice in one quotation is passed once.
   In the check, syntax values are copied freely, so a name used again after a
   quotation is not a use after move. Other opaque types are not copied this way.
7. **The interpreter's static check is stricter than the front end about
   generic helpers.** It needs a `Parse` bound on a type parameter to be on an
   `impl`; it reports `(String)` for the one-field pattern `ParseError message`
   (the front end unwraps it); and `non-exhaustive match` on a `Sequence<T>`
   with a type-parameter element. New helpers were written to avoid these. They
   go away with task 6 or Phase 6.
8. **Quotation lowering adds edges the closure scan cannot have. Open;
   Phase 4.** A quotation calls `__quote`, `Splice.splice` and (for
   `parse_quote`) `Parse.parse_quote`. Counting these as references of the
   macro gives a cycle: `when` → `Splice` impl for `Expr` → `CharExpr` → `Char` →
   `len_utf8`, which uses `when`. It showed as `macro `when` is needed to
   expand code that `when` itself depends on`. For now the closure scan does not
   count quotation calls, and the check takes `std.syntax` whole. Phase 4 has
   to break the cycle (for example `Char` written without `when`, or a
   splice impl set that does not reach `Char`) before closure-only builds of
   the standard library can compile quotations.
9. **Imports in partial own-package modules. Fixed.** The filter did not
   resolve `package.`-rooted imports, because `Program::imported_module` only
   knows paths relative to the importing module, and it counted a submodule or
   companion name as an importable item. `use package.hash.(Hash, Hasher)`
   therefore survived when `Hasher` was only the companion and the type was
   outside the closure. The check resolves a package-rooted path against the
   program's dotted module names and keeps only names defined by items, not by
   submodules. The standard library is also checked whole when a corpus input
   is itself a standard-library file (see "Design changes"). Every `stdlib/`
   input now passes.

The standard library's macros now check with no diagnostics, so the gaps that
group 1 used to hide were looked at again. No macro relies on a `?` on a
non-`Try`, a `return` from a nested block, a by-address `mut` difference, an
integer literal with no width, or a `collect` without an expected type: the
front end accepts every closure, and `Staple.md` now describes the shared
semantics. The two `typegroup ConversionError` chains are still recorded as
generated-item loops but produce no check failure; Phase 4 still has to decide
whether its closure-only build needs to break them. The check passes without
narrowing closures with selected impls, so there is no new evidence for that
change; it stays a Phase 4 option.

### Review of this phase

A review found ten problems in the phase's work. All were addressed in one
change, with tests except where noted:

- Refinement patterns, narrowing a wrapper with a product pattern and unfolding
  a stand-in also applied to ordinary programs. They now need the check's mode.
- Mode `1` checked every macro. It now checks invoked macros. Because a
  dependency package is checked whole once one of its macros is, an invoked
  `std` macro still checks all of `std`, so `1` costs about the same as `all`.
- A resolver that failed without a diagnostic counted as a pass, and its fallback
  lost the pre-expanded state. It now reports an error and keeps that state.
  **Not tested:** nothing in the test suite can make the resolver fail this way.
- `$ name` with a space was not seen as a splice.
- Every opaque type was treated as copyable syntax. Only `std.syntax` types are
  now. The syntax types are still not declared `Copy`, so the mode checks that
  Decision 6 wanted gone remain.
- The widened `compile_types_compatible` also loosened the interpreter's static
  check in normal builds. It is narrower, but `Sequence` of `SyntaxNode` against
  a written type stays lenient.
- Variadic `Parse` impls were dropped with no record. The exclusion stays and is
  now documented (group 2).
- `std.syntax` accepted any unknown intrinsic in the check. It now accepts only
  the closed list in `is_macro_runtime_intrinsic`.
- Smaller: a doc comment on the wrong method, a quadratic lookup and a redundant
  `Option`.

Left as it was: other scans for `$` followed by a name
(`macro_expand.rs`, `macro_expand/scan.rs`) still miss the spaced form. They
predate this phase. Coverage shortcuts in `coverage_is_useful` are not gated on
the mode; a probe with `Result<I32, Never>` behaved as before.

After these fixes:

- `cargo test --workspace` passes.
- `scripts/compare-expansion.py` against `996e1bee`: 232 of 232 inputs match.
- `scripts/check-macro-closures.py --mode all` over the corpus: 232 inputs, 46
  with diagnostics. All 45 `stdlib/` inputs fail, as they do on the binary from
  before these fixes: each treats `std` as the program's own package, in
  closure-only mode (group 9). The imports explain some of the roughly 350
  messages. The rest (`unknown name `Splice``, `unknown name `__pointer_read``)
  are not traced. The last was `staple-compiler/examples/macros.sta`, resolved
  below.
- `staple check` of a small program using a template macro, one run each, on a
  machine that was also busy: off 1.11 s, `1` 7.16 s, `all` 7.12 s.

A macro's result is no longer assumed. The check used to ascribe `Expr` to the
result of every unannotated macro, which rejected a bare `quote`, a form
`Staple.md` documents, and was wrong for a macro that returns an item. Now a
`quote` result is `Syntax` with no annotation, and a `parse_quote` needs the
macro's declared result or `Type ::`; without either the check reports `no trait
implementation or matching bound is available`, which does not name the rule.
Expansion still infers the result from the fragment's shape, so normal builds
accept what the check rejects, until Phase 6 removes that inference.

This meant annotating `while`, `discard` and `for` in `std.flow`,
`replace_argument` in `staple-compiler/examples/macros.sta`, which also gained
its missing `Syntax` import, and `widen` and `identity` in the
`conversions_as.sta` fixture. After it:

- Every input outside `stdlib/` passes the check (187 inputs).
- `scripts/compare-expansion.py` against the capture before it: 229 of 232 match.
  The three that differ are the three edited files.
- `cargo test --workspace` passes.

Finishing the phase (2026-11-05):

- `scripts/check-macro-closures.py --mode all`: 232 inputs, 0 with diagnostics.
- `cargo test -p staple-compiler --test compiler`: 845 passed.
- `cargo test -p staple-compiler --lib`: 510 passed, 2 ignored.
- `scripts/compare-expansion.py` against `62d80109`: 226 of 232 match; the six
  differing inputs are the standard-library files listed below.
- The check costs about 3.2–3.3 s on B1–B3 (Measurements).

No test needed changing for diagnostic order. The check's diagnostics now come
after expansion, but the suite only asserts on their content, not their order.

A second review, of the work that closed the phase, found problems in moving
the static check after expansion and in the check's imports. Fixed, with tests:

- A helper's form depended on which macro reached it first, and definitions are
  held in a hash map, so `staple check` of one unchanged file passed or failed by
  chance (8 passes in 20 runs). A helper's form now depends only on whether a
  ready closure holds it, and definitions and helpers are visited in syntax order.
- A macro with a static error ran before the error was reported. It is now
  checked before its first run, including a macro that shares a helper already
  rejected. An invalid `Parse` impl stops expansion again.
- A wrong macro call in an unused macro was not reported. It is again.
- A call that failed to expand also reported "had not been expanded". That
  report now appears only when nothing else has been reported.
- Imports of own-package submodules were dropped, which reported `unknown name`.
  Companion modules still do not count as importable.
- Mode `1` failed on standard-library files, because the snapshot and the builder
  disagreed on whether `std` is checked whole.
- `Staple.md` described the check's `?` as what a default build does.
- Package-rooted imports in a real `staple.kdl` package now have a test.

After these fixes, `scripts/check-macro-closures.py` reports nothing on all 232
inputs in both `all` and `1`, `scripts/compare-expansion.py` matches on all 232
against the capture before them, and `cargo test --workspace` passes.

### Expansion difference from the start of the phase

`scripts/compare-expansion.py` against `62d80109` (binary and standard library
built from that commit, current corpus): 226 of 232 inputs match. The six that
differ are the standard-library files this phase edited: `std/cmp.sta`,
`std/default.sta`, `std/hash.sta`, `std/typegroup.sta` (new imports),
`std/syntax.sta` (`Splice`, `Parse.parse_quote`, three intrinsic declarations,
`wrap pub` on three body types) and `std/flow.sta` (`Expr ::` on `while`,
`discard` and `for`, added when an unannotated macro stopped assuming an
expression result). Expansion of every other input, including all fixtures and
corpus programs, is unchanged.

### Later phases

- **Phase 3** must declare the syntax types as real types (group 1), rewrite or
  remove the `Parse` impls the front end rejects (group 2), and implement
  `__quote`, `__splice_node`, `__splice_join` and the template table exactly as
  lowered here: ids index `TemplateTable::templates` in the order the check
  program lowers them, and `Splice.splice` is applied once per distinct name in
  order of first use.
- **Phase 4** must resolve the quotation cycle (group 8), decide how a
  dependency's macros are built (the check checks them whole), and decide
  whether the generated-item loops in `std.convert` matter to a closure-only
  package build. It also carries the option of narrowing closures with the
  typechecker's selected impls, for which this phase found no new evidence.
- **Phase 6** deletes the interpreter's static check, which removes group 7.
