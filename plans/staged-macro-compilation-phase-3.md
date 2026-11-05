# Staged macro compilation, Phase 3: syntax runtime

Breakdown of Phase 3 in [`staged-macro-compilation.md`](staged-macro-compilation.md).
Line references are against `docs/staged-macro-plan` at `ef52149a`
(2026-11-05), the end of Phase 2.

Phase 2 proved that macro code *type-checks* as ordinary code. Phase 3 makes it
*run*: syntax values get a real representation, the intrinsics get real
implementations, and a macro can be called from Rust with real arguments and
give back real syntax.

There is no macro server and no shared-library build in this phase (Phase 4).
Tests run compiled macro code in-process through a JIT harness. Normal builds
still expand with the interpreter, and their output must not change.

## Rules for the implementor

- **Keep the status section of this file up to date.** Each time you finish a
  task, find a blocker or measure something, update
  [Implementation status](#implementation-status) in the same commit. Mark the
  task done, partly done or blocked, add the commit, and write down what you
  found. Anything that changes the plan goes in [Findings](#findings). When the
  phase ends, update the Phase 3 section of the umbrella plan with a short
  status paragraph, and change later phases where the findings disagree with
  them.
- Commit everything to `docs/staged-macro-plan`, with no pull requests. A task
  can take several commits, but the tests pass after each one.
- Don't refer to the plan files or phase numbers in the code and
  documentation, except from the plan files themselves.
- Don't run `staple fmt` (see `AGENTS.md`), and don't use its output in tests.
- Interpreter expansion must not change. Run `scripts/compare-expansion.py`
  against the start of the phase after every task that touches the expander or
  the standard library, and explain every difference in Findings.
- `scripts/check-macro-closures.py --mode all` stays clean after every commit.
- Don't add `if`, `when`, `for` or `while` to new code in `std.syntax`. Write it
  with `match` and `loop`. Every macro with a quotation reaches this code, so a
  flow macro here adds to the quotation cycle Phase 4 has to break (Phase 2,
  group 8).

## What already exists

All of these were read from the source at `ef52149a`.

- **Quotations are already lowered for the check.**
  `macro_expand/lower_quote.rs` rewrites `quote` to `__quote (id, splices)` and
  `parse_quote` to `Parse.parse_quote (id, splices)`. `splices` holds
  `Splice.splice name` once per distinct name, in order of first use. But
  `TemplateTable` only stores each fragment's *text* (`lower_quote.rs:33`),
  which is not enough to instantiate it: tokens carry hygiene and origins.
- **The check program exists.** `macro_expand/check_program.rs` builds it
  (`build_check_program`, `:202`), with macros converted to generated functions
  and compile-time items kept. Lowering has never seen this program.
- **The front end only accepts these types behind mode gates.** The resolver's
  `ordinary_syntax_types` makes the data types ordinary and leaves `Syntax` and
  the `Unstructured*` types as `BuiltinType::Syntax` opaque handles
  (`resolve.rs:2655`–`:2671`, `typecheck.rs:21613`). Copying syntax freely, direct
  self-reference through `@recursive_constructor` (`typecheck.rs:20943`), and
  refinement patterns such as `Sequence (first, rest)` are also gated on the
  mode. `Syntax` and the `Unstructured*` types even say `impl !Copy`
  (`stdlib/std/syntax.sta`).
- **Macro code relies on the copy exception.** The check treats syntax as
  copyable (`is_syntax_value`, `typecheck.rs:21723`, used at `:3367` and `:10923`).
  A temporary probe at `ef52149a` removed that exception for the copy check at
  `:3367` and ran the closure check over the standard library's macros. It gave
  33 errors: `typegroup` 8, `default` 7, `cmp` 5, `hash` 5, `fmt` 4, `flow` 3,
  `syntax` 1. Nearly all are "cannot move out of a borrowed value", since
  parameters borrow by default. This is a lower bound: no "use of moved value"
  errors appeared, which may only surface once these are fixed, and the fixtures
  were not run.
- **The syntax data types are infinitely sized as written.** `CallExpr` holds
  two `Expr` values, and `Expr` includes `CallExpr`. `Sequence<T>` holds a
  `Sequence<T>`. The checker uses stand-ins, but nothing gives them a layout.
- **The declared types have nowhere to keep source syntax.** `Ident<S>` wraps a
  spelling, `CallExpr` has only `callee` and `argument`, and `Braced<C>` wraps
  only its contents. The interpreter keeps the source in `SyntaxValue`
  (`macro_expand.rs:293`), which is how hygiene, spans and trivia survive. This
  is the main design problem in this phase.
- **Several intrinsics can't be implemented by a host function as declared.**
  `__parse_primitive: <T> …`, `__parenthesized: <C> (Syntax; C) -> …` (and
  siblings), `__sequence_push: <T> …`, `__splice_node: <T> …` and
  `__splice_join: <T> …` are generic over Staple types whose layout the host
  doesn't know. The closed list is `is_macro_runtime_intrinsic`
  (`macro_expand/parse.rs:40`).
- **The parsing logic is written against the interpreter.** `parse_primitive`
  (`macro_expand/parse.rs:1071`) mixes reusable parts (`parse_expression_prefix`,
  `syntax_slice`, `describe_position`) with `Value` construction. So do the
  quote-filling helpers in `macro_expand.rs` (`append_syntax_tokens`, `:907`) and `match_macro_arguments`, which returns `MacroArgument`s that
  contain `Value`s.
- **`Parse` impls on variadic products and on `Ident<S>` are rejected** by the
  front end, so the check leaves the variadic ones out
  (`is_variadic_parse_impl`, `check_program.rs:391`, Phase 2 group 2). `Staple.md` documents
  `Parse.parse.<(A; B)>`, and macros use `Separated<(Ident; Equals; Expr), Comma>`.
- **A host can already call compiled code in-process.** Tests in `lower.rs` JIT
  a module with `create_jit_execution_engine` (for example `lower.rs:16247`). Phase 0 found no build-time gain from the JIT, but it needs no
  linker, so it suits a test harness.
- **Bytes can cross without per-byte calls.** `std.syntax` is in the same package
  as `string_shared_bytes` (`string.sta:37`) and `SharedBuffer.unsafe_data`
  (`shared_buffer.sta:21`), both `pub(package)`. Phase 0's findings 5 and 9 ask
  for whole-buffer transfers.

## Decisions for this phase

1. **`std.syntax` data types become boxed nodes with a hidden origin slot.**
   This is a new standard-library-only modifier, `@syntax_node`, for each
   recursive or source-carrying data type (`Ident`, `CallExpr`, `StringExpr`,
   `CharExpr`, `Sequence`, `Separated`, the delimiter types, `TypeParameter(s)`,
   and the structured `Type`, `Pattern`, `Item` and `Modifier` nodes).
   - **Run-time representation:** a pointer to an immutable GC-allocated
     `(representation, origin: U64)`. Constructors allocate with origin `0`.
     Patterns and field reads load from it. A field update (`changed.argument =
     r`) copies the box and keeps the origin.
   - **Front end:** the surface doesn't change. `Ident name`,
     `CallExpr (callee: a, argument: b)`, `Sequence (first, rest)` and
     `Braced (contents)` all read as they do today. Only two intrinsics see the
     slot: `__origin: <T> T -> Syntax` and `__with_origin: <T> (T; Syntax) -> T`.
     Ownership is checked as for any other wrapper (Decision 2), so a pattern can
     move a field out. The types are not `Shared` or `Unique` at the surface:
     `Shared` is `Copy` and forbids moving a non-`Copy` payload out. `Unique`
     would change how macro code binds and builds fields (an untyped binding gets
     the handle), and the interpreter has no memory model to run it. The box is purely a run-time representation with no
     `Drop`, so a move copies a pointer. It may still reuse `Gc.allocate`.
   - **Why:** one mechanism solves both the infinite size and the lost source.
     Rejected alternatives:
     - A visible `origin` field would change every macro's patterns, and the
       interpreter would have to pretend to have it.
     - Handles for all nodes, with intrinsics behind patterns, would mean
       special-casing patterns and field access throughout the front end.
     - Rebuilding from children alone would lose trivia and the spans of
       unchanged nodes, so expansions would differ from the interpreter's.
   - The box stores the spelling of `Ident<S>` as a `String` for every `S`, so a
     value's layout doesn't depend on whether its type is `Ident<"else">` or
     `Ident<String>`.
   - Task 2 starts by proving this works in lowering. If it fails, stop and
     raise the design again before going on. Don't work around it.
2. **Most syntax is non-`Copy`, `Clone` is a deep copy, and a splice borrows.**
   This replaces the check's copy exception with real declarations.

   | Kind | Types |
   |---|---|
   | `Copy` | `Ident`, `Comma`, `Equals`, `FatArrow`, `Visibility`, `ParseStream` |
   | Non-`Copy`, `Clone` | `Syntax`, `Expr`, `Type`, `Pattern`, `Item`, `Modifier` and their nodes, the delimiter types |
   | Follows its elements | `Sequence<T>`, `Separated<T, S>` |

   - **A splice borrows.** `Splice.splice` stays `T -> Syntax`, which borrows today
     and becomes `ref T -> Syntax` under the planned memory model, with no change
     to macro code. A quotation keeps Phase 2's one `Splice.splice` per distinct
     name. A spliced parameter or match binding needs no clone. This is how Rust's
     `quote` treats `#x`. A moving splice was rejected: spliced values are usually
     borrowed, so it would need a clone or `move` at most splices, and a `ref` could
     not remove that need later.
   - **A name may repeat in a quotation.** A splice only reads its value, so nothing
     limits how many times one quotation uses a name, and `quote` allows the same.
     - `parse_quote { $value + $value }` is valid. It emits the expression twice, so
       the generated code evaluates it twice. A macro that wants one evaluation writes
       `{ let v = $value; v + v }`.
     - The expansion goes through the normal front end, so a repeated `Item` is
       reported as a duplicate definition and a moved variable used twice as a use
       after move.
     - A rule against repeats of `Expr`, `Item`, `Pattern` and `Syntax` was dropped.
       It would refuse correct code such as `match x { A => $body, B => $body }`, it
       couldn't see duplication across two quotations, and cloning the value would
       silence it while still emitting the expression twice.
   - **Why nodes are non-`Copy` anyway.** A deep copy should be visible where it
     happens, a node can't be duplicated by accident when it is stored or returned,
     and this matches the ownership the planned memory model gives other types.
   - **`Clone` is a deep copy of the structured nodes.** Each node has a real `Clone`
     impl in `std.syntax`. Copies keep their origin handles, so an unchanged clone
     still rebuilds to its original tokens (Decision 3). The copy stops at `Syntax`
     and the `Unstructured*` handles: their arena fragments are immutable, so copying
     the handle can't be observed. `Clone` for `Sequence<T>` loops instead of
     recursing, so a long sequence doesn't use deep stack. `duplicate_modifier` is
     replaced by `Clone`, since the parent renumbers syntax ids in every result.
   - **The interpreter doesn't enforce any of this.** `Clone` on syntax is an
     identity there (`@interpreted_only`), because rebuilding a node through its
     constructor would lose the value's hidden source. Until Phase 5, normal builds
     accept what the closure check rejects, as they already do for an unannotated
     `parse_quote`.

3. **Mutation is detected by comparing children, not with a dirty flag.** Each
   `Splice` impl is Staple code that splices its children and asks the host to
   rebuild: `__call_syntax (origin; callee_syntax; argument_syntax)`. The host
   returns `origin` when the origin's children are those same handles, and new
   tokens otherwise. An unchanged tree gives back its original handle all the way
   up, so it stays lossless. A changed node gets new tokens, and its unchanged
   children keep theirs.
4. **Both engines share `std.syntax`, with two bodies where they must differ.**
   The interpreter is still the default and needs `__parse_primitive`. Compiled
   code can't use it. Two temporary built-in modifiers mark the differences:
   `@interpreted_only` and `@compiled_only`. The expander drops `@compiled_only`
   items, and the check program drops `@interpreted_only` items. Everything that
   can be shared stays shared. Phase 6 deletes the interpreted items and the
   modifiers.
5. **The ABI carries scalars, handles and byte buffers, nothing else.** Every
   generic intrinsic in the list above is replaced with non-generic intrinsics
   plus Staple code. Host functions are `extern "C"` Rust, each named
   `__staple_syntax_<name>`, and lowering treats a macro-runtime intrinsic as a
   call to that symbol. A table of `(name, pointer)` pairs is the one source of
   truth, and the JIT harness here and the library binding in Phase 4 both read
   it. Handle `0` means "none".
6. **Shared code moves out of `MacroExpander`.** The reusable parts of parsing,
   quote filling and fragment matching become plain functions that work on
   `Syntax` and AST nodes, in `macro_expand/syntax_ops.rs`. The interpreter and
   the runtime both call them, and they survive Phase 6.
7. **Runtime code lives in `staple-compiler/src/macro_runtime/`:** `arena.rs`,
   `intrinsics.rs`, `quote.rs`, `args.rs`, `wire.rs`, `harness.rs`
   (`#[cfg(test)]`). `staple-cli` can depend on it for the server in Phase 4.
8. **Arguments cross as a tree, and decode through a trait.** This replaces the
   umbrella plan's "one decoder generated per `MetaType`".
   - The parent turns a match into an `ArgTree`: `Node(handle)`, `List(children)`,
     `Absent`, `Group(origin, child)`.
   - A new trait, `FromMatch T { from_match: MatchNode -> T }`, has impls next to
     `Parse`, in the same shapes: leaves build nodes, `Sequence`, `Option`,
     `Separated`, delimiters and products recurse.
   - Each macro gets a generated entry function in Staple:
     `(arguments: MatchNode) -> Syntax`. It decodes each parameter, calls the
     macro's function and applies `Splice.splice` to the result. The check covers
     the entry function like any other code, so a macro whose result can't be
     spliced becomes a type error.
9. **Products get fixed-arity impls.** Variadic `impl` targets stay rejected.
   `std.syntax` declares `Parse`, `FromMatch` and (where needed) `Splice` for slotted
   and comma products of arity 2 through 8, written out by hand. A larger product
   has no implementation, which the type checker reports. If the corpus uses a
   larger arity, raise the limit and record it. If `impl<A, B where Parse A, Parse
   B> Parse (A; B)` turns out to be rejected as well, fix that rule in the
   checker for all programs instead of synthesizing impls per use.
10. **Results and arguments cross the wire as token fragments, not ASTs.** The wire
   format carries tokens (kind, text, span, origin, hygiene) and each fragment's
   `definition_module`, `expansion_mark` and `identifier_origins`, with strings
   interned. The receiver parses a fragment when it needs a node. The parent
   reparses a result by the macro's declared category. This is simple, but it
   relies on reparsing being faithful, so task 11 tests exactly that.
11. **The parent still owns hygiene.** Each call carries a context: the macro's
    definition module, the fresh expansion mark and the call-site span. `__quote`
    stamps template tokens from it, as the interpreter does. A constructed
    identifier takes its hygiene from it. The parent renumbers syntax ids in
    every result. Read how the interpreter handles a quotation inside a helper
    from another module, and match it.
12. **`compile_error` records and then exits.** The host records the message, and
    the Staple function calls `exit`, which Phase 4's trap catches. In this phase,
    test the recording in Rust and test the `Result` paths in Staple. End-to-end
    coverage of `compile_error` waits for Phase 4.

13. **A sequence splices with a plain `$name`.** `$name...` is removed, with no alias.
    - The value's type already says it is a sequence. `Splice` is implemented for
      `Sequence<T>`, and `$name` and `$name...` already lower to the same
      `Splice.splice name`. The elements are placed side by side either way, with no
      separators, and an empty sequence inserts nothing.
    - `...` only works in item lists, type-parameter lists and raw-token substitution,
      so removing it makes a sequence work in every position.
    - It also frees `...` for spreading. The parser ignores the space before it, so
      `f $a ...$b` is read as a repeated splice and rejected today.
    - The grammar conflict is only in the interpreter. It parses `$name` followed by an
      item as a visibility splice (`$visibility type ...`), and it doesn't accept
      `$items` at the end of an item list. Compiled quotations fill in tokens before
      parsing, so they don't have this conflict.
    - The interpreter should send a quotation that splices a sequence through its
      existing raw-token substitution, chosen from the value as
      `quote_contains_raw_splice` does for raw fragments. Task 7 starts by proving this.
      If it fails, stop and raise the design again. Don't keep `...` to work around it.

## Tasks

The tasks are in dependency order.

### 1. JIT harness and lowering inventory

- Add `macro_runtime/harness.rs`, test-only. It runs the check program through
  `Lowerer` and codegen, JITs it, maps intrinsic symbols with
  `add_global_mapping`, runs initialization and calls a named function. A
  compiled program costs seconds, so share one per test binary, as far as
  possible.
- Lower the check program for the standard library's macros and write every
  distinct failure into Findings, grouped by cause, as Phase 2 did. This is the
  list of what tasks 2 to 9 have to fix, and it may add to them.
- Make the macro-runtime intrinsics lower as external symbols (Decision 5), with
  stub host functions at first.

### 2. Boxed nodes

- Add `@syntax_node` (Decision 1) with `__origin` and `__with_origin`, and
  first prove the design: `Sequence<Ident<String>>` and an `Expr` tree are
  constructed, matched, updated and compared in compiled code, under the harness.
  Record the result in Findings before going on.
- Run-time matching of refinement patterns: `first: Ident<"else">` against an `Expr`
  or `SyntaxNode` compares the spelling at run time. `Sequence (first, rest)`
  selects the alternative by width. Test both from compiled code.
- Remove each mode gate that boxed declarations make unnecessary, and list what
  stays in Findings. `ordinary_syntax_types` itself stays until Phase 6, since
  normal builds still must not see these types as data.

### 3. Ownership of syntax

Do this as one change, so the closure check stays clean at every commit.

- Declare `Copy` and `Clone` as Decision 2 says, and remove the `impl !Copy`
  declarations only where the table contradicts them. `Sequence<T>` and
  `Separated<T, S>` are `Copy` when their elements are. If conditional `Copy` isn't
  supported on these types, record it and raise it before working around it.
- Remove the copy exception (`is_syntax_value`, `typecheck.rs:3367` and `:10923`).
  An `@` pattern on a non-`Copy` node is then an error, as for any other type.
- Add the deep `Clone` impls, with the handle types copying only the handle. Check
  whether `impl<T where Copy T> Clone T` (`stdlib/std/clone.sta`) overlaps with a
  conditionally `Copy` `Sequence<T>` that has its own `Clone` impl. If it does, make
  `Sequence` and `Separated` always non-`Copy`, and update Decision 2.
- Fix the macro code the check now rejects: the standard library's 33 known errors
  and whatever appears once they are gone, then the fixtures, the test sources and
  `staple-compiler/examples/macros.sta`. Prefer `move` on parameters and typed nodes
  such as `TypeParameters`. Use `Clone.clone` only where a value is really needed
  twice. Write the final counts in Findings.
- Make `Clone.clone` on syntax work under the interpreter (Decision 2), and replace
  `duplicate_modifier`.
- Tests: a use after a move is reported, `Clone.clone` makes it valid, `Ident` is
  copied freely, a `Sequence` of `Ident` and a `Sequence` of `Expr` each behave as
  the table says, a spliced borrowed binding needs no clone, and an `Expr` and an
  opaque `Syntax` value can each be spliced twice in one quotation without a clone.

### 4. Arena and core intrinsics

- `arena.rs`: per-call storage for syntax fragments and parsed nodes, reset when
  a call ends. Templates live outside it, for the life of the library.
- Do the `syntax_ops.rs` extraction (Decision 6). The corpus comparison must show
  no change.
- Streams: `stream`, `is_empty`, `remaining`, `error_at`, `__offset`,
  `__take_rest`. A `ParseStream` is an immutable `(fragment, offset)` handle, so
  a copy is a fork, and advancing assigns a new handle to the `mut` binding.
- Text: whole-buffer transfer in both directions, using the `pub(package)` buffer
  access noted above.
- Rust unit tests for all of them, with no compiled code.

### 5. Node views and rebuilding

- For each node kind, add the non-generic intrinsics that expose it: classifying
  a fragment (`__classify_expr` and so on), reading parts (`__call_parts`,
  `__ident_text`, the fields of `TypeDeclarationItem`, `ModifiedItem`,
  `ModifierCall`, `TypeParameters`, `ProductType`), and rebuilding (Decision 3).
- In `std.syntax`, write `@compiled_only` functions that build the Staple value
  from a fragment and attach its origin, and `Splice` impls that replace
  `__splice_node` and `__splice_join`.
- `Sequence` construction becomes Staple code. Parsing loops build in reverse and
  reverse at the end, so a long sequence isn't quadratic. `FromIterator` keeps its
  behavior.
- Tests under the harness: round trip each node kind unchanged (the same handle
  comes back), changed fields, constructed nodes, and sums of each kind.

### 6. `Parse` impls

- Write the `@compiled_only` `Parse` impls (Decision 4) for the leaves, delimiters,
  `Option`, `Sequence`, `Separated`, and fixed-arity products (Decision 9).
- `Ident<S>`: a macro-runtime intrinsic reads `S`'s spelling once specialized,
  with `String` accepting any spelling. Find out why the front end rejects this
  impl today and fix the cause. Stop excluding impls from the check
  (`is_variadic_parse_impl`).
- Error messages must match the interpreter's, since diagnostics depend on them.
  They come from the shared code (Decision 6).
- Tests: the behaviors `Staple.md` documents, each run under the harness and, where
  it applies, compared with the interpreter. That covers greediness, forks,
  `Option` backtracking, trailing commas, and `SyntaxNode` atoms.

### 7. Plain sequence splices

Do this as one change with its own commit, so the interpreter and compiled code accept
the same source from here on (Decision 13).

- Start with the interpreter (`staple-compiler/src/macro_expand.rs`). Route a quotation
  that splices a sequence through raw-token substitution, and prove these cases before
  going on: `$items` at the end of an item list, `$items` followed by another item, and
  `$visibility type ...` still working as a visibility splice. Today, replacing
  `$items...` in `stdlib/std/typegroup.sta` fails with "quotation requires a contextual
  syntax type", so the template parse has to change as well.
- Remove `...` from splices: `SpliceExpression.repeated`, `Item::RepeatedItemSplice` and
  everything that matches on it, the `...` requirement on type-parameter splices in
  `staple-syntax/src/parser.rs`, and the `Ellipsis` check in `substitute_raw_quote`. A
  `...` after a splice is then an ordinary token.
- Migrate the uses: `stdlib/std/typegroup.sta` (2), `stdlib/std/fmt.sta` (1),
  `staple-compiler/src/macro_expand/check_program.rs`,
  `staple-compiler/tests/compiler.rs`, `staple-compiler/tests/derive_default.rs` and
  `staple-compiler/examples/macros.sta`.
- Update `Staple.md`, which describes `$items...` in the syntax-values section, the
  multi-item quotation section and the `Splice` lowering section. Remove the wording that
  ties sequence splices to item lists.
- Replace `reserves_repeated_splice_syntax` (`staple-syntax/tests/parser.rs`) and
  `rejects_repeated_splices_with_a_specific_diagnostic`
  (`staple-compiler/tests/compiler.rs`), which test the old syntax.
- Tests:
  - A `Sequence` of `Item`, `Ident`, `Expr`, `Type` and `Pattern` spliced with `$name`
    in the positions the grammar allows, and in a type-parameter list.
  - An empty sequence.
  - `f $a ...$b` and `f $a ...pair` spreading in the generated code.
  - `$name...` with nothing to spread being a parse error.
  - A sequence of something that isn't syntax being rejected, by the interpreter and by
    the closure check.
  - Hygiene of spliced elements. The item-list path used to substitute trees rather than
    tokens, so `typegroup` and `derive_debug` expansions must match the output from
    before this change. Record any difference in Findings.

### 8. Quotations

- Change `TemplateTable` to hold each fragment's tokens and definition module. Keep
  the id order Phase 2 fixed.
- A template's holes map to `splices` by name, so a repeated name fills every one
  of its holes from the same entry.
- `macro_runtime/quote.rs`: `__quote` fills a template using the shared code,
  stamps hygiene from the call context (Decision 11) and handles sequence and raw
  splices. `Parse.parse_quote` needs no change.
- Tests: hygiene in the same way the macro tests check it (a quoted name resolving
  in the macro's module, a spliced one at the call site), each result category,
  spaced splices, a repeated `Ident` and `Expr` splice, and a sequence spliced with
  `$items`.

### 9. Arguments and results

- `args.rs`: convert a matched macro call into an `ArgTree` (Decision 8), starting
  from what `match_macro_arguments` returns today. Include `MacroCallMetadata`
  and top-level `Sequence` parameters.
- Add `FromMatch`, its impls and the generated entry function to the check program.
- Convert a result handle back by the declared category, including
  `Sequence<Item>` and raw `Syntax`. Reuse the interpreter's existing reparse
  rules for placement. Renumber ids, and apply marks (Decision 11).

### 10. Wire format

- `wire.rs`: versioned, length-prefixed encoding of fragments, `ArgTree`, call
  context, results and errors, as in Decision 10. The server that uses it comes in
  Phase 4, so nothing here is process-specific.
- Tests: a round trip over every token of every corpus input, with every field
  compared. Record encoded sizes for `@derive_eq` on the B2 input.

### 11. Compare against the interpreter

- Add a test, using the harness, that expands calls through *both* engines and
  compares the normalized rendered output. It goes through the wire encoding as
  well, so reparsing (Decision 10) is covered.
- Cover at least: `if`, `when`, `while`, `for`, `discard`, `@derive_eq`,
  `@derive_hash`, `@derive_debug`, `@derive_default`, `typegroup`, every macro
  shape in the Phase 2 test, and the macro example. Compare spans and hygiene as
  well as text, by resolving and type-checking the expansion.
- Every difference is fixed or written down with a reason. A difference that has
  to stay also gets a line in the umbrella plan's Phase 5 section, since that
  phase compares the same way at full scale.
- Measure call cost for these macros against the interpreter's, under the harness.

### 12. Language reference

- Document the ownership rule: which syntax types are `Copy`, that a splice borrows
  its value, and that `Clone.clone` is a deep copy of the structured nodes. Update
  the `let original = value; let mut changed = value` example, which now needs a
  clone.
- Say that a name may repeat in a quotation, that this repeats the expression in the
  generated code, and that a `let` in the quotation evaluates it once.
- Describe how syntax values keep their source: a changed node gets new tokens,
  and everything unchanged keeps its own.
- Document the product arity limit for `Parse`.
- Remove any statement that this phase made untrue, and keep the interpreter's
  evaluation limits.

### 13. Close out

- Run the corpus comparison, the closure check and the full test suite, and record
  the results.
- Update the umbrella plan: a status paragraph for Phase 3, and changes to
  Phases 4 to 6 from the findings. Phase 4 inherits the host table, `exit` trap,
  entry functions and wire format. Phase 6 inherits deleting `@interpreted_only`
  items and rewriting matching so it produces `ArgTree` directly.

## Out of scope

- The server, library builds, caching and timeouts (Phase 4).
- The quotation cycle through `Char`, and generated-item loops (Phase 4).
- Enforcing compile-time effects (Phase 4).
- Using compiled macros in normal builds (Phase 5).
- The known Phase 2 limits: default builds accepting an unannotated `parse_quote`,
  and a `const` running a helper with a static error. Both end with the interpreter.

## Done when

- The check program for the standard library and the fixtures lowers, compiles
  and runs under the harness.
- Every macro listed in task 11 expands through compiled code with results that
  match the interpreter, apart from differences listed in Findings with a reason.
- No host function is generic over a Staple value's layout (`__origin` and
  `__with_origin` are lowered inline, on the box pointer), and no
  `Parse` impl is excluded from the check.
- Syntax ownership follows Decision 2: the check's copy exception is gone, and a
  name can be spliced more than once in a quotation without a clone.
- `$name...` no longer exists, and a sequence splices with `$name` under both the
  interpreter and compiled code (Decision 13).
- The wire format round-trips the corpus.
- Interpreter output on the corpus is unchanged, apart from standard-library files
  edited for this phase, and the closure check is clean.

## Implementation status

Update this section as you work (see "Rules for the implementor").

| Task | Status | Commit | Notes |
|---|---|---|---|
| 1. JIT harness and lowering inventory | partly done | this commit | `macro_runtime/harness.rs` builds, checks and lowers the standard library's check program. Generic foreign intrinsics now emit trapping stubs cast to each call's signature. Whole-program emission stops at one LLVM verifier failure, recorded in Findings. |
| 2. Boxed nodes | partly done | this commit | `@syntax_node` marks the node types. Lowering and codegen give them a pointer-sized box `(representation, origin)`, constructors allocate one, nominal patterns unbox them, product patterns select sum alternatives by width, field reads load the representation, and `__origin`/`__with_origin` lower inline. The compiled-code proof waits on the LLVM verifier failure recorded in Findings. |
| 3. Ownership of syntax | not started | | |
| 4. Arena and core intrinsics | not started | | |
| 5. Node views and rebuilding | not started | | |
| 6. `Parse` impls | not started | | |
| 7. Plain sequence splices | not started | | |
| 8. Quotations | not started | | |
| 9. Arguments and results | not started | | |
| 10. Wire format | not started | | |
| 11. Compare against the interpreter | not started | | |
| 12. Language reference | not started | | |
| 13. Close out | not started | | |

## Measurements

_None yet._ For each one, record what was measured, the result, the machine, the
commit and the command. Include per-call cost against the interpreter (task 11) and
encoded sizes (task 10).

## Findings

### Lowering inventory (task 1)

The standard-library check program lowered for the first time after these
changes. Before them, every distinct failure was one of:

- **Product patterns over recursive stand-ins.** `Sequence (first, rest)` and
  the product patterns inside `ModifiedItem` saw the `Opaque` placeholder the
  checker leaves for a recursive syntax type instead of the wrapper's
  representation. Lowering now resolves those placeholders to the wrapper or
  to its representation, depending on whether the pattern names the wrapper
  (`Sequence (...)`) or destructures it (the argument product).
- **Sum alternatives behind stand-ins.** `None`/`Some`, `ModifiedItem`, and
  `WrapperBody` alternatives are `Opaque` stand-ins in some sums. The pattern
  planner now accepts an opaque alternative with the expected type id and
  reads its representation from the pattern's checked type.
- **Product alternatives of a sum.** A product pattern whose subject is a sum
  now records the alternative selected by the pattern's width, so `Sequence
  ()` and `Sequence (first, rest)` get a run-time tag test.
- **Inferred fields.** Coercions and generic constructor calls can retain an
  `Inferred` field after checking. Such a field adopts the source type and
  names no runtime layout, so its coercion is the identity and descriptor
  scanning skips it.
- **Generic foreign declarations.** A macro-runtime intrinsic is an external
  binding, not a compiler intrinsic. It is declared as `Forall` until a call
  instantiates it, so codegen declares it from the call rather than skipping
  it as an ordinary extern. This is the remaining blocker for compiling the
  whole standard-library check program.
- **Syntax handles.** `Syntax`, `ParseStream` and the `Unstructured*` types
  are `U64` handles; boxed syntax nodes are pointers. Before this, codegen
  rejected `Syntax` as an opaque type with no by-value representation.

After these fixes the standard-library check program resolves, type-checks
and lowers. Code generation then got through the generic foreign declarations
by emitting one trapping stub per generic intrinsic and casting it to each
call's instantiated signature at the call site. Boxed node field reads lower as
new `SyntaxNodeField` accesses that load the representation from the box. With
those changes the whole check program's only remaining emit diagnostic is an
LLVM verifier failure:

```
PHI node operands are not the same type as the result!
%match.value = phi { i32, [1 x ptr] } [ %fallback, ... ], [ %extern.call, ... ]
```

It appears while emitting match arms in the whole standard-library program,
after the generic stubs and field accesses are in place. No JIT execution has
been attempted past this point.  The whole-program emit takes about 6.7 s in
the test profile. This is the next blocker; it is recorded here rather than
worked around.

### Boxed representation (task 2, in progress)

The box works in lowering and in the IR: a node is a GC pointer to
`(representation, origin: U64)`; construction stores origin `0`, and a direct
or adapter constructor call boxes its representation. Pattern tests load the
representation before matching, and `Sequence` alternatives are selected by
tag. `__origin` and `__with_origin` now lower inline as `SyntaxOrigin` and
`SyntaxWithOrigin` expressions, and field reads through a boxed node lower as
`SyntaxNodeField` accesses. The compiled-code proof still waits on the LLVM
verifier failure recorded above, so no JIT result exists yet.
