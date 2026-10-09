# michi-cli

The `michi` binary. It reads a `.michi` file, a description of a pipeline
with no Rust in it, and runs the pipeline through the `michi` library.

```
michi bench.michi              # run every pipeline in the file
michi bench.michi --dry-run    # print the plan and the cores each command gets
michi bench.michi --sh         # print the file as a bash script
michi bench.michi -p quick     # run one pipeline by name
michi bench.michi --silent --table runs.tbl
```

A file that starts with `#!/usr/bin/env michi` runs as `./bench.michi`,
and takes the same flags after its name.

A Vim syntax file for `.michi` is under `editors/vim/`.
