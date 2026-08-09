# The staple language

**staple** is an expression-oriented programming language. Its compiler is
called **stapler**.

This document describes the language as it currently exists. Details listed as
unspecified are not yet part of the language design.

## Language goals

staple has two major, long-term goals:

1. Effects and signals are core language features. Reactive programming should
   feel native to staple in the way that signals and effects feel native when
   using Solid.js, rather than being an incidental library convention.
2. Metaprogramming is powerful and approachable. staple should provide the
   directness and expressive power associated with Lisp metaprogramming while
   retaining staple's own syntax.

These are defining goals of the language, even though their precise syntax and
semantics have not yet been designed. Other language features should be
evaluated partly by how well they compose with reactivity and metaprogramming.

### Effects and signals

Signals represent values that can change over time. Reading a signal from a
reactive computation establishes a dependency on that signal. When a signal's
value changes, computations which depend on it can be updated automatically.

Effects are computations which react to their signal dependencies. As in
Solid.js, dependency tracking should be fine-grained: changing a signal should
update the computations which actually depend on it, without requiring a
virtual DOM or broad re-execution of unrelated code.

Signals and effects are intended to be language concepts rather than ordinary
types and functions supplied solely by a framework. This allows staple's type
system, compiler, runtime, tooling, and metaprogramming facilities to understand
reactive relationships directly.

The following details remain unspecified:

- the syntax for creating, reading, and writing signals;
- the syntax for declaring effects and derived computations;
- whether signal reads are implicit or explicit;
- effect scheduling and batching;
- ownership, disposal, and lifetime rules;
- error handling and cycle detection; and
- how reactive behavior appears in the type system.

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
resolution and type checking. `Syntax` is the permissive sum of `Expr`,
`Ident String`, `Type`, `Pattern`, and `Item`. A macro body is a curried
compile-time Staple function. Every parameter consumes one atomic syntax unit;
an omitted parameter type means `Syntax`:

```staple
macro choose = condition => then => else => quote {
    match $condition {
        True() => $then,
        False() => $else,
    }
}
```

Typed parameters constrain the grammar accepted at an invocation. `Ident` is a
generic syntax type whose argument constrains its spelling. `Ident String`
accepts any identifier and `Ident "else"` accepts exactly the identifier
`else`. The compiler-owned, methodless `StringType` trait is satisfied by
`String` and every string literal type, and constrains the argument of `Ident`.
It cannot be implemented explicitly. A literal identifier parameter may be
left unbound or given a binding when its syntax is needed:

```staple
macro conditional =
    condition: Expr =>
    then_branch: Expr =>
    Ident "else" =>
    else_branch: Expr =>
    quote {
        match $condition {
            True() => $then_branch,
            False() => $else_branch,
        }
    }
```

The braces in `quote { expression }` delimit one expression and are not part of
the result. `$name` splices an expression supplied by the caller. `$names...`
is reserved for future syntax sequences and is currently rejected.

Macros are hygienic. Names and bindings written in a quotation retain the
definition module's environment and receive a fresh expansion identity, while
spliced expressions retain their caller environment. A macro consumes the
number of arguments described by its curried `Syntax` type; further call
arguments apply to the expanded expression.

Compile-time evaluation supports pure functions, bindings, products, matches,
literals, recursion, and pure integer operations. It rejects external or
runtime-only effects. Expansion is limited to 128 nested macros and each
top-level invocation is limited to 1,000,000 evaluation steps. `Syntax` is
opaque and compile-time-only. This release supports expression results and
scalar splices; syntax inspection, repeated splices, and item, type, or pattern
generation remain future work.

Compiler-provided macros use typed bodyless contracts. `std.core` declares
`pub macro quote: Syntax -> Syntax`, and `std.cinterop` declares
`pub macro c_string: Expr -> Expr`.

## Source files

staple source files use the `.sta` extension.

Whitespace and comments are preserved by the parser, so parsing and reproducing
a source file does not change its text. Newlines also separate adjacent
statements where they would otherwise form a single expression. A semicolon may
be written after any statement or top-level item as an explicit separator,
including after the last item in a sequence.

Line comments begin with `//` and continue to the end of the line.

### Compiler output

`stapler` builds a native executable by default. Without `-o`, the output is
written next to the source file without the `.sta` extension:

```text
stapler examples/hello_world.sta
# writes examples/hello_world
```

It can explicitly write LLVM IR, a native object file, or a linked executable:

```text
stapler --emit llvm examples/hello_world.sta       # LLVM IR on stdout
stapler --emit llvm -o hello_world.ll examples/hello_world.sta
stapler --emit object -o hello_world.o examples/hello_world.sta
stapler --emit exe -o hello_world examples/hello_world.sta
```

The native target is used unless `--target <triple>` is supplied. Executables
are linked through `$CC`, or `cc` when `$CC` is unset. `--linker` selects a
different linker driver. `-L <path>` and `-l <name>` add library search paths
and libraries for executable output.

Stapler loads the standard library at compile time. `--stdlib <path>` selects
its root explicitly, `STAPLE_STDLIB` provides the same path through the
environment, and an installed compiler otherwise looks in
`../lib/staple/stdlib` relative to its executable. The standard-library root
contains `std/core.sta`, feature modules under `std/core/`, and
`std/cinterop.sta`. Core features include numbers, booleans, strings,
references, results, syntax, equality, copying, dropping, and defaults.
`std.core` re-exports their public items and remains the stable prelude
interface.

### Top-level statements

A source file may contain expression statements alongside bindings, type
declarations, and foreign declarations:

```staple
use std.cinterop *

extern "c" {
    let printf: (CPointer CChar, ...) -> I32
}

printf (c_string "hello, world!\n")
```

There is no distinguished source-level entry-point function. Stapler generates
one native `main` function for the entry source file and the modules reachable
from it.

### Modules and `use`

Every `.sta` file is a module. A dotted module path is resolved from the entry
file's directory by replacing dots with path separators and adding `.sta`:

```staple
use tools.format
// loads tools/format.sta
```

When the entry program is read from standard input, module paths are resolved
from stapler's current working directory. Only modules reachable from the entry
module are compiled. A source file is loaded once even when several modules use
it, and mutually recursive module dependencies are allowed.

A module can be brought into scope as a namespace. The namespace name is the
last component of its path:

```staple
use path.to.another_module
another_module.func ()
let value: another_module.MyType
```

Public items can instead be imported directly:

```staple
use path.to.another_module *
use path.to.another_module (func, MyType)
use path.to.another_module func as my_func
```

The wildcard form imports every public named item. The parenthesized form
imports only the listed items. The `as` form imports one item under a different
local name. Imports are hoisted and apply throughout their module. Prefixing an
item import with `pub` re-exports it from the importing module:

```staple
pub use path.to.another_module (func, MyType)
pub use path.to.another_module func as public_func
pub use path.to.another_module *
```

Re-exports may be chained through multiple modules. Importing two items under
the same name, or colliding with a local declaration, is an error.

Top-level declarations are private by default. `pub` exports a binding or type:

```staple
pub def format = (value: I32) => value
pub type alias Number = I32
```

`pub extern` exports every binding declared by that external block.

Every reachable module's top-level statements execute exactly once. Dependencies
are initialized before modules which use them. Mutually recursive groups are
initialized in canonical file-path order, and statements within one module keep
source order. Module globals begin in the `Declared` state, enter `Initializing`
while their initializer is evaluated, and become `Initialized(value)` only after
that value has been stored. Reading a global before it is `Initialized` is an
initialization error; globals never expose a default representation.

## Bindings

staple has two binding keywords: `let` and `def`.

### `let`

`let` declares or defines a value. Bindings are immutable unless their binding
pattern is marked `mut`:

```staple
let answer = 42
let mut counter = 0
counter = counter + 1
```

A type may be written after the binding name:

```staple
let answer: I32 = 42
```

`mut` may mark individual names in any binding pattern, including function
parameters and match arms. A mutable `let` must have an initializer. Assignment
is a statement and its right-hand side must have the type of the destination:

```staple
let (mut left, right) = (1, 2)
left = right

def increment = (mut value: I32) => {
    value = value + 1
    value
}
```

Fields and indices of a by-value product are writable when the product is rooted
in a mutable binding. Mutable locals captured by functions are shared cells, so
the defining scope and all closures observe subsequent assignments. Public
mutable module bindings remain assignable only from their declaring module.

An external declaration may omit its value because its implementation is
provided outside staple:

```staple
use std.cinterop *

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

### Binding type annotations

The optional type annotation on a binding describes the complete type of its
value:

```staple
def get_number: _ -> I32 = () => {
    42
}
```

`_` is an inferred type placeholder. In this example, stapler infers the
function's parameter type from `()` while requiring its result to be `I32`.
Inferred placeholders may appear wherever a type is expected.

## Values and expressions

staple is expression-oriented. Function values, function applications, products,
and blocks are all expressions.

The syntax currently recognized for literal values includes strings and
integers:

```staple
"hello"
42
```

String literals use double quotes and produce owned UTF-8 `String` values. A
backslash protects the following quote from ending the string. Supported
escapes are `\\n`, `\\r`, `\\t`, `\\0`, `\\\\`, and `\\"`.

The primitive `c_string` macro from `std.cinterop` accepts only a string literal
and produces an owned `CString` backed by allocated NUL-terminated storage:

```staple
use std.cinterop (c_string, CString)

def message = () => c_string "hello"
```

Macros are module items and support the same namespace, glob, selected, and
renamed import forms as values and types. Public macros retain their definition
environment, including private helpers used by generated syntax.

Operators are ordinary curried function values. The standard prelude provides
`+`, `-`, `*`, and `/` through the `Add`, `Subtract`, `Multiply`, and `Divide`
traits. Each operator is one bounded generic function; arithmetic is not
implemented with function overloading. The standard integer implementations
are backed by private compiler intrinsics. A symbolic function may
declare its fixity as part of its binding:

```staple
def infixl 6 <>: I32 -> I32 -> I32 = left => right => left
```

`infixl`, `infixr`, and `infix` declare left, right, and no associativity. The
following integer is a precedence from `0` through `9`. A function without an
explicit fixity defaults to `infixl 9`. Fixity modifiers are allowed only on
module-level `let` and `def` bindings and external declarations.

Symbolic operators use maximal sequences of
`!#$%&*+./<=>?@\^|-~:`. Staple's arrows, comments, and structural punctuation
remain reserved in their grammatical contexts.

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

Named and unnamed elements can be mixed in the same product. Whether names must be
unique and whether a value's element names must exactly match its annotated
type remain unspecified.

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
change the order or size of a product. Accessing an absent name or an index outside
the product's fixed bounds is an error. Whether that error is always detected at
compile time remains unspecified.

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

Homogeneous products support variable indexing with a `USize` expression:

```staple
let index: USize = 1
let value = values[index]
```

The index is checked against the fixed product length. A statically known bad
index is rejected; a dynamically out-of-bounds index traps.

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

`=>` introduces the body of the abstraction. Stapler infers the result type
from the body unless a surrounding function type or a `satisfies` expression
constrains it.

A binding pattern normally has a name and a type:

```staple
s: String => s
```

The type may be omitted when a surrounding function type supplies it:

```staple
def identity: I32 -> I32 = value => value
```

An omitted parameter type without such a context is an error. Stapler does not
infer parameter types from operations in the function body.

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

A function-valued `def` may introduce compile-time type parameters before its
ordinary function type:

```staple
def identity: T => T -> T = value => value
def first: (A, B) => (A, B) -> A = (a, b) => a
def choose: T => (Bool, T, T) -> T = (condition, a, b) => {
    // ...
}
```

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
and recursive calls must retain the current specialization. Generic `let` and
`extern` declarations are not supported.

Every compile-time type parameter has an implicit `Sized` bound. It may
therefore appear in by-value parameters, results, products, and represented
types. A declaration that only stores or forwards a type behind a constructor
which accepts unsized arguments can relax that default after introducing the
parameter:

```staple
def preserve_ref: T => ?Sized T => Ref T -> Ref T = value => value
```

The `?Sized T =>` clause must name an already introduced parameter and may
appear only once for that parameter. It is a relaxation, not a trait bound:
`Sized T =>` remains the ordinary bounded-generic spelling. A relaxed parameter
cannot itself be passed or returned by value. `Sized` is compiler-derived, so
explicit `impl Sized` declarations are rejected.

### Traits and bounded generic functions

A trait declares a set of functions for one type parameter:

```staple
trait ToString = T => {
    to_string: T -> String
}
```

Trait members must have function types, must mention the trait parameter, and
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
```

Implementation targets may be built-in, nominal, product, pointer, or function
types. Implementations have no visibility modifier: every implementation in the
loaded program is available globally. Defining the same trait/type pair twice,
including through two aliases of the same type, is an error.

A generic `def` adds one or more trait bounds between its compile-time parameter
binder and its ordinary function type:

```staple
def print: T => ToString T => T -> () = value => {
    print_string (to_string value)
}
```

Bounds are explicit and must be propagated by other generic functions. A
concrete use must have a matching implementation. Trait members are first-class
function values and may be called unqualified when unambiguous or qualified as
`ToString.to_string`; a namespace-qualified trait may be used as
`strings.ToString.to_string`.

Traits use static dispatch. Bounds and implementations add no runtime values or
function parameters. During monomorphization, Stapler substitutes the concrete
type arguments and emits a direct reference to the selected implementation
member. Trait objects, runtime dictionaries, supertraits, associated items,
default methods, generic implementations, and independently generic trait
members are not currently supported.

## Function application

Function application is written by placing the argument expression after the
function expression. No dedicated call punctuation is required.

```staple
println "Hello, world!"
```

Because each function accepts one value, passing several logical arguments
means passing a product:

```staple
printf (c_string "%s\n", string_to_c_string s)
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

An infix call supplies its left and right operands through two curried calls:

```staple
def infixl 6 <>: I32 -> I32 -> I32 = left => right => left
def infixr 5 choose: I32 -> I32 -> I32 = left => right => right

1 <> 2
1 `choose` 2
(<>) 1 2
(<>)
```

The last expression passes `<>` as a value rather than calling it. Function
application and access bind more tightly than infix calls. Operators at the
same precedence must use a compatible associativity; chaining an `infix`
operator is an error.

Public functions carry their fixity through namespace, glob, selected, and
renamed imports. Symbolic selected imports use an extra pair of parentheses,
for example `use math ((<>))`; qualified values and calls may be written
`(math.<>)` and `1 math.<> 2`.

## Block expressions

Braces construct a block expression:

```staple
{
    let x = 1
    x
}
```

A block may contain bindings, returns, and expressions. Its final statement
determines the block's value: a final expression supplies its value, while an
empty block or a block ending in a non-expression supplies `()`. A semicolon
after the final expression does not discard that value.

## Match expressions

`match` exhaustively selects an alternative of an open sum and produces a
value:

```staple
def unwrap = result: Ok I32 | IOError => match result {
    Ok value => value,
    IOError _ => 0,
}
```

The subject is evaluated exactly once and must have a sum or product type.
Every arm has the form `<pattern> => <expression>`. Arms are separated by
commas, and a trailing comma is permitted. Each arm has its own scope, so names
introduced by its pattern are visible only in that arm's expression.

A nominal pattern selects one sum alternative and may recursively destructure
its representation with binding, product, nominal, and wildcard patterns. The
existing empty representation syntax selects singleton alternatives, including
the standard-library boolean values:

```staple
match value {
    True() => "yes",
    False() => "no",
}
```

A binding pattern at the root is a catch-all and binds the complete subject value.
`_` is a wildcard pattern which matches without binding a name; wildcards may
also be used in function parameters and destructuring bindings.

Product subjects are matched structurally. Their element patterns may select
sum alternatives, and coverage is checked across every possible combination:

```staple
def same = (left: Bool, right: Bool) => match (left, right) {
    (True(), True()) => True,
    (False(), False()) => True,
    _ => False,
}
```

A match must cover every possible sum alternative and product combination or
include a catch-all. Duplicate or otherwise redundant arms are errors. Literal
patterns, alternative patterns, and match guards are not currently supported.

An expected type is applied to every arm. Without one, equal arm types remain
that type; differing represented nominal results are joined into an open sum by
the same rules used for inferred function results. Arms which return from the
enclosing function do not contribute to the match value type. If every arm
returns, the match itself does not continue.

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
Bool
String
Ref T
CString
CChar
CPointer CChar
```

The integer types, `Bool`, `String`, arithmetic and comparison traits, and their functions
from `std.core` are imported implicitly into every source module. These are
ordinary type names rather than keywords, so a local declaration or explicit
import can shadow a prelude name. Integer literals use an expected integer type
when one is available and otherwise default to `I32`. Mixed-type arithmetic is
not implicit.

`String` is an immutable garbage-collected handle. Its managed descriptor
contains a UTF-8 byte pointer, byte length, and capacity; the byte storage is
managed as well. Copying a `String` copies the handle.

`Ref T` is a garbage-collected reference to a value of type `T`. Its standard
declaration is `pub(repr) type Ref = T => ?Sized T => T`, so its payload may be
sized or unsized while the reference value itself always has a known
representation.
Constructing `Ref value` copies or moves `value` into a managed allocation;
copying a `Ref` copies only its non-null handle. Product fields and indices can
be accessed and assigned directly through the handle. Every alias observes the
same payload mutation, even when the binding containing the `Ref` is immutable:

```staple
let point: Ref (x: I32, y: I32) = Ref (x: 10, y: 20)
let x = point.x
point.x = 30
let Ref (captured_x, captured_y) = point
```

The prelude function `replace: T => (Ref T, T) -> T` replaces a whole fixed
payload and returns the previous value:

```staple
let value = Ref 10
let previous = replace (value, 20)
```

`replace` is not available for erased product payloads because an erased product
cannot be passed by value. Individual elements of `Ref T[]` remain writable by
index.

`Ref` follows the ordinary literal representation rules when nested inside a
nominal type. For example, this declaration retains both constructor layers:

```staple
type RefPoint = Ref (x: I32, y: I32)
let point = RefPoint (Ref (x: 10, y: 20))
let RefPoint (Ref (x, y)) = point
```

The length of a homogeneous product can be erased behind `Ref`:

```staple
let fixed: Ref I32[3] = Ref (10, 20, 30)
let values: Ref I32[] = fixed
let count: USize = length values
let second = values.1
let index: USize = 2
let third = values[index]
```

`Ref T[]` is a pointer-and-length view of an allocation whose concrete length
is still fixed. It is not a dynamic array and is not equal to any `Ref T[N]`;
fixed references are implicitly converted when an erased reference is
expected. Literal and variable indexing perform runtime bounds checks. Erased
products are unsized: they cannot be used by value, spread, destructured, or
passed through a foreign ABI. Transparent aliases may name unsized types, and
those aliases may be used behind a constructor such as `Ref` whose parameter
is declared `?Sized`.

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
trait Copy = T => {}

trait Drop = T => {
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
with `use std.cinterop *`. `CPointer T` is the language's C pointer type; the
pointee is a compile-time argument with no runtime field. There are no `*T` or
`*const T` type forms and Staple does not distinguish mutable and const C
pointers.

`CString` is an owned, move-only pointer to NUL-terminated bytes and drops with
`free`. It is compatible with `CPointer CChar`, but an arbitrary
`CPointer CChar` is not assumed to be NUL terminated. Passing a `CString` to a
C function creates a call-scoped view rather than transferring ownership, and
C declarations may not return `CString`. `string_from_c_string` consumes its
argument, validates and copies UTF-8 into a `String`, then frees it;
`string_to_c_string` allocates an owned copy, appends a terminator, and traps on
an interior NUL byte. Invalid UTF-8 also traps.

An underscore asks stapler to infer a type:

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
`Compare` for `<`, `<=`, `>`, and `>=`. These operators compare homogeneous
operands and return `Bool`. Ordering comparisons use signed semantics for signed
types and unsigned semantics for unsigned types. `Compare` does not currently
require `Eq`; trait prerequisites will provide that relationship in the future.
Staple does not currently provide implicit numeric conversions or floating-point
types.

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
pub type CPointer = Pointee => opaque
```

Arguments are part of nominal identity, so two applications with different
arguments are distinct. A by-value use requires a representation supplied by
the compiler or another future implementation mechanism. `std.core.I32` and
`std.cinterop.CPointer` are opaque declarations whose representations are
provided by Stapler.

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

The unique value is not a function and cannot be called. In a pattern, the
nominal form uses an empty representation pattern:

```staple
let Ready() = state
```

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

`pub(repr)` exposes the representation and generated constructor as part of the
module interface:

```staple
pub(repr) type Box = T => (value: T)
```

Importers may construct `Box` values and use `Box pattern` to destructure them,
including through namespace, selected, renamed, or glob imports. Every named
type directly referenced by a public representation must also be public.
`pub(repr)` is rejected on aliases and opaque declarations.

Represented types, aliases, and explicitly opaque declarations may introduce
compile-time parameters using the same binder syntax as generic functions:

```staple
type Box = T => (value: T)
type HashMap = (K, V) => (key: K, value: V)
type alias Pair = (A, B) => (A, B)
```

Type application uses left-associative juxtaposition. A product binder consumes
one product type argument, while curried binders consume successive arguments:

```staple
HashMap (String, I32)

// Given: type CurriedMap = K => V => (key: K, value: V)
CurriedMap String I32
```

A type annotation must apply every compile-time parameter. Applying a
non-parameterized type, supplying the wrong product shape, or leaving a type
partially applied is an error.

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

### Open sum types

A sum type lists the nominal variants which a value may contain:

```staple
Ok Tree | IOError | ParseError
```

Variants do not share a declaration. Modules may define represented nominal
types independently and combine them wherever a type is accepted. `Ok` is a
public represented type from `std.core`:

```staple
pub(repr) type Ok = T => T
```

Every alternative must currently be a fully applied represented nominal type.
Primitive, product, function, opaque, and partially applied types cannot be
alternatives. A transparent alias may name a sum or nominal alternative, but
does not introduce another variant identity.

Sums are unordered, flattened, and duplicate-free. Consequently `A | B` and
`B | A` are the same type, nested sums are flattened, and `A | A` is `A`.
Different applications of one constructor, such as `Ok I32 | Ok String`, may
not occur in one sum because a nominal pattern would not distinguish them.

Type application binds more tightly than `|`, and `|` binds more tightly than
the function arrow. Thus `String -> Ok Tree | IOError` is a function returning
the sum.

A nominal value is injected implicitly when a sum is expected. A smaller sum
is likewise widened implicitly to a sum containing all of its alternatives.
Sums cannot be narrowed implicitly. Standalone nominal values retain their
zero-cost representation; a value acquires a runtime tag only while stored in
a sum.

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

When the function result is omitted, Stapler joins its trailing value, reachable
explicit returns, and every propagated alternative. The example therefore
infers `Ok Tree | IOError | ParseError`. With an explicit binding annotation or
`satisfies` constraint on the body, every normal and propagated result must be
contained in that type.

Sum types use Staple's internal tagged inline representation and may not appear
anywhere inside an `extern` binding type.

## Foreign declarations

An `extern` block declares values supplied by a foreign ABI. The ABI name is a
string following `extern`:

```staple
use std.cinterop *

extern "c" {
    let printf: (CPointer CChar, ...) -> I32
}
```

The example declares an immutable foreign value named `printf`. Its type is a
function from one product parameter—containing a C string pointer followed by
variadic values—to an `I32` result.

External symbols are resolved by the native linker when producing an
executable. Libraries outside the platform defaults can be supplied to
`stapler --emit exe` with `-L` and `-l`. Calling-convention details, supported
ABI names, foreign symbol aliases, and foreign type layouts remain unspecified.
