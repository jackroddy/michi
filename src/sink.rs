//! Where results go.
//!
//! A [`Pipeline`](crate::Pipeline) writes no output of its own. It runs
//! commands and reports what happened to the sinks registered on it. Printing
//! progress and writing the summary table are both sinks.
//!
//! # What a sink is promised
//!
//! Everything arrives on one thread, in order, so a sink needs no locking of
//! its own. The calls nest the way the run does:
//!
//! - [`start`](Sink::start) once, before anything runs.
//! - Then, for every step in turn: [`step_start`](Sink::step_start), the items
//!   inside it, then [`step_done`](Sink::step_done). Every step gets both, in
//!   that order, including the ones a failed run never reached, which get
//!   them one after the other with skipped items between.
//! - [`abandoned`](Sink::abandoned) if the run is being given up on, once.
//! - [`finish`](Sink::finish) once, however it ended.
//!
//! Within a step, each item is announced to [`item_done`](Sink::item_done)
//! **exactly once**, and never as [`Status::NotRun`] — by the time you see one
//! it has finished, failed to start, or been skipped.
//!
//! An item that actually ran also gets one [`item_start`](Sink::item_start)
//! before that. Not every item does: one that a stopping step never reached
//! goes straight to `item_done` as skipped. So `item_start` implies an
//! `item_done` will follow, but not the other way round.
//!
//! Both carry the item's position in its step, which tells two items apart
//! when they share a name: [`label`](crate::Item::label) is not unique, since
//! an unnamed command goes by the program it runs.
//!
//! [`Status::NotRun`]: crate::Status

use crate::item::Item;
use crate::step::Step;

/// A receiver for what the pipeline did.
///
/// Every method does nothing by default. Returning `Err` from any of them stops
/// the run with [`Error::Sink`](crate::Error::Sink) holding what the sink
/// returned; `?` on any std error or `anyhow::Error` produces one.
pub trait Sink {
    /// Everything the pipeline is about to run, before any of it has.
    ///
    /// A sink that needs the shape of the whole run, such as the full set of
    /// field keys, works it out here.
    fn start(
        &mut self,
        steps: &[Step<'_>],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = steps;
        Ok(())
    }

    /// The pipeline is starting this step.
    fn step_start(
        &mut self,
        step: &Step<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = step;
        Ok(())
    }

    /// One item holds the cores it asked for and is about to be spawned or
    /// called. `at` is its position in the step.
    ///
    /// Any wait for cores is over, so time measured from here matches the wall
    /// clock that gets recorded.
    fn item_start(
        &mut self,
        step: &Step<'_>,
        at: usize,
        item: Item<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = (step, at, item);
        Ok(())
    }

    /// One item reached its final state. `step` is the one holding it, for the
    /// name and whether its commands ran together, and `at` is its position in
    /// that step.
    fn item_done(
        &mut self,
        step: &Step<'_>,
        at: usize,
        item: Item<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = (step, at, item);
        Ok(())
    }

    /// Every item in `step` has been announced, and its wall clock is final.
    fn step_done(
        &mut self,
        step: &Step<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = step;
        Ok(())
    }

    /// The run is being given up on, and why. The steps after this one still
    /// report their items as skipped, and [`finish`](Sink::finish) still
    /// follows.
    fn abandoned(&mut self, why: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = why;
        Ok(())
    }

    /// The pipeline is over, however it ended.
    fn finish(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}
