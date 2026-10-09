# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-10-09

### Added

- The `.michi` file format: `data` blocks, `pipeline` and `step` blocks,
  `#[…]` attributes for every builder setting, parameter lists with ranges
  and tuple patterns, sweep blocks that repeat a step, a step's body or a
  single command, and commands written as shell text ending in `;`.
- The `michi` binary. `michi FILE` runs every pipeline in the file;
  `--dry-run` prints the plan, `--sh` prints the file as a bash script,
  `-p NAME` picks a pipeline, `--silent` drops progress output and
  `--table PATH` writes a table.
- A Vim syntax file under `editors/vim/`.

[Unreleased]: https://github.com/jackroddy/michi/compare/michi-cli-v0.1.0...HEAD
[0.1.0]: https://github.com/jackroddy/michi/releases/tag/michi-cli-v0.1.0
