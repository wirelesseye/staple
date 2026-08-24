# The staple language

**staple** is an expression-oriented programming language. Its compiler is
called **Stapler**.

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
lexical reaction ownership through the `Reactive` resource. Batching and
alternative schedulers remain future work.

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
the three delimited syntax types. `Expr` is currently the sum of `Ident String`,
`CallExpr`, and `UnstructuredExpr`.
`CallExpr` exposes `callee: Expr` and
`argument: Expr`; `UnstructuredExpr` preserves every other expression form
without exposing its fields yet. A macro body is a curried compile-time Staple
function. Every parameter consumes one atomic syntax unit; an omitted parameter
type means `SyntaxNode`, not opaque `Syntax`:

```staple
macro choose = condition => then => else => parse_quote {
    match $condition {
        True => $then,
        False => $else,
    }
}
```

Typed parameters constrain the grammar accepted at an invocation. `Ident` is a
generic syntax type whose argument constrains its spelling, declared as
`pub(repr) type Ident (Spelling = String) where Spelling <: String = Spelling`.
`Ident` and `Ident String` both accept any identifier — `Spelling` defaults to
`String` (see [Default type parameters](#default-type-parameters)) — and
`Ident "else"` accepts exactly the identifier `else`, because every string
literal type is a subtype of `String` (see [Subtype
bounds](#subtype-bounds)). A literal identifier parameter may use
a typed wildcard when its syntax is not needed, or a binding when it is:

```staple
macro conditional =
    condition: Expr =>
    then_branch: Expr =>
    _: Ident "else" =>
    else_branch: Expr =>
    parse_quote {
        match $condition {
            True => $then_branch,
            False => $else_branch,
    }
}
```

Delimited syntax parameters match one balanced source argument while exposing
its contents structurally. Product commas separate expected children and do
not require commas in the source, so `Parenthesized (Ident String, Ident
String)` accepts `(left right)`. `Parenthesized`, `Bracketed`, and `Braced` may
instead contain `Sequence T`, which accepts zero or more consecutive values of
`T`; for example, `Bracketed (Sequence Ident String)` accepts `[]` and `[one
two]`. A function-style macro may also use one `Sequence T` as a top-level
parameter when a later parameter is guaranteed to consume source syntax:

```staple
macro collect =
    values: Sequence (Ident String) =>
    _: Equals =>
    body: Braced Syntax => ...
```

The sequence consumes zero or more consecutive top-level arguments. Matching
starts greedily and backs up until the remaining parameters match, so the
following `Equals` terminates `values`. `Visibility` and
`MacroCallVisibility` do not count as terminators because they may supply an
implicit `Private` value without consuming source. A top-level sequence may
not be the last parameter, appear more than once in a signature, or be used by
a modifier macro. `Sequence` inside delimiters remains restricted to the
entire contents of one of the three delimiter types.
Captured sequences may be passed through compile-time helpers and destructured
as `Sequence ()` or `Sequence (first: T, rest: Sequence T)`. `Sequence
SyntaxNode` uses shortest structural atoms, treating a nested balanced delimiter
as one atom. In contrast, `Parenthesized Syntax`, `Bracketed Syntax`, and
`Braced Syntax` capture their entire contents as one opaque fragment, including
trivia and syntax that is not yet structurally supported. A single fixed
content never needs the grouping parens that separate multiple ones, so
`Parenthesized Expr` and `Parenthesized (Expr)` are read identically; the
parens are required only once there is more than one fixed element, as in
`Parenthesized (Ident String, Ident String)`.

`Comma` is singleton syntax and may be matched explicitly, as in
`Parenthesized (Ident String, Comma, Ident String)`. A homogeneous sequence
whose elements are separated by commas uses `Separated (T) Comma`, for example
`Parenthesized (Separated (Ident String) Comma)`. It accepts empty and singleton
contents, requires one comma between subsequent elements, and permits a
trailing comma. Its value contains the elements without the commas, together
with its separator and whether the source had a trailing separator. Fresh
values use `Separated (separator: Comma, elements: (...), trailing: True)` or
the corresponding `False` value.
Like `Sequence`, `Separated` is valid only as the complete contents of a
delimiter. Its `elements` field is a `Sequence`. A separated element may be a
fixed product shape. `Optional T` is available only inside such products and
captures `None` or `Some value`, so
`Braced (Separated (Ident String, Optional Type) Comma)` accepts both
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
fields yet, but may be stored, passed through compile-time helpers, and spliced
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
pub macro quote: Braced Syntax -> Syntax
pub macro parse_quote: Braced Syntax -> Syntax
```

These signatures describe the syntax emitted by the intrinsic macros, not the
type of the compile-time syntax value that the emitted expression constructs.

`parse_quote` uses its contextual type to choose the kind of syntax value its
emitted expression constructs. Supported contexts are `SyntaxNode`, `Expr`,
`Type`, `Pattern`, `Item`, `Sequence Item`, `Comma`, `Equals`, `FatArrow`,
`Visibility`, and the three delimited syntax types (`Parenthesized`,
`Bracketed`, `Braced`) with any of their supported content shapes — never
`Syntax`, which is `quote`'s own always-opaque value, not a structured syntax
node `parse_quote` can construct contextually.
`SyntaxNode` requires exactly one shortest
structural node. The grammatical categories parse the complete fragment in
their respective contexts, while `Sequence Item` parses zero or more complete
items in source order. Annotations, `satisfies`, declared helper or macro
result types, and other typed compile-time boundaries provide `parse_quote`'s
contextual construction type; requesting `Syntax` through any of them is rejected. A
`parse_quote` reached with no contextual type at all — for example inside an
untyped compile-time helper — is rejected the same way. This contextual
selection is specific to `parse_quote`: wrapping a `quote` fragment in
`satisfies` or a typed binding does not reinterpret it, since `quote` always
returns opaque `Syntax`.

A macro's declared or inferred result is also checked against its body: when
that result is a concrete syntax type — `SyntaxNode`, `Expr`, `Type`,
`Pattern`, `Item`, `Sequence Item`, `Comma`, `Equals`, `FatArrow`,
`Visibility`, or a delimited syntax type — but the body's tail expression is
a bare `quote { ... }`, the mismatch is rejected at the declaration, since
`quote` can only ever produce opaque `Syntax`. Replacing that `quote` with
`parse_quote` resolves it.

Opaque fragments may contain empty input, punctuation, comments, or temporarily
malformed grammar and may be spliced into a later `parse_quote` for contextual
interpretation. A raw `Syntax` result entering ordinary expression or item
source is reparsed for that placement. `$name` splices a captured value while
preserving its hygiene. `$items...` splices a `Sequence Item` in an item-list
position.

Identifiers and calls may also be inspected, constructed, and changed as
compile-time values:

```staple
macro replace_argument = value: CallExpr => replacement: Expr => {
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

macro @replace: Parenthesized Expr -> Item -> Item =
    value => item => parse_quote { let generated = $value }

@identity
@replace(42)
def original = () => 0
```

`@doc` is a built-in modifier that attaches Markdown documentation to a named
declaration. It requires one string literal, may be repeated, and is shown by
editor hover:

```staple
@doc("Returns the number of items.")
pub def length: List T -> USize = list => ...
```

Triple-slash comments are shorthand for `@doc`, with the text after `///`
used verbatim. Consecutive documentation comments preserve their source order:

```staple
/// Returns the number of items.
/// This operation does not mutate the list.
pub def length: List T -> USize = list => ...
```

Documentation may be attached to types, bindings, modules, macros, traits,
trait members, implementation members, and extern bindings. It has no runtime
effect. `@doc` is compiler-reserved; qualified user modifiers named `doc`
remain ordinary modifiers.

A modifier has signature `Item -> Item`, `Item -> Sequence Item`, or
`Item -> Syntax`, optionally preceded by one optional explicit
`Parenthesized Expr`, `Parenthesized Type`, or `Parenthesized Pattern`
argument before the compiler-supplied item. `Parenthesized` is required
because the explicit argument is always delimited by parentheses, including
an expression argument, matching how every other delimited macro argument is
declared. `Ident`, `CallExpr`, and `UnstructuredExpr` may be used as narrower
expression parameter types. `SyntaxNode`, opaque `Syntax`, and `Item` are not
valid explicit modifier arguments.

Modifier lists are part of item syntax. The closest modifier runs first, so
`@outer @inner def ...` expands as `outer(inner(def ...))`. A modifier may
return an item that itself has modifiers; those modifiers are expanded before
the surrounding chain continues. Modifiers may target `let`, `def`, `type`,
`extern`, `trait`, and `impl` items. They cannot target expression statements,
`use`, `mod`, or `macro` declarations. Modifier and function-style macro names
occupy separate namespaces, and imports, renames, namespaces, and re-exports
carry both.

Modifiers may also target eligible private items in block expressions. A block
modifier may produce zero, one, or many replacement items, which are spliced at
the modifier's position. Every result must itself be valid in a block, so public
items and `extern`, `macro`, `trait`, or `impl` declarations are rejected.

A modifier declared `Item -> Item` must return exactly one `Item`, as before.
A modifier declared `Item -> Sequence Item` or `Item -> Syntax` may instead
expand to zero, one, or many items, reusing the same reparse and splice
machinery as an item-producing function-style macro. When such a modifier is
not the outermost one applied to the item — that is, a further modifier in
the same chain still has to consume its result — only the *first* generated
item continues the chain: it becomes the item handed to the next modifier
out, and any of its own nested modifiers are expanded first, exactly as for a
single-item result. Every other generated item is deferred and spliced in
after the chain's eventual result, in the order it was produced, untouched by
the remaining modifiers in this chain. A modifier that produces zero items in
a non-outermost position has nothing to hand the next modifier and is
rejected. At the outermost position, all items produced by that final
application — combined with any items deferred by earlier modifiers in the
chain — become the item's replacement; zero items there deletes the item
entirely, matching how an item-producing function-style macro invocation with
no replacement behaves.

`Item` exposes type declarations as `TypeDeclarationItem`, including their
kind, name and spelling, applied declared type, flattened type-parameter names,
and optional opaque representation. All other items match `UnstructuredItem`
and remain lossless but opaque. `BindingPattern` and `NominalPattern` provide
the structured pattern construction needed by declaration modifiers.

Visibility is also compiler-owned syntax. Its three atomic variants are
`Private`, `Public`, and `PublicRepr`. They can be matched and passed through
compile-time helpers accepting `Visibility`, but cannot survive expansion as
runtime values. `MacroCallVisibility` is a special marker permitted only as
the first parameter of a function-style macro:

```staple
macro define_alias =
    vis: MacroCallVisibility =>
    ty: Type =>
    parse_quote { $vis type alias Generated = $ty }

pub define_alias I32
```

`pub` or `pub(repr)` before the macro name supplies `Public` or `PublicRepr`.
An unprefixed call supplies `Private`. The captured value is accepted anywhere
that expects `Visibility`; this is a compiler-owned compatibility rule rather
than general language subtyping. A prefixed call is parsed in module-item
grammar, but its macro may return either an item or an expression. Normal
syntax placement rules still reject type, pattern, or visibility results that
have no valid placement.

An ordinary `Visibility` parameter may appear in any position. At that
position `pub` or `pub(repr)` consumes one source atom; if neither is present,
`Private` is injected without consuming the following argument:

```staple
macro configure =
    value: Expr =>
    vis: Visibility =>
    ty: Type => ...

configure value I32
configure value pub I32
configure value pub(repr) I32
```

Visibility-aware overload matching ranks candidates by source atoms consumed,
not by the number of parameters after implicit values are inserted. Therefore
an ordinary overload and an implicitly-private overload consuming the same
source syntax are ambiguous. An explicit prefix before the macro name selects
only overloads beginning with `MacroCallVisibility`.

`Visibility` is also a `parse_quote` result type, following the same
absence-means-`Private` convention: `parse_quote { }: Visibility` yields
`Private`, `parse_quote { pub }: Visibility` yields `Public`, and
`parse_quote { pub(repr) }: Visibility` yields `PublicRepr`.

Inside an item quotation, a visibility value may be spliced immediately before
a declaration. `Private` emits no prefix, `Public` emits `pub`, and
`PublicRepr` emits `pub(repr)`. Existing declaration rules are checked after
substitution. `PublicRepr` is valid on represented distinct types and on
singleton types, where representation visibility is a no-op.
Modifiers surrounding a visibility-aware call run after that call has produced
its item.

Quotations may contain multiple items and return `Sequence Item`. Such a
sequence can replace a top-level macro invocation or be inserted into an inline
module with `$items...`. Identifier syntax may be spliced into generated module
and type names. `parse_quote` type quotations are selected contextually by an
annotated compile-time binding or `satisfies Type`:

```staple
let alternative: Type = parse_quote { $group.$variant }
parse_quote { $left | $right } satisfies Type
```


Macros are hygienic. Names and bindings written in a quotation retain the
definition module's environment and receive a fresh expansion identity, while
spliced expressions retain their caller environment. A macro normally consumes
the number of arguments described by its curried syntax-node parameter types;
a top-level `Sequence` instead consumes as many matching arguments as possible
while preserving a match for its required suffix. Further call arguments apply
to the expanded expression.

Macros may be overloaded by declaring the same name more than once in one
module. An invocation selects the complete matching overload that consumes the
most syntax atoms. Same-length matches use pattern specificity: a literal
identifier is narrower than `Ident String`, `Ident String` is narrower than
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
mutable local cells, products, matches, literals, recursion, and pure integer
operations. It rejects external or runtime-only effects. Expansion is limited
to 128 nested macros and each top-level invocation is limited to 1,000,000
evaluation steps. Syntax
values are compile-time-only. This release supports `parse_quote`-driven
contextual expression, type, pattern, item, and item-sequence quotation;
`quote`'s opaque fragments; structured identifier and call expressions;
scalar splices; and repeated item splices.
It also supports opaque item input through modifier macros and atomic visibility
syntax. General repeated expression, type, and pattern splices,
function-style item input, item inspection, and structured access to other
expression forms remain future work.

Compiler-provided macros use typed bodyless contracts. `std.syntax` declares
`pub macro quote: Braced Syntax -> Syntax` and
`pub macro parse_quote: Braced Syntax -> Syntax`, and
`std.cinterop` declares `pub macro c_string: Expr -> Expr`. Neither module is
re-exported by `std.core`; their APIs require explicit imports.

## Source files

staple source files use the `.sta` extension.

Whitespace and comments are preserved by the parser, so parsing and reproducing
a source file does not change its text. Newlines also separate adjacent
statements where they would otherwise form a single expression. A semicolon may
be written after any item as an explicit separator, including after the last item in a sequence.

Line comments begin with `//` and continue to the end of the line.

### Top-level statements

A source file may contain expression statements alongside bindings, type
declarations, and foreign declarations:

```staple
use std.cinterop.*

extern "c" {
    let printf: (CPointer CChar, ...) -> I32
}

printf (c_string "hello, world!\n")
```

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

Every `.sta` file is a module. Each compilation has a module root. A dotted
module path is resolved from that root by replacing dots with path separators
and adding `.sta`:

```staple
use tools.format
// loads tools/format.sta
```

Paths beginning with `std` resolve from the standard-library module root rather
than the package module root. `std.core` is the stable prelude interface and
provides core numbers, booleans, strings, references, results, syntax,
equality, copying, dropping, defaults, and indexing traits. Interoperability features are
available through `std.cinterop`.

The contextual leading component `package` always selects the current package
root, bypassing inline children with the same name:

```staple
use package.models.User
```

Here `models` loads `<module-root>/models.sta`. A bare `use package` refers to
the entry module. A file named `package.sta` is addressed as
`use package.package`.

Public items may also be used through a root-qualified name without a `use`
declaration:

```staple
std.io.println "Hello"
let user: package.models.User = package.models.default_user
```

The prefix before the item is resolved as a module path using the same
longest-file-prefix and public-inline-submodule rules as `use`. Such a reference
loads the target module and establishes an initialization dependency. Only the
explicit roots `std` and `package` receive this treatment; other dotted
expressions remain namespace or product access. `use` remains useful for short
local names and is required for re-exporting.

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
use path.to.another_module.(func, MyType)
use path.to.another_module.func
use path.to.another_module.func as my_func
```

The wildcard form imports every public named item. The parenthesized form
imports only the listed items. The `as` form imports one item under a different
local name. Imports are hoisted and apply throughout their module. Prefixing an
item import with `pub` re-exports it from the importing module:

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
    pub type alias Number = I32
    pub let answer: Number = 42
}
```

### Type companions

Named types and type aliases may have companion items. A companion behaves as
the type's namespace, so its public members are selected through the type name:

```staple
type Animal = ...

companion Animal {
    pub def move_to = animal: Animal => position: (F32, F32) => animal
}

let animal: Animal = ...
let moved = Animal.move_to animal (1.0, 1.0)
```

Companion functions can also be called with postfix method syntax. The
receiver is supplied as the function's first curried argument:

```staple
let moved = animal^move_to (1.0, 1.0)
// Equivalent to Animal.move_to animal (1.0, 1.0)
```

A bare `animal^move_to` is the receiver-applied value, so it may be a
partially-applied function when the companion function accepts more arguments.
Method lookup uses the receiver's static named type; aliases retain companion
identity when introduced by an annotation or an explicitly typed function
parameter or result.

More than one companion block may contribute to the same type. Companion
blocks have no visibility of their own; visibility is declared on their items.
Their bodies can refer directly to declarations in the containing module.
Generic and constrained targets use the same bracketed parameter syntax as
trait implementations, for example `companion<T where Bound T> Box T`.
Ordinary `mod` declarations may not share a name with a type; use `companion`
when defining a type-qualified namespace.

An external import resolves the longest existing `.sta` file prefix and then
traverses public inline submodules. Thus `use library.api.answer` first prefers
`library/api.sta`; if that file does not exist, it loads `library.sta` and looks
for a public `api` submodule.

A parent can re-export public items from a child, including a private child:

```staple
mod implementation {
    pub def format = value => value
    pub type alias Number = I32
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
pub type alias Number = I32
```

`pub extern` exports every binding declared by that external block.

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
`Ref`, requires the same `mut` root:

```staple
let mut counter = 0
counter = counter + 1

let mut point = (x: 3, y: 4)
point.x = 5

let mut cell: Ref I32 = Ref 1
cell.0 = 2

let borrowed: Ref I32 = Ref 3
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

An external declaration may omit its value because its implementation is
provided outside staple:

```staple
use std.cinterop.*

extern "c" {
    let printf: (CPointer CChar, ...) -> I32
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
def fibonacci: I32 -> I32 = n =>
    match n < 2 {
        True() => n,
        False() => fibonacci (n - 1) + fibonacci (n - 2),
    }

const y = fibonacci 10
```

An initializer must fold to an integer, a string, or a product (including a
nested product) of those; a value that cannot be represented this way —
a function, a resource, and so on — is a compile-time error. `const` requires
an initializer, and, like a compile-time helper's evaluation generally, an
initializer that recurses without bound (including through a self-referential
`const`) is rejected rather than left to hang or exhaust memory.

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

The syntax currently recognized for literal values includes strings, integers,
and decimal floating-point numbers:

```staple
"hello"
42
1.0
.5
1e3
```

Float literals accept decimal points and scientific notation. They use an
expected `F32` or `F64` type when available and otherwise default to `F64`.
`1.` is a float literal, while `1.field` remains member access. Integer literals
remain integer values; Staple does not implicitly convert between numeric types.

String literals use double quotes and produce owned UTF-8 `String` values. A
backslash protects the following quote from ending the string. Supported
escapes are `\\n`, `\\r`, `\\t`, `\\0`, `\\\\`, `\\"`, and `\\$`.

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

The primitive `c_string` macro from `std.cinterop` accepts only a string literal
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
directly to a call against a standard prelude trait, and arithmetic is not
implemented with function overloading:

| Operator(s) | Precedence | Associativity | Desugars to |
|---|---|---|---|
| `*`, `/` | 7 | left | `Multiply.multiply`, `Divide.divide` |
| `+`, `-` | 6 | left | `Add.add`, `Subtract.subtract` |
| `==`, `!=`, `<`, `<=`, `>`, `>=` | 4 | none | `Eq.equal`/`Eq.not_equal`, `PartialOrd.lt`/`le`/`gt`/`ge` |
| `..`, `..=` | 3 | none | the prelude's `range`/`range_inclusive` functions |
| `&&` | 2 | left | not a call; see below |
| `\|\|` | 1 | left | not a call; see below |

Function application and access bind tighter than every operator above.
Chaining two non-associative operators at the same precedence (for example
`1 == 2 == 3`) is a parse error; parenthesize to disambiguate. The standard
integer implementations of `Add`, `Subtract`, `Multiply`, `Divide`, `Eq`, and
`PartialOrd` are backed by private compiler intrinsics. `..`/`..=` are not
trait-based: they call the prelude's `range`/`range_inclusive` functions
directly, which construct `Range T`/`RangeInclusive T` values.

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
designator cannot name an unknown field, initialize a position already
consumed by the positional prefix or another designator, or leave any expected
position uninitialized. Positional elements and positional spreads cannot
appear after the first designator. Initializer expressions are evaluated once
in source order even though designated values are placed in product order.

Ordinary `name: value` syntax never performs designated placement. Designated
initializers also do not combine with a `...=` named spread. Named spreads keep
their separate merge semantics described below, including later-field
overrides.

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

Fixed homogeneous products may be written with repetition syntax. `T[N]` is
exactly equivalent to a product containing `N` copies of `T`, rather than a
distinct array type. In particular, `T[0]` is `()` and `T[1]` is `T`. A fully
expanded product may contain at most 65,535 elements.

Product type elements can be flattened explicitly with `...`:

```staple
let values: (String, ...I32[3])
// Equivalent to (String, I32, I32, I32)
```

A spread operand must be a fixed product. Explicit `...T[0]` and `...T[1]`
contribute zero and one elements respectively. A bare trailing `...` retains
its separate meaning in a C-variadic function parameter type.

Fixed product values can be flattened in the same way:

```staple
let coordinates = (x: 10, y: 20)
let entry = (name: "origin", ...coordinates, visible: True)
// Equivalent to (name: "origin", x: 10, y: 20, visible: True)
```

Multiple value spreads may appear in any position. Each spread operand is
evaluated exactly once and must have a fixed product type; erased products,
references, and scalar values cannot be spread. Names belonging to the spread
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

`Index` has target, position, and output parameters, with the target and
position determining the output. User-defined implementations may use any
position type. The compiler derives `Index P USize Output` for every non-empty
fixed product whose elements are all `Copy`; `Output` is the duplicate-free sum
of its element types. Thus indexing `(I32, String, I32)` produces
`I32 | String`, while indexing `I32[N]` produces `I32`. It also derives `Index`
for fixed and erased homogeneous references when their element type is `Copy`.
Known bad fixed-product indices are rejected and dynamic out-of-bounds indices
trap.

`MutateIndex` replaces one element through a target in place. Indexed
assignment delegates only to this trait:

```staple
target[index] = replacement
// Equivalent to MutateIndex.mutate_index (target, index, replacement)
```

The compiler derives `MutateIndex` for non-empty homogeneous fixed products, by
value, and for fixed and erased homogeneous references. The mutable `Target`
parameter passes by address either way (see the "Mutable parameters" subsection
under "Functions"), so a by-value target's root binding must be declared
`mut` just as a `Ref` target's must. These structural implementations cannot
be overridden. Other types may define ordinary explicit implementations of
both traits.

Products have a structural `Default` implementation when every element type
implements `Default`. The expected type determines the result of `default ()`:

```staple
let zeros: I32[4] = default ()
let mixed: (I32, Bool, String) = default ()
```

Each product element is initialized by a separate call to its element type's
`Default.default` member. This is construction, not a `repeat` operation: it
also works for heterogeneous and named products, and does not copy one value
into every position. The standard integer defaults are zero, `Bool` defaults
to `False`, and `String` defaults to the empty string.

The empty product is default-constructible without an element constraint.
Because `T[1]` is normalized to `T`, its default is simply the default of `T`.
Distinct types can define their own explicit `Default` implementation, while
product implementations are structural and cannot be overridden. `Ref T` does
not implement `Default` merely because `T` does.

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
single representation value of a distinct type when that representation is
visible.

`=>` introduces the body of the abstraction. The compiler infers the result
type from the body unless a surrounding function type or a `satisfies`
expression constrains it.

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
type UserId = I32
def unwrap: UserId -> I32 = UserId value => value

let user: UserId = UserId 42
let UserId value = user
```

The same syntax works inside products and with qualified type names. A generic
nominal pattern must receive its applied type from the value being destructured
or a surrounding function annotation.

A singleton product pattern is equivalent to its contained pattern, so
`(value: T)` matches the same values as `value: T`.

A function body may explicitly constrain its result with `satisfies`:

```staple
let get_number = () => {
    42
} satisfies I32
```

`<expression> satisfies <type>` is a general type-ascription expression, not a
runtime conversion. It checks the expression with the given expected type and
evaluates to the expression's value. `satisfies` has lower precedence than
function application and infix operators, so `a + b satisfies I32` means
`(a + b) satisfies I32`. Parentheses may constrain a smaller expression.

When a `satisfies` expression is a function body, its type constrains the
function result, including values produced by explicit `return` statements.
For named bindings, a complete binding annotation is the canonical way to
write both the parameter and result types:

```staple
def identity: I32 -> I32 = value => value
```

A binding annotation may also constrain the complete function type. A function
declaration uses the same function-type syntax and omits the value:

```staple
let add: (x: I32, y: I32) -> I32
```

### Generic functions

A function-valued `def` may introduce compile-time type parameters in angle
brackets before its ordinary function type:

```staple
def identity: <T> T -> T = value => value
def first: <A, B> (A, B) -> A = (a, b) => a
def choose: <T> (Bool, T, T) -> T = (condition, a, b) => {
    // ...
}
```

Product-pattern parameters are not permitted inside the angle brackets
themselves — `first`'s two type parameters are declared as `<A, B>`, flattened,
even though its value parameter is the product `(A, B)`.

The compiler infers concrete type arguments from call arguments and the
expected result or function type. A generic function can therefore be used as
a first-class value when its context fixes a concrete function type:

```staple
let int_identity: I32 -> I32 = identity
```

Generic functions are rank-1: an ordinary parameter or result type cannot
itself contain an uninstantiated generic scheme. Compile-time parameters must
be declared explicitly; unannotated definitions are not generalized. Each
reachable concrete use is monomorphized, unused instantiations produce no code,
and recursive calls must retain the current specialization. A top-level
generic `let` and `extern` declarations are not supported, since nothing ever
supplies concrete type arguments for them the way a call site does for a
`def`. A local `let` inside a generic `def`'s body may mention that def's own
compile-time parameters, since it is specialized along with the rest of the
body at each concrete use:

```staple
def make_list: <T where Default T> () -> T[32] = () => {
    let list: T[32] = default ()
    list
}
```

Every compile-time type parameter has an implicit `Sized` bound. It may
therefore appear in by-value parameters, results, products, and represented
types. A declaration that only stores or forwards a type behind a constructor
which accepts unsized arguments can relax that default after introducing the
parameter:

```staple
def preserve_ref: <T where ?Sized T> Ref T -> Ref T = value => value
```

The `?Sized T` relaxation must name an already introduced parameter and may
appear only once for that parameter within the `where` clause. It is a
relaxation, not a trait bound: `where Sized T` remains the ordinary
bounded-generic spelling. A relaxed parameter cannot itself be passed or
returned by value. `Sized` is compiler-derived, so explicit `impl Sized`
declarations are rejected.

### Typed resources

A typed resource is an implicit value identified by its concrete nominal type.
There is no separate resource declaration: any fully concrete, sized nominal
type whose representation is compiler-derived `Copy` may be used. For example:

```staple
type Clock = (
    now: () -> I32,
)

type Logger = (
    write: String -> (),
)
```

Transparent aliases use the identity of their underlying nominal type. A
structural type, ordinary opaque type, unspecialized generic type, unsized type,
or move-only nominal type is not eligible. The compiler-represented opaque
`std.io.IO` and `std.core.Reactive` are compiler-represented opaque exceptions. Consequently resources never add
a new borrowing or ownership mode: their hidden values can always be copied.

Function arrows list their effects in braces. Effect sets are
unordered and duplicate-free, and each arrow in a curried type has its own set:

```staple
def timestamp: () ->{Clock} I32 = () => {
    let clock = resource Clock
    clock.now ()
}
```

Generic functions may quantify over an effect set with an `effect` parameter:

```staple
def twice: <effect E> (() ->{E} ()) ->{E} () = f => { f (); f () }
```

An effect variable may represent the empty set, resources, state effects, or a
combination of them. It can form an open row with fixed effects, as in
`{E, IO}`; inference chooses the minimal substitution satisfying every
occurrence. Each set may contain at most one variable. Effect parameters exist
only on generic function bindings, may appear only in effect sets, have no
runtime representation, and are concrete before code generation.

### Signals and reactions

A signal is a writable binding whose ordinary reads and writes participate in
runtime dependency tracking. Its visible type remains the value type:

```staple
let signal count = 0
count = count + 1
```

`reaction` accepts a zero-argument callback (and therefore benefits from
call-site implicit thunking), runs it immediately, and reruns it synchronously
after every assignment to a signal read by its previous run. Dependencies are
recollected on every run. Assigning the same value still notifies dependents.

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
Separate assignments remain unbatched. A reaction that synchronously triggers
itself, or a derived binding that reads itself while evaluating, traps instead
of recurring indefinitely.

### Call-site implicit thunking

At a function call, an expression of type `T` may be supplied to a parameter
of type `() -> T`. The compiler treats that argument as `() => expression`, so
the expression is evaluated only when the callback is invoked:

```staple
def evaluate: (() -> I32) -> I32 = callback => callback ()

let answer = evaluate { expensive_computation () }
```

This adaptation is restricted to call arguments. It is not a general coercion,
so `let callback: () -> I32 = 42` remains invalid. A directly compatible
function value always wins over implicit thunking. The generated callback
captures values and infers the effects of its body exactly like an explicit
anonymous function; this allows effect-polymorphic higher-order functions to
preserve deferred resource and state effects.

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
recursion, trait methods, and function values. An unannotated function infers
the minimal required set. An explicit set remains part of its declared
contract: resources may be unused, but the body cannot require an unlisted
resource. Function types with different effect sets are distinct and are not
implicitly widened.

Resources are supplied lexically with `with`:

```staple
with Clock = system_clock {
    timestamp ()
}
```

The provider is evaluated before its binding is installed. A nested provider
of the same concrete type shadows the nearest outer provider, while its own
initializer can still use the outer value. The body result is the result of the
`with` expression. Creating a function inside `with` does not capture the
provider: the function keeps the resource in its type and receives it when it
is called.

Resource values are passed as hidden `Copy` parameters, after the closure
environment and before explicit arguments. Executable top-level initialization
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

Macros may quote, splice, generate, and transform resource syntax. Attempting
to evaluate `resource` or `with` as a compile-time macro operation is rejected;
providers exist only in runtime lexical scopes.

### Mutable parameters

`mut` on the parameter side of a function arrow declares which arguments the
function may write into. It is separate from callable effects such as `IO` and
`state`:

```staple
def f1: mut A -> () = a => { ... }                   // may mutate the whole parameter
def f2: (mut A, B) -> () = (a, b) => { ... }        // may mutate parameter 0 only
def f3: (mut a: A, b: B) -> () = (a, b) => { ... }  // same, named
def f4: (mut a: A, b: B) ->{IO} () = (a, b) => { ... }
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
the top-level parameter product. If a function has both parameter markers and
an explicit annotation, their mutation targets must match exactly. The marker
does not introduce an ordinary mutable local; it declares the function
parameter position that is passed by address.

The effect permits both writing through a parameter and replacing the
parameter value itself. A mutable argument is passed by address, so either
kind of change is visible in the caller:

```staple
def clear: mut Ref (I32, I32) -> () = cell => { cell.0 = 0 }

let mut counter: Ref (I32, I32) = Ref (1, 2)
clear counter

let fixed: Ref (I32, I32) = Ref (1, 2)
clear fixed // error: `fixed` is not declared `mut`

def replace: mut I32 -> () = value => { value = 42 }
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
    partial_cmp: T -> T -> Option Ordering
    lt: T -> T -> Bool = left => right => match (partial_cmp left right) {
        Some Less() => True,
        _ => False,
    }
}

trait Ord T where Eq T, PartialOrd T {
    cmp: T -> T -> Ordering
}

trait Increment T {
    increment: T -> T
    increment_twice: T -> T = value => increment (increment value)
}
```

A trait may declare functional dependencies in its `where` clause, alongside
any prerequisite trait bounds. A dependency states that the parameters on the
left uniquely determine the parameter on the right:

```staple
trait Iterator Iter Item where Iter ~> Item {
    next: Iter -> IterStep (Iter, Item)
}

trait Add Left Right Output where {Left, Right} ~> Output {
    add: Left -> Right -> Output
}
```

The left side is either one parameter or a non-empty comma-separated set in
braces. The right side is one parameter. A trait may declare multiple
dependencies, including chains such as `where A ~> B, B ~> C`. Dependencies and
prerequisite trait bounds are comma-separated entries of the same `where`
clause and may be mixed freely, as in `where Source ~> Iter, Iterator Iter`.

A dependent argument may be written as `_` when all of its determinants are
known. If every remaining argument is inferable, the trailing arguments may be
omitted entirely. These forms are equivalent:

```staple
Iterator Iter Item
Iterator Iter _
Iterator Iter

Add I32 I32 I32
Add I32 I32 _
Add I32 I32
```

An underscore in a non-dependent position is an error. Product binders may
infer individual elements, as in `Convert (From, _)`, when the corresponding
parameter is functionally dependent. Trait implementation headers do not
support this inference and must name every argument explicitly; see
[Generic trait implementations](#generic-trait-implementations) below for how
an implementation may still be generic over its own compile-time parameters.

Functional dependencies are global coherence promises. Two implementations
cannot agree on every determinant while choosing different dependent types;
contradictory generic bounds are rejected for the same reason.

Trait members must have function types, must mention every trait parameter, and
cannot contain inferred types. Traits may be exported with `pub trait` and are
imported with the same namespace, selected, renamed, and glob forms as other
public declarations.

An implementation supplies every trait member exactly once for a fully concrete
type. Member types are taken from the trait and do not need to be repeated:

```staple
impl ToString I32 {
    def to_string = number => {
        // ...
    }
}

impl Add I32 I32 I32 {
    def add = (left, right) => left + right
}

impl Convert (I32, String) {
    def convert = value => "converted"
}
```

Curried binders consume successive arguments, while a product binder consumes
one product argument. Parentheses group an applied or otherwise complex type as
one argument. Implementation arguments may be built-in, nominal, product,
pointer, or function types, and must be fully concrete except where they
mention the implementation's own compile-time parameters (see
[Generic trait implementations](#generic-trait-implementations) below).
Implementations have no visibility modifier: every implementation in the
loaded program is available globally. Defining the same trait/argument
combination twice, including through aliases of the same types or through
alpha-equivalent generic headers, is an error.

A trait may place one or more prerequisite bounds in a `where` clause between
its parameters and member block. Every implementation must satisfy the
instantiated prerequisites, and a generic bound makes its prerequisites
available transitively:

```staple
def equal_ordered: <T where Ord T> T -> T -> Bool = left => right => {
    left == right
}
```

Prerequisite cycles are rejected. Prerequisites participate only in static
checking and do not add runtime dictionaries or function parameters.

A member may provide a default function body after `=`. An implementation may
omit a defaulted member or replace it with an explicit member of the same name.
Calls from a default body use normal trait dispatch, so an explicit member on
the concrete implementation overrides a sibling default. Default bodies are
generic over the trait parameters and are monomorphized only when used.

A generic `def` adds one or more trait bounds in a `where` clause inside its
angle-bracketed compile-time parameter list, before its ordinary function
type:

```staple
def print: <T where ToString T> T ->{IO} () = value => {
    print_string (to_string value)
}

def combine: <L, R, O where Add L R O> (L, R) -> O = pair => {
    Add.add pair
}
```

Bounds are explicit and must be propagated by other generic functions. A
concrete use must have a matching implementation. Trait members are first-class
function values and may be called unqualified when unambiguous or qualified as
`ToString.to_string`; a namespace-qualified trait may be used as
`strings.ToString.to_string`.

Traits use static dispatch. Bounds and implementations add no runtime values or
function parameters. During monomorphization, the compiler substitutes the
concrete trait arguments and emits a direct reference to the selected
implementation member. An entire implementation may be generic over its own
compile-time parameters (see below); an individual trait member may not
independently introduce parameters beyond the ones its implementation
declares. Trait objects, runtime dictionaries, and associated items are not
currently supported.

### Generic trait implementations

An implementation may itself be generic, using the same angle-bracketed
`<...>` parameter list and `where`-clause syntax as a generic `def` or
`macro`. A generic implementation applies conditionally: it is available for
any instantiation of its own compile-time parameters that satisfies its
bounds.

```staple
trait Bound T { check: T -> Bool }
trait Target T { act: T -> T }

impl Bound I32 { def check = value => True }

impl <T where Bound T> Target T {
    def act = value => value
}
```

Here `Target` is implemented for every `T` that implements `Bound`, so
`Target.act` is available for `I32` because of the concrete `Bound I32`
implementation above. A generic implementation's own bounds are available
inside its members' bodies, exactly as they are inside a bounded generic
`def`. `?Sized` relaxations and `<:` subtype bounds may also appear in an
implementation's `where` clause, alongside trait bounds, exactly as in a
generic `def`.

Two implementations that are alpha-equivalent — the same header shape up to
renaming the compile-time parameters — are rejected as duplicates, the same as
two concrete implementations for the same arguments. The compiler does not
perform full overlap checking between implementations, so a concrete
implementation and a generic implementation whose bound happens to be
satisfied by the same concrete type may both be declared; dispatch reports an
ambiguous-implementation error only where a call site is actually reachable
that both would apply to.

### Subtype bounds

A generic `def` or `type` may also bound a compile-time parameter with `<:`,
placed alongside trait bounds in the same `where` clause:

```staple
def string_identity: <T where T <: String> T -> T = x => x
```

`T <: SuperType` is not backed by an `impl`; it is checked against a fixed
subtyping relation instead:

- `T <: T` (reflexivity);
- every string literal type is a subtype of `String`, and a literal type whose
  values are a subset of another literal type's values is a subtype of it;
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
parameters](#default-type-parameters)): `pub(repr) type Ident (Spelling =
String) where Spelling <: String = Spelling`.

### Default type parameters

A `type` or `trait` may give one or more of its trailing compile-time
parameters a default with `=`, written inline with the parameter. Because a
juxtaposed parameter list has no other delimiter, a defaulted parameter must
be parenthesized to disambiguate the `=` from the trailing `=` that precedes
a type's body or the `{` that opens a trait's member block:

```staple
type Box (T = String) = (value: T)
type alias Pair A (B = A) = (A, B)
trait Increment (T = I32) { increment: T -> T }
```

The default is part of that one parameter's own parentheses; it is not a
separate clause. It may be combined with a subtype or trait bound in the
`where` clause, as in `Ident`'s spelling parameter (see
[Metaprogramming](#metaprogramming)), which declares a default together with a
subtype bound: `pub(repr) type Ident (Spelling = String) where Spelling <:
String = Spelling`. Only a plain named parameter can carry a default — a
product or splice pattern in the parameter position is a parse error if
followed by `=`.

At a use site, trailing arguments may be omitted as long as every parameter
from that point on has a default; the omitted arguments are filled in from
their defaults instead of leaving the type partially applied:

```staple
let boxed: Box = Box (value: "hi")       // T defaults to String
let explicit: Box I32 = Box (value: 42)  // an explicit argument overrides the default
```

A default expression may refer to an earlier parameter in the same list —
`Pair I32` above supplies only `A`, and `B`'s default resolves it to `I32` too
— but not to a later one. If any parameter from the first omitted one onward
lacks a default, none of the trailing arguments are filled in and the type is
left partially applied as before. A filled-in default is still checked
against that parameter's own subtype and trait bounds, exactly as an explicit
argument would be.

Trait defaults apply the same way to under-supplied trait arguments, in both
an `impl` header and a bound clause — complementing the functional-dependency
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

Because each function accepts one value, passing several logical arguments
means passing a product:

```staple
printf (c_string "%s\n", CString.from_string s)
```

Here, `printf` receives one two-element product.

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

The fixed operators (`+ - * / == != < <= > >= .. ..=`, see
[Values and expressions](#values-and-expressions)) desugar to the same shape:
an operator call supplies its left and right operands through two curried
calls against the operator's trait method or prelude function.

```staple
1 + 2
1 == 2
```

`1 + 2` desugars to `Add.add 1 2`; there is no way to write a custom infix
operator or to pass a builtin operator as a bare value. `&&` and `||` are the
exception: they do not desugar to a call at all, since a call would evaluate
both operands eagerly and lose short-circuiting.

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
def unwrap = result: Ok I32 | IOError => match result {
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
its representation with binding, product, nominal, and wildcard patterns. A
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
returns, the match itself does not continue.

The standard prelude supplies a braced, clause-oriented `if` form implemented as a macro:

```staple
if {
    first_condition => first_branch,
    second_condition => second_branch,
    else => fallback,
}
```

It evaluates conditions from top to bottom and evaluates only the branch
belonging to the first true condition. Its optional final `else` clause supplies
the fallback; without one, the fallback is `()`. An `else` clause must be last,
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
the unit form. All reachable breaks must produce compatible values, using the
same result-joining rules as functions and match expressions. A loop with no
reachable break diverges and may be used wherever an expected type is available.

`break` and `continue` cannot cross a function boundary, and labeled loops are
not supported. Values owned by an iteration are dropped before a break,
continue, or implicit next iteration; a value moved out by `break` becomes the
loop result and is preserved.

The prelude supplies a `while` form implemented in `std.core.flow` using
`loop`, `break`, and a boolean match:

```staple
while condition body
```

The condition is evaluated before every iteration, and the loop returns `()`
when it becomes `False`.

## Iteration and ranges

The prelude's consuming iterator protocol returns the successor iterator state
with every step. `Iter` functionally determines `Item`:

```staple
pub mod IterStep {
    pub(repr) type Done Iter = Iter
    pub(repr) type Yield (Item, Iter) = (Item, Iter)
}

pub type alias IterStep (Iter, Item) =
    IterStep.Done Iter |
    IterStep.Yield (Item, Iter)

pub trait Iterator Iter Item where Iter ~> Item {
    next: Iter -> IterStep (Iter, Item)
}

pub trait IntoIterator Source Iter where Source ~> Iter, Iterator Iter {
    into_iterator: Source -> Iter
}
```

`IterStep.Done iterator` retains the terminal state, while
`IterStep.Yield (item, iterator)` contains an item and the state used for the
next call. `Done` and `Yield` remain inside the `IterStep` namespace. An
`IntoIterator` implementation consumes its source and selects one default
iterator type; wrapper types can provide alternative iteration modes.

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
an overflowing successor. `Range T` and `RangeInclusive T` expose their
representations so custom concrete iterator implementations can use them.

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
Bool
String
Ref T
Slice T
Buffer T
CString
CChar
CPointer CChar
Ordering
Option T
```

The numeric types, `Bool`, `String`, arithmetic and ordering traits, `ToString`, and their functions
from `std.core` are imported implicitly into every source module. These are
ordinary type names rather than keywords, so a local declaration or explicit
import can shadow a prelude name. Integer literals use an expected integer type
when one is available and otherwise default to `I32`. Mixed-type arithmetic is
not implicit.

`ToString` has the member `to_string: T -> String` and is implemented for all
integer and floating-point types, `Bool`, and `String`. Integers use decimal
notation, booleans produce `"True"` or `"False"`, and converting a `String`
returns it unchanged. Floating-point values use round-trip decimal notation.

`Display` and `Debug` format values into a mutable `Formatter`. Their common
protocol is `fmt: (T, mut Formatter) -> ()`: `Display` is intended for
human-facing text, while `Debug` is intended for developer-facing structure.
These formatting types and traits are declared in `std.core.fmt` and re-exported
by `std.core` into the prelude.
`Formatter.display value` and `Formatter.debug value` create a formatter,
dispatch the corresponding trait, and return the finished `String`.

Manual implementations append text with `Formatter.write` and may delegate
nested values directly to either formatting trait:

```staple
type Point = (x: I32, y: I32)

impl Debug Point {
    def fmt = (Point (x, y), formatter) => {
        Formatter.write (formatter, "Point ")
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

`String` is an immutable garbage-collected UTF-8 byte sequence. The standard
library declares it as a nominal type with the private representation
`Slice U8`. At runtime it contains a managed byte pointer and a byte length; it
has no capacity field or separate descriptor allocation. Copying a `String`
copies those two words and keeps the managed byte storage shared.
Two strings can be concatenated with `+`; this dispatches through the standard
`Add String` implementation and returns a newly allocated `String`.

`Ref T` is a garbage-collected reference to a value of type `T`. Its standard
declaration is `pub(repr) type Ref T where ?Sized T = T`, so its payload may be
sized or unsized while the reference value itself always has a known
representation.
Constructing `Ref value` copies or moves `value` into a managed allocation;
copying a `Ref` copies only its non-null handle. Product fields and indices can
be accessed and assigned directly through the handle when the binding holding
the handle is declared `mut`. Every alias, not only the binding that performed
the write, observes the resulting payload mutation, since they all share the
same managed allocation:

```staple
let mut point: Ref (x: I32, y: I32) = Ref (x: 10, y: 20)
let x = point.x
point.x = 30
let Ref (captured_x, captured_y) = point
```

The companion function `Ref.replace: <T> (Ref T, T) -> T` replaces a whole fixed
payload and returns the previous value:

```staple
let value = Ref 10
let previous = Ref.replace (value, 20)
```

`Ref` follows the ordinary literal representation rules when nested inside a
nominal type. For example, this declaration retains both constructor layers:

```staple
type RefPoint = Ref (x: I32, y: I32)
let point = RefPoint (Ref (x: 10, y: 20))
let RefPoint (Ref (x, y)) = point
```

The length of a homogeneous product can be erased into a `Slice`:

```staple
let fixed: Ref I32[3] = Ref (10, 20, 30)
let values: Slice I32 = fixed
let count: USize = Slice.length values
let second = values.1
let index: USize = 2
let third = values[index]
```

`Slice T` is a pointer-and-length view of an allocation whose concrete length
is still fixed. It is not a dynamic array and is not equal to any `Ref T[N]`;
a fixed reference is implicitly converted wherever a `Slice T` is expected
(as in the `let values: Slice I32 = fixed` binding above), and the companion
function `Slice.from_ref: <T> Ref T -> Slice T` performs the same conversion
explicitly — `Slice.from_ref fixed` — for use as a first-class function value
or wherever an explicit spelling is clearer. `Slice.from_ref` also accepts a
singleton `Ref T` (treated as a length-1 slice) and an empty `Ref ()`
(treated as a length-0 slice, requiring an expected `Slice` type to infer its
element type). Literal and variable indexing perform runtime bounds checks.
Erased products are unsized: they cannot be used by value, spread,
destructured, or passed through a foreign ABI.

Transparent aliases may name the underlying unsized array shape (`type alias
Elements T = T[]`), but that shape can only be completed into a usable type
through `Slice` — writing `Ref` directly around an unsized array, whether
spelled out (`Ref T[]`) or reached through such an alias, is rejected; use
`Slice T` instead. `Ref` remains generic over `?Sized T` so that functions
like `preserve: <T where ?Sized T> Ref T -> Ref T` work uniformly whether
`T` ends up sized or erased, but constructing that erasure in the first
place always goes through `Slice`.

`Buffer T` is low-level, fixed-capacity contiguous storage with an initialized
prefix. `Buffer.with_capacity` allocates space without constructing any `T`
values, so it does not require `Default T`. Buffer handles are Copy aliases of
the same managed allocation; `Buffer.length` and `Buffer.capacity` report its
current initialized length and fixed capacity.

`Buffer.push` appends while spare capacity remains, and `Buffer.pop` moves the
last initialized element into an `Option T`. Both require a mutable buffer
argument. Pushing to a full buffer traps; growth belongs in higher-level
containers such as `List`. `Buffer.get_ref` returns a managed reference to an
initialized element and traps for an out-of-bounds index. Pushing does not
relocate storage, but popping invalidates references to the removed slot.

`Buffer.freeze` seals the shared allocation against every subsequent push and
pop and returns a zero-copy `Slice T` over its initialized prefix. Repeated
freezes are harmless. The allocation drops exactly its initialized elements
when unreachable; an element reference or frozen slice keeps it alive.

`Buffer.transfer` moves every initialized element of one buffer onto the end
of another's initialized prefix, in order, and empties the source. Both
buffer arguments must be mutable. It traps if either buffer is frozen, if the
destination lacks enough spare capacity for the source's elements, or if the
source and destination are the same buffer. No allocation occurs: elements
are moved in place by one bulk copy, and any references into the moved
elements of the source are invalidated exactly as `Buffer.pop` invalidates a
reference to a popped element. This is the primitive higher-level growable
containers such as `List` use to move an initialized prefix into a larger
allocation.

`List T` is a growable array built on top of `Buffer T`. `List.new` and
`List.with_capacity` construct an empty list with no or a chosen initial
capacity, and `List.length`/`List.capacity` report its current element count
and storage size, exactly like the matching `Buffer` operations. `List.push`
appends an element and, unlike `Buffer.push`, never traps for lack of space:
once the current capacity is exhausted it allocates a larger `Buffer`
(doubling, with a minimum of four elements) and moves every existing element
across with `Buffer.transfer` before appending. `List.pop` removes and
returns the last element as an `Option T`, exactly like `Buffer.pop`.

`List.of` is a macro that builds a list from a fixed, comma-separated set of
values in one expression — `List.of (1, 2, 3)` — inferring the element type
from the values the same way `List.new` followed by a sequence of
`List.push` calls would. It is declared inside `List`'s own companion, so
`Type.macro(...)` call syntax resolves a macro through the type's companion
namespace the same way ordinary `Type.method(...)` calls already resolve a
function through it.

`List.get_ref` and, where `Copy T`, `List.get` return an element by
reference or by copy respectively, each wrapped in `Option (Ref T)`/
`Option T` so that an out-of-bounds index produces `None` instead of
trapping. `List.get_ref_unchecked` and `List.get_unchecked` are the
trapping counterparts. `list[index]` and `list[index] = value` delegate to
`Index`/`MutateIndex`, backed by `get_unchecked`/`get_ref_unchecked`, so
bracket indexing keeps the trapping behavior it has elsewhere in the
language; `for item in list` delegates to `IntoIterator`/`Iterator` and
yields owned copies. Both bracket indexing and iteration therefore require
`Copy T`, the same as `List.get`.

`List` handles are Copy aliases of the underlying `Buffer`, just as `Buffer`
handles alias their allocation, with one difference: growth replaces that
`Buffer` outright rather than mutating it in place, and only the specific
binding passed to the growing `push` call is updated to point at the new
allocation. A `List` handle copied out beforehand keeps referring to the
pre-growth allocation and does not observe later pushes.

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

Values are affine unless they are structurally `Copy`. Assignment, argument
passing, return, whole-value destructuring, and closure capture move a
non-`Copy` value; using it afterward is an error. Integers, `Bool`, `String`,
`Ref T`, C pointers, functions, and products/sums/distinct values made entirely
from `Copy` fields are copied implicitly. The public prelude trait `Copy` can be
used as a generic bound, but implementations are compiler-inferred and an
explicit `impl Copy` is rejected. A custom `Drop` implementation makes its
distinct target move-only regardless of its representation.

```staple
trait Copy T {}

trait Drop T {
    drop: T -> ()
}

type File = I32
impl Drop File {
    def drop = File descriptor => close descriptor
}
```

Owned locals and parameters are dropped in reverse lexical order on normal
scope exit, explicit `return`, and propagation. `drop value` consumes and
destroys a value early. A destructor may inspect copied fields and make scoped
C calls, but may not move out of its value; after the custom destructor returns,
the representation's owned fields are dropped automatically. Partial field
moves through `.name` or `.index` are rejected; destructure the whole owned
value instead. Move-only globals are not supported.

When an unreachable managed payload or closure environment owns a `Drop`
value, collection finalizes it exactly once before reclaiming its storage.
Finalization is two-phase: all unreachable finalizers run while their objects
still exist, resurrection does not retain those objects, and allocation is
allowed while nested collection is suppressed.

`CChar`, `CString`, and the generic `CPointer` constructor are public opaque
types in `std.cinterop`. Source code must import them explicitly, for example
with `use std.cinterop.*`. `CPointer T` is the language's C pointer type; the
pointee is a compile-time argument with no runtime field. There are no `*T` or
`*const T` type forms and Staple does not distinguish mutable and const C
pointers.

`CString` is an owned, move-only pointer to NUL-terminated bytes and drops with
`free`. It is compatible with `CPointer CChar`, but an arbitrary
`CPointer CChar` is not assumed to be NUL terminated. Passing a `CString` to a
C function creates a call-scoped view rather than transferring ownership, and
C declarations may not return `CString`. `CString.to_string` consumes its
argument, validates and copies UTF-8 into a `String`, then frees it;
`CString.from_string` allocates an owned copy, appends a terminator, and traps on
an interior NUL byte. Invalid UTF-8 also traps.

An underscore asks the compiler to infer a type:

```staple
_
_ -> I32
```

The currently supported type syntax also includes type application, product
types, variadic markers in external function signatures, and function types:

```staple
CPointer CChar
(I32, String)
(CPointer CChar, ...) -> I32
```

`...` denotes the variadic portion of an external function parameter product. Its
meaning outside foreign declarations is currently unspecified.

The fixed-width standard-library integers are `I8`, `I16`, `I32`, `I64`, `U8`,
`U16`, `U32`, and `U64`. `ISize` and `USize` use the target pointer width.
Arithmetic is homogeneous: both operands and the result have the same type.
Signed integer division uses signed semantics, while unsigned integer division
uses unsigned semantics. All integer types implement `Eq` for `==` and `!=`, and
`PartialOrd` and `Ord` for ordering. `PartialOrd.partial_cmp` returns
`Option Ordering`; its default `lt`, `le`, `gt`, and `ge` methods back `<`, `<=`,
`>`, and `>=`. `Ord.cmp` returns `Less`, `Equal`, or `Greater` and requires both
`Eq` and `PartialOrd`. Integer ordering uses signed semantics for signed types
and unsigned semantics for unsigned types.

`F32` and `F64` use IEEE-754 single and double precision. Both implement the
arithmetic traits, `Eq`, and `PartialOrd`, but not `Ord`. A comparison involving
NaN makes `partial_cmp` return `None`; ordered boolean comparisons are false,
`==` is false, and `!=` is true. Float division by zero follows IEEE behavior.
Staple does not provide implicit numeric conversions.

### Type declarations

staple distinguishes transparent aliases, distinct type definitions, and
opaque type declarations.

An opaque type has no source-level representation and uses the explicit
`opaque` marker:

```staple
pub type Handle = opaque
```

An opaque type may also accept compile-time arguments when its body is the
`opaque` marker:

```staple
pub type CPointer Pointee = opaque
```

Arguments are part of nominal identity, so two applications with different
arguments are distinct. A by-value use requires a representation supplied by
the compiler or another future implementation mechanism. `std.core.I32` and
`std.cinterop.CPointer` are opaque declarations whose representations are
provided by the compiler.

#### Singleton types

A bodyless type declaration introduces both a nominal type and its unique
value under the same name:

```staple
pub type Ready

let state: Ready = Ready
```

Singleton values have the zero-sized representation `()` but remain nominally
distinct from `()` and from every other singleton type. A public singleton
exports its value together with its type; `pub(repr)` is neither needed nor
accepted. A private singleton keeps both names private.

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
let Ready()? = operation()
```

The standard-library `Bool` type is defined entirely in these terms:

```staple
pub type True
pub type False
pub type alias Bool = True | False
```

`True` and `False` are the two values of `Bool`; `Bool` itself adds no nominal
wrapper or compiler-specific type identity.

#### `type`

`type` creates a distinct nominal type
with the same runtime representation as its underlying type:

```staple
type UserId = I32
type OrderId = I32
```

`UserId`, `OrderId`, and `I32` are distinct types and are not implicitly
interchangeable, even though they share a representation. This provides type
safety without adding a runtime wrapper. A represented distinct type also
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
type User = (name: String, age: I32)
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

`pub(repr)` exposes the representation and generated constructor as part of the
module interface:

```staple
pub(repr) type Box T = (value: T)
```

Importers may construct `Box` values and use `Box pattern` to destructure them,
including through namespace, selected, renamed, or glob imports. Every named
type directly referenced by a public representation must also be public.
`pub(repr)` is rejected on aliases and opaque declarations.

Represented types, aliases, and explicitly opaque declarations may introduce
compile-time parameters directly after the type name, juxtaposed rather than
bracketed as with generic functions:

```staple
type Box T = (value: T)
type HashMap (K, V) = (key: K, value: V)
type alias Pair (A, B) = (A, B)
```

Type application uses left-associative juxtaposition. A product binder consumes
one product type argument, while curried binders consume successive arguments:

```staple
HashMap (String, I32)

// Given: type CurriedMap K V = (key: K, value: V)
CurriedMap String I32
```

A type annotation must apply every compile-time parameter, unless the omitted
trailing parameters all have defaults (see [Default type
parameters](#default-type-parameters)). Applying a non-parameterized type,
supplying the wrong product shape, or leaving a type partially applied is an
error.

Product types and values may have a trailing comma.

#### `type alias`

`type alias` gives another name to an
existing type without creating a new type. The alias and its underlying type
are interchangeable:

```staple
type alias Person = (
    name: String,
    age: I32,
)
```

Here, `person` is exactly the named product type on the right-hand side. The alias
does not create a separate nominal identity.

### Sum types

A sum type lists the variants which a value may contain:

Staple’s sum types are open: their alternatives are ordinary types rather than cases owned by the sum.

```staple
Ok Tree | IOError | ParseError
```

Variants do not share a declaration. Modules may define represented nominal
types independently and combine them wherever a type is accepted. `Ok` is a
public represented type from `std.core`:

```staple
pub(repr) type Ok T = T
```

Every alternative must be a sized value type. Primitive, product,
function, opaque, reference, and fully applied nominal types may all be
alternatives. Unsized and partially applied types cannot be alternatives. A
transparent alias may name a sum or alternative, but does not introduce another
variant identity.

Sized compile-time parameters may be alternatives in generic code. Their sum
representation is specialized and canonicalized for each concrete use, so
`A | B` collapses to one alternative when `A` and `B` specialize to the same
type.

Sums are unordered, flattened, and duplicate-free. Consequently `A | B` and
`B | A` are the same type, nested sums are flattened, and `A | A` is `A`.
Variant identity is the complete type, so different applications of one
constructor, such as `Ok I32 | Ok String`, are distinct alternatives. A nominal
pattern must select exactly one alternative; use a typed binding to disambiguate
repeated nominal constructors.

Type application binds more tightly than `|`, and `|` binds more tightly than
the function arrow. Thus `String -> Ok Tree | IOError` is a function returning
the sum.

Values of any alternative type are injected implicitly when a sum is expected.
A smaller sum is likewise widened implicitly to a sum containing all of its
alternatives. Sums cannot be narrowed implicitly. Standalone values retain
their ordinary representation; a value acquires a runtime tag only while stored
in a sum. An exact alternative is preferred; otherwise the value must have
exactly one alternative to which it can be coerced. A typed match binding
selects and extracts an arbitrary variant:

```staple
def describe = value: I32 | String => match value {
    number: I32 => number,
    text: String => 0,
}
```

An untyped binding or `_` matches the whole sum.

#### Propagating bindings

A `?` after a nominal destructuring pattern selects its success alternative
and returns every other alternative from the enclosing function:

```staple
def load = (path: String) => {
    let Ok(file)? = read_file(path)
    let Ok(tree)? = parse_tree_from_file(file)
    Ok(tree)
}
```

The right-hand expression must have a sum type containing exactly one
alternative with the pattern's nominal constructor. It is evaluated once. On
the selected tag, the payload is destructured and execution continues. Any
other tag returns immediately and is widened into the enclosing result type.
The selected representation must be visible under the ordinary `pub(repr)`
rules.

When the function result is omitted, the compiler joins its trailing value,
reachable explicit returns, and every propagated alternative. The example
therefore infers `Ok Tree | IOError | ParseError`. With an explicit binding
annotation or `satisfies` constraint on the body, every normal and propagated
result must be contained in that type.

Sum types use Staple's internal tagged inline representation and may not appear
anywhere inside an `extern` binding type.

The prelude supplies a `typegroup` macro that conveniently generates sum types:

```staple
pub(repr) typegroup Pattern {
    Literal String,
    Wildcard,
}
```

Generic parameters are written between the group name and its body, using the
same juxtaposed syntax as generic type declarations. Product parameter patterns
and multiple parameter atoms may be mixed freely:

```staple
pub(repr) typegroup Result T E {
    Ok T,
    Err E,
}

pub(repr) typegroup Mixed A (B, C) D {
    Empty,
    Value (A, B, C, D),
}
```

It generates a same-named inline module containing the nominal variants and a
same-named parent-module alias whose alternatives are the qualified variants.
Private groups use a private module and alias while keeping child variants
public-representation inside that private boundary. `pub` groups expose opaque
variants, and `pub(repr)` groups expose their representations.

## Foreign declarations

An `extern` block declares values supplied by a foreign ABI. The ABI name is a
string following `extern`:

```staple
use std.cinterop.*

extern "c" {
    let printf: (CPointer CChar, ...) -> I32
}
```

The example declares an immutable foreign value named `printf`. Its type is a
function from one product parameter—containing a C string pointer followed by
variadic values—to an `I32` result.

External symbols are resolved during native linking. Calling-convention
details, supported ABI names, foreign symbol aliases, foreign library selection,
and foreign type layouts remain unspecified.
