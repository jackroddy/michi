//! One thing a step holds, as a sink sees it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::closure::Closure;
use crate::cmd::Cmd;
use crate::execute::{Policy, Status};

/// A command or a closure, whichever a step holds.
#[derive(Clone, Copy, Debug)]
pub enum Item<'a> {
    Cmd(&'a Cmd),
    Closure(&'a Closure<'a>),
}

impl<'a> Item<'a> {
    /// What to call this in a table or on a progress line.
    pub fn label(self) -> String {
        match self {
            Item::Cmd(cmd) => cmd.label(),
            Item::Closure(closure) => closure.label().to_string(),
        }
    }

    pub fn status(self) -> &'a Status {
        match self {
            Item::Cmd(cmd) => cmd.status(),
            Item::Closure(closure) => closure.status(),
        }
    }

    pub fn fields(self) -> &'a BTreeMap<String, String> {
        match self {
            Item::Cmd(cmd) => cmd.fields(),
            Item::Closure(closure) => closure.fields(),
        }
    }

    pub fn tags(self) -> &'a BTreeSet<String> {
        match self {
            Item::Cmd(cmd) => cmd.tags(),
            Item::Closure(closure) => closure.tags(),
        }
    }

    /// The line you could paste into a shell to get the same thing. A closure
    /// has none, run or not.
    pub fn line(self) -> Option<String> {
        match self {
            Item::Cmd(cmd) => Some(cmd.line()),
            Item::Closure(_) => None,
        }
    }

    /// Where its stderr went, for one whose stderr was routed to a file.
    pub fn stderr_path(self) -> Option<&'a Path> {
        match self {
            Item::Cmd(cmd) => cmd.stderr_path(),
            Item::Closure(_) => None,
        }
    }

    /// What it exited with. `None` for a closure, whose status says how it
    /// returned.
    pub fn exit(self) -> Option<i32> {
        match self {
            Item::Cmd(cmd) => cmd.status().timing().map(|t| t.exit),
            Item::Closure(_) => None,
        }
    }

    /// How many cores it asked for, zero for one that asked for none.
    pub fn cores(self) -> usize {
        match self {
            Item::Cmd(cmd) => cmd.cores.unwrap_or(0),
            Item::Closure(_) => 0,
        }
    }

    /// The cpus it is pinned to: empty until it holds them and after it
    /// releases them. `None` for a closure, which holds no cpus of its own.
    pub fn cpus(self) -> Option<&'a [usize]> {
        match self {
            Item::Cmd(cmd) => Some(&cmd.cpus),
            Item::Closure(_) => None,
        }
    }

    pub fn nodes(self) -> Option<&'a [usize]> {
        match self {
            Item::Cmd(cmd) => Some(&cmd.nodes),
            Item::Closure(_) => None,
        }
    }

    /// The memory policy it ran under: `prefer:1`, `bind:0-1` or `default`.
    /// `None` for a closure, for a command never placed on a node, and for one
    /// refused its bind.
    pub fn policy(self) -> Option<String> {
        let Item::Cmd(cmd) = self else {
            return None;
        };
        if cmd.nodes.is_empty() {
            return None;
        }
        cmd.policy.label()
    }

    /// Why it ran without the memory preference it asked for, if it did.
    pub fn policy_note(self) -> Option<&'a str> {
        match self {
            Item::Cmd(Cmd {
                policy: Policy::Dropped(why),
                ..
            }) => Some(why),
            _ => None,
        }
    }

    /// Whether it ran across the whole of a pool it shared, rather than on cores
    /// leased to it alone. Its [`cpus`](Item::cpus) are then the pool's.
    pub fn pooled(self) -> bool {
        match self {
            Item::Cmd(cmd) => cmd.pooled,
            Item::Closure(_) => false,
        }
    }

    /// Whether the machine this ran on has more than one memory node.
    pub fn numa(self) -> bool {
        match self {
            Item::Cmd(cmd) => cmd.numa,
            Item::Closure(_) => false,
        }
    }
}
