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
   - **Run-time representation:** a pointer to a GC-allocated
     `(representation, origin: U64)`. Constructors allocate with origin `0`.
     Patterns and field reads load from it. A field assignment
     (`changed.argument = r`) writes into the box in place and keeps the origin. This
     is safe because a non-`Copy` node has one owner: a move transfers the pointer,
     and `Clone` shares no structured box (Decision 2). `Copy` nodes may share boxes,
     but they have no fields to assign.
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
   | Non-`Copy`, `Clone` | `Syntax`, `Expr`, `Type`, `Pattern`, `Item`, `Modifier` and their nodes, the delimiter types, `Sequence<T>`, `Separated<T, S>` |

   - **`Sequence` and `Separated` are always non-`Copy`, whatever their elements.**
     `Sequence<Ident>` is non-`Copy` even though `Ident` is `Copy`. Their recursive
     representation makes structural `Copy` false, and explicit positive `Copy` impls
     are rejected. They are `Clone` when their elements are, through
     `impl<T where Clone T> Clone Sequence<T>` and
     `impl<Element, Separator where Clone Element, Clone Separator> Clone
     Separated<Element, Separator>`. Generic node types such as the delimiters
     follow the same rule.
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
   - **`Clone` is a real deep copy of the structured nodes.** Compiled, each node's
     `Clone` impl is Staple code in `std.syntax` that allocates a new box, clones
     each child and copies the origin with `__with_origin`. A clone shares no box
     with the original, apart from the cases below, so an assignment to one can't
     change the other. It costs time linear in the size of the tree, which is why it
     is explicit.
     - **Origins are kept.** Decision 3 compares origin handles, not box addresses, so
       an unchanged clone still rebuilds to its original tokens.
     - **`__with_origin` copies only the top box.** Its result shares children with
       its argument, so it must only be given a value the caller owns and gives up, as
       a `Clone` impl's newly built node is.
     - **The copy stops at `Copy` nodes and handles.** `Ident` and the other `Copy`
       types are copied as values, so their boxes may be shared. `Syntax` and the
       `Unstructured*` types copy only the handle, because their arena fragments are
       immutable. An inline intrinsic does that, limited to these handle types.
     - **`Sequence` and `Separated` loop.** Their `Clone` impls build in a loop rather
       than recursing, so a long sequence doesn't use deep stack.
     - `duplicate_modifier` is replaced by `Clone`, since the parent renumbers syntax
       ids in every result.
   - **The interpreter's `Clone` stays an identity.** Rebuilding a node through its
     constructor would lose the value's hidden source. Assigning to an interpreter
     value must not change another binding of it, which task 3 tests. So each impl has two bodies
     (Decision 4): the deep copy is `@compiled_only` and the identity is
     `@interpreted_only`. Until Phase 5, normal builds accept what the closure check
     rejects, as they already do for an unannotated `parse_quote`.

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
  declarations only where the table contradicts them.
- Remove the copy exception (`is_syntax_value`, `typecheck.rs:3367` and `:10923`).
  An `@` pattern on a non-`Copy` node is then an error, as for any other type.
- Add `@interpreted_only` and `@compiled_only` (Decision 4), which this task is the
  first to need, and the two-body `Clone` impls from Decision 2. Handle types copy
  only the handle. Write the impls with `match` and `loop`.
- Fix the macro code the check now rejects: the standard library's 33 known errors
  and whatever appears once they are gone, then the fixtures, the test sources and
  `staple-compiler/examples/macros.sta`. Prefer `move` on parameters and typed nodes
  such as `TypeParameters`. Use `Clone.clone` only where a value is really needed
  twice. Write the final counts in Findings.
- Make `Clone.clone` on syntax work under the interpreter (Decision 2), and replace
  `duplicate_modifier`.
- Tests: a use after a move is reported, `Clone.clone` makes it valid, `Ident` is
  copied freely, a `Sequence` of `Ident` and a `Sequence` of `Expr` are both non-`Copy`
  and `Clone`, a spliced borrowed binding needs no clone, and an `Expr` and an
  opaque `Syntax` value can each be spliced twice in one quotation without a clone.
- Tests under the harness, running compiled code: `Clone.clone` of a `CallExpr` tree and
  of a `Sequence` gives equal contents, different box addresses for every structured
  node, and the same origins; a long `Sequence` clones without deep stack; and assigning
  to a field of a clone, or of the original, leaves the other unchanged, including a field
  of a nested node.
- Tests of in-place assignment: the box address and origin stay the same, the new value
  is read back, and an assignment through a `mut` parameter is visible to the caller. Run
  each under the harness, and the separate-bindings case under the interpreter too.

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
- Syntax ownership follows Decision 2: the check's copy exception is gone, a
  name can be spliced more than once in a quotation without a clone, and `Clone` is a
  deep copy in compiled code.
- `$name...` no longer exists, and a sequence splices with `$name` under both the
  interpreter and compiled code (Decision 13).
- The wire format round-trips the corpus.
- Interpreter output on the corpus is unchanged, apart from standard-library files
  edited for this phase, and the closure check is clean.

## Implementation status

Update this section as you work (see "Rules for the implementor").

| Task | Status | Commit | Notes |
|---|---|---|---|
| 1. JIT harness and lowering inventory | done | `8c30bf5`, `fcfa409` | `macro_runtime/harness.rs` builds, checks, lowers and JITs the whole check program. Running the program's `main` first sets the collector's stack bottom, registers roots and runs module initializers, so plain functions and their globals work. Generic foreign intrinsics emit trapping stubs cast to each call's signature, and non-generic ones trap with their own name. A harness test takes about 25 s. |
| 2. Boxed nodes | done | `8c30bf5`, `725cff0`, `fcfa409` | `@syntax_node` marks the node types. Constructors allocate a pointer-sized box `(representation, origin)`, and patterns, field reads and `.*` read through it. Refinement patterns compare the spelling at run time, including a set such as `Ident<"a" \| "b">`. Field assignment writes into the box. `__origin` and `__with_origin` lower inline and run under the harness. Generic constructors are solved in the checker, and the `Inferred` shortcut is now an internal error for a product-to-sum coercion. |
| 3. Ownership of syntax | done | `24dd4cf`, `fcfa409` | `Sequence<T>` and `Separated<T, S>` are always non-`Copy`. `Clone` is a deep copy in compiled code and the interpreter's is an identity, using `@compiled_only` and `@interpreted_only`. Handle types copy through `__clone_syntax`, which compiled code rejects for anything else. `duplicate_modifier` is gone. The check treats a splice as a read only for the real `std.syntax` `Splice`. See Findings for the suite and corpus results. |
| 4. Arena and core intrinsics | done | `b1207bf8` | `macro_runtime/arena.rs` keeps syntax fragments, text, parse streams and parsed nodes for one call; handles carry a generation and `reset` makes earlier handles stale. `macro_expand/syntax_ops.rs` owns the shared slicing, position, delimiter, hygiene and token-filling helpers, plus text lexing. Streams and text run through host intrinsics, with two-body `is_empty` and `error_at` where the ABI differs. See Findings. |
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

- **Whole-program JIT, one harness test:** about 25 s from check program to result
  on the development machine (macOS, arm64), command
  `cargo test -p staple-compiler --lib macro_runtime::harness::tests::plain_functions_run_after_initialization`.
- **Whole-program emit:** about 6.7 s in the test profile.

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
- **Inferred fields.** A coercion may still see an `Inferred` position, which
  adopts the source's type and names no layout, so it stays the identity.
  A product-to-sum coercion is the exception and is an internal error, because
  a sum alternative is chosen by its exact type. Descriptor scanning skips
  `Inferred` fields.
- **Generic foreign declarations.** A macro-runtime intrinsic is an external
  binding, not a compiler intrinsic. It is declared as `Forall` until a call
  instantiates it, so codegen declares it from the call rather than skipping
  it as an ordinary extern.
- **Syntax handles.** `Syntax`, `ParseStream` and the `Unstructured*` types
  are `U64` handles; boxed syntax nodes are pointers. Before this, codegen
  rejected `Syntax` as an opaque type with no by-value representation.

After these fixes the standard-library check program resolves, type-checks
and lowers. Code generation then got through the generic foreign declarations
by emitting one trapping stub per generic intrinsic and casting it to each
call's instantiated signature at the call site. Boxed node field reads lower as
new `SyntaxNodeField` accesses that load the representation from the box. The
whole-program emit then failed the LLVM verifier on a match phi, whose cause is
described under "Boxed representation". It is fixed.

### Ownership of syntax (task 3)

Conditional `Copy` is unavailable, so Decision 2 makes `Sequence<T>` and
`Separated<T, S>` always non-`Copy`. `Ident<S>` stays `Copy`; its spelling is the
builtin `String`, which `is_copy_type` treats as copyable. An earlier attempt
marked `Ident` `!Copy` itself, which caused a false "Ident is non-Copy" reading;
that marker is not needed.

**Deep `Clone`.** `std.syntax` has one `@compiled_only` impl per node, which builds
a new box from cloned children and copies the origin with `__with_origin`. The
`@interpreted_only` impl of the same type is the identity. The handle types share
one impl, `__clone_syntax value`, and lowering rejects that intrinsic on anything
but an opaque handle, so it can't alias a box.

`Sequence` clones in two loops, cloning into a reversed list and then reversing it.
A borrow can't be rebound in Staple, so the walk starts from `__with_origin (value,
__origin value)`, one owned copy of the top box, and moves down the original's
cells from there. It only reads what it moves. If a `Sequence` element type ever
gains a `Drop` impl this has to change, because the moved `first` is dropped at the
end of its arm while the original still owns it. No syntax type has one.

`Separated` clones its `elements` through that impl, so it needs no loop of its own.
`Option` has no `Clone` impl, because one would overlap the `Copy` blanket when its
payload is `Copy`, so `std.syntax` uses a compiled-only `clone_option` helper.

**The two stage modifiers.** They are removed before anything indexes the program,
so the interpreter, its trait registry and normal builds only see the interpreted
view. The check program's modules leave out `@interpreted_only` items and gain the
expanded `@compiled_only` ones. They are recognized only in the standard library's
modules, and elsewhere they are unknown modifiers. The closure-only harness mode
(`check_run_for_with (source, false)`) has no caller, and doesn't apply this swap
to its closure groups; nothing that uses it depends on a two-body item yet.

**In-place assignment.** A field place loads the box pointer from the binding's cell
and addresses the field inside the box, so the box and origin stay the same and an
assignment through a `mut` parameter is visible to the caller. Dynamic code reports
a diagnostic for a node field, as it already did for reads.

**Macro code changes.** `when_clauses`, `when` and `typegroup_items` no longer clone
their clause lists: a `Sequence` of slotted products has no `Clone`, and a move or a
borrow is enough. `__with_origin` now takes a plain `(T, Syntax)` product, because a
slotted parameter is a juxtaposed call and never reaches the inline lowering.

The `Clone` overlap was real. `impl<T where Copy T> Clone T` and
`impl<T> Clone (Sequence<T>)` are reported as ambiguous at a `Clone.clone` call
unless negative `!Copy` headers are consulted. Two places had to change:

- The macro trait registry records `impl !Copy` headers and excludes the
  blanket when one matches. A header whose binder parameters are not yet known
  still excludes the blanket when the target is the same nominal constructor
  with unknown arguments, as happens when a generic `Clone` target is matched
  before its element type is fixed.
- The ordinary overlap checker asked "could this open type be `Copy`?" before
  consulting negative impls. It now consults `is_copy_with` first, so a
  negative impl on `Sequence<T>` or a concrete node excludes the blanket.
  `!Copy` targets may also be a union alias or a sum of negatable alternatives;
  `TypeBody` is such a union.

`typegroup_prepend` now distributes a copied modifier with `Clone.clone`
instead of `duplicate_modifier`; the expander freshens each generated item, so
the copies still get distinct identities. `duplicate_modifier` is removed.

Removing the copy exception reported the expected 33 errors across `cmp`,
`default`, `hash`, `typegroup`, `flow`, `fmt` and `syntax`. They are fixed by
`move` on parameters or bindings that are consumed once and `Clone.clone`
where a value is needed twice. Two ownership-checker gaps appeared during the
migration and are fixed with it:

- A curried macro parameter is captured by each nested closure. When the
  captured value is already frozen (a borrowed parameter or a borrowed
  capture), the nested closure now records a borrowed capture instead of
  trying to move the value into itself.
- A quotation splice reads its argument. The checker now treats a resolved
  `Splice.splice` call as non-consuming, so a borrowed binding can be spliced
  without a clone. Repeated splices in one quotation are covered too. Only the
  trait declared in `std.syntax` gets this: a user trait named `Splice` moves
  its argument as usual.

`@` patterns now require `Copy` for syntax too: `Expr`, `Type`, `Pattern`,
`Item`, `Modifier` and `SyntaxNode` are negative-`Copy` aliases, so an
`@` pattern may bind an `Ident` but not an `Expr`. The positive compile-time
test uses `Ident<String>`; a closure-check test asserts the `Expr` rejection.

New closure-check tests cover a use after a move and the `Clone.clone` that
fixes it, `Ident` copying, `Sequence<Ident>`/`Sequence<Expr>` and
`Separated<Ident, Comma>` being non-`Copy` with `Clone` impls, and borrowed and
repeated splices with no clone. The standard library, macro fixtures and
`staple-compiler/examples/macros.sta` are migrated. Full-suite counts and the
closure/expansion corpus results are recorded below.

The expansion gate reports 224/232 matched against the commit before this phase. The
eight differences are the eight standard-library files whose source changed:
`stdlib/{annotation,cmp,default,flow,fmt,hash,syntax,typegroup}.sta`. Each is an
ownership rewrite (`move` or `Clone.clone`) or a new declaration: `Clone` and the
`!Copy` markers in `std.syntax`, and `@syntax_node` in `std.annotation`. The corpus,
fixtures and examples expand exactly as before, and the `@compiled_only` bodies don't
appear in any expansion.

Final verification for tasks 1 to 3:

- `cargo test -p staple-compiler --no-fail-fast`: every suite passes, including
  `--lib` with 535 passed, 0 failed, 3 ignored, and `--test compiler` with 854
  passed, 0 failed.
- `cargo test -p staple-compiler --test constructor_inference`: 8 passed.
- `python3 scripts/check-macro-closures.py --mode all`: 232 inputs, 0 with
  diagnostics, 0 distinct messages. It takes about 26 minutes on the development
  machine. Run alongside the test suites, 20 inputs hit the 120 s limit, so run it
  on its own.

After that run, an unused method, field and parameter were removed and the table's
comparison derive was dropped. Those edits only affect dead code, and were checked with
`cargo check --all-targets` and the intrinsic and harness tests, not a second full run.

### Arena and core intrinsics (task 4)

`macro_runtime/arena.rs` is the per-call store. It holds `Syntax` fragments, text, immutable parse streams and parsed AST nodes. A handle is `(generation, index)` with zero meaning "none"; `reset` clears the entries and starts a new generation, so a handle left over from an earlier call is rejected instead of naming a different value. Host-lexed fragments take their syntax ids from the arena's counter. Templates are not per-call state and are not in this arena.

`macro_expand/syntax_ops.rs` now holds the helpers both engines use: `syntax_slice`, `describe_position`, `skip_trivia`, `matching_close`, `matching_angle_close`, `root_hygiene`, `append_syntax_tokens`, `is_structured_product_type`, `matches_single_token`, `item_syntax`, and a new `syntax_from_text`. The interpreter imports them and behaves as before. The expansion comparison against the start of this task matches 231 of 232 inputs; the only difference is `stdlib/syntax.sta`, which gained the items below. Against the phase-start baseline the earlier six standard-library differences from tasks 2 and 3 remain.

Three ABI shapes needed a decision:

- **`ParseStream` was not a syntax handle.** It was absent from the builtin type list in `resolve.rs`, so codegen had no by-value representation for it. It is now registered like `Syntax` and the `Unstructured*` types, represented as a `U64`.
- **A native extern cannot update a `mut` argument.** Intrinsic and native-extern calls evaluate arguments by value, so the old `__take_rest: mut ParseStream -> Syntax` could not be implemented by a host function. It now takes a stream and returns `(Syntax; ParseStream)`; `Parse Syntax` assigns the advanced handle back to its `mut` binding. The interpreter returns the same product and no longer treats `__take_rest` as a `mut` intrinsic.
- **`Bool` and `ParseError` are not ABI scalars.** `Bool` is a tagged sum, and `ParseError` is a Staple wrapper the host cannot build. `__is_empty` therefore returns a `U8`, and the compiled `is_empty` body compares it with `1`; the interpreted `is_empty` stays an extern returning `Bool`. `__error_at` returns a text handle, and the compiled `error_at` body builds the `ParseError` in Staple. Both public names have `@interpreted_only` and `@compiled_only` bodies.

Text moves in whole buffers. `__text_new` copies Staple bytes into arena text; `__text_length` and `__text_copy` copy text into a Staple `Buffer`; the compiled `text_string` helper turns that buffer into a `String` through the `pub(package)` `Buffer.unsafe_element`, `BufferHeader.set_length`, `SharedBuffer.unsafe_from_raw` and `string_from_shared_bytes_unchecked`. `__syntax_text` and `__syntax_from_text` bridge syntax and text, and `syntax_from_string`/`string_from_syntax` expose the bridge to compiled macro code. `match` has no integer-literal pattern, so the `U8` result is compared with `== 1`.

The compiled `Parse` leaf path still traps: `__parse_primitive` and the `__*_group` intrinsics remain unimplemented until task 6 replaces them. The core path is covered by the harness test `streams_and_text_run_through_the_intrinsics`, which runs `stream`, `is_empty`, `Parse.parse_all.<Syntax>` through `__take_rest`, `error_at` and both text directions in compiled code. Rust unit tests cover the arena, every stream and text intrinsic (including `remaining` and `__offset`), and the extracted syntax helpers without compiled code.

Final verification: `cargo test -p staple-compiler --no-fail-fast` passes every suite, including the library tests (552 passed, 0 failed, 3 ignored), the compiler tests (854 passed, 0 failed) and `constructor_inference` (8 passed). The new unit tests are 7 for the arena, 9 for the intrinsics and 3 for the syntax helpers. The expansion gate matches 231 of 232 inputs against the start of this task; the only difference is `stdlib/syntax.sta`. `scripts/check-macro-closures.py --mode all` reports 232 inputs, 0 with diagnostics, 0 distinct messages.

### Boxed representation (task 2)

The box works in lowering and in the IR: a node is a GC pointer to
`(representation, origin: U64)`; construction stores origin `0`, and a direct
or adapter constructor call boxes its representation. Pattern tests load the
representation before matching, and `Sequence` alternatives are selected by
tag. `__origin` and `__with_origin` now lower inline as `SyntaxOrigin` and
`SyntaxWithOrigin` expressions, and field reads through a boxed node lower as
`SyntaxNodeField` accesses.

The whole-program JIT now runs the proof: a `Sequence<Ident<String>>` is
constructed and matched by alternative; an `Expr`/`CallExpr` tree is
constructed, its field is read, a changed node is rebuilt from it, and the
result is matched; and a refinement binding (`word: Ident<"else">`) compares
the spelling at run time. `__origin` and `__with_origin` lower inline. The
bindings stay private to `std.syntax`, so the tests that run them append small
accessors to a temporary copy of the standard library.

Three faults in this first version were found in review and fixed, each now with a
compiled test:

- **Refinement never tested.** The checker overwrote the pattern's alternative with
  the refined type, so no tag test was planned, and instance materialization then
  dropped the spelling. The checker now records the alternative and the accepted
  spellings, and materialization keeps them.
- **Assignment wrote to the stack.** See "In-place assignment" above.
- **`.*` read the box pointer.** A representation or scalar read on a node returned
  the pointer as if it were the contents. It now loads from the box.

Mode gates: none became unnecessary with the boxed declarations alone. The
copy exception goes with task 3. The `ordinary_syntax_types` gates, recursive
stand-ins, refinement coverage and the standard-library checks stay until the
interpreter goes in Phase 6, because normal builds must still treat these
types as compile-time only.

The earlier
failure was not a box or representation problem. It reproduces without syntax
types:

```staple
pub type Empty wrap pub ()
pub type Pair<Element> wrap pub Empty | (first: Element, rest: I32)

def compute: () -> I32 = () => {
    let one: Pair<I32> = Pair (first: 5, rest: 0)
    classify one
}
```

A constructor call was checked by erasing its element parameter to
`Inferred`; the expected result was only used for numeric literals, and a
product argument never solved an element through a sum parameter. Top-level
lets happened to work because their expected result reached the call
differently from a `def` body. The checker now solves a constructor call's
parameters from the expected result and, when the argument has one matching
sum alternative, from that alternative. It also records the concrete function
type on the callee, and re-checks the argument against the instantiated
parameter. Tests for the annotated `def`, constructor result, top-level let,
unannotated let, ambiguous alternative and wrong annotation are in
`staple-compiler/tests/constructor_inference.rs`.

The checker first only handled concrete solutions, so a constructor inside a generic
function kept `Inferred` fields and the coercion shortcut built an untagged value.
Now only the constructor's own parameters must be solved, and a solution may name
an enclosing function's parameter. An argument with an unsolved coercion is re-checked
against the sum alternative it selected, so a nested constructor such as `Sequence
(first: item, rest: Sequence ())` learns its element type. An annotation also settles
an argument that matches two alternatives. The shortcut is now an internal error for a
product-to-sum coercion, and the standard library's check program lowers with it.

Quotation lowering annotates its generated lists as `Sequence<Syntax>`, and a few
standard-library macro lists carry explicit element types.

A borrowed argument with a coercion passed the address of the uncoerced value when
its place was rooted in a `mut` binding, so a callee read an `Ident` box as an `Expr`
sum. It now passes a coerced copy, as a plain `let` local already did. This wasn't
specific to syntax, and a test covers a product argument too.

The proof ran under the whole-program harness in about 8.6 s. The expansion
comparison against the start of the phase matches 231 of 232 inputs; the only
difference is `stdlib/annotation.sta`, which gained the `@syntax_node`
declaration. The closure check reports 232 inputs and no diagnostics.

A separate general codegen bug found on the way is fixed: a call whose own
result is `Never` was not treated as divergent when the surrounding expression
had the expected non-`Never` type, so `compile_error` arms put an empty value
into a match phi.

### Collector limit found by the long-sequence test

The collector's mark phase was recursive, so a collection overflowed a 2 MiB stack on a
linked list somewhere between 8,000 and 20,000 cells. The `Clone` loop wasn't the cause;
any long linked data in any program had this limit. It was fixed in the collector with an
explicit mark stack, as its own change (`a888224`). The long-clone test now clones 500,000
cells at the default collection threshold.

### Known limits left as they are

- A refinement pattern needs a sum subject. `Ident<"x">` against a bare `Ident<String>`
  is rejected with "expected `Ident<...`", so the refinement tests match through a sum
  such as `Expr`.
- `x.* = ...` on a wrapper expects the wrapper's type rather than its representation.
  This was already so before boxed nodes.
- A `Sequence` of slotted products has no `Clone`, because products have none. Macro
  code moves or borrows those lists.
