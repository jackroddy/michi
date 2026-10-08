//! Which CPUs a command is allowed to run on.
//!
//! A command asks for a number of cores and never says which. [`Cores`] tracks
//! which are free, leases that many, and takes them back when the command is
//! done. The child pins itself between the fork and the exec.
//!
//! A command's cores come off one memory node wherever that node has enough
//! free, so its threads and the memory they touch are on the same node.

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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    /// The node with the fewest free cores that still fits, keeping the others
    /// whole for a wider request. Concurrent commands share a node's cache and
    /// memory bandwidth.
    #[default]
    Pack,

    /// The node with the most free cores, so concurrent commands land on
    /// separate nodes until there are more of them than nodes.
    Spread,
}

/// The cores a pipeline has to hand out, one logical CPU per physical core,
/// and which of them are in use.
#[derive(Debug, Default)]
pub(crate) struct Cores {
    pool: Vec<Cpu>,
    /// The cpus currently leased out.
    taken: Mutex<Vec<usize>>,

    /// What a command that cannot be placed yet waits on.
    freed: Condvar,

    /// Which node a request comes off when more than one could hold it.
    pub(crate) placement: Placement,

    /// The memory nodes this process may allocate from, or empty where that
    /// could not be read.
    pub(crate) mems: Vec<usize>,

    /// Whether the cpus this process may run on cover more than one memory
    /// node.
    //
    // read off the whole pool once and copied into every
    // carve: a pool carved onto one node of a two node machine
    // still names its node and sets its policy
    pub(crate) numa: bool,

    /// Whether this is a pool carved for a pipeline or a step, which a command
    /// asking for no cores runs across whole.
    pub(crate) pooled: bool,
}

/// Cores held for as long as one command needs them, handed back on drop so a
/// command that failed to spawn, timed out or panicked releases them too.
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

        // only the first cpu of each sibling group joins the pool:
        // two logical cpus on one physical core share its
        // execution units, and a command given both would get
        // less than two cores of work while the table said 2
        for cpu in allowed() {
            if spoken_for.contains(&cpu) {
                continue;
            }
            spoken_for.extend(siblings(cpu));
            pool.push(cpu);
        }

        Cores::new(locate(&pool, &nodes()), mems())
    }

    fn new(pool: Vec<Cpu>, mems: Vec<usize>) -> Cores {
        Cores {
            numa: spans_nodes(&pool),
            pool,
            taken: Mutex::new(Vec::new()),
            freed: Condvar::new(),
            placement: Placement::Pack,
            mems,
            pooled: false,
        }
    }

    /// A pool of exactly these cpus, all on node 0.
    #[cfg(test)]
    pub(crate) fn with_pool(pool: Vec<usize>) -> Cores {
        let pool = pool.into_iter().map(|id| (id, 0)).collect::<Vec<_>>();
        Cores::with_layout(&pool)
    }

    /// A pool laid out as `(cpu, node)`, for placing commands on a machine this
    /// one is not.
    #[cfg(test)]
    pub(crate) fn with_layout(layout: &[(usize, usize)]) -> Cores {
        let pool = layout.iter().map(|&(id, node)| Cpu { id, node }).collect();
        Cores::new(pool, Vec::new())
    }

    /// A pool of `size` of these cpus, chosen the way a command's would be.
    ///
    /// Nothing may be leased from this one while the carve is in use: a
    /// pipeline carves before its run and a step as it starts, when nothing
    /// else holds any.
    pub(crate) fn carve(&self, size: usize) -> Cores {
        let mut pool = place(&self.pool, size, self.placement);
        pool.sort_unstable_by_key(|cpu| cpu.id);
        Cores {
            placement: self.placement,
            numa: self.numa,
            pooled: true,
            ..Cores::new(pool, self.mems.clone())
        }
    }

    /// The cpus of a carved pool and the nodes they sit on, for a command that
    /// shares it rather than leasing cores of its own. `None` for the machine's
    /// own pool, whose commands asking for no cores are not pinned at all.
    pub(crate) fn whole(&self) -> Option<(Vec<usize>, Vec<usize>)> {
        if !self.pooled {
            return None;
        }
        let cpus = self.pool.iter().map(|cpu| cpu.id).collect();
        Some((cpus, self.nodes_of(&self.pool)))
    }

    /// The nodes `cpus` sit on, lowest first, or none on a machine with one.
    fn nodes_of(&self, cpus: &[Cpu]) -> Vec<usize> {
        if !self.numa {
            return Vec::new();
        }
        let mut nodes: Vec<usize> = cpus.iter().map(|cpu| cpu.node).collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    /// This pool's cpus, grouped by the node they sit on.
    #[cfg(test)]
    pub(crate) fn by_node(&self) -> BTreeMap<usize, Vec<usize>> {
        let mut nodes: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for cpu in &self.pool {
            nodes.entry(cpu.node).or_default().push(cpu.id);
        }
        nodes
    }

    pub(crate) fn len(&self) -> usize {
        self.pool.len()
    }

    /// Take `size` cores, waiting for them if they are not free yet.
    ///
    /// `None` for a request of zero or of more than the pool holds, and once
    /// `abandon` returns true while waiting.
    pub(crate) fn acquire(&self, size: usize, abandon: &dyn Fn() -> bool) -> Option<Lease<'_>> {
        if size == 0 || size > self.pool.len() {
            return None;
        }

        let mut taken = self.taken.lock().unwrap();
        loop {
            if let Some(lease) = self.grab(&mut taken, size) {
                return Some(lease);
            }
            if abandon() {
                return None;
            }
            taken = self.freed.wait(taken).unwrap();
        }
    }

    /// Take `size` cores if they are free right now, and never wait. For
    /// working out what a run would look like without running it.
    pub(crate) fn try_acquire(&self, size: usize) -> Option<Lease<'_>> {
        self.acquire(size, &|| true)
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

        // the best arrangement free now, even across nodes: one
        // node is worth choosing but not worth holding a
        // command back for
        let placed = place(&free, size, self.placement);

        // lowest first, so a run with the machine to itself
        // places its commands the same way every time and the
        // cpu column reads in order
        let mut cpus: Vec<usize> = placed.iter().map(|cpu| cpu.id).collect();
        cpus.sort_unstable();

        let nodes = self.nodes_of(&placed);

        taken.extend(&cpus);
        Some(Lease {
            cores: self,
            cpus,
            nodes,
        })
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

/// Whether these cpus cover more than one memory node.
fn spans_nodes(pool: &[Cpu]) -> bool {
    let mut nodes = pool.iter().map(|cpu| cpu.node);
    let first = nodes.next();
    nodes.any(|node| Some(node) != first)
}

/// The `size` cpus to hand out, off one memory node wherever one of them has
/// that many free.
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

/// The pool's cpus, each with the memory node it sits on.
///
/// A cpu that no node's cpulist names puts the whole pool on node 0, which is
/// a pool with one node and so no policy at all.
fn locate(cpus: &[usize], nodes: &BTreeMap<usize, usize>) -> Vec<Cpu> {
    // a map missing one cpu may be missing a whole node, and
    // placing by it would bind commands to nodes it has wrong
    let whole = cpus.iter().all(|cpu| nodes.contains_key(cpu));

    cpus.iter()
        .map(|&id| Cpu {
            id,
            node: if whole { nodes[&id] } else { 0 },
        })
        .collect()
}

/// Which memory node each cpu belongs to.
fn nodes() -> BTreeMap<usize, usize> {
    let mut out = BTreeMap::new();

    // a kernel built without NUMA has no node directory, and
    // locate then puts every cpu on node 0
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

/// The calling thread's affinity, put back when this is dropped.
pub(crate) struct Pinned {
    #[cfg(target_os = "linux")]
    was: libc::cpu_set_t,
}

/// Pin the calling thread to `cpus` until the guard is dropped. Nothing for an
/// empty list, and nothing where the thread's affinity cannot be read or set.
#[cfg(target_os = "linux")]
pub(crate) fn pin_thread(cpus: &[usize]) -> Option<Pinned> {
    if cpus.is_empty() {
        return None;
    }
    let size = size_of::<libc::cpu_set_t>();

    // SAFETY: cpu_set_t is a plain bit array, and all zeroes
    // is the empty set
    let mut was: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    // SAFETY: both sets are cpu_set_t of the size passed, and pid 0 is the
    // calling thread
    unsafe {
        if libc::sched_getaffinity(0, size, &mut was) != 0 {
            return None;
        }
        if libc::sched_setaffinity(0, size, &mask(cpus)) != 0 {
            return None;
        }
    }
    Some(Pinned { was })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn pin_thread(_cpus: &[usize]) -> Option<Pinned> {
    None
}

impl Drop for Pinned {
    fn drop(&mut self) {
        // SAFETY: `was` came back from sched_getaffinity on this thread
        #[cfg(target_os = "linux")]
        unsafe {
            libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &self.was);
        }
    }
}

/// The affinity mask for `cpus`, built before the fork: what installs it runs
/// after, where only async-signal-safe calls are allowed.
#[cfg(target_os = "linux")]
pub(crate) fn mask(cpus: &[usize]) -> libc::cpu_set_t {
    // SAFETY: cpu_set_t is a plain bit array, and all zeroes
    // is the empty set
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    // a cpu at or past CPU_SETSIZE would write outside the
    // set. allowed cannot produce one, a pool from elsewhere
    // could
    for cpu in cpus.iter().filter(|cpu| **cpu < libc::CPU_SETSIZE as usize) {
        // SAFETY: the filter keeps cpu inside the set
        unsafe { libc::CPU_SET(*cpu, &mut set) };
    }
    set
}

/// The node mask for `nodes`, and the `maxnode` to pass `set_mempolicy` with it,
/// built before the fork as [`mask`] is.
#[cfg(target_os = "linux")]
pub(crate) fn nodemask(nodes: &[usize]) -> (Vec<libc::c_ulong>, usize) {
    let Some(highest) = nodes.iter().copied().max() else {
        return (Vec::new(), 0);
    };

    let word = libc::c_ulong::BITS as usize;
    let read = highest + 1;
    let mut mask = vec![0 as libc::c_ulong; read.div_ceil(word)];

    for node in nodes {
        mask[node / word] |= 1 << (node % word);
    }

    // **note: the kernel decrements maxnode before reading the
    //         mask (get_nodes in mm/mempolicy.c), so it reads
    //         maxnode - 1 bits. highest + 1 drops the top node's
    //         bit, and a lone node then reads as an empty mask:
    //         EINVAL for bind, a silent MPOL_LOCAL for preferred
    (mask, read + 1)
}

/// A cpu list in the form `taskset -c` takes: `0`, `0,2`, `0-3`, `0-94:2`,
/// empty for nothing.
pub(crate) fn list(cpus: &[usize]) -> String {
    let mut cpus = cpus.to_vec();
    cpus.sort_unstable();
    cpus.dedup();

    let mut parts = Vec::new();
    let mut at = 0;
    while at < cpus.len() {
        let first = cpus[at];
        let step = cpus.get(at + 1).map_or(0, |next| next - first);
        let mut last = at;
        while step > 0 && cpus.get(last + 1) == Some(&(cpus[last] + step)) {
            last += 1;
        }

        // a step of 1 is a range from two cpus on, and any other
        // step from three: 0,2 is shorter than 0-2:2
        match (last - at + 1, step) {
            (2.., 1) => parts.push(format!("{first}-{}", cpus[last])),
            (3.., _) => parts.push(format!("{first}-{}:{step}", cpus[last])),
            _ => {
                last = at;
                parts.push(first.to_string());
            }
        }
        at = last + 1;
    }
    parts.join(",")
}

/// How wide the list for `cores` cpus comes out if they are low-numbered and
/// evenly spaced: `0`, `0,2`, and a run such as `0-10:2` from three on.
pub(crate) fn list_width(cores: usize) -> usize {
    match cores {
        0 => 0,
        1 => 1,
        2 => 3,

        // the low guess: a higher cpu or a ragged list widens
        // the column past this once the cpus are known
        _ => "0-10:2".len(),
    }
}

/// The CPUs this process may run on, fewer than the machine has under `taskset`
/// or a cpuset.
#[cfg(target_os = "linux")]
fn allowed() -> Vec<usize> {
    // SAFETY: cpu_set_t is a plain bit array, all zeroes is
    // the empty set, and sched_getaffinity writes at most the
    // size passed into it
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) };
    if rc != 0 {
        return Vec::new();
    }

    // SAFETY: every cpu in the range is inside the set
    (0..libc::CPU_SETSIZE as usize)
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
        .collect()
}

/// Every CPU the machine has, since there is no affinity to read here.
#[cfg(not(target_os = "linux"))]
fn allowed() -> Vec<usize> {
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    (0..cpus).collect()
}

/// Every logical CPU sharing a physical core with this one, itself included, or
/// only this one where its topology cannot be read.
fn siblings(cpu: usize) -> Vec<usize> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    match std::fs::read_to_string(path) {
        Ok(text) => parse_list(&text),
        Err(_) => vec![cpu],
    }
}

/// A kernel cpulist: `0-1`, `16`, `0-3,8-11`.
pub(crate) fn parse_list(text: &str) -> Vec<usize> {
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
    fn a_list_compresses_runs_and_leaves_a_step_of_one_unsaid() {
        assert_eq!(list(&[]), "");
        assert_eq!(list(&[3]), "3");
        assert_eq!(list(&[0, 2]), "0,2");
        assert_eq!(list(&[0, 1]), "0-1");
        assert_eq!(list(&[0, 1, 2, 3]), "0-3");
        assert_eq!(list(&[0, 2, 4, 6]), "0-6:2");
        // a pair a step apart is no run, so the next one starts
        // from its second cpu
        assert_eq!(list(&[0, 2, 3, 4]), "0,2-4");
        assert_eq!(
            list(&[4, 0, 2, 2]),
            "0-4:2",
            "sorted and deduplicated first"
        );

        // one node of a machine that numbers its nodes alternately,
        // and a request that took five off one node and the rest off
        // the other
        let odd: Vec<usize> = (1..96).step_by(2).collect();
        assert_eq!(list(&odd), "1-95:2");
        let mut spanning = vec![0, 1, 2, 3, 4];
        spanning.extend((6..=94).step_by(2));
        assert_eq!(list(&spanning), "0-4,6-94:2");
    }

    #[test]
    fn parse_list_reads_what_the_kernel_writes() {
        assert_eq!(parse_list("16"), vec![16]);
        assert_eq!(parse_list("0-1"), vec![0, 1]);
        assert_eq!(parse_list("0-3,8-11"), vec![0, 1, 2, 3, 8, 9, 10, 11]);
        assert_eq!(parse_list("0,4,8"), vec![0, 4, 8]);
        // sysfs files end in a newline
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
        cores.placement = Placement::Spread;

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
    fn a_cpu_no_node_names_puts_the_pool_on_one_node() {
        let nodes = BTreeMap::from([(0, 0), (1, 1), (2, 0)]);

        let placed = locate(&[0, 1, 2], &nodes);
        assert_eq!(placed.iter().map(|c| c.node).collect::<Vec<_>>(), [0, 1, 0]);

        // node 1's cpulist unread, so cpu 3 is nowhere
        let partial = locate(&[0, 1, 2, 3], &nodes);
        assert!(partial.iter().all(|c| c.node == 0));
    }

    #[test]
    fn a_carved_pool_sits_on_one_node_and_still_names_it() {
        let machine = Cores::with_layout(&[(0, 0), (1, 1), (2, 0), (3, 1), (4, 0), (5, 1)]);
        assert!(machine.whole().is_none(), "the machine is not a pool");

        let pool = machine.carve(2);
        assert_eq!(pool.whole(), Some((vec![0, 2], vec![0])));

        // one node's worth of cpus, on a machine with two: a lease
        // out of it still says where it is
        let lease = pool.acquire(1, &|| false).expect("one of two");
        assert_eq!(lease.nodes(), [0]);
    }

    #[test]
    fn a_pool_carved_from_a_pool_stays_inside_it() {
        let machine = Cores::with_layout(&[(0, 0), (1, 1), (2, 0), (3, 1), (4, 0), (5, 1)]);
        let outer = machine.carve(4);
        let inner = outer.carve(3);

        let (cpus, nodes) = inner.whole().unwrap();
        assert_eq!(cpus.len(), 3);
        let (outside, _) = outer.whole().unwrap();
        assert!(cpus.iter().all(|cpu| outside.contains(cpu)), "{cpus:?}");
        // the outer pool took node 0's three and one of node 1's,
        // so node 0 alone holds the inner one
        assert_eq!(nodes, [0]);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_pinned_thread_gets_its_affinity_back() {
        let before = allowed();
        {
            let _pinned = pin_thread(&before[..1]).expect("pin to one allowed cpu");
            assert_eq!(allowed(), before[..1]);
        }
        assert_eq!(allowed(), before);
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
        // and took nothing
        assert_eq!(
            cores.try_acquire(2).map(|l| l.cpus().to_vec()),
            Some(vec![0, 2])
        );
    }

    #[test]
    fn a_wait_can_be_abandoned() {
        let cores = Cores::with_pool(vec![0, 2]);
        let _all = cores.acquire(2, &|| false).expect("the whole pool");
        assert!(cores.acquire(1, &|| true).is_none());
    }

    #[test]
    fn a_waiting_thread_gets_the_cores_when_they_come_back() {
        let cores = Cores::with_pool(vec![0, 2, 4, 6]);
        let all = cores.acquire(4, &|| false).expect("the whole pool");

        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| cores.acquire(2, &|| false).map(|l| l.cpus().to_vec()));
            // long enough for the other thread to be waiting
            // before the drop
            std::thread::sleep(Duration::from_millis(50));
            drop(all);

            // note: if the wake breaks, this hangs rather than
            //       fails, since the wait has no timeout
            assert_eq!(waiting.join().unwrap(), Some(vec![0, 2]));
        });
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_mask_holds_exactly_the_cpus_it_was_given() {
        let set = mask(&[0, 2]);
        // SAFETY: cpus 0 to 7 are inside the set
        let held: Vec<usize> = (0..8)
            .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
            .collect();

        assert_eq!(held, [0, 2]);
        assert_eq!(list(&held), "0,2");
    }
}
