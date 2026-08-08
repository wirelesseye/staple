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

The eventual design should make common transformations concise, support
composition between independently written metaprograms, and produce useful
source locations and diagnostics. It should also define when generated code is
expanded and how names are resolved, so metaprogramming remains predictable in
larger programs.

The following details remain unspecified:

- the syntax used to quote and construct code;
- the representation exposed to metaprograms;
- compile-time evaluation and staging rules;
- macro hygiene and explicit name capture;
- the boundary between macros and ordinary functions;
- access to types and reactive dependency information during expansion; and
- restrictions placed on compile-time effects.

The only macro currently implemented is the compiler-provided `c_string` macro.
The parser and module system recognize opaque declarations such as
`pub macro c_string`, but user-defined macro bodies remain future work.

## Source files

staple source files use the `.sta` extension.

Whitespace and comments are preserved by the parser, so parsing and reproducing
a source file does not change its text. Newlines also separate adjacent
statements where they would otherwise form a single expression.

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
contains `std/core.sta` and `std/cinterop.sta`.

### Top-level statements

A source file may contain expression statements alongside bindings, type
declarations, and foreign declarations:

```staple
use std.cinterop.*

extern "c" {
    let printf: (*const CChar, ...) -> I32
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
use path.to.another_module.*
use path.to.another_module.(func, MyType)
use path.to.another_module.func as my_func
```

The wildcard form imports every public named item. The parenthesized form
imports only the listed items. The `as` form imports one item under a different
local name. Imports are hoisted, apply throughout their module, and are not
re-exported. Importing two items under the same name, or colliding with a local
declaration, is an error.

Top-level declarations are private by default. `pub` exports a binding or type:

```staple
pub def format = (value: I32) => value
pub type alias Number = I32
```

`pub extern` exports every binding declared by that external block. `pub use`
is not supported.

Every reachable module's top-level statements execute exactly once. Dependencies
are initialized before modules which use them. Mutually recursive groups are
initialized in canonical file-path order, and statements within one module keep
source order. Reading a top-level value from a module that has not yet run its
initializer observes that value's default representation.

## Bindings

staple has two binding keywords: `let` and `def`.

### `let`

`let` declares or defines an immutable value.

```staple
let answer = 42
```

A type may be written after the binding name:

```staple
let answer: I32 = 42
```

An external declaration may omit its value because its implementation is
provided outside staple:

```staple
use std.cinterop.*

extern "c" {
    let printf: (*const CChar, ...) -> I32
}
```

A function declaration may omit its value when its complete function type is
given. Whether other ordinary, non-external `let` declarations may omit their
value is currently unspecified.

### `def`

`def` defines a hoisted value. It is comparable to JavaScript's `var`: the
binding is available throughout its containing scope, rather than becoming
available only after the definition.

```staple
def greet = () => {
    printf (c_string "hello, world!\n")
}
```

`def` is not a function-declaration keyword. It can bind any value. In the
example above, the value assigned to `greet` happens to be a function value.

The precise behavior of reading a hoisted binding before its initializer is
evaluated is currently unspecified.

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
and produces a `CString` backed by static NUL-terminated storage:

```staple
use std.cinterop.(c_string, CString)

let message: CString = c_string "hello"
```

Macros are module items and support the same namespace, glob, selected, and
renamed import forms as values and types. User-defined macros are not yet
supported.

Operators are ordinary curried function values. The standard prelude currently
provides `+`, `-`, `*`, and `/` for `I32`; their public definitions are ordinary
functions backed by private compiler intrinsics. A symbolic function may
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

## Functions

A function value has one of the following forms:

```text
<parameter> => <body expression>
<parameter> -> <result type> => <body expression>
```

There is no distinction between a top-level function and an inline function or
lambda. Both are function values and use the same syntax.

Every function takes exactly one argument and matches it with a pattern.
Patterns are recursive: a binding pattern introduces one name, while a product
pattern matches the elements of a product. Product patterns may contain other
product patterns.

`=>` introduces the body of the abstraction. `->`, when present, introduces an
explicit result type. Without an explicit result type, stapler infers it from
the body.

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

A singleton product pattern is equivalent to its contained pattern, so
`(value: T)` matches the same values as `value: T`.

A function value may declare its result type directly:

```staple
let get_number = () -> I32 => {
    42
}
```

A binding annotation may also constrain the complete function type. A function
declaration uses the same function-type syntax and omits the value:

```staple
let add: (x: I32, y: I32) -> I32
```

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
for example `use math.((<>))`; qualified values and calls may be written
`(math.<>)` and `1 math.<> 2`.

## Block expressions

Braces construct a block expression:

```staple
{
    let x = 1
    x
}
```

A block may contain bindings and expressions. Its final expression is the
value returned by the block.

The value of an empty block and whether a trailing separator changes a block's
value are currently unspecified.

## Types

Named types are written as identifiers:

```staple
I32
Bool
String
CString
CChar
```

`I32`, `Bool`, `String`, and the arithmetic functions from `std.core` are imported
implicitly into every source module. These are ordinary type names rather than
keywords, so a local declaration or explicit import can shadow a prelude name.
Integer literals have type `I32`.

`String` is an owned UTF-8 buffer represented by a pointer, byte length, and
capacity. String literals have this canonical type. Until Staple gains
move/drop semantics, allocated string buffers live until process exit.

`CChar` and `CString` are public opaque types in `std.cinterop`. Source code
must import them explicitly, for example with `use std.cinterop.*`. `CString`
is a raw pointer to NUL-terminated bytes. `string_from_c_string` validates and
copies UTF-8 into a `String`; `string_to_c_string` copies a `String`, appends a
terminator, and traps on an interior NUL byte. Invalid UTF-8 also traps.

An underscore asks stapler to infer a type:

```staple
_
_ -> I32
```

The currently supported type syntax also includes pointer types, product types,
variadic markers in external function signatures, and function types:

```staple
*const CChar
(I32, String)
(*const CChar, ...) -> I32
```

`...` denotes the variadic portion of an external function parameter product. Its
meaning outside foreign declarations is currently unspecified.

`I32` is currently the only standard-library numeric type. The future numeric
type set, mutability model for pointers, type inference rules, and broader type
compatibility rules remain unspecified.

### Type declarations

staple distinguishes transparent aliases, distinct type definitions, and
opaque type declarations.

An opaque type has no source-level representation:

```staple
pub type Handle
```

Opaque values can be named and used behind pointers. A by-value use requires a
representation supplied by the compiler or another future implementation
mechanism. `std.core.I32` is an opaque declaration whose representation is
provided by Stapler.

#### `type`

`type` creates a distinct nominal type
with the same runtime representation as its underlying type:

```staple
type UserId = I32
type OrderId = I32
```

`UserId`, `OrderId`, and `I32` are distinct types and are not implicitly
interchangeable, even though they share a representation. This provides type
safety without adding a runtime wrapper. The syntax for constructing,
unwrapping, or explicitly converting these types remains unspecified.

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

## Foreign declarations

An `extern` block declares values supplied by a foreign ABI. The ABI name is a
string following `extern`:

```staple
use std.cinterop.*

extern "c" {
    let printf: (*const CChar, ...) -> I32
}
```

The example declares an immutable foreign value named `printf`. Its type is a
function from one product parameter—containing a C string pointer followed by
variadic values—to an `I32` result.

External symbols are resolved by the native linker when producing an
executable. Libraries outside the platform defaults can be supplied to
`stapler --emit exe` with `-L` and `-l`. Calling-convention details, supported
ABI names, foreign symbol aliases, and foreign type layouts remain unspecified.
