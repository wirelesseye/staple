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

Whitespace and comments are preserved by the parser, so parsing and reproducing
a source file does not change its text. Newlines also separate adjacent
statements where they would otherwise form a single expression.

Line comments begin with `//` and continue to the end of the line.

### Top-level statements

A source file may contain expression statements alongside bindings, type
declarations, and foreign declarations:

```staple
extern "c" {
    let printf: (*const c_char, ...) -> i32
}

printf ("hello, world!\n")
```

There is no distinguished entry-point function. 

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

A function declaration may omit its value when its complete function type is
given. Whether other ordinary, non-external `let` declarations may omit their
value is currently unspecified.

### `def`

`def` defines a hoisted value. It is comparable to JavaScript's `var`: the
binding is available throughout its containing scope, rather than becoming
available only after the definition.

```staple
def greet = () => {
    printf ("hello, world!\n")
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
def get_number: _ -> i32 = () => {
    42
}
```

`_` is an inferred type placeholder. In this example, stapler infers the
function's parameter type from `()` while requiring its result to be `i32`.
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

String literals use double quotes. A backslash protects the following quote
from ending the string. The complete set and meaning of escape sequences is
currently unspecified.

The arithmetic operators currently recognized by stapler are:

```text
*  /
+  -
```

Multiplication and division bind more tightly than addition and subtraction.

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
let args: (name: string, int)
```

Here, `args` is a two-element product. Its first element is named `name` and has
type `string`; its second element is unnamed and has type `int`.

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
let args: (name: string, int)

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

A binding pattern has a name and a type:

```staple
s: string => printf ("%s\n", s)
```

A nullary product pattern is written as `()`:

```staple
() => 42
```

A product pattern names and types each of its elements:

```staple
(a: i32, b: i32) => a + b
```

Consequently, this definition does not define a function with two parameters:

```staple
let add = (a: i32, b: i32) => a + b
```

It defines a function whose single parameter is a two-element product.

Patterns may be nested:

```staple
(x: i32, (y: i32, z: i32)) => x + y + z
```

A singleton product pattern is equivalent to its contained pattern, so
`(value: T)` matches the same values as `value: T`.

A function value may declare its result type directly:

```staple
let get_number = () -> i32 => {
    42
}
```

A binding annotation may also constrain the complete function type. A function
declaration uses the same function-type syntax and omits the value:

```staple
let add: (x: i32, y: i32) -> i32
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
printf ("%s\n", s)
```

Here, `printf` receives one two-element product.

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

The currently supported type syntax also includes pointer types, product types,
variadic markers in external function signatures, and function types:

```staple
*const c_char
(i32, string)
(*const c_char, ...) -> i32
```

`...` denotes the variadic portion of an external function parameter product. Its
meaning outside foreign declarations is currently unspecified.

The built-in type set, mutability model for pointers, type inference rules, and
type compatibility rules remain unspecified.

### Type declarations

staple distinguishes transparent aliases from distinct type definitions.

#### `type`

`type` creates a distinct nominal type
with the same runtime representation as its underlying type:

```staple
type UserId = int
type OrderId = int
```

`user_id`, `order_id`, and `int` are distinct types and are not implicitly
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
    name: string,
    age: i32,
)
```

Here, `person` is exactly the named product type on the right-hand side. The alias
does not create a separate nominal identity.

## Foreign declarations

An `extern` block declares values supplied by a foreign ABI. The ABI name is a
string following `extern`:

```staple
extern "c" {
    let printf: (*const c_char, ...) -> i32
}
```

The example declares an immutable foreign value named `printf`. Its type is a
function from one product parameter—containing a C string pointer followed by
variadic values—to an `i32` result.

Only the syntax of foreign declarations is currently defined. Linking,
calling-convention details, supported ABI names, and foreign type layouts are
unspecified.
