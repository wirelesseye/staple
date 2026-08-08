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
contains `std/core.sta` and `std/cinterop.sta`.

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
    let printf: (CPointer CChar, ...) -> I32
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

## Functions

A function value has one of the following forms:

```text
<parameter> => <body expression>
<parameter> -> <result type> => <body expression>
```

There is no distinction between a top-level function and an inline function or
lambda. Both are function values and use the same syntax.

Every function takes exactly one argument and matches it with a pattern.
Patterns are recursive: a binding pattern introduces one name, a product
pattern matches the elements of a product, and a nominal pattern exposes the
single representation value of a distinct type when that representation is
visible.

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

A block may contain bindings, returns, and expressions. Its final statement
determines the block's value: a final expression supplies its value, while an
empty block or a block ending in a non-expression supplies `()`. A semicolon
after the final expression does not discard that value.

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
CString
CChar
CPointer CChar
```

The integer types, `Bool`, `String`, arithmetic traits, and arithmetic functions
from `std.core` are imported implicitly into every source module. These are
ordinary type names rather than keywords, so a local declaration or explicit
import can shadow a prelude name. Integer literals use an expected integer type
when one is available and otherwise default to `I32`. Mixed-type arithmetic is
not implicit.

`String` is an owned UTF-8 buffer represented by a pointer, byte length, and
capacity. String literals have this canonical type. Until Staple gains
move/drop semantics, allocated string buffers live until process exit.

`CChar`, `CString`, and the generic `CPointer` constructor are public opaque
types in `std.cinterop`. Source code must import them explicitly, for example
with `use std.cinterop.*`. `CPointer T` is the language's C pointer type; the
pointee is a compile-time argument with no runtime field. There are no `*T` or
`*const T` type forms and Staple does not distinguish mutable and const C
pointers.

`CString` remains a distinct raw pointer to NUL-terminated bytes. It is
compatible with `CPointer CChar`, but an arbitrary `CPointer CChar` is not
assumed to be NUL terminated. `string_from_c_string` validates and copies UTF-8
into a `String`; `string_to_c_string` copies a `String`, appends a terminator,
and traps on an interior NUL byte. Invalid UTF-8 also traps.

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
uses unsigned semantics. Staple does not currently provide implicit numeric
conversions or floating-point types.

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
infers `Ok Tree | IOError | ParseError`. With an explicit result annotation,
every normal and propagated result must be contained in that annotation.

Sum types use Staple's internal tagged inline representation and may not appear
anywhere inside an `extern` binding type.

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

External symbols are resolved by the native linker when producing an
executable. Libraries outside the platform defaults can be supplied to
`stapler --emit exe` with `-L` and `-l`. Calling-convention details, supported
ABI names, foreign symbol aliases, and foreign type layouts remain unspecified.
