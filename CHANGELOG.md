# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A command's cores now come off one memory node. A command asking for `cores`
  gets them from a single node wherever one has that many free, where before it
  got whichever cpus were lowest-numbered, so its threads and the memory they
  touch stay on one side of the interconnect. A request larger than any one node
  has free crosses as few of them as its size forces. Placement only ever
  chooses among the cpus free at that moment, so nothing waits longer than it
  did before.
- A command placed on one node asks the kernel for its pages there, through
  `MPOL_PREFERRED` and not `MPOL_BIND`: one that outgrows its node spills onto
  another and runs slowly instead of being killed. A command that had to take
  cores off two nodes states no preference, which leaves each thread's pages on
  the node that touched them.
- `dry_run` and the table say which node a command landed on. A machine with one
  node has nothing to say there, so it gets no node column and no node in the
  `dry_run` line.
- `Item::nodes()` and `Item::numa()`, so a sink of your own can report placement
  the way the built-in table does.

### Changed

- `Cmd` keeps its options and positionals per subcommand rather than in one flat
  bucket each. `sub` starts a level; `flag`, `arg` and `path` fill in the
  current one, so `Cmd::new("git").arg("-C", dir).sub("commit").arg("-m", msg)`
  now runs as `git -C dir commit -m msg`. In 0.1.0 `Cmd` emitted every
  subcommand word ahead of every option, which put that line out of reach.
  Within a level the order is unchanged: options as given, positionals last.
  No public signature changed, and a command whose options and paths were all
  added after its last `sub` runs exactly as it did before. An option or path
  added *before* a `sub` now comes out ahead of that subcommand word instead of
  behind it.
- `Closure` takes a lifetime, so a closure can borrow a local instead of owning
  its captures or sharing them through an `Arc`. The lifetime propagates through
  `Step`, `PipelineBuilder` and `Pipeline`, which now carry one too, so code
  naming any of those types in a struct field or an impl header has to name the
  lifetime as well. `Send` is still required, because the pipeline moves a step
  into a scoped thread to run a batch.
- michi builds on 64-bit targets only, and says so at compile time. It reaches
  the pinning and accounting syscalls through argument widths that have only
  ever been built and run against on 64-bit Linux, and a narrower target was
  never checked rather than deliberately supported.

## [0.1.0] - 2026-09-08

### Added

- `Cmd`, a builder for a command: program, nested subcommands, flags, options,
  positionals, environment, working directory, timeout, and stdout/stderr
  redirection, plus fields and tags of your own to carry through to the output.
- `Step`, a group of commands run `serial`, `batched` n at a time, or
  `from_closures`. `OnError` sets how far a failure reaches: run the rest of the
  step anyway, skip the rest of the step, or skip everything after it and fail
  the run, which is the default.
- `Closure`, Rust to run in place of a command. Only its wall clock is measured:
  a thread has no `wait4` to ask for cpu time or peak memory, and it can be
  neither killed on a deadline nor pinned.
- `PipelineBuilder` and `Pipeline`, which assemble steps and sinks, resolve core
  counts and stderr routing at `build()`, and `run()` the steps in order.
  `dry_run()` prints the argv of every command and the cpus it would be pinned
  to, taking and returning real leases so the pinning it shows is one a run
  could produce.
- Core pinning. The pool holds one logical cpu per physical core, so a command
  asking for two cores gets two sets of execution units rather than a
  hyperthread pair. Cores are asked for per command or per step with `cores()`,
  and the affinity mask is installed between fork and exec. A command that
  cannot be placed yet waits rather than spinning, and a lease goes back to the
  pool on drop, so a command that failed to spawn releases its cores the same
  way one that finished does.
- `Timing` on every finished item: wall clock for everything, and user time,
  system time, peak resident memory and exit status for commands, read from
  `wait4`.
- `Sink`, the trait for watching a run, with the event order an implementation
  can rely on, and two of them. `Progress` draws a live block with a spinner,
  colors and marks, and stands down to a plain line per result when the output
  is not a terminal or when asked to. `Table` writes an aligned text table of
  wall, user, sys, cpu%, max_rss, exit, status and argv, plus any fields the
  commands carried; `Mode` chooses between one table at the end, a block per
  step as each finishes, or ragged blocks carrying only the columns their own
  commands use.
- Per-run stderr capture, one file per command under a timestamped directory,
  with the path reported alongside the command that wrote it.
- Timeouts, enforced with `SIGTERM` and then `SIGKILL` five seconds later, and
  batch cancellation, which signals the commands still running once a failure
  has ended the step.

[Unreleased]: https://github.com/jackroddy/michi/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jackroddy/michi/releases/tag/v0.1.0
