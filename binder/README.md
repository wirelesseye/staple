# Binder

Binder is the project manager for the [Staple language](../Staple.md). It is a
separate executable from [Stapler](../stapler/README.md) and invokes the compiler
as a subprocess. A Staple toolchain installation should provide both programs;
Binder looks for Stapler beside its own executable first and then on `PATH`.

## Manifest

A project is described by `binder.kdl`:

```kdl
package "hello" {
    root "src"
    entry "main.sta"
}
```

`root` is relative to the manifest directory and defaults to `src`. `entry` is
relative to that source root and defaults to `main.sta`, so the minimal manifest
is:

```kdl
package "hello"
```

Binder discovers the nearest `binder.kdl` in the current directory or its
ancestors. `--manifest-path <path>` selects one explicitly.

Binder v1 does not support dependencies, workspaces, profiles, incremental
builds, or project lockfiles.

## Commands

Check a project without generating code:

```text
binder check
```

Build a native executable:

```text
binder build
```

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

