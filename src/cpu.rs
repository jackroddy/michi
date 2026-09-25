//! Which CPUs a command is allowed to run on.
//!
//! A command asks for a number of cores and never says which. [`Cores`] keeps
//! track of what is free, hands out that many, and takes them back when the
//! command is done. The pinning happens in the child itself, between the fork
//! and the exec.
//!
//! Which ones it hands out is not arbitrary. A command's cores come off one
//! memory node wherever enough of that node is free, so its threads and the
//! memory they touch stay on the same side of the interconnect.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex};

/// One cpu in the pool, and where it sits on the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cpu {
    id: usize,

    /// The memory node it belongs to.
    node: usize,
}

/// Which node a command's cores come off, when more than one could hold them.
///
/// A machine with more than one memory node has no default: a pipeline placing
/// commands on one has to choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The node with the fewest free cores that still fits, keeping the others
    /// whole for a wider request. Concurrent commands share a node's cache and
    /// memory bandwidth.
    Pack,

    /// The node with the most free cores, so concurrent commands land on
    /// separate nodes until there are more of them than nodes.
    Spread,
}

/// The cores a pipeline has to hand out, and which of them are in use.
///
/// The pool holds one logical CPU per physical core. Two logical CPUs on one
/// physical core are not two cores — they share the execution units, so a
/// command given both would get somewhere around 1.3 cores' worth of work done
/// while the table claimed it had 2. Only the first of each sibling group is
/// ever handed out and the rest sit idle, which is why a 32-CPU machine with
/// hyperthreading has 24 of these rather than 32.
#[derive(Debug, Default)]
pub(crate) struct Cores {
    pool: Vec<Cpu>,
    /// The cpus currently leased out, and something to wait on for one to come
    /// back. A command that cannot be placed yet sleeps here rather than
    /// spinning, which would burn a core to wait for a core.
    taken: Mutex<Vec<usize>>,
    freed: Condvar,

    /// Which node a request comes off when more than one could hold it. `None`
    /// is only ever left on a pool the pipeline has not asked to place across
    /// nodes, where it packs.
    pub(crate) placement: Option<Placement>,

    /// The memory nodes this process may allocate from, or empty where that
    /// could not be read.
    pub(crate) mems: Vec<usize>,
}

/// Cores held for as long as one command needs them.
///
/// Handing them back is [`Drop`]'s job rather than the caller's, so a command
/// that failed to spawn, timed out, or panicked releases its cores the same way
/// one that finished does.
pub(crate) struct Lease<'a> {
    cores: &'a Cores,
    cpus: Vec<usize>,

    /// The memory nodes those cpus sit on, or nothing at all on a machine with
    /// only one.
    //
    // empty is what keeps a single node machine out of
    // set_mempolicy and out of the table's node column:
    // naming its only node asks for the placement every
    // allocation already gets
    nodes: Vec<usize>,
}

impl Cores {
    pub(crate) fn read() -> Cores {
        let mut pool = Vec::new();
        let mut spoken_for = Vec::new();

        let nodes = nodes();

        for cpu in allowed() {
            if spoken_for.contains(&cpu) {
                continue;
            }
            spoken_for.extend(siblings(cpu));
            pool.push(Cpu {
                id: cpu,
                node: nodes.get(&cpu).copied().unwrap_or(0),
            });
        }

        Cores {
            pool,
            taken: Mutex::new(Vec::new()),
            freed: Condvar::new(),
            placement: None,
            mems: mems(),
        }
    }

    /// A pool of exactly these cpus, all on one node, so the handing-out can be
    /// exercised without depending on what the machine happens to have.
    #[cfg(test)]
    pub(crate) fn with_pool(pool: Vec<usize>) -> Cores {
        let pool = pool.into_iter().map(|id| (id, 0)).collect::<Vec<_>>();
        Cores::with_layout(&pool)
    }

    /// A pool laid out as `(cpu, node)`, for placing commands on a machine this
    /// one is not.
    #[cfg(test)]
    pub(crate) fn with_layout(layout: &[(usize, usize)]) -> Cores {
        Cores {
            pool: layout
                .iter()
                .map(|(id, node)| Cpu {
                    id: *id,
                    node: *node,
                })
                .collect(),
            taken: Mutex::new(Vec::new()),
            freed: Condvar::new(),
            placement: None,
            mems: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.pool.len()
    }

    /// Take `size` cores, waiting for them if they are not free yet.
    ///
    /// `None` for a command that asked for none, and for one that gave up
    /// waiting because the run is stopping — neither is going to be pinned.
    /// A request larger than the whole machine would wait forever, which is why
    /// the pipeline refuses to build one.
    pub(crate) fn acquire(&self, size: usize, abandon: &dyn Fn() -> bool) -> Option<Lease<'_>> {
        if size == 0 || size > self.pool.len() {
            return None;
        }

        let mut taken = self.taken.lock().unwrap();
        loop {
            if abandon() {
                return None;
            }
            if let Some(lease) = self.grab(&mut taken, size) {
                return Some(lease);
            }
            taken = self.freed.wait(taken).unwrap();
        }
    }

    /// Take `size` cores if they are free right now, and never wait. For
    /// working out what a run would look like without running it.
    pub(crate) fn try_acquire(&self, size: usize) -> Option<Lease<'_>> {
        if size == 0 || size > self.pool.len() {
            return None;
        }
        self.grab(&mut self.taken.lock().unwrap(), size)
    }

    fn grab(&self, taken: &mut Vec<usize>, size: usize) -> Option<Lease<'_>> {
        let free: Vec<Cpu> = self
            .pool
            .iter()
            .filter(|cpu| !taken.contains(&cpu.id))
            .copied()
            .collect();

        if free.len() < size {
            return None;
        }

        let placed = place(&free, size, self.placement.unwrap_or(Placement::Pack));

        // lowest first, so a run with the machine to itself
        // places its commands the same way every time and the
        // cpu column reads in order
        let mut cpus: Vec<usize> = placed.iter().map(|cpu| cpu.id).collect();
        cpus.sort_unstable();

        let mut nodes: Vec<usize> = match self.spans_nodes() {
            true => placed.iter().map(|cpu| cpu.node).collect(),
            false => Vec::new(),
        };
        nodes.sort_unstable();
        nodes.dedup();

        taken.extend(&cpus);
        Some(Lease {
            cores: self,
            cpus,
            nodes,
        })
    }

    /// Whether the pool covers more than one memory node.
    pub(crate) fn spans_nodes(&self) -> bool {
        let mut nodes = self.pool.iter().map(|cpu| cpu.node);
        let first = nodes.next();
        nodes.any(|node| Some(node) != first)
    }

    /// Wake anything waiting on cores that are never coming, so a stopping run
    /// does not leave workers parked.
    pub(crate) fn wake(&self) {
        self.freed.notify_all();
    }

    fn release(&self, cpus: &[usize]) {
        let mut taken = self.taken.lock().unwrap();
        taken.retain(|cpu| !cpus.contains(cpu));
        drop(taken);
        self.freed.notify_all();
    }
}

impl Lease<'_> {
    pub(crate) fn cpus(&self) -> &[usize] {
        &self.cpus
    }

    pub(crate) fn nodes(&self) -> &[usize] {
        &self.nodes
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.cores.release(&self.cpus);
    }
}

/// The `size` cpus to hand out, off one memory node wherever one of them has
/// that many free.
///
/// Cores on one node reach their memory without crossing the interconnect. That
/// is worth choosing but not worth queueing for, so this takes the best
/// arrangement free at the moment the request can be met, and never holds a
/// command back waiting for a better one.
fn place(free: &[Cpu], size: usize, placement: Placement) -> Vec<Cpu> {
    let mut nodes = nodewise(free);
    if placement == Placement::Spread {
        nodes.sort_by_key(|node| (std::cmp::Reverse(node.len()), node[0].id));
    }

    for node in nodes {
        if node.len() >= size {
            return node[..size].to_vec();
        }
    }

    // nothing holds the whole request, so spend the fullest
    // node first and cross as few as the size forces
    let mut nodes = nodewise(free);
    nodes.sort_by_key(|node| (std::cmp::Reverse(node.len()), node[0].id));

    let mut out: Vec<Cpu> = nodes.into_iter().flatten().collect();
    out.truncate(size);
    out
}

/// The free cpus grouped by node, smallest group first, and lowest node first
/// among groups of a size so the same request lands the same way twice.
fn nodewise(free: &[Cpu]) -> Vec<Vec<Cpu>> {
    let mut nodes: BTreeMap<usize, Vec<Cpu>> = BTreeMap::new();

    for cpu in free {
        nodes.entry(cpu.node).or_default().push(*cpu);
    }

    let mut out: Vec<Vec<Cpu>> = nodes.into_values().collect();
    out.sort_by_key(|node| (node.len(), node[0].id));
    out
}

/// Which memory node each cpu belongs to.
///
/// A kernel built without NUMA has no `node` directory to read, and every cpu
/// comes back missing from this and placed on node 0, which is the whole of the
/// truth on such a machine.
fn nodes() -> BTreeMap<usize, usize> {
    let mut out = BTreeMap::new();

    let Ok(dir) = std::fs::read_dir("/sys/devices/system/node") else {
        return out;
    };

    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(node) = name.to_str().and_then(|name| name.strip_prefix("node")) else {
            continue;
        };
        let Ok(node) = node.parse::<usize>() else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(entry.path().join("cpulist")) else {
            continue;
        };

        for cpu in parse_list(&text) {
            out.insert(cpu, node);
        }
    }

    out
}

/// The memory nodes this process may allocate from, as its cpuset allows them.
#[cfg(target_os = "linux")]
fn mems() -> Vec<usize> {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return Vec::new();
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("Mems_allowed_list:"))
        .map(parse_list)
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn mems() -> Vec<usize> {
    Vec::new()
}

/// The affinity mask for `cpus`, built while there is still a whole program to
/// build it in. What installs it runs after the fork, where about the only
/// thing left that is safe to do is make one syscall.
///
/// A cpu at or past `CPU_SETSIZE` would write outside the set, so it is dropped
/// rather than trusted. `allowed` cannot produce one, and this is what keeps
/// that true if a pool ever comes from somewhere else.
#[cfg(target_os = "linux")]
pub(crate) fn mask(cpus: &[usize]) -> libc::cpu_set_t {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for cpu in cpus.iter().filter(|cpu| **cpu < libc::CPU_SETSIZE as usize) {
        unsafe { libc::CPU_SET(*cpu, &mut set) };
    }
    set
}

/// The node mask for `nodes`, and the `maxnode` to pass `set_mempolicy` with it.
///
/// Built out here for the reason the cpu mask is: what installs it runs after
/// the fork.
#[cfg(target_os = "linux")]
pub(crate) fn nodemask(nodes: &[usize]) -> (Vec<libc::c_ulong>, usize) {
    let Some(highest) = nodes.iter().copied().max() else {
        return (Vec::new(), 0);
    };

    // **note: the kernel decrements maxnode before reading the
    //         mask (get_nodes in mm/mempolicy.c), so it reads
    //         maxnode - 1 bits. highest + 1 drops the top node's
    //         bit, and a lone node then reads as an empty mask:
    //         EINVAL for bind, a silent MPOL_LOCAL for preferred.
    //         libnuma passes one extra for the same reason
    let word = libc::c_ulong::BITS as usize;
    let read = highest + 1;
    let mut mask = vec![0 as libc::c_ulong; read.div_ceil(word)];

    for node in nodes {
        mask[node / word] |= 1 << (node % word);
    }

    (mask, read + 1)
}

/// A cpu list the way the kernel writes one: `0`, `0,2`, empty for nothing.
pub(crate) fn list(cpus: &[usize]) -> String {
    let parts: Vec<String> = cpus.iter().map(|cpu| cpu.to_string()).collect();
    parts.join(",")
}

/// How wide the list for `cores` cpus comes out if every one of them is a
/// single digit: one digit each, and a comma between them.
///
/// Room to reserve before anything has run and we know which cpus they are.
/// The low guess on purpose — a cpu past the first ten grows the column past
/// this, and nothing is reserved for a width most runs will not use.
pub(crate) fn list_width(cores: usize) -> usize {
    match cores {
        0 => 0,
        cores => 2 * cores - 1,
    }
}

/// The CPUs this process may run on, which is not the same as every CPU the
/// machine has — a benchmark started under `taskset` gets a smaller set, and
/// handing out anything outside it would pin commands nowhere.
#[cfg(target_os = "linux")]
fn allowed() -> Vec<usize> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) };
    if rc != 0 {
        return Vec::new();
    }

    (0..libc::CPU_SETSIZE as usize)
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
        .collect()
}

/// Every CPU the machine has. There is no `taskset` here to ask for a
/// smaller set, so this is the honest answer rather than an approximation.
#[cfg(not(target_os = "linux"))]
fn allowed() -> Vec<usize> {
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    (0..cpus).collect()
}

/// Every logical CPU sharing a physical core with this one, itself included.
/// A CPU whose topology we cannot read is treated as a core of its own, which
/// is the reading that under-promises.
fn siblings(cpu: usize) -> Vec<usize> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    match std::fs::read_to_string(path) {
        Ok(text) => parse_list(&text),
        Err(_) => vec![cpu],
    }
}

/// A kernel cpulist: `0-1`, `16`, `0-3,8-11`.
fn parse_list(text: &str) -> Vec<usize> {
    let mut out = Vec::new();

    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((lo, hi)) => {
                if let (Ok(lo), Ok(hi)) = (lo.parse::<usize>(), hi.parse::<usize>()) {
                    out.extend(lo..=hi);
                }
            }
            None => out.extend(part.parse::<usize>()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parse_list_reads_what_the_kernel_writes() {
        assert_eq!(parse_list("16"), vec![16]);
        assert_eq!(parse_list("0-1"), vec![0, 1]);
        assert_eq!(parse_list("0-3,8-11"), vec![0, 1, 2, 3, 8, 9, 10, 11]);
        assert_eq!(parse_list("0,4,8"), vec![0, 4, 8]);
        // sysfs files come with one
        assert_eq!(parse_list("2-3\n"), vec![2, 3]);
    }

    #[test]
    fn parse_list_gives_up_on_a_part_it_cannot_read_rather_than_the_whole_line() {
        assert_eq!(parse_list(""), Vec::<usize>::new());
        assert_eq!(parse_list("\n"), Vec::<usize>::new());
        assert_eq!(parse_list("junk"), Vec::<usize>::new());
        assert_eq!(parse_list("0,junk,2"), vec![0, 2]);
        // a range that runs backwards yields nothing, not a panic
        assert_eq!(parse_list("5-2"), Vec::<usize>::new());
    }

    /// Two nodes of four, the way a small two socket machine comes out.
    fn two_nodes() -> Cores {
        Cores::with_layout(&[
            (0, 0),
            (2, 0),
            (4, 0),
            (6, 0),
            (8, 1),
            (10, 1),
            (12, 1),
            (14, 1),
        ])
    }

    #[test]
    fn a_command_takes_its_cores_off_one_node() {
        let cores = two_nodes();
        let lease = cores.acquire(4, &|| false).expect("a node's worth");

        assert_eq!(lease.cpus(), [0, 2, 4, 6]);
        assert_eq!(lease.nodes(), [0]);
    }

    #[test]
    fn a_narrow_request_leaves_the_wide_node_whole() {
        // two free on one node and four on the other: taking the
        // pair off the small one keeps the big one able to hold a
        // request that only it could
        let cores = Cores::with_layout(&[(0, 0), (2, 0), (4, 1), (6, 1), (8, 1), (10, 1)]);

        let small = cores.acquire(2, &|| false).expect("the pair");
        assert_eq!(small.cpus(), [0, 2]);

        let wide = cores
            .acquire(4, &|| false)
            .expect("the other node, still whole");
        assert_eq!(wide.cpus(), [4, 6, 8, 10]);
        assert_eq!(wide.nodes(), [1]);
    }

    #[test]
    fn spreading_takes_the_node_with_the_most_free() {
        let mut cores = Cores::with_layout(&[(0, 0), (2, 0), (4, 0), (6, 1), (8, 1), (10, 1)]);
        cores.placement = Some(Placement::Spread);

        let first = cores.acquire(2, &|| false).expect("a pair");
        assert_eq!(first.nodes(), [0]);

        // packing would put this beside the first, on the one
        // core node 0 has left
        let second = cores.acquire(1, &|| false).expect("one more");
        assert_eq!(second.nodes(), [1]);
    }

    #[test]
    fn a_request_no_node_can_hold_crosses_as_few_as_it_must() {
        let cores = two_nodes();
        let lease = cores.acquire(6, &|| false).expect("more than a node holds");

        assert_eq!(lease.cpus(), [0, 2, 4, 6, 8, 10]);
        assert_eq!(
            lease.nodes(),
            [0, 1],
            "one node filled before the next is touched"
        );
    }

    #[test]
    fn a_machine_with_one_node_reports_none() {
        let cores = Cores::with_pool(vec![0, 2, 4, 6]);
        let lease = cores.acquire(2, &|| false).expect("two of four");

        assert_eq!(lease.cpus(), [0, 2]);
        assert!(
            lease.nodes().is_empty(),
            "nothing to prefer, and nothing for the table to say"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn nodemask_sets_a_bit_per_node() {
        assert_eq!(nodemask(&[0]), (vec![0b1], 2));
        assert_eq!(nodemask(&[1]), (vec![0b10], 3));
        assert_eq!(nodemask(&[0, 1]), (vec![0b11], 3));
        // a node past the first word grows the mask rather than
        // writing off the end of it
        let past = libc::c_ulong::BITS as usize;
        assert_eq!(nodemask(&[past]), (vec![0, 1], past + 2));
        // no nodes is the single node machine asking for no policy
        assert_eq!(nodemask(&[]), (Vec::new(), 0));
    }

    #[test]
    fn cores_go_out_lowest_first() {
        let cores = Cores::with_pool(vec![0, 2, 4, 6]);
        let lease = cores.acquire(2, &|| false).expect("two of four");
        assert_eq!(lease.cpus(), [0, 2]);
    }

    #[test]
    fn two_leases_never_share_a_core() {
        let cores = Cores::with_pool(vec![0, 2, 4, 6]);
        let first = cores.acquire(2, &|| false).expect("two of four");
        let second = cores.acquire(2, &|| false).expect("the other two");

        assert_eq!(first.cpus(), [0, 2]);
        assert_eq!(second.cpus(), [4, 6]);
        assert!(cores.try_acquire(1).is_none(), "pool should be empty");
    }

    #[test]
    fn dropping_a_lease_hands_the_cores_back() {
        let cores = Cores::with_pool(vec![0, 2, 4, 6]);
        {
            let _all = cores.acquire(4, &|| false).expect("the whole pool");
            assert!(cores.try_acquire(1).is_none(), "nothing should be left");
        }
        assert_eq!(
            cores.try_acquire(4).map(|l| l.cpus().to_vec()),
            Some(vec![0, 2, 4, 6]),
            "the whole pool should be back"
        );
    }

    #[test]
    fn packing_leaves_no_gap() {
        let cores = Cores::with_pool(vec![0, 2, 4, 6, 8, 10]);
        let small = cores.acquire(1, &|| false).expect("one");
        let big = cores.acquire(3, &|| false).expect("three");

        assert_eq!(small.cpus(), [0]);
        assert_eq!(
            big.cpus(),
            [2, 4, 6],
            "should start right after the small one"
        );
    }

    #[test]
    fn asking_for_more_than_exists_is_refused_rather_than_waited_on() {
        let cores = Cores::with_pool(vec![0, 2]);
        assert!(cores.acquire(3, &|| false).is_none());
        assert!(cores.try_acquire(3).is_none());
    }

    #[test]
    fn asking_for_none_gets_none() {
        let cores = Cores::with_pool(vec![0, 2]);
        assert!(cores.acquire(0, &|| false).is_none());
        // and it did not quietly take anything on the way past
        assert_eq!(
            cores.try_acquire(2).map(|l| l.cpus().to_vec()),
            Some(vec![0, 2])
        );
    }

    #[test]
    fn a_wait_can_be_abandoned() {
        let cores = Cores::with_pool(vec![0, 2]);
        let _all = cores.acquire(2, &|| false).expect("the whole pool");
        // nothing is free, and the predicate says do not wait for it
        assert!(cores.acquire(1, &|| true).is_none());
    }

    /// Note: if the wake ever breaks, this hangs rather than failing — there is
    /// no timeout on the inner wait to bound it with.
    #[test]
    fn a_waiting_thread_gets_the_cores_when_they_come_back() {
        let cores = Cores::with_pool(vec![0, 2, 4, 6]);
        let all = cores.acquire(4, &|| false).expect("the whole pool");

        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| cores.acquire(2, &|| false).map(|l| l.cpus().to_vec()));
            // long enough that the other thread is parked rather than racing us
            std::thread::sleep(Duration::from_millis(50));
            drop(all);
            assert_eq!(waiting.join().unwrap(), Some(vec![0, 2]));
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_mask_holds_exactly_the_cpus_it_was_given() {
        let set = mask(&[0, 2]);
        let held: Vec<usize> = (0..8)
            .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
            .collect();

        assert_eq!(held, [0, 2]);
        assert_eq!(list(&held), "0,2");
    }
}
