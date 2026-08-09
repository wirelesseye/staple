# Staple Language Support

Basic Visual Studio Code support for the Staple programming language.

## Features

- Staple language detection for `.sta` files
- Syntax highlighting for comments, strings, numbers, keywords, declarations,
  types, macro quotations and splices, punctuation, and operators
- Line-comment toggling and bracket matching
- Semantic highlighting for namespaces, types, traits, macros, functions,
  parameters, variables, and properties
- Live parse, import, name-resolution, type, and ownership diagnostics
- Hover information for inferred expression, binding, and parameter types

Completion, formatting, navigation, and rename are not yet provided.

## Language server

Install the standard library and build the server from the repository:

```sh
./stapler/scripts/install-stdlib.sh
cd stapler
cargo build --bin staple-lsp
```

Install `staple-lsp` on `PATH`, or set `staple.languageServer.path` to the
absolute path of the built executable. If the compiler cannot discover the
standard library from its executable-relative or per-user installation, or
from `STAPLE_STDLIB`, set
`staple.standardLibrary.path` to the repository's `stapler/stdlib` directory.

## Local development

Run `npm install`, open this directory in Visual Studio Code, and press `F5` to
launch an Extension Development Host. Open any `.sta` file in the new window to
test the extension.

The extension can also be loaded directly from the command line:

```sh
code --extensionDevelopmentPath=/absolute/path/to/staple/editors/vscode \
  /absolute/path/to/example.sta
```

Use **Developer: Inspect Editor Tokens and Scopes** from the Command Palette to
inspect the TextMate scopes assigned by the grammar.
