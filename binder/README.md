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

`root` and `entry` are relative to the manifest directory and default to
`src/root.sta` and `src/main.sta`. The root module is optional; its parent
directory still anchors package module paths. The minimal manifest is:

```kdl
package "hello"
```

Binder discovers the nearest `binder.kdl` in the current directory or its
ancestors. `--manifest-path <path>` selects one explicitly.

Binder v1 does not support dependencies, workspaces, profiles, incremental
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
