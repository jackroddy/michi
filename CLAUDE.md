# michi workspace

Two crates. `michi/` is the library: it assembles shell command pipelines and
instruments them. `michi-cli/` is the `michi` binary, which runs a pipeline
written in a `.michi` file. Each has its own `CLAUDE.md`.

## Branches

All working changes go on `dev`. Do not open a feature branch unless I ask for
one.

`main` carries releases and nothing else. Push to it when cutting a release,
and leave it alone the rest of the time.

## Formatting

`rustfmt` is the format. Run `cargo fmt --all` before committing.

## Where things go

Issues, decisions, and design go in foam. The repository holds code, tests,
and each crate's `README.md`, `CHANGELOG.md` and `CLAUDE.md`. A design
document, a sketch, or a plan is never a committed file: its text goes on the
relevant foam issue as a note, split by topic when it is long.
