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
  did before. If sysfs leaves any cpu out of every node's cpulist, michi treats
  the whole pool as one node rather than place commands by a partial map.
- `PipelineBuilder::placement` chooses which node a command's cores come off
  when more than one could hold them, taking a `Placement`. `Pack`, the
  default, puts a command on the node with the fewest free cores that still
  fits, keeping the others whole for a wider request. `Spread` puts it on the
  node with the most free cores, so concurrent commands land on separate nodes
  until there are more of them than nodes. On a machine with one node the
  setting changes nothing.
- A command placed on one node asks the kernel for its pages there. It spills
  onto another node when that one fills, so a command needing more memory than
  its node has runs slowly rather than dying.
- `Cmd::memory` and `Step::memory` choose that policy, taking a `Memory`.
  `Preferred` is the default and the behaviour above. `FirstTouch` asks the
  kernel for nothing, for pinning cores without touching memory placement.
  `Bound` holds a command to the nodes it took cores from, and the kernel kills
  it when they fill. A step's setting covers the commands under it, the way its
  core count does. `Preferred` can name only one node, so a command that took
  cores off two states no preference; `Bound` names every node it was given.
- `PipelineBuilder::pool` and `Step::pool` carve a set of cores for the whole
  run or for one step. A command in a pool that asks for no cores of its own
  runs across all of the pool's cores, sharing them with the rest of the pool,
  and the kernel's scheduler moves its threads to whichever of them is idle. A
  command that asks for cores leases them out of the pool as before. A step's
  pool comes out of the pipeline's if there is one, and closures run pinned to
  the pool too. `build` fails for a pool larger than what it is carved from.
  The table names a step's pool on the step's own line, each command sharing
  it reads `pool` in the cpus column, and `dry_run` shows the same.
- Before a command starts, michi checks its nodes against the ones the process
  may allocate from (`Mems_allowed` in `/proc/self/status`). A preference for a
  node outside them is left unset and the command runs anyway. A bind to one
  fails the command without starting it. A preference the kernel turns down
  at exec is skipped too, and the command runs without it.
- `dry_run` and the table say which node a command landed on. A machine with one
  node has nothing to say there, so it gets no node column and no node in the
  `dry_run` line. Beside the node column, a `policy` column holds the memory
  policy each command ran under (`prefer:1`, `bind:0-1` or `default`), with the
  reason when a preference was left unset. `dry_run` shows the same policy at
  the end of each command's line, and marks a command that would wait for cores
  another command in its step still holds.
- `Item::nodes()`, `Item::numa()`, `Item::policy()` and `Item::policy_note()`,
  so a sink of your own can report placement the way the built-in table does.
- `Progress` names the node beside the time and memory on a finished line. It
  says nothing while a command is still running, because a batch hands its
  placement back only once the command is done, and nothing at all on a machine
  with one node. When a command's memory preference was left unset, the line
  says why.

### Changed

- `PipelineBuilder::build` and `Pipeline::run` return `michi::Error` in place
  of `anyhow::Error`, and anyhow is no longer a dependency. `Error::Cores` and
  `Error::Pool` say what did not fit and what it was carved from (`Within`),
  `Error::Io` names the path, `Error::Sink` holds what a sink returned, and
  `Error::Step` names the step that ended the run. `Error` implements
  `std::error::Error`, so code using anyhow still calls michi with `?`.
- `Closure::new` and every `Sink` method return
  `Result<(), Box<dyn std::error::Error + Send + Sync>>` in place of
  `anyhow::Result<()>`. `?` works on any std error and on an `anyhow::Error`,
  and `Err("why".into())` gives a plain message. anyhow's `bail!` and `ensure!`
  return without `?`, so inside a closure or a sink method they need a
  function of their own that returns `anyhow::Result`, called with `?`.
- The table's cpus column and `dry_run` write cpu lists in the form
  `taskset -c` takes, with runs compressed: `0-3` for consecutive cpus,
  `0-94:2` for every other one, and `0,2` as before. Node lists follow the same
  rule. On a machine whose cpu numbers alternate between nodes, one node's cpus
  come out as a single stride.
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
