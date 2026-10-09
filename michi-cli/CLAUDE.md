# michi-cli

The `michi` binary: it reads a `.michi` file, which describes a pipeline
without Rust, and runs it through the library.

## Status

Every pass exists, and the binary runs a file. The design of the file
format is in foam, as notes on the milestone issue for the format, one
note per topic. Read those before touching the parser.

## Command line

`michi FILE` runs every pipeline in the file in order and stops at the
first that fails. `--dry-run` prints the library's plan instead, `--sh`
prints the file as a bash script, `-p NAME` picks a pipeline and repeats,
`--silent` drops progress output, `--table PATH` writes a table and is an
error when the file declares one. A Progress sink with defaults is on
unless the file declares its own. Exit codes: 0 when every step passed,
1 when a step or pipeline failed while running, 2 when nothing ran, for
a file that would not load, a pipeline this machine cannot build, or a
bad command line. The flags are parsed with clap, the one dependency the
crate has beyond the library; the parser itself has none.

## Structure

The parser is written by hand with no dependencies. Four passes, in
order, because an attribute argument cannot be typed until its sweep
variable is bound:

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

michi-cli follows [semantic versioning](https://semver.org/spec/v2.0.0.html).
While the crate is below 1.0, a breaking change takes the minor number.
A change to what a `.michi` file means is a breaking change.

Tag every release, annotated, as `michi-cli-vMAJOR.MINOR.PATCH`, on the
commit that was published.

## Changelog

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Anything that changes what someone running the binary sees goes under
`[Unreleased]` in the commit that changes it.
