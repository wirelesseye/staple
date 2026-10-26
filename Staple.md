# The staple language

**Staple** is a programming language with reactive signals and powerful,
approachable metaprogramming as core language features. Its compiler and tooling ship as a
single executable called **`staple`**.

This document describes the language as it currently exists. Details listed as
unspecified are not yet part of the language design.

## Language goals

staple has two major, long-term goals:

1. Reactions and signals are core language features. Reactive programming should
   feel native to staple in the way that signals and reactions feel native when
   using Solid.js, rather than being an incidental library convention.
2. Metaprogramming is powerful and approachable. staple should provide the
   directness and expressive power associated with Lisp metaprogramming while
   retaining staple's own syntax.

These are defining goals of the language. Other language features should be
evaluated partly by how well they compose with reactivity and metaprogramming.

### Reactions and signals

Signals represent values that can change over time. Reading a signal from a
reactive computation establishes a dependency on that signal. When a signal's
value changes, computations which depend on it can be updated automatically.

Reactions are computations which react to their signal dependencies. As in
Solid.js, dependency tracking should be fine-grained: changing a signal should
update the computations which actually depend on it, without requiring a
virtual DOM or broad re-execution of unrelated code.

Signals and reactions are intended to be language concepts rather than ordinary
types and functions supplied solely by a framework. This allows staple's type
system, compiler, runtime, tooling, and metaprogramming facilities to understand
reactive relationships directly.

The reactive runtime provides writable `let signal` bindings, lazily cached
derived `let` bindings, synchronous reactions, dynamic dependency tracking, and
lexical reaction ownership through the `Reactive` resource. A FIFO executor
drains reactions to quiescence and supports explicit batching. Stackless
coroutines with explicit host-driven schedulers build on the same executor
(see [Coroutines](#coroutines)); the two are kept isolated — a `pump` is
rejected while a reaction or batch is in flight, and dependency tracking is
detached around every coroutine resume. Precise or incremental collection,
expression-position `await`, and alternative executor policies remain future
work.

### Metaprogramming

Metaprogramming should make staple programs capable of inspecting, generating,
and transforming staple code. The experience should aim for Lisp's combination
of expressive power and conceptual simplicity, without requiring staple's
surface syntax to become S-expressions.

The parser's lossless syntax representation is important to this goal. Source
code, including comments and formatting, can be represented as data rather than
being reduced immediately to semantic values. Metaprograms should be able to
work with structured syntax instead of assembling source-code strings.

Expression macros transform compiler-owned syntax values before ordinary name
resolution and type checking. The syntax API is declared by `std.syntax` and is
not part of the `std.core` prelude, so modules using it must import the required
names explicitly, for example `use std.syntax (parse_quote, Expr, SyntaxNode)`.
Examples below assume the syntax names they use have been imported.

`Syntax` is an opaque, arbitrary lossless syntax fragment. It preserves source
text, trivia, spans, and hygiene without requiring its contents to form one
structural or grammatical node. `SyntaxNode` is the structural sum of `Expr`,
`Type`, `Pattern`, `Item`, `Visibility`, punctuation nodes such as `Comma`, and
the three delimited syntax types. `Expr` is currently the sum of `Ident<String>`,
`CallExpr`, and `UnstructuredExpr`.
`CallExpr` exposes `callee: Expr` and
`argument: Expr`; `UnstructuredExpr` preserves every other expression form
without exposing its fields yet. `Type` is likewise the sum of `ProductType`
and `UnstructuredType`. `ProductType` wraps the `Sequence<ProductTypeField>` of
a plain comma-separated product type such as `(x: I32 = 0, String)`; each
`ProductTypeField (name, field_type, default)` holds an `Option<Ident<String>>`,
the field's `Type`, and its `= ...` default as an `Option<Expr>`. Slotted and
variadic products, spreads, and products with `mut` or `move` elements are
`UnstructuredType`, like every other type form. Both are exposed for
inspection only: a `Type` value is spliced as written. A macro body is a compile-time Staple function
whose parameters use a slotted list, followed by a single `=>` before the
body — never a curried `a => b => body` chain. Every parameter consumes one
atomic syntax unit; an omitted parameter type means `SyntaxNode`, not opaque
`Syntax`:

```staple
macro choose = (condition; then; else) => parse_quote {
    match $condition {
        True => $then,
        False => $else,
    }
}
```

A slot whose type is a product consumes one parenthesized argument whose
elements are separated by commas, and binds it as a product of syntax values.
The empty product `()` accepts only `()`:

```staple
macro unit = () => quote {}
unit ()

macro sum = (a: Expr; b: Expr, c: Expr; d: Expr) => parse_quote { $a + $b + $c + $d }
sum 1 (2, 3) 4
```

Here `sum` has three slots, and its second slot is the product `(b: Expr, c:
Expr)`, so `(2, 3)` is one argument. An annotated macro spells the same
signature `(Expr; (Expr, Expr); Expr) -> Expr`.

Typed parameters constrain the grammar accepted at an invocation. `Ident` is a
generic syntax type whose argument constrains its spelling, declared as
`pub type Ident<Spelling = String where Spelling <: String> wrap pub Spelling`.
`Ident` and `Ident<String>` both accept any identifier — `Spelling` defaults to
`String` (see [Default type parameters](#default-type-parameters)) — and
`Ident<"else">` accepts exactly the identifier `else`, because every string
literal type is a subtype of `String` (see [Subtype
bounds](#subtype-bounds)). A literal identifier parameter may use
a typed wildcard when its syntax is not needed, or a binding when it is:

```staple
macro conditional = (
    condition: Expr;
    then_branch: Expr;
    _: Ident<"else">;
    else_branch: Expr;
) =>
    parse_quote {
        match $condition {
            True => $then_branch,
            False => $else_branch,
    }
}
```

Delimited syntax parameters match one balanced source argument while exposing
its contents structurally. Slotted contents are juxtaposed in the source, so
`Parenthesized<(Ident<String>; Ident<String>)>` accepts `(left right)`. Product
contents are separated by commas, so `Parenthesized (Ident String, Ident
String)` accepts `(left, right)`, with an optional trailing comma:

```staple
(1 2 3)   // Parenthesized (Expr; Expr; Expr)
(1, 2, 3) // Parenthesized (Expr, Expr, Expr)
```

`Angled<C>` explicitly captures `<...>` using the same content shapes, such as
`Angled<Separated<Type, Comma>>` for `<I32, List<String>>`. Nested type arguments
are preserved, and comparisons inside ordinary bracketed groups do not close
an angle capture. Angle groups are recognized in macro argument syntax; ordinary
comparison expressions retain priority when they parse successfully.

`TypeParameter` parses one declaration parameter and its optional default.
`TypeParameters` parses a complete `<...>` declaration header, using the same
grammar as `type`, including defaults and the inner `where` clause. Both support
`Parse`, contextual `parse_quote`, inspection, and lossless quotation splices.
Their fields are:

- `TypeParameter.name`: `Ident<String>`.
- `TypeParameter.default`: `Option<Type>`.
- `TypeParameter.declaration`: the parameter's original `Syntax`.
- `TypeParameter.argument`: the parameter's name as `Type`.
- `TypeParameters.parameters`: `Sequence<TypeParameter>`.
- `TypeParameters.declaration`: the complete original `<...>` header as `Syntax`.
- `TypeParameters.arguments`: the corresponding application header, without
  defaults or bounds, as `Syntax`.
- `TypeParameters.bounds`: the original `where` clause, or empty `Syntax`.

For example, a header `<T = String where T <: String>` retains that entire
header in `declaration`, while `arguments` is `<T>`. A macro can accept
`Option<TypeParameters>` to make the header optional. Top-level `Option<T>`
macro parameters try consuming one matching argument and backtrack to `None`
if the remaining parameters cannot match.

Matched contents destructure the same way, as in `Parenthesized (left; right)`
or `Parenthesized (left, right)`. A product nested inside slotted contents is
itself a parenthesized, comma-separated group, so `Parenthesized (Ident String;
(Expr, Expr))` accepts `(pair (1, 2))`. `Parenthesized`, `Bracketed`, `Braced`, and `Angled` may
instead contain `Sequence<T>`, which accepts zero or more consecutive values of
`T`; for example, `Bracketed<Sequence<Ident<String>>>` accepts `[]` and `[one
two]`. A function-style macro may also use one `Sequence T` as a top-level
parameter:

```staple
macro collect = (
    values: Sequence<Ident<String>>;
    _: Equals;
    body: Braced<Syntax>;
) => ...
```

The sequence consumes zero or more consecutive top-level arguments. Matching
starts greedily and backs up until the remaining parameters match, so the
following `Equals` terminates `values`. `Visibility` and
`MacroCallMetadata` do not count as terminators because they may supply an
implicit `Private` value without consuming source. A top-level sequence may be
the final parameter, in which case it has no required suffix and simply
consumes the maximal run of consecutive matching arguments; a longer complete
overload still wins the atom-count comparison, and any arguments left after the
run apply to the expanded expression as an ordinary call. A top-level sequence
may not appear more than once in a signature or be used by a modifier macro.
`Sequence` inside delimiters remains restricted to the entire contents of one
of the three delimiter types.
Captured sequences may be passed through compile-time helpers and destructured
as `Sequence<()>` or `Sequence (first: T, rest: Sequence<T>)`. `Sequence
SyntaxNode` uses shortest structural atoms, treating a nested balanced delimiter
as one atom. In contrast, `Parenthesized<Syntax>`, `Bracketed<Syntax>`, and
`Braced<Syntax>` capture their entire contents as one opaque fragment, including
trivia and syntax that is not yet structurally supported. A single fixed
content never needs the grouping parens that separate multiple ones, so
`Parenthesized<Expr>` and `Parenthesized<Expr>` are read identically; the
parens are required only once there is more than one fixed element, as in
`Parenthesized<(Ident<String>; Ident<String>)>`.

`Comma` is singleton syntax and may be matched explicitly, as in
`Parenthesized<(Ident<String>; Comma; Ident<String>)>`. A homogeneous sequence
whose elements are separated by commas uses `Separated<T, Comma>`, for example
`Parenthesized<Separated<Ident<String>, Comma>>`. It accepts empty and singleton
contents, requires one comma between subsequent elements, and permits a
trailing comma. Its value contains the elements without the commas, together
with its separator and whether the source had a trailing separator. Fresh
values use `Separated (separator: Comma, elements: (...), trailing: True)` or
the corresponding `False` value.
Like `Sequence`, `Separated` is valid only as the complete contents of a
delimiter. Its `elements` field is a `Sequence`. A separated element may be a
slotted product shape. `Option<T>` (the ordinary `std.option` type) is
available as a macro parameter shape only inside such slotted products and
captures `None` or `Some value`, so
`Braced<Separated<(Ident<String>; Option<Type>), Comma>>` accepts both
`Literal String` and `Wildcard` entries.

`Type` and `Pattern` parameters accept opaque type and pattern syntax. One
atomic name may be passed directly. Compound syntax uses an outer pair of
parentheses to delimit the macro argument. When those parentheses are required
by the type or pattern itself, as for a product, the same pair serves both
purposes:

```staple
inspect_type I32
inspect_type (I32 -> I32)
inspect_type (Result I32 Error)
inspect_type (I32, String)

inspect_pattern name
inspect_pattern (Some value)
inspect_pattern (left, right)
```

An additional grouping pair remains accepted, so
`inspect_type ((I32, String))` and `inspect_pattern ((left, right))` capture the
same product syntax. The matcher first interprets an outer pair as an argument
delimiter, preserving existing calls such as `(Some value)`, then retains that
pair when its contents alone are not a complete type or pattern.

Consequently, `inspect_type Result I32` is an invocation with separate
arguments, not one applied type. Type and pattern values have no structural
fields beyond `ProductType` yet, but may be stored, passed through compile-time helpers, and spliced
into matching positions in a quotation. For example, `$ty` may appear in a
type annotation and `$pattern` may appear as a binding, function, or match
pattern. A category-mismatched splice is rejected during expansion.

When overloads from expression and type or pattern grammars accept the same
source syntax, the invocation is ambiguous. Specialized syntax categories are
narrower than `SyntaxNode`, and `SyntaxNode` is narrower than opaque `Syntax`.
`Type`, `Pattern`, and `Expr` remain mutually incomparable where their source
grammars overlap.

The braces in `quote { syntax }` and `parse_quote { syntax }` delimit an
opaque source fragment and are not part of the result. Both substitute
splices first. `quote` always returns the substituted fragment as opaque
`Syntax`, without parsing it. `parse_quote` additionally interprets the
fragment according to its expected result type. Their compiler-provided
contracts are:

```staple
pub macro quote: Braced<Syntax> -> Syntax
pub macro parse_quote: Braced<Syntax> -> Syntax
```

These signatures describe the syntax emitted by the intrinsic macros, not the
type of the compile-time syntax value that the emitted expression constructs.

`parse_quote` uses its contextual type to choose the kind of syntax value its
emitted expression constructs. Supported contexts are `SyntaxNode`, `Expr`,
`Type`, `Pattern`, `Item`, `Sequence<Item>`, `Comma`, `Equals`, `FatArrow`,
`Visibility`, and the three delimited syntax types (`Parenthesized`,
`Bracketed`, `Braced`) with any of their supported content shapes — never
`Syntax`, which is `quote`'s own always-opaque value, not a structured syntax
node `parse_quote` can construct contextually.
`SyntaxNode` requires exactly one shortest
structural node. The grammatical categories parse the complete fragment in
their respective contexts, while `Sequence<Item>` parses zero or more complete
items in source order. Annotations, type ascriptions, declared helper or macro
result types, and other typed compile-time boundaries provide `parse_quote`'s
contextual construction type; requesting `Syntax` through any of them is rejected.
A value-defined helper declares its result with an ascription, as in
`def increment = value: Expr => Expr :: parse_quote { $value + 1 }`. A
`parse_quote` reached with no contextual type at all — for example inside an
untyped compile-time helper — is rejected the same way. This contextual
selection is specific to `parse_quote`: wrapping a `quote` fragment in
a type ascription or a typed binding does not reinterpret it, since `quote` always
returns opaque `Syntax`.

A macro's declared or inferred result is also checked against its body: when
that result is a concrete syntax type — `SyntaxNode`, `Expr`, `Type`,
`Pattern`, `Item`, `Sequence<Item>`, `Comma`, `Equals`, `FatArrow`,
`Visibility`, or a delimited syntax type — but the body's tail expression is
a bare `quote { ... }`, the mismatch is rejected at the declaration, since
`quote` can only ever produce opaque `Syntax`. Replacing that `quote` with
`parse_quote` resolves it.

Opaque fragments may contain empty input, punctuation, comments, or temporarily
malformed grammar and may be spliced into a later `parse_quote` for contextual
interpretation. A raw `Syntax` result entering ordinary expression or item
source is reparsed for that placement. `$name` splices a captured value while
preserving its hygiene. `$items...` splices a `Sequence<Item>` in an item-list
position.

##### Parsing syntax with `Parse`

A macro or compile-time helper that holds an opaque `Syntax` value, such as the
contents of a captured `Braced<Syntax>`, can parse structure out of it with the
streaming `Parse` trait in `std.syntax`:

```staple
pub type ParseError wrap pub (message: String)
pub type ParseStream opaque

pub extern "staple-intrinsic" {
    stream: Syntax -> ParseStream          // starts at the beginning
    is_empty: ParseStream -> Bool          // only trivia remains
    remaining: ParseStream -> Syntax       // the unconsumed rest, losslessly
    error_at: (ParseStream; String) -> ParseError
}

pub trait Parse T {
    parse: mut ParseStream -> Result<T, ParseError>
    parse_all: Syntax -> Result T ParseError = ... // rejects leftover syntax
}
```

A `ParseStream` is a cursor over a syntax fragment and a copyable value, so
copying one forks it: `let mut fork = input` is a backtracking point and
`input = fork` commits it. `Parse.parse` reads one `T` from the front of the
stream and advances the stream in place; on failure it returns a `ParseError`
whose message names the stream's position and leaves the caller's stream where
it was. `Parse.parse_all` parses all of a `Syntax` as one `T` and reports
`unexpected syntax` for anything left over. The target is the expected type of
the call, as with `parse_quote`, or an explicit type argument:

```staple
macro fields = body: Braced<Syntax> => {
    let Braced (contents) = body
    let parsed: Result<Separated<(Ident<String>; Equals; Expr), Comma>, ParseError> =
        Parse.parse_all contents
    match parsed {
        Ok fields => ...,
        ParseError (message) => panic message,
    }
}

let mut input = stream contents
let name = Parse.parse.<Ident String> input?
let comma = Parse.parse.<Comma> input?
```

A call with neither a contextual `Result<T, ParseError>` type nor a type argument
is rejected, as is a type with no `Parse` implementation. `Parse.parse`,
`syntax.Parse.parse`, and (when `Parse` is imported) a bare `parse` all name the
same member, and the intrinsics import like any other `std.syntax` item.

`Parse` is greedy and only backtracks through explicit forks, which is
different from how macro parameters match. A macro parameter shaped
`(Expr; Expr)` matches `a b` as one atom each, but `Parse.parse.<(Expr; Expr)>`
on `a b` fails, because the first `Expr` reads the whole call `a b`. Use
`SyntaxNode`, which reads one shortest structural atom (a balanced group counts
as one), or delimiters to get atom semantics. Likewise, `Option<Type>` reads a
`Type` whenever one starts there, and `I32 value` is one applied type. Products
parse their *bare* contents: `(A; B)` reads `A` then `B`, juxtaposed, and
`(A, B)` reads `A`, a comma, then `B` with one optional trailing comma;
`Parenthesized<(A, B)>` is the delimited form that requires the parentheses.

The built-in nodes implement `Parse` with ordinary `impl` items in
`std/syntax.sta`:

- `Expr`, `CallExpr`, `StringExpr`, `UnstructuredExpr`, `Type`, `ProductType`,
  `UnstructuredType`, `Pattern`,
  `BindingPattern`, `NominalPattern`, `Item`, `ModifiedItem`,
  `TypeDeclarationItem`, `UnstructuredItem`, `Modifier`, `ModifierCall`,
  `UnstructuredModifier`, and `TypeBody` read the
  longest node the grammar accepts from the cursor and stop where it stops: `Expr`
  reads `a + b` from `a + b, c`.
- `Ident<S>` (a spelling or `String`), `Comma`, `Equals`, and `FatArrow` read one
  token. `Visibility` reads `pub` or `pub(package)`, and yields `Private` without
  consuming anything when neither is written. `SyntaxNode` reads one atom.
- `Syntax` takes everything that is left, `()` takes nothing, and
  `Option<T>` yields `None` without consuming anything when `T` does not parse.
- `Parenthesized<C>`, `Bracketed<C>`, and `Braced<C>` parse their contents as `C` and
  require them to be used up. The result keeps the group's original syntax.
- Products `(A; B)` and `(A, B)`, `Sequence<T>` (until the stream ends or `T`
  stops parsing), and `Separated<T, Comma>` (an optional trailing comma).

A program adds its own implementations. They are ordinary Staple code that runs
at compile time, may be generic, and recurse through `Parse.parse`:

```staple
pub type KeyValue wrap pub (key: Ident<String>, value: Expr)

impl Parse KeyValue {
    parse = mut input => {
        let key = Parse.parse.<Ident<String>> input?
        let equals = Parse.parse.<Equals> input?
        let value = Parse.parse.<Expr> input?
        Ok (KeyValue (key: key, value: value))
    }
}

impl<T where Parse T> Parse Pair<T> {
    parse = mut input => {
        let first = Parse.parse.<T> input?
        let comma = Parse.parse.<Comma> input?
        Ok (Pair (first: first, second: Parse.parse.<T> input?))
    }
}
```

`parse` must declare its parameter `mut`; `parse_all` may be replaced the same
way. A type parameter used as a target needs a `Parse` bound. The compiler
never type-checks these implementations as runtime code: they are checked
as compile-time code and dropped after macro expansion, together with the
intrinsics they call. Because of that, the orphan rule is applied by the macro
expander: outside the standard library, the implemented type must be a nominal
type declared in the implementing package. User `Parse` types are not macro
parameter types, and macro argument matching is unchanged; the trait is used by
explicit calls only.

Compile-time `mut` parameters are passed by address. A call such as
`skip_one input` hands the callee the caller's own `mut` binding, so the callee's
assignments are visible after the call, while passing an immutable binding to a
`mut` parameter is rejected like it is at run time. In compile-time code, a
postfix `?` on a `Result` yields the `Ok` payload, and any other value leaves
the innermost compile-time function with that value; `return` likewise leaves
the whole function rather than just its block. Compile-time recursion, including
the recursion of `Sequence` and `Separated`, shares the evaluation depth limit
described above.

Identifiers and calls may also be inspected, constructed, and changed as
compile-time values:

```staple
macro replace_argument = (value: CallExpr; replacement: Expr) => {
    let original = value
    let mut changed = value
    changed.argument = replacement
    parse_quote { ($original, $changed) }
}

macro build_call = _: Expr => {
    let name = Ident "generated"
    let call = CallExpr (callee: name, argument: quote { 1 })
    call
}
```

Syntax mutation follows ordinary value semantics: changing `changed` does not
change `original`. A `mut` syntax binding captured by a compile-time function
is a shared cell, like an ordinary captured `mut` binding.
Constructed identifiers use the macro definition's hygiene context. Syntax
values inserted as call children retain their existing context.

A macro may return one item when its invocation is the entire top-level
expression item:

```staple
macro define_answer: Expr -> Item =
    value => parse_quote {
        def answer = () => $value
    }

define_answer 42
```

Item-producing macros may generate statements, type declarations, extern
blocks, trait declarations, trait implementations, `use` declarations, and
submodules (`mod`). Generated `macro` declarations are rejected because they
would require rebuilding expansion and loading scopes. An item result is
invalid in bindings, function bodies, blocks, call arguments, and every other
expression position. `Item` values may be passed between compile-time helpers.
Function-style macros cannot accept `Item` parameters.

Modifier macros are the item-input form. They are declared with `macro @name`
and applied immediately before a resolver-safe definition:

```staple
macro @identity: Item -> Item = item => item

macro @replace: (Parenthesized<Expr>; Item) -> Item =
    (value; item) => parse_quote { let generated = $value }

@identity
@replace(42)
def original = () => 0
```

`@doc` is a built-in modifier that attaches Markdown documentation to a named
declaration. It requires one string literal, may be repeated, and is shown by
editor hover:

```staple
@doc("Returns the number of items.")
pub def length: List<T> -> USize = list => ...
```

Triple-slash comments are shorthand for `@doc`, with the text after `///`
used verbatim. Consecutive documentation comments preserve their source order:

```staple
/// Returns the number of items.
/// This operation does not mutate the list.
pub def length: List<T> -> USize = list => ...
```

Documentation may be attached to types, bindings, modules, macros, traits,
trait members, implementation members, and extern bindings. It has no runtime
effect. `@doc` is compiler-reserved; qualified user modifiers named `doc`
remain ordinary modifiers.

A modifier has signature `Item -> Item`, `Item -> Sequence<Item>`, or
`Item -> Syntax`, optionally preceded by one optional explicit
`Parenthesized<Expr>`, `Parenthesized<Type>`, `Parenthesized<Pattern>`,
`Parenthesized<Modifier>`, or `Parenthesized<Separated<Modifier, Comma>>`
argument before the compiler-supplied item. `Parenthesized` is required
because the explicit argument is always delimited by parentheses, including
an expression argument, matching how every other delimited macro argument is
declared. `Ident`, `CallExpr`, and `UnstructuredExpr` may be used as narrower
expression parameter types. `SyntaxNode`, opaque `Syntax`, and `Item` are not
valid explicit modifier arguments.

Modifier lists are part of item syntax. The outermost modifier runs first. For
`@outer @inner def ...`, `@outer` receives a `ModifiedItem` containing
`@inner` and the definition. Returning that item continues expansion with
`@inner`; replacing or unwrapping it discards the remaining modifier. A
returned item with modifiers is handled the same way, one outermost modifier
per expansion step. Modifiers may target `let`, `def`, `type`,
`extern`, `trait`, and `impl` items. They cannot target expression statements,
`use`, `mod`, or `macro` declarations. Modifier and function-style macro names
occupy separate namespaces, and imports, renames, namespaces, and re-exports
carry both.

Modifiers may also target eligible private items in block expressions. A block
modifier may produce zero, one, or many replacement items, which are spliced at
the modifier's position. Every result must itself be valid in a block, so public
items and `extern`, `macro`, `trait`, or `impl` declarations are rejected.

A modifier declared `Item -> Item` must return exactly one `Item`.
A modifier declared `Item -> Sequence<Item>` or `Item -> Syntax` may instead
expand to zero, one, or many items, reusing the same reparse and splice
machinery as an item-producing function-style macro. Every returned item is
expanded independently; a returned `ModifiedItem` continues with its own
outermost modifier. Producing zero items deletes the modified item.

`Item` exposes pending modifier lists as `ModifiedItem (modifiers, item)`.
Modifiers are ordered outermost first and represented by lossless `Modifier`
values, a sum of `ModifierCall` and `UnstructuredModifier`. Written invocations
match `ModifierCall (namespace, name, argument)`: the namespace is a sequence of
identifiers, outermost first, and the optional opaque `Syntax` argument excludes
its parentheses. Documentation comments match only `UnstructuredModifier` and
retain their comment form when passed through. Constructing a `ModifierCall`
reparses its invocation. `duplicate_modifier` creates two independent modifiers
with fresh syntax identities, preserving source locations and dropping any
original enclosing item wrapper. `ModifiedItem` can be destructured and reconstructed to
preserve, reorder, or discard pending modifiers. A wrapped type declaration
matches `ModifiedItem`, not `TypeDeclarationItem`, until unwrapped.
`Item` exposes unwrapped type declarations as `TypeDeclarationItem (name,
declared_type, type_parameters, body, resource_dependencies)`: the name as an
`Ident` (match `Ident spelling` for its text), the applied declared type, the
flattened type-parameter names, the body as an `Option<TypeBody>` (`None` for a
bodyless singleton; `WrapperBody` and `FromBody` carry the representation
visibility and type), and the entries of its `with {…}` clause as
`ResourceDependency (resource_type, mutable)` values, in written order. All
other items match `UnstructuredItem`
and remain lossless but opaque. `BindingPattern` and `NominalPattern` provide
the structured pattern construction needed by declaration modifiers.

Visibility is also compiler-owned syntax. Its atomic variants are `Private`,
`Package`, and `Public`. They can be matched and passed through compile-time
helpers accepting `Visibility`, but cannot survive expansion as runtime values.
`MacroCallMetadata` is a compiler-owned product permitted only as the first
parameter of a function-style macro:

```staple
pub type MacroCallMetadata wrap (
    modifiers: Sequence<Modifier>,
    visibility: Visibility,
)
```

```staple
macro define_alias = (metadata: MacroCallMetadata; ty: Type) => {
    let visibility = metadata.visibility
    parse_quote { $visibility type Generated = $ty }
}

pub define_alias I32
```

The metadata contains the ordered `Sequence<Modifier>` prefix and the supplied
visibility. `pub` and `pub(package)` before the macro name supply the
corresponding visibility; an unprefixed call supplies `Private`, and no-prefix
calls receive an empty modifier sequence. A prefixed call is parsed in
module-item grammar, but its macro may return either an item or an expression.
Normal syntax placement rules still reject type, pattern, or visibility results
that have no valid placement.

A function-style macro whose first parameter is `MacroCallMetadata` may declare
its vocabulary between its name and annotation:

```staple
macro emit modifiers (@traced, @all(Separated<Modifier, Comma>)):
    (MacroCallMetadata; Expr) -> Item = (metadata; value) => ...
```

Only unqualified prefixes match vocabulary names, by spelling. A listed name
wins even when a real modifier macro of that name is in scope; qualify the name
to reach that macro. Arguments follow the modifier argument rules above. A
nested modifier must include `@`, as in `@all(@derive_debug)`. Other prefixes
must name real modifier macros in scope. Unknown prefixes and wrong argument
shapes are errors at the call site. Each overload has its own vocabulary,
checked after overload selection; vocabulary does not select an overload.
Macros without a clause retain permissive metadata prefixes. Nested modifiers
remain unresolved until applied. Prefixes left in metadata are not forwarded
automatically.

`typegroup` accepts `@all(...)` for its group and every variant, and
`@variants(...)` for its variants alone. Their comma-separated modifiers are
prepended in prefix order, ahead of each variant's own modifiers. Ordinary
prefixes still apply only to the group, which keeps the written prefix order
with each `@all(...)` replaced by its contents. Repeated unknown-modifier
errors from the same source location are reported once.

An ordinary `Visibility` parameter may appear in any position. At that
position each visibility form consumes one source atom; if none is present,
`Private` is injected without consuming the following argument:

```staple
macro configure = (
    value: Expr;
    vis: Visibility;
    ty: Type;
) => ...

configure value I32
configure value pub I32
configure value pub(package) I32
```

Metadata-aware overload matching ranks candidates by source atoms consumed,
not by the number of parameters after implicit values are inserted. Therefore
an ordinary overload and an implicitly-private overload consuming the same
source syntax are ambiguous. An explicit prefix before the macro name selects
only overloads beginning with `MacroCallMetadata`.

`Visibility` is also a `parse_quote` result type, following the same
absence-means-`Private` convention: `parse_quote { }: Visibility` yields
`Private`, while `pub` and `pub(package)` yield `Public` and `Package`.

Inside an item quotation, a visibility value may be spliced immediately before
a declaration. `Private` emits no prefix, `Package` emits `pub(package)`, and
`Public` emits `pub`. Existing declaration rules are checked after
substitution. Representation visibility is written in the type body itself:
`wrap pub T` exposes the constructor to importers, and
`wrap pub(package) T` keeps it package-scoped.
Modifiers surrounding a metadata-aware call belong to the metadata value and
are not automatically applied after the call has produced its result.

Quotations may contain multiple items and return `Sequence<Item>`. Such a
sequence can replace a top-level macro invocation or be inserted into an inline
module with `$items...`. Identifier syntax may be spliced into generated module
and type names. `parse_quote` type quotations are selected contextually by an
annotated compile-time binding or `Type :: expression`:

```staple
let alternative: Type = parse_quote { $group.$variant }
Type :: parse_quote { $left | $right }
```


Macros are hygienic. Names and bindings written in a quotation retain the
definition module's environment and receive a fresh expansion identity, while
spliced expressions retain their caller environment. Hygiene follows the
syntax rather than the API that parses it: each token of a quotation records
where it was written, so a `quote` fragment reparsed by `parse_quote` or by
`Parse` resolves its quoted names in the macro's module and its spliced names
where the caller wrote them. Syntax taken directly from a caller is
unaffected and resolves at the call site. A macro normally consumes
the number of arguments described by its syntax-node parameter slots;
a top-level `Sequence` instead consumes as many matching arguments as possible
while preserving a match for any following parameters. When it is the final
parameter it has no such suffix and consumes the whole remaining run. Further
call arguments apply to the expanded expression.

Macro signatures with multiple parameters must spell the slotted product literally (`(Expr; Item) -> Item`); macro signatures are read before alias expansion, so an alias counts as one parameter. Macro overloading is unaffected by the removal of function overloading.

Macros may be overloaded by declaring the same name more than once in one
module. An invocation selects the complete matching overload that consumes the
most syntax atoms. Same-length matches use pattern specificity: a literal
identifier is narrower than `Ident<String>`, `Ident<String>` is narrower than
`Expr`, specialized categories are narrower than `SyntaxNode`, and `SyntaxNode`
is narrower than opaque `Syntax`. Specificity must hold at every consumed
source atom; incomparable matches are ambiguous. A fixed parameter is more
specific than an otherwise equivalent element captured by a top-level
sequence. Exact duplicate patterns are rejected.

An incomplete longer overload does not prevent a shorter complete overload
from matching. Any syntax left after the shorter expansion is applied to its
result as an ordinary call. Imports, renames, namespaces, and re-exports carry
the whole overload set, but overload sets from unrelated imports do not merge.

Compile-time evaluation supports pure functions, bindings, reassignable and
mutable local cells, products, matches, literals, recursion, pure integer
operations, and `loop` with `break`, `break value`, and `continue`. `return`
and `?` inside a loop leave the enclosing function. It rejects external or runtime-only effects. Expansion is limited
to 128 nested macros and each top-level invocation is limited to 1,000,000
evaluation steps. Syntax
values are compile-time-only. This release supports `parse_quote`-driven
contextual expression, type, pattern, item, and item-sequence quotation;
`quote`'s opaque fragments; structured identifier and call expressions;
scalar splices; repeated item and product-expression splices; streaming `Parse`
of syntax; and ordinary trait and companion calls, including iterator pipes.
It also supports opaque item input through modifier macros and atomic visibility
syntax. General repeated type and pattern splices,
function-style item input, item inspection, and structured access to other
expression forms remain future work.

Compile-time trait calls select an implementation from the receiver value's
shape (`Iterator.next iter`), or from an expected result type
(`let value: T = Default.default ()`). Explicit `.<...>` arguments, generic
bindings, and type ascriptions also supply dispatch types. Nominal values erase
their type arguments; unbound arguments do not constrain implementation bounds.
Integer and float values have no width for dispatch, so implementations for
multiple widths are ambiguous. Ambiguous calls report the matching headers.
Compile-time checking remains permissive for these calls: missing implementations
and undetermined dispatch types are diagnosed when the macro evaluates them.

`Sequence<T>` implements `Iterator`, `IntoIterator`, `Default`, and
`FromIterator`. Its iterator consumes elements in order. Iterate a `Separated`
value through its `.elements` sequence. Compile-time code can use `for`, `map`,
`filter`, `fold`, `enumerate`, `zip`, `skip`, `extend`, and the other ordinary
iterator companion operations. Pipes select nominal companions first, then
applicable blanket companions, lexical helpers, and trait members.

```staple
let names: Sequence<Ident> = fields |> map ((field: Field) => field.name) |> collect
let count = names |> fold 0 ((total, _) => total + 1)
for name in names { /* use name */ }
```

At compile time, `collect` needs an expected `Sequence<...>` type or explicit
arguments such as `collect.<Sequence<Expr>, _>`. Sequence and comma-separated
parsing use loops, so their length is bounded by the evaluation-step budget
rather than recursive helper depth. Ranges, `to_list`, and collecting into
`List<T>` or `String` remain runtime-only because their implementations need
width-specific dispatch or runtime storage intrinsics.

Compiler-provided macros use typed bodyless contracts. `std.syntax` declares
`pub macro quote: Braced<Syntax> -> Syntax` and
`pub macro parse_quote: Braced<Syntax> -> Syntax`, and
`std.cinterop` declares `pub macro c_string: Expr -> Expr`. `std.source`
declares `pub macro line: () -> Expr`, `pub macro column: () -> Expr`, and
`pub macro file: () -> Expr`. None of these modules is re-exported by
`std.core` or the prelude; their APIs require explicit imports.

`line ()` and `column ()` expand to integer literals holding the 1-based line
and column (counted in characters) of the invocation, and `file ()` expands to
a string literal holding the invoking file's path, relative to the containing
package's directory when the file lies inside it:

```staple
use std.source.(line, column, file)

let location = (file (), line (), column ())
```

A call written at the source site of the outermost macro invocation, such as
one passed as a macro argument, reports its own location. A call produced by a
macro definition reports the location of the outermost invocation that led to
it.

#### `@track_caller` and `Location`

`@track_caller`, declared in `std.source` as `pub macro @track_caller: Item ->
Item`, makes a function report where it was called from instead of where it is
defined. It is not re-exported by `std.core` or the prelude, so it needs
`use std.source.(track_caller)`; a `companion` body sees its defining
module's import. It applies only to a module-level or companion `def` whose value is a
function literal, or to a trait member. It takes no argument, and is rejected on
`let`, `const`, `extern`, type, trait, and impl items, on implementation
members, on local `def`s, and on a function that captures local variables
(module-level values are not captures). Only the outer stage of a curried `def`
is tracked.

On a trait member, `@track_caller` is written among the member's modifiers
like `@doc` and needs no import. It tracks the member's default and every
implementation of the member, which inherit it and cannot declare it
themselves, so every call to the member reports its caller whichever
implementation is selected:

```staple
trait Check T {
    @track_caller
    check: T -> () = value => println "checked at ${Location.caller ()}"
}
```

A tracked trait member used as a value fixes the site at the reference, like
a tracked `def`; the value must have a concrete type whose implementation is
known.

`std.source.Location` is a `(file, line, column)` value. `Location.caller ()`
is itself a `@track_caller` function, so it reports the location of its own
call, and inside a `@track_caller` function the location of that function's
caller:

```staple
use std.io.(IO, println)
use std.source.(Location, track_caller)

@track_caller
pub def check = condition: Bool =>{IO} () :: match condition {
    True => (),
    False => println "check failed at ${Location.caller ()}",
}

check (1 > 2)   // prints the position of `check` in this call
```

The location is the start of the referencing expression: `f` in `f x`, `f` in
`x |> f`, and `Location` in `Location.caller ()`. `Location.file`,
`Location.line` and `Location.column` read its parts, with `file` and `column`
counted as in `std.source.file` and `std.source.column`, so `Location.file`
names the same path as `file ()`. `Location` implements `Display` as
`file:line:column`. Its representation is private, so a `Location` can only come
from `Location.caller`.

A `@track_caller` function passes the location through a call to another
`@track_caller` function only from its own body: such a call reports the same
caller, so wrappers stay transparent. Closures, block arguments, and `coro`
bodies inside the function are separate functions and report their own sites,
and `Location.caller ()` inside them reports that site; the compiler-reserved
`__caller_*` intrinsics are rejected there. A value reference (`let g = f`)
fixes the site at the reference and does not change the function's type, so
`g ()` reports the `let`. A function referring to itself, directly or as a
value, forwards its own caller. A generic `@track_caller` function stays
monomorphised, but using one as a polymorphic value (for example by binding it
to a quantified type annotation) is an error.

A reference produced by a macro definition reports the outermost macro
invocation that led to it, as `std.source` macros do. The location costs one
constant pointer per call: it travels in the closure environment slot that a
captureless function leaves unused, so the function's type and calling
convention do not change.

The prelude's `panic` (from `std.process`) is a `@track_caller` function. It
writes the message and the location of its call to standard output, then exits
with status 1; called from a `@track_caller` function, it reports that
function's caller:

```text
panic: expected a positive value at src/main.sta:7:9
```

A binding that is modified only by `@doc` or `@track_caller` remains available
to compile-time macro evaluation, so macro bodies can still call `panic`, which
reports a compile-time diagnostic there.

## Source files

staple source files use the `.sta` extension.

Whitespace and comments are preserved by the parser, so parsing and reproducing
a source file does not change its text. Newlines also separate adjacent
statements where they would otherwise form a single expression. A semicolon may
be written after any item as an explicit separator, including after the last item in a sequence.

A line indented further than the first token of the current statement
continues that statement instead of ending it. The statement is the innermost
item, block item, trait or implementation member, or extern binding:

```staple
let total = first
    + second
    + third
tail marker name alpha
    beta
```

Every continuation line is compared with the statement's first token, so
consecutive continuation lines may share one indentation. A line at or left of
that column ends the statement as before, and a block's statements are each
measured from their own first token. The same rule applies wherever a newline
would otherwise end an expression, type, pattern argument, pipe stage, or
macro argument run, so a trailing top-level `Sequence` macro parameter also
captures arguments on indented continuation lines. Indentation is measured in
characters; a tab in the indentation of a line whose column decides whether it
continues a statement, or of that statement's own line, is rejected, so indent
with spaces. The formatter indents continuation lines one level deeper than
their statement.

Line comments begin with `//` and continue to the end of the line. Block
comments are delimited by `/*` and `*/`, may span multiple lines, and nest, so
every `/*` inside a block comment must be balanced by a matching `*/`.

### Top-level statements

A source file may contain expressions alongside items, including bindings,
declarations, and trait implementations. 

The entry module's top-level statements are the program: `std.io.IO` is
implicitly available to them, so output can be produced directly, without
defining any function:

```staple
use std.io.println

println "Hello, world!"
```

`std.core.Reactive` is available the same way, but only when the top level
actually needs it: if any top-level statement requires `Reactive` — directly,
or transitively through a called function — the entry point implicitly opens
one `reactive_scope`, disposing it once the top-level statements finish, so
`reaction`, `signal`, and `snapshot` (see [Signals and
reactions](#signals-and-reactions)) may be used without a `with` block:

```staple
let signal count = 0
reaction { println "count: $count" }
count = 1
```

An entry module that never touches reactivity pays no cost for this: no
scope is created, and no `Reactive` value exists to be looked up.

Both implicit resources are scoped to the entry module alone; a non-entry
module's top-level statements still cannot require any resource. `main`
carries no special meaning — a `def main`, `let main`, or imported `main` is
an ordinary binding like any other, in every module. The native entry point
initializes every reachable module, in dependency order, and then returns
status zero.

### Modules and `use`

Every `.sta` file is a module. Modules are private by default. A file that is
intended to be imported by another module declares that at the start of the
file with a bare `pub mod`; a bare `mod` explicitly retains private visibility:

```staple
@doc("Public formatting utilities")
pub mod
```

The bare declaration describes the current module and may carry the same
modifier list as another declaration. It must precede ordinary module items and
may appear at most once. It is distinct from `pub mod api { ... }`, which
declares a public child named `api` in the current module.

Each package has a root module. The package manifest defaults it to `src/root.sta`; the file
is optional, but its directory still anchors dotted module paths. Paths are
resolved relative to that directory by replacing dots with path separators and
adding `.sta`:

```staple
use tools.format
// loads tools/format.sta
```

The standard library is a library package (`stdlib/staple.kdl`) that every
package implicitly depends on under the alias `std`; no `dependencies` entry is
needed, and a manifest may not bind `std` itself. Paths beginning with `std`
therefore resolve from the standard-library source root through the same
dependency-alias machinery as any local dependency. `std.core` is the small,
always-imported language interface: booleans, numbers, indexing protocols,
reactive primitives, ranges, flow macros (`if`, `when`, `while`,
`for`, and the rest of `std.flow`), and compiler modifiers. The separate
`std.prelude` interface is imported by default and provides strings,
`StringBuilder`, lists, iteration, formatting traits, defaults, and results. Lower-level
modules such as `std.shared`, `std.unique`, `std.buffer`, `std.shared_buffer`,
`std.io`, `std.syntax`, and `std.cinterop` require explicit imports.

`staple build` and `staple run` compile the standard library, like every other
non-root package, into its own cached object (see README.md, "Incremental
builds"). Installing the standard library prebuilds it into
`<stdlib>/target/<triple>/`; the cache is keyed on the compiler executable, so
it only serves the `staple` that built it. `--no-incremental` or
`STAPLE_INCREMENTAL=0` compiles one whole-program module instead.

A package disables its implicit prelude with `prelude #false` in `staple.kdl`.
The standard library does so itself: its modules see only `std.core` and
import everything else explicitly.
A file module may opt out independently:

```staple
@no_prelude
pub mod
```

Core names remain available. `use std.prelude.*` explicitly restores the
prelude regardless of either opt-out. A module-level opt-out does not propagate
to inline child modules.

Implicitly imported core and prelude items yield to the module's own items and
explicit imports. A module that defines or explicitly imports a name, in any
namespace, hides every implicit item of that name: after a local `type List`,
`List` no longer refers to the prelude's `List` type, constructor, or companion
namespace, so `List.new ()` is an error. An explicit import that names the same
item as a definition in the same module is an error.

Packages conditionally include items with the compiler-provided
`@feature` modifier declared by `std.core`:

```staple
@feature("networking")
@feature("tls")
pub def connect = ...
```

Each modifier takes one declared feature name, and repeated modifiers require
all names. Disabled items are removed before import discovery, macro expansion,
name resolution, and type checking. The unqualified modifier is reserved like
`@doc`; qualified user modifiers named `feature` remain ordinary modifiers.
Using `@feature` without a package manifest is an error.

The keyword `package` always selects the current package root, bypassing inline
children with the same name:

```staple
use package.models.User
```

Here `models` loads `src/models.sta` with the default root. Public items declared
by an existing root module are available directly as `package.item`. The entry
remains `src/main.sta` by default and is addressed by its relative module path,
so its public items are available as `package.main.item`. Custom entry paths use
the same relative mapping. A file named `package.sta` is addressed as `use
package.package`.

Packages may declare `kind "library"` and be used through local path
dependencies. A dependency declaration's key is a logical Staple root:

```kdl
package "application" {
    dependencies {
        geometry path="../geometry"
    }
}
```

`use geometry.shapes.Circle` resolves from the dependency's source root. The
alias may differ from its package name, and each dependency module's `package`
keyword continues to refer to that dependency rather than the consuming
application. Dependency aliases take precedence over same-named local modules.
Only public file modules and public items cross a package boundary. A library
may omit both its entry and its root file; an absent root behaves as an empty
package namespace while public file modules remain addressable below it.

Public items may also be used through a root-qualified name without a `use`
declaration:

```staple
std.io.println "Hello"
let user: package.models.User = package.models.default_user
```

The prefix before the item is resolved as a module path using the same
longest-file-prefix and public-inline-submodule rules as `use`. Such a reference
loads the target module and establishes an initialization dependency. Only
`package` and dependency aliases (including the implicit `std`) receive this
treatment; other dotted expressions remain namespace or product access. `use`
remains useful for short local names and is required for re-exporting.

Only modules reachable from the entry module through `use` declarations or
root-qualified references are compiled. A source file is loaded once even when
several modules refer to it, and mutually recursive module dependencies are
allowed.

A module can be brought into scope as a namespace. The namespace name is the
last component of its path:

```staple
use path.to.another_module
another_module.func ()
let value: another_module.MyType
```

Public items can instead be imported directly:

```staple
use path.to.another_module.*
use path.to.another_module.(func as my_func, MyType)
use path.to.another_module.func
use path.to.another_module.func as my_func
```

The wildcard form imports every public named item. The parenthesized form
imports only the listed items; any entry may carry an `as` clause to bind it
under a different local name. The trailing `as` form imports one item under a
different local name. Imports are hoisted and apply throughout their module.
Prefixing an item import with `pub` re-exports it from the importing module:

```staple
pub use path.to.another_module.(func, MyType)
pub use path.to.another_module.func as public_func
pub use path.to.another_module.*
```

Re-exports may be chained through multiple modules. Importing two items under
the same name, or colliding with a local declaration, is an error.

For a bare dotted `use`, the compiler resolves the full path as a module and
the final component as an item from its parent module. A type or trait can be
imported alongside a same-named module, as with the type and inline module
generated by `typegroup`. A same-named value, macro, or distinct module
namespace is ambiguous and must be renamed.

Modules may also be declared inline. An inline submodule has its own scope and
does not inherit names from its parent; parent items must be imported through
the relative `super` module:

```staple
let offset = 1

mod arithmetic {
    use super.offset
    pub def increment = value => value + offset
}
```

Inline submodules may be nested to any depth. Each leading `super` moves up one
level, so `use super.super.item` imports `item` from the grandparent. Using
`super` from a file module is an error. A parent can always use its direct child
as a namespace, but inline submodules are private outside their parent unless
declared with `pub mod`:

```staple
pub mod api {
    pub type Number = I32
    pub let answer: Number = 42
}
```

### Type companions

Named types may have companion items. A companion behaves as the type's
namespace, so its public members are selected through the type name:

```staple
type Animal wrap ...

companion Animal {
    pub def teleport = (animal: Animal; x: F32; y: F32) => Animal :: animal
}

let animal: Animal = ...
let hopped = Animal.teleport animal 1.0 2.0
```

Companion functions, visible trait functions, and functions in lexical scope
can also be called with pipe syntax. The receiver fills the function's first slot, and the stage's application chain
supplies the remaining slots, exactly as in a direct juxtaposed call:

```staple
let hopped = animal |> teleport 1.0 2.0
// Equivalent to Animal.teleport animal 1.0 2.0

let position: (F32, F32) = (1.0, 2.0)
let moved = animal |> teleport ...position
// Equivalent to Animal.teleport animal ...position
```

Slots keep their exact arity through `|>`. A bare `animal |> teleport`, or a
stage that supplies fewer arguments than the remaining slots, is an incomplete
call rather than a partial closure. A spread in a stage may fill only the slots
after the receiver.

A function that takes a single parameter, rather than a slotted product,
receives the receiver as that parameter. When the result is another function,
as in a curried companion, the stage's arguments apply to that result:

```staple
companion Animal {
    pub def move_to = animal: Animal => position: (F32, F32) => animal
}

let moved = animal |> move_to (1.0, 1.0)
// Equivalent to Animal.move_to animal (1.0, 1.0)
```

Unlike a slotted call, a bare `animal |> move_to` is complete: it is the
receiver-applied value, which here is a function still awaiting `position`.

Companion lookup uses the receiver's static named type. A companion belongs
to its target's identity rather than its name. Type aliases are transparent,
so `companion A` for `type A = B` is the same as `companion B`: its
members belong to `B` and are reachable as `B.member`, as `A.member` or
through any other alias of `B`, and by method syntax on any `B` value. An
alias of a structural type gives a structural companion, described below.

A companion block may be written in any module of the package that declares
its target. Like trait implementations, companions obey a package-level
orphan rule: a companion of a type declared in another package is rejected,
whether the block names the type directly or through an alias, so a
dependency's types cannot gain competing members from several packages.

```staple
// units.sta
pub type Meters wrap pub I32

// conversions.sta, in the same package
use package.units.Meters

companion Meters {
    pub def to_feet = value: Meters => I32 :: value.* * 3
}

// anywhere `Meters` and the member are visible
let feet = Meters 2 |> to_feet           // Meters.to_feet (Meters 2)
```

The builtin numeric types have companions too, so their per-type helpers need
no import. `F32` and `F64` provide the standard floating-point toolbox:

```staple
let cell = (x / size) |> floor        // F64.floor x
let n = x |> round |> try_convert.<_, I64>
let hypotenuse = (a * a + b * b) |> sqrt
```

`floor`, `ceil`, `trunc`, `round` (ties away from zero), `round_even` (ties to
even), `fract`, `abs`, `sqrt`, `copysign`, `min`, `max`, `clamp`, and
`mul_add` are exact and backed by LLVM intrinsics. `min` and `max` ignore a
NaN operand; `clamp` panics at its caller when `lo > hi` or a bound is NaN. `is_nan`,
`is_infinite`, `is_finite`, and `is_sign_negative` classify a value.
`remainder_euclid` and `divide_euclid` provide Euclidean division over `%`.
`to_bits`/`from_bits` expose the exact bit pattern (`F32.to_bits: F32 -> U32`,
`F64.to_bits: F64 -> U64`), and the constants `Epsilon`, `Min`, `Max`,
`MinPositive`, `Pi`, `E`, `NaN`, `Infinity`, and `NegInfinity` are companion
items. Rounding returns the float type; converting to an integer is the
separate checked conversion.

The integer types have companions with the same pipe syntax. `I8`, `I16`,
`I32`, `I64`, `U8`, `U16`, `U32`, and `U64` provide the constants `Min`,
`Max`, and `Bits`; `ISize` and `USize` do not yet provide those three constants
because their width is only known during code generation. Every companion
provides `min`, `max`, `clamp`, `abs_diff`, checked, wrapping, saturating, and
overflowing arithmetic, Euclidean division, powers and logarithms, integer
square root, bit counts and rotations, `swap_bytes`, and `reverse_bits`.
Signed companions add `abs`, `unsigned_abs`, negation helpers, `signum`, and
sign tests. Unsigned companions add power-of-two helpers, `divide_ceil`, and
`checked_add_signed`.

The unsigned companions also provide `cast_signed`, which reinterprets the bits
as the signed type of the same width (`U8` to `I8`, ..., `USize` to `ISize`);
the signed companions provide the reverse as `cast_unsigned`. These are not
range-checked conversions: `U32.cast_signed 0xFFFF_FFFF` is `-1`, and
`I8.cast_unsigned I8.Min` is `128`. LLVM integers have no signedness and both
types have the same width, so a sign cast compiles to no instruction.

`checked_*` returns `Option<T>`, and `overflowing_*` returns `(T, Bool)`, the
wrapped result and whether it overflowed. Counts and exponents are `U32`, as
is `Bits` where a companion defines it. `abs` wraps at the minimum value, just
like prefix `-`;
`checked_abs` returns `None` there, while `unsigned_abs` returns the exact
magnitude in the unsigned type of the same width. `clamp`, Euclidean division,
`ilog2`, `ilog10`, `isqrt`, and `divide_ceil` panic at their caller on invalid
input, matching how integer `/` and `%` report a zero divisor.

Pipes associate left and bind below every binary operator. Each stage groups
its ordinary application arguments before the next pipe:

```staple
value |> method_a 1 |> method_b 2
// Equivalent to method_b (method_a value 1) 2, with receiver-based lookup.
a + b |> method
value |> method (1 + 2)
```

Operator expressions in stage arguments require parentheses; `value |> method
1 + 2` is rejected. Parenthesized stages such as `value |> (method_a 1)` are
also accepted. Existing newline rules still separate statements.

A bare stage name has up to three kinds of candidates: an accessible
companion function of the receiver's static named type, the value the name
denotes in lexical scope (a local function, closure, parameter, module item,
or import), and the trait functions of that name visible in scope. A
candidate is compatible when the receiver and supplied arguments check
against it, with trait functions using existing implementations and generic
bounds; a lexical value that is not a function never competes. Trait methods
also work for structural receivers.

A compatible companion function takes priority over trait functions, and
multiple compatible trait functions are ambiguous. A compatible lexical
function is ambiguous with a compatible companion or trait function of the
same name, rather than either one silently winning:

```staple
companion Animal { pub def legs = animal: Animal => 4 }

def count = (animal: Animal; legs: Animal -> I32) => {
    animal |> legs            // error: ambiguous pipe stage `legs`
    animal |> Animal.legs     // the companion function
    legs animal               // the local function
}
```

Select a companion with `value |> Animal.teleport 1.0 2.0` and a trait
function with `value |> Trait.method arguments`; call a shadowed lexical
function directly, or bind it to another name. The type of a lexical
candidate must already be known where the stage is checked. Arbitrary
callable expressions are not pipe stages. The receiver only ever fills the
first slot; it is never combined with the stage's arguments into a product.

More than one companion block may contribute to the same type, from any
modules of its package, and no block is special: the members of all of them,
values, types and macros alike, form the type's one namespace, reached as
`Type.member` wherever the type is visible, and a member name may be declared
by only one block. Companion blocks have no visibility of their own;
visibility is declared on their items, and a member without `pub` is
reachable only from the module its block is written in. A block's body sees everything the
module it is written in sees, including that module's imports, and its own
members. It does not see the members of the type's other blocks directly; it
names them as `Type.member` or calls them with method syntax, like any other
module. A `use` inside a block applies to that block only: it reaches neither
the module the block is written in nor the type's other blocks, and it may
import a name that module already imports.
Generic and constrained targets use the same bracketed parameter syntax as
trait implementations, for example `companion<T where Bound T> Box<T>`.
Ordinary `mod` declarations may not share a name with a type; use `companion`
when defining a type-qualified namespace.

#### Blanket companions

A companion whose target is one of its own type parameters is a *blanket
companion*. Its members belong to every type that satisfies its bounds:

```staple
pub trait Measure T { measure: T -> USize }

companion<T where Measure T> T {
    pub def double_size = value: T => USize :: Measure.measure value * 2
}

type Meters wrap I32
impl Measure Meters { measure = value => 4 }

let piped = Meters 1 |> double_size          // 8
let named = Meters.double_size (Meters 1)    // 8
```

A blanket member is a pipe-stage candidate for any receiver, named or
structural, including a value whose type is a bounded type parameter. It
takes the place of the receiver type's own companion member, which the
overlap rule below keeps from existing, so it has the same priority over
trait functions. `Type.member` selects it for a named type that satisfies
its bounds. The blanket parameter is then that type, so `Meters.double_size 3`
is an error rather than a use at `I32`. For a generic type, `Box.member`
stands for `Box<A>` at every `A`. A blanket member is not reachable as
`T.member`, because the block declares no namespace.

Like trait implementations, blanket companions apply throughout the loaded
program, so these rules keep them coherent:

- The block needs at least one trait bound on its parameter.
- The orphan rule: one of those traits must be declared in the companion's
  package. Otherwise a dependency's types could gain competing members from
  several packages. Adding blanket members to every `std.iterator.Iterator`
  is therefore reserved to the standard library. A package can still bound the
  parameter by its own trait as well.
- Two blanket companions may declare the same member name only when no type
  could satisfy both of their bounds. For each bound naming the parameter
  directly, the types its trait's implementations cover are compared: the
  members are disjoint when one pair of bounds covers no common type. The
  standard library's `unwrap` for `Try` values therefore leaves room for a
  package's own `unwrap` over a trait its `Try` types do not implement. A
  receiver whose type is a parameter bounded by both traits makes the pipe
  stage ambiguous, and `Type.member` picks the member whose bounds `Type`
  satisfies.
- A type's own companion member may not share a name with a blanket member
  whose bounds the type could satisfy. This is checked once every
  implementation is known, so it covers an implementation declared after the
  companion. For a generic type, it is an error if any implementation could
  cover it.

Visibility works as for other companion items: a member without `pub` is
reachable only from the module that declares the block.

#### Structural companions

A companion's target may be a structural type, such as a sum or product,
written directly or through an alias. The standard library could give every
`Result` a member this way; `companion<T, E> Result<T, E>` is the same block,
since `Result<T, E>` is an alias of `Ok<T> | E`:

```staple
companion<T, E> (Ok<T> | E) {
    pub def succeeded = value: Ok<T> | E => Bool :: match value {
        Ok _ => True,
        _ => False,
    }
}
```

A structural companion belongs to its target's identity. Its members apply,
by method syntax, to a receiver whose static type is the target, comparing
sum alternatives as an unordered set after aliases are expanded, and taking
priority over trait functions as a type's own members do. A wider sum is a
different type: a companion of `Ok<I32> | String` does not apply to
`Ok<I32> | String | Bool`. The target has no namespace of its own, so outside
method syntax its members are reached through an alias of a matching type,
from any package: after `type Answer = Ok<I32> | String`,
`Answer.succeeded` is the member at `Ok<I32> | String`.

The companion's own parameters match any type. A bare parameter alternative
of a sum stands for the rest of the receiver's alternatives, of which there
must be at least one: `Ok<T> | E` matches `Ok<I32> | String` with `E` as
`String`, and `Ok<I32> | String | Bool` with `E` as `String | Bool`, but not
`Ok<I32>` alone. A sum in a target may have at most one bare parameter
alternative. A target that matches a receiver in more than one way, as
`Ok<T> | E` does `Ok<I32> | Ok<U8>`, makes its members ambiguous for that
receiver.

These rules keep structural companions coherent:

- The orphan rule: one of the nominal types named in the target, looking
  through aliases, must be declared in the companion's package. Thus only the
  standard library may write `companion (Ok<I32> | String)`, while a package
  may write `companion (Ok<I32> | Meters)` for its own `Meters`.
- Two structural members may share a name only when no type could match both
  targets, and a structural member may not share a name with a blanket member
  whose bounds its target could satisfy.

An external import resolves the longest existing `.sta` file prefix and then
traverses public inline submodules. Thus `use library.api.answer` first prefers
`library/api.sta`; if that file does not exist, it loads `library.sta` and looks
for a public `api` submodule.

A parent can re-export public items from a child, including a private child:

```staple
mod implementation {
    pub def format = value => value
    pub type Number = I32
}

pub use implementation.format
pub use implementation.*
```

Selected, renamed, glob, and chained re-exports preserve values, types,
constructors, traits, macros, public inline-module namespaces, and operator
fixities. A public `use` cannot re-export a private item.

Top-level declarations are private by default. `pub` exports a binding or type:

```staple
pub def format = (value: I32) => value
pub type Number = I32
```

`pub extern` exports every binding declared by that external block. A binding
inside a private extern block may use `pub` or `pub(package)` to expose that
binding individually, allowing private helpers and public intrinsics in one block.

Every reachable module's top-level statements execute exactly once. Dependencies
are initialized before modules which use them. Mutually recursive groups are
initialized by canonical file path and then logical submodule path, and
statements within one module keep source order. Declaring an inline submodule
makes it reachable, so its top-level statements also execute exactly once.
The entry module is always initialized last, after every module it depends
on, so its top-level statements effectively run as the program body.
Module globals begin in the `Declared` state, enter `Initializing`
while their initializer is evaluated, and become `Initialized(value)` only after
that value has been stored. Reading a global before it is `Initialized` is an
initialization error; globals never expose a default representation.

## Bindings

staple has two binding keywords: `let` and `def`. `mut` is a modifier on
`let` that governs both whether the binding may be reassigned and whether the
value already held by the binding may be written into.
A later `let` may shadow an earlier one of the same name in the same scope:

```staple
let x = 1
let x = x + 1       // shadows the first `x`; refers to it while initializing
let mut x = x * 2   // shadows again; `x` is now mutable
```

Shadowing has two exceptions. First, `def` bindings are hoisted (see below)
and so participate in neither direction of shadowing: a `def` can never
coexist with another binding of the same name in its scope, regardless of
declaration order. Second, two `pub` bindings of the same name in the same
scope remain a "duplicate definition" error, since a public name is an
export and two of them would leave it ambiguous what an importer sees; a
`pub` binding and a private binding of the same name may still shadow each
other in either direction. A single pattern also may not bind the same name
twice — `let (a, a) = (1, 2)` is rejected, since there is no earlier/later
between two names introduced by one pattern.

### `let`

`let` declares or defines a value. The binding itself cannot be reassigned,
and its value cannot be written into:

```staple
let answer = 42
answer = 43 // error: `answer` is not declared `mut`
```

A type may be written after the binding name:

```staple
let answer: I32 = 42
```

### `mut`

`mut` marks a binding that may both be reassigned and have its value written
into. Reassignment is an item whose right-hand side must have the type of
the destination; writing into fields of a by-value product, or through a
`Shared`, requires the same `mut` root:

```staple
let mut counter = 0
counter = counter + 1

let mut point = (x: 3, y: 4)
point.x = 5

let mut cell: Shared<I32> = Shared 1
cell.0 = 2

let borrowed: Shared<I32> = Shared 3
borrowed.0 = 4 // error: `borrowed` is not declared `mut`
```

`mut` may mark individual names in any binding pattern, including match arms
— each destructured name that needs to be mutable carries its own `mut`;
there is no form that marks every name in a destructuring pattern at once:

```staple
let (mut left, mut right) = (1, 2)
left = right
```

A `mut` binding must have an initializer.

Bracket assignment is delegated to `MutateIndex`, whose `Target` parameter
carries mutable parameter permission (see the "Mutable parameters" subsection
under "Functions"); `a[i] = v` therefore also requires `a`'s root binding to be
declared `mut`. Bindings captured by functions are shared cells whenever they
are declared `mut`, so the defining scope and all closures observe subsequent
assignments. A public `mut` module binding remains reassignable and writable
only from its declaring module.

A `mut` marker on a function parameter is a different thing from `mut` on a
`let` binding: it declares mutation permission on that parameter position (see
the "Mutable parameters" subsection under "Functions") rather than an ordinary
mutable local, and it is allowed only on a whole parameter binding or a
direct element of the top-level parameter product — never on a pattern
nested any deeper, and never combined with another binding-pattern modifier.
Without a marker (or a matching `mut` parameter in the function's type), a
parameter can neither be written into nor reassigned.
With one, both are available, and because the marked position is passed by
address, the caller observes the change too. A function that instead wants a
private local seeded from a parameter's initial value — one whose mutations
stay invisible to the caller and needs no mutable parameter declaration — should
shadow it with a `let mut` binding in the body:

```staple
def increment = (value: I32) => {
    let mut value = value
    value = value + 1
    value
}
```

A function declaration may omit its value when its complete function type is
given. Whether other ordinary, non-external `let` declarations may omit their
value is currently unspecified.

### `def`

`def` defines a hoisted value. The name is available throughout its containing
scope, rather than becoming available only after the definition.

```staple
def greet = () => {
    printf (c_string "hello, world!\n")
}
```

`def` is not a function-declaration keyword. It can bind any value. In the
example above, the value assigned to `greet` happens to be a function value.

A hoisted binding begins `Declared`, becomes `Initializing` while its initializer
is evaluated, and then becomes `Initialized(value)`. Evaluating its value in
either of the first two states is an initialization error. The compiler reports
direct, source-order violations statically and emits a runtime check when the
evaluation time cannot be established statically.

Creating a function closure captures references for later use; it does not
evaluate the captured values. Recursive and mutually recursive closures are
therefore legal:

```staple
def first = () => second ()
def second = () => first ()
```

Calling such a closure before all values it needs have been initialized still
produces an initialization error. Function-valued bindings otherwise follow the
same initialization rules as bindings of every other value type.

### `const`

`const` declares a named compile-time value. A use of the name behaves as the
value itself, rather than as a read from runtime storage:

```staple
const x: I32 = 1 + 3
```

The initializer is evaluated by the compiler, and may call ordinary functions,
including recursively:

```staple
def fibonacci = n: I32 => I32 ::
    match n < 2 {
        True() => n,
        False() => fibonacci (n - 1) + fibonacci (n - 2),
    }

const y = fibonacci 10
```

An initializer must fold to an integer, a float, a string, or a product
(including a nested product) of those; a value that cannot be represented this
way — a function, a resource, and so on — is a compile-time error. Floats fold
under IEEE rules, including non-finite results: `const x: F64 = 1.0 / 0.0` is
positive infinity and `const y: F64 = 0.0 / 0.0` is NaN. A float *literal*
must still be finite; only computed values may be non-finite. A folded float
keeps the float type it was computed at, so `const pi = F32.Pi` is an `F32`,
and mixing `F32` and `F64` while folding is an error. A typed float that no
literal can spell, such as an unannotated `F32` constant or a non-finite one,
is represented exactly by its bit pattern, so NaN payload bits survive
folding. A folded NaN has the same bits whichever machine compiles it: an
operation on non-NaN operands, such as `0.0 / 0.0` or `F64.sqrt (-1.0)`,
produces the positive quiet NaN (`0x7FF8000000000000`, or `0x7FC00000` for
`F32`), and a NaN operand propagates with its sign and payload, quieted. `abs`
and `copysign` change only the sign bit, even of a NaN. A NaN the program
computes at run time is whatever the target's hardware produces. A folded
integer likewise keeps the integer type a suffix or a typed operand gave it.
A suffixed literal folds to a typed value, arithmetic over it keeps that type,
and mixing two different integer types while folding is an error, the same as
mixing `F32` and `F64`. An untyped folded integer reifies as a plain decimal
literal and takes its type from context as before; a typed one reifies with its
suffix, so `const Small = 5u8` is a `U8` constant. `const` requires an initializer, and, like a compile-time
helper's evaluation generally, an initializer that recurses without bound
(including through a self-referential `const`) is rejected rather than left to
hang or exhaust memory.

Like `def`, `const` is hoisted: its name is available throughout its
containing scope regardless of declaration order. Unlike `def`, it has no
`Declared`/`Initializing`/`Initialized` runtime state, since its value is
already fully known before type checking begins — there is no initialization
order for a compiler-computed constant to violate.

`const` cannot be marked `mut`, and does not accept compile-time (`<...>`)
parameters.

### Binding type annotations

The optional type annotation on a binding describes the complete type of its
value:

```staple
def get_number: _ -> I32 = () => {
    42
}
```

`_` is an inferred type placeholder. In this example, the compiler infers the
function's parameter type from `()` while requiring its result to be `I32`.
Inferred placeholders may appear wherever a type is expected.

## Values and expressions

staple is expression-oriented. Function values, function applications, products,
and blocks are all expressions.

The syntax currently recognized for literal values includes strings, characters, integers,
and floating-point numbers:

```staple
"hello"
42
0xFF
0b1010
0o755
1_000_000
1.5
.5
1e3
3u8
0x7F_i8
1.5f32
7f64
```

Integer literals are decimal unless they start with `0x` (hexadecimal), `0b`
(binary), or `0o` (octal). The prefixes are lowercase; `0X`, `0B`, and `0O` are
errors. A leading zero does not make a literal octal: `0755` is seven hundred
fifty-five, and `0o755` is the octal spelling. A single `_` may separate digits,
as in `1_000_000`, `0xFFFF_FFFF`, and `0b1010_0101`; it is valid only between
digits (or immediately before a type suffix) and carries no meaning. A literal
cannot begin with `_`.

A lowercase type suffix fixes the literal's type: `42i8`, `3u8`, `0x7F_i8`,
and `1usize` are integers of the named type, while `0.5f32`, `1e3f64`, and
`7f64` are floats. A suffixed literal does not take its type from context, so
`let x: I32 = 3u8` is a type mismatch. Uppercase suffixes such as `42U8` are
errors. A separator
cannot sit next to a `.` or an exponent marker; `1_.5` and `1.5_e3` are
malformed. An integer suffix on a float literal (`1.5u8`) and a float suffix on
a binary or octal literal (`0b1f32`, `0o7f64`) are errors. On a hexadecimal
literal a trailing `f32` or `f64` is read as hex digits, so `0x1f32` and
`0x1_f32` are the integer `0x1F32`.

Float literals accept decimal points and scientific notation. They use an
expected `F32` or `F64` type when available and otherwise default to `F64`; an
`f32`/`f64` suffix overrides the expected type just as an integer suffix does.
`1.` is a float literal, while `1.field` remains member access. Integer literals
remain integer values; Staple does not implicitly convert between numeric types.

A prefixed or separated integer literal denotes the same non-negative number as
its decimal spelling and must fit its target type by the same magnitude rule as
any other literal. Reading a literal as a bit pattern for a signed type is not
implicit: `0xFFFFFFFF` is a valid `U32` but not a valid `I32`, and `0x80i8` is
an error. Prefix `-` is not part of a literal, so `-128i8` is also an error:
`128i8` is range-checked before the negation applies. Use `I8.Min` for that
value, or reinterpret bits deliberately with `U32.cast_signed` (the unsigned
companions' `cast_signed`, or `I8.cast_unsigned` in the other direction); the
pipe form is `0xFFFF_FFFFu32 |> cast_signed`.

String literals use double quotes and produce owned UTF-8 `String` values. A
backslash protects the following quote from ending the string. Supported
escapes are `\\n`, `\\r`, `\\t`, `\\0`, `\\\\`, `\\"`, `\\'`, `\\$`, and `\\u{…}`.
Unicode escapes contain 1 to 6 hex digits and must name a Unicode scalar value.

An unescaped dollar sign introduces interpolation. `$name` displays a single
identifier, while `${expression}` displays any expression. Appending `:?`
before the closing brace selects developer-facing `Debug` formatting instead:

```staple
let name = "Ada"
let point = (x: 3, y: 4)
let message = "hello $name; point = ${point:?}; total = ${1 + 2}; \$5"
```

Interpolations are evaluated exactly once, from left to right. Ordinary
interpolation requires the value's type to implement `Display`; `:?` requires
`Debug`. A literal dollar sign must be written `\$`. Interpolation is an
expression feature and is rejected where the grammar requires a literal
string, such as string-literal types and patterns.

### Character literals

Single quotes enclose exactly one Unicode scalar value and produce `Char`:

```staple
let initial = 'S'
let tab = '\t'
let snowman = '\u{2603}'
```

`Char` represents code points through U+10FFFF, excluding the surrogates
U+D800 through U+DFFF. It is not a grapheme: `e` followed by U+0301 is two
Chars, even when displayed as one character. Empty and multi-scalar literals
are errors. Character literals support `\n`, `\r`, `\t`, `\0`, `\\`,
`\'`, `\"`, and `\u{…}`; `$` is ordinary text and has no interpolation.

`Char.from_u32` returns `Option<Char>`; `Char.to_u32`, `Char.len_utf8`, and
`Char.is_ascii` inspect a scalar. `Convert Char U32` and `TryConvert U32 Char`
provide checked conversion. `Display` writes UTF-8; `Debug` writes a quoted,
escaped literal.

`String.chars` iterates scalar values without copying the bytes. Collecting
Chars produces a String; `String.char_count` counts scalars in O(n) time.
Use `String.bytes` for UTF-8 bytes.

The `c_string` library macro from `std.cinterop` accepts only a string literal
and produces an owned `CString` backed by allocated NUL-terminated storage:

```staple
use std.cinterop.(c_string, CString)

def message = () => c_string "hello"
```

Macros are module items and support the same namespace, glob, selected, and
renamed import forms as values and types. Public macros retain their definition
environment, including private helpers used by generated syntax.

Operators are fixed grammar with fixed precedence and associativity; there is
no user-definable operator or infix-function syntax. Each operator desugars
directly to a call against a standard library trait. The traits in
`std.ops` (`Add`, `Subtract`, `Multiply`, `Divide`, `Remainder`, `Neg`, and
`Not`) and
`std.cmp` (`Eq`, `PartialOrd`, and `Ord`) and their methods require explicit
imports; operator syntax works without
these imports. Every name an operator desugars to resolves at its
standard-library definition, so local items or imports named `Convert`, `Eq`,
`Try`, `Ok`, `range`, and so on never change what an operator means:

| Operator(s) | Precedence | Associativity | Desugars to |
|---|---|---|---|
| `?` (postfix) | 9 | — | `match (Try.branch operand) { Ok output => output, residual => return residual }` |
| `-`, `!` (prefix) | 8 | — | `Neg.negate`, `Not.not` |
| `as` | 7.5 | left | `Convert.convert.<_, Type> expression` |
| `*`, `/`, `%` | 7 | left | `Multiply.multiply`, `Divide.divide`, `Remainder.remainder` |
| `+`, `-` | 6 | left | `Add.add`, `Subtract.subtract` |
| `==`, `!=`, `<`, `<=`, `>`, `>=` | 4 | none | `Eq.eq`/`Eq.ne`, `PartialOrd.lt`/`le`/`gt`/`ge` |
| `..`, `..=` | 3 | none | the prelude's `range`/`range_inclusive` functions |
| `&&` | 2 | left | not a call; see below |
| `\|\|` | 1 | left | not a call; see below |
| `\|>` | 0 | left | receiver-first function, companion, or trait call |

Function application and access bind tighter than every operator above, and
postfix `?` (see [Propagating with `?`](#propagating-with-)) ends the call or pipeline stage
on its left.
Chaining two non-associative operators at the same precedence (for example
`1 == 2 == 3`) is a parse error; parenthesize to disambiguate. The standard
integer implementations of `Add`, `Subtract`, `Multiply`, `Divide`,
`Remainder`, `Eq`, and `PartialOrd` are backed by private compiler
intrinsics. `+`, `-`, `*`, and prefix `-` wrap on overflow; the integer
companions provide checked, saturating, and overflowing alternatives when
overflow must be observed. `%` is truncated remainder: its result takes the
sign of the dividend, so `-7 % 3` is `-1` and `a == (a / b) * b + a % b` holds
whenever both sides are defined. `Divide.divide` and `Remainder.remainder` are
`@track_caller` members: integer division or remainder by zero, and signed
`MIN / -1`, [panic](#track_caller-and-location) at the operator, while
`MIN % -1` is `0`. `..`/`..=` are not
trait-based: they call the prelude's `range`/`range_inclusive` functions
directly, which construct `Range<T>`/`RangeInclusive<T>` values.

The prefix operators `-` and `!` bind tighter than every binary operator but
looser than application and access, so `-a.b` negates `a.b` and `-a * b` is
`(-a) * b`; a call or larger operand must be parenthesized (`-(f x)`). They
stack (`- -x`, `!!x`). `-` desugars to `Neg.negate` and `!` to `Not.not`, both
prefix trait calls of the form `Trait.method operand`. `Neg` is implemented for
the signed integer types and `F32`/`F64` (in terms of `Subtract`, so unsigned
integers have no `Neg`); `Not` is implemented for `Bool`.

`&&` and `||` are boolean and/or. Unlike every other operator, they are not
backed by a trait and cannot be overloaded: both operands and the result are
always `Bool`. They short-circuit — `right` is only evaluated when its value
is needed to determine the result, so side effects in `right` do not happen
otherwise:

```staple
def positive_and_small = n: I32 => n > 0 && n < 10
```

`a && b` evaluates `a`; if it is `False`, the result is `False` without
evaluating `b`, otherwise the result is `b`. `a || b` evaluates `a`; if it is
`True`, the result is `True` without evaluating `b`, otherwise the result is
`b`. Both operators are left-associative and may be chained or mixed freely
(`&&` binds tighter than `||`), unlike the non-associative comparison and
range operators.

### Products

Products are staple's single fixed-size aggregate form. They replace tuples,
structs, records, and fixed-size arrays found in other languages. A product has
an ordered, fixed number of elements, and its elements may have different types.

Parentheses construct a product:

```staple
()          // a nullary product
(value)     // equivalent to value
(a, b)      // a product containing two values
```

A non-variadic product with exactly one element is definitionally
equal to that element: `(T) = T` and `(value) = value`. The parser preserves
the parentheses, while type checking and code generation normalize the
singleton product away. A name on a singleton product type does not create a
distinct wrapper type.

#### Slotted products

Inside parentheses, `;` separates slots and `,` separates the elements
within each slot. Semicolons bind looser than commas: `(A; B, C; D)` has
three slots, with a plain product `(B, C)` as its second slot. A singleton
slot retains its name; a multi-element slot is unnamed. To name it, write
`(a: A; p: (b: B, c: C); d: D)`.

The same syntax works in types, expressions, and patterns:

```staple
let p: (I32; I32, I32; I32) = (1; 2, 3; 4)
let (a; b, c; d) = p
```

A trailing `;` is formatting only. `(A;)` and `(A)` normalize to `A`,
including dropping a singleton slot's name. `()` is unit; there is no
zero-slot product. Slotted products cannot be variadic.

A slotted product is a regular value type and may occur in bindings,
fields, lists, results, and type arguments. It has the same layout and
structural traits as its plain counterpart, but is a distinct type:
`(A; B)` never unifies with `(A, B)`. Access by name or index selects a slot
(`p.x`, `p.0`). Slotted patterns destructure in bindings, matches, and
function parameters; a binding pattern binds the whole value.

Spreads accept either fixed product kind. The target's separator determines
the result: `(...p)` constructs a plain product, and `(...q;)` constructs a
slotted product. The same rule applies to type spreads such as
`(...Inputs; String)`. There is no implicit conversion between the kinds.

Slot `mut` and `move` markers belong to the function type. They are allowed
only on direct slots of its parameter, never on a slotted value type
elsewhere. Mark individual slots rather than the whole slotted parameter.
Slot defaults are not allowed; a slot may contain a `from` type whose
representation declares defaults.

#### Named elements

Every product element may optionally have a name. Names are written before the
element type in a product type:

```staple
let args: (name: String, I32)
```

Here, `args` is a two-element product. Its first element is named `name` and has
type `String`; its second element is unnamed and has type `I32`.

Names may likewise be supplied when constructing a product value:

```staple
let args = (name: "staple", 1)
```

Named and unnamed elements can be mixed in the same product. Names in a fixed
product type must be unique, including names contributed by a product spread.

An ordinary named value element remains positional. When an expected product
type is available, an explicitly supplied value name must match the expected
name at the same position. A value may omit an expected name, but it cannot
supply a different name or attach a name to an unnamed expected position:

```staple
let point: (x: I32, y: I32) = (x: 10, 20) // valid
let point: (x: I32, y: I32) = (y: 10, x: 20) // error
let pair: (I32, y: I32) = (x: 10, 20) // error
```

#### Designated initializers

A leading dot makes a product value element a contextual, name-directed
initializer. Designated initializers may follow a positional prefix and may be
written in any order:

```staple
let value: (I32, I32, a: I32, b: I32) =
    (1, 2, .b: 4, .a: 3)
// Equivalent to (1, 2, a: 3, b: 4)
```

The expected fixed product shape and its field names must already be known;
the individual element types may still contain inference variables. Each
`.name:` selects the uniquely named position in that expected product. A
designator cannot name an unknown field or leave any expected position
uninitialized. A designator may name a position already filled by the
positional prefix or an earlier designator; the later value wins, matching the
later-field-wins merge semantics of a `...=` named spread. Positional elements
and positional spreads cannot appear after the first designator. Initializer
expressions are all evaluated once in source order even though designated
values are placed in product order.

Ordinary `name: value` syntax never performs designated placement. Designated
initializers also do not combine with a `...=` named spread. Named spreads keep
their separate merge semantics described below, including later-field
overrides.

#### Default field values

A named field may provide a default expression with `=`, but only in the
product that is the representation of a named [`wrap`](#wrap) or
[`from`](#from) type declaration, or in a product nested in that
representation's fields:

```staple
type Point from (x: I32 = 0, y: I32 = 0)
type TextArgs from (String, x: I32 = 0, y: I32 = 0)
type Options wrap (verbose: Bool = False, depth: I32 = 1)

type Config from (window: (width: I32 = 800, height: I32 = 600), title: String = "app")

let origin: Point = ()
let config: Config = (window: (.height: 900))
def text = args: TextArgs => () :: ()

text ("Hello")          // equivalent to text ("Hello", 0, 0)
text ("Hello", .y: 10) // equivalent to text ("Hello", 0, 10)
let options = Options (.depth: 3)
```

Defaults are rejected everywhere else: on anonymous product types such as
`let point: (x: I32 = 0, y: I32 = 0) = ()`, on function parameter types, on
aliases, and on products inside a representation's function types or type
arguments, such as `(x: I32 = 0, y: I32) -> I32` or `List<(x: I32 = 0)>`. A
nested product field without a default of its own must still be written, as
`window: ()` when every nested field defaults. To give a function optional
arguments, accept a `from` type whose representation declares them, as `text`
does above.

A product written where the named type is expected is checked against its
representation: for a `from` type through the implicit conversion, and for a
`wrap` type as the constructor's argument. Positional construction may omit
only a trailing suffix for which every field has a default. Designated
initializers may leave any defaulted field absent; every non-defaulted field
must still be initialized. `()` is therefore valid when every field defaults.
Explicit expressions are evaluated once in source order, followed by the
defaults of absent fields in product-field order. A default is not evaluated
when an explicit initializer supplies that field.

Defaults are construction metadata of the named type, not part of the
representation's structural type identity or runtime representation; a
constructed value always holds every field.

The `@derive_default` modifier from `std.default` implements `Default` for a
`wrap` or `from` type using these defaults: each field with a default uses it,
a nested product with defaults of its own is built field by field, and every
other field uses `Default.default ()`:

```staple
@derive_default
type Config from (window: (width: I32 = 800, height: I32), title: String = "app")

let config: Config = default () // ((800, 0), "app")
```

Defaults must be pure and portable: they may use literals, global names, and
compile-time parameters, but may not capture local runtime values or refer to
sibling fields. Unnamed fields, variadic markers, repeated or spread elements,
and singleton products that normalize to their element type cannot declare a
default. A named spread (`...=`) retains its complete-field requirement and
does not fill absent fields from defaults.

#### Element access

Every element can be accessed by its zero-based index. A named element can also
be accessed by name:

```staple
let args: (name: String, I32)

args.name // the first element, accessed by name
args.0    // the first element, accessed by index
args.1    // the second element, accessed by index
```

Name and index access refer to the same underlying elements; names do not
change the order or size of a product. A product's shape is always known at
compile time, so accessing an absent name or an index outside the product's
fixed bounds is always rejected at compile time, never deferred to a runtime
check.

A homogeneous product type — one whose elements all have the same type — is
also called an *array type*, and is written with repetition syntax `(T ^ N)`:

```staple
let zeros: (I32 ^ 3) = (0, 0, 0)
```

`(T ^ N)` is exactly equivalent to a product containing `N` copies of `T`,
rather than a distinct array type. In particular, `(T ^ 0)` is `()` and `(T ^ 1)`
is `T`. One repetition may have at most 4,294,967,295 elements, and a product
must fit the target's maximum object size. A function's parameter may have at
most 65,535 positions, since each position is passed as its own argument: a
function whose whole parameter is `(T ^ N)` takes `N` arguments. Array values
are stored like any other local value, so a very large array can exhaust the
stack. The length is mandatory: there is no unsized array spelling.

Repeated products are stored as runs, so `(I32, I32, I32)` and `(I32 ^ 3)` are
the same normal form. Types print in that normal form: a diagnostic shows
`(I32 ^ 3)`, not three separate elements.

The length is a compile-time type. Non-negative integer type literals such as
`0`, `3`, `0x10`, and `1_024` are singleton subtypes of `USize` and
automatically satisfy the sealed prelude trait `Natural`. A literal type accepts
any radix and digit separators, but never a type suffix, since it has no
runtime type of its own: `(U8 ^ 1_024)` is valid and `(U8 ^ 3u8)` is not. An
alias or a parameter constrained by that trait may be used as a length, for
example `type Vector<T, N where Natural N> = (T ^ N)`. The length must resolve
to one singleton literal before code generation. Number literal types use the
`USize` runtime representation, so an exact value annotation such as
`let three: 3 = 3` is valid and widens to `USize`; an unconstrained
`let three = 3` still infers `I32`.

An array *value* whose elements are all the same may be written with the
repetition form `(value ^ count)`:

```staple
let zeros: (I32 ^ 3) = (0 ^ 3) // (0, 0, 0)
```

`(value ^ count)` is equivalent to a product with `count` copies of `value`;
`(value ^ 0)` is `()` and `(value ^ 1)` is `value`. The count uses the same
grammar and rules as an array type's length: it is a compile-time type, not a
value. A non-negative
integer type literal, a type alias, or a compile-time parameter constrained by
`Natural` may be used, for example:

```staple
type Count = 3
let cells: (I32 ^ Count) = (0 ^ Count)

def repeat = <T, N where Copy T, Natural N> value: T => (T ^ N) :: (value ^ N)
```

A runtime value is never a valid count, even when its type is a singleton: with
`let n: 3 = 3`, `(0 ^ n)` is rejected. `value` is evaluated exactly once; when
`count` is not known to be `1` its type must be `Copy`, since the one value is
copied into every position. The `^` form takes exactly one value: an element
name, a `...` spread, or a `.name:` designator before the `^` is a syntax
error.

Product type elements can be flattened explicitly with `...`:

```staple
let values: (String, ...(I32 ^ 3))
// Equivalent to (String, I32, I32, I32)
```

A spread operand must be a fixed product. Explicit `...(T ^ 0)` and `...(T ^ 1)`
contribute zero and one elements respectively. A bare trailing `...` retains
its separate meaning in a C-variadic function parameter type.

Fixed product values can be flattened in the same way:

```staple
let coordinates = (x: 10, y: 20)
let entry = (name: "origin", ...coordinates, visible: True)
// Equivalent to (name: "origin", x: 10, y: 20, visible: True)
```

Multiple value spreads may appear in any position. Each spread operand is
evaluated exactly once and must have a fixed product type; references and
scalar values cannot be spread. Names belonging to the spread
product's elements are preserved. Spreads are also allowed when constructing a
product used as a function argument.

A spread written `...=` flattens its operand by name instead of by position.
Every element of the operand must be named, and the surrounding product must
itself consist entirely of named elements and `...`/`...=` spreads:

```staple
let dimensions = (
    height: 600,
    width: 800,
)
let config: (
    width: I32,
    height: I32,
    title: String,
) = (
    ...=dimensions,
    title: "Staple",
)
// Equivalent to (width: 800, height: 600, title: "Staple")
```

Each named field, whether written explicitly or contributed by a `...=`
spread, is looked up by name in the surrounding product's expected type, so
its position in the constructed value does not depend on where it was
written or on its position within the spread operand. A later field or
`...=` spread overrides an earlier one with the same name. Because placement
is name-driven, `...=` requires the enclosing product to have a known,
fully-named expected product type; every expected field must be supplied
exactly once, and supplying a field the expected type does not have is an
error.

Bracket indexing is delegated to the prelude `Index` trait:

```staple
let index: USize = 1
let value = values[index]
```

`Index` has target and position parameters and an associated `Output` type
determined by them. User-defined implementations may use any
position type. The compiler derives `Index P USize Output` for every non-empty
fixed product whose elements are all `Copy`; `Output` is the duplicate-free sum
of its element types. Thus indexing `(I32, String, I32)` produces
`I32 | String`, while indexing `(I32 ^ N)` produces `I32`. The standard library
implements `Index SharedBuffer<T> USize` with `type Output = T` when `T` is `Copy`, backed by
`SharedBuffer.get_unchecked`, so a `SharedBuffer<T>` is indexed like a fixed product of `T`. Known
bad fixed-product indices are rejected and dynamic out-of-bounds indices trap.

A `Shared<T>` is transparent for indexing, the same way it is for field access:
`reference[index]` is `(*reference)[index]`, so `Index Shared<T>` and
`MutateIndex Shared<T>` are derived whenever `T` itself has the corresponding
implementation, whether structural or explicit. This composes through nested
references and lets a user-defined `Index`/`MutateIndex` implementation on a
nominal type be used through a handle without an explicit dereference.

`MutateIndex` replaces one element through a target in place. Indexed
assignment delegates only to this trait:

```staple
target[index] = replacement
// Equivalent to MutateIndex.mutate_index (target, index, replacement)
```

The compiler derives `MutateIndex` for non-empty arrays, by
value. The standard library implements `MutateIndex SharedBuffer<T> USize T` for
every `T`, backed by `SharedBuffer.set_unchecked`, which replaces the element through `Shared.replace`. The mutable `Target`
parameter passes by address either way (see the "Mutable parameters" subsection
under "Functions"), so a by-value target's root binding must be declared
`mut` just as a `Shared` target's must. These structural implementations cannot
be overridden. Other types may define ordinary explicit implementations of
both traits.

Fixed products have a compiler-provided structural `Default` implementation
whenever every element type implements `Default`. The default value holds each
element's `Default.default ()`, evaluated in element order. This applies to
heterogeneous, named, nested, and repeated products, and to the empty product
`()`, so `Default T` implies `Default (T ^ N)`:

```staple
let pair: (I32, Bool) = default ()     // (0, False)
let named: (x: I32, label: String) = default ()
let unit: () = default ()
```

A product whose fields declare defaults, which only the representation of a
`wrap` or `from` type can (see [Default field values](#default-field-values)),
has no structural `Default`: filling it structurally would ignore those
defaults. Use `@derive_default` on the named type instead. Because the
structural implementation exists, a product cannot implement `Default`
explicitly. Because `(T ^ 1)` is normalized to `T`, it continues to use the
`Default` implementation of `T`. Variadic products, sums, and `Shared<T>` do not
implement `Default` structurally.

## Functions

A function value has the following form:

```text
<parameter> => <body expression>
```

There is no distinction between a top-level function and an inline function or
lambda. Both are function values and use the same syntax.

Every function takes exactly one argument and matches it with a pattern.
Patterns are recursive: a binding pattern introduces one name, a product
pattern matches the elements of a product, and a nominal pattern exposes the
single representation value of a wrapper type when that representation is
visible.

`=>` introduces the body of the abstraction. The compiler infers the result
type from the body unless a surrounding function type or a type ascription
expression constrains it.

A function value may declare an effect lower bound immediately after `=>`:

```staple
def apply = <effect E> f: (() ->{E} ()) =>{E} f ()
```

The declared effects remain part of the function even when unused, and merge
with effects inferred from its body. An empty `=>{}` adds no requirements.
A surrounding function type still constrains the combined effects.
The opening brace must touch the arrow: `=>{IO}` declares effects, while
`=> { ... }` introduces a body block. Spaces, newlines, and comments cannot
separate an arrow from its effect set; function types likewise require
`->{IO}`, rather than `-> {IO}`.

A function whose result type is `Never` has no normal-return path. Calling it
makes the following path unreachable, just like evaluating any other
expression of type `Never`.

A binding pattern normally has a name and a type:

```staple
s: String => s
```

Use `_` when a parameter is intentionally unused. It may carry the same type
annotation as a named binding, but it does not introduce a name:

```staple
_: String => ()
```

The type may be omitted when a surrounding function type supplies it:

```staple
def identity: I32 -> I32 = value => value
```

An omitted parameter type without such a context is an error. The compiler does
not infer parameter types from operations in the function body.

A nullary product pattern is written as `()`:

```staple
() => 42
```

A product pattern names and types each of its elements:

```staple
(a: I32, b: I32) => a + b
```

Consequently, this definition does not define a function with two parameters:

```staple
let add = (a: I32, b: I32) => a + b
```

It defines a function whose single parameter is a two-element product.

Patterns may be nested:

```staple
(x: I32, (y: I32, z: I32)) => x + y + z
```

An at-pattern binds the complete value and also matches it with a nested
pattern. `@` associates to the right:

```staple
let point@(x, y) = get_point ()

let make_point = args@(_: I32, _: I32) => {
    args
}

let first@second@(left, right) = (1, 2)
```

The name before `@` is always a binding, even when capitalized, and may use
the usual `mut` modifier outside function parameter position, or a type
annotation. An annotation constrains the complete value at that pattern
position; it does not select a sum alternative independently of the nested
pattern. At-patterns are available in `let` bindings, function parameters,
match arms, propagating bindings, and quoted or spliced patterns.

At runtime, the complete value must be `Copy`, because the alias and nested
bindings receive independent copies. This also means mutating a `mut` alias
does not change its nested bindings. Compile-time syntax patterns use the
compile-time evaluator's ordinary value-copying behavior instead. Product
function parameters retain their flattened calling convention; the callee
reconstructs the complete product when the alias is used.

Nominal patterns use the generated constructor name followed by a nested
pattern. They are irrefutable and add no runtime check or wrapper:

```staple
type UserId wrap I32
def unwrap = (UserId value): UserId => I32 :: value

let user: UserId = UserId 42
let UserId value = user
```

The same syntax works inside products and with qualified type names. A generic
nominal pattern must receive its applied type from the value being destructured,
a surrounding function annotation, or its own annotation.

A parenthesized nominal pattern followed by `: Type` annotates the whole
pattern. Without the parentheses the annotation belongs to the inner pattern:
`Box value: I32` destructures a `Box` whose representation is `I32`, while
`(Box value): Box<I32>` declares that the destructured value is a `Box<I32>`.
This is how a function value names the type of a destructured parameter:

```staple
type Box<T> wrap T
def first = ((Box value): Box<I32>; index: USize) => I32 :: value
def unbox = <T> move (Box value): Box<T> => T :: value
```

Only a single nominal pattern may be annotated this way; a product or binding
pattern annotates its own elements instead.

A singleton product pattern is equivalent to its contained pattern, so
`(value: T)` matches the same values as `value: T`.

A function body may explicitly constrain its result with a type ascription:

```staple
let get_number = () => I32 :: {
    42
}
```

`<type> :: <expression>` is a general type-ascription expression, not a
runtime conversion. It checks the expression with the given expected type and
evaluates to the expression's value. `::` has lower precedence than
function application and infix operators, so `I32 :: a + b` means
`I32 :: (a + b)`. Parentheses may constrain a smaller expression.

When a type-ascription expression is a function body, its type constrains the
function result, including values produced by explicit `return` statements.
A function value can write both its parameter type and result ascription:

```staple
def identity = value: I32 => I32 :: value
```

A binding annotation may also constrain the complete function type. A function
declaration uses the same function-type syntax and omits the value:

```staple
let add: (x: I32, y: I32) -> I32
```

### Generic functions

A function value declares compile-time parameters in angle brackets before its
parameter pattern. Its type is a *scheme*: `<T> move T -> T` describes a function
that works for every `T`.

```staple
def identity = <T> move value: T => T :: value
let duplicate = <T where Copy T> value: T => (T, T) :: (value, value)

type Box<T> wrap T
companion<T> Box<T> {
    pub def map = <U> (move box: Box<T>, move f: (move T -> U)) => Box<U> :: {
        let Box value = box
        Box (f value)
    }
}

trait Visit T { visit: <A> (T, move A) -> A }
impl Visit I32 { visit = <B> (_, move value: B) => value }
```

Companion, implementation, and trait parameters remain in scope in their
members. An implementation of a generic trait member, and a generic trait
default method, must declare the member's own parameters on its function value.
Constructors of generic types (`Box` has scheme `<T> move T -> Box<T>`) and generic
intrinsics are also generic.

#### Declaring parameters

A list contains type parameters and `effect` parameters, followed by an
optional `where` clause:

```staple
def first = <A, B> (move a: A, b: B) => A :: a
def show_all = <T where Display T> items: List<T> =>{IO} () :: {
    // ...
}
def twice = <effect E> f: (() ->{E} ()) =>{E} () :: { f (); f () }
```

Parameters are positional: `first` declares `<A, B>` even though its runtime
parameter is a product. Product patterns are not allowed inside angle brackets.
The `where` clause holds trait bounds (see [Traits and bounded generic
functions](#traits-and-bounded-generic-functions)), subtype bounds (see
[Subtype bounds](#subtype-bounds)), associated-type equalities (`==`), and
`?Sized` relaxations. Effect parameters
stand for effect sets; see [Typed resources](#typed-resources).

Every type parameter has an implicit `Sized` bound. A function that forwards a
type behind a constructor accepting unsized arguments can relax that default:

```staple
def preserve_shared = <T where ?Sized T> value: Shared<T> => value
```

`?Sized T` must name a parameter of the same list and may appear once per
parameter. It is a relaxation, not a trait bound: `where Sized T` remains an
ordinary bound. A relaxed parameter cannot be passed or returned by value.
`Sized` is compiler-derived, so explicit `impl Sized` declarations are rejected.

Compile-time parameters declared by a value are visible in its pattern, effects,
and body, including local type annotations:

```staple
def singleton = <T> move value: T => List<T> :: {
    let mut items: List<T> = List.new ()
    List.push items value
    items
}
```

Without a surrounding generic function type, each runtime parameter needs a
fully known type in terms of the declared parameters. `<T> x => x` is an error;
`<T> move x: T => x` supplies the required type. The result may be inferred, as in
`def drop = <T> move value: T => ()`. A recursive generic function must ascribe
its result (`R :: ...`) before it can call itself. Definitions are never
implicitly generalized.

Only the outermost function in a curried chain may declare parameters:
`<T where Copy T> x: T => y: T => x` declares `T` for both arrows; `x => <T> y: T => y` is
rejected. A function value that is not the direct body of another arrow is a
separate value and may declare its own list: in higher-rank arguments, type
ascriptions, and blocks, as in `x => { <T> move y: T => y }`, which returns a
polymorphic value.

#### With an explicit type annotation

An annotation is a contract with its own independently scoped parameters. The
value's parameters are the function's canonical signature:

```staple
def identity: <T> move T -> T = <U> move value: U => U :: value
def copy_identity: <T where Copy T> T -> T = <U where Copy U> value: U => value

// Errors: the value must declare parameters, and annotation names cannot leak.
def missing: <T> move T -> T = move value => value
def out_of_scope: <T> move T -> T = <U> move value: T => T :: value
```

The lists must have the same count, parameter kinds (`T` versus `effect E`),
sizedness, and bounds after positional renaming. Bounds are compared as sets,
so their order does not matter. A missing or extra bound is an error. The same
rule applies to any function value checked against a generic function type,
including trait implementations, default methods, ascriptions, and higher-rank
arguments. A non-function value such as `let f: <T> move T -> T = identity` does
not need to redeclare the parameters.

An `extern` block inside a companion inherits its parameters and bounds.
Generic `extern` annotations are allowed only for the reserved
`"staple-intrinsic"` ABI. Intrinsics must be called directly; their type and
effect arguments are inferred at the call.

#### Using a generic function

Each use of a generic function infers its type arguments from the value
arguments and from the expected type. A use where an ordinary function type is
expected is an *instantiation*, which produces an ordinary function value:

```staple
let answer = identity 42
let int_identity: I32 -> I32 = identity
```

The argument is inferred first. When the type arguments it gives conflict with
the expected type, they are inferred from the expected type alone, provided
the argument coerces into the resulting parameter type. A numeric literal
argument takes its type from the expected type in the same way:

```staple
let absent: Option<Bool> = Some False   // `T = Bool`, not the singleton `False`
let wide: Option<I64> = Some 7          // `T = I64`, not the default `I32`
let either: Bool | I32 = identity flag  // `T = Bool | I32`
```

Type arguments can also be supplied explicitly with `.<…>`, which binds like
member access, before function application. `_` leaves one to inference; a
`_` nested inside an argument (`RawPtr<_>`) is completed from the expected type,
as in an ascription. Without a value argument, the result is the instantiated
function:

```staple
let list = List.new.<I32> ()
let text = identity.<String> "hello"
let specialized = identity.<String>
let mapped = Box.map.<I32, String> (Box 1, move value => "${value}")
let visited = Visit.visit.<_, I32> (1, 42)
```

Arguments follow the scheme's parameter order and must match its arity. For a
companion member that is the companion's parameters followed by the member's
own; for a trait member, the trait's explicit parameters (without associated
types) followed by the member's own. Explicit arguments must satisfy the
bounds and agree with the value arguments and the expected type.

A use of a generic function by name at known types is *monomorphized*: each
reachable instantiation produces its own code, and an instantiation that is
never reached produces none. This covers `def`s, companion and implementation
members, trait members whose trait arguments are known (static dispatch), and a
scheme-annotated `let` called by name. A function used as a polymorphic value
instead keeps its parameters at run time (below), and calls through such a value
are not monomorphized.

Monomorphization needs a finite set of instantiations. A recursive call must
use the current instantiation; a call to itself at a type that contains its own
parameter is *polymorphic recursion* and is rejected. Recursion that reaches a
deeper instantiation through other functions or trait members, such as two
functions that call each other with `Wrap value`, is reported as polymorphic
recursion when an instantiation's type arguments nest more than 16 levels deep.

#### Polymorphic values

A scheme is an ordinary type. It may appear in a binding annotation, a
parameter, a result, a product or `wrap` field, and as a type argument when an
annotation spells it out:

```staple
def consume = f: (<T> move T -> T) => (I32, String) :: (f 42, f "hello")

type Functions wrap (identity: <T> move T -> T,)

let mut selected: <T> move T -> T = identity
let functions = Functions (identity: selected)
let pair = consume functions.identity
let inline = consume (<U> move value: U => value)
```

A generic function becomes a *polymorphic value* when its context expects a
scheme, as `identity` does above. A lambda checked against a scheme is one, and
a binding without an annotation takes the scheme of its initializer
(`let id = identity`). A scheme's parameters are scoped to it, so two schemes
that differ only in parameter names are the same type.

- **Calls.** Each call infers its own type arguments, so one value may be
  called at several types, and `.<…>` works as for any generic function.
- **Instantiation.** Where an ordinary function type is expected, the value is
  instantiated as described above. The instantiated type may take and return
  functions, including `mut` function parameters and values that combine a
  function with a surrounding type parameter, such as
  `(callback: I32 -> I32, state: A)`, and the result may be stored directly into
  a `from` wrapper or a sum.
- **Subsumption.** A polymorphic value may be used where another scheme is
  expected when its own scheme can be instantiated to the expected body with
  the expected scheme's parameters held abstract. Parameters may be reordered
  or instantiated to compound types:

  ```staple
  def pick_first = <A, B> (move a: A, move b: B) => A :: a
  let swapped: <Y, X> (move X, move Y) -> X = pick_first
  let pairs: <T> move (T, T) -> (T, T) = identity
  ```

  Matching is shallow: only the outermost scheme of a value is instantiated,
  never one nested in an argument or result. An explicit argument such as
  `f.<(<U> move U -> U)>` may name a scheme.
- **Bounds.** A scheme may carry trait bounds, as in
  `<T where Display T> move T ->{IO} ()`. Each call or instantiation supplies a
  dictionary for every bound at the types it uses (see [Traits and bounded
  generic functions](#traits-and-bounded-generic-functions)). A value used at
  another scheme obtains its dictionaries from that scheme's bounds and their
  prerequisites.
- **Effects.** A scheme with `effect` parameters may be called with a different
  effect set each time (see [Typed resources](#typed-resources)).
- **Storage.** A polymorphic value is a closure and is `Copy`. A `let mut`
  binding keeps the scheme it was declared with, and an assignment must supply
  a value of that scheme.

A function can become a polymorphic value only when its implementation can run
without knowing its type arguments at compile time. The compiler reports the
following where the value is built; the same function still works when it is
called directly:

* a trait obligation in the implementation that no dictionary can satisfy: it
  must come from a bound in scope or match exactly one implementation;
* a `Natural` bound or an array length that depends on a type parameter;
* a scheme that names an associated type of its parameters, such as
  `<Iter where Iterator Iter> move Iter -> IterStep<Iter, Iterator.Item Iter>`;
* a coroutine, async, or reactive body;
* a function value behind `Shared`, `SharedBuffer`, `Buffer`, or a shared cell that
  crosses into the implementation.

The checker also rejects schemes in signals and in ordinary `extern`
signatures and intrinsics used as values.

### Typed resources

A typed resource is an implicit value identified by its concrete nominal type.
There is no separate resource declaration: any fully concrete, sized nominal
type may be used, including sized opaque types. Unsized types cannot be
used as resources. For example:

```staple
type Clock wrap (
    now: () -> I32,
)

type Logger wrap (
    write: String -> (),
)
```

`resource` and `with` are reserved keywords and cannot be used as identifiers.

Transparent aliases use the identity of their underlying nominal type. A
structural type, unsized type, or unspecialized generic type is not eligible.

#### Resource dependencies

A type declaration may require other resources with a `with` clause. The
clause follows the declaration's generics and `where` constraints and comes
before the body marker (`=`, `wrap`, `from`, or `opaque`), or before the end of
a bodyless singleton declaration:

```staple
type Logger wrap (
    write: String -> (),
)

type Database with {Logger} wrap (
    path: String,
)

def log = (message: String) =>{Logger} () :: (resource Logger).write message

def query = (sql: String) =>{Database} Rows :: {
    log "query: $sql"   // allowed: `{Database}` implies `{Logger}`
    ...
}

with Logger = stdout_logger () {
    with Database = open_database "app.db" {
        query "select 1"
    }
}
```

Braces are required even for one entry. `with {}` is accepted and means
nothing. Entries may be marked `mut`. The clause adds requirements to effect
sets; it does not bundle values. An effect set that mentions `Database` stands
for `{Database, Logger}`. A function with that set receives one hidden
parameter for each resource, and each is resolved from the innermost provider
at the call site, exactly as if both had been written out.

Every checked effect set is kept closed under dependencies, transitively.
Closing happens before the canonical sort and merge, so `->{Database}` and
`->{Database, Logger}` are the same type with the same hidden-parameter order.
Mutability merges as usual: `{Database, mut Logger}` stays `mut Logger`, while
`{Database}` gives `Logger` the mode declared by the dependency. The modes are
independent: `{mut Database}` does not make `Logger` mutable, and `{Database}`
does not make `Database` mutable. Effect-variable inference still chooses the
minimal substitution, because every set it compares is already closed. Adding
or removing a dependency changes the type of every function whose set mentions
the type; for a public type this is a breaking change.

Each entry must name a type that is valid as a resource on its own: concrete
after substitution, sized, and nominal. Entries may name the declaration's type
parameters, as in `type Cache<K> with {Store<K>}`; they are substituted when
`Cache<…>` is used in an effect set. State effects, effect variables, named
entries such as `{log: Logger}`, the declaring type itself, and any dependency
cycle are rejected. Transparent aliases take their identity and dependencies
from their underlying type, so they cannot declare a clause of their own. Each
dependency must be at least as visible as the declaring type; a `pub` type
cannot depend on a private resource that importers cannot name or provide.

A named resource entry names only the type it spells. In `{db: Database}` the
implied `Logger` is still reached as `resource Logger` inside the body. To name
it as well, list it explicitly: `{db: Database, log: Logger}` has the same set,
now with two names.

Providers are not paired with dependencies. `with Database = …` does not
capture a `Logger`; an inner `with Logger = quiet_logger () { … }` changes the
logger that database code observes in that block. Nothing ties a database to
"its" logger. A provider site is not checked for the dependencies of the
provided type: the body's requirements include `Logger` once expanded and pass
up to the enclosing function, so an error comes only from an explicit enclosing
contract. Provider order matters only for initializers, which run in the outer
scope; if `open_database` is itself `->{Logger}`, then `Logger` must be the
outer provider. A `Database` passed, stored, or returned as an ordinary value
carries no `Logger`; the clause matters only where the type appears in an
effect set.

Resource ownership follows ordinary parameter ownership. An immutable `Copy`
resource is copied, while an immutable non-`Copy` resource is shared-borrowed
and cannot be moved, dropped, or returned whole. A resource entry may be marked
`mut` to request a mutable borrow:

```staple
def increment = () =>{mut Counter} () :: {
    (resource Counter).value = (resource Counter).value + 1
}
```

A function value's resource set may give a resource a local name:

```staple
def increment = () =>{mut counter: Counter} () :: {
    counter.value = counter.value + 1
}
```

Within the body, `counter` is an alias for `resource Counter`, with the same
borrowing and mutation rules. Names follow lexical scope and may be shadowed
inside nested scopes. They do not change the function's type or resource
identity; function type signatures list only the resource types. Unnamed and
named entries may appear in the same set.

`mut R` subsumes `R` when checking a function body, and a set containing both
canonicalizes to `mut R`. Mutation is performed directly through `resource R`
or its resource name, or by forwarding either expression to a mutable
parameter. Binding a mutable-borrow alias with `let` is not supported.

Function arrows list their effects in braces. Effect sets are
unordered and duplicate-free, and each arrow in a curried type has its own set:

```staple
def timestamp = () =>{Clock} I32 :: {
    let clock = resource Clock<clock.now, ()>
}
```

Generic functions may quantify over an effect set with an `effect` parameter:

```staple
def twice = <effect E> f: (() ->{E} ()) =>{E} () :: { f (); f () }
```

An effect variable may represent the empty set, resources, state effects, or a
combination of them. It can form an open row with fixed effects, as in
`{E, IO}`; inference chooses the minimal substitution satisfying every
occurrence. Each set may contain at most one variable. Effect parameters exist
on generic function bindings and effect-parameterized type declarations and
may appear only in effect sets. They never change a value's representation: a
function value is the same pair whatever its effect set. Called directly, an
effect parameter is concrete before code generation. Inside a polymorphic
value it is not known until the value is called, and the caller passes the
resources of the instantiated set alongside the call. Type declarations use the
declaration-site syntax described under [Types](#types).

An effect-polymorphic function can be used as a polymorphic value, stored,
passed, and called with a different set each time:

```staple
let stored: <effect E> (() ->{E} ()) ->{E} () = twice
def run = repeat: (<effect E> (() ->{E} ()) ->{E} ()) =>{IO} () :: {
    repeat (() => println "hi")           // E := {IO}
    repeat (() => ())                     // E := {}
}
run stored
```

### Signals and reactions

A signal is a writable binding whose ordinary reads and writes participate in
runtime dependency tracking. Its visible type remains the value type:

```staple
let signal count = 0
count = count + 1
```

`reaction` accepts a zero-argument callback, usually written as a
[block argument](#block-arguments). Its initial run and every rerun are scheduled on
the reactive executor. Outside a batch, the executor drains synchronously to
quiescence before ordinary execution continues, so a reaction still runs
immediately when created and has completed before a signal assignment returns.
Dependencies are recollected on every run. Assigning the same value still
notifies dependents. Writing a product field of a signal also notifies
that signal's dependents, including reactions that read the field.

Reaction work is FIFO. Reactions made runnable by one propagation are appended
in creation order, and work made runnable while a reaction runs is appended to
the queue tail. Duplicate scheduling is coalesced. A reaction is never invoked
recursively: if it changes one of its own dependencies, it runs again after its
current invocation returns. If one synchronous drain reaches 100,000 reaction
executions without stabilizing, the runtime reports that the reactive update
did not stabilize and traps.

Reactions belong to the innermost `Reactive` resource and are disposed when its
`with` scope exits, including through `return`, `break`, or `continue`:

```staple
with Reactive = reactive_scope () {
    reaction { println "count: $count" }
    count = 1
}
```

At the entry module's top level, a `Reactive` scope is opened implicitly
whenever it's needed, so the `with` block above may be omitted there (see
[Top-level statements](#top-level-statements)).

An immutable `let` whose initializer depends on a signal is a persistent
derived binding. Its visible type is unchanged. The initializer is evaluated
on the first read, cached, and evaluated again on the first read after one of
the signals used by its previous evaluation changes:

```staple
let signal count = 1
let doubled = count * 2
```

Dependencies are recollected after each evaluation, so conditional
derivations only subscribe to the branch they actually read. Derived bindings
are not writable and their initializers must be pure apart from reading
signals. A mutable binding or independent signal initialized from reactive
data must use `snapshot`.

`snapshot` evaluates its argument without recording reactive dependencies and
states that an eager, non-reactive value is intentional:

```staple
let frozen = snapshot (count * 2)
```

The runtime propagates derived invalidation before synchronously flushing
reactions, preventing intermediate values in derived chains and diamonds.
Separate assignments remain unbatched. A derived binding that reads itself
while evaluating still traps.

`batch` evaluates a zero-argument callback synchronously while deferring
reaction execution until the outermost batch exits:

```staple
batch {
    count = 1
    count = 2
    count = 3
}
```

Signal writes and reads take effect immediately inside the callback, and
derived bindings are invalidated normally. Each dependent reaction is queued
at most once and observes the resulting state when the batch finishes. Nested
batches share the outer batch. A reaction created inside a batch also defers
its initial run; if its owning `Reactive` scope exits first, the queued run is
discarded safely.

### Block arguments

A block written directly as a call argument, for a parameter of type
`() -> T`, is a **block argument**: the compiler treats `{ … }` as
`() => { … }`, so the block runs only when the callback is invoked, as many
times as it is invoked. Its value must have type `T`:

```staple
def twice = <effect E> (f: () ->{E} ()) => {
    f ()
    f ()
}

twice {
    println "hello"
}
twice (println "hello") // error: expected `() -> ()`, found `()`
```

Only a bare block is adapted; it may also be a direct element of a product
argument, as in `add_later (40, { 2 })`, or fill a slot, as in
`direct 2 { 3 }`. Any other argument is passed as written, so the braces show
at the call site which code is deferred. A block argument always becomes the
callback, even when the block's value is itself a compatible function. To
compute a function eagerly with a block, wrap it in grouping parentheses:

```staple
twice ({ make_callback () }) // evaluates the block once and passes its value
```

Block arguments are restricted to call arguments; they are not a general
coercion, so `let callback: () -> I32 = { 42 }` remains invalid. A pipe
receiver is evaluated before the call, so `{ … } |> f` passes the block's
value rather than a block argument. A spread operand is never a block
argument either.

A block argument is the body of its callback. It captures values and infers
the effects of its body exactly like an explicit anonymous function, which
lets effect-polymorphic higher-order functions preserve deferred resource and
state effects. `return` inside it returns from the callback, not from the
enclosing function, and `?` likewise propagates to the callback's result.
This differs from the blocks of `with`, `loop`, and macros such as `if`,
which are part of the enclosing function. `break` and `continue` cannot
leave a block argument, though a loop written entirely inside it may use
them:

```staple
def evaluate = callback: (() -> I32) => I32 :: callback ()

let answer = evaluate {
    if cached { return cached_value }
    expensive_computation ()
}
```

The empty set may be written explicitly as `->{}`. Reordering or repeating
entries does not change a function type or the canonical hidden-parameter
order.

Mutable bindings accessed from outside a function—either lexical captures or
module bindings—contribute state effects. Reading such a cell requires
`state.read`, writing it requires `state.write`, and a function that does both
uses the canonical combined spelling `state`. These effects are compile-time
properties and do not add hidden runtime parameters:

```staple
let mut count = 0
let next: () ->{state} I32 = () => {
    count = count + 1
    count
}
```

The compiler retains the identity of each captured cell read or written for
analysis, while function type equality uses only the public state effect.

`resource Clock` has type `Clock` and makes the enclosing function require that
resource. Calls propagate requirements transitively, including through
recursion, trait methods, and function values. A function without a type effect
contract infers the union of its body effects and any function-value lower
bound. An explicit set in a function type remains part of its declared
contract: resources may be unused, but the body cannot require an unlisted
resource. Function types with different effect sets are distinct and are not
implicitly widened.

Resources are supplied lexically with `with`:

```staple
with Clock = system_clock {
    timestamp ()
}
```

The provider mode is explicit. `with R = value` supplies an immutable
resource: it copies `value` when `R` is `Copy`, and otherwise shared-borrows
it. `with mut R = value` supplies a mutable resource:

```staple
let mut config = Config (x: 42)
with mut Config = config {
    update_config ()
}
```

A mutable named provider must be declared `mut`. A non-place mutable provider
is materialized in scope-local storage and its final value is discarded. An
immutable provider cannot satisfy a `mut R` requirement.

The provider is evaluated before its binding is installed. A nested provider
of the same concrete type shadows the nearest outer provider, while its own
initializer can still use the outer value. The body result is the result of the
`with` expression. Creating a function inside `with` does not capture the
provider: the function keeps the resource in its type and receives it when it
is called.

Resource values are passed as hidden parameters after the closure environment
and before explicit arguments. Immutable `Copy` resources are passed by value;
immutable non-`Copy` resources are passed by shared address; and `mut` resources
are passed by mutable address. Executable top-level initialization
must supply every required resource. External functions cannot declare Staple
resources because foreign ABIs do not include these hidden parameters.
Resource-bearing function types and nominal resource identities are preserved
when public definitions are imported or re-exported by another module.

`std.io` exports the opaque `IO` resource and the functions `print` and
`println`, both with type `String ->{IO} ()`. `IO` is implicitly available to
the entry module's top-level statements; ordinary code receives it
transitively through its function resource contract. Calling either output
function from a non-entry module's top-level initialization is therefore
still rejected.

Standard output and input operations are byte based and preserve embedded NUL.
`print`, `println`, `eprint`, and `eprintln` accept `String` and panic on write
failure; the `e` variants use standard error. Their `try_` variants return
`Result<(), IoError>`. `flush_stdout ()` and `flush_stderr ()` return the same
result and expose errors that occur while draining buffered output. Flush output
explicitly before waiting for a response to a prompt.

`stdin ()`, `stdout ()`, and `stderr ()` return `Stdin`, `Stdout`, and `Stderr`
handles to the process streams. They share the underlying C stdio buffering;
creating or dropping a handle does not open or close the stream. `Read` supplies
`read_byte: mut R ->{IO} Result<Option<U8>, IoError>`; `None` means EOF.
`Write` supplies `write_all: (mut W; SharedBuffer<U8>) ->{IO} Result<(), IoError>`
and `flush: mut W ->{IO} Result<(), IoError>`. A failed write may have emitted a
prefix and must not be retried blindly. `IoError` currently reports failure
without an operating-system error code. `write_string writer text` writes a
string through any `Write` implementation without a C-string conversion.

`read_line ()` reads standard input; `read_line_from reader` accepts any `Read`
implementation. Both return `Result<Option<String>, ReadLineError>`, where
`ReadLineError` is `IoError | InvalidUtf8`. They remove LF and a preceding CR,
return `Some ""` for an empty line, return a final unterminated line, and return
`None` only when EOF occurs before any bytes. Invalid UTF-8 consumes the full
line and returns `InvalidUtf8`.

`std.fs` provides files and directories on 64-bit Linux and Apple targets.
All disk operations require `IO`. `Path` accepts a `String` implicitly and
provides pure `/`-separated path operations; interior NUL produces
`FsError.InvalidPath` at the OS boundary. `FsError` is one nominal error type
with qualified variants such as `FsError.NotFound`, `PermissionDenied`,
`AlreadyExists`, `NotADirectory`, `IsADirectory`, `DirectoryNotEmpty`, and
`Os (code: I32)` for other OS errors. `exists` returns false only for
`NotFound`; other errors propagate.

`read`, `read_to_string`, `write`, `write_string`, and `append_string` open
and close files within the call. Writes report errors deferred until close.
`File` supports streaming, seeking, metadata, flushing, and syncing, and
implements `std.io.Read` and `Write`. It is not `Copy`; dropping it closes
its stream and ignores close errors. Consume it with `File.close` to observe
those errors. `File.open_with` takes `OpenOptions`; use named overrides such
as `OpenOptions (.write: True, .create_new: True)`.

`read_dir` eagerly returns a name-sorted `List<DirEntry>`, excluding `.` and
`..`. Invalid UTF-8 names fail the entire listing with `InvalidUtf8`.
`metadata` follows symlinks; `symlink_metadata` inspects the final link.
`remove_dir_all` inspects links rather than traversing their targets; a
non-UTF-8 child name produces `FsError.Os` with the target's `EILSEQ` code.
Paths and directory names are UTF-8. Windows filesystem calls currently
produce an unsupported-target diagnostic; Windows path separators and wide
character APIs are reserved for a later phase.

The runtime supplies a target-specific stdio bridge using length-based
`fwrite`, `fflush`, and `fgetc`; no new C interop syntax is required. Panic
messages use a best-effort write and flush to standard error, including embedded
NUL bytes, without recursively panicking if the diagnostic cannot be written.

Macros may quote, splice, generate, and transform resource syntax. Attempting
to evaluate `resource` or `with` as a compile-time macro operation is rejected;
providers exist only in runtime lexical scopes.

Nested providers continue to shadow by nominal resource type.

### Mutable parameters

`mut` on the parameter side of a function arrow declares which arguments the
function may write into. It is separate from callable effects such as `IO` and
`state`:

```staple
def f1: mut A -> () = mut a => { ... }                   // may mutate the whole parameter
def f2: (mut A, B) -> () = (mut a, b) => { ... }        // may mutate parameter 0 only
def f3: (mut a: A, b: B) -> () = (mut a, b) => { ... }  // same, named
def f4: (mut a: A, b: B) ->{IO} () = (mut a, b) => { ... }
```

Prefixing the whole parameter type permits mutation of the complete argument.
For a product parameter, `mut` may instead prefix individual positional or
named elements. A `mut` marker on a parameter binding declares the same
permission without requiring a function type annotation:

```staple
def f1 = mut a: A => { ... }                  // mut A -> ...
def f2 = (mut a: A, b: B) => { ... }          // (mut A, B) -> ...
def f3 = (a: A, mut b: B) => { ... }          // (A, mut B) -> ...
```

Markers are allowed only on a whole parameter binding or a direct binding in
the top-level parameter product. When a function has an explicit type
annotation, its parameter markers are not optional: the pattern must repeat a
`mut` for every position the annotation declares mutable, and their mutation
targets must match exactly. The marker does not introduce an ordinary mutable
local; it declares the function parameter position that is passed by address.

Inside parentheses, a marker followed by a type and then `->` belongs to that
function type, not to the enclosing element or slot. This is how a
higher-order function takes a mutating callback:

```staple
let reset: (mut I32 -> ()) = mut n => { n = 0 }
def call: (mut I32 -> ()) -> () = f => ()         // parameter is `mut I32 -> ()`
def twice = <A> (f: (mut A -> ()); mut a: A) => () :: { f a; f a }
```

Only a marker with no following `->` marks the element itself, as on `mut A`
in the second slot of `twice`.

`mut` may also prefix a parenthesized product pattern, marking the whole
destructured parameter mutable as one unit rather than by individual element:

```staple
def f5: mut (A, B) -> () = mut (a, b) => { ... }  // whole parameter, destructured
```

This differs from `f2` above (`mut` on one element) in that every element is
part of the same by-address parameter; it is equivalent to binding the whole
parameter and destructuring it in the body (`mut pair => { let (a, b) = pair; ... }`),
written directly in the pattern. This form exists only at a function's own
parameter position — `let` and `match` patterns still reject `mut`/`move`
before `(`.

The effect permits both writing through a parameter and replacing the
parameter value itself. A mutable argument is passed by address, so either
kind of change is visible in the caller:

```staple
def clear = mut cell: Shared<(I32, I32)> => () :: { cell.0 = 0 }

let mut counter: Shared<(I32, I32)> = Shared (1, 2)
clear counter

let fixed: Shared<(I32, I32)> = Shared (1, 2)
clear fixed // error: `fixed` is not declared `mut`

def replace = mut value: I32 => () :: { value = 42 }
let mut answer = 0
replace answer // answer is now 42
```

A non-place argument is materialized into call-scoped mutable storage. Its
final value is discarded (and dropped when necessary), which permits useful
one-shot calls without weakening immutable named bindings:

```staple
replace (20 + 22) // allowed; the final 42 is discarded
let fixed_answer = 0
replace fixed_answer // error: the named binding is not `mut`
```

Mutable parameter permissions are never inferred from a function body and
never propagate from a callee into its caller. A function that assigns through a parameter,
captures it for mutation, or forwards it to a mutating callee must declare the
corresponding parameter marker or mutable parameter type. Runtime resource
effects continue to infer and propagate independently. Mutation permissions carry no
runtime value or extra argument, but affected parameter positions use an
address-passing ABI.

`MutateIndex.mutate_index` declares its `Target` parameter mutable, so
`a[i] = v` requires `a`'s root binding to be declared `mut`.

### Move and borrow parameters

An ordinary (unmarked) parameter of a `Copy` type is passed by copy, as
before. An ordinary parameter of a non-`Copy` type is instead an implicit
**shared borrow**: the callee may read it — access fields, pass it on to
another borrow parameter, match on it — but may not move it, drop it, or
write through it, and the caller keeps ownership and its value remains usable
after the call:

```staple
type File wrap I32
impl Drop File {
    cleanup = File descriptor => close descriptor
}

def describe = file: File => String :: "file $file"
// `file` is borrowed here: `describe` cannot move, drop, or return it whole.
```

`move` marks a parameter as an ownership transfer instead, matching a plain
`Copy` parameter's behavior for `Copy` types and today's ordinary
argument-passing for non-`Copy` types: the callee owns the value, may move it,
store it, return it, or let it drop at scope exit, and the caller can no
longer use the argument afterward:

```staple
def close_file = move file: File => () :: drop file

let handle: File = open "log.txt"
close_file handle
handle // error: use of moved value
```

A temporary argument is a value that belongs to no binding, such as a call
result or a constructor application. When it is passed to a borrowed
parameter, the call site owns it and drops it as soon as the call returns; when
it is passed to a `move` parameter, the callee owns it. A named binding passed
to a borrowed parameter, including a named product spread into borrowed
parameters, stays owned by its binding. Spreading a temporary product applies
the rule to each element: elements in borrowed parameters are dropped after the
call, and elements in `move` parameters belong to the callee. The rule is the
same for direct calls, closure calls, trait methods, and C functions.

`move` uses the same syntax and placement rules as `mut`: it may prefix a
whole parameter type or binding, or a direct positional or named element of a
top-level product parameter (a parenthesized `move T -> R` is a function type,
as with `mut`), and a `move` marker on a parameter binding
declares the same effect without requiring a function type annotation:

```staple
def f1: move A -> () = move a => { ... }               // owns the whole parameter
def f2: (A, move B) -> () = (a, move b) => { ... }     // owns parameter 1 only
def f3 = (a: A, move b: B) => { ... }                  // same, from the binding
```

When a function has an explicit annotation, its parameter markers must
repeat and match the annotation's `move` targets exactly, the same as for
`mut`. A trait implementation's method is checked the same way against the
trait's method declaration, even when the parameter's type is `Copy`. A
parameter cannot be marked both `mut` and `move`: a mutable borrow and an
ownership transfer are different effects. `move` and `mut` never change the
parameter's exposed type — it stays `T`, never `Shared<T>` — only its ownership
and calling convention.

Like `mut`, `move` may prefix a destructuring pattern to own the whole
parameter as one unit, whether that pattern is a parenthesized product or a
nominal destructure:

```staple
def f4 = <A, B> move (a: A, _: B) => A :: a
def unbox = <T> move box: Box<T> => T :: { let Box value = box; value }
```

Every destructured binding is owned, exactly as if the whole parameter were
bound and destructured in the body. Prefixing a parenthesized product pattern
is the way to own a parameter whose type is an alias of a product — for
example, an aliased pair `type Pair<A, B> = (A, B)` — since `move`/`mut`
on the aliased type is necessarily a whole-parameter marker rather than one
over individual elements.

Without the marker, a nominal destructure — `Box value => ...` — is an
ordinary borrow: its non-`Copy` fields are borrowed too, and moving one out is
an error. Mark the whole pattern `move` to take ownership of its fields, or
bind a moved parameter and destructure it in the body. Match bindings borrow
unless their uses demand ownership.

`move` is always legal on a `Copy` parameter, where it is a no-op: copying
and moving a `Copy` value are indistinguishable, so `move T` and plain `T`
behave identically for `Copy` types. It matters only for non-`Copy` types,
where it is the only way to hand ownership of a parameter to the callee.
Consequently, generic code over an unconstrained type parameter `T` — which
is conservatively treated as non-`Copy`, since no `Copy T` bound is known —
must mark a parameter `move` before moving, returning, or storing it:

```staple
def identity = <T> move value: T => T :: value
```

Constructs that consume a value outright — `drop`, string interpolation,
indexed assignment's replacement value, and a constructor call's arguments —
always require ownership. Match bindings borrow non-`Copy` values by default.
Passing a binding to a `move` parameter, returning it, storing it in owned
storage, or dropping it requires ownership. If any arm requires ownership,
the whole match consumes its subject on every path. A borrowed subject cannot
satisfy that demand. `Copy` bindings copy their values.

Patterns automatically dereference `Unique<T>` and `Shared<T>` when their
shape selects the payload rather than the handle. Untyped bindings and
wildcards match the handle itself. Owned `Unique` payloads can be extracted
when a binding needs ownership; borrowed `Unique` payloads and all `Shared`
payloads can only be borrowed or copied. Extracting a `Unique` frees its
allocation and drops unused owned fields. Moving or mutating an owner while
an arm borrows its payload is rejected. Shared alias checks are conservative:
while a named shared subject's non-`Copy` payload is borrowed, mutation through
other shared handles is also rejected. Capturing an
ordinary non-`Copy` parameter in a curried closure is the exception: the
closure keeps a shared borrow of that parameter. Capturing a `mut` parameter
similarly keeps a mutable borrow; `mut` does not transfer ownership.

Borrowed partial applications are conservatively lexical. They may be called
immediately or stored in an immutable local and called within that local's
block. The source remains borrowed until the end of the block. A borrowed
closure cannot be returned, passed as an argument, placed in a product,
assigned, stored globally, carried out of a loop, or captured by another
closure. Borrow-producing functions must be called through their statically
resolved name or companion access rather than through a function alias.

Use a `move` parameter when the partial application must own its captured
value and escape independently:

```staple
def borrowed = value: File => other: File => describe value
def owned = move value: File => other: File => { drop value; describe other }
```

### Traits and bounded generic functions

A trait declares a set of functions for one or more compile-time parameters,
written directly after the trait name with no surrounding brackets and no `=`.
Its parameters may be given as bare juxtaposed names or grouped in a product
binder:

```staple
trait ToString T {
    to_string: T -> String
}

trait Add Left Right Output {
    add: (Left, Right) -> Output
}

trait Convert (From, To) {
    convert: From -> To
}

trait PartialOrd T where Copy T {
    partial_cmp: (T; T) -> Option<Ordering>
    lt: (T; T) -> Bool = (left; right) => match (partial_cmp left right) {
        Some Ordering.Less() => True,
        _ => False,
    }
}

trait Ord T where Eq T, PartialOrd T {
    cmp: (T; T) -> Ordering
}

trait Increment T {
    increment: T -> T
    increment_twice: T -> T = value => increment (increment value)
}
```

A trait may declare associated types with `type` inside its body. An
associated type is a type that each implementation chooses, uniquely determined
by the trait's parameters:

```staple
trait Iterator Iter {
    type Item
    next: Iter -> IterStep<Iter, Item>
}

trait Add Left Right {
    type Output
    add: Left -> Right -> Output
}
```

Inside the trait body, an associated type is in scope by its bare name, as
`Item` and `Output` are above. An associated type may carry its own `where`
clause; those bounds become prerequisites of the trait, as in
`type Iter where Iterator Iter`. Trait parameters are written in the header
only, so `Iterator Iter` is the complete bound for an iterator type.

An associated type is named from outside the trait with the projection
`Trait.Name arguments`, where the arguments are the trait's explicit
parameters. A projection of concrete types normalizes through the matching
implementation, so `Iterator.Item Range<I32>` is `I32`. A projection of generic
types requires a matching bound in the same `where` clause:

```staple
def next_item = <Iter where Iterator Iter> move iter: Iter =>
    IterStep<Iter, Iterator.Item Iter> :: Iterator.next iter
```

A `where` clause may require exact equality of associated types with `==`:

```staple
def chain = <L, R where Iterator L, Iterator R,
    Iterator.Item L == Iterator.Item R>
    (move left: L; move right: R) => Chain<L, R, Iterator.Item L> :: ...

def integer_item = <Iter where Iterator Iter, Iterator.Item Iter == I32>
    move iter: Iter => Option<I32> :: iter |> find (x => True)
```

At least one side must be an associated-type projection; the other may be
another projection, a type parameter, or a concrete or compound type. Both
sides use ordinary type syntax, including parentheses around applied projection
arguments. Equalities are symmetric, transitive, and independent of the order
of constraints. Write multiple equalities as separate comma-separated clauses.
`==` requires the same type, including ownership markers and effects in function
types; it performs no conversion or subtype widening.

The projections require matching trait bounds, including bounds supplied by
trait prerequisites. An equality does not introduce a trait bound implicitly.
Inside the declaration the equal types are interchangeable, including inside
other types. At each call or type instantiation the caller must establish the
equality. Associated types can determine an ordinary type argument, but an
equality does not select an iterator or other trait argument by searching for
implementations with the requested associated type. Contradictory and recursive
equalities are errors. A union may appear in an equality, but the compiler does
not infer its individual alternatives from that equality: union construction is
not injective (`A | I32 == String | I32` does not determine `A`). Associated-type
equality constraints are available wherever trait bounds are allowed, including
generic function types, type and
trait declarations, associated-type declarations, implementations, and companions.
Implementation arguments retain their existing restrictions; put projection
relationships in the implementation's `where` clause. The existing restriction
on polymorphic values whose schemes retain an associated-type projection still
applies; an equality with a concrete type or an explicit type parameter can
remove that projection. Other existing restrictions on polymorphic
implementations still apply, including transitive calls to consumers whose
associated item type is not available to their runtime implementation.

An implementation binds every associated type exactly once with
`type Name = Type`:

```staple
impl Iterator Counter {
    type Item = I32
    next = move Counter (current) => IterStep.Yield (current, Counter (current + 1))
}
```

Associated types are global coherence promises. Two implementations cannot
agree on every trait parameter while binding different associated types;
contradictory generic bounds are rejected for the same reason. Implementation
headers must name every trait parameter explicitly; see
[Generic trait implementations](#generic-trait-implementations) below for how
an implementation may still be generic over its own compile-time parameters.

Trait members must have function types, must mention every trait parameter
(see below for a member that fixes one through its own bounds instead), and
cannot contain inferred types. Traits may be exported with `pub trait` and are
imported with the same namespace, selected, renamed, and glob forms as other
public declarations.

An implementation supplies every trait member exactly once for a fully concrete
type. Member types are taken from the trait and do not need to be repeated:

```staple
impl ToString I32 {
    to_string = number => {
        // ...
    }
}

impl Add I32 I32 I32 {
    add = (left, right) => left + right
}

impl Convert (I32, String) {
    convert = value => "converted"
}
```

Juxtaposed binders each consume one successive argument and must all be
supplied together, while a product binder consumes one product argument.
Parentheses group an applied or otherwise complex type as
one argument. Implementation arguments may be built-in, nominal, product,
sum, pointer, or function types, and must be fully concrete except where they
mention the implementation's own compile-time parameters (see
[Generic trait implementations](#generic-trait-implementations) below).
Implementations have no visibility modifier: every implementation in the
loaded program is available globally. Defining the same trait/argument
combination twice, including through aliases of the same types or through
alpha-equivalent generic headers, is an error.

Trait implementations obey a package-level orphan rule. An implementation is
legal only when the package declaring it also declares the trait or at least
one top-level nominal implementation argument. A local nominal type nested
inside a product, reference, buffer, function, sum, or external type
constructor does not make that outer argument local. Type aliases likewise use
the ownership of their resolved underlying type. This prevents downstream
packages from defining competing implementations for the same external trait
and external types.

A trait may place one or more prerequisite bounds in a `where` clause between
its parameters and member block. Every implementation must satisfy the
instantiated prerequisites, and a generic bound makes its prerequisites
available transitively:

```staple
def equal_ordered = <T where Ord T> left: T => right: T => Bool :: {
    left == right
}
```

Prerequisite cycles are rejected. A prerequisite is part of the bound's
dictionary when the bound is passed at run time (see [Traits and bounded
generic functions](#traits-and-bounded-generic-functions)): it adds nothing to
code that is monomorphized.

[Resource dependencies](#resource-dependencies) play a similar role by making
another requirement available, but they are not coherent the way trait
implementations are. They are re-resolved from the innermost provider at every
call, so shadowing a provider changes what the dependency observes.

A member may provide a default function body after `=`. An implementation may
omit a defaulted member or replace it with an explicit member of the same name.
Calls from a default body use normal trait dispatch, so an explicit member on
the concrete implementation overrides a sibling default. Default bodies are
generic over the trait parameters and are monomorphized only when used.

A generic `def` adds one or more trait bounds in a `where` clause inside its
angle-bracketed compile-time parameter list, before its runtime parameter
pattern:

```staple
def print = <T where ToString T> value: T =>{IO} () :: {
    print_string (to_string value)
}

def combine = <L, R, O where Add L R O> pair: (L, R) => O :: {
    Add.add pair
}
```

Bounds are explicit and must be propagated by other generic functions. A
concrete use must have a matching implementation. Trait members are first-class
function values and may be called unqualified when unambiguous or qualified as
`ToString.to_string`; a namespace-qualified trait may be used as
`strings.ToString.to_string`.

Traits use a hybrid strategy. A call whose types are known at compile time is
monomorphized: the compiler substitutes the concrete trait arguments and emits
a direct reference to the selected implementation member, with no run-time
value or function parameter. A generic `def` that is used as a polymorphic
value (see [Generic functions](#generic-functions)) keeps its type parameters
at run time, and each of its trait bounds becomes a hidden *dictionary*
argument after the type descriptors: a table of the trait's members, its
prerequisite dictionaries, and its associated types. Code that calls a bound's
member, or passes the bound to another generic function, reads the dictionary
instead of selecting an implementation.

A dictionary is chosen where a run-time obligation arises. A bound in scope
supplies its own dictionary, and its prerequisites supply theirs (an `Ord T`
bound provides `Eq T`). Otherwise exactly one implementation must match the
obligation when the run-time types are treated as unknowns, and its `where`
clause must be provable; the dictionary is built from that implementation's
dictionaries when the code runs. An obligation that more than one
implementation could match is rejected at the place the polymorphic value is
built, so that dictionary passing always selects what monomorphization would;
because overlapping implementations are already rejected (see [Generic trait
implementations](#generic-trait-implementations)), this does not arise for
ordinary programs. The compiler-derived `Debug` of products and sums is built
from their components' dictionaries, at run-time and at concrete types alike.
`Copy`, `Sized`, and `Drop` need no dictionary. An entire implementation may be
generic over its own compile-time parameters (see below). Trait objects are
not supported.

A member may also declare its own type or effect parameters:

```staple
trait Visit T { visit: <A> (T, move A) -> A }
impl Visit I32 { visit = (_, move value) => value }
let answer = Visit.visit.<_, I32> (1, 42)
let member = Visit.visit
let text = member (1, "hello")
```

The implementation must work for every member argument; its body is checked
with those parameters held abstract. The member's parameter names are in
scope in its default body, along with the trait's parameters. An `impl` body
uses its own header's parameter names and does not inherit the member's names.
A member parameter that the function type does not mention is
allowed; its argument must then be given explicitly, as in
`Measure.measure.<_, String>`. Each member must mention every explicit trait
parameter in its function type, or fix it through one of its own bounds. An
equality is the usual way: in
`insert_all: <Iter where Iterator Iter, Iterator.Item Iter == Item> (move Target; move Iter) -> Target`,
`Item` appears only in the equality, and a use determines it from `Iter`.

As a polymorphic value, `Visit.visit` has the scheme
`<T, A where Visit T> (T, move A) -> A`. The trait's explicit parameters precede
the member's own parameters, and the trait bound precedes the member's bounds.
Explicit `.<…>` arguments follow that order; hidden associated-type parameters
are omitted, and `_` leaves a position to inference. Each use instantiates the
scheme independently. Ordinary members such as `ToString.to_string` can also
be stored or passed as polymorphic values. Schemes that name associated-type
projections retain the [polymorphic-value restriction](#generic-functions).

Concrete calls specialize both sets of parameters. In dictionary-passed code,
a generic member's slot accepts its own descriptors, bound dictionaries, and
effect arguments from the caller. An instantiated member closure retains any
trait descriptors and dictionaries it needs, so it may escape the call that
created it.

### Generic trait implementations

An implementation may itself be generic, using the same angle-bracketed
`<...>` parameter list and `where`-clause syntax as a generic `def` or
`macro`. A generic implementation applies conditionally: it is available for
any instantiation of its own compile-time parameters that satisfies its
bounds.

```staple
trait Bound T { check: T -> Bool }
trait Target T { act: T -> T }

impl Bound I32 { check = value => True }

impl <T where Bound T> Target T {
    act = value => value
}
```

Here `Target` is implemented for every `T` that implements `Bound`, so
`Target.act` is available for `I32` because of the concrete `Bound I32`
implementation above. A generic implementation's own bounds are available
inside its members' bodies, exactly as they are inside a bounded generic
`def`. `?Sized` relaxations, `<:` subtype bounds, and associated-type equality
constraints may also appear in an implementation's `where` clause, alongside
trait bounds, exactly as in a generic `def`.

Two implementations of the same trait overlap when their headers unify and
every bound of both could hold for the unified arguments; overlapping
implementations are rejected as duplicates, the same as two concrete
implementations for the same arguments. A concrete implementation and a
generic one whose bound the concrete type satisfies therefore cannot both be
declared. Two implementations whose headers unify may coexist only when a bound
rules the overlap out: `impl<T where Copy T> Clone T` and
`impl<T where Clone T> Clone List<T>` coexist because `List<T>` is never
`Copy`.

Sum headers unify as unordered sets. A sum alternative that is a bare
compile-time parameter of the implementation acts as a row: it stands for
every alternative the rest of the header leaves unmatched, which may be none
(`Never`), one, or several. For a trait `Try`, `impl<T, E> Try Result<T, E>`
therefore applies to
`Ok<I32>` (`E = Never`), to `Ok<I32> | String`, and to `Ok<I32> | String | Bool`
(`E = String | Bool`), and it overlaps a concrete implementation for any of
those types, whatever order their alternatives are written in. It does not
overlap `impl Try (Bool | String)`, which has no `Ok` alternative.

### Subtype bounds

A generic `def` or `type` may also bound a compile-time parameter with `<:`,
placed alongside trait bounds in the same `where` clause:

```staple
def string_identity = <T where T <: String> x: T => T :: x
```

`T <: SuperType` is not backed by an `impl`; it is checked against a fixed
subtyping relation instead:

- `T <: T` (reflexivity);
- `Never <: T` for every value type `T`;
- every string literal type is a subtype of `String`, and a literal type whose
  values are a subset of another literal type's values is a subtype of it;
- every non-negative integer literal type is a subtype of `USize` and
  automatically satisfies the sealed marker trait `Natural`;
- `A <: A | B` and `B <: A | B` (a type is a subtype of any union containing
  it, or containing a wider type it is a subtype of); and
- if `A <: C` and `B <: C`, then `A | B <: C` (a union is a subtype of `C`
  exactly when each of its alternatives is).

Unlike a trait bound, satisfying `T <: SuperType` does not widen the argument:
calling `string_identity "foo"` infers `T` as the literal type `"foo"`, not
`String`, so the call's result type is `"foo"`. The declared bound is still
enforced — `string_identity 1` is rejected, since `I32` is not a subtype of
`String`. `Ident`'s spelling parameter (see [Metaprogramming](#metaprogramming))
is bounded this way, combined with a default (see [Default type
parameters](#default-type-parameters)): `pub type Ident<Spelling = String
where Spelling <: String> wrap pub Spelling`.

### Default type parameters

A `type` or `trait` may give one or more of its trailing compile-time
parameters a default with `=`, written inline with the parameter. In a `type`
the default sits inside the angle brackets; a `trait`, whose parameters are
juxtaposed, parenthesizes the defaulted parameter to disambiguate the `=` from
the `{` that opens its member block:

```staple
type Box<T = String> wrap (value: T)
type Pair<A, B = A> = (A, B)
trait Increment (T = I32) { increment: T -> T }
```

The default belongs to that one parameter; it is not a separate clause. It may
be combined with a subtype or trait bound in the `where` clause, which closes
the bracketed list, as in `Ident`'s spelling parameter (see
[Metaprogramming](#metaprogramming)), which declares a default together with a
subtype bound: `pub type Ident<Spelling = String where Spelling <: String>
wrap pub Spelling`. Only a plain named parameter can carry a default — a
splice pattern in the parameter position is a parse error if followed by `=`.

At a use site, trailing arguments may be omitted as long as every parameter
from that point on has a default; the omitted arguments are filled in from
their defaults instead of leaving the type partially applied:

```staple
let boxed: Box = Box (value: "hi")       // T defaults to String
let explicit: Box<I32> = Box (value: 42)  // an explicit argument overrides the default
```

A default expression may refer to an earlier parameter in the same list —
`Pair<I32>` above supplies only `A`, and `B`'s default resolves it to `I32` too
— but not to a later one. If any parameter from the first omitted one onward
lacks a default, none of the trailing arguments are filled in and the type is
left partially applied as before. A filled-in default is still checked
against that parameter's own subtype and trait bounds, exactly as an explicit
argument would be.

Trait defaults apply the same way to under-supplied trait arguments, in both
an `impl` header and a bound clause — complementing the associated-type
based inference described above ([Traits and bounded generic
functions](#traits-and-bounded-generic-functions)). Given `trait Converts
From (To = String) { convert: From -> To }`, `impl Converts I32 { ... }`
supplies only `From` and fills `To` with `String`.

## Function application

Function application is written by placing the argument expression after the
function expression. No dedicated call punctuation is required.

```staple
println "Hello, world!"
```

A function with a plain product parameter receives the product as one argument:

```staple
printf (c_string "%s\n", CString.from_string_unchecked s)
```

Here, `printf` receives one two-element product, assuming `s` contains no NUL bytes.

Function application associates to the left, while function types associate to
the right. Nested function values can therefore define a curried API:

```staple
def add = a: I32 => b: I32 => a + b
def annotated_add: I32 -> I32 -> I32 = a => b => a + b

def add_one = add 1
add_one 2
add 1 2
```

Both calls produce `3`. `add 1` returns a function that captures the value of
`a`; functions may capture lexical values, escape their defining scope, and be
stored or passed like other values. Product-parameter functions remain the
ordinary choice when all arguments should be supplied together.

### Calls with slots

A function whose static parameter type is a slotted product takes one
juxtaposed argument per slot:

```staple
def add: (I32; I32) -> I32 = (x; y) => x + y
add 1 2 // 3
add 1   // error: incomplete juxtaposed function call
```

Each argument fills the next slot, regardless of its type. Supplying fewer
slots is an incomplete-call error; extra arguments apply to the result.
The receiver in `p |> f` fills the first slot. A plain product parameter or
an opaque generic parameter is one slot. The callee must already have a
known function type; the checker does not infer a curried or slotted shape
from its arguments.

`...expr` spreads a fixed plain or slotted product into consecutive slots.
It binds to one atomic argument, so use `f ...(g x)` to spread a call result.
A spread cannot overrun the remaining slots, and its shape must be known
when checked. `...=` is not allowed at call sites. A block argument fills
one slot and is never a spread operand.

```staple
let p = (1; 2)
add ...p
add ...(1, 2)
add p // type error; spread it into the slots: `add ...p`
```

Generic function parameters keep their declaration's slot count when
specialized. In this example `f` always accepts one opaque `A`, including
when `A` is a slotted product:

```staple
def compose = <A, B, C> (f: A -> B; g: B -> C) => a: A => C :: g (f a)
```

#### Function names are not overloaded

A function name refers to exactly one definition. Defining two `def`s with the
same name in one module or lexical scope is a duplicate definition error,
whatever their arities, and the same applies to trait members, `impl` members,
companion items and `extern` members. Extern symbols are emitted under their
declared names. Macros may still be overloaded (see
[Metaprogramming](#metaprogramming)).

## Block expressions

Braces construct a block expression:

```staple
{
    let x = 1
    x
}
```

A block may contain bindings, assignments, returns, loop control, expressions,
private `mod` and `type` declarations, and private `use` declarations. Modifier
macros may be applied to eligible declarations. Other item forms, including
`extern`, `macro`, `trait`, and `impl`, are rejected in a block. Its final item
determines the block's value: a final expression supplies its value, while an
empty block or a block ending in a non-expression supplies `()`. A semicolon
after the final expression does not discard that value.

## Match expressions

`match` exhaustively selects a pattern and produces a value:

```staple
def unwrap = result: Ok<I32> | IOError => match result {
    Ok value => value,
    IOError _ => 0,
}
```

The subject is evaluated exactly once and may have any runtime value type.
Every arm has the form `<pattern> => <expression>`. Arms are separated by
commas, and a trailing comma is permitted. Each arm has its own scope, so names
introduced by its pattern are visible only in that arm's expression. The
patterns available depend on the subject type; a binding or wildcard can match
any value.

A nominal pattern selects one sum alternative and may recursively destructure
its representation with binding, product, nominal, and wildcard patterns.
Patterns also see through one [`from`](#from) layer, so a subject whose nominal
type is a `from` wrapper is matched by patterns for its representation. A
bare singleton name selects its unique alternative, including the
standard-library boolean values:

```staple
match value {
    True => "yes",
    False => "no",
}
```

Bare names which do not resolve to singleton values remain binding patterns.
An annotation or `mut` always makes a name a binding. The explicit empty
representation forms `True()` and `False()` remain accepted.

A binding pattern at the root is a catch-all and binds the complete subject value.
`_` is a wildcard pattern which matches without binding a name; wildcards may
also be used in function parameters and destructuring bindings.

Product subjects are matched structurally. Their element patterns may select
sum alternatives, and coverage is checked across every possible combination:

```staple
def same = (left: Bool, right: Bool) => match (left, right) {
    (True, True) => True,
    (False, False) => True,
    _ => False,
}
```

A match must cover every possible sum alternative and product combination or
include a catch-all. Duplicate or otherwise redundant arms are errors. Literal
patterns, alternative patterns, and match guards are not currently supported.

An expected type is applied to every arm. Without one, equal arm types remain
that type; differing represented nominal results are joined into an sum by
the same rules used for inferred function results. Arms which return from the
enclosing function do not contribute to the match value type. If every arm
returns or otherwise has type `Never`, the match has type `Never` and does not
continue normally.

The standard prelude supplies two conditional forms, both implemented as macros.

`when` is a braced, clause-oriented form:

```staple
when {
    first_condition => first_branch,
    second_condition => second_branch,
    else => fallback,
}
```

It evaluates conditions from top to bottom and evaluates only the branch
belonging to the first true condition. Its optional final `else` clause supplies
the fallback; without one, the fallback is `()`. An `else` clause must be last,
and branch types join normally.

`if` is an infix form: a condition, a branch, then any number of
`else if` arms and an optional trailing `else`:

```staple
if first_condition { first_branch }
else if second_condition { second_branch }
else { fallback }
```

Each branch is an ordinary `Expr`, so `if ready { run () } else 0` is also
valid — braces are just the usual block expression. The condition is a single
atomic argument: a compound condition is parenthesised, `if (count > 0) { ... }`,
the same convention `while` and `for` use. Without a trailing `else` the
fallback is `()`. It lowers to the same first-true-branch semantics as `when`,
and branch types join normally.

## Return statements

`return` immediately exits the nearest enclosing function with the value of
its required expression:

```staple
def answer = () => {
    return 42
    0 // unreachable
}
```

Here `answer` returns `42`. Returning unit is written explicitly as `return ()`;
`return` is not permitted outside a function.

## Loop expressions

`loop` repeatedly evaluates a block and is itself an expression. Falling through the
body or executing `continue` starts the next iteration:

```staple
def answer = () => loop {
    break 42
}
```

`break value` exits the nearest enclosing loop and supplies its result. `break`
without a value supplies `()`; a newline, semicolon, or closing brace terminates
the unit form. The type of a loop is the join of the values supplied by all
reachable breaks targeting that loop, using the same result-joining rules as
functions and match expressions. Breaks targeting nested loops do not
contribute. A break whose value has type `Never` is itself unreachable and also
does not contribute. A loop with no reachable break has type `Never` and does
not continue normally.

`break` and `continue` cannot cross a function boundary, and labeled loops are
not supported. Values owned by an iteration are dropped before a break,
continue, or implicit next iteration; a value moved out by `break` becomes the
loop result and is preserved.

`std.core` supplies a `while` form implemented in `std.flow` using
`loop`, `break`, and a boolean match:

```staple
while condition body
```

The condition is evaluated before every iteration, and the loop returns `()`
when it becomes `False`.

## Coroutines

A coroutine is a computation that can **pause in the middle and resume
later**. Use one whenever a piece of logic has to wait — for the next frame, a
timer, a reply from the host, a change to a signal, or another job — and you
want to write it as a straight sequence of steps rather than as a state
machine or a chain of callbacks.

For example, a game tutorial that waits for the player is three steps written
in order:

```staple
use std.coroutine.*
use std.io.(IO, println)

let signal jumps = 0
let signal coins = 0

def tutorial = () => Coroutine{Reactive, IO}<()> :: coro {
    println "Press the jump button."
    await (until { jumps > 0 })
    println "Nice! Now collect a coin."
    await (until { coins > 0 })
    println "Tutorial complete."
}
```

Each `await` parks the tutorial until its condition holds; the rest of the
program keeps running in the meantime. Without coroutines the same logic would
be a `step` variable plus a per-frame `match` on it, and every new step would
have to be threaded through that state by hand.

Typical uses are:

- **Scripted behaviour** — cutscenes, AI routines, tutorials, and animations
  that wait for frames, time, or game state (see [`until`](#until)).
- **Waiting on the outside world** — a request whose answer arrives later
  from the host, a timer, or an event (see [Completions](#completions)).
- **Running several jobs at once** — interleaving independent jobs on one
  thread, waiting for their results, and cancelling the ones no longer needed
  (see [Coroutines and tasks](#coroutines-and-tasks)).

Everything is cooperative and single-threaded: a coroutine runs until it
reaches an `await`, and nothing runs at all until a program explicitly drives
it. `std.coroutine` declares the two compiler-known types; import them (or
`std.coroutine.*`) to use them:

```staple
pub type Coroutine{E}<T> opaque
pub type Task<T> opaque
```

### Building and awaiting coroutines

`coro { block }` builds a `Coroutine{E}<T>`. The braces delimit a block whose
tail expression is the coroutine's result `T`, and `E` records the effects the
body will perform (see [Deferred effects](#deferred-effects)). Building a
coroutine **does not run it**: a coroutine value is a description of work, much
like a function that has not been called yet.

`await expression` runs a coroutine as part of the current one, pausing the
current coroutine wherever the awaited one pauses, and yields its result.
Coroutines therefore compose the way functions do:

```staple
use std.coroutine.(Coroutine, block_on)
use std.io.(IO, println)

def greet = name: String => Coroutine{IO}<()> :: coro {
    println "hello, ${name}"
}

def add_later = (a: I32, b: I32) => Coroutine<I32> :: coro { a + b }

def main_flow = () => Coroutine{IO}<I32> :: coro {
    await (greet "Ada")
    let x = await (add_later (1, 2))
    let y = await (add_later (x, 10))
    y
}

let job = main_flow ()
println "built, nothing printed yet"
let total = block_on job          // prints "hello, Ada"
println "total = ${total:?}"      // total = 13
```

`std.coroutine.block_on` is the simplest way to run a coroutine: it runs it to
completion on the current thread and yields its result. It is only for
coroutines that never park themselves on a scheduler — `yield_now`, `until`,
awaiting a `Task`, or awaiting a `Wait` all need a [scheduler](#schedulers-and-pumping).

`await` binds a call or postfix expression — `await run ()` is
`await (run ())` — and a larger operand must be parenthesized, as in
`await (a + b)`. Both `coro` and `await` are contextual keywords: an
identifier named `coro` or `await` is still valid everywhere the keyword form
does not apply.

### Coroutines and tasks

`Coroutine` and `Task` are the two halves of running concurrent work:

| | `Coroutine{E}<T>` | `Task<T>` |
| --- | --- | --- |
| What it is | A description of work that has not started | A handle to work that is already scheduled |
| Created by | `coro { … }` (or a function returning one) | `spawn coroutine` |
| Runs | Only when something `await`s or `spawn`s it | Independently, whenever its scheduler is pumped |
| `await` it | Runs it *inline*: the awaiter continues only once it finishes | Waits for the already-running task to finish |
| `await` yields | `T` | `Completed<T> \| Cancelled` |
| Can be cancelled | No — drop it instead and it never runs | Yes, with `Task.cancel` |

The difference shows up as soon as two pieces of work pause. Here `count`
prints a line, then gives other tasks a turn with `yield_now`:

```staple
use std.coroutine.*
use std.io.(IO, println)

def count = (name: String, n: I32) => Coroutine{Tasks, IO}<()> :: coro {
    for i in (1 ..= n) {
        println "${name} ${i:?}"
        await (yield_now ())
    }
}

// Awaiting coroutines runs them one after another, inside one task.
def one_after_another = () => Coroutine{Tasks, IO}<()> :: coro {
    await (count ("a", 2))
    await (count ("b", 2))
}

def run_all = sched: Scheduler => {
    while ((pump (sched, 64)).ready > 0) {}
}

let sched = scheduler ()
with Tasks = task_scope (sched) {
    let _ = spawn (one_after_another ())
    run_all sched                     // a 1, a 2, b 1, b 2

    let _ = spawn (count ("a", 2))    // two separate tasks...
    let _ = spawn (count ("b", 2))
    run_all sched                     // ...interleave: a 1, b 1, a 2, b 2
}
```

Awaiting a coroutine is like calling a function: the caller waits for the
callee. Spawning is like starting a thread: the spawner gets a handle back
immediately and both make progress. Reach for `await (some_coroutine ())` to
split a behaviour into helpers, and for `spawn` when work should proceed on its
own, be observed, or be cancelled.

Because a task can be cancelled before it produces a value, awaiting a `Task<T>`
yields `Completed<T> | Cancelled` rather than a bare `T`. Awaiting a
`Coroutine` cannot be interrupted that way, so it yields `T` directly.

### Schedulers and pumping

A **scheduler** owns a queue of ready tasks, and nothing runs until the
program **pumps** it. This puts the host — typically a game or UI loop — in
charge of when coroutine code executes.

```staple
use std.coroutine.*

let sched = scheduler ()
with Tasks = task_scope (sched) {
    let handle = spawn (worker ())
    let progress = pump (sched, 64)   // (executed, ready)
}
```

- `scheduler ()` creates a scheduler. Several schedulers can coexist — for
  example one for per-frame updates and one for fixed-rate physics ticks.
  Destroying a scheduler closes its scopes and cancels their tasks.
- `with Tasks = task_scope (sched) { ... }` opens an affine task scope bound to
  `sched` that provides the implicit `Tasks` capability, exactly as
  `with Reactive = ...` provides `Reactive`. `spawn` requires it, so a function
  or coroutine that spawns lists `Tasks` in its effects. Normal exit closes the
  task scope. `return`, `break`, and `continue` close every task scope opened
  since their target, in reverse order, just as they dispose reactive scopes. A
  `return` targets the function exit, or the end of the enclosing `coro`
  body; `break` and `continue` target their loop.
  Closing a scope cancels its unfinished tasks (see
  [Cancellation](#cancellation)).
- `spawn coroutine` consumes the coroutine, schedules it on the current scope's
  scheduler, and returns a `Task<T>` handle. It also requires the coroutine's own
  effects `E` at the spawn site, so its resources are captured then. Dropping
  the handle does not cancel the task.
- `pump (sched, limit)` runs a bounded snapshot of the ready tasks and returns
  `(executed, ready)`: how many resumes it performed and how many tasks are
  still ready. Tasks that suspend, and tasks spawned during the pump, become
  eligible only for a **later** pump; carried-over work is pumped ahead of newly
  spawned work. A host usually pumps once per frame or tick, or loops until
  `ready` is zero, as `run_all` does above.
  The `limit` bounds *resumes*, not instructions — a coroutine that loops
  forever without an `await` still hangs the pump. A `pump` while another
  `pump` is running, while a reaction is executing, or inside an open `batch`
  traps: a reaction must never drive coroutines, and a batch means to defer
  effects a pump would run.
- `await (yield_now ())` suspends the current task until a later pump. Use it
  to spread long work across frames or to let other tasks take a turn.
- `Task.is_finished handle` reports whether the task has finished (completed or
  cancelled) without consuming the handle.
- Dependency tracking is detached around every coroutine resume: a signal read
  inside a coroutine body never subscribes an enclosing reaction, so driving a
  coroutine from a `derived` or reaction computation does not entangle the two.

### Waiting for a task

`await handle` inside a coroutine suspends the current task until the task
behind `handle` finishes, then yields `Completed value` or `Cancelled`. A
coroutine can start several tasks, carry on with its own work, and collect the
results later:

```staple
use std.coroutine.*
use std.io.(IO, println)

def slow_square = n: I32 => Coroutine{Tasks, IO}<I32> :: coro {
    for _ in (0 .. 3) {
        await (yield_now ())
    }
    n * n
}

def supervisor = () => Coroutine{Tasks, IO}<()> :: coro {
    let kept = spawn (slow_square 3)
    let dropped = spawn (slow_square 4)
    println "both spawned; supervisor keeps going"
    await (yield_now ())
    Task.cancel dropped               // no longer needed

    let a = await kept
    match a {
        Completed value => println "kept: ${value:?}",       // kept: 9
        Cancelled() => println "kept: cancelled",
    }
    let b = await dropped
    match b {
        Completed value => println "dropped: ${value:?}",
        Cancelled() => println "dropped: cancelled",         // this one
    }
}
```

The awaiting task registers itself as the target's sole waiter and is
re-queued on the target's own scheduler when it finishes, so a pump loop makes
progress even across an `await` on another task. `await` consumes the handle:
awaiting the same handle twice is a use-after-move. Driving a task-awaiting
coroutine with `block_on` traps; it needs a real pump loop.

### Cancellation

`Task.cancel handle` requests that a task stop. It borrows the handle, so the
task can still be awaited afterwards to observe `Cancelled`, as `supervisor`
does above.

- Cancellation is idempotent, and a no-op once the task has finished.
- A task that has not started yet never runs its body.
- A running task is not interrupted mid-step. It unwinds at its **next**
  `await` / `yield_now` / return boundary — dropping the frame locals it has
  initialised — rather than delivering a value. `is_finished` turns true only
  once the task has actually been driven through that unwind: `cancel`
  re-queues the task, and a `pump` completes it.
- Closing a `with Tasks` scope cancels every task still running under it,
  youngest first, and drives each through its unwind before the block exits,
  so teardown is deterministic. Destroying the scheduler is not required for
  this. Giving an object its own task scope, or keeping its task handles and
  cancelling them when it is destroyed, makes all of its behaviours stop with
  it.

Some cases are only partly handled today:

- A task cancelled while it is awaiting a plain **child coroutine**
  (`yield_now`, an ordinary `coro`) lets that child run to completion before
  the parent unwinds. The exception is `until`, whose subscription *is* torn
  down at the cancellation boundary.
- A parent task that finishes while it still has unfinished child *tasks*
  leaves them for the scope to clean up rather than cancelling them eagerly.
- Drop-needing captures of a coroutine cancelled *after* it started running are
  reclaimed by the collector rather than dropped at the unwind.

### Completions

A `completion` connects a task to something outside the scheduler. It is a
one-shot pair: a `Wait<T>` that a task awaits, and a `Resolver<T>` that anyone —
the host, a timer, an event handler — uses to deliver the value later.

```staple
use std.coroutine.*
use std.io.(IO, println)

// `completion`'s `T` is not inferred from the scheduler alone; give it a typed home.
def name_request: Scheduler -> (wait: Wait<String>, resolver: Resolver<String>) =
    s => completion s

def greet_user = move wait: Wait<String> => Coroutine{IO}<()> :: coro {
    println "waiting for the name..."
    let outcome = await wait
    match outcome {
        Completed name => println "hello, ${name}",
        Cancelled() => println "request failed",
    }
}

let sched = scheduler ()
with Tasks = task_scope (sched) {
    let (wait, resolver) = name_request sched
    let _ = spawn (greet_user wait)

    let _ = pump (sched, 64)          // waiting for the name...
    Resolver.complete resolver "Ada"  // the reply arrives
    let _ = pump (sched, 64)          // hello, Ada
}
```

- `completion sched` returns an affine `Wait<T>` and `Resolver<T>`, both bound to
  `sched`. Inference needs `T` from context: annotate the binding or wrap the
  call in a typed helper, as `name_request` does.
- `await wait` consumes the `Wait` and yields `Completed<T> | Cancelled`,
  exactly like `await` on a `Task`. It may only be awaited from a task **on the
  same scheduler**; doing otherwise traps.
- `Resolver.complete resolver value` consumes the resolver, stores the value,
  and wakes a registered waiter on the next pump. Completing *before* the
  `await` registers is fine: the value is stored, and the `await` returns it in
  the same resume without suspending.
- `Resolver.cancel resolver`, and dropping an unresolved resolver, wake the
  waiter with `Cancelled`. Dropping an unconsumed `Wait` abandons the
  operation; a later `complete` then finds the consumer gone and drops the
  value.
- Cancelling a task that is parked on a `Wait` unwinds it at that boundary
  (like any await); a subsequent `complete` on that completion is a no-op.
- `completion_with_cancel (sched, on_cancel)` attaches an owned `() -> ()`
  callback, which may not suspend. It runs **exactly once** if the consumer
  abandons the wait before it resolves — the `Wait` is dropped unconsumed, or a
  task parked on it is cancelled. A successful `complete` / `cancel` releases
  the callback without running it. Libraries use it to unregister a timer or
  an external subscription when nobody is waiting any more.
- `completion_token sched` returns `(Wait<()>, CompletionToken)` for host
  integration. The `CompletionToken` is an opaque, C-compatible handle a host
  holds across the FFI boundary and wakes with the C entry points
  `__staple_completion_token_resolve` / `_cancel` / `_release` (unit-valued;
  called on the runtime thread — the host marshals worker-thread notifications
  first). From Staple, `CompletionToken.resolve` / `.cancel` drive it, and
  dropping it releases it, which cancels an unresolved completion.

### `until`

`until { predicate }` returns a `Coroutine{Reactive} ()` that suspends the
current task until `predicate` first observes `True`. It turns any condition
over [signals](#signals-and-reactions) into something a task can wait for — the `tutorial`
example at the top of this section is built from it:

```staple
let signal health = 3

def death_watch = () => Coroutine{Reactive, IO}<()> :: coro {
    await (until { health <= 0 })
    println "game over"
}
```

Spawned under a `with Reactive` scope, `death_watch` stays parked while the
program lowers `health`, and prints once it reaches zero.

- The predicate may only **read signals** — no writes, other effects, or
  suspension, as enforced by its `() ->{state.read} Bool` type.
- It is evaluated synchronously once when the coroutine runs (an
  already-`True` predicate proceeds without suspending), then again on each
  change to a signal it read, refreshing its dependencies each time. Only
  committed states are observed, so a transient `True` inside a `batch` is not
  an event.
- The first `True` latches, unsubscribes, and resumes the task. Cancelling the
  task while it is parked in `until` tears the subscription down at that
  boundary. `until` may be awaited through intervening helper coroutines — it
  finds its scheduler by walking the parent chain to the spawned task.
- Implementation caveat: each `until` leaks a small reaction record (the
  subscription is cleared but the struct is not freed); a proper
  executor-cooperative detach is a follow-on.

### Deferred effects

`coro { … }` is a pure construction expression. Its body's effects are
*deferred*: they are recorded on the coroutine's effect row `E` instead of
being contributed to the enclosing function, and they happen only when the
coroutine runs:

```staple
use std.io.(IO, println)

// Building the coroutine is pure; the `IO` effect rides on the type.
def build = () => Coroutine{IO}<()> :: coro { println "hi" }
```

Omitting the row is the empty set, exactly as for an effect-parameterized type
declaration: `Coroutine<T>` is `Coroutine{}<T>`.

When a `coro` is checked against an expected `Coroutine{E}<T>`, such as a
declared result type, `E` is an upper bound on the body's effects, exactly as a
function declaration's effect row bounds its body. The coroutine takes the
declared row, so starting or awaiting it supplies every effect in `E` even if
the body uses fewer. A body that needs an effect outside `E` is an error:

```staple
// Accepted: the body is pure, but the declared row may be larger.
def quiet = () => Coroutine{IO}<I32> :: coro { 5 }

// Rejected: the body prints, but the declared row is empty.
def loud = () => Coroutine<I32> :: coro {
    println "x"
    5
}
```

Without an expected type, or when the expected row is still an effect variable
to infer (as for a generic parameter such as `block_on`'s), the row is exactly
the body's inferred effects.

The row comes due wherever the coroutine is started:

- `await` on a `Coroutine{E}<T>` folds `E` into the awaiting body's own row, so
  `def run = () => Coroutine{IO} () :: coro { await (build ()) }` needs `IO`.
- `spawn` requires `E` at the spawn site, alongside `Tasks`.
- `block_on` requires `E` from its caller.

Deferred effects are discharged against the resources of whoever drives the
coroutine.

### Rules for `await` and ownership

**Where `await` may appear.** `await` is allowed only directly inside a `coro`
body. An ordinary nested function does not inherit that permission, and `await`
is rejected inside reactions, derived bindings, batching callbacks, and
compile-time evaluation.

Within the body, `await` must be a **statement** — `let x = await e` or a bare
`await e` — including inside `match` arms and `loop` bodies, but not as a
sub-expression (`f (await a)`, `await a + await b`, `match await t { … }`).
Bind each `await` to a `let` and combine the results afterwards:

```staple
// Rejected: `let total = await a + await b`
let x = await a
let y = await b
let total = x + y
```

**Returning.** A `coro` body runs as its own function, so `return value`
inside it finishes the coroutine with `value`, and a failing `?` finishes it
with the residual alternative; neither leaves the function that built the
coroutine. Its owned values and frame bindings are dropped exactly as when the
body ends normally. `break` and `continue` cannot leave a `coro` body, though a
loop written entirely inside it may use them:

```staple
def first_positive = n: I32 => Coroutine{}<I32> :: coro {
    if (n > 0) { return n }
    let fallback = await (default_value ())
    fallback
}
```

**Ownership.** `Coroutine`, `Task`, `Wait`, and `Resolver` values are affine.
Building a coroutine copies its `Copy` captures and moves its owned non-`Copy`
captures; a captured borrowed view is rejected, so a function that builds a
coroutine around a non-`Copy` parameter takes it with `move`, as `greet_user`
does above. `await` consumes its operand, so awaiting the same value twice is
a use-after-move. Dropping a coroutine that was never awaited or spawned
destroys its captures without running the body.

### How coroutines execute

Each coroutine `resume` is a state machine: it `switch`es on the frame's resume
state to the point after its last `await`. A coroutine's body-local `let`
bindings live in its GC-rooted frame so they survive a suspension; `await`
records the point, hands the awaited child to the driver, and returns. The
driver trampolines the parent chain, so an arbitrarily deep nest of `await`s
costs no native stack. The frame holds the coroutine's moved captures until it
completes or is dropped, and completion and drop each run a deterministic
cleanup exactly once.

### Worked examples

`staple-compiler/examples/coroutines.sta` is a runnable walkthrough of the
manual scheduler surface — a `scheduler`, a `with Tasks` scope, a
producer/consumer pair joined by `await`, a `completion` resolved from outside
the scheduler, `Task.cancel`, and scope teardown.

`staple-compiler/examples/game_loop/` builds a small host adapter — per-frame
and fixed-tick waits, scaled and unscaled clocks with deadline sleeps, separate
schedulers for update and fixed update, and entity-owned behaviours cancelled
on destruction — entirely on `until` and the other primitives above, with no game concept in the compiler or the executor. It is
the reference for wiring a real update loop to Staple coroutines.

## Iteration and ranges

The prelude's consuming iterator protocol returns the successor iterator state
with every step. `Iter` determines its associated `Item`:

```staple
pub mod IterStep {
    pub type Done<Iter> wrap pub Iter
    pub type Yield<Item, Iter> wrap pub (Item, Iter)
}

pub type IterStep<Iter, Item> =
    IterStep.Done<Iter> |
    IterStep.Yield<Item, Iter>

pub trait Iterator Iter {
    type Item
    next: move Iter -> IterStep<Iter, Item>
}

pub trait IntoIterator Source {
    type Iter where Iterator Iter
    into_iterator: move Source -> Iter
}
```

`IterStep.Done iterator` retains the terminal state, while
`IterStep.Yield (item, iterator)` contains an item and the state used for the
next call. `next` consumes the iterator it advances and returns the successor
state inside the step. `Done` and `Yield` remain inside the `IterStep`
namespace. An `IntoIterator` implementation consumes its source and selects
one default iterator type; wrapper types can provide alternative iteration
modes. The `move` marker makes that consumption explicit: a non-`Copy` source
such as a `List<T>` is moved into the iterator, so the binding cannot be used
again, while a `Copy` source such as a range is copied and remains usable.
Clone a non-`Copy` source before iterating to keep the original.

`for` is a prelude macro which accepts any `IntoIterator` source:

```staple
for item in items {
    consume item
}

for (key, value) in pairs {
    consume (key, value)
}
```

The source is evaluated once. Each yielded item is matched against the binding
pattern in a fresh iteration scope. The successor iterator is installed before
the body, so `continue` advances normally. `break` exits the iteration and
`return` exits the enclosing function. Normal exhaustion produces `()`, making
the complete `for` expression unit-valued.

Function application binds more tightly than operators, so a compound
iterator expression must be parenthesized as one macro argument. In particular,
ranges are written:

```staple
for index in (0 .. 10) { consume index }
for index in (0 ..= 10) { consume index }
```

`start .. end` excludes `end`; `start ..= end` includes it. Both are fixed,
non-associative operators at precedence 3 (see
[Values and expressions](#values-and-expressions)) that desugar to the
prelude's `range`/`range_inclusive` functions and evaluate their endpoints
once. Standard iteration is available for every signed, unsigned, and
pointer-sized integer
type. Ranges advance by one, and a range whose start is greater than its end is
empty. Inclusive iteration yields a maximum integer endpoint without computing
an overflowing successor. `Range<T>` and `RangeInclusive<T>` expose their
representations so custom concrete iterator implementations can use them.

### Iterator operations

Every `Iterator` has the following operations as
[blanket companion](#blanket-companions) members, declared in
`std.iterator`. Each takes the iterator in its first slot, so it reads as a
pipeline:

```staple
let total = (0 .. 10) |> filter (x => x > 4) |> map (x => x * 2) |> fold 0 ((sum, x) => sum + x)

for (index, word) in (words |> into_iterator |> enumerate) {
    println "${index}: ${word}"
}
```

The receiver must be an `Iterator`. Convert another `IntoIterator` source, such
as a `List`, with `|> into_iterator` first. Being members rather than free
functions, the operations take no names from a program: a module may define
its own `count` or `map` and still pipe into the iterator's. Outside a pipe,
name one through an iterator type, as in `Range.sum (1 .. 5)`.

*Adapters* are lazy: each returns a new iterator that owns the one it was given
and does its work only when advanced.

| Operation | Arguments after the iterator | Yields |
|---|---|---|
| `map` | `f: move Item -> B` | `f` of each item |
| `filter` | `keep: Item -> Bool` | the items `keep` accepts |
| `filter_map` | `f: move Item -> Option<B>` | the values `f` returns as `Some` |
| `take` | `count: USize` | at most the first `count` items |
| `skip` | `count: USize` | the items after the first `count` |
| `take_while` | `keep: Item -> Bool` | items up to the first one `keep` rejects |
| `enumerate` | — | `(index, item)` pairs, counting from `0` |
| `zip` | `move right: R` | `(item, right_item)` pairs, until either side runs out |
| `chain` | `move second: R` (same item type) | the items of the iterator, then those of `second` |
| `flat_map` | `f: move Item -> J` (`Iterator J`) | the items of each iterator `f` returns |
| `scan` | `move init: S; f: (move S, move Item) -> (S, Option<B>)` | each `Some` value, stopping at `None` |
| `skip_while` | `keep: Item -> Bool` | the items after the first one `keep` rejects; that item is kept |
| `step_by` | `count: USize` | the first item, then every `count`th item |
| `peekable` | — | the same items, with `Peekable.peek` to inspect the next one |

*Consumers* drive the iterator to the end, or until they have their answer,
and return a value.

| Operation | Arguments after the iterator | Returns |
|---|---|---|
| `fold` | `move init: A; f: (move A, move Item) ->{E} A` | the final accumulator |
| `for_each` | `f: move Item ->{E} ()` | `()` |
| `reduce` | `f: (move Item, move Item) ->{E} Item` | `Option<Item>`, `None` when empty |
| `count` | — | `USize` |
| `any` / `all` | `test: Item ->{E} Bool` | `Bool`; an empty iterator satisfies `all` |
| `find` | `test: Item ->{E} Bool` | the first matching item, or `None` |
| `position` | `test: Item ->{E} Bool` | the first matching index as `Option<USize>` |
| `nth` | `index: USize` | the item at that index, or `None`; earlier items are dropped |
| `last` | — | `Option<Item>` |
| `try_fold` | `move init: A; f: (move A, move Item) ->{E} R` (`Try R`, `Try.Output R == A`) | `Result<A, Try.Residual R>`; a failure stops the fold |
| `collect` | — (target from the expected type; `where Default C, FromIterator C Item`) | the collected target |
| `to_list` | — | a new `List<Item>` |
| `extend` | `move target: C` (`where FromIterator C Item`) | `target` with every item added |
| `sum` | — (`where Add Item, Default Item`) | the total, starting from `Default.default ()` |
| `min` / `max` | — (`where Ord Item`) | `Option<Item>`; the first smallest, the last largest |
| `min_by` / `max_by` | `compare: (Item, Item) ->{E} Ordering` | `Option<Item>`; the first smallest, the last largest |

Source constructors are free functions in `std.iterator`:

| Constructor | Arguments | Yields |
|---|---|---|
| `empty` | `<T> ()` | no items |
| `once` | `move value: T` | `value` once |
| `repeat_n` | `move value: T; count: USize` (`Clone T`) | `count` copies of `value` |
| `successors` | `move first: Option<T>; next: T -> Option<T>` | `first`, then each successor until `next` returns `None` |

`peekable` is a consuming adapter like the others, so `Peekable.peek` returns
the next item together with the iterator that still holds it:
`(Option<Item>, Peekable<Iter, Item>)`.

**Collecting.** `collect` is generic in its target: the expected type chooses
it. It starts from `Default.default ()` and adds the items with
`FromIterator.insert_all`, which by default inserts them one at a time with
`insert`. The trait takes the target and the item type as separate
parameters, so one target can accept more than one item type:

```staple
pub trait FromIterator Target Item {
    insert: (move Target; move Item) -> Target
    insert_all: <Iter where Iterator Iter, Iterator.Item Iter == Item>
        (move Target; move Iter) -> Target = ...
}

let numbers: List<I32> = (1 .. 4) |> map (x => x * 10) |> collect
let words: String = (1 .. 4) |> map (x => "w${x}") |> collect
```

`List<T>` collects its items in order, and `String` concatenates string pieces.
`String` overrides `insert_all` to build its bytes in one pass, since each
`insert` copies the text built so far. Override it for any target whose
`insert` is expensive. In generic code, state the bound the target needs,
such as `FromIterator C (Iterator.Item Iter)` or `Iterator.Item Iter == I32`
for a `List<I32>`.

With no expected type to choose a target, use `to_list`; `extend` adds the items
to an existing value instead of starting from a default:

```staple
let values = (1 .. 4) |> map (x => x * 10) |> to_list
let extended = (1 .. 4) |> extend values
```

A new target implements the trait, starting from whatever `Default` provides:

```staple
type Count wrap USize

impl Default Count { default = () => Count 0 }

impl FromIterator Count I32 {
    insert = (move Count total; move item) => Count (total + 1)
}
```

**Effects.** `Iterator.next` is pure, so an adapter's function must be pure
too: an adapter runs it when the iterator is advanced, perhaps far from where
the pipeline was built. Consumers run immediately, so their functions may have
any effect set `E`, which the call then requires. Put effects in the consumer:

```staple
(1 .. 4) |> map (x => x * x) |> for_each (square => println "${square}")
```

A closure that mutates a captured `let mut` has the `state` effect and is
therefore rejected by adapters as well. Keep such state in a consumer's
accumulator instead, as `fold` does.

**Ownership.** Every operation takes the iterator by `move`. Functions that
transform an item (`map`, `filter_map`, `flat_map`, `scan`, `fold`,
`for_each`, `reduce`, `try_fold`) receive it by `move`. Functions that only
test an item (`filter`, `take_while`, `skip_while`, `any`, `all`, `find`,
`position`, `min_by`, `max_by`) borrow it, so the operation can still yield or
return it, and this works for non-`Copy` items. `take_while` drops the first
item it rejects. `skip_while` yields the first item its predicate rejects.
`zip` drops an item it has taken from the left side when the right side has
run out. Consumers that stop early (`any`, `all`, `find`, `position`, `nth`,
and `try_fold` on failure) drop the rest of the iterator. Source constructors
take their functions by value as well, since they run while the iterator is
advanced.

**Adapter types.** Each adapter has its own result type (`Map<Iter, B>`,
`Filter<Iter, Item>`, `Take<Iter, Item>`, and so on). Its closure is not part of
that type, so two `map` pipelines over the same kind of source have the same
type. `chain` accepts different iterator types with equal item types, and returns
`Chain<L, R, Item>`:

```staple
(0 .. 3) |> chain ((3 .. 6) |> filter (x => x > 3)) |> to_list
// contains 0, 1, 2, 4, 5
```

## Types

Named types are written as identifiers:

```staple
I8
I16
I32
I64
U8
U16
U32
U64
ISize
USize
F32
F64
Never
Bool
String
Shared T
SharedBuffer T
Buffer T
CString
CChar
RawPtr CChar
Ordering
Option T
```

The numeric types, `Never`, `Bool`, `String`, `ToString`, and their functions
from `std.core` are imported implicitly into every source module. The comparison
traits `Eq`, `PartialOrd`, and `Ord` and the `Ordering` result live in
`std.cmp` and require explicit imports, although comparison operator syntax
works without those imports. These are
ordinary type names rather than keywords, so a local declaration or explicit
import can shadow a prelude name. Integer literals use an expected integer type
when one is available and otherwise default to `I32`, unless a type suffix names
their type. Mixed-type arithmetic is not implicit.

`Option<T>` is declared in `std.option` as the sum `None | Some<T>`, where
`None` is a singleton type and `Some<T>` is a `from` type; `std.core` re-exports
all three. Because `Some` is a [`from`](#from) type, a value converts implicitly
wherever an `Option` is expected:

```staple
def head = (values: List<I32>) => Option<I32> :: List.get values 0
let port: Option<I32> = 8080   // implicitly `Some 8080`
```

As with `Ok`, an exact alternative wins over the `from` conversion, so a bare
`None` selects the `None` alternative rather than `Some<None>`.

`Never` is the uninhabited bottom type. No expression can normally produce a
`Never` value, and `Never` is a subtype of every value type, so a divergent
expression can appear wherever another value type is expected. Joining
`Never` with another type yields the other type; a join containing only
`Never` remains `Never`.

`ToString` has the member `to_string: T -> String` and is implemented for all
integer and floating-point types, `Bool`, and `String`. Integers use decimal
notation, booleans produce `"True"` or `"False"`, and converting a `String`
returns it unchanged. Floating-point values use round-trip decimal notation.

`Display` and `Debug` format values into a mutable `Formatter`. Their common
protocol is `fmt: (T, mut Formatter) -> ()`: `Display` is intended for
human-facing text, while `Debug` is intended for developer-facing structure.
These formatting types and traits are declared in `std.fmt`; `Display` and `Debug` are re-exported
by `std.core` into the prelude.
`Formatter.display value` and `Formatter.debug value` create a formatter,
dispatch the corresponding trait, and return the finished `String`. A
`Formatter` is a nominal type with the private representation `List<U8>`; the
finished string takes over the list's storage without copying, including any
spare capacity.

Manual implementations append text with `Formatter.write` and may delegate
nested values directly to either formatting trait:

```staple
type Point wrap (x: I32, y: I32)

impl Debug Point {
    fmt = (Point (x, y), formatter) => {
        Formatter.write formatter "Point "
        Debug.fmt ((x: x, y: y), formatter)
    }
}
```

Products and sums have compiler-provided structural `Debug` implementations
whenever all their elements or alternatives implement `Debug`. Products use
parentheses and comma-separated fields; sums delegate directly to the active
alternative. Nominal types do not inherit `Debug` from their representation,
but the source-defined stdlib modifier `@derive_debug` generates an
implementation that prefixes the type name and delegates to the representation.
Scalar `ToString` implementations delegate to `Display`.

Fixed products likewise have a compiler-provided structural `Default`
implementation; see [Element access](#element-access). Nominal types do not
inherit it, but `@derive_default` in `std.default` generates one from the
representation, using its [default field values](#default-field-values). A
bodyless singleton type's default is the singleton itself. A generic type's
implementation requires `Default` only for the field types that use
`Default.default ()`.

Fixed-arity products and sums likewise have compiler-provided structural `Eq`
implementations whenever all their elements or alternatives implement `Eq`.
Products compare their elements in order, stopping at the first unequal pair.
Two sum values are equal when they hold the same alternative and that
alternative's `Eq` reports the payloads equal. `Bool` is the sum
`True | False`, so its equality is derived this way from the singleton
`Eq True` and `Eq False` implementations.

`String` is an immutable garbage-collected UTF-8 byte sequence. The standard
library declares it as a nominal type with the private representation
`SharedBuffer<U8>`. At runtime it is one pointer to a managed header holding
the byte length, a capacity, and the bytes; there is no separate descriptor
allocation. The capacity is at least the length: a string built from a list
keeps the list's spare capacity, which is never used. Copying a `String` copies the pointer and
shares the bytes.
Two strings can be concatenated with `+`; this dispatches through the standard
`Add String` implementation and returns a newly allocated `String`. The
standard library implements `Eq String`, comparing the two UTF-8 byte
sequences.

`String.to_bytes: String => List<U8>` copies the UTF-8 bytes into a new list, so
mutating the list never affects the string. `String.bytes: String => StringBytes`
returns a non-copying iterator over the bytes: `StringBytes` implements
`Iterator` with `Item = U8` and an identity `IntoIterator`, so
`for byte in (String.bytes text)` works. `String.from_utf8: move List U8 =>
Result String std.string.InvalidUtf8` consumes the list, takes over its
storage without copying through `Buffer.into_shared_buffer`, and validates the
bytes; because the list is moved in, nothing can mutate the bytes
after validation. `String.from_utf8_unchecked: move List<U8> => String` skips the
validation when the caller knows the bytes are valid UTF-8. The underlying
`SharedBuffer<U8>` is private to the standard library.

`StringBuilder` is the mutable UTF-8 buffer for constructing strings. It is
available from the prelude and owns its allocation uniquely; it is not `Copy`.
`new ()`, `with_capacity bytes`, and `from_string text` create builders.
`length` and `capacity` count bytes. `push` appends one `Char`, `append` copies
one `String` with a bulk byte copy, and `append_display` formats a `Display`
value and appends the resulting text. `pop` removes the last scalar,
returning `None` when empty. `clear` retains the allocation. `finish` consumes
the builder and returns a `String` without copying; `to_string` instead copies
the contents and leaves the builder usable:

```staple
let mut output = StringBuilder.with_capacity 64
output |> append "total: "
output |> append_display 42
output |> push '\n'
let text = StringBuilder.finish output
```

Builders implement `Default` and `FromIterator` for both `Char` and `String`.
They do not implement `Display`: formatting takes its value by move, so
interpolating a builder would consume it. Read the contents with `to_string`. Collecting into `String` also builds through a builder.
`Formatter` wraps a builder, so interpolation and `Display`/`Debug` formatting
use the same growth and copying operations. `Formatter.push` writes a scalar
without an intermediate string allocation.

String queries and ranges use **byte offsets**. `length` is the byte length;
`char_count` counts Unicode scalars in O(n). `is_empty`, `is_char_boundary`,
`starts_with`, `ends_with`, and `contains` inspect text without changing it.
`find` and `rfind` return `Option<USize>` for the first or last substring
match; `find_char` searches for a scalar. An empty needle matches at zero
for `find` and at the byte length for `rfind`. `char_indices` yields
`(byte_offset, Char)` pairs. `PartialOrd String` and `Ord String` compare
UTF-8 bytes lexicographically, which also orders Unicode scalar values.

`slice` takes a half-open `Range<USize>` and copies the substring. It returns
`None` for reversed bounds, offsets past the end, or offsets inside a scalar.
For example, `String.slice "héllo" ((USize :: 1) .. (USize :: 3))` returns
`Some "é"`. `strip_prefix` and `strip_suffix` return copied substrings when
the requested text matches. `trim`, `trim_start`, and `trim_end` remove Unicode
`White_Space`, as defined by `Char.is_whitespace`.

`split text separator`, `lines text`, and `split_whitespace text` are lazy,
copyable iterators that allocate only the substrings they yield. `split`
preserves empty fields, including a final empty field, and panics with
`String.split: empty separator` for an empty separator. `split_once` returns
an optional pair at the first match; an empty separator matches at the start.
`lines` splits on LF, removes a CR immediately before each LF, and omits the
empty line after a final LF. A CR that is not followed by LF stays in the line. `split_whitespace` skips runs of Unicode whitespace.

`replace text needle replacement` replaces nonoverlapping matches; an empty
needle inserts the replacement at every scalar boundary, including both ends.
`repeat text count` repeats text, and `join separator values` joins any
`IntoIterator` of strings. These operations build fresh strings.
`to_ascii_uppercase`, `to_ascii_lowercase`, and `eq_ignore_ascii_case` affect
ASCII letters only; non-ASCII scalars remain unchanged. `Char` has matching
ASCII case operations, `to_string`, and the predicates `is_ascii_digit`,
`is_ascii_alphabetic`, `is_ascii_alphanumeric`, `is_ascii_uppercase`, and
`is_ascii_lowercase`.

`Shared<T>` is a garbage-collected reference to a value of type `T`, declared in
`std.shared` and imported with `use std.shared.(Shared)`. Its standard
declaration is `@sized_opaque pub type Shared<T where ?Sized T> opaque`:
its payload may be sized or unsized while the handle has a compiler-supplied
representation. `Unique<T>` is likewise opaque and owns its payload affinely.
`Shared.new` and `Unique.new` allocate payloads; the compiler-provided
`Shared value` and `Unique value` construction forms remain available.
Opaque handles have no nominal payload patterns: `Shared pattern` and
`Unique pattern` are rejected. Use automatic payload matching, `get`, or
`Unique.into_inner` instead.
`get` copies a payload when `Copy T`. `Unique.into_inner` consumes an owner,
returns its payload, and frees its allocation without dropping that payload.
Constructing `Shared value` copies or moves `value` into a managed allocation;
copying a `Shared` copies only its non-null handle. Product fields and indices,
including bracket indexing and index assignment, can be accessed and assigned
directly through the handle when the binding holding the handle is declared
`mut`; a `Shared` is transparent for field access and indexing, so these compose
through nested references as well. The standard library implements
`Eq Shared<T>` whenever `T` implements `Eq`, comparing the referenced values
rather than the handles. Every alias, not only the binding that performed
the write, observes the resulting payload mutation, since they all share the
same managed allocation:

```staple
let mut point: Shared<(x: I32, y: I32)> = Shared (x: 10, y: 20)
let x = point.x
point.x = 30
let (captured_x, captured_y) = Shared.get point
```

The companion method `Shared.replace: <T> (mut Shared<T>, move T) -> T` replaces a whole
fixed payload and returns the previous value:

```staple
let mut value = Shared 10
let previous = Shared.replace (value, 20)
```

`Unique.replace: <T> (mut Unique<T>, move T) -> T` has the same replacement
semantics for unique owners. Both methods take a non-slotted product argument;
they cannot be partially applied as `reference |> replace value`.

A nominal wrapper can contain a `Shared` handle. Construction retains both
layers, and the wrapper pattern can automatically project through the handle
to match its payload:

```staple
type SharedPoint wrap Shared<(x: I32, y: I32)>
let point = SharedPoint (Shared (x: 10, y: 20))
let SharedPoint (x, y) = point
```

An array can be moved into a `SharedBuffer`:

```staple
let values: SharedBuffer<I32> = SharedBuffer.from_array (10, 20, 30)
let count: USize = SharedBuffer.length values
let index: USize = 2
let third = values[index]
let second = SharedBuffer.get values 1
```

`SharedBuffer<T>` is a garbage-collected, fixed-length sequence of `T`,
declared as a sized opaque type with no constructor. It is
not a dynamic array and is not equal to any `T ^ N`, and an array never
converts to a `SharedBuffer` implicitly: the companion function
`SharedBuffer.from_array: <N where Natural N> move (T ^ N) -> SharedBuffer<T>`
performs the conversion explicitly by moving the array's elements into a new
allocation, so `T` need not be `Copy`. An empty `()` becomes a length-0
buffer, requiring an expected `SharedBuffer` type to infer its element type.
The buffer owns its elements and drops each exactly once when it becomes
unreachable.

At runtime a `SharedBuffer<T>` is one pointer to a managed header holding the
element count, a capacity of at least that count, and the elements. A shared
buffer made by `Buffer.into_shared_buffer` keeps its source buffer's capacity;
the spare storage is never used, because the length is fixed. A
`SharedBuffer<T>` is a copyable handle whose copies share that allocation, so
it is shared-mutable: a write through one copy is visible through every other
copy. `SharedBuffer.length` reports the number of elements. Where `Copy T`,
`SharedBuffer.get: (SharedBuffer<T>; USize) -> Option<T>` returns a copy of an
element, or `None` when out of bounds, and `SharedBuffer.get_unchecked` returns
it directly, trapping when out of bounds. `SharedBuffer.set` replaces an
element through a `mut` handle binding, returning the old value as `Some<T>`, or
`None` (dropping the new value) for an out-of-bounds position;
`SharedBuffer.set_unchecked` traps instead and also returns the old value.
`SharedBuffer.get_shared: <T> (SharedBuffer<T>; USize) -> Option<Shared<T>>` returns an
owned shared handle to an element, or `None` when out of bounds, and
`SharedBuffer.get_shared_unchecked: <T> (SharedBuffer<T>; USize) -> Shared<T>` returns
such a handle by position, trapping when out of bounds. Each handle keeps the
buffer allocation alive and observes replacements of its element; the buffer
continues to own the element itself. These handles give access to non-`Copy`
elements. Literal and variable indexing perform runtime bounds
checks through the standard library's `Index`/`MutateIndex` implementations.
Where `Copy T`, the standard library also implements `IntoIterator`/`Iterator`
for `SharedBuffer<T>` through `SharedBufferIter<T>`, so `for item in values`
yields owned copies and the handle stays usable, and iterable again, after a
loop. Where `Eq T`, the standard library also implements `Eq SharedBuffer<T>`:
buffers of different lengths are unequal, and equal-length buffers are compared
element by element. A `SharedBuffer<T>` is not a product: it cannot be spread
or destructured, and it cannot cross a foreign ABI.

`Buffer<T>` is low-level, fixed-capacity contiguous storage with an initialized
prefix. `Buffer.with_capacity` allocates space without constructing any `T`
values, so it does not require `Default T`. Buffers are move-only owners of
their allocation, which they release eagerly rather than leaving to the garbage
collector: after dropping every initialized element in index order, the
allocation is freed when the buffer goes out of scope or is consumed, like a
`Unique` payload. The collector traces it through its owner, so a buffer held
in a managed object can be part of a collectable cycle. `Buffer.length` and `Buffer.capacity`
report its current initialized length and fixed capacity. Where `Clone T`, cloning a
buffer clones every initialized element into an independent allocation while
preserving the original capacity.

`Buffer.push` appends while spare capacity remains, and `Buffer.pop` moves the
last initialized element into an `Option<T>`. Both require a mutable buffer
argument. Pushing to a full buffer traps; growth belongs in higher-level
containers such as `List`. Where `Copy T`, `Buffer.get` returns a copy of an
initialized element as an `Option<T>`, or `None` for an out-of-bounds index, and
`Buffer.get_unchecked` returns it directly and traps for an out-of-bounds
index. `Buffer.set` replaces an element, returning the old value as `Some<T>`, or
`None` (dropping the new value) for an out-of-bounds index;
`Buffer.set_unchecked` traps instead and also returns the old value. Buffers hand
out no references to their elements.

`Buffer.from_array: <N where Natural N> move (T ^ N) -> Buffer<T>` moves the
elements of a fixed array into a new buffer whose capacity and length are both
`N`; the buffer owns the moved elements. `Buffer.into_shared_buffer` consumes the
buffer and returns a `SharedBuffer<T>` over the same allocation, without
copying. The allocation, including any spare capacity, is then garbage
collected instead of released eagerly. The consumed buffer cannot subsequently
be used or shared again. The shared buffer drops exactly its initialized
elements when unreachable; a `SharedBuffer` or a `Shared` to one of its
elements keeps it alive.

`Buffer.transfer` moves every initialized element of one buffer onto the end
of another's initialized prefix, in order, and empties the source. Both
buffer arguments must be mutable. It traps if the destination lacks enough
spare capacity for the source's elements, or if the source and destination are
the same buffer. No allocation occurs: elements
are moved by one bulk copy. This is the primitive higher-level growable
containers such as `List` use to move an initialized prefix into a larger
allocation.

`List<T>` is a growable array built on top of `Buffer<T>`. `List.new` and
`List.with_capacity` construct an empty list with no or a chosen initial
capacity, and `List.length`/`List.capacity` report its current element count
and storage size, exactly like the matching `Buffer` operations. `List.push`
appends an element and, unlike `Buffer.push`, never traps for lack of space:
once the current capacity is exhausted it allocates a larger `Buffer`
(doubling, with a minimum of four elements) and moves every existing element
across with `Buffer.transfer` before appending. `List.pop` removes and
returns the last element as an `Option<T>`, exactly like `Buffer.pop`.

`List.of: <N where Natural N> move (T ^ N) -> List<T>` builds a list from a
fixed array of values — `List.of (1, 2, 3)` — moving the elements into a
buffer whose capacity is exactly the array length. Because `(T ^ 0)` is `()`
and `(T ^ 1)` is `T`, `List.of ()` builds an empty list and `List.of value`
builds a singleton from a single expression. It is declared inside `List`'s
own companion, so ordinary `Type.method(...)` call syntax resolves it.

Where `Copy T`, `List.get` returns an element by copy, wrapped in `Option<T>`
so that an out-of-bounds index produces `None` instead of trapping, and
`List.get_unchecked` is the trapping counterpart. `List.set` replaces an
element, returning the old value as `Some<T>`, or `None` for an out-of-bounds
index; `List.set_unchecked` traps instead and also returns the old value.
`list[index]` and `list[index] = value` delegate to `Index`/`MutateIndex`, backed by
`get_unchecked`/`set_unchecked`, so bracket indexing keeps the trapping
behavior it has elsewhere in the language; `for item in list` consumes the list — `IntoIterator` moves its
source — and yields owned copies. Iteration requires `Copy T`, the same as
`List.get`; clone the list with `Clone.clone` first when it must remain
usable.

`List.into_buffer: move List<T> => Buffer<T>` consumes the list and returns its
backing buffer; turn that buffer into a `SharedBuffer<T>` without copying with
`Buffer.into_shared_buffer`, as in
`list |> List.into_buffer |> Buffer.into_shared_buffer`.

`List` is move-only like its underlying `Buffer`, and likewise owned rather
than garbage collected: dropping a list drops its elements and frees its storage. Where `Clone T`, cloning a
list clones every initialized element into independent storage and preserves
the source list's capacity; subsequent growth or mutation of either list does
not affect the other.

Managed references use a non-moving, single-threaded, stop-the-world
mark-and-sweep collector. Collection occurs automatically as the live heap
crosses a growing allocation threshold. Stack/register values, module globals,
managed payloads, closure environments, and recursive binding cells are
scanned conservatively, so an integer or raw bit pattern that resembles a
managed address can keep an otherwise unreachable object alive temporarily.
There are currently no null, weak, manual-collection, or
collector-statistics operations. Closure environments are managed objects.
Recursive binding cells remain permanent conservative root regions and are a
future migration.

Values are affine unless they are structurally `Copy`. Assignment, return,
whole-value destructuring, and closure capture move a non-`Copy` value; using
it afterward is an error. Argument passing moves a non-`Copy` value only into
a `move` parameter; an ordinary parameter borrows it instead (see "Move and
borrow parameters" above). Integers, `Bool`, `String`,
`Shared<T>`, C pointers, functions, and products/sums/wrapper values made entirely
from `Copy` fields are copied implicitly. The public prelude trait `Copy` can be
used as a generic bound, but implementations are compiler-inferred and an
explicit `impl Copy` is rejected. A custom `Drop` implementation makes its
wrapper target move-only regardless of its representation. A wrapper target
can also be opted out of `Copy` directly, without a `Drop` implementation, with
a negative `impl !Copy T {}` declaration; this only affects assignment and
moves, not destruction, so the type still needs no `cleanup` member and is not
finalized on collection.

```staple
trait Copy T {}

trait Drop T {
    cleanup: T -> ()
}

type File wrap I32
impl Drop File {
    cleanup = File descriptor => close descriptor
}

type Handle wrap I32
impl !Copy Handle {}
```

A `Drop` implementation applies to a concrete type exactly when its header
unifies with that type and its conditional bounds hold under the unification,
the same rule every other trait uses. Coherence rejects overlapping
implementations, bound-aware, so at most one implementation applies to any
type and selection needs no precedence. A `Drop` implementation's bounds may
constrain only its own type parameters (`impl<T where Copy T> Drop Box<T>`);
a bound on the target type itself (for example `where Copy Box<T>`) is
rejected. A type that still mentions type parameters and may have an
applicable implementation is treated as not `Copy`, so a template that copies
such a value is a move error even if some instantiation would qualify.

```staple
type Box<T> wrap (T)
impl<T where Copy T> Drop Box<T> {
    cleanup = Box value => ()
}

// `Box I32` has the custom destructor above; `Box CString` does not (its
// `CString` field is still freed), because `CString` is not `Copy`.
```

`Clone` is a separate prelude trait for explicit duplication and, unlike Rust,
is not a prerequisite of `Copy`: `Copy` remains fully compiler-inferred as
described above, and a type needs no `Clone` implementation to be `Copy`.
Every `Copy` type conversely gets `Clone` for free, through a blanket
implementation in the standard library:

```staple
trait Clone T {
    clone: T -> T
}

impl<T where Copy T> Clone T {
    clone = value => value
}
```

A non-`Copy` type has no implicit `Clone`; implement it manually to allow
explicit duplication:

```staple
type Handle wrap I32
impl !Copy Handle {}
impl Clone Handle {
    clone = Handle descriptor => Handle descriptor
}
```

Owned locals and parameters are dropped in reverse lexical order on normal
scope exit, explicit `return`, and propagation. `drop value` consumes and
destroys a value early; it is an ordinary function whose moved parameter is
dropped on return. The compiler calls `Drop.cleanup` itself, so referencing it
directly — as a call or a value — is rejected, since the value would be
destroyed again on its normal drop. A destructor may inspect copied fields and
make scoped C calls, but may not move out of its value; after the custom
destructor returns, the representation's owned fields are dropped
automatically. Partial field moves through `.name` or `.index` are rejected;
destructure the whole owned value instead.

A global binding may hold a move-only value, but the value can only ever be
borrowed — shared or `mut` — from top-level statements or from any function;
it can never be moved out. Top-level values have program lifetime: they are
not guaranteed to run `Drop` at normal process termination.

When an unreachable managed payload or closure environment owns a `Drop`
value, collection finalizes it exactly once before reclaiming its storage.
Finalization is two-phase: all unreachable finalizers run while their objects
still exist, resurrection does not retain those objects, and allocation is
allowed while nested collection is suppressed.

A managed allocation stays reachable through a pointer to any position inside
it, not only its start. A `Shared` to one element of a `SharedBuffer`, or a
`SharedBuffer` made by `Buffer.into_shared_buffer`, keeps the whole allocation alive, including the other
elements, until no such pointer remains.

`CChar` is a public opaque type in `std.cinterop`. `CString` is a public
wrapper around `RawMutPtr<CChar>` with a private representation.
`RawPtr`, `RawMutPtr`, `RawOpaquePtr`, and `RawOpaqueMutPtr` are public opaque
types in `std.ptr`. Source code must import them explicitly, for example with
`use std.cinterop.*` and `use std.ptr.*`. `RawPtr<T>` is a non-owning, low-level
pointer type; the pointee is a compile-time argument with no runtime field.
Raw pointers are `Copy` and do not manage allocation lifetime or validate their
targets. They can also be used for C interop. There are no `*T` or `*const T`
type forms. `RawMutPtr<T>` permits raw reads and writes, while `RawPtr<T>`
permits raw reads. Their opaque counterparts have no pointee type and must be
converted to typed pointers before accessing memory.

`CString` is an owned, move-only pointer to NUL-terminated bytes allocated with
`malloc`. Its library `Drop` implementation releases that allocation with `free`.
The `c_string` macro uses a literal primitive to copy constant bytes directly
into malloc storage without first allocating a managed `String`.

`CString` is compatible with `RawPtr<CChar>`, but an arbitrary `RawPtr<CChar>`
is not assumed to be NUL terminated. Passing a `CString` to a
C function creates a call-scoped view rather than transferring ownership. A
temporary CString remains alive until the C call returns, then is freed. C code
must not free the borrowed pointer or retain it beyond the owner's lifetime.
C declarations may not return `CString`. `CString.to_string` takes its argument
as `move CString`: it validates and copies UTF-8 into a `String`, returning
`Result<String, InvalidUtf8>`. It frees the C string on success and failure, so
the argument cannot be used afterward. `CString.from_string` returns
`Result<CString, InteriorNul>`, rejecting embedded NUL bytes before allocating an
owned, terminated copy. `CString.to_string_unchecked` and
`CString.from_string_unchecked` perform the same copies without validation;
their callers must ensure valid UTF-8 and absence of embedded NULs, respectively.

An underscore asks the compiler to infer a type:

```staple
_
_ -> I32
```

The currently supported type syntax also includes type application, product
types, variadic markers in external function signatures, and function types:

```staple
RawPtr<CChar>
(I32, String)
(RawPtr<CChar>, ...) -> I32
```

`...` denotes the variadic portion of an external function parameter product. Its
meaning outside foreign declarations is currently unspecified.

The fixed-width standard-library integers are `I8`, `I16`, `I32`, `I64`, `U8`,
`U16`, `U32`, and `U64`. `ISize` and `USize` use the target pointer width.
Arithmetic is homogeneous: both operands and the result have the same type.
Signed integer division uses signed semantics, while unsigned integer division
uses unsigned semantics. All integer types implement `Eq` for `==` and `!=`, and
`PartialOrd` and `Ord` for ordering. `PartialOrd.partial_cmp` returns
`Option<Ordering>`; its default `lt`, `le`, `gt`, and `ge` methods back `<`, `<=`,
`>`, and `>=`. `Ord.cmp` returns `Ordering.Less`, `Ordering.Equal`, or
`Ordering.Greater` and requires both `Eq` and `PartialOrd`. Integer ordering uses signed semantics for signed types
and unsigned semantics for unsigned types.

Every integer type also implements the bitwise traits from `std.ops`:
`BitAnd`, `BitOr`, and `BitXor` for `bit_and`, `bit_or`, and `bit_xor`;
`BitNot` for `bit_not`; and `ShiftLeft` and `ShiftRight` for `shift_left` and
`shift_right`. These traits have no operator syntax and, like the operator
traits, their methods require explicit imports. They are homogeneous: the
shift amount has the same type as the shifted value. Right shifts are
arithmetic for signed integers and logical for unsigned integers.
`ShiftLeft.shift_left` and `ShiftRight.shift_right` are `@track_caller`
members: a count that is negative or not smaller than the width
[panics](#track_caller-and-location) at the call. The integer companions'
`wrapping_shift_left` and `wrapping_shift_right` take a `U32` count and reduce
it modulo the width instead.

`F32` and `F64` use IEEE-754 single and double precision. Both implement the
arithmetic traits, `Eq`, and `PartialOrd`, but not `Ord`. A comparison involving
NaN makes `partial_cmp` return `None`; ordered boolean comparisons are false,
`==` is false, and `!=` is true. Float division by zero follows IEEE behavior.
Staple does not provide implicit numeric conversions.

Explicit conversions use `Convert From To` and `TryConvert From To` from
`std.convert`, also available through the prelude. `Convert.convert` consumes
its input and returns `To`. `TryConvert.try_convert` returns `Result<To, Error>`,
where `Error` is an associated type. Every type converts to itself, and every
`Convert` implementation also implements `TryConvert` with `Error = Never`.

Numeric `Convert` implementations preserve every source value: integer
widening, unsigned integers to sufficiently wider signed integers, `F32` to
`F64`, and integers small enough to be exactly represented by the target float.
Guarantees for `ISize` and `USize` hold on both 32-bit and 64-bit targets.
Other numeric pairs implement `TryConvert`, with `ConversionError.OutOfRange`
or `ConversionError.PrecisionLoss`. Float-to-integer conversion requires an
integral value in range; NaN and infinity return `ConversionError.NonFinite`.
`F64` to `F32` accepts NaN and infinities, without promising preservation of
NaN payload bits, and requires finite values to be represented exactly.

`Bool` converts to every integer as zero or one; checked integer-to-`Bool`
conversion accepts only those two values. Numbers and booleans convert to
`String` using their existing formatting. Checked string parsing consumes the
complete input and rejects whitespace and embedded NULs. Signed integers
accept decimal digits with an optional `+` or `-`; unsigned integers accept
an optional `+`. Floating-point parsing uses the C library's floating-point
syntax, including exponents, hexadecimal floats, NaN, and infinity, and rounds
to the target precision. Syntax failures return `InvalidNumber`; overflow
and underflow reported by the parser return `OutOfRange`. Boolean parsing
accepts `True`, `true`, `False`, and `false`, otherwise returning `std.boolean.InvalidBool`.

`String` has no `Convert` implementation to a byte sequence; `String.to_bytes`
is the explicit copy into a `List<U8>`. Arrays and buffers have no
`Convert` implementations; use `SharedBuffer.from_array`, `Buffer.from_array`,
and `Buffer.into_shared_buffer` explicitly. Checked byte
list-to-string conversion is `TryConvert List<U8> String` (or
`String.from_utf8`), which consumes the list and returns
`Result<String, std.string.InvalidUtf8>`. `String.from_utf8_unchecked` constructs
a string without validation when the caller knows the bytes are valid UTF-8.
Checked `String` to `CString` conversion rejects embedded NULs with
`std.cinterop.InteriorNul`; checked `CString` to `String` validates UTF-8 and frees the
consumed C string on both success and failure.

`RawPtr<T>` and `USize` convert in both directions. An unchanged round trip
preserves the pointer. Integer-to-pointer conversion does not validate the
address, alignment, allocation lifetime, or whether dereferencing is permitted.
Address arithmetic follows the existing `USize` arithmetic rules.

The `offset pointer bytes` companion function on all four pointer types returns
a new pointer of the same type, offset by `bytes: ISize`. All count bytes,
regardless of the pointee type; negative offsets move backward. They do not
dereference the pointer, validate its address or allocation lifetime, or
transfer ownership.


`RawOpaquePtr` is a non-owning, `Copy` low-level pointer with no pointee type,
compatible with C `void *` in parameters and returns. It implements `Convert`
to and from every `RawPtr<T>`, and to and from `USize`. These explicit
conversions preserve the pointer, including null, without allocation,
validation, or ownership transfer. A typed result does not establish that the
pointer is valid or aligned for its pointee. The blanket `TryConvert`
implementation supplies `Error = Never` for all these conversions.

```staple
let opaque = pointer as RawOpaquePtr
let restored = opaque as RawPtr<I32>
let inferred: RawPtr<I32> = opaque as _
let address = opaque as USize
```


`RawMutPtr<T>` and `RawOpaqueMutPtr` also convert to and from `USize`.
All four pointer families explicitly convert to one another, including conversions
that recover mutability. Direct conversions between `RawPtr<T>` and `RawMutPtr<T>`
preserve `T`; reinterpret a pointee through an opaque pointer. These conversions
do not validate memory or grant ownership, and there are no implicit pointer
conversions. The existing call-scoped `CString` view remains limited to
`RawPtr<CChar>`.

All four pointer types provide `null ()` and implement `Eq` by address, without
requiring `Eq` or `Copy` on the pointee. Comparing different pointer types requires
an explicit conversion. Typed null pointers infer their pointee from context.

`RawPtr.load pointer` and `RawMutPtr.load pointer` copy a value from memory.
`RawMutPtr.store pointer value` overwrites memory and returns `()`, without
requiring a mutable pointer binding or dropping the previous value. Loads
require a sized, `Copy` pointee; stores accept any sized pointee and move the
incoming value into memory. The caller must arrange for that value to be dropped
or moved out exactly once. Overwriting an initialized resource without first
releasing it can leak that resource. Both operations perform ordinary, non-atomic
memory accesses without runtime validation: callers must provide valid, aligned
memory with a live allocation; loads require an initialized valid value and
stores require writable memory. Opaque pointers provide neither operation.

```staple
let null: RawMutPtr<I32> = RawMutPtr.null ()
let writable: RawMutPtr<I32> = pointer as RawMutPtr<I32>
RawMutPtr.store writable 42
let value = RawPtr.load (writable as RawPtr<I32>)
let same = writable == (pointer as RawMutPtr<I32>)
```


`expression as Type` is syntactic sugar for `Convert.convert.<_, Type> expression`.
It uses `Convert`, so it requires an infallible conversion implementation.
It binds tighter than arithmetic and comparisons, but looser than prefix
operators, function application, and access. Chained conversions associate
left to right; parenthesize a larger source expression to convert its result:

```staple
let wide = (I8 :: 42) as I32
let floating: F64 = wide as _
let total = (wide + 1) as F64
```

### Type declarations

staple distinguishes transparent aliases, wrapper type definitions, and
opaque type declarations. A declaration is the `type` keyword, a name,
optional angle-bracketed compile-time parameters, and an optional body whose
marker (`opaque`, `wrap`, `from`, or `=`) says what kind of type it declares:

```staple
// Singleton
pub type Foo

// Opaque
pub type Foo opaque

// Implicitly introduced nominal type
pub type Foo<T> from pub T

// Explicitly constructed nominal type
pub type Foo<T> wrap pub T

// Transparent alias
pub type Foo<T> = T
```

The representation visibility of `wrap` and `from` bodies is written after the
marker, immediately before the underlying type. Type arguments are written the
same way they are declared: `Foo<A, B>`.

An opaque type has no source-level representation and uses the explicit
`opaque` marker:

```staple
pub type Handle opaque
```

An opaque type may also accept compile-time arguments when its body is the
`opaque` marker:

```staple
pub type RawPtr<Pointee> opaque
```

Arguments are part of nominal identity, so two applications with different
arguments are distinct. Opaque types are unsized and non-`Copy` by default.
They may be referenced through `Shared`, whose parameter accepts unsized types,
but cannot be used by value or satisfy implicit `Sized` bounds.

The standard-library-only intrinsic modifier `@sized_opaque`, declared in
`std.annotation` with `pub(package)` visibility, marks opaque types with
compiler-supplied sized representations. Compiler-known sized opaque types such as `std.core.I32` and
`std.ptr.RawPtr` must carry this modifier. The modifier grants sizedness;
sized opaque types are `Copy` by default. The stdlib explicitly declares
`impl !Copy` for opaque types that own resources or must be consumed once.
Negative implementations may quantify effect parameters, for example
`impl<T, effect E> !Copy Coroutine{E}<T> {}` covers every deferred effect row.

#### Singleton types

A bodyless type declaration introduces both a nominal type and its unique
value under the same name:

```staple
pub type Ready

let state: Ready = Ready
```

Singleton values have the zero-sized representation `()` but remain nominally
distinct from `()` and from every other singleton type. A public singleton
exports its value together with its type; a private singleton keeps both names
private.

The unique value is not a function and cannot be called. In a match pattern,
its bare name selects the singleton without binding a variable:

```staple
pub type Waiting
let status: Ready | Waiting = state

match status {
    Ready => (),
    Waiting => (),
}
```

The explicit nominal form `Ready()` remains available, including in
destructuring and propagating bindings.

The same form can select a singleton alternative in a propagating binding:

```staple
let ?Ready() = operation()
```

The standard-library `Bool` type is defined entirely in these terms:

```staple
pub type True
pub type False
pub type Bool = True | False
```

`True` and `False` are ordinary singletons, and `Bool` is an
[alias](#aliases) of their sum. It is therefore exactly `True | False`, not a
nominal type or a `from` wrapper, and it has no compiler-specific type
identity. A bare `True` or `False` is injected into a `Bool` where one is
expected, and patterns such as `True` select the matching sum alternative
directly.

#### `wrap`

A body beginning with `wrap` creates a nominal wrapper type
with the same runtime representation as its underlying type:

```staple
type UserId wrap I32
type OrderId wrap I32
```

`UserId` and `OrderId` are wrapper types over `I32`. The three types are not
implicitly interchangeable, even though they share a representation. This provides type
safety without adding a runtime wrapper. A represented wrapper type also
declares a private constructor in its defining module:

```staple
let user: UserId = UserId 42
```

Construction and nominal-pattern destructuring are zero-cost conversions. No
standalone unwrap operation is generated. With ordinary `pub type`, the type is
public but its representation and constructor remain private to its defining
module. Opaque declarations have no constructor.

A represented nominal value can expose one layer of its inner representation
with `.*` when that representation is visible in the current scope:

```staple
type User wrap (name: String, age: I32)
let user = User (name: "Ada", age: 42)
let inner = user.*
inner.name
```

This projection is zero-cost and preserves the value's storage when used as a
place. Named and positional access provide a one-layer shortcut, so `user.name`
and `user.0` mean `user.*.name` and `user.*.0`. The shortcut does not recursively
unwrap nested nominal types; use one `.*` for each visible layer, as in
`outer.*.*.name`. Both explicit and shortcut forms are rejected when the
representation is private in the current scope.

Indexing has no such shortcut: a represented type is indexable only through
its own `Index` and `MutateIndex` implementations, so `value[index]` on a
wrapper without one is an error. Index the representation explicitly with
`value.*[index]`, or implement the traits to make the wrapper indexable.

`wrap pub T` exposes the representation and generated constructor as part of
the module interface:

```staple
pub type Box<T> wrap pub (value: T)
```

Importers may construct `Box` values and use `Box pattern` to destructure them,
including through namespace, selected, renamed, or glob imports. Every named
type directly referenced by a public representation must also be public.
Representation visibility is rejected on aliases and opaque declarations, which
have no constructor. It is also available on `from` bodies, described next.

Packages add a middle visibility level between private and public:

```staple
pub(package) def internal_helper = 42
pub type Shared wrap pub(package) (value: I32)
```

`pub(package)` names are available from any module with the same canonical
package identity. `wrap pub(package) T` keeps representation access inside
that package while the type name stays public. A package-visible
re-export uses `pub(package) use`; ordinary `pub use` cannot promote a
package-visible declaration into an external interface. A `pub use module.*`
glob carries package-visible names at package visibility: same-package modules
reach them through the re-export, while the re-exporting module's public
interface omits them. Package visibility is
rejected when run without a package manifest. Thus visibility forms
the lattice `private < package < public`: public representations may reference
only public types, while package representations may also reference
package-visible types.

Represented types, aliases, and explicitly opaque declarations may introduce
compile-time parameters in angle brackets directly after the type name, as
generic functions do. Each parameter is a plain name, optionally with a default
(see [Default type parameters](#default-type-parameters)); product parameters
such as `(A, B)` are not allowed, so declare each parameter separately. A
`where` clause goes inside the brackets, after the parameters:

```staple
type Box<T> wrap (value: T)
type HashMap<K, V> wrap (key: K, value: V)
type Pair<A, B> = (A, B)
```

They may also bind one effect-row parameter in braces immediately after the
type name. The effect parameter precedes any ordinary compile-time parameters
and may be used in effect sets within the representation:

```staple
type Callback{E} = () ->{E} ()
type Handler{E}<T> = T ->{E} ()
```

Declaration braces must contain exactly one new effect-variable name. They are
not an effect set themselves: the lone name always declares a variable, while
empty braces and multiple entries are rejected. Bodyless singleton
declarations cannot have an effect parameter. Represented, alias, and
explicitly opaque declarations may have one.

At a use site, braces supply the full effect set before ordinary type
arguments. The set may be empty or contain resources, state effects, and at
most one in-scope effect variable, following the same rules as a function
arrow's effect set:

```staple
Callback{IO}
Handler{state, IO}<I32>

def adapt: <effect F> Callback{F} -> Handler{F, IO}<I32>
```

Omitting a type's effect argument is always equivalent to supplying `{}`.
This default is applied before ordinary type arguments, so `Callback` means
`Callback{}` and `Handler<I32>` means `Handler{}<I32>`.

Type application uses angle brackets, with the arguments separated by commas
and each one consuming one parameter in order. A product type is an ordinary
argument, written with its own parentheses:

```staple
// Given: type Entry<K, V> wrap (key: K, value: V)
Entry<String, I32>

// Given: type Cell<T> wrap (value: T)
Cell<(String, I32)>
```

Type constructors are not curried. `Entry<String>` is not a type constructor
awaiting `V`; every argument must be supplied in the same application.
Traits keep their juxtaposed argument lists (`Add Left Right`), so a generic
type used as a trait argument is written without surrounding parentheses, as in
`Iterator Map<S, Out>`.

A type annotation must apply every ordinary compile-time parameter, unless the
omitted trailing parameters all have defaults (see [Default type
parameters](#default-type-parameters)). An omitted effect-row parameter uses
the empty set as described above. Applying a non-parameterized type, supplying
the wrong product shape, or leaving a type partially applied is an error. This
applies to a self-reference too: a recursive `type Node<T>` must still write its
own ordinary arguments, as `Node<T>` or `Node<I32>`, never a bare `Node`.

A represented type may refer to itself, directly or mutually, only where the
reference passes through an out-of-line indirection — `Shared`, `Unique`,
`SharedBuffer`, `Buffer`, or a `Syntax` value — so that every instance has a
finite layout. A representation that would contain itself by value
(`type Loop wrap (head: I32, tail: Loop)`) is rejected as a cyclic
definition; routing the back-edge through `Shared`
(`type Loop wrap (head: I32, tail: Option<Shared<Loop>>)`) or a buffer
(`type Tree wrap (value: I32, children: Buffer<Tree>)`) is well-founded and
accepted. A represented type whose parameters all occur in its
representation only behind such an indirection guards a back-edge the same
way, so `type Tree wrap (value: I32, children: List<Tree>)` is accepted
because `List<T>` wraps `Buffer<T>`. A type that stores a parameter inline,
such as `type Pair<T> wrap (first: T, second: I32)`, does not:
`type Loop wrap (pair: Pair<Loop>)` is rejected as cyclic.

Product types and values may have a trailing comma.

#### `from`

A body beginning with `from` declares a represented nominal type, exactly as
`wrap` does, together with an implicit one-way conversion from its
representation:

```staple
type Meters from F64

let distance: Meters = 1.5  // implicitly `Meters 1.5`
let raw = 1.5               // still `F64`
let converted: Meters = raw
```

`Meters` and `F64` remain different types; `from` is neither an alias nor a
subtype relationship. Only the direction `F64 -> Meters` is implicit.
Projection back to the representation still uses `.*`, field access, or a
nominal pattern, subject to representation visibility.

The conversion is contextual. It applies only when an expression is checked
against an expected `Meters`, for example in an annotated binding, an argument
position, a function result, or `Meters :: expression`, and it never changes
bottom-up inference. A value is accepted when it can be checked as the
representation by the ordinary rules, including literals, sum injection, and
sum widening; the conversion then wraps it. When a sum is expected, an exact
alternative always wins, then an alternative reached by an ordinary coercion,
and only then a `from` alternative. If more than one `from` alternative accepts
the value, the expression is ambiguous and rejected:

```staple
type A from B

let exact: A | B = b    // selects `B`, not `A`
```

At most one `from` conversion is inserted for a mismatch. Given
`type A from B` and `type C from A`, both `B -> A` and `A -> C` are
implicit, but `B -> C` is not. Conversions also do not lift through other
types: `Shared<B>`, `(B, B)`, `List<B>`, and `() -> B` do not convert to the
corresponding `A` forms. A literal written directly in such a position is
still checked element by element, so `let pair: (A, A) = (b1, b2)` converts
each element.

`from` does not imply `B <: A`. Subtype bounds, trait implementations, and
generic inference do not see a `B` as an `A`, and member or method lookup on a
`B` value never converts the receiver. The implicit conversion has the same
ownership behavior as calling the constructor: a non-`Copy` source is moved.

The visibility before `from` controls *representation* visibility, not the
visibility of the conversion. The conversion and constructor are usable
wherever the type itself is visible; projection with `.*`, field
shortcuts, and pattern matching through the representation follow the
representation visibility:

| Declaration | External `Bar -> Foo` | External representation access |
| --- | :---: | :---: |
| `type Foo from Bar` | no, `Foo` is private | no |
| `pub type Foo from Bar` | yes | no |
| `pub type Foo from pub(package) Bar` | yes | package only |
| `pub type Foo from pub Bar` | yes | yes |

Because a public conversion names its source, every named type directly
referenced by the source of a public (or package-visible) `from` type must be
at least as visible as the type. `@recursive_constructor` types cannot use
`from`.

Patterns see through one `from` layer. When a pattern does not select the
`from` type itself, it is checked against the type's visible representation:

```staple
type Point from (I32, I32)

match point {
    (x, y) => x + y,    // matches through `Point`'s product representation
}
```

Within a sum, a pattern that selects no alternative directly matches inside the
unique `from` alternative whose representation it selects from. Here `Inner`
is the only alternative whose representation contains `I32`, so the typed
pattern reaches it through `Inner`'s representation:

```staple
type Inner from I32
let choice: Inner | String = 4

match choice {
    number: I32 => number,
    _: String => 0,
}
```

Typed patterns, destructuring bindings, and propagating bindings
(`let ?Some(value) = option`) see through the same way. Exhaustiveness is
checked against the representation, and the representation must be visible
where the pattern is written.

#### Aliases

A body beginning with `=` gives another name to an existing type without
creating a new type. The alias and its underlying type are interchangeable:

```staple
type Person = (
    name: String,
    age: I32,
)
```

Here, `person` is exactly the named product type on the right-hand side. The alias
does not create a separate nominal identity, and it has no companion of its
own: `companion Person` would be `companion` of the underlying type (see [Type
companions](#type-companions)). Diagnostics describe an alias by its underlying
type, so an error about a `Bool` may name `True | False`.

### Sum types

A sum type lists the variants which a value may contain:

Staple’s sum types are open: their alternatives are ordinary types rather than cases owned by the sum.

```staple
Ok<Tree> | IOError | ParseError
```

Variants do not share a declaration. Modules may define represented nominal
types independently and combine them wherever a type is accepted. `Ok` and
`Result` are public types from `std.result`, also available through the prelude:

```staple
pub type Ok<T> from pub T
pub type Result<T, E> = Ok<T> | E
```

`Result<T, E>` is therefore exactly the sum `Ok<T> | E`, with the error case
unwrapped. Because `Ok` is a [`from`](#from) type, a success value converts
implicitly wherever a `Result` is expected:

```staple
def parse = (flag: Bool) => Result<I32, String> :: when {
    flag => 42,       // implicitly `Ok 42`
    else => "bad",    // the `String` error
}
```

An exact alternative still wins over the `from` conversion, so when the error
type overlaps the success type, as in `Result<String, String>`, a bare `String`
is the error and success must be written `Ok value`. Inside a generic body
the alternative is chosen once against the type parameters: in
`<T, E> value: T => Result<T, E> :: value`, `value` is always `Ok`, even when
`E` is instantiated to `T`.

Every alternative must be a sized value type. Primitive, product,
function, opaque, reference, and fully applied nominal types may all be
alternatives. Partially applied types cannot be alternatives. A
transparent alias may name a sum or alternative, but does not introduce another
variant identity.

Sized compile-time parameters may be alternatives in generic code. Their sum
representation is specialized and canonicalized for each concrete use, so
`A | B` collapses to one alternative when `A` and `B` specialize to the same
type.

Sums are unordered, flattened, and duplicate-free. Consequently `A | B` and
`B | A` are the same type, nested sums are flattened, and `A | A` is `A`.
Because `Never` has no values, it is removed when normalizing a sum with another
alternative, so `Never | A` is `A`. An alias of a sum dissolves the same way, so
`Bool | I32` is the three-alternative sum `True | False | I32`, with one tag.
Variant identity is the complete type, so different applications of one
constructor, such as `Ok<I32> | Ok<String>`, are distinct alternatives. A nominal
pattern must select exactly one alternative; use a typed binding to disambiguate
repeated nominal constructors.

Type application binds more tightly than `|`, and `|` binds more tightly than
the function arrow. Thus `String -> Ok<Tree> | IOError` is a function returning
the sum.

Values of any alternative type are injected implicitly when a sum is expected.
A smaller sum is likewise widened implicitly to a sum containing all of its
alternatives. Sums cannot be narrowed implicitly. Standalone values retain
their ordinary representation; a value acquires a runtime tag only while stored
in a sum. An exact alternative is preferred; otherwise the value must have
exactly one alternative to which it can be coerced, considering `from`
conversions only when no other coercion applies. A typed match binding
selects and extracts an arbitrary variant:

```staple
def describe = value: I32 | String => match value {
    number: I32 => number,
    text: String => 0,
}
```

A typed wildcard such as `_: I32` likewise selects that alternative without
binding it. An untyped binding or `_` matches the whole sum.

When a generic signature is matched against a sum, a bare compile-time
parameter alternative stands for every alternative the rest of the sum leaves
unmatched: none (`Never`), one, or a sum of several. Passing an
`Ok<I32> | String | Bool` to `<T, E> Result<T, E> -> T` infers `T = I32` and
`E = String | Bool`. With two or more bare parameter alternatives, each must
match a single alternative or `Never`. Because `Bool` is the two alternatives
`True | False`, `<A, B> A | B` does not infer from a `Bool | I32` value.

#### Propagating bindings

`let ?` followed by a nominal destructuring pattern selects its success
alternative and returns every other alternative from the enclosing function. The
`?` is written attached to the pattern, `let ?Ok(file) = ...`; the formatter
normalizes `let? Ok(file)` and `let ? Ok(file)` to that spelling.

```staple
def load = (path: String) => {
    let ?Ok(file) = read_file(path)
    let ?Ok(tree) = parse_tree_from_file(file)
    Ok(tree)
}
```

The right-hand expression must have a sum type, or a `from` type over a sum
whose representation is visible, containing exactly one alternative with the
pattern's nominal constructor. It is evaluated once. On
the selected tag, the payload is destructured and execution continues. Any
other tag returns immediately and is widened into the enclosing result type.
The selected representation must be visible under the ordinary
representation-visibility rules.

When the function result is omitted, the compiler joins its trailing value,
reachable explicit returns, and every propagated alternative. The example
therefore infers `Ok<Tree> | IOError | ParseError`. With an explicit binding
annotation or type-ascription constraint on the body, every normal and propagated
result must be contained in that type.

Sum types use Staple's internal tagged inline representation and may not appear
anywhere inside an `extern` binding type.

The prelude supplies a `typegroup` macro that conveniently generates a
nominal type over a sum of variants:

```staple
pub typegroup Pattern {
    Literal wrap pub String,
    Wildcard,
}
```

Generic parameters follow the group name in `<...>`, using the same declaration
parameter grammar as `type`. Defaults and an inner `where` clause are retained
on the group and every represented variant; singleton variants remain
unparameterized. Applications of variants use only argument names, excluding
defaults and bounds:

```staple
pub typegroup Exchange<T, E> {
    Request wrap pub T,
    Response wrap pub E,
}

pub typegroup Mixed<A, B, C, D> {
    Empty,
    Value wrap pub (A, B, C, D),
}
```

Each entry becomes a public declaration in the group's companion module. A bare
name is a singleton variant, and `wrap T`, `wrap pub T`,
`wrap pub(package) T`, `from T` (with the same visibility forms), and
`= T` follow the ordinary type declaration body grammar. `opaque`
entries are rejected, because an opaque type cannot be a sum alternative.
Representation visibility is written explicitly on each constructor variant.

The group itself is a [`from`](#from) type over the sum of its variants, whose
representation is as visible as the group:
`pub typegroup Pattern { ... }` declares
`pub type Pattern from pub Pattern.Literal | Pattern.Wildcard`. A variant
value is therefore introduced implicitly wherever the group is expected, and
patterns on a group value see through to its variants:

```staple
let pattern: Pattern = Pattern.Wildcard

match pattern {
    Pattern.Literal text => text,
    Pattern.Wildcard => "*",
}
```

#### Unwrapping with `Try`

The prelude's `Try` trait, from `std.try`, splits a value into a success and
whatever remains of a failure:

```staple
pub trait Try T {
    type Output
    type Residual
    branch: move T -> Ok<Output> | Residual
}

companion<T where Try T> T {
    @track_caller
    pub def unwrap = move value: T => Try.Output T :: ...
    @track_caller
    pub def expect = (move value: T; message: String) => Try.Output T :: ...
}
```

`branch` returns the success as `Ok` and the failure as its residual, so its
result is a `Result<Output, Residual>` that `match`, `let ?` and the postfix
[`?` operator](#propagating-with-) take apart. `unwrap` and `expect` are
[blanket companion](#blanket-companions) members defined from `branch`, so an
implementation cannot replace them. They return the success value and otherwise
[panic](#track_caller-and-location) at their caller, `expect` with the given message:

```staple
let port = read_port () |> expect "reading the port"
let config = Result.expect (load ()) "loading config"
```

The standard library implements `Try Result<T, E>`, with `Output = T` and
`Residual = E`, and `Try Option<T>`, with `Output = T` and `Residual = None`.
Because `Result<T, E>` is the sum `Ok<T> | E`, the first implementation covers
every sum with exactly one `Ok` alternative: `Ok<I32>` (`E = Never`),
`Ok<I32> | String`, and `Ok<I32> | String | Bool` (`E = String | Bool`) all
implement `Try`. A sum with two `Ok` alternatives does not, because its
success type would be ambiguous.

#### Propagating with `?`

A postfix `?` unwraps a `Try` value in place. `operand ?` is shorthand for

```staple
match (Try.branch operand) {
    Ok output => output,
    residual => return residual,
}
```

so a success evaluates to its `Output` and a failure returns its `Residual`
from the enclosing function or lambda. `Try` and `Ok` always mean the standard
library's, whatever the module imports or declares:

```staple
def load = (path: String) => {
    let file = read_file path ?
    let tree = parse_tree_from_file file ?
    Ok(tree)
}
```

Because `Result<T, E>` is the sum `Ok<T> | E` and `Option<T>` is `None | Some<T>`, a
residual is returned unchanged and widens into the function's result type like
a [`let ?` propagation](#propagating-bindings) does. With no declared result,
the compiler joins the trailing value with every residual; `load` above infers
`Ok<Tree> | IOError | ParseError`. `Option` and `Result` can share a function,
which then returns `Ok<T> | None | E`. With a declared result, every residual
must be contained in it. A `Try` type whose `Residual` is `Never`, such as
`Ok<I32>`, adds nothing. `?` is only allowed inside a function and is not
available during compile-time macro evaluation.

`?` ends the application on its left: `f a b ?` is `(f a b) ?`, and so is a
`|>` pipeline so far, so `x |> parse ? |> check ?` is `((x |> parse) ? |> check) ?`.
It binds tighter than every prefix and binary operator, and `await`'s operand
is the call it prefixes, so `await fetch url ?` is `(await (fetch url)) ?`.
Accesses may follow (`load path ?.port`) and `?` can repeat, but a juxtaposed
argument cannot: `f x ? y` is a parse error. Parenthesize instead, as in
`f (x ?) y` or `(f x ?) y`. By convention a space separates `?` from its
operand, as in `f arg ?`, so that it does not read as applying to `arg`.

## Foreign declarations

An `extern` block declares values supplied by a foreign ABI. The ABI name is a
string following `extern`:

```staple
use std.cinterop.*
use std.ptr.*

extern "c" {
    printf: (RawPtr<CChar>, ...) -> I32
}
```

The example declares an immutable foreign value named `printf`. Its type is a
function from one product parameter—containing a C string pointer followed by
variadic values—to an `I32` result.

External symbols are resolved during native linking. Calling-convention
details, supported ABI names, foreign symbol aliases, foreign library selection,
and foreign type layouts remain unspecified.
