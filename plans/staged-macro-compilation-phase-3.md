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
      `Splice.splice name`.
    - **A `Sequence` splices side by side, and a comma list uses `Separated`.**
      The compiled `Splice` impls already work this way: a `Sequence` joins its
      elements with a space (two items on one line still parse), and a
      `Separated` writes its separator between elements. The interpreter gives
      `$name...` a third meaning today: in a product, such as `($parts...)`, each
      element becomes its own comma-separated product element. Only tests use
      it, and it is dropped. Code that wants commas builds a `Separated` and
      splices it inside its delimiter, as
      `Parenthesized (Separated (separator: Comma, elements: parts, trailing: False))`.
      The interpreter already turns that value into a product, and compiled code
      writes `(a, b, c)`.
    - It also frees `...` for spreading. The parser ignores the space before it, so
      `f $a ...$b` is read as a repeated splice and rejected today.
    - **The interpreter keeps substituting trees.** It doesn't switch to raw-token
      substitution. An interpreter value's tokens are not its current value: a
      quotation substituted as a tree keeps its template's `$name` tokens, and a
      constructed node (`Ident`, `CallExpr`, `StringExpr`, ...) has synthetic
      syntax with no tokens at all, so the raw path drops it. The task 7
      experiment failed for this reason (see Findings). The interpreter is
      deleted in Phase 6, so it isn't worth making every value carry current
      tokens.
    - **The template grammar decides an item-list splice by its line.** In an
      item list, `$name` with an item after it on the same line stays a visibility
      splice (`$visibility type ...`). `$name` that ends its line, or comes before
      `}` or the end, is an item splice. The value decides how many items it
      gives: an `Item` gives one, a `Sequence<Item>` gives its elements, and an
      empty sequence gives none. This is syntax, so the resolver, the closure
      check and the check program read templates the same way without values.
      Compiled quotations fill tokens before parsing and don't need the rule.
    - **Where the interpreter splices a list.** Besides item lists, the tree path
      takes a plain `$name` in a type-parameter list: an `Ident` gives one
      parameter, and a `Separated<Ident<String>, Comma>` gives its elements. A
      bare `Sequence` or `Separated` anywhere else, such as `f $args` or
      `($parts)`, is an interpreter error that names the position. Compiled
      macros accept it. This is a known difference until Phase 6, like the
      closure check's.

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

- Start with the interpreter template grammar in `staple-syntax/src/parser.rs`.
  `$name` in an item list that ends its line, or comes before `}` or the end, is an
  item splice; with an item after it on the same line, it stays a visibility splice.
  Rename `Item::RepeatedItemSplice` to `Item::ItemSplice`. A type-parameter splice
  no longer needs `...`. Then teach `substitute_item_list` to take an `Item` as well
  as a `Sequence<Item>`, `substitute_type_parameter_list` to take an `Ident` or a
  `Separated<Ident<String>, Comma>` (Decision 13). Prove these before going on: `$items` at the end of an item list,
  `$items` followed by another item, an empty sequence, a single `Item`, and
  `$visibility type ...` still working as a visibility splice.
- Remove `...` from splices: `SpliceExpression.repeated`, the repeated-product path in
  `substitute_splices`, the "repeated splices are not supported yet" diagnostics, and
  the `Ellipsis` handling in `substitute_raw_quote`. A `...` after a splice is then an
  ordinary token. `substitute_raw_quote` keeps taking raw fragments, type parameters
  and `quote` results as it does today. A `Sequence` value there still splices its
  elements' tokens side by side.
- Report a `Sequence` or `Separated` spliced anywhere else as an interpreter error that
  names the position. Don't add a raw-token fallback for it.
- An item splice whose value is neither an `Item` nor a `Sequence<Item>`, such as a
  `Visibility` left alone on its line, is an error that says `$name` on its own line
  is an item splice.
- `separated_parenthesized` always builds a product, but compiled code writes `(a)` for
  one element without a trailing comma, which parses as `a`. Make the interpreter
  match the tokens.
- Migrate the uses:
  - `stdlib/std/typegroup.sta` (2): `$items...` becomes `$items`.
  - `stdlib/std/fmt.sta` (1): build a `Separated<Ident<String>, Comma>` from
    `parameters` and splice it in the type-parameter list.
  - `staple-compiler/src/macro_expand/check_program.rs` (`($rest...)`,
    `($other, $rest...)`), `staple-compiler/tests/compiler.rs` (`($names...,)` and
    the repeated-splice diagnostic test) and `staple-compiler/tests/derive_default.rs`
    (`($parts...)`): each builds a `Parenthesized` around a `Separated` and splices
    it whole, and the expected output stays the same.
  - `staple-compiler/examples/macros.sta` and the comment above `Splice Sequence` in
    `stdlib/std/syntax.sta`.
- Update `Staple.md`, which describes `$items...` in the syntax-values section, the
  multi-item quotation section and the `Splice` lowering section. Say that a `Sequence`
  splices side by side and a `Separated` with its separators, and give the item-list
  line rule. Remove the wording that ties sequence splices to item lists.
- Replace `reserves_repeated_splice_syntax` (`staple-syntax/tests/parser.rs`) and
  `rejects_repeated_splices_with_a_specific_diagnostic`
  (`staple-compiler/tests/compiler.rs`), which test the old syntax.
- Tests:
  - The item-list cases above, in the interpreter and as parsed syntax.
  - A `Parenthesized<Separated<Expr, Comma>>` spliced with zero, one and several
    elements, with and without a trailing comma, and a `Separated<Ident<String>, Comma>`
    as a type-parameter list.
  - A `Sequence` spliced outside the supported lists, reported by the interpreter.
  - `f $a ...$b` and `f $a ...pair` spreading in the generated code.
  - `$name...` with nothing to spread being a parse error.
  - A sequence of something that isn't syntax being rejected, by the interpreter and by
    the closure check.
  - `typegroup` and `derive_debug` expansions match the output from before this change,
    since they still go through tree substitution. Record any difference in Findings.

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
  spaced splices, a repeated `Ident` and `Expr` splice, a sequence spliced with
  `$items`, a `Parenthesized<Separated<Expr, Comma>>`, and a `Sequence` in an
  expression position that the interpreter rejects (Decision 13).

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
| 3. Ownership of syntax | done | `24dd4cf`, `fcfa409`, `08129752` | `Sequence<T>` and `Separated<T, S>` are always non-`Copy`. `Clone` is a deep copy in compiled code and the interpreter's is an identity, using `@compiled_only` and `@interpreted_only`. Handle types copy through `__clone_syntax`, which compiled code rejects for anything else. `duplicate_modifier` is gone. The check treats a splice as a read only for the real `std.syntax` `Splice`. See Findings for the suite and corpus results. |
| 4. Arena and core intrinsics | done | `b1207bf8` | `macro_runtime/arena.rs` keeps syntax fragments, text, parse streams and parsed nodes for one call; handles carry a generation and `reset` makes earlier handles stale. `macro_expand/syntax_ops.rs` owns the shared slicing, position, delimiter, hygiene and token-filling helpers, plus text lexing. Streams and text run through host intrinsics, with two-body `is_empty` and `error_at` where the ABI differs. See Findings. |
| 5. Node views and rebuilding | done | `e9f38d44`, `ead6c8a8` | Non-generic classification, part and rebuild intrinsics cover expressions, types, patterns, items, modifiers, type bodies, type parameters, delimiters, visibility and punctuation. `@compiled_only` `from_syntax` functions attach origins, and compiled `Splice` impls return the origin for unchanged nodes. Sequence construction is Staple code, compiled `Splice Separated` is implemented, constructed non-empty products rebuild, and the harness asserts same-handle round trips. `__ident_spelling` is an inline intrinsic that reads an identifier box as a `String`. See Findings. |
| 6. `Parse` impls | done | `1416a3f9`, `5ed41520` | The interpreter and compiled code share one leaf parser, so grammar and diagnostics agree. Compiled leaves and groups return an arena result handle; `Ident<S>` reads `S`'s spelling with a compiler intrinsic. The check gets fixed-arity product impls while the interpreter keeps the variadic ones, and `is_variadic_parse_impl` is gone. The harness checks 21 parse behaviors, including exact error messages and same-handle list round trips. Parsed lists carry origins, and a spelling set is a code generation error. See Findings. |
| 7. Plain sequence splices | done | `7b1ad2cd`, `8a50319f` | Tree substitution uses the line rule for item splices; type parameters accept identifiers or comma lists. Repeated markers are removed, uses and documentation are migrated, and interpreter, closure and expansion checks pass. See Findings. |
| 8. Quotations | done | this commit | Templates keep their tokens and module. Compiled `__quote` fills them with the interpreter's raw-quote code, stamping quoted tokens from the call context. Quoted nodes now rebuild to their origin. See Findings. |
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

### Intrinsic parameters

The `std.syntax` intrinsics with several parameters take a plain product, as
`std.alloc`'s `__checked_mul` does, and are called as `__text_copy (handle,
destination, length)` rather than spread from a slotted product (`2a169f16`).
The `External` call route already passes a plain product's elements as
consecutive native arguments, inline intrinsics read any unnamed product, and
the interpreter accepts either product kind, so only the 44 declarations, the
call sites and the generated `__quote` call changed. The public `error_at`,
the slotted `Parse.parse_quote` and the slotted intrinsic results such as
`__take_rest`'s are unchanged. All workspace suites pass (2317 tests), the
expansion matches the previous capture for 231 of 232 inputs (the only
difference is the `__quote` call in `stdlib/syntax.sta`), and the closure
check reports 0 diagnostics.

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

### Node views and rebuilding (task 5)

The host exposes a node through classification, part and rebuild intrinsics rather than a generic layout query. `__classify_expr`, `__classify_type`, `__classify_pattern`, `__classify_item`, `__classify_modifier`, `__classify_type_body` and the matching part accessors return child syntax handles or text handles. `@compiled_only` `from_syntax` functions in `std.syntax` turn those handles into the Staple node values and attach the fragment with `__with_origin`; `Splice` impls rebuild from the fields.

Unchanged nodes return their original handle. Most rebuild functions compare the non-trivia tokens they would produce with the origin's, by kind, text and hygiene; if they match, the origin is returned, so the original trivia and spans survive. `__call_syntax` is stricter for calls: it compares the child syntax values to the origin's parsed children and reuses the original separator tokens between them. The delimiter, sequence and separated rebuilds use the same idea.

`Ident<S>` needed one compiler intrinsic. Its box stores a `String` for every `S`, but `value.*` is typed as `S` and `S` is not usable as `String` through the subtype bound. `__ident_spelling` is lowered inline as `SyntaxIdentSpelling`: it emits the box representation as a `String`. The compiled `Splice Ident<S>` then uses the normal text transfer to rebuild or return the origin. This also lets a constructed `Ident "name"` splice without an origin.

`Splice Sequence<T>` reverses the sequence while preserving each cell's origin, then joins the cells from the end with `__sequence_syntax` and returns the whole origin when the non-trivia tokens match. `sequence_reverse` and `sequence_push` have stage-specific bodies: the compiled bodies are Staple loops, and the interpreted bodies call interpreted-only `__sequence_reverse`/`__sequence_push` intrinsics. The shared `Parse` loops prepend and reverse once, and `FromIterator.insert` calls `sequence_push`, so compiled parsing and collection use Staple construction rather than the generic push intrinsic. `Iterator Sequence<T>::next` now annotates its empty result as `Sequence<T> :: Sequence ()`, which the compiled instantiation needs.

Compile-time matching treated a one-element `Sequence` value as a genuine product, so a single-parameter helper applied to a one-element sequence bound its parameter to the element instead of the sequence. That broke `sequence_reverse` inside `Parse Sequence`/`Parse Separated` and left one-element parses empty. A parenthesized single pattern is now grouping for a one-element sequence too, matching the type checker; the new `parse_reads_single_element_sequences_and_separated` test covers both parsers.

`Splice Separated<Element, Separator>` is compiled as a Staple loop over the elements, joining each spliced element with the spliced separator and appending a trailing separator when asked. `Splice ProductType` rebuilds every product from its fields: `product_type_from_syntax` stores the origin on the element sequence, and `product_type_syntax_of` reads it through a helper binding before rebuilding. Parsed products return their original handle when unchanged; constructed empty and non-empty products build `()` and `(x: I32, y: I32)` respectively. A standalone `ProductTypeField` splice still rebuilds canonical text because the field is an inspection view with no origin of its own; the product-level handle is what round-trips.

The harness test `node_views_round_trip_and_rebuild` has 42 probes. It exercises expression, type, pattern, item, type body, modified item, modifier, type-parameter and type-parameters round trips, constructed calls, patterns, modifiers, items, type parameters, products and sequences, changed call fields, delimiters, punctuation and the `SyntaxNode` sum dispatch. Same-handle assertions on simple inputs cover every node kind that carries an origin: expression, type, pattern, item, modified item, modifier, type body, type parameter, type parameters, product, delimiter, `SyntaxNode`, sequence and separated. The standard-library check program lowers with the compiled views.

Final verification: `cargo test -p staple-compiler --no-fail-fast` passes every suite: the library tests (555 passed, 0 failed, 3 ignored), the compiler tests (855 passed, 0 failed), and every other integration suite. The harness test `node_views_round_trip_and_rebuild` has 42 probes, including same-handle assertions for every node kind that carries an origin, constructed empty and non-empty products, compiled `Splice Separated`, and compiled `FromIterator` on a one-element sequence. The new compiler test `parse_reads_single_element_sequences_and_separated` covers the one-element parse paths. The expansion comparison against the end of task 4 matches all 232 inputs, so the interpreter's output did not change; against the start of task 4 it matches 231 of 232, with `stdlib/syntax.sta` the only difference from the tasks' own stdlib edits. `scripts/check-macro-closures.py --mode all --timeout 600` reports 232 inputs, 0 with diagnostics, 0 distinct messages. With the default 120 s timeout, two inputs (including `examples/macros.sta`) time out; both pass with the longer timeout, and the compiled node views add enough standard-library items that the check needs it.

#### Review fixes

A review found that the first version rebuilt common parsed nodes wrongly even when they were unchanged, because the tests only used trivial inputs. Each fault below now has a harness probe in `rebuilt_items_keep_what_their_values_do_not_model` or a unit test.

- **Sequences ran their elements together.** A sequence cell with no origin, or whose origin has no trivia between the element and the rest, now puts a space between them. A constructed `[a, b]` splices as `a b`, where it was `ab` and read back as one identifier.
- **Lists inside items lost their separators.** `ModifierCall` namespaces, type parameter names and resource dependencies are joined by `join_spliced` with `.` or `, `, rather than spliced as plain sequences. `@std.annotation.sized_opaque` had come back as `@stdannotation.sized_opaque`, and `type Pair<A, B>` as `type Pair<AB>`.
- **Type declarations lost what their values don't model.** `__type_declaration_syntax` now writes each part back into the origin's tokens. Visibility, bounds, defaults and spacing stay as written; the parameter list is kept whole while its names are unchanged, and is rewritten as plain names when they change. `declared_type` is not written, because it is the name applied to the parameters. The canonical form for a constructed declaration follows the grammar's order: name, parameters, `with` clause, body.
- **The comparison now includes hygiene.** Text alone would hand back the origin when a child was replaced by the same text from another context, which changes what a name refers to. The token's `origin` field is not compared, because copying a token records where it came from. Source positions are not compared either, so a replacement with the same text and hygiene keeps the origin's positions.
- **Views were quadratic.** Every accessor reparsed the whole fragment. The arena now caches a parse per handle and grammar category, and `call_parts` moves the call's parse into its children's entries and records the children for `__call_syntax`. A 120-argument call chain round-trips in about 0.7 ms, where it took 42 ms. Children of a fragment parse live in a copy of the origin's tokens, so `range_in` locates them by comparing the copy, and `__call_syntax` now reuses the original separator.
- **`Splice Separated` needed `Clone`.** The compiled impl now walks the borrowed elements with `join_spliced`, so its bounds match what splicing needs.

The shared parser in `staple-syntax` recurses on a long application: in a debug-build test thread with a 2 MiB stack it overflows on `f a a …` somewhere below 300 arguments, before any view code runs. That limit is the parser's and is not addressed here. Constructed identifiers still carry no hygiene; Decision 11 gives them the call context's, which arrives with the call context.

Verification of the fixes: `cargo test -p staple-compiler --no-fail-fast` passes every suite, including the library tests (559 passed, 0 failed, 3 ignored) and the compiler tests (855 passed, 0 failed). The expansion comparison against the end of the first task 5 commit matches all 232 inputs, so the interpreter's output did not change. `scripts/check-macro-closures.py --mode all --timeout 600` reports 232 inputs, 0 with diagnostics, 0 distinct messages.

### `Parse` impls (task 6)

The interpreter and the syntax runtime now read leaf nodes through the same
parser. `syntax_ops::parse_leaf` maps a `LeafKind` and an optional identifier
spelling to one of the structured nodes, `parse_group` reads a delimited group,
and `parse_syntax_node_atom` chooses the `SyntaxNode` alternative. The
interpreter's `__parse_primitive` path builds its `Value` from those results,
and the host maps the same kinds from the numbers the compiled `std.syntax`
implementations pass. The accepted grammar and the
`expected <type>[: <detail>] at <position>` messages therefore have one source.

The compiled implementations are:

- `@compiled_only` impls for every leaf category: expressions and their
  structured alternatives, types, patterns, items, modifiers, type bodies,
  type parameters, punctuation, visibility, and `SyntaxNode`.
- `@interpreted_only` copies of the old `__parse_primitive` impls remain for
  the interpreter. Delimiter impls also have two bodies: interpreted ones keep
  `__parenthesized_group` and `__parenthesized`, while compiled ones use
  `__parse_group` and construct the node with `__with_origin`.
- `Parse Syntax`, `Parse ()` and `Parse Option<T>` are shared, because they
  only call other `Parse` members and the two-body `is_empty`/`error_at`
  helpers. `Parse Sequence<T>` and `Parse Separated<T, Comma>` have two
  bodies since the review fixes below: the compiled ones attach origins.
- The interpreter keeps the variadic product impls as
  `@interpreted_only`. The check gets fixed-arity slotted and comma impls for
  arity 2 through 8, `@compiled_only`. `is_variadic_parse_impl` is removed, so
  the check sees every compiled `Parse` impl. A product with more than 8
  elements has no implementation; the corpus and the standard library use at
  most 3 (`Separated<(Ident; Equals; Expr), Comma>`). Comma products keep the
  old optional-separator behavior: a missing separator is reported as the next
  element when the stream has ended, and an adjacent element as a missing
  comma.
- `Ident<S>` uses `__ident_type_spelling: <S> () -> String`, a compiler
  intrinsic that returns the literal spelling once `S` is specialized and an
  empty string for `String`. `__ident_spelling_value: <S> String -> S` types
  the parsed spelling; the box already stores a `String` for every `S`. The
  Phase 2 rejection ("trait implementation target must be fully concrete") no
  longer reproduces now that the syntax data types are ordinary boxed types,
  so the impl checks as written.

**Host result shape.** A parse returns one arena result handle, not a
multi-field product. A host function returning a five-`U64` product crashed
the JIT with `SIGBUS` whenever it was called from a trait method, even when the
host returned a constant; the same shape is not called elsewhere from a trait
method. `__parse_result_status`, `_tag`, `_syntax`, `_stream`, `_next`, and
`_error` read the outcome. Each is a scalar return, so the ABI carries only
scalars and handles. Phase 4 should either keep this shape for parse entry
points or fix the product-return ABI before sharing more multi-field results.

**Compiled impl placement.** The compiled `Parse` impls are placed before the
shared items in the check module instead of being appended with the other
compiled items. Appending them made `staple check` on a standard library file
take about 104 s instead of 7 s: the shared generic `Parse Option`,
`Sequence` and `Separated` impls and the product impls were then consulted
before the concrete leaf impls, and obligations with inferred arguments
revisited the standard library's own functions. Putting the concrete compiled
impls first brings a file to about 29 s; the remaining difference is the extra
impls themselves. This is an ordering sensitivity in trait obligation
resolution, not a Parse-specific cost, and Phase 4's whole-library check should
keep the concrete impls first.

The compiler-intrinsic declarations must stay visible to the normal resolver,
so `is_compile_time_parse_item` now keeps a `staple-intrinsic` block in
`std.syntax` when none of its bindings is a macro-runtime intrinsic. Blocks
whose bindings the host implements are still dropped with the other
compile-time items.

Tests: `macro_runtime::harness::tests::parse_implementations_run_in_compiled_code`
runs 18 probes under the harness. It covers expression greediness, `Option`
backtracking, explicit fork and commit, a failed parse leaving the stream
alone, delimiter parsing, `Sequence`, `Separated` with a trailing comma, comma
products, `SyntaxNode` atoms, accepted and rejected `Ident<"else">` spellings,
and the exact `expected Equals ...` and `expected Ident<"else"> ...` messages.
The interpreter's `parse_*` compiler tests cover the same behaviors and are
unchanged, which is the comparison for this task. The full suite and the
closure/corpus checks are recorded below.

Cost: with the concrete compiled impls first, the parse test with 18 probes
takes about 51 s at the default collection threshold, close to the 50 s of a
plain `run_probes` test at the end of task 5. Appending the compiled impls had
made the same parse test take about 220 s. This is the check program's cost,
not a per-call cost.

Verification:

- `cargo test -p staple-compiler --test compiler`: 855 passed, 0 failed.
  `cargo test -p staple-compiler --test compiler parse_`: 26 passed.
- `cargo test -p staple-compiler --lib macro_runtime::harness -- --test-threads=1`:
  17 passed, 0 failed, 1 ignored (846.39 s for 18 tests).
- `cargo test -p staple-compiler --lib -- --skip macro_runtime::harness --test-threads=2`:
  543 passed, 0 failed, 2 ignored (1631.53 s). The other integration suites
  (`allocation` through `unique_gc`) also pass.
- `scripts/check-macro-closures.py --mode all --timeout 600`: 232 inputs, 0
  with diagnostics, 0 distinct messages. The run used one worker at a time so
  a single check's peak memory could not crowd out the machine; worker count
  does not change which inputs are checked.
- Expansion comparison against the end of task 5: 231 of 232 inputs match;
  the only difference is `stdlib/syntax.sta`, which gained the declarations
  and compiled impls. Against the start of the phase: 224 of 232 match, and
  the eight differences are the standard-library files whose source changed in
  tasks 2, 3 and 6 (`annotation`, `cmp`, `default`, `flow`, `fmt`, `hash`,
  `syntax`, `typegroup`). The interpreter's expansion of corpus, examples and
  fixtures is unchanged.

#### Review fixes (task 6)

A review of the first version found five faults. Each now has a test.

- **Parsed lists had no origin.** `Sequence` and `Separated` values came out
  of `Parse` without an origin, so splicing an unchanged list rebuilt it:
  `f a,  g b // note\n, h` came back as `f a,  g b, h`, losing the comment, and
  two items on separate lines gained a space before the line break. A new host
  intrinsic, `__stream_span`, returns the tokens two cursors over one fragment
  passed between them, trimmed of trivia at both ends. The compiled
  `Parse Sequence<T>` gives each cell the span from its element to the end of
  the list, which is the suffix origin `__sequence_syntax` expects, and the
  compiled `Parse Separated<T, Comma>` gives the list the span it read. The
  interpreter keeps the old bodies as `@interpreted_only`. An unchanged list
  now splices back to its origin handle, comments and layout included. A
  changed `Separated` element still rebuilds the whole list with plain
  separators, since `Splice Separated` has no per-element origins.
- **`Ident<"a" | "b">` accepted only `"a"`.** `__ident_type_spelling` took the
  first literal of a set, so `b` failed with `expected Ident<"a">`. The
  interpreter rejects the set, and compiled code now does too: code generation
  reports "an identifier spelling must be one string literal" for a set, and
  "an identifier spelling must be known when macro code is compiled" when the
  type still names a type parameter, where it used to accept any spelling.
  The static and dynamic code generators share the check.
- **Unused product-returning intrinsics stayed declared.** The per-field
  intrinsics replaced `__product_type_field_parts`,
  `__type_declaration_parts`, `__modifier_call_parts`,
  `__type_parameter_parts` and `__type_parameters_parts`, but these remained
  in the table and the extern block, each returning a three- to five-field
  product of the shape that crashes the JIT. They and their result structs are
  removed. The ABI fault itself is not fixed.
- **A failed `CallExpr` view built a fake node.** `call_expr_from_syntax`
  returned `call ident` when the parsed syntax was not a call. It now panics,
  since the shared parser only reports a call when it read one.
- **`parse_group_intrinsic` saved an id counter it never advanced.** The group
  parser allocates no syntax ids, so the bookkeeping is gone.

Tests: `parse_implementations_run_in_compiled_code` now has 21 probes. The
new ones check that a `Separated<Expr, Comma>` keeps an interior comment, that
a `Sequence<Item>` keeps its line breaks and a comment between items, and that
splicing an unchanged `Sequence` twice returns the same handle. The existing
`Separated` probe now expects the source spacing (`a, b,`) where it expected
the rebuilt `a,b,`. `identifier_spelling_sets_do_not_compile` checks the
spelling-set diagnostic.

Verification:

- `cargo test --workspace --exclude staple --no-fail-fast` passes every
  suite: the compiler library tests (561 passed, 0 failed, 3 ignored), the
  compiler tests (855 passed, 0 failed), the parser tests (176 passed) and
  every other integration suite.
- `cargo test -p staple --bin staple`: 288 passed, 1 failed, 2 ignored. The
  failure, `lsp::hover::tests::displays_declared_types_for_macro_helper_parameters`,
  is not from these fixes: it fails the same way at `a5c17c65` and passes at
  the branch's merge base, and bisecting puts it at `24dd4cf3` (task 3). One
  of `first`, `rest`, `modifiers`, `variant` or `body` in `std.typegroup` now
  hovers as `SyntaxNode`. Earlier verification ran only the compiler package,
  which is why it was not recorded.
  Fixed in `08129752`. The `match modifiers` in `std.typegroup` became
  `match Clone.clone modifiers`, and the compile-time check typed every
  trait-member call as unknown, so the arm bindings got no recorded type and
  the resolver fell back to the syntactic `SyntaxNode`. The check now gives a
  `std.clone` `Clone.clone` call its argument's type. After the fix the CLI
  suite passes (289 passed, 0 failed, 2 ignored), every other suite still
  passes, the expansion matches the previous capture for all 232 inputs, and
  the closure check reports 0 diagnostics.
- Expansion comparison against `a5c17c65`: 231 of 232 inputs match; the only
  difference is `stdlib/syntax.sta`, from this change's own edits. The
  interpreter's expansion is unchanged.
- `scripts/check-macro-closures.py --mode all --timeout 600`: 232 inputs, 0
  with diagnostics, 0 distinct messages.

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

### Plain sequence splice experiment (task 7)

The initial task 7 experiment was blocked at its required interpreter proof. Following the original Decision 13,
an experiment added `Value::Sequence` and `SyntaxValue::Items` to
`quote_contains_raw_splice` and let `substitute_raw_quote` append sequence
elements for a plain `$name`. The old ellipsis consumption was kept only for
this experiment, so the existing standard-library source could still load
before migration. No syntax variants or standard-library uses were removed.

A compiler test constructed an item sequence inside an `Expr -> Sequence<Item>`
macro, spliced it after a declaration and before a declaration in separate
macros, and also exercised `$visibility type $name = I32`. Running
`cargo test -p staple-compiler --test compiler
plain_sequence_splices_in_item_lists_preserve_visibility -- --nocapture`
failed during standard-library resolution, before those cases could pass:
`ConversionError` had no accessible companion member `OutOfRange`, and the
sum built by `typegroup_sum` reported unknown types `OutOfRange`,
`PrecisionLoss`, `NonFinite` and `InvalidNumber`.

The raw path cannot currently treat a structured node's backing tokens as its
current value. `substitute_identifier` changes the AST name and records an
identifier origin, but does not replace its `$variant` template tokens.
`substitute_item_list` previously inserted the resulting AST directly;
`substitute_raw_quote` instead copies `item_syntax(item).tokens()` through
`append_syntax_tokens` and reparses them. That discards the substituted tree.
Routing existing `typegroup` sequences through that path therefore changes
expansion even before removing `...`. Identifier origins and root hygiene do
not reconstruct the missing substituted tokens.

Per the original Decision 13's instruction to stop if the experiment fails, the experimental
code and failing test were removed. At that point task 7 needed a revised design for obtaining
current tokens from interpreter syntax values while preserving substitutions,
trivia and hygiene before the source migration can proceed. Retaining `...`
was not the proposed solution. At the end of that experiment the compiler,
standard library, language reference and accepted splice syntax remained at the
end of task 6.

Verification after removing the experiment: the existing compiler test
`contextual_item_sequence_quotes_flatten_empty_single_and_multiple_results`
passes, and the existing debug CLI checks `stdlib/std/convert.sta` successfully.
An expansion capture with that CLI has 232 inputs and no non-zero exits.
Comparison with `/tmp/exp-baseline`, the saved start-of-phase capture, matches
224 of 232 inputs: the same eight edited standard-library files already
recorded under task 6. There are no retained compiler or standard-library
changes to compare. The full test suite and closure check were not rerun for
this plan-only commit; their last results remain those recorded under task 6.

#### Review of the experiment

The finding holds, and the raw path fails more widely than it records:

- **Constructed nodes have no tokens.** `construct_syntax` gives `Ident`,
  `CallExpr`, `StringExpr`, `CharExpr` and the pattern constructors
  `Syntax::synthetic` syntax, so `append_syntax_tokens` copies nothing for
  them. Any quotation that goes through the raw path, which already includes
  one with a raw fragment splice, drops a constructed value without a
  diagnostic.
- **Products give `$name...` a third meaning.** Tree substitution expands
  `($parts...)` into one comma-separated product element per item.
  `derive_default.rs`'s `@describe` test depends on it (its output is
  `("x", "=", ...)`), and `check_program.rs` and `compiler.rs` use it.
  The raw path and the compiled `Splice` impls place elements side by side. So
  Decision 13's "the elements are placed side by side either way" was only true
  of the raw path. Only tests use the product meaning, so it is dropped rather
  than carried over to plain `$name`.

The revised Decision 13 keeps tree substitution in the interpreter and decides
an item-list splice by its line. Commas come from the value's type: a
`Separated` writes them, as compiled code already does, and a quotation splices
it inside its delimiter. Raw-token substitution
for sequences is no longer part of the design.


### Plain sequence splices (task 7)

The revised design is implemented with tree substitution. `ItemSplice` replaces
`RepeatedItemSplice`; in an item list a splice that ends its line, or precedes
`}` or the end, inserts items. A declaration after it on the same line remains
a visibility splice. An item splice accepts one `Item`, a `Sequence<Item>` or
an empty sequence. A lone quotation splice parses as an expression first, so
an `Item` or `Sequence<Item>` context reinterprets it as an item splice.
Blocks and companion/module bodies use the same list substitution. A final
review found that a modifier before an item splice reached the old unreachable
arm for repeated list splices. In that nested position a single `Item` now
substitutes normally, and a `Sequence` or `Separated` gets a diagnostic naming
the nested item position. The interpreter regression cases cover both paths.

Type-parameter splices accept an `Ident` or a comma-separated list of identifiers.
`derived_debug_implementation` now builds a `Separated` from an explicit clone
of its borrowed parameter sequence. The test-only comma-product splices now
construct `Parenthesized (Separated (...))` and splice the whole value. The
migrated closure-check fixture clones its borrowed expressions and sequences
before constructing its comma list.

`SpliceExpression.repeated`, the product-element expansion path and all splice
ellipsis consumption are removed. In a quotation, `f $a ...$b` and
`f $a ...pair` now parse as ordinary spread calls. An incomplete expression
quotation such as `$name...` reports the expression parser's error. Raw `quote`
fragments remain allowed to hold incomplete grammar. There is no new raw-token
fallback for a sequence in a contextual quotation: bare `Sequence` and
`Separated` values in expression, type or pattern positions give an interpreter
diagnostic naming the position. A visibility on its own line gives the
item-splice diagnostic.

A constructed parenthesized comma list with one element and no trailing comma
now becomes that expression, matching `(a)` in compiled output; with a trailing
comma it is a singleton product. Empty lists without a trailing comma remain
unit. The existing constructor already rejects an empty list with a trailing
separator, which would otherwise write invalid `(,)`; a regression test now
covers that diagnostic as well.

Verification:

- `cargo check --workspace`: passes, including the LSP consumers of the renamed
  AST variant.
- `cargo test -p staple-syntax`: 208 passed (29 unit tests, 2 formatter tests and
  177 parser tests). No Staple formatter command was run or added.
- Compiler integration tests: all 859 cases pass. The broad run passed 858 and
  exposed an old expected diagnostic for `parse_quote { $value }` in an `Item`
  context. That case now expects the required own-line item-splice diagnostic
  and passes on its targeted rerun. Final targeted runs also cover the spread
  error and all constructed comma-list cases, including the empty/trailing
  constructor rejection.
- `cargo test -p staple-compiler --test derive_default`: 11 passed, including
  the migrated `macros_inspect_product_type_fields` output test.
- The lowering rejection test passes. The negative closure test for
  `Sequence<I32>` passes with the existing missing-trait-implementation
  diagnostic. Its first assertion had incorrectly required `Splice` in the
  diagnostic's text; the compiler was already rejecting the value.
- All 30 check-program cases are verified: the broad run passed the other 29,
  and the corrected `Sequence<I32>` diagnostic assertion passes on its targeted
  rerun. This includes the migrated comma-list fixture, every macro fixture,
  and checking the standard library as its own package.
- `scripts/check-macro-closures.py --mode all --timeout 600`: 232 inputs,
  0 with diagnostics, 0 distinct messages, using four workers.
- The final release build and workspace check pass. The seven focused compiler
  tests were rerun after the nested-item fix, and the release CLI now accepts
  the modifier-before-splice probe that previously panicked.

Expansion comparison uses 232 inputs, all expanding successfully. Against an
exact baseline built from `259ca3fd`, 230 match byte for byte. The two expected
source differences are the comment in `examples/macros.sta` and the two
`$items...` quotation templates in `stdlib/typegroup.sta`, now `$items`.
Generated typegroup and derive-debug output is unchanged. A refreshed capture
with the final release binary after the nested-item fix matches the earlier
capture for all 232 inputs, with no non-zero exits. The baseline capture
uses the archived standard library, examples and fixtures, reusing the current
corpus source dump because no corpus source changed.

Against the saved start-of-phase capture (`/tmp/exp-baseline`), 223 of 232 match.
The differences are the example comment above and the same eight changed
standard-library sources already recorded under task 6 (`annotation`, `cmp`,
`default`, `flow`, `fmt`, `hash`, `syntax`, `typegroup`). There are no unexplained
generated-code differences. The older `/tmp/staple-task7-before` capture also
predates intervening `std.syntax` changes; comparison against the exact
`259ca3fd` baseline removes those from the task's difference list.

Cost check: release `staple check` with `STAPLE_CHECK_MACRO_CLOSURES=all` on
`stdlib/std/char.sta` took 61.72 s user CPU / 4.63 s system CPU before, and
62.20 s user CPU / 4.46 s system CPU after. Wall times were 108.27 s and
106.83 s under concurrent test load. This does not show a material checking
cost change. The corpus check uses four workers and a 600 s per-input timeout
on the 16 GiB, eight-CPU development machine.

Review of task 7:

- A quotation whose result is `Syntax` takes the raw token path, which rejected
  a bare `Separated` with "does not contain syntax". It now writes each
  element's tokens with the separator between them, plus the trailing one when
  `trailing` is set. A parsed separator keeps its own token; one built with
  `Comma` carries the syntax of that name, so the separator's spelling is
  written instead. A test covers both a constructed and a parsed list, and
  Staple.md now says bare lists work anywhere in such a quotation.
- `rejects_splice_ellipsis_without_a_spread_operand` passed only because some
  diagnostic appeared. It now asserts the expression parser's error. The raw
  reparse for an `Expr` context, which produces that error instead of a
  spanless "quotation requires a contextual syntax type", has a comment
  saying so.
- The closure-check fixture had lost its plain `Sequence` splice when the
  comma lists moved to `Parenthesized (Separated (...))`. It now includes
  `$callee $arguments` with a `Sequence<Expr>`.
- The four unsupported-position messages share one helper and quote
  `Sequence` and `Separated`. The tests assert the full message.
- Unrelated reformatting in the parser and its tests is reverted, stray blank
  lines from removing `repeated` are gone, and long lines are wrapped.

Verification of the review fixes: `cargo test --workspace` passes 2324 tests.
The only failure on the first run was the rewritten closure-check fixture, whose
helper did not type-check as compiled code; its inline replacement passes on a
targeted rerun. A fresh expansion capture matches the task 7 capture for all
232 inputs, and `scripts/check-macro-closures.py --mode all --timeout 600`
reports 232 inputs, 0 with diagnostics.

### Quotations (task 8)

`TemplateTable` holds a `Template` per quotation: the tokens between its braces
and the module whose code holds it. The check program keeps the expanded
program's module ids, so lowering records the module of the items it is
lowering. Template ids are still assigned in lowering order.

Compiled `__quote` can't be a host function as declared: its splices arrive
as a `Sequence<Syntax>`, a GC box whose layout the host doesn't read. So it is
`@compiled_only` Staple code that walks the list and hands each handle to
three host functions, `__quote_begin`, `__quote_splice` and `__quote_end`. The
interpreter keeps an `@interpreted_only` extern of the same name, which it never
calls. `Parse.parse_quote` now takes its splices with `move`, so it can pass them
on; the generated call always builds a fresh list, so macro code doesn't change.

The interpreter's raw-quote filling moved to `syntax_ops::fill_quote`, which
both engines call. A quoted token gets the hygiene of the module whose code
holds the quotation and a fresh mark, and an origin back into that module's
source. That module is the helper's when a helper in another module quotes, as
in the interpreter, where a function value carries the module that defined it.
Each quotation takes the call's next mark, matching the interpreter's mark per
quotation. Spliced tokens keep the hygiene they bring. `CallContext` also
carries the macro's module and the call site, which nothing reads yet: task 9
needs them for results, and a constructed identifier still gets no hygiene from
the context (Decision 11).

Filled quotations exposed a gap in the node rebuilds. A rebuild compared tokens
with their hygiene, and its own keywords and punctuation have none, so an
unchanged quoted node never matched its origin and was rebuilt: `parse_quote {
type A = I32 }` spliced back as `type A =  I32`. Only an identifier's hygiene
changes what syntax means, so the comparison now ignores hygiene on every other
token. A quoted `wrap`, `from` or `opaque` body still rebuilds, because those
words lex as identifiers; the result is correct but not lossless. Task 11 will
show whether that matters.

Differences for task 11:

- Compiled `Parse` for `Sequence<Item>` rejects `;` between items. The
  interpreter's `parse_quote` accepts `type A = I32; type B = I32` because it
  parses the template as an item list. The leaf parser is shared with the
  interpreter's `Parse`, so changing it would change interpreted argument
  parsing too.
- A `parse_quote` result's origin is the whole filled fragment, so splicing it
  unchanged gives the template's leading space (`" f a"`). A parsed
  `Sequence<Item>` splices from its first item.


Verification: `cargo test --workspace` passes 2328 tests. The expansion
comparison matches 231 of 232 inputs; the other is `std/syntax.sta` itself,
whose only change is `parse_quote` taking `move`. The macro closure check
reports no diagnostics.
