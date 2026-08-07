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

## Source files

staple source files use the `.sta` extension.

Whitespace, newlines, and comments are not semantically significant in the
syntax currently supported by stapler. They are nevertheless preserved by the
parser, so parsing and reproducing a source file does not change its text.

Line comments begin with `//` and continue to the end of the line.

## Bindings

staple has two binding keywords: `let` and `def`.

### `let`

`let` declares or defines an immutable value.

```staple
let answer = 42
```

A type may be written after the binding name:

```staple
let answer: i32 = 42
```

An external declaration may omit its value because its implementation is
provided outside staple:

```staple
extern "c" {
    let printf: (*const c_char, ...) -> i32
}
```

Whether an ordinary, non-external `let` declaration may omit its value is
currently unspecified.

### `def`

`def` defines a hoisted value. It is comparable to JavaScript's `var`: the
binding is available throughout its containing scope, rather than becoming
available only after the definition.

```staple
def main: _ -> i32 = () -> {
    printf ("hello, world!\n")
}
```

`def` is not a function-declaration keyword. It can bind any value. In the
example above, the value assigned to `main` happens to be a function value.

The precise behavior of reading a hoisted binding before its initializer is
evaluated is currently unspecified.

### Binding type annotations

The optional type annotation on a binding describes the complete type of its
value. Function result types are declared here rather than inside function-value
syntax:

```staple
def main: _ -> i32 = () -> {
    0
}
```

`_` is an inferred type placeholder. In this example, stapler infers the
function's parameter type from `()` while requiring its result to be `i32`.
Inferred placeholders may appear wherever a type is expected.

## Values and expressions

staple is expression-oriented. Function values, function applications, lists,
and blocks are all expressions.

The syntax currently recognized for literal values includes strings and
integers:

```staple
"hello"
42
```

String literals use double quotes. A backslash protects the following quote
from ending the string. The complete set and meaning of escape sequences is
currently unspecified.

The arithmetic operators currently recognized by stapler are:

```text
*  /
+  -
```

Multiplication and division bind more tightly than addition and subtraction.

### Lists

The staple list is its single fixed-size product type. Lists replace tuples,
structs, records, and fixed-size arrays found in other languages. A list has an
ordered, fixed number of elements, and its elements may have different types.

Parentheses construct a list:

```staple
()          // a nullary list
(value)     // a list containing one value
(a, b)      // a list containing two values
```

Parentheses are therefore not grouping syntax in the current design.

#### Named elements

Every list element may optionally have a name. Names are written before the
element type in a list type:

```staple
let args: (name: string, int)
```

Here, `args` is a two-element list. Its first element is named `name` and has
type `string`; its second element is unnamed and has type `int`.

Names may likewise be supplied when constructing a list value:

```staple
let args = (name: "staple", 1)
```

Named and unnamed elements can be mixed in the same list. Whether names must be
unique and whether a value's element names must exactly match its annotated
type remain unspecified.

#### Element access

Every element can be accessed by its zero-based index. A named element can also
be accessed by name:

```staple
let args: (name: string, int)

args.name // the first element, accessed by name
args.0    // the first element, accessed by index
args.1    // the second element, accessed by index
```

Name and index access refer to the same underlying elements; names do not
change the order or size of a list. Accessing an absent name or an index outside
the list's fixed bounds is an error. Whether that error is always detected at
compile time remains unspecified.

## Functions

A function is a value with the following general form:

```text
<parameter> -> <body expression>
```

There is no distinction between a top-level function and an inline function or
lambda. Both are function values and use the same syntax.

Every function takes exactly one parameter. That parameter can represent:

- an ordinary value;
- a nullary list; or
- a list containing one or more values.

An ordinary parameter has a name and a type:

```staple
s: string -> printf ("%s\n", s)
```

A nullary-list parameter is written as `()`:

```staple
() -> 42
```

A list parameter names and types each of its elements:

```staple
(a: i32, b: i32) -> a + b
```

Consequently, this definition does not define a function with two parameters:

```staple
def add = (a: i32, b: i32) -> a + b
```

It defines a function whose single parameter is a two-element list.

A binding may constrain the complete type of a function value, including its
result type:

```staple
def main: _ -> i32 = () -> {
    0
}
```

The result type is not part of the function value itself. This keeps the arrow
unambiguous: everything following `->` is the body expression.

## Function application

Function application is written by placing the argument expression after the
function expression. No dedicated call punctuation is required.

```staple
println "Hello, world!"
```

Because each function accepts one value, passing several logical arguments
means passing a list:

```staple
printf ("%s\n", s)
```

Here, `printf` receives one two-element list.

## Block expressions

Braces construct a block expression:

```staple
{
    let x = 1
    x + 1
}
```

A block may contain bindings and expressions. Its final expression is the
value returned by the block.

The value of an empty block and whether a trailing separator changes a block's
value are currently unspecified.

## Types

Named types are written as identifiers:

```staple
i32
string
c_char
```

An underscore asks stapler to infer a type:

```staple
_
_ -> i32
```

The currently supported type syntax also includes pointer types, list types,
variadic markers in external function signatures, and function types:

```staple
*const c_char
(i32, string)
(*const c_char, ...) -> i32
```

`...` denotes the variadic portion of an external function parameter list. Its
meaning outside foreign declarations is currently unspecified.

The built-in type set, mutability model for pointers, type inference rules, and
type compatibility rules remain unspecified.

### Type aliases

`type alias` gives a name to an existing type. It is especially useful for
named list types which would be structs or records in other languages:

```staple
type alias person = (
    name: string,
    age: i32,
)
```

List types and values may have a trailing comma. Whether an alias is purely
structural or introduces a distinct nominal type remains unspecified.

## Foreign declarations

An `extern` block declares values supplied by a foreign ABI. The ABI name is a
string following `extern`:

```staple
extern "c" {
    let printf: (*const c_char, ...) -> i32
}
```

The example declares an immutable foreign value named `printf`. Its type is a
function from one list parameter—containing a C string pointer followed by
variadic values—to an `i32` result.

Only the syntax of foreign declarations is currently defined. Linking,
calling-convention details, supported ABI names, and foreign type layouts are
unspecified.

## Complete example

```staple
extern "c" {
    let printf: (*const c_char, ...) -> i32
}

type alias person = (
    name: string,
    age: i32,
)

def main: _ -> i32 = () -> {
    printf ("hello, world!\n")
}
```

This program declares the external `printf` value, introduces `person` as an
alias for a named two-element list type, and defines a hoisted `main` binding.
The binding annotation requires an `i32` result and infers the input type.
`main` contains a nullary function value whose block body applies `printf` to a
one-element list and returns the result of that final expression.
