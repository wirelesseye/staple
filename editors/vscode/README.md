# Staple Language Support

Basic Visual Studio Code support for the Staple programming language.

## Features

- Staple language detection for `.sta` files
- Syntax highlighting for comments, strings, numbers, keywords, declarations,
  types, macro quotations and splices, punctuation, and operators
- Line-comment toggling and bracket matching

This first version is intentionally limited to declarative language support. It
does not provide diagnostics, completion, formatting, navigation, or a language
server.

## Local development

Open this directory in Visual Studio Code and press `F5` to launch an Extension
Development Host. Open any `.sta` file in the new window to test the extension.

The extension can also be loaded directly from the command line:

```sh
code --extensionDevelopmentPath=/absolute/path/to/staple/editors/vscode \
  /absolute/path/to/example.sta
```

Use **Developer: Inspect Editor Tokens and Scopes** from the Command Palette to
inspect the TextMate scopes assigned by the grammar.
