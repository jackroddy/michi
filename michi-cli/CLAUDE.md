# michi-cli

The `michi` binary: it reads a `.michi` file, which describes a pipeline
without Rust, and runs it through the library. The crate name is a
placeholder until Jack chooses one. The binary is named `michi` either way.

## Status

Only the lexer and parser exist. The command line is not designed. The
design of the file format is in foam, as notes on the milestone issue for
the format, one note per topic. Read those before touching the parser.

## Structure

The parser is written by hand and has no dependencies beyond the library.
Four passes, in order, because an attribute argument cannot be typed until
its sweep variable is bound:

- `lex.rs`: text to tokens, and the scan of a command to its `;` under the
  shell's quoting rules.
- `parse.rs`, `ast.rs`: tokens to an untyped tree.
- resolve: data references looked up, ranges evaluated. Not written.
- expand: sweeps applied, labels assigned. Not written.
- lower: the flat tree to the library's builders, arguments typed. Not
  written.

`error.rs` is the one error type: a message, a span, and the sweep bindings
if any. The parser stops at the first error.

## Tests

A golden test is a pair under `tests/cases/`: `x.michi` beside the `x.sh`
it should compile to. Adding a case is adding the pair and nothing else.
Sweep expansion gets unit tests of its own, since the script leaves the
loops to bash.

## Releases

Tag as `michi-cli-vMAJOR.MINOR.PATCH`. Nothing is published until the
crate has its real name.
