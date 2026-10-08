# michi-cli

The `michi` binary. It reads a `.michi` file, a description of a pipeline
with no Rust in it, and runs the pipeline through the `michi` library.

Under construction. Today the binary parses a file and reports either an
error or the number of pipelines in it:

```
cargo run -p michi-cli -- pipeline.michi
```
