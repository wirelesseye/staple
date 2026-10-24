# AGENTS.md

## Do not use the Staple formatter

The Staple formatter is **not production-ready**. Do not run it or build
workflows around it until that changes.

This covers the formatter shipped with the language and its tooling:

- `staple fmt` (including `staple fmt --check`), defined in
  [`staple-cli/src/fmt.rs`](staple-cli/src/fmt.rs)
- the underlying implementation in
  [`staple-syntax/src/formatter.rs`](staple-syntax/src/formatter.rs)

Specifically:

- **Do not run `staple fmt` on `.sta` files** in this repository, the standard
  library, examples, or tests. Its output is not yet trusted to preserve
  formatting intent, and it may produce diffs that are hard to review or that
  change code beyond whitespace.
- **Do not add formatter steps** to CI, commit hooks, scripts, or editor
  integration. A format-on-save or format-on-commit setup would rewrite files
  with an unfinished tool.
- **Do not use formatter output as a source of truth** in tests or snapshots.
  Assert on parsed syntax or compiler behavior instead.
- **Do not treat a formatter change as a mechanical cleanup.** If you are
  working on the formatter itself, keep that work isolated and clearly
  described in its own change, with tests for the specific behavior you touch.

When writing Staple code, format it by hand and match the style of the
surrounding file.
