//! The list of steps, and running them.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use crate::cmd::{Cmd, Output};
use crate::cpu::{Cores, Placement};
use crate::error::{BoxError, Error, Within};
use crate::execute::{Batch, Status, policy, stamp};
use crate::item::Item;
use crate::label;
use crate::sink::Sink;
use crate::step::{Items, Step, Strategy};

/// Where failure logs go unless told otherwise.
const STDERR_DIR: &str = "stderr";

/// A pipeline under construction.
pub struct PipelineBuilder<'a> {
    steps: Vec<Step<'a>>,
    sinks: Sinks,
    stderr_dir: Option<PathBuf>,
    placement: Placement,
    pool: Option<usize>,
}

impl Default for PipelineBuilder<'_> {
    fn default() -> Self {
        PipelineBuilder {
            steps: Vec::new(),
            sinks: Sinks::default(),
            stderr_dir: Some(PathBuf::from(STDERR_DIR)),
            placement: Placement::Pack,
            pool: None,
        }
    }
}

impl<'a> PipelineBuilder<'a> {
    pub fn new() -> Self {
        PipelineBuilder::default()
    }

    pub fn step(mut self, step: impl Into<Step<'a>>) -> Self {
        self.steps.push(step.into());
        self
    }

    pub fn sink(mut self, sink: impl Sink + 'static) -> Self {
        self.sinks.0.push(Box::new(sink));
        self
    }

    pub fn stderr_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.stderr_dir = Some(dir.into());
        self
    }

    pub fn no_stderr(mut self) -> Self {
        self.stderr_dir = None;
        self
    }

    /// Which node a command's cores come off when more than one could hold
    /// them. The default is [`Placement::Pack`].
    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    /// Carve `cores` physical cores for the whole run, and run every step
    /// inside them.
    ///
    /// A command that asks for no cores of its own runs across the whole pool,
    /// or its step's if the step carved one, and the scheduler moves it
    /// wherever a core is idle. Closures are pinned to it too.
    pub fn pool(mut self, cores: usize) -> Self {
        self.pool = Some(cores);
        self
    }

    pub fn build(self) -> Result<Pipeline<'a>, Error> {
        self.build_on(Cores::read())
    }

    fn build_on(self, mut cores: Cores) -> Result<Pipeline<'a>, Error> {
        let PipelineBuilder {
            mut steps,
            sinks,
            stderr_dir,
            placement,
            pool,
        } = self;

        let maybe_dir = stderr_dir.map(|dir| dir.join(stamp()));
        let mut stderr_wanted = false;
        cores.placement = placement;

        if let Some(size) = pool {
            if size == 0 || size > cores.len() {
                return Err(Error::Pool {
                    step: None,
                    size,
                    room: cores.len(),
                    within: Within::Machine,
                });
            }
            cores = cores.carve(size);
        }
        let around = match cores.pooled {
            true => Within::Pipeline,
            false => Within::Machine,
        };

        for (s, step) in steps.iter_mut().enumerate() {
            let s_idx = s + 1;
            step.index = Some(s_idx);
            let step_part = label::filename(s_idx, step.name.as_deref());
            let step_label = step.label();

            // what a command in this step can have at most: the
            // step's pool, if it carves one, or whatever it is in
            let (room, within) = match step.pool {
                Some(size) if size == 0 || size > cores.len() => {
                    return Err(Error::Pool {
                        step: Some(step_label),
                        size,
                        room: cores.len(),
                        within: around,
                    });
                }
                Some(size) => (size, Within::Step),
                None => (cores.len(), around),
            };
            step.pooled = step.pool.is_some() || cores.pooled;

            // a closure routes no stderr and asks for no cores, so there is
            // nothing here for one to pick up
            let Items::Cmds {
                cmds,
                cores: inherited,
                memory: inherited_memory,
                ..
            } = &mut step.items
            else {
                continue;
            };
            let inherited = *inherited;
            let inherited_memory = *inherited_memory;

            for (c, cmd) in cmds.iter_mut().enumerate() {
                let c_idx = c + 1;

                // if we have a stderr dir, we redirect every un-routed stderr
                // to its own file just in case the command ends up failing
                if let Some(dir) = &maybe_dir
                    && cmd.stderr == Output::Null
                {
                    let cmd_part = label::filename(c_idx, cmd.name.as_deref());
                    cmd.stderr =
                        Output::OnFailure(dir.join(format!("{step_part}.{cmd_part}.stderr")));
                    stderr_wanted = true;
                }

                // resolved once here, so nothing downstream has to know a
                // command's core count can come from the step holding it
                if cmd.cores.is_none() {
                    cmd.cores = inherited;
                }
                if cmd.memory.is_none() {
                    cmd.memory = inherited_memory;
                }
                cmd.numa = cores.numa;

                // the only ask the machine could never satisfy: anything else is
                // a matter of waiting, since commands hand their cores back
                let want = cmd.cores.unwrap_or(0);
                if want > room {
                    return Err(Error::Cores {
                        step: step_label,
                        cmd: cmd.label(),
                        want,
                        room,
                        within,
                    });
                }
            }
        }

        Ok(Pipeline {
            steps,
            sinks,
            stderr_dir: if stderr_wanted { maybe_dir } else { None },
            cores,
        })
    }
}

pub struct Pipeline<'a> {
    steps: Vec<Step<'a>>,
    sinks: Sinks,
    stderr_dir: Option<PathBuf>,
    cores: Cores,
}

impl Pipeline<'_> {
    pub fn dry_run(&self) {
        for step in &self.steps {
            let carved = step.pool.map(|size| self.cores.carve(size));
            let cores = carved.as_ref().unwrap_or(&self.cores);

            let whole = cores.whole();
            let pool = match &whole {
                Some((cpus, nodes)) if nodes.is_empty() => {
                    format!(" [pool cpu {}]", crate::cpu::list(cpus))
                }
                Some((cpus, nodes)) => format!(
                    " [pool cpu {} node {}]",
                    crate::cpu::list(cpus),
                    crate::cpu::list(nodes)
                ),
                None => String::new(),
            };
            // a machine with one node sets no policy, so a command
            // sharing the pool there has nothing to add to "pool"
            let shared = |cmd: &Cmd| match &whole {
                Some((_, nodes)) if !nodes.is_empty() => format!(
                    " [pool {}]",
                    policy(cmd.memory.unwrap_or_default(), nodes, &cores.mems).describe()
                ),
                _ => " [pool]".to_string(),
            };
            println!("# {}{pool}", step.label());

            // cores really are taken and given back here, so the pinning shown
            // is one a run could produce. only as many are held at once as the
            // step could run at once; which command gets which is a guess,
            // since that depends on the order workers pick them up
            let mut held = VecDeque::new();

            for cmd in step.cmds() {
                if held.len() >= step.width() {
                    held.pop_front();
                }
                let lease = cores.try_acquire(cmd.cores.unwrap_or(0));
                let cpus = lease.as_ref().map(|l| l.cpus()).unwrap_or_default();
                let nodes = lease.as_ref().map(|l| l.nodes()).unwrap_or_default();
                // the pinning is no longer part of the command, so it gets said
                // beside it rather than shown in it
                let pin = match (cpus, nodes) {
                    // asked for cores the others in its step still hold
                    ([], _) if cmd.cores.unwrap_or(0) > 0 => " [waits for cores]".to_string(),
                    ([], _) if whole.is_some() => shared(cmd),
                    ([], _) => String::new(),
                    (cpus, []) => format!(" [cpu {}]", crate::cpu::list(cpus)),
                    (cpus, nodes) => format!(
                        " [cpu {} node {} {}]",
                        crate::cpu::list(cpus),
                        crate::cpu::list(nodes),
                        policy(cmd.memory.unwrap_or_default(), nodes, &cores.mems).describe()
                    ),
                };
                println!("{} {}{pin}", cmd.label(), cmd.line());

                held.extend(lease);
            }

            for closure in step.closures() {
                // no line to paste, so its name is all there is to show
                println!("{}", closure.label());
            }
        }
    }

    pub fn run(mut self) -> Result<(), Error> {
        // the steps come out so that the rest of the pipeline can be borrowed
        // while they are worked through. run consumes self, so nobody sees the
        // field it leaves behind
        let mut steps = std::mem::take(&mut self.steps);

        self.sinks.each(|s| s.start(&steps))?;

        if let Some(dir) = &self.stderr_dir {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.clone(),
                source,
            })?;
        }

        let outcome = self.run_steps(&mut steps);

        // both of these happen whichever way the run went, so the table is
        // finished and the stderr log is where it says it is by the time the
        // caller hears about anything
        let finished = self.sinks.each(|s| s.finish());
        if let Some(dir) = &self.stderr_dir {
            std::fs::remove_dir(dir).ok();
            if let Some(parent) = dir.parent() {
                std::fs::remove_dir(parent).ok();
            }
        }

        // a sink that could not write comes first: if the table never made it to
        // disk, which command failed is not on record anyway
        let failure = outcome?;
        finished?;

        // last, so everything above has already happened
        match failure {
            Some((step, why)) => Err(Error::Step { step, why }),
            None => Ok(()),
        }
    }

    /// Every step in turn, stopping at the first one that ends the run. Gives
    /// back the command that ended it, if one did.
    fn run_steps(&mut self, steps: &mut [Step<'_>]) -> Result<Option<(String, String)>, Error> {
        let mut failure = None;
        let mut remaining_steps = steps.iter_mut();

        for step in remaining_steps.by_ref() {
            // the step's own pool stands in for the pipeline's for
            // as long as the step runs. nothing else is running, so
            // every core the carve could want is free
            let outer = step.pool.map(|size| {
                let carved = self.cores.carve(size);
                std::mem::replace(&mut self.cores, carved)
            });
            if let Some((cpus, nodes)) = self.cores.whole() {
                step.pool_cpus = cpus;
                step.pool_nodes = nodes;
            }

            let ran = self.run_step(step);
            if let Some(outer) = outer {
                self.cores = outer;
            }
            ran?;

            if let Some(why) = step.aborts() {
                self.sinks.each(|s| s.abandoned(&why))?;
                failure = Some((step.label(), why));
                break;
            }
        }

        // any steps after an early abort get to report here. they still get a
        // start of their own, so a sink never sees a step end that it was not
        // told had begun
        for step in remaining_steps {
            self.sinks.each(|s| s.step_start(step))?;
            self.skip_rest(step)?;
            self.sinks.each(|s| s.step_done(step))?;
        }

        Ok(failure)
    }

    /// One step, start to finish, with its sinks told either side.
    fn run_step(&mut self, step: &mut Step<'_>) -> Result<(), Error> {
        self.sinks.each(|s| s.step_start(step))?;
        match step.strategy() {
            Some(Strategy::Serial) => self.serial(step)?,
            Some(Strategy::Batched { jobs }) => self.batch(step, jobs)?,
            None => self.closures(step)?,
        }
        self.skip_rest(step)?;
        self.sinks.each(|s| s.step_done(step))
    }

    /// One command at a time, stopping early if the step says to.
    fn serial(&mut self, step: &mut Step<'_>) -> Result<(), Error> {
        let start = Instant::now();

        for j in 0..step.cmds().len() {
            // a serial step is the only thing running, so its cores are free
            // the moment it asks: nothing here waits, and announcing before the
            // lease says the same thing as announcing after it
            self.sinks
                .each(|s| s.item_start(step, j, Item::Cmd(&step.cmds()[j])))?;
            self.cores.execute(&mut step.cmds_mut()[j]);
            step.elapsed_s = Some(start.elapsed().as_secs_f64());

            self.sinks
                .each(|s| s.item_done(step, j, Item::Cmd(&step.cmds()[j])))?;
            if step.skips() {
                break;
            }
        }
        Ok(())
    }

    /// One closure at a time, stopping early if the step says to.
    ///
    /// Always serial: a closure runs on the thread that reached it, and nothing
    /// here spawns another.
    fn closures(&mut self, step: &mut Step<'_>) -> Result<(), Error> {
        let start = Instant::now();

        // this is the caller's own thread, so it gets its affinity
        // back once the step is done
        let _pinned = crate::cpu::pin_thread(&step.pool_cpus);

        for j in 0..step.closures().len() {
            // a closure takes no cores, so there is nothing for it to wait on
            self.sinks
                .each(|s| s.item_start(step, j, Item::Closure(&step.closures()[j])))?;
            step.closures_mut()[j].execute();
            step.elapsed_s = Some(start.elapsed().as_secs_f64());

            self.sinks
                .each(|s| s.item_done(step, j, Item::Closure(&step.closures()[j])))?;
            if step.skips() {
                break;
            }
        }
        Ok(())
    }

    /// `jobs` at a time. Workers take the next command whenever they are free, so
    /// one slow command does not idle the rest.
    ///
    /// A step that has failed and is set to stop does not wait for the rest of
    /// the batch to finish: nothing new is taken, and what is already running is
    /// killed. Those come back as `exit 143`, which is a real failure and reads
    /// as one, so a step that stopped shows the one command that broke it and the
    /// ones it took down with it.
    fn batch(&mut self, step: &mut Step<'_>, jobs: usize) -> Result<(), Error> {
        /// What a worker has to say about the command it claimed. Both go back
        /// over the one channel, so the main thread stays the only place that
        /// talks to a sink.
        ///
        /// The command is boxed because a `Started` carries nothing, and every
        /// message on the channel would otherwise be as big as the largest.
        enum Event {
            Started,
            Done(Box<Cmd>),
        }

        let start = Instant::now();

        // the workers run against a copy, which leaves the step free for the main
        // thread to write results into; cloning a Cmd costs nothing next to
        // spawning a process
        let copies: Vec<Cmd> = step.cmds().to_vec();
        let next = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel();
        // made here and dropped here, so a cancel reaches this step's commands
        // and nothing else
        let batch = Batch::new(&self.cores);
        let sinks = &mut self.sinks;

        std::thread::scope(|scope| -> Result<(), Error> {
            for _ in 0..jobs {
                let tx = tx.clone();
                let next = &next;
                let copies = &copies;
                let batch = &batch;
                scope.spawn(move || {
                    loop {
                        if batch.cancelled() {
                            break;
                        }
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        if k >= copies.len() {
                            break;
                        }
                        let mut cmd = copies[k].clone();
                        // announced from inside, once it holds its cores: a
                        // batch can queue for them, and saying so out here
                        // would count the queuing as part of the run
                        batch.execute(&mut cmd, || {
                            let _ = tx.send((k, Event::Started));
                        });
                        // a closed channel means the main thread gave up on us
                        if tx.send((k, Event::Done(Box::new(cmd)))).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);

            // everything the workers have to say is applied here rather than in
            // the workers themselves, so sinks stay single-threaded and the
            // table keeps up during a long batch
            for (k, event) in rx {
                // giving up on the run is not a reason to leave a batch of
                // searches running behind us, so a sink that could not write
                // cancels before it propagates
                let told = match event {
                    // the copy still sitting in the step has not run, which is
                    // all a start has to say: its name, its fields, its tags
                    Event::Started => {
                        sinks.each(|s| s.item_start(step, k, Item::Cmd(&step.cmds()[k])))
                    }

                    Event::Done(cmd) => {
                        // the whole command comes back, not just its status, so
                        // the cpus the worker pinned it to are what gets reported
                        step.cmds_mut()[k] = *cmd;
                        step.elapsed_s = Some(start.elapsed().as_secs_f64());

                        // one the batch was cancelled out from under comes back
                        // never having run. leaving it alone lets skip_rest
                        // announce it once, as skipped, alongside the ones no
                        // worker ever picked up
                        match step.cmds()[k].status() {
                            Status::NotRun => Ok(()),
                            _ => sinks.each(|s| s.item_done(step, k, Item::Cmd(&step.cmds()[k]))),
                        }
                    }
                };

                if let Err(e) = told {
                    batch.cancel(scope);
                    return Err(e);
                }

                if step.skips() {
                    batch.cancel(scope);
                }
            }
            Ok(())
        })
    }

    /// Marks anything that never ran as skipped and tells the sinks about it.
    ///
    /// Two cases end up here: the tail of a step that stopped partway, and every
    /// command of a step the pipeline never got to.
    fn skip_rest(&mut self, step: &mut Step<'_>) -> Result<(), Error> {
        for j in 0..step.cmds().len() {
            if matches!(step.cmds()[j].status(), Status::NotRun) {
                step.cmds_mut()[j].status = Status::Skipped;
                self.sinks
                    .each(|s| s.item_done(step, j, Item::Cmd(&step.cmds()[j])))?;
            }
        }
        for j in 0..step.closures().len() {
            if matches!(step.closures()[j].status(), Status::NotRun) {
                step.closures_mut()[j].status = Status::Skipped;
                self.sinks
                    .each(|s| s.item_done(step, j, Item::Closure(&step.closures()[j])))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::closure::Closure;
    use crate::cmd::Memory;
    use crate::cpu::Cores;
    use crate::step::OnError;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Log {
        /// One entry per `record`, so anything announced twice shows up twice.
        records: Vec<(String, String, Status)>,
        /// One entry per `item_start`, by name.
        started: Vec<String>,
        /// `+label` on a step start and `-label` on a step done, so the two can
        /// be checked for pairing and order.
        order: Vec<String>,
        abandoned: Option<String>,
        finished: usize,
    }

    impl Log {
        fn of(&self, cmd: &str) -> Vec<&Status> {
            self.records
                .iter()
                .filter(|(_, label, _)| label == cmd)
                .map(|(_, _, status)| status)
                .collect()
        }
    }

    /// A sink that writes down everything it is told, and can be asked to fail
    /// when a particular command arrives.
    #[derive(Clone, Default)]
    struct Recorder {
        log: Arc<Mutex<Log>>,
        broken_by: Option<String>,
    }

    impl Recorder {
        fn broken_by(cmd: &str) -> Recorder {
            Recorder {
                broken_by: Some(cmd.to_string()),
                ..Recorder::default()
            }
        }
    }

    impl Sink for Recorder {
        fn step_start(&mut self, step: &Step<'_>) -> Result<(), BoxError> {
            let mut log = self.log.lock().unwrap();
            log.order.push(format!("+{}", step.label()));
            Ok(())
        }

        fn item_start(
            &mut self,
            _step: &Step<'_>,
            _at: usize,
            item: Item<'_>,
        ) -> Result<(), BoxError> {
            self.log.lock().unwrap().started.push(item.label());
            Ok(())
        }

        fn item_done(
            &mut self,
            step: &Step<'_>,
            _at: usize,
            item: Item<'_>,
        ) -> Result<(), BoxError> {
            let mut log = self.log.lock().unwrap();
            log.records
                .push((step.label(), item.label(), item.status().clone()));
            drop(log);

            if self.broken_by.as_deref() == Some(item.label().as_str()) {
                return Err("the sink cannot write".into());
            }
            Ok(())
        }

        fn step_done(&mut self, step: &Step<'_>) -> Result<(), BoxError> {
            let mut log = self.log.lock().unwrap();
            log.order.push(format!("-{}", step.label()));
            Ok(())
        }

        fn abandoned(&mut self, why: &str) -> Result<(), BoxError> {
            self.log.lock().unwrap().abandoned = Some(why.to_string());
            Ok(())
        }

        fn finish(&mut self) -> Result<(), BoxError> {
            self.log.lock().unwrap().finished += 1;
            Ok(())
        }
    }

    fn sh(name: &str, script: &str) -> Cmd {
        Cmd::new("/bin/sh").name(name).arg("-c", script)
    }

    #[test]
    fn a_command_takes_its_core_count_from_the_step_unless_it_has_its_own() {
        let pipeline = PipelineBuilder::new()
            .step(Step::serial([Cmd::new("/a"), Cmd::new("/b").cores(2)]).cores(4))
            .step(Step::serial([Cmd::new("/c")]))
            .no_stderr()
            .build()
            .unwrap();

        assert_eq!(pipeline.steps[0].cmds()[0].cores, Some(4));
        assert_eq!(pipeline.steps[0].cmds()[1].cores, Some(2));
        assert_eq!(pipeline.steps[1].cmds()[0].cores, None);
    }

    #[test]
    fn a_pipeline_packs_unless_told_to_spread() {
        let two = || Cores::with_layout(&[(0, 0), (1, 1)]);
        let pinned = || Step::serial([Cmd::new("/a").cores(1)]);

        let packed = PipelineBuilder::new()
            .step(pinned())
            .no_stderr()
            .build_on(two())
            .unwrap();
        assert_eq!(packed.cores.placement, Placement::Pack);

        let spread = PipelineBuilder::new()
            .step(pinned())
            .placement(Placement::Spread)
            .no_stderr()
            .build_on(two())
            .unwrap();
        assert_eq!(spread.cores.placement, Placement::Spread);
    }

    #[test]
    fn a_command_takes_its_memory_policy_from_the_step_unless_it_has_its_own() {
        let pipeline = PipelineBuilder::new()
            .step(
                Step::serial([Cmd::new("/a"), Cmd::new("/b").memory(Memory::Bound)])
                    .memory(Memory::FirstTouch),
            )
            .step(Step::serial([Cmd::new("/c")]))
            .no_stderr()
            .build()
            .unwrap();

        assert_eq!(pipeline.steps[0].cmds()[0].memory, Some(Memory::FirstTouch));
        assert_eq!(pipeline.steps[0].cmds()[1].memory, Some(Memory::Bound));
        // nothing said anywhere, which execute reads as the default
        assert_eq!(pipeline.steps[1].cmds()[0].memory, None);
    }

    #[test]
    fn a_pool_has_to_fit_inside_what_it_is_carved_from() {
        let machine = || Cores::with_layout(&[(0, 0), (1, 1), (2, 0), (3, 1)]);
        let build = |builder: PipelineBuilder<'static>| builder.no_stderr().build_on(machine());

        assert!(
            build(PipelineBuilder::new().pool(5)).is_err(),
            "more than the machine"
        );
        assert!(
            build(PipelineBuilder::new().pool(0)).is_err(),
            "an empty pool"
        );

        let step = || Step::serial([Cmd::new("/a")]);
        assert!(build(PipelineBuilder::new().pool(2).step(step().pool(3))).is_err());
        assert!(build(PipelineBuilder::new().pool(3).step(step().pool(3))).is_ok());

        let greedy = Step::serial([Cmd::new("/a").cores(3)]).pool(2);
        let Err(error) = build(PipelineBuilder::new().step(greedy)) else {
            panic!("a command wider than its step's pool should not build");
        };
        assert!(
            error.to_string().contains("its step's pool has 2"),
            "{error}"
        );
    }

    #[test]
    fn every_step_of_a_pooled_pipeline_is_pooled() {
        let pipeline = PipelineBuilder::new()
            .pool(2)
            .step(Step::serial([Cmd::new("/a")]))
            .step(Step::from_closures([Closure::new("c", || Ok(()))]))
            .no_stderr()
            .build_on(Cores::with_layout(&[(0, 0), (1, 1), (2, 0), (3, 1)]))
            .unwrap();

        assert!(pipeline.steps.iter().all(|step| step.pooled));
        assert_eq!(pipeline.cores.whole().map(|w| w.0), Some(vec![0, 2]));
    }

    #[test]
    fn asking_for_more_cores_than_the_machine_has_fails_the_build() {
        let too_many = Cores::read().len() + 1;
        let built = PipelineBuilder::new()
            .step(
                Step::serial([Cmd::new("/a").name("greedy")])
                    .name("big")
                    .cores(too_many),
            )
            .no_stderr()
            .build();

        let Err(error) = built else {
            panic!("a pipeline that can never be placed should not build");
        };
        let error = error.to_string();

        assert!(error.contains("greedy"), "{error}");
        assert!(error.contains("big"), "{error}");
        assert!(error.contains(&too_many.to_string()), "{error}");
    }

    #[test]
    fn steps_are_numbered_from_one() {
        let pipeline = PipelineBuilder::new()
            .step(Cmd::new("/a"))
            .step(Cmd::new("/b"))
            .no_stderr()
            .build()
            .unwrap();

        assert_eq!(pipeline.steps[0].label(), "[1]");
        assert_eq!(pipeline.steps[1].label(), "[2]");
    }

    #[test]
    fn an_unrouted_stderr_gets_a_file_of_its_own_and_a_routed_one_is_left_alone() {
        let pipeline = PipelineBuilder::new()
            .step(
                Step::serial([
                    Cmd::new("/a").name("x"),
                    Cmd::new("/b").name("y").stderr(Output::Inherit),
                ])
                .name("s"),
            )
            .stderr_dir("/tmp/pipeline-test-stderr")
            .build()
            .unwrap();

        let Output::OnFailure(path) = &pipeline.steps[0].cmds()[0].stderr else {
            panic!(
                "expected a failure log, got {:?}",
                pipeline.steps[0].cmds()[0].stderr
            );
        };
        assert!(path.ends_with("1-s.1-x.stderr"), "{}", path.display());
        assert_eq!(pipeline.steps[0].cmds()[1].stderr, Output::Inherit);
        assert!(pipeline.stderr_dir.is_some());
    }

    #[test]
    fn no_directory_is_claimed_when_nothing_needed_a_failure_log() {
        let pipeline = PipelineBuilder::new()
            .step(Cmd::new("/a").stderr(Output::Inherit))
            .stderr_dir("/tmp/pipeline-test-unused")
            .build()
            .unwrap();

        assert!(pipeline.stderr_dir.is_none());
    }

    #[test]
    fn a_failed_step_ends_the_run_and_says_which_command_did_it() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        let error = PipelineBuilder::new()
            .step(Step::serial([sh("first", "exit 0")]))
            .step(Step::serial([sh("bad", "exit 1")]))
            .step(Step::serial([sh("never", "exit 0")]))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .unwrap_err()
            .to_string();

        assert!(error.contains("bad"), "{error}");

        let log = log.lock().unwrap();
        assert!(matches!(log.of("first")[..], [Status::Finished(_)]));
        assert!(matches!(log.of("never")[..], [Status::Skipped]));
    }

    #[test]
    fn continue_lets_the_rest_of_the_run_happen() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        PipelineBuilder::new()
            .step(
                Step::serial([sh("bad", "exit 1"), sh("after", "exit 0")])
                    .on_error(OnError::Continue),
            )
            .step(Step::serial([sh("later", "exit 0")]))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .expect("a tolerated failure should not end the run");

        let log = log.lock().unwrap();
        assert!(matches!(log.of("after")[..], [Status::Finished(_)]));
        assert!(matches!(log.of("later")[..], [Status::Finished(_)]));
    }

    #[test]
    fn skip_leaves_the_rest_of_its_own_step_and_nothing_else() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        PipelineBuilder::new()
            .step(
                Step::serial([sh("bad", "exit 1"), sh("sibling", "exit 0")])
                    .on_error(OnError::Skip),
            )
            .step(Step::serial([sh("later", "exit 0")]))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .expect("skip should not fail the run");

        let log = log.lock().unwrap();
        assert!(matches!(log.of("sibling")[..], [Status::Skipped]));
        assert!(matches!(log.of("later")[..], [Status::Finished(_)]));
    }

    #[test]
    fn a_sink_that_cannot_write_still_gets_told_the_run_is_over() {
        let recorder = Recorder::broken_by("bad");
        let log = Arc::clone(&recorder.log);

        let error = PipelineBuilder::new()
            .step(Step::serial([sh("bad", "exit 0")]))
            .step(Step::serial([sh("later", "exit 0")]))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .unwrap_err()
            .to_string();

        assert!(error.contains("cannot write"), "{error}");
        assert_eq!(
            log.lock().unwrap().finished,
            1,
            "finish must run even when the walk gave up"
        );
    }

    #[test]
    fn every_command_is_announced_exactly_once_and_never_as_not_run() {
        // the contract Sink promises: by the time you see a command it has
        // finished, failed to start, or been skipped
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        let names = [
            "serial-ok",
            "batched-a",
            "batched-b",
            "batched-c",
            "stopper",
            "sibling",
            "unreached",
        ];

        PipelineBuilder::new()
            .step(Step::serial([sh("serial-ok", "exit 0")]))
            .step(Step::batched(
                2,
                [
                    sh("batched-a", "exit 0"),
                    sh("batched-b", "exit 0"),
                    sh("batched-c", "exit 0"),
                ],
            ))
            .step(
                Step::batched(1, [sh("stopper", "exit 1"), sh("sibling", "sleep 5")])
                    .on_error(OnError::Skip),
            )
            .step(Step::serial([sh("unreached", "exit 0")]))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .expect("skip should not fail the run");

        let log = log.lock().unwrap();
        for name in names {
            let seen = log.of(name);
            assert_eq!(seen.len(), 1, "{name} was announced {} times", seen.len());
            assert!(
                !matches!(seen[0], Status::NotRun),
                "{name} was announced as NotRun"
            );
        }
        assert_eq!(
            log.records.len(),
            names.len(),
            "something extra was announced"
        );
    }

    #[test]
    fn a_command_the_batch_gave_up_on_before_starting_it_is_announced_once() {
        // the awkward case for the contract: a worker takes a command, parks
        // waiting for cores that the hog is holding, and is woken by the cancel.
        // it comes back never having run, and both the batch and skip_rest have
        // an opinion about saying so
        let whole_machine = Cores::read().len();
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        PipelineBuilder::new()
            .step(
                Step::batched(
                    3,
                    [
                        // slow enough that the other two are placed first
                        sh("boom", "sleep 0.3; exit 1"),
                        sh("hog", "sleep 5").cores(whole_machine),
                        sh("waiter", "sleep 5").cores(1),
                    ],
                )
                .on_error(OnError::Skip),
            )
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .expect("skip should not fail the run");

        let log = log.lock().unwrap();
        let seen = log.of("waiter");
        assert_eq!(seen.len(), 1, "announced {} times: {seen:?}", seen.len());
        assert!(!matches!(seen[0], Status::NotRun), "announced as NotRun");
    }

    #[test]
    fn a_batch_reports_every_command_it_was_given() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        PipelineBuilder::new()
            .step(Step::batched(
                3,
                (0..6).map(|i| sh(&format!("job-{i}"), "exit 0")),
            ))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .unwrap();

        let log = log.lock().unwrap();
        assert_eq!(log.records.len(), 6);
        for i in 0..6 {
            assert!(matches!(
                log.of(&format!("job-{i}"))[..],
                [Status::Finished(_)]
            ));
        }
    }

    #[test]
    fn a_closure_that_returns_an_error_ends_the_run_and_names_itself() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        let error = PipelineBuilder::new()
            .step(Step::from_closures([Closure::new("boom", || {
                Err("no such database".into())
            })]))
            .step(Step::serial([sh("never", "exit 0")]))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .unwrap_err()
            .to_string();

        assert!(error.contains("boom"), "{error}");
        // there is no stderr file to send anyone to, so the reason has to be in
        // the error itself
        assert!(error.contains("no such database"), "{error}");

        let log = log.lock().unwrap();
        match log.of("boom")[..] {
            [Status::Failed(why)] => assert!(why.contains("no such database"), "{why}"),
            ref other => panic!("expected one failure, got {other:?}"),
        }
        assert!(matches!(log.of("never")[..], [Status::Skipped]));
    }

    #[test]
    fn a_tolerated_closure_failure_lets_the_rest_of_the_run_happen() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        PipelineBuilder::new()
            .step(
                Step::from_closures([
                    Closure::new("bad", || Err("nope".into())),
                    Closure::new("after", || Ok(())),
                ])
                .on_error(OnError::Continue),
            )
            .step(Step::serial([sh("later", "exit 0")]))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .expect("a tolerated failure should not end the run");

        let log = log.lock().unwrap();
        assert!(matches!(log.of("bad")[..], [Status::Failed(_)]));
        assert!(matches!(log.of("after")[..], [Status::Finished(_)]));
        assert!(matches!(log.of("later")[..], [Status::Finished(_)]));
    }

    /// This one prints a panic backtrace notice while it runs. That is the point
    /// of it: the default hook still fires, and the run carries on regardless.
    #[test]
    fn a_panicking_closure_fails_only_itself() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        PipelineBuilder::new()
            .step(
                Step::from_closures([
                    Closure::new("panics", || panic!("boom {}", 7)),
                    Closure::new("after", || Ok(())),
                ])
                .on_error(OnError::Continue),
            )
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .expect("a caught panic is a failed closure, not a failed run");

        let log = log.lock().unwrap();
        match log.of("panics")[..] {
            [Status::Failed(why)] => {
                assert!(why.contains("panicked"), "{why}");
                assert!(why.contains("boom 7"), "{why}");
            }
            ref other => panic!("expected one caught panic, got {other:?}"),
        }
        assert!(matches!(log.of("after")[..], [Status::Finished(_)]));
    }

    #[test]
    fn a_closure_reports_a_wall_clock_and_nothing_else() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        PipelineBuilder::new()
            .step(Closure::new("sleeps", || {
                std::thread::sleep(std::time::Duration::from_millis(30));
                Ok(())
            }))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .unwrap();

        let log = log.lock().unwrap();
        match log.of("sleeps")[..] {
            [Status::Finished(t)] => {
                assert!(t.wall_s >= 0.03, "wall clock too short: {t:?}");
                // the whole contract: a thread has no wait4 to ask for these
                assert_eq!(t.user_s, None, "{t:?}");
                assert_eq!(t.sys_s, None, "{t:?}");
                assert_eq!(t.max_rss_kb, None, "{t:?}");
                assert_eq!(t.exit, 0, "{t:?}");
            }
            ref other => panic!("expected one finished closure, got {other:?}"),
        }
    }

    #[test]
    fn a_closure_can_move_values_in_and_hand_one_back() {
        let (tx, rx) = mpsc::channel();
        let owned = vec![1u8, 2, 3];
        // not Sync, so this would not compile against a shared `Fn` closure
        let counter = std::cell::RefCell::new(0usize);

        PipelineBuilder::new()
            .step(Closure::new("hands-back", move || {
                *counter.borrow_mut() += owned.len();
                tx.send((owned, *counter.borrow())).ok();
                Ok(())
            }))
            .no_stderr()
            .build()
            .unwrap()
            .run()
            .unwrap();

        assert_eq!(rx.recv().unwrap(), (vec![1, 2, 3], 3));
    }

    #[test]
    fn a_closure_can_borrow_a_local_instead_of_moving_it() {
        let greeting = String::from("borrowed");
        let items: Vec<u8> = vec![1, 2, 3];
        let (tx, rx) = mpsc::channel();

        // no move, no clone, no Arc: that this compiles is the assertion
        PipelineBuilder::new()
            .step(Closure::new("borrows", || {
                tx.send(format!("{greeting}: {}", items.len())).ok();
                Ok(())
            }))
            .no_stderr()
            .build()
            .unwrap()
            .run()
            .unwrap();

        assert_eq!(rx.recv().unwrap(), "borrowed: 3");
    }

    #[test]
    fn a_closure_that_never_ran_is_announced_once_as_skipped() {
        let recorder = Recorder::default();
        let log = Arc::clone(&recorder.log);

        PipelineBuilder::new()
            // the first stops its own step, so its sibling never runs
            .step(Step::from_closures([
                Closure::new("bad", || Err("nope".into())),
                Closure::new("sibling", || Ok(())),
            ]))
            // and this whole step is never reached
            .step(Step::from_closures([Closure::new("unreached", || Ok(()))]))
            .no_stderr()
            .sink(recorder)
            .build()
            .unwrap()
            .run()
            .unwrap_err();

        let log = log.lock().unwrap();
        // exactly once each, and never as NotRun, which is what a sink is promised
        assert!(matches!(log.of("sibling")[..], [Status::Skipped]));
        assert!(matches!(log.of("unreached")[..], [Status::Skipped]));
        assert_eq!(log.records.len(), 3, "{:?}", log.records);
    }
}

/// Every sink registered on a pipeline, so the rest of this file can talk to
/// them as if there were one.
#[derive(Default)]
struct Sinks(Vec<Box<dyn Sink>>);

impl Sinks {
    /// Tell every sink in turn, stopping at the first one that fails.
    fn each(
        &mut self,
        mut tell: impl FnMut(&mut dyn Sink) -> Result<(), BoxError>,
    ) -> Result<(), Error> {
        self.0
            .iter_mut()
            .try_for_each(|sink| tell(sink.as_mut()))
            .map_err(Error::Sink)
    }
}
