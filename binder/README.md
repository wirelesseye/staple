# Binder

Binder is the project manager for the [Staple language](../Staple.md). It is a
separate executable from [Stapler](../stapler/README.md) and invokes the compiler
as a subprocess. A Staple toolchain installation should provide both programs;
Binder looks for the Stapler executable, `stpl`, beside its own executable first
and then on `PATH`.

## Manifest

A project is described by `binder.kdl`:

```kdl
package "hello" {
    root "src/root.sta"
    entry "src/main.sta"
}
```

Packages default to `kind "executable"`. A consumable package declares itself
as a library and may omit its entry:

```kdl
package "geometry" {
    kind "library"
    dependencies {
        math path="../math"
    }
}
```

Dependency paths are relative to the declaring manifest and must name local
directories containing a library `binder.kdl`. The dependency node name is its
Staple import alias, so the example is imported with `use math...` even if the
dependency package has a different name. Dependencies resolve recursively;
cycles and executable dependency targets are rejected.

The standard library is itself a library package (`stdlib/binder.kdl`, shipped
beside the compiler). The compiler adds it to every graph as an implicit
dependency under the alias `std`, so `use std....` works without a
`dependencies` entry and `std` is a reserved alias a manifest may not bind.

`root` and `entry` are relative to the manifest directory. `root` defaults to
`src/root.sta`, and its file is optional; its parent directory still anchors
package module paths. Executable entries default to `src/main.sta`. Libraries
only have an entry when one is explicitly declared. The minimal executable
manifest is:

```kdl
package "hello"
```

Binder discovers the nearest `binder.kdl` in the current directory or its
ancestors. `--manifest-path <path>` selects one explicitly.

Binder v1 supports local directory dependencies only. It does not support
registry or Git dependencies, versions, workspaces, profiles, incremental
builds, or project lockfiles.

## Commands

Create a new project in `<name>/`:

```text
binder new hello
```

This creates:

```text
hello/
├── binder.kdl
├── .gitignore
└── src/
    └── main.sta
```

The manifest uses the default root module and entry path; `main.sta` contains a
hello-world program, and `.gitignore` ignores `/build`. Binder refuses to create
the project if the destination already exists and does not initialize a Git
repository.

Check a project without generating code:

```text
binder check
```

For a library, this validates its root plus every public file module and their
reachable imports. Unused private files are outside that public surface.

Build a native executable:

```text
binder build
```

For a library without an entry, `build` performs the same validation as
`check`, produces no artifact, and rejects output, target, and linker options.
A library with an explicit entry also builds and runs as an executable.

The default output is `build/<package>` beside the manifest. `-o <path>` selects
a different output. Build also accepts `--target`, `--stdlib`, `--linker`, `-L`,
and `-l`.

Build for the host and run the persistent executable:

```text
binder run -- first --second
```

`binder run` performs a fresh build, inherits standard input and output, and
forwards arguments following `--`. It accepts `--stdlib`, `--linker`, `-L`, and
`-l`; cross-target execution and `-o` are not supported.
