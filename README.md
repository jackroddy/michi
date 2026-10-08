# michi

A workspace of two crates.

- [`michi`](michi/): a Rust library that assembles shell command pipelines and
  instruments them: wall clock, cpu time, peak memory, and core pinning.
- [`michi-cli`](michi-cli/): the `michi` binary, which runs a pipeline written
  in a `.michi` file. Under construction.
