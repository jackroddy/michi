# michi-cli

The `michi` binary: it reads a `.michi` file, which describes a pipeline
without Rust, and runs it through the library. The crate name is a
placeholder until Jack chooses one. The binary is named `michi` either way.

## Status

Every pass exists, and the binary runs a file. The command line is not
designed: `michi FILE`, which runs every pipeline in the file in order,
and `michi sh FILE`, which prints the script, are placeholders for it.
The design of the file format is in foam, as notes on the milestone issue
for the format, one note per topic. Read those before touching the parser.

## Structure

The parser is written by hand and has no dependencies beyond the library.
Four passes, in order, because an attribute argument cannot be typed until
its sweep variable is bound:

- `lex.rs`: text to tokens, and the scan of a command to its `;` under the
  shell's quoting rules.
- `parse.rs`, `ast.rs`: tokens to an untyped tree.
- `resolve.rs`: data references and parameter lists checked, ranges
  turned into their values. The script comes from this tree.
- `sh.rs`: the resolved tree as a bash script.
- `expand.rs`: sweeps applied, every `$N` replaced, labels assigned, the
  bindings kept for later errors.
- `lower.rs`: the flat tree as the library's builders, every attribute
  argument typed here. Each command runs as `/bin/sh -c TEXT`.

`error.rs` is the one error type: a message, a span, and the sweep bindings
if any. The parser stops at the first error.

## Tests

A golden case is three files under `tests/cases/`: `x.michi`, the `x.sh`
it compiles to, and the `x.out` that script prints when bash runs it in
an empty directory. Cases use only commands that exist everywhere, such
as `seq`, `wc`, `echo` and `printf`, and print nothing that depends on
the machine. Adding a case is writing the `.michi`, running the tests
with `MICHI_UPDATE_GOLDEN=1`, and reading the two files that produces.

## Releases

Tag as `michi-cli-vMAJOR.MINOR.PATCH`. Nothing is published until the
crate has its real name.
