# Stapler

Stapler is the compiler and language server for the [Staple language](../Staple.md).

## Compiling

The `stpl` command builds a native executable by default. Without `-o`, the
output is written next to the source file without the `.sta` extension:

```text
stpl examples/hello_world.sta
# writes examples/hello_world
```

It can explicitly write LLVM IR, a native object file, or a linked executable:

```text
stpl --emit llvm examples/hello_world.sta
stpl --emit llvm -o hello_world.ll examples/hello_world.sta
stpl --emit object -o hello_world.o examples/hello_world.sta
stpl --emit exe -o hello_world examples/hello_world.sta
```

The native target is used unless `--target <triple>` is supplied. Executables
are linked through `$CC`, or `cc` when `$CC` is unset. `--linker` selects a
different linker driver. `-L <path>` and `-l <name>` add library search paths
and libraries for executable output.

## Checking

`stpl check` stops after loading, name resolution, and type checking. An
explicit package root module allows an entry file to live in a nested directory
while imports remain rooted at the package root's directory:

```text
stpl check --module-root src --package-root src/root.sta src/bin/main.sta
```

## Running

`stpl run` runs a source file without leaving a compiled executable next to
it:

```text
stpl run examples/hello_world.sta
stpl run examples/hello_world.sta -- first --second
```

Run mode privately compiles and links a temporary host executable, executes it
with inherited standard input and output, and removes its temporary artifacts.
Arguments after `--` are forwarded to the process, although Staple does not yet
provide a source-level API for reading them. `--stdlib`, `--linker`, `-L`, and
`-l` are supported; `-o`, `--emit`, and cross-target `--target` are not.

## Standard library

Stapler loads the standard library at compile time. Install the repository copy
to the default per-user location with:

```sh
./scripts/install-stdlib.sh
```

This installs to `~/.local/lib/staple/stdlib`; an optional first argument selects
a different destination. `--stdlib <path>` selects the root explicitly and
`STAPLE_STDLIB` provides the same path through the environment. Otherwise the
compiler and language server look in `../lib/staple/stdlib` relative to their
executable and then in the per-user default location.

The standard-library root contains `std/core.sta`, feature modules under
`std/core/`, and `std/cinterop.sta`. Core features include numbers, booleans,
strings, references, results, syntax, equality, copying, dropping, and defaults.
`std.core` re-exports their public items and remains the stable prelude
interface.
