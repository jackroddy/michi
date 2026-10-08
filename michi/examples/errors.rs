//! What a pipeline's errors look like to the caller, and how closures and sinks
//! hand theirs back: with std errors alone, with an error type of your own, or
//! with anyhow.
//!
//! Every case checks what it got, so a clean exit means each one came out as
//! described.
//!
//! `cargo run --example errors`

use std::fmt;

use anyhow::Context as _;
use michi::{Closure, Cmd, Error, Item, PipelineBuilder, Sink, Step, Within};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// An error type of the caller's own.
#[derive(Debug)]
struct OverBudget {
    done: usize,
}

impl fmt::Display for OverBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "over budget after {} commands", self.done)
    }
}

impl std::error::Error for OverBudget {}

/// A sink that gives up after `limit` commands, in its own error type.
struct Budget {
    done: usize,
    limit: usize,
}

impl Sink for Budget {
    fn item_done(&mut self, _step: &Step<'_>, _at: usize, _item: Item<'_>) -> Result<(), BoxError> {
        self.done += 1;
        if self.done > self.limit {
            return Err(Box::new(OverBudget { done: self.done }));
        }
        Ok(())
    }
}

/// A sink written against anyhow, the way code that already uses it would be.
struct Strict;

impl Sink for Strict {
    fn start(&mut self, steps: &[Step<'_>]) -> Result<(), BoxError> {
        // ? turns the anyhow::Error into the box
        expect_steps(steps.len(), 3).context("Strict refused the run")?;
        Ok(())
    }
}

fn expect_steps(found: usize, wanted: usize) -> anyhow::Result<()> {
    // bail! and ensure! return an anyhow::Error without going through ?, so
    // they belong in a function like this one rather than in a closure or a
    // sink method, which then calls it with ?
    anyhow::ensure!(found == wanted, "expected {wanted} steps, found {found}");
    Ok(())
}

fn true_cmd(name: &str) -> Cmd {
    Cmd::new("/bin/true").name(name)
}

/// Code that already uses anyhow calls michi with ? like anything else.
fn with_anyhow() -> anyhow::Result<()> {
    PipelineBuilder::new()
        .step(true_cmd("fine"))
        .no_stderr()
        .build()?
        .run()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // a build that can never be placed says what did not fit, as fields
    let built = PipelineBuilder::new()
        .step(Step::serial([true_cmd("greedy").cores(100_000)]))
        .no_stderr()
        .build();
    match built {
        Err(Error::Cores {
            want,
            within: Within::Machine,
            ..
        }) => assert_eq!(want, 100_000),
        other => panic!("expected Error::Cores, got {:?}", other.err()),
    }
    println!("build refused: wants more cores than the machine has");

    // closures with std errors alone: ? on an io error, and a plain message
    let run = PipelineBuilder::new()
        .step(Step::from_closures([
            Closure::new("read", || {
                std::fs::read_to_string("/proc/self/status")?;
                Ok(())
            }),
            Closure::new("refuse", || Err("not today".into())),
        ]))
        .no_stderr()
        .build()?
        .run();
    match &run {
        Err(Error::Step { why, .. }) => assert_eq!(why, "refuse failed: not today"),
        other => panic!("expected Error::Step, got {other:?}"),
    }
    println!("std closure: {}", run.unwrap_err());

    // a closure over anyhow code keeps the whole context chain
    let run = PipelineBuilder::new()
        .step(Step::from_closures([Closure::new("count", || {
            Ok(expect_steps(2, 3).context("counting")?)
        })]))
        .no_stderr()
        .build()?
        .run();
    match &run {
        Err(Error::Step { why, .. }) => {
            assert_eq!(why, "count failed: counting: expected 3 steps, found 2")
        }
        other => panic!("expected Error::Step, got {other:?}"),
    }
    println!("anyhow closure: {}", run.unwrap_err());

    // a sink's own error type comes back inside Error::Sink, and downcasts
    let run = PipelineBuilder::new()
        .step(Step::serial([true_cmd("a"), true_cmd("b"), true_cmd("c")]))
        .sink(Budget { done: 0, limit: 2 })
        .no_stderr()
        .build()?
        .run();
    match &run {
        Err(Error::Sink(e)) => {
            let over = e.downcast_ref::<OverBudget>().expect("the sink's own type");
            assert_eq!(over.done, 3);
        }
        other => panic!("expected Error::Sink, got {other:?}"),
    }
    println!("typed sink: {}", run.unwrap_err());

    // a sink written with anyhow comes back the same way, and its context
    // is the error's source chain
    let run = PipelineBuilder::new()
        .step(true_cmd("only"))
        .sink(Strict)
        .no_stderr()
        .build()?
        .run();
    let err = run.expect_err("Strict wants three steps");
    assert!(matches!(err, Error::Sink(_)));
    let source = std::error::Error::source(&err).map(|e| e.to_string());
    assert_eq!(err.to_string(), "Strict refused the run");
    assert_eq!(source.as_deref(), Some("expected 3 steps, found 1"));
    println!("anyhow sink: {err}: {}", source.unwrap_or_default());

    with_anyhow().map_err(|e| format!("{e:#}"))?;
    println!("anyhow caller: ? on michi's Error works");

    Ok(())
}
