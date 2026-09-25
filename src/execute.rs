//! Running commands and finding out what they cost.

use std::fs::{File, OpenOptions};
use std::io;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::Scope;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::closure::Closure;
use crate::cmd::{Cmd, Memory, Output};
use crate::cpu::{Cores, Lease};

/// How long anything gets to leave on a SIGTERM before it gets a SIGKILL.
const SIGTERM_GRACE: Duration = Duration::from_secs(5);

/// `MPOL_PREFERRED` from `linux/mempolicy.h`, which libc carries no constant
/// for.
#[cfg(target_os = "linux")]
const MPOL_PREFERRED: libc::c_int = 1;

/// `MPOL_BIND`, from the same header.
#[cfg(target_os = "linux")]
const MPOL_BIND: libc::c_int = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    pub wall_s: f64,
    /// Absent for a closure: a thread has no `wait4` to ask for these.
    pub user_s: Option<f64>,
    pub sys_s: Option<f64>,
    pub max_rss_kb: Option<i64>,
    pub exit: i32,
}

impl Timing {
    pub fn ok(&self) -> bool {
        self.exit == 0
    }
}

#[derive(Clone, Debug)]
pub enum Status {
    NotRun,
    Skipped,
    Failed(String),
    Finished(Timing),
    TimedOut(Timing),
}

impl Status {
    pub fn failed(&self) -> bool {
        match self {
            Status::NotRun | Status::Skipped => false,
            Status::Failed(_) | Status::TimedOut(_) => true,
            Status::Finished(t) => !t.ok(),
        }
    }

    /// What this cost, for the two states that got far enough to have an
    /// answer. A command killed on its deadline still burned everything it says
    /// it burned, so it counts.
    pub fn timing(&self) -> Option<&Timing> {
        match self {
            Status::Finished(t) | Status::TimedOut(t) => Some(t),
            _ => None,
        }
    }
}

impl Cores {
    /// Run one command. Nothing can cut it short but its own timeout.
    pub(crate) fn execute(&self, cmd: &mut Cmd) {
        // held until the command is finished with, however it finishes; the cpus
        // go back on the way out of scope
        let lease = self.acquire(cmd.cores.unwrap_or(0), &|| false);
        settle(cmd, &lease, self);

        let status = match cmd.spawn() {
            Err(e) => Status::Failed(e),
            Ok((pid, start)) => wait(pid, start, cmd.timeout, || {}),
        };
        cmd.report(status);
    }
}

impl Closure<'_> {
    /// Run it and time it.
    ///
    /// The wall clock is all there is. A panic is caught here, so one bad
    /// closure costs one step's worth rather than the run — the default hook
    /// still prints its backtrace on the way past, and `panic = "abort"` turns
    /// catching off entirely.
    pub(crate) fn execute(&mut self) {
        // a closure that has already run has nothing left to do, and its status
        // already says how it went
        let Some(f) = self.f.take() else { return };

        let start = Instant::now();
        let out = catch_unwind(AssertUnwindSafe(f));
        let timing = Timing {
            wall_s: start.elapsed().as_secs_f64(),
            user_s: None,
            sys_s: None,
            max_rss_kb: None,
            // nothing exited, but `ok` reads this to say it went fine
            exit: 0,
        };

        self.status = match out {
            Ok(Ok(())) => Status::Finished(timing),
            Ok(Err(e)) => Status::Failed(crate::error::chain(&*e)),
            Err(p) => Status::Failed(format!("panicked: {}", panic_msg(&*p))),
        };
    }
}

/// What a panic said, for the two payload types `panic!` actually produces.
fn panic_msg(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

/// One batch step: whether it has been cancelled, and the pids it has running.
///
/// Both are made fresh for the step, so cancelling reaches that step's commands
/// and nothing else, and leaves nothing behind for the next step.
pub(crate) struct Batch<'a> {
    cores: &'a Cores,
    cancelled: AtomicBool,
    /// A cancel has to reach every process at once, and this is the only place
    /// their pids are collected.
    running: Mutex<Vec<libc::pid_t>>,
    emptied: Condvar,
}

impl<'a> Batch<'a> {
    pub(crate) fn new(cores: &'a Cores) -> Batch<'a> {
        Batch {
            cores,
            cancelled: AtomicBool::new(false),
            running: Mutex::new(Vec::new()),
            emptied: Condvar::new(),
        }
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Run one command as part of the batch, which may be cancelled under it.
    ///
    /// `started` runs once the command has its cores and is about to be
    /// spawned, so anything timing it from there measures the same stretch the
    /// command itself reports. A batch can wait a long time for cores, and
    /// counting that would put the two out by however long the wait was.
    pub(crate) fn execute(&self, cmd: &mut Cmd, started: impl FnOnce()) {
        let lease = self
            .cores
            .acquire(cmd.cores.unwrap_or(0), &|| self.cancelled());

        // an empty lease means one of two things: the command asked for no
        // pinning, or the wait was cut short by a cancel. only the second is a
        // reason not to run it — and leaving the status alone reports it as
        // skipped, like the commands no worker reached
        if self.cancelled() {
            return;
        }
        settle(cmd, &lease, self.cores);
        started();

        let status = match cmd.spawn() {
            Err(e) => Status::Failed(e),
            Ok((pid, start)) => {
                self.add(pid);
                wait(pid, start, cmd.timeout, || self.remove(pid))
            }
        };
        cmd.report(status);
    }

    /// Nothing new starts, and everything running is terminated. Calling this
    /// more than once does nothing extra, which a batch relies on: it cancels
    /// once per result after the first failure.
    pub(crate) fn cancel<'s>(&'s self, scope: &'s Scope<'s, '_>) {
        if self.cancelled.swap(true, Ordering::Relaxed) {
            return;
        }

        // the flag is set before the lock, so a command registering right now
        // either appears in this list or sees the flag itself
        let running = self.running.lock().unwrap();
        for pid in running.iter() {
            signal(*pid, libc::SIGTERM);
        }
        drop(running);

        // a worker asleep waiting for cores would never see the flag on its own,
        // and the cores it is waiting for may never come back
        self.cores.wake();

        scope.spawn(|| self.kill_remaining());
    }

    /// Whatever ignores the SIGTERM gets a SIGKILL once the grace is up. Only
    /// pids still listed, and a listed pid has not been reaped, so this cannot
    /// reach a process that stopped being ours.
    fn kill_remaining(&self) {
        let running = self.running.lock().unwrap();
        let (running, grace) = self
            .emptied
            .wait_timeout_while(running, SIGTERM_GRACE, |running| !running.is_empty())
            .unwrap();

        if grace.timed_out() {
            for pid in running.iter() {
                signal(*pid, libc::SIGKILL);
            }
        }
    }

    fn add(&self, pid: libc::pid_t) {
        let mut running = self.running.lock().unwrap();
        running.push(pid);
        // one that started while the batch was being cancelled still has to go
        if self.cancelled() {
            signal(pid, libc::SIGTERM);
        }
    }

    fn remove(&self, pid: libc::pid_t) {
        let mut running = self.running.lock().unwrap();
        running.retain(|p| *p != pid);
        if running.is_empty() {
            self.emptied.notify_all();
        }
    }
}

/// Write where a command landed onto it, and the memory policy that follows
/// from that. One with no lease of its own, in a pool, gets the whole pool.
fn settle(cmd: &mut Cmd, lease: &Option<Lease>, cores: &Cores) {
    let (cpus, nodes) = match (lease, cores.whole()) {
        (Some(lease), _) => (lease.cpus().to_vec(), lease.nodes().to_vec()),
        (None, Some(whole)) if cmd.cores.unwrap_or(0) == 0 => {
            cmd.pooled = true;
            whole
        }
        (None, _) => (Vec::new(), Vec::new()),
    };
    cmd.cpus = cpus;
    cmd.nodes = nodes;
    cmd.policy = policy(cmd.memory.unwrap_or_default(), &cmd.nodes, &cores.mems);
}

/// The memory policy a placed command runs under.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Policy {
    /// Nothing asked of the kernel, which leaves each page on the node of the
    /// thread that first touched it.
    #[default]
    Default,
    Preferred(usize),
    Bound(Vec<usize>),

    /// A preference left unset because its node is outside the process's
    /// allowed mems, and why.
    Dropped(String),

    /// A bind refused for the same reason. The command is not run.
    Refused(String),
}

impl Policy {
    /// The policy the command runs under: `prefer:1`, `bind:0-1` or `default`.
    /// `None` for a refused bind, which never runs.
    pub(crate) fn label(&self) -> Option<String> {
        match self {
            Policy::Preferred(node) => Some(format!("prefer:{node}")),
            Policy::Bound(nodes) => Some(format!("bind:{}", crate::cpu::list(nodes))),
            Policy::Default | Policy::Dropped(_) => Some("default".into()),
            Policy::Refused(_) => None,
        }
    }

    /// How the table and `dry_run` show it: the label, with the reason beside
    /// a dropped preference or refused bind.
    pub(crate) fn describe(&self) -> String {
        match (self, self.label()) {
            (Policy::Dropped(why), Some(label)) => format!("{label} ({why})"),
            (Policy::Refused(why), _) => format!("refused ({why})"),
            (_, label) => label.unwrap_or_default(),
        }
    }
}

/// The memory policy for a command placed on `nodes`, given the nodes the
/// process may allocate from. An empty `mems` is one that could not be read,
/// and allows every node.
///
/// A machine with one node hands out no nodes at all, first touch is the
/// kernel's own behaviour and needs no call, and `MPOL_PREFERRED` names a
/// single node, so a command preferring one after it had to take cores off two
/// has no way to say which.
pub(crate) fn policy(memory: Memory, nodes: &[usize], mems: &[usize]) -> Policy {
    let allowed = |node: &usize| mems.is_empty() || mems.contains(node);

    match (memory, nodes) {
        (_, []) | (Memory::FirstTouch, _) => Policy::Default,
        (Memory::Preferred, [node]) if allowed(node) => Policy::Preferred(*node),
        (Memory::Preferred, [node]) => Policy::Dropped(format!("node {node} not in Mems_allowed")),
        (Memory::Preferred, _) => Policy::Default,
        (Memory::Bound, nodes) => {
            let outside: Vec<usize> = nodes.iter().copied().filter(|n| !allowed(n)).collect();
            match outside.as_slice() {
                // bind takes a mask rather than one node, so a
                // command that had to span two is still held to
                // exactly those
                [] => Policy::Bound(nodes.to_vec()),
                outside => Policy::Refused(format!(
                    "cannot bind to node {}: not in Mems_allowed {}",
                    crate::cpu::list(outside),
                    crate::cpu::list(mems)
                )),
            }
        }
    }
}

/// The `set_mempolicy` call for `policy`: a mode, a node mask, and the
/// `maxnode` to pass with it.
#[cfg(target_os = "linux")]
fn call(policy: &Policy) -> Option<(libc::c_int, Vec<libc::c_ulong>, usize)> {
    let (mode, nodes) = match policy {
        Policy::Preferred(node) => (MPOL_PREFERRED, std::slice::from_ref(node)),
        Policy::Bound(nodes) => (MPOL_BIND, nodes.as_slice()),
        Policy::Default | Policy::Dropped(_) | Policy::Refused(_) => return None,
    };

    let (mask, maxnode) = crate::cpu::nodemask(nodes);
    Some((mode, mask, maxnode))
}

/// An io error with what was being done when it happened, the way a command's
/// failure is reported: `failed to spawn /bin/x: No such file or directory`.
fn because(what: impl std::fmt::Display) -> impl FnOnce(io::Error) -> String {
    move |e| format!("{what}: {e}")
}

fn signal(pid: libc::pid_t, sig: libc::c_int) {
    unsafe { libc::kill(pid, sig) };
}

pub(crate) fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let now = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let mut buf = [0u8; 32];

    let written = unsafe {
        if libc::localtime_r(&now, &mut tm).is_null() {
            return secs.to_string();
        }
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            c"%Y%m%d-%H%M%S".as_ptr(),
            &tm,
        )
    };
    String::from_utf8_lossy(&buf[..written]).into_owned()
}

impl Cmd {
    /// Start the process, giving back its pid and the moment it started.
    fn spawn(&self) -> Result<(libc::pid_t, Instant), String> {
        let mut proc = Command::new(&self.program);
        proc.args(self.args());
        proc.envs(&self.env);
        if let Some(dir) = &self.dir {
            proc.current_dir(dir);
        }
        proc.stdout(self.stdout.stdio()?);
        proc.stderr(self.stderr.stdio()?);

        // the child pins itself on its way to exec, so the pid we end up
        // waiting on is the program's own and every number we measure is still
        // the program's. the masks are made out here because the hook runs
        // between the fork and the exec, where anything that allocates or takes
        // a lock can hang for good. affinity and memory policy both survive an
        // exec, so setting them now is enough.
        //
        // note: no pinning syscall exists on macOS, so cpus are still leased
        // and counted there but the child is never actually bound to one.
        #[cfg(target_os = "linux")]
        if !self.cpus.is_empty() {
            let set = crate::cpu::mask(&self.cpus);

            if let Policy::Refused(why) = &self.policy {
                return Err(why.clone());
            }
            let policy = call(&self.policy);

            unsafe {
                proc.pre_exec(move || {
                    if libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }

                    if let Some((mode, mask, bits)) = &policy {
                        // glibc wraps neither of the mempolicy
                        // calls, so this is the raw syscall
                        let rc =
                            libc::syscall(libc::SYS_set_mempolicy, *mode, mask.as_ptr(), *bits);

                        // a preference is advice, so one the kernel
                        // turns down still runs. nothing can report it
                        // from here, which is why the parent checks
                        // the allowed mems before the fork
                        if rc != 0 && *mode == MPOL_BIND {
                            return Err(std::io::Error::last_os_error());
                        }
                    }

                    Ok(())
                });
            }
        }

        let start = Instant::now();
        let child = proc.spawn().map_err(because(format!(
            "failed to spawn {}",
            self.program.display()
        )))?;

        // the handle is dropped here and the pid outlives it. std's Child has
        // no Drop, so nothing waits on the process and nothing frees the
        // number — which is what leaves it there for reap to find. giving this
        // function something that reaps on drop would hand reap a pid the
        // kernel is free to have reused.
        Ok((child.id() as libc::pid_t, start))
    }

    /// Record how the command went, and drop a stderr file that was only being
    /// kept in case of failure and has nothing in it.
    fn report(&mut self, status: Status) {
        if let Output::OnFailure(path) = &self.stderr {
            let keep = status.failed() && std::fs::metadata(path).is_ok_and(|m| m.len() > 0);
            if !keep {
                std::fs::remove_file(path).ok();
            }
        }
        self.status = status;
    }
}

impl Output {
    fn stdio(&self) -> Result<Stdio, String> {
        let make_dir = |path: &Path| -> Result<(), String> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(because(format!("failed to create {}", dir.display())))?;
            }
            Ok(())
        };

        Ok(match self {
            Output::Null => Stdio::null(),
            Output::Inherit => Stdio::inherit(),
            Output::File(path) | Output::OnFailure(path) => {
                make_dir(path)?;
                let file = File::create(path)
                    .map_err(because(format!("failed to create {}", path.display())))?;
                Stdio::from(file)
            }
            Output::Append(path) => {
                make_dir(path)?;
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(because(format!(
                        "failed to open {} for append",
                        path.display()
                    )))?;
                Stdio::from(file)
            }
        })
    }
}

/// A process with a deadline on it.
///
/// Only made for a command that asked for a timeout — one that did not is waited
/// on directly and never touches a lock. `finished` is set while the process is
/// over but not yet reaped, which is the window in which the waiting thread can
/// be told to leave the pid alone.
struct Deadline {
    pid: libc::pid_t,
    finished: Mutex<bool>,
    changed: Condvar,
}

impl Deadline {
    fn new(pid: libc::pid_t) -> Deadline {
        Deadline {
            pid,
            finished: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    /// Wait `after` for the process, then terminate it. True if it came to that.
    fn kill_after(&self, after: Duration) -> bool {
        if self.wait_for_finish(after) {
            return false;
        }
        signal(self.pid, libc::SIGTERM);
        if !self.wait_for_finish(SIGTERM_GRACE) {
            signal(self.pid, libc::SIGKILL);
        }
        true
    }

    /// Wait up to `limit` for the process to be over. True if it is.
    fn wait_for_finish(&self, limit: Duration) -> bool {
        let finished = self.finished.lock().unwrap();
        let (finished, _) = self
            .changed
            .wait_timeout_while(finished, limit, |finished| !*finished)
            .unwrap();
        *finished
    }

    fn set_finished(&self) {
        *self.finished.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

/// Wait for the process, terminating it if it runs longer than `limit`. The flag
/// says whether it came to that.
///
/// `exited` is handed straight to [`reap`]. A command with no limit needs no
/// second thread and no lock, which is the common case.
fn wait(
    pid: libc::pid_t,
    start: Instant,
    limit: Option<Duration>,
    exited: impl FnOnce(),
) -> Status {
    let Some(limit) = limit else {
        return reap(pid, start, exited).map_or_else(Status::Failed, Status::Finished);
    };

    let deadline = Deadline::new(pid);
    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| deadline.kill_after(limit));
        let timing = reap(pid, start, || {
            deadline.set_finished();
            exited();
        });
        let killed = waiting.join().unwrap_or(false);
        match (timing, killed) {
            (Ok(timing), true) => Status::TimedOut(timing),
            (Ok(timing), false) => Status::Finished(timing),
            (Err(e), _) => Status::Failed(e),
        }
    })
}

/// Block until the process is over, then collect what it cost.
///
/// Two waits rather than one. `waitid` says it is over but leaves the pid held,
/// and only `wait4` hands it back — `exited` runs in the gap between them, which
/// is the one moment anything else holding this pid can be told to stop
/// signalling it. It runs whether or not the first wait worked, so a caller
/// never has to arrange it a second time: a wait that failed leaves nothing more
/// to be done with this pid either.
fn reap(pid: libc::pid_t, start: Instant, exited: impl FnOnce()) -> Result<Timing, String> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    let waited = (rc == 0).then_some(()).ok_or_else(io::Error::last_os_error);

    // taken here rather than after the reap, so it is when the process ended
    let wall_s = start.elapsed().as_secs_f64();

    exited();
    waited.map_err(because("waitid failed"))?;

    let mut status: libc::c_int = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // std never reaps a child on its own, so nothing in it races with this
    let rc = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
    if rc < 0 {
        return Err(because("wait4 failed")(io::Error::last_os_error()));
    }

    let exit = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        -1
    };

    Ok(Timing {
        wall_s,
        user_s: Some(secs(usage.ru_utime)),
        sys_s: Some(secs(usage.ru_stime)),
        max_rss_kb: Some(max_rss_kb(&usage)),
        exit,
    })
}

/// `ru_maxrss` in kilobytes. The kernel hands it back in kilobytes on Linux
/// and bytes on macOS, so only macOS needs the conversion.
#[cfg(target_os = "linux")]
fn max_rss_kb(usage: &libc::rusage) -> i64 {
    usage.ru_maxrss
}

#[cfg(not(target_os = "linux"))]
fn max_rss_kb(usage: &libc::rusage) -> i64 {
    usage.ru_maxrss / 1024
}

fn secs(tv: libc::timeval) -> f64 {
    tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_with_no_cores_in_a_pool_shares_all_of_it() {
        let pool = Cores::with_layout(&[(0, 0), (1, 1), (2, 0), (3, 1)]).carve(2);

        let mut shared = Cmd::new("/a");
        settle(&mut shared, &None, &pool);
        assert!(shared.pooled);
        assert_eq!(
            (shared.cpus.as_slice(), shared.nodes.as_slice()),
            (&[0, 2][..], &[0][..])
        );
        assert_eq!(shared.policy, Policy::Preferred(0));

        // one asking for cores leases them out of the pool instead
        let mut own = Cmd::new("/b").cores(1);
        let lease = pool.acquire(1, &|| false);
        settle(&mut own, &lease, &pool);
        assert!(!own.pooled);
        assert_eq!(own.cpus, [0]);
    }

    #[test]
    fn outside_a_pool_a_command_with_no_cores_is_not_pinned() {
        let machine = Cores::with_layout(&[(0, 0), (1, 1)]);
        let mut cmd = Cmd::new("/a");
        settle(&mut cmd, &None, &machine);
        assert!(!cmd.pooled && cmd.cpus.is_empty());
    }

    #[test]
    fn a_machine_with_one_node_asks_for_no_policy() {
        // an empty node list is what a single node pool hands out
        for memory in [Memory::Preferred, Memory::Bound, Memory::FirstTouch] {
            assert_eq!(policy(memory, &[], &[]), Policy::Default, "{memory:?}");
        }
    }

    #[test]
    fn first_touch_asks_for_no_policy_wherever_it_landed() {
        assert_eq!(policy(Memory::FirstTouch, &[1], &[]), Policy::Default);
        assert_eq!(policy(Memory::FirstTouch, &[0, 1], &[]), Policy::Default);
    }

    #[test]
    fn preferred_names_the_one_node_it_can_name() {
        assert_eq!(
            policy(Memory::Preferred, &[1], &[0, 1]),
            Policy::Preferred(1)
        );
        // MPOL_PREFERRED has no way to say "either of these two",
        // so a command that had to span says nothing at all
        assert_eq!(policy(Memory::Preferred, &[0, 1], &[0, 1]), Policy::Default);
    }

    #[test]
    fn bound_holds_a_command_to_every_node_it_took_cores_from() {
        assert_eq!(policy(Memory::Bound, &[1], &[0, 1]), Policy::Bound(vec![1]));
        assert_eq!(
            policy(Memory::Bound, &[0, 1], &[0, 1]),
            Policy::Bound(vec![0, 1])
        );
    }

    #[test]
    fn a_node_outside_the_allowed_mems_drops_a_preference_and_refuses_a_bind() {
        assert_eq!(
            policy(Memory::Preferred, &[1], &[0]),
            Policy::Dropped("node 1 not in Mems_allowed".into())
        );
        assert_eq!(
            policy(Memory::Bound, &[0, 1], &[0]),
            Policy::Refused("cannot bind to node 1: not in Mems_allowed 0".into())
        );
    }

    #[test]
    fn unreadable_mems_allow_every_node() {
        assert_eq!(policy(Memory::Preferred, &[1], &[]), Policy::Preferred(1));
        assert_eq!(policy(Memory::Bound, &[1], &[]), Policy::Bound(vec![1]));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_policy_becomes_a_mode_a_mask_and_a_maxnode() {
        assert_eq!(
            call(&Policy::Preferred(1)),
            Some((MPOL_PREFERRED, vec![0b10], 3))
        );
        assert_eq!(
            call(&Policy::Bound(vec![0, 1])),
            Some((MPOL_BIND, vec![0b11], 3))
        );
        assert_eq!(call(&Policy::Dropped("x".into())), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_kernel_reads_every_node_in_the_mask() {
        // memory policy is per thread and each test has its own,
        // so binding this one leaks into nothing else. node 0
        // exists on every numa kernel
        let (mask, maxnode) = crate::cpu::nodemask(&[0]);
        let rc =
            unsafe { libc::syscall(libc::SYS_set_mempolicy, MPOL_BIND, mask.as_ptr(), maxnode) };
        let err = std::io::Error::last_os_error();
        if rc != 0 && err.raw_os_error() == Some(libc::ENOSYS) {
            return;
        }

        // room for 1024 nodes, since get_mempolicy refuses a mask
        // narrower than the machine's node count
        let mut mode: libc::c_int = -1;
        let mut got = [0 as libc::c_ulong; 1024 / libc::c_ulong::BITS as usize];
        unsafe {
            libc::syscall(
                libc::SYS_get_mempolicy,
                &mut mode,
                got.as_mut_ptr(),
                1024usize,
                std::ptr::null::<libc::c_void>(),
                0usize,
            );
            libc::syscall(
                libc::SYS_set_mempolicy,
                0,
                std::ptr::null::<libc::c_ulong>(),
                0usize,
            );
        }

        assert_eq!(rc, 0, "set_mempolicy: {err}");
        assert_eq!((mode, got[0]), (MPOL_BIND, 0b1));
    }
    use std::sync::atomic::AtomicUsize;

    fn sh(script: &str) -> Cmd {
        Cmd::new("/bin/sh").arg("-c", script)
    }

    fn start(script: &str) -> (libc::pid_t, Instant) {
        sh(script).spawn().expect("spawn")
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pipeline-test-{name}"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reap_reports_the_exit_code() {
        let (pid, at) = start("exit 3");
        let timing = reap(pid, at, || {}).expect("reap");
        assert_eq!(timing.exit, 3);
        assert!(!timing.ok());
    }

    #[test]
    fn reap_turns_a_signal_into_128_plus_it() {
        let (pid, at) = start("kill -TERM $$");
        let timing = reap(pid, at, || {}).expect("reap");
        assert_eq!(timing.exit, 128 + libc::SIGTERM);
    }

    #[test]
    fn reap_collects_what_the_process_actually_cost() {
        let (pid, at) = start("seq 1 400000 > /dev/null");
        let timing = reap(pid, at, || {}).expect("reap");

        assert!(
            timing.user_s.is_some_and(|s| s > 0.0),
            "no user time: {timing:?}"
        );
        assert!(
            timing.max_rss_kb.is_some_and(|k| k > 0),
            "no peak rss: {timing:?}"
        );
        assert!(timing.wall_s > 0.0, "no wall clock: {timing:?}");
    }

    #[test]
    fn the_pid_is_still_ours_when_exited_runs_and_gone_after() {
        // this is the whole reason for waitid(WNOWAIT): anything holding the pid
        // has to be told to let go while it is still a zombie, because once the
        // reap lands it could belong to someone else
        let (pid, at) = start("exit 0");

        let mut ours_in_the_gap = None;
        reap(pid, at, || {
            ours_in_the_gap = Some(unsafe { libc::kill(pid, 0) });
        })
        .expect("reap");

        assert_eq!(ours_in_the_gap, Some(0), "pid should still be ours");
        assert!(
            reap(pid, at, || {}).is_err(),
            "a second reap should find nothing, so the first one really reaped"
        );
    }

    #[test]
    fn exited_runs_even_when_the_wait_fails() {
        // nothing is ever told twice, so the one call has to happen on both paths
        let ran = AtomicUsize::new(0);
        let bogus = -424242;

        let result = reap(bogus, Instant::now(), || {
            ran.fetch_add(1, Ordering::Relaxed);
        });

        assert!(result.is_err(), "waiting on a pid we never spawned");
        assert_eq!(ran.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_command_that_beats_its_deadline_does_not_wait_for_it() {
        // if the thread minding the deadline is not woken, this takes the full
        // minute instead of no time at all
        let cores = Cores::with_pool(vec![]);
        let mut cmd = sh("exit 0").timeout(Duration::from_secs(60));

        let at = Instant::now();
        cores.execute(&mut cmd);

        assert!(
            matches!(cmd.status(), Status::Finished(_)),
            "{:?}",
            cmd.status()
        );
        assert!(
            at.elapsed() < Duration::from_secs(5),
            "took {:?}, so the deadline was slept out",
            at.elapsed()
        );
    }

    #[test]
    fn a_command_that_outstays_its_deadline_is_terminated() {
        let cores = Cores::with_pool(vec![]);
        let mut cmd = sh("sleep 30").timeout(Duration::from_millis(200));

        let at = Instant::now();
        cores.execute(&mut cmd);

        let Status::TimedOut(timing) = cmd.status() else {
            panic!("expected a timeout, got {:?}", cmd.status());
        };
        assert_eq!(timing.exit, 128 + libc::SIGTERM);
        assert!(at.elapsed() < Duration::from_secs(5), "{:?}", at.elapsed());
    }

    #[test]
    fn a_command_that_ignores_the_deadline_is_killed_once_the_grace_is_up() {
        // the slow one: SIGTERM_GRACE has to elapse before the SIGKILL lands
        let cores = Cores::with_pool(vec![]);
        let mut cmd = sh("trap '' TERM; sleep 30").timeout(Duration::from_millis(200));

        let at = Instant::now();
        cores.execute(&mut cmd);

        let Status::TimedOut(timing) = cmd.status() else {
            panic!("expected a timeout, got {:?}", cmd.status());
        };
        assert_eq!(timing.exit, 128 + libc::SIGKILL, "should have been killed");
        assert!(
            at.elapsed() >= SIGTERM_GRACE,
            "killed before the grace was up: {:?}",
            at.elapsed()
        );
    }

    #[test]
    fn a_command_that_cannot_start_says_so_rather_than_panicking() {
        let cores = Cores::with_pool(vec![]);
        let mut cmd = Cmd::new("/no/such/tool");
        cores.execute(&mut cmd);

        let Status::Failed(why) = cmd.status() else {
            panic!("expected a failure to launch, got {:?}", cmd.status());
        };
        assert!(why.contains("/no/such/tool"), "unhelpful message: {why}");
    }

    fn exited(exit: i32) -> Status {
        Status::Finished(Timing {
            wall_s: 0.0,
            user_s: Some(0.0),
            sys_s: Some(0.0),
            max_rss_kb: Some(0),
            exit,
        })
    }

    #[test]
    fn a_failure_with_something_to_say_keeps_its_stderr() {
        let path = scratch("keep").join("e.stderr");
        std::fs::write(&path, b"no such database").unwrap();

        let mut cmd = Cmd::new("/x").stderr(Output::OnFailure(path.clone()));
        cmd.report(exited(1));

        assert!(path.exists(), "a failure with output should be kept");
    }

    #[test]
    fn a_failure_with_nothing_to_say_leaves_no_file_behind() {
        let path = scratch("empty").join("e.stderr");
        std::fs::write(&path, b"").unwrap();

        let mut cmd = Cmd::new("/x").stderr(Output::OnFailure(path.clone()));
        cmd.report(Status::Failed("could not spawn".into()));

        assert!(!path.exists(), "an empty log is not worth keeping");
    }

    #[test]
    fn a_command_that_worked_leaves_no_file_behind() {
        let path = scratch("ok").join("e.stderr");
        std::fs::write(&path, b"a warning, perhaps").unwrap();

        let mut cmd = Cmd::new("/x").stderr(Output::OnFailure(path.clone()));
        cmd.report(exited(0));

        assert!(
            !path.exists(),
            "nothing failed, so there is nothing to read"
        );
    }

    #[test]
    fn a_stderr_the_caller_asked_for_is_kept_whatever_happened() {
        let path = scratch("asked").join("e.stderr");
        std::fs::write(&path, b"").unwrap();

        let mut cmd = Cmd::new("/x").stderr(Output::File(path.clone()));
        cmd.report(exited(0));

        assert!(path.exists(), "only OnFailure files are tidied away");
    }

    #[test]
    fn an_output_file_brings_its_directory_with_it() {
        let path = scratch("mkdir").join("a/b/c.txt");
        Output::File(path.clone()).stdio().expect("stdio");
        assert!(path.exists());
    }

    #[test]
    fn appending_adds_to_what_is_there_and_a_file_replaces_it() {
        let dir = scratch("append");
        let path = dir.join("out.txt");
        std::fs::write(&path, b"first\n").unwrap();

        drop(Output::Append(path.clone()).stdio().expect("stdio"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");

        drop(Output::File(path.clone()).stdio().expect("stdio"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }
}
