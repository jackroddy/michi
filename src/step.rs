use crate::closure::Closure;
use crate::cmd::{Cmd, Memory};
use crate::execute::Status;
use crate::item::Item;
use crate::label;

/// How far a failed command reaches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnError {
    /// Run the rest of this step's commands anyway.
    Continue,
    /// Skip the rest of this step. The pipeline carries on to the next one.
    Skip,
    /// Skip the rest of this step, skip every step after it, and fail the run.
    #[default]
    Abort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    Serial,
    Batched { jobs: usize },
}

/// What a step holds: commands or closures.
#[derive(Debug)]
pub(crate) enum Items<'a> {
    Cmds {
        cmds: Vec<Cmd>,
        strategy: Strategy,
        /// How many cores each of these asks for, unless it asked for itself.
        cores: Option<usize>,

        /// Where each of these takes its pages from, unless it said for itself.
        memory: Option<Memory>,
    },
    /// Closures, run one after another on the calling thread.
    Closures(Vec<Closure<'a>>),
}

#[derive(Debug)]
pub struct Step<'a> {
    pub(crate) name: Option<String>,
    pub(crate) index: Option<usize>,
    pub(crate) on_error: OnError,
    pub(crate) elapsed_s: Option<f64>,
    pub(crate) items: Items<'a>,

    /// How many cores to carve for this step alone, when it asked.
    pub(crate) pool: Option<usize>,

    /// Whether it runs in a pool, its own or the pipeline's. Settled when the
    /// pipeline is built.
    pub(crate) pooled: bool,

    /// The cpus of that pool, once the step has started.
    pub(crate) pool_cpus: Vec<usize>,

    /// The nodes those cpus sit on, empty on a machine with one.
    pub(crate) pool_nodes: Vec<usize>,
}

impl<'a> Step<'a> {
    pub fn serial(cmds: impl IntoIterator<Item = Cmd>) -> Self {
        Step::of(Items::Cmds {
            cmds: cmds.into_iter().collect(),
            strategy: Strategy::Serial,
            cores: None,
            memory: None,
        })
    }

    pub fn batched(jobs: usize, cmds: impl IntoIterator<Item = Cmd>) -> Self {
        Step::of(Items::Cmds {
            cmds: cmds.into_iter().collect(),
            strategy: Strategy::Batched { jobs: jobs.max(1) },
            cores: None,
            memory: None,
        })
    }

    /// One closure after another, on the thread running the pipeline.
    pub fn from_closures(closures: impl IntoIterator<Item = Closure<'a>>) -> Self {
        Step::of(Items::Closures(closures.into_iter().collect()))
    }

    fn of(items: Items<'a>) -> Self {
        Step {
            name: None,
            index: None,
            on_error: OnError::default(),
            elapsed_s: None,
            items,
            pool: None,
            pooled: false,
            pool_cpus: Vec::new(),
            pool_nodes: Vec::new(),
        }
    }

    /// Pin each of this step's commands to `cores` physical cores.
    ///
    /// Per command, not per step: a batch of four with `cores(2)` asks for eight
    /// cores. If the machine cannot spare that many at once, the commands that
    /// cannot be placed wait for the ones that can.
    pub fn cores(mut self, cores: usize) -> Self {
        if let Items::Cmds { cores: c, .. } = &mut self.items {
            *c = Some(cores);
        }
        self
    }

    /// Carve `cores` physical cores for this step as it starts, out of the
    /// pipeline's pool if it has one, and give them back when it ends.
    ///
    /// A command with no cores of its own may run on any core of the pool; one
    /// that asks for some leases them from it. Closures are pinned to the pool.
    pub fn pool(mut self, cores: usize) -> Self {
        self.pool = Some(cores);
        self
    }

    /// Where these commands take their pages from, for any that did not set
    /// their own.
    pub fn memory(mut self, memory: Memory) -> Self {
        // a closure holds no cores, so it is never placed anywhere
        // to take its pages from
        if let Items::Cmds { memory: m, .. } = &mut self.items {
            *m = Some(memory);
        }
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn on_error(mut self, on_error: OnError) -> Self {
        self.on_error = on_error;
        self
    }

    /// What to call this step. Without a name it is empty until the pipeline
    /// is built and numbers it.
    pub fn label(&self) -> String {
        match self.index {
            Some(index) => label::label(index, self.name.as_deref()),
            None => self.name.clone().unwrap_or_default(),
        }
    }

    pub fn cmds(&self) -> &[Cmd] {
        match &self.items {
            Items::Cmds { cmds, .. } => cmds,
            Items::Closures(_) => &[],
        }
    }

    pub fn closures(&self) -> &[Closure<'a>] {
        match &self.items {
            Items::Cmds { .. } => &[],
            Items::Closures(closures) => closures,
        }
    }

    /// Everything this step holds, in the order it was given.
    pub fn items(&self) -> impl Iterator<Item = Item<'_>> {
        self.cmds()
            .iter()
            .map(Item::Cmd)
            .chain(self.closures().iter().map(Item::Closure))
    }

    pub fn wall_s(&self) -> Option<f64> {
        self.elapsed_s
    }

    /// How this step's commands run. `None` for a step of closures, which has
    /// no commands to run either way.
    pub fn strategy(&self) -> Option<Strategy> {
        match &self.items {
            Items::Cmds { strategy, .. } => Some(*strategy),
            Items::Closures(_) => None,
        }
    }

    /// How many of this step's commands can be running at once.
    pub fn width(&self) -> usize {
        match self.strategy() {
            Some(Strategy::Batched { jobs }) => jobs,
            _ => 1,
        }
    }

    pub(crate) fn cmds_mut(&mut self) -> &mut [Cmd] {
        match &mut self.items {
            Items::Cmds { cmds, .. } => cmds,
            Items::Closures(_) => &mut [],
        }
    }

    pub(crate) fn closures_mut(&mut self) -> &mut [Closure<'a>] {
        match &mut self.items {
            Items::Cmds { .. } => &mut [],
            Items::Closures(closures) => closures,
        }
    }

    /// Whether the rest of this step's commands are worth running.
    pub(crate) fn skips(&self) -> bool {
        self.on_error != OnError::Continue && self.failed().is_some()
    }

    /// What to say about the thing that ends the run, if this step holds one.
    pub(crate) fn aborts(&self) -> Option<String> {
        (self.on_error == OnError::Abort)
            .then(|| self.failed())
            .flatten()
    }

    /// The first thing in this step that failed, described.
    fn failed(&self) -> Option<String> {
        match &self.items {
            Items::Cmds { cmds, .. } => cmds
                .iter()
                .find(|c| c.status().failed())
                .map(|c| format!("{} failed: {}", c.label(), c.line())),
            // a closure has no stderr file, so its message is the
            // only record of why it failed
            Items::Closures(closures) => {
                closures.iter().find(|c| c.status().failed()).map(|c| {
                    match c.status() {
                        Status::Failed(why) => format!("{} failed: {why}", c.label()),
                        // a failure that carries no message
                        _ => format!("{} failed", c.label()),
                    }
                })
            }
        }
    }
}

impl From<Cmd> for Step<'_> {
    fn from(cmd: Cmd) -> Self {
        Step::serial([cmd])
    }
}

impl<'a> From<Closure<'a>> for Step<'a> {
    fn from(closure: Closure<'a>) -> Step<'a> {
        Step::from_closures([closure])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::{Status, Timing};

    fn timing(exit: i32) -> Timing {
        Timing {
            wall_s: 1.0,
            user_s: Some(1.0),
            sys_s: Some(0.0),
            max_rss_kb: Some(1024),
            exit,
        }
    }

    /// Two commands, the second having gone however `outcome` says.
    fn step<'a>(on_error: OnError, outcome: Status) -> Step<'a> {
        let mut step = Step::serial([Cmd::new("/a"), Cmd::new("/b")]).on_error(on_error);
        step.cmds_mut()[0].status = Status::Finished(timing(0));
        step.cmds_mut()[1].status = outcome;
        step
    }

    #[test]
    fn nothing_failed_so_nothing_reaches_anywhere() {
        for on_error in [OnError::Continue, OnError::Skip, OnError::Abort] {
            let step = step(on_error, Status::Finished(timing(0)));
            assert!(!step.skips(), "{on_error:?} skipped a clean step");
            assert!(step.aborts().is_none(), "{on_error:?} aborted a clean step");
        }
    }

    #[test]
    fn continue_lets_a_failure_pass() {
        let step = step(OnError::Continue, Status::Finished(timing(1)));
        assert!(!step.skips());
        assert!(step.aborts().is_none());
    }

    #[test]
    fn skip_stops_the_step_and_stops_there() {
        let step = step(OnError::Skip, Status::Finished(timing(1)));
        assert!(step.skips());
        assert!(step.aborts().is_none());
    }

    #[test]
    fn abort_stops_the_step_and_names_what_did_it() {
        let step = step(OnError::Abort, Status::Finished(timing(1)));
        assert!(step.skips());
        let why = step.aborts().expect("a failing command should abort");
        assert!(why.starts_with("b failed: "), "{why}");
    }

    #[test]
    fn a_command_that_never_ran_is_not_a_failure() {
        for outcome in [Status::NotRun, Status::Skipped] {
            let step = step(OnError::Abort, outcome);
            assert!(!step.skips());
            assert!(step.aborts().is_none());
        }
    }

    #[test]
    fn every_way_of_failing_counts() {
        for outcome in [
            Status::Failed("could not spawn".into()),
            Status::TimedOut(timing(143)),
            Status::Finished(timing(1)),
        ] {
            let step = step(OnError::Abort, outcome);
            assert!(step.skips());
            assert!(step.aborts().is_some());
        }
    }

    #[test]
    fn a_batch_always_has_a_worker() {
        // zero jobs would mean no workers at all, which hangs rather than
        // finishing empty
        assert_eq!(Step::batched(0, [Cmd::new("/a")]).width(), 1);
        assert_eq!(Step::batched(4, [Cmd::new("/a")]).width(), 4);
        assert_eq!(Step::serial([Cmd::new("/a")]).width(), 1);
    }
}
