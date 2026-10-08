//! What the kernel applies to a pinned command, read back from the command
//! itself. These need two or more memory nodes, and skip on a machine without.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;
use crate::cmd::Memory;

const PROBE: &str = "grep Cpus_allowed_list /proc/$$/status; head -1 /proc/$$/numa_maps";

/// Set in the environment of the test binary when it is run as a child by
/// [`pages_land_on_the_node_the_command_runs_on`].
const CHILD: &str = "MICHI_NUMA_CHILD";

/// The nodes this process may run on and allocate from.
struct Machine {
    /// Each node's cpus, one per physical core.
    nodes: BTreeMap<usize, Vec<usize>>,
    mems: Vec<usize>,
}

/// This machine, or `None` where the process sees one memory node.
fn machine() -> Option<Machine> {
    let cores = Cores::read();
    let mut nodes = cores.by_node();
    nodes.retain(|node, _| cores.mems.is_empty() || cores.mems.contains(node));
    if !cores.numa || nodes.len() < 2 {
        eprintln!("skipped: this process sees one memory node");
        return None;
    }
    Some(Machine {
        nodes,
        mems: cores.mems.clone(),
    })
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pipeline-numa-{name}"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("out")
}

/// Run `cmd` on `cpus`, under `memory` for `nodes`, and give back its stdout.
fn run(mut cmd: Cmd, cpus: &[usize], nodes: &[usize], memory: Memory, mems: &[usize]) -> String {
    let out = scratch(&format!(
        "{}-{}",
        crate::cpu::list(cpus),
        std::process::id()
    ));
    cmd = cmd.stdout(Output::File(out.clone()));
    cmd.cpus = cpus.to_vec();
    cmd.nodes = nodes.to_vec();
    cmd.policy = policy(memory, nodes, mems);

    let (pid, start) = cmd.spawn().expect("spawn");
    let status = wait(pid, start, None, || {});
    assert!(
        matches!(&status, Status::Finished(t) if t.ok()),
        "{status:?}"
    );
    std::fs::read_to_string(out).unwrap()
}

/// The cpus a probe was allowed, and the policy its first mapping ran under.
fn probe(cpus: &[usize], nodes: &[usize], memory: Memory, mems: &[usize]) -> (Vec<usize>, String) {
    let text = run(
        Cmd::new("/bin/sh").arg("-c", PROBE),
        cpus,
        nodes,
        memory,
        mems,
    );
    let mut lines = text.lines();
    let allowed = lines
        .next()
        .and_then(|l| l.split_once(':'))
        .expect("Cpus_allowed_list")
        .1;
    let policy = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .expect("numa_maps");
    (crate::cpu::parse_list(allowed), policy.to_string())
}

#[test]
fn a_command_on_each_node_runs_there_under_its_policy() {
    let Some(Machine { nodes, mems }) = machine() else {
        return;
    };

    for (&node, cpus) in &nodes {
        let cpus = &cpus[..cpus.len().min(2)];
        for (memory, want) in [
            (Memory::Preferred, format!("prefer:{node}")),
            (Memory::Bound, format!("bind:{node}")),
            (Memory::FirstTouch, "default".to_string()),
        ] {
            let (allowed, policy) = probe(cpus, &[node], memory, &mems);
            assert_eq!(allowed, cpus, "{memory:?} on node {node}");
            assert_eq!(policy, want, "{memory:?} on node {node}");
        }
    }
}

#[test]
fn a_command_across_two_nodes_is_bound_to_both_and_prefers_neither() {
    let Some(Machine { nodes, mems }) = machine() else {
        return;
    };

    let pair: Vec<usize> = nodes.keys().copied().take(2).collect();
    let cpus: Vec<usize> = pair.iter().map(|node| nodes[node][0]).collect();

    // numa_maps writes a node list the way cpu::list does for two
    let (allowed, policy) = probe(&cpus, &pair, Memory::Bound, &mems);
    assert_eq!(allowed, cpus);
    assert_eq!(policy, format!("bind:{}", crate::cpu::list(&pair)));

    let (_, policy) = probe(&cpus, &pair, Memory::Preferred, &mems);
    assert_eq!(policy, "default");
}

/// The child half of [`pages_land_on_the_node_the_command_runs_on`]: touch
/// 64 MiB and print how many KiB of anonymous memory sit on each node. Does
/// nothing when run as an ordinary test.
#[test]
fn allocate_and_report() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }

    let mut block = vec![0u8; 64 << 20];
    for page in block.chunks_mut(4096) {
        page[0] = 1;
    }
    std::hint::black_box(&block);

    let mut kib: BTreeMap<usize, u64> = BTreeMap::new();
    for line in std::fs::read_to_string("/proc/self/numa_maps")
        .unwrap()
        .lines()
    {
        if !line.contains("anon=") {
            continue;
        }
        let page_kib = line
            .split_whitespace()
            .find_map(|f| f.strip_prefix("kernelpagesize_kB="))
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(4);
        for field in line.split_whitespace() {
            let Some((node, pages)) = field.strip_prefix('N').and_then(|f| f.split_once('='))
            else {
                continue;
            };
            if let (Ok(node), Ok(pages)) = (node.parse(), pages.parse::<u64>()) {
                *kib.entry(node).or_default() += pages * page_kib;
            }
        }
    }

    let fields: Vec<String> = kib
        .iter()
        .map(|(node, kib)| format!("{node}={kib}"))
        .collect();
    println!("michi-numa-kib {}", fields.join(" "));
}

#[test]
fn pages_land_on_the_node_the_command_runs_on() {
    let Some(Machine { nodes, mems }) = machine() else {
        return;
    };

    let me = std::env::current_exe().unwrap();
    for (&node, cpus) in &nodes {
        let cpus = &cpus[..1];
        for memory in [Memory::Preferred, Memory::Bound, Memory::FirstTouch] {
            let child = Cmd::new(&me)
                .env(CHILD, "1")
                .path("execute::numa::allocate_and_report")
                .flag("--exact")
                .flag("--nocapture")
                .flag("--test-threads=1");
            let text = run(child, cpus, &[node], memory, &mems);

            // libtest has already written "test <name> ... " on
            // this line, so the report is found where it starts
            let report = text
                .split_once("michi-numa-kib ")
                .and_then(|(_, rest)| rest.lines().next())
                .unwrap_or_else(|| panic!("no report in {text:?}"));
            let kib: BTreeMap<usize, u64> = report
                .split_whitespace()
                .filter_map(|f| f.split_once('='))
                .map(|(n, k)| (n.parse().unwrap(), k.parse().unwrap()))
                .collect();

            let here = kib.get(&node).copied().unwrap_or(0);
            let total: u64 = kib.values().sum();
            // first touch is local too, because the thread
            // doing the touching is pinned to this node
            assert!(
                here * 10 >= total * 9,
                "{memory:?} on node {node}: {kib:?} KiB by node"
            );
        }
    }
}

#[test]
fn commands_sharing_a_pool_may_run_on_all_of_it() {
    let cores = Cores::read();
    if cores.len() < 4 {
        eprintln!("skipped: fewer than four cores to pool");
        return;
    }
    // the pipeline carves from the same pool with the same
    // placement, so it lands on these
    let (pool, _) = cores.carve(4).whole().unwrap();

    let dir = scratch("pool").parent().unwrap().to_path_buf();
    let cmds: Vec<Cmd> = (0..4)
        .map(|k| {
            Cmd::new("/bin/sh")
                .arg("-c", "grep Cpus_allowed_list /proc/$$/status")
                .stdout(Output::File(dir.join(k.to_string())))
        })
        .collect();
    crate::PipelineBuilder::new()
        .step(crate::Step::batched(4, cmds).pool(4))
        .no_stderr()
        .build()
        .unwrap()
        .run()
        .unwrap();

    for k in 0..4 {
        let text = std::fs::read_to_string(dir.join(k.to_string())).unwrap();
        let allowed = text.split_once(':').expect("Cpus_allowed_list").1;
        assert_eq!(crate::cpu::parse_list(allowed), pool, "command {k}");
    }
}
