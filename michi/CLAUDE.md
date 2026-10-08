# michi

A Rust library for assembling shell command pipelines and instrumenting them:
wall clock, cpu time, peak memory, and core pinning. A `Step` holds commands or
closures, a `Sink` watches a run, and `Progress` and `Table` are the two sinks
that ship with it.

## Releases

michi follows [semantic versioning](https://semver.org/spec/v2.0.0.html). While
the crate is below 1.0, a breaking change takes the minor number.

Tag every release, annotated, as `michi-vMAJOR.MINOR.PATCH`, on the commit that
was published. Releases up to `v0.2.0` carry the bare `v` prefix from before
the workspace had a second crate.

## Changelog

`CHANGELOG.md` follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Anything that changes what someone using the library sees goes under
`[Unreleased]` in the commit that changes it.

## Platforms

64-bit Linux. The build fails on a narrower target. It compiles on macOS, where
there is no affinity syscall to call, so cores are still leased and counted but
nothing is ever pinned to one.
