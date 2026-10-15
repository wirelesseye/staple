# Staple

Staple is a programming language with reactive signals and powerful,
approachable metaprogramming as core language features. See
[Staple.md](Staple.md) for the language reference.

The toolchain is a single executable, `staple`, that builds from this Cargo
workspace:

| crate | contents |
| --- | --- |
| [`staple-cli`](staple-cli) | the `staple` executable (this is the only binary) |
| [`staple-compiler`](staple-compiler) | the compiler library: macro expansion, name resolution, checking, typed lowering, and LLVM emission |
| [`staple-project`](staple-project) | the package-manifest and dependency-graph loader |
| [`staple-syntax`](staple-syntax) | syntax trees, lexer, and parser |

```sh
cargo build            # produces target/debug/staple
```

The compiler crate documentation describes phase responsibilities and the
validated lowering boundary. Open it with `cargo doc -p staple-compiler --open`.

## Commands

```text
staple new <name>        create a new package in <name>/
staple build             compile the current package to build/<package>
staple check [file]      type-check the current package, or a single source file
staple run [file] [-- …] build and run the current package, or a single file
staple expand <file|->    print one source file after macro expansion
staple compile <file>    emit LLVM IR, an object file, or an executable
staple fmt [--check] <file|->  format one source file (or standard input)
staple lsp               run the language server on stdin/stdout
```

`check` and `run` operate on the current package (discovered by walking up to
the nearest `staple.kdl`) unless a `.sta` file — or `--manifest-path <path>` —
is given, in which case they act on that file or manifest with the low-level
compiler options (`--module-root`, `--package-root`, `--package-name`).

`staple compile` only produces artifacts and never executes anything. It takes
a source file or `--manifest-path`, honours `--emit llvm|object|exe` (default
`exe`), `-o`, `--target`, `--linker`, `--stdlib`, and `-L`/`-l`. Run
`staple <command> --help` for the full option list.

The project commands invoke the compiler in-process; the only child process
`staple run` starts is the program it just built.

## Incremental builds

`build` and `run` compile each non-root package to its own object file and
reuse it while nothing it depends on changes. An object's file name is a
fingerprint of the compiler executable, the target triple, the package's
sources and resolved features, and its dependencies' fingerprints, so a stale
object is never linked. Objects live in `<package>/target/staple/<triple>/`, and
in `$XDG_CACHE_HOME/staple/<triple>/` (else `~/.cache/staple/<triple>/`) for the
standard library when it has no installed objects and for packages whose
directory is not writable.
`staple build` on a library package populates these caches without producing an
executable. `--no-incremental` (or `STAPLE_INCREMENTAL=0`) compiles one
whole-program module instead.

## Manifest

A package is described by `staple.kdl`:

```kdl
package "hello" {
    root "src"
    entry "src/main.sta"
}
```

Packages default to `kind "executable"`. A consumable package declares itself a
library and may omit its entry:

```kdl
package "geometry" {
    kind "library"
    dependencies {
        math path="../math"
    }
}
```

Dependency paths are relative to the declaring manifest and must name local
directories containing a library `staple.kdl`. The dependency node name is its
Staple import alias, so the example is imported with `use math…` even when the
dependency package has a different name. Dependencies resolve recursively;
cycles and executable dependency targets are rejected.

The standard library is itself a library package
(`stdlib/staple.kdl`, shipped with the toolchain). It is added
to every graph as an implicit dependency under the alias `std`, so `use std.…`
works without a `dependencies` entry and `std` is a reserved alias a manifest
may not bind.

`root` and `entry` are relative to the manifest directory. `root` names the
directory that anchors package module paths and defaults to `src`; the root
module is always `root.sta` inside it, and that file is optional. Executable
entries default to `src/main.sta`. Libraries
only have an entry when one is explicitly declared. The minimal executable
manifest is:

```kdl
package "hello"
```

Packages import `std.prelude.*` automatically by default. Set `prelude #false`
inside `package` to import only the always-available `std.core` interface.

Only local directory dependencies are supported: no registry or Git
dependencies, versions, workspaces, profiles, incremental builds, or lockfiles.

### Features

Packages declare additive features in their manifest:

```kdl
package "application" {
    features {
        default "logging"
        logging
        full "logging" "geometry/simd"
    }
    dependencies {
        geometry path="../geometry" default-features=#false {
            features "base"
        }
    }
}
```

`build`, `check`, and `run` accept repeatable `--features <comma-separated>`,
`--all-features`, and `--no-default-features`. Staple items gate on features
with `@feature("name")`.

## Standard library

The compiler loads the standard library at compile time. Install the repository
copy to the default per-user location with:

```sh
./scripts/install-stdlib.sh
```

This installs to `~/.local/lib/staple/stdlib`; an optional first argument selects
a different destination. It also prebuilds the standard library's objects into
`<destination>/target/<triple>/` with `staple` from `PATH` (or `$STAPLE`; it
falls back to building the compiler from the repository), so ordinary builds
skip compiling `std`. Set `STAPLE_INSTALL_OBJECTS=0` to copy sources only.
Objects are fingerprinted against the compiler executable, so reinstall after
upgrading `staple` and install with the `staple` you will run. `--stdlib <path>` selects the root explicitly and
`STAPLE_STDLIB` provides the same path through the environment. Otherwise the
executable looks in `../lib/staple/stdlib` relative to itself and then in the
per-user default location.
