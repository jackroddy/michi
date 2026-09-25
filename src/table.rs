//! A sink that writes the summary table.
//!
//! Two kinds of row, told apart by the first column. A step row carries the
//! step's name and its wall clock; the command rows under it carry a `|` or `||`
//! instead of a name, and their own numbers. A step of one command collapses to
//! a single row, since the two would otherwise say the same thing twice.
//!
//! A block is built when its step finishes, because a column's width is not
//! known until the last cell in it has arrived. [`Mode`] decides what happens
//! to it then.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::error::{BoxError, Error};
use crate::execute::{Status, Timing};
use crate::fmt::{bytes, cpu_pct};
use crate::item::Item;
use crate::sink::Sink;
use crate::step::{Step, Strategy};

use toil::{Align, Cell, Column, Header, Schema, Widths};

/// Marks a command that ran after the one above it.
const SERIAL: &str = "|";
/// Marks a command that ran alongside the others in its step.
const BATCH: &str = "||";

/// Whether every block gets its own header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Headers {
    /// One header at the top of the file. Its widths are the floor for every
    /// block, so blocks line up with it and with each other until some value
    /// turns out wider than the label above it.
    Once,
    /// A header on every block, so each block reads on its own.
    Each,
}

/// How the table is laid out and when it reaches the file.
///
/// The combinations that make no sense cannot be written down: there is nothing
/// to decide about headers when the file holds one block, and ragged columns
/// force a header on every block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Hold everything back and write one table with every row padded alike.
    /// Nothing reaches the file until the run is over.
    Whole,
    /// Write each step's block as that step finishes. Every block carries the
    /// same columns, padded to its own contents. The default, with
    /// [`Headers::Once`].
    Blocks { headers: Headers },
    /// Write each step's block as that step finishes, each carrying only the
    /// columns its own commands use.
    Ragged,
}

/// Writes a table of everything the pipeline ran.
#[derive(Debug)]
pub struct Table {
    path: PathBuf,
    mode: Mode,
    /// The columns every block carries, worked out before anything runs so blocks
    /// agree whatever order fields turn up in. Unused by [`Mode::Ragged`], which
    /// asks each step instead.
    columns: Columns,
    /// Every row so far, for [`Mode::Whole`].
    rows: Vec<Vec<Cell>>,
    /// Widths every block starts from, worked out before the run from everything
    /// already known: names, fields, tags, argv. Only the numbers are missing,
    /// and their headings are wider than they usually are. `None` for the modes
    /// that do not share widths between blocks.
    floor: Option<Widths>,
    text: String,
}

impl Default for Mode {
    fn default() -> Mode {
        Mode::Blocks {
            headers: Headers::Once,
        }
    }
}

impl Table {
    pub fn new(path: impl Into<PathBuf>) -> Table {
        Table {
            path: path.into(),
            mode: Mode::default(),
            columns: Columns::default(),
            rows: Vec::new(),
            floor: None,
            text: String::new(),
        }
    }

    pub fn mode(mut self, mode: Mode) -> Table {
        self.mode = mode;
        self
    }

    fn flush(&mut self) -> Result<(), BoxError> {
        let io = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::Io { path, source }
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        std::fs::write(&self.path, &self.text).map_err(io(&self.path))?;
        Ok(())
    }
}

impl Sink for Table {
    fn start(&mut self, steps: &[Step<'_>]) -> Result<(), BoxError> {
        self.columns = Columns::of(steps);

        // Ragged blocks are meant to differ, and Whole renders in one go, so
        // neither has anything to share.
        self.floor = match self.mode {
            Mode::Ragged | Mode::Whole => None,
            _ => Some(self.columns.measure(steps)),
        };

        self.text.clear();
        if matches!(
            self.mode,
            Mode::Blocks {
                headers: Headers::Once
            }
        ) {
            self.text = self.columns.render(&[], Header::Show, self.floor.as_ref());
        }
        self.flush()
    }

    // no `record`: a step's rows are built from the step itself once it is done,
    // which keeps them in the order the commands were declared rather than the
    // order a batch happened to finish them in

    fn step_done(&mut self, step: &Step<'_>) -> Result<(), BoxError> {
        let columns = match self.mode {
            Mode::Ragged => Columns::of(std::slice::from_ref(step)),
            _ => self.columns.clone(),
        };

        let mut rows = columns.block(step);

        let header = match self.mode {
            Mode::Whole => {
                self.rows.append(&mut rows);
                return Ok(());
            }
            Mode::Blocks {
                headers: Headers::Once,
            } => Header::Hide,
            _ => Header::Show,
        };

        self.text
            .push_str(&columns.render(&rows, header, self.floor.as_ref()));
        self.flush()
    }

    fn finish(&mut self) -> Result<(), BoxError> {
        if self.mode == Mode::Whole {
            let rows = std::mem::take(&mut self.rows);
            self.text = self.columns.render(&rows, Header::Show, None);
            self.flush()?;
        }
        Ok(())
    }
}

/// The field keys and tags a block carries, and which placement columns it
/// needs.
///
/// Which ones there are depends on the commands, so every part of a block —
/// the header, the step line, each command line — has to agree about them. They
/// live here rather than being handed to each in turn.
#[derive(Clone, Debug, Default)]
struct Columns {
    keys: Vec<String>,
    tags: Vec<String>,
    /// Whether anything in the run asks to be pinned. A run where nothing does
    /// gets no cpus column at all, rather than one of nothing but dashes.
    ///
    /// This asks what was requested and not where anything landed, because the
    /// columns are settled before the run and nothing has landed anywhere yet.
    cpus: bool,

    /// Whether to say which memory node each command landed on. A machine with
    /// one node has nothing to say here, so it gets no column rather than one
    /// reading `0` all the way down.
    nodes: bool,

    /// How wide the cpus column is reserved before anything has cpus to show.
    //
    // every cpus cell still says `-` before the run, but how
    // many each command gets is settled, so the column can be
    // sized for the widest of those rather than for a dash
    cpus_width: usize,
}

impl Columns {
    /// The keys and tags these steps carry, each in sorted order. Commands and
    /// closures share the columns, since they share the table.
    fn of(steps: &[Step<'_>]) -> Columns {
        let mut keys = BTreeSet::new();
        let mut tags = BTreeSet::new();
        let mut cpus = false;
        let mut nodes = false;
        let mut cpus_width = 0;

        for step in steps {
            for item in step.items() {
                // anything in a pool is pinned, whether or not it
                // asked for cores of its own
                let placed = item.cores() > 0 || step.pooled;
                keys.extend(item.fields().keys().cloned());
                tags.extend(item.tags().iter().cloned());
                cpus |= placed;
                nodes |= placed && item.numa();
                cpus_width = cpus_width.max(crate::cpu::list_width(item.cores()));
            }
        }

        Columns {
            keys: keys.into_iter().collect(),
            tags: tags.into_iter().collect(),
            cpus,
            nodes,
            cpus_width,
        }
    }

    /// The columns, in the order [`cells`](Columns::cells) fills them.
    ///
    /// Placement sits just before argv because the widths reserved for it are
    /// only guesses, and everything a wider value shifts along is then argv,
    /// which is last and unpadded anyway.
    fn schema(&self) -> Schema {
        let mut columns = vec![Column::new("step"), Column::new("cmd")];
        columns.extend(self.keys.iter().chain(&self.tags).map(Column::new));
        columns.extend(["wall(s)", "user(s)", "sys(s)"].map(|label| Column::new(label).fixed(2)));
        columns.extend(["cpu(%)", "max_rss", "exit", "status"].map(Column::new));
        if self.nodes {
            columns.push(Column::new("node"));
            // prefer:N is as wide as a policy gets on a machine
            // with under ten nodes, so that is the guess
            columns.push(Column::new("policy").min_width("prefer:0".len()));
        }
        if self.cpus {
            columns.push(Column::new("cpus").min_width(self.cpus_width));
        }
        columns.push(Column::new("argv").ragged());
        Schema::new(columns)
    }

    /// `rows` laid out under these columns, from `floor` where there is one.
    fn render(&self, rows: &[Vec<Cell>], header: Header, floor: Option<&Widths>) -> String {
        let schema = self.schema();
        let floor = floor.cloned().unwrap_or_else(|| schema.widths());
        let mut table = toil::Table::new(schema);
        for row in rows {
            table.row(row.iter().cloned());
        }
        table.render_with(&floor, header)
    }

    /// How wide each column has to be for every block to fit under one header.
    ///
    /// Everything but the numbers is already known before the run, and the
    /// headings above the numbers are wider than the numbers usually are.
    fn measure(&self, steps: &[Step<'_>]) -> Widths {
        let schema = self.schema();
        let rows: Vec<_> = steps
            .iter()
            .flat_map(|step| self.block(step))
            .map(|cells| schema.row(cells))
            .collect();
        schema.measure(&rows)
    }

    /// One step's rows: its own line, then a line per command.
    fn block(&self, step: &Step<'_>) -> Vec<Vec<Cell>> {
        let mut rows = Vec::new();

        // a step of one would just repeat itself, so it gets no line of its own
        // and keeps the first column instead, with its command filling in the rest
        let alone = step.items().count() == 1;
        let first = if alone {
            Cell::from(step.label())
        } else {
            rows.push(self.step_row(step));
            Cell::from(match step.strategy() {
                Some(Strategy::Batched { .. }) => BATCH,
                _ => SERIAL,
            })
            .align(Align::Right)
        };

        for item in step.items() {
            // under a step line of its own, which names the pool,
            // a command sharing it just says so. a closure is
            // never placed itself, so one alone in its step shows
            // the pool it ran pinned to
            let pool = match item {
                Item::Closure(_) if alone && step.pooled => {
                    Pool::Named(&step.pool_cpus, &step.pool_nodes)
                }
                _ if !alone && item.pooled() => Pool::Shared,
                _ => Pool::Own,
            };
            rows.push(self.row(first.clone(), item, pool));
        }
        rows
    }

    /// The step's own line: measured wall clock, and its commands' CPU added up.
    fn step_row(&self, step: &Step<'_>) -> Vec<Cell> {
        let timings: Vec<&Timing> = step
            .items()
            .filter_map(|item| item.status().timing())
            .collect();

        // exit, status and argv belong to commands; a count or a rollup here
        // would be a different quantity sharing a column
        let mut cost = Cost {
            wall_s: step.wall_s(),
            ..Cost::default()
        };

        if !timings.is_empty() {
            // summed CPU against measured wall is what shows whether a batch
            // actually bought anything: a step that ran four at once reads about
            // four times what any one of them did
            //
            // a closure has no cpu figure, and a sum over only what we did
            // measure would read as the step's whole cost. std's Sum for Option
            // gives up on the total instead, which is the honest answer
            cost.user_s = timings.iter().map(|t| t.user_s).sum();
            cost.sys_s = timings.iter().map(|t| t.sys_s).sum();
            // the largest any one process got, which is not the same as the most
            // the step held at once — wait4 cannot tell us that
            // a max, unlike a sum, is not spoiled by one with no number at all
            cost.max_rss_kb = timings.iter().filter_map(|t| t.max_rss_kb).max();
        }

        // the cpus a whole step held are only the step's to report
        // when it carved them as a pool. otherwise they are the
        // commands', and the step line leaves them alone the way
        // it does exit and argv
        let pool = |list: &[usize]| {
            if step.pooled {
                listed(list)
            } else {
                Cell::missing()
            }
        };
        self.cells(Line {
            first: Cell::from(step.label()),
            name: Cell::missing(),
            keys: vec![Cell::missing(); self.keys.len() + self.tags.len()],
            cost,
            node: pool(&step.pool_nodes),
            policy: Cell::missing(),
            cpus: pool(&step.pool_cpus),
            argv: Cell::missing(),
        })
    }

    /// One item's line, whichever kind it is. `first` is the step name for a
    /// collapsed step of one, and a right-aligned `|` or `||` otherwise. The
    /// columns a closure has no answer for come back `None` from [`Item`] and
    /// print as `-`.
    fn row(&self, first: Cell, item: Item<'_>, pool: Pool<'_>) -> Vec<Cell> {
        // two separate questions: what it cost, and how it went. one that could
        // not start has nothing to say about the first
        let t = item.status().timing();
        let (cpus, nodes) = match pool {
            Pool::Named(cpus, nodes) => (cpus, nodes),
            _ => (
                item.cpus().unwrap_or_default(),
                item.nodes().unwrap_or_default(),
            ),
        };
        let policy = match (item.policy(), item.policy_note()) {
            (Some(policy), Some(note)) => Some(format!("{policy} ({note})")),
            (policy, _) => policy,
        };

        self.cells(Line {
            first,
            name: Cell::from(item.label()),
            keys: self.key_cells(item.fields(), item.tags()),
            cost: Cost {
                wall_s: t.map(|t| t.wall_s),
                user_s: t.and_then(|t| t.user_s),
                sys_s: t.and_then(|t| t.sys_s),
                max_rss_kb: t.and_then(|t| t.max_rss_kb),
                exit: item.exit(),
                status: Some(status_word(item.status())),
            },
            node: listed(nodes),
            policy: Cell::from(policy),
            cpus: match pool {
                Pool::Shared => Cell::from("pool"),
                _ => listed(cpus),
            },
            argv: Cell::from(item.line()),
        })
    }

    /// A line's cells, in the order [`schema`](Columns::schema) names the
    /// columns, leaving out the placement ones this table does not have.
    fn cells(&self, line: Line) -> Vec<Cell> {
        let Line {
            first,
            name,
            keys,
            cost,
            node,
            policy,
            cpus,
            argv,
        } = line;

        let mut cells = vec![first, name];
        cells.extend(keys);
        cells.extend(cost.cells());
        if self.nodes {
            cells.extend([node, policy]);
        }
        if self.cpus {
            cells.push(cpus);
        }
        cells.push(argv);
        cells
    }

    /// The field and tag cells, which every row carries the same way.
    fn key_cells(&self, fields: &BTreeMap<String, String>, tags: &BTreeSet<String>) -> Vec<Cell> {
        let fields = self.keys.iter().map(|k| Cell::from(fields.get(k)));
        let tags = self
            .tags
            .iter()
            .map(|t| Cell::from(tags.contains(t).then_some("x")));
        fields.chain(tags).collect()
    }
}

/// Everything one line says, before the columns it has decide what is left
/// out.
struct Line {
    first: Cell,
    name: Cell,
    keys: Vec<Cell>,
    cost: Cost,
    node: Cell,
    policy: Cell,
    cpus: Cell,
    argv: Cell,
}

/// What something cost and how it went. Anything left out prints as `-`,
/// which is how a command that never started says it has no numbers.
#[derive(Default)]
struct Cost {
    wall_s: Option<f64>,
    user_s: Option<f64>,
    sys_s: Option<f64>,
    max_rss_kb: Option<i64>,
    exit: Option<i32>,
    status: Option<&'static str>,
}

impl Cost {
    fn cells(self) -> [Cell; 7] {
        let cpu = match (self.user_s, self.sys_s) {
            (Some(user), Some(sys)) => Some(cpu_pct(user + sys, self.wall_s)),
            _ => None,
        };
        [
            Cell::from(self.wall_s),
            Cell::from(self.user_s),
            Cell::from(self.sys_s),
            Cell::from(cpu),
            Cell::from(self.max_rss_kb.map(bytes)),
            Cell::from(self.exit),
            Cell::from(self.status),
        ]
    }
}

/// A cpu or node list, or `-` for one with nothing in it: a command that asked
/// for cpus and never got as far as holding any reads the same as having none.
fn listed(list: &[usize]) -> Cell {
    match list {
        [] => Cell::missing(),
        list => Cell::from(crate::cpu::list(list)),
    }
}

/// What a row says about a pool its item ran in.
#[derive(Clone, Copy)]
enum Pool<'a> {
    /// Nothing: its own cpus, or none.
    Own,
    /// It shares the pool its step line names.
    Shared,
    /// It ran pinned to these cpus and nodes, with no step line to name them.
    Named(&'a [usize], &'a [usize]),
}

/// The status column. It answers one question — did this work — and leaves the
/// exit column to say what the command actually reported, which is also how
/// "never started" tells itself apart from "started and failed" without a word
/// of its own: there is no exit code beside it.
fn status_word(status: &Status) -> &'static str {
    match status {
        Status::NotRun => "-",
        Status::Skipped => "skip",
        Status::TimedOut(_) => "time",
        Status::Finished(t) if t.ok() => "ok",
        _ => "fail",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::closure::Closure;
    use crate::cmd::{Cmd, Output};
    use crate::execute::Policy;
    use crate::step::OnError;

    /// Fixed numbers so a rendered table is the same every time: 2.25s of CPU
    /// over 1.5s of wall is 150%, and 2048 KiB is 2.00MiB.
    fn timing(exit: i32) -> Timing {
        Timing {
            wall_s: 1.5,
            user_s: Some(2.0),
            sys_s: Some(0.25),
            max_rss_kb: Some(2048),
            exit,
        }
    }

    fn cmd(program: &str, name: &str) -> Cmd {
        Cmd::new(program)
            .name(name)
            .stdout(Output::Inherit)
            .stderr(Output::Inherit)
    }

    fn finish(step: &mut Step<'_>, index: usize) {
        step.index = Some(index);
        step.elapsed_s = Some(1.5);
        for cmd in step.cmds_mut() {
            cmd.status = Status::Finished(timing(0));
        }
        for closure in step.closures_mut() {
            // a closure only ever has a wall clock behind it
            closure.status = Status::Finished(Timing {
                user_s: None,
                sys_s: None,
                max_rss_kb: None,
                ..timing(0)
            });
        }
    }

    /// A step of one that collapses, then a batch of two carrying a field.
    fn steps<'a>() -> Vec<Step<'a>> {
        let mut setup = Step::serial([cmd("/mkdir", "mkdir")]).name("setup");
        finish(&mut setup, 1);

        let mut burn = Step::batched(
            2,
            [
                cmd("/a", "a").field("job", 1),
                cmd("/b", "b").field("job", 2),
            ],
        )
        .name("burn");
        finish(&mut burn, 2);

        vec![setup, burn]
    }

    /// A path of this test's own. Tests share a process and run at once, so a
    /// shared one would have them deleting each other's output.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pipeline-table-{name}"));
        std::fs::remove_dir_all(&dir).ok();
        dir.join("runs.tbl")
    }

    /// Drive the sink the way a pipeline would, and give back what it wrote.
    fn write(name: &str, mode: Mode, steps: &[Step<'_>]) -> String {
        let path = scratch(name);

        let mut table = Table::new(&path).mode(mode);
        table.start(steps).unwrap();
        for step in steps {
            table.step_done(step).unwrap();
        }
        table.finish().unwrap();

        std::fs::read_to_string(&path).unwrap()
    }

    fn header_lines(text: &str) -> usize {
        text.lines().filter(|l| l.starts_with("# step")).count()
    }

    #[test]
    fn a_block_lays_out_under_its_header() {
        let expected = "\
# step     cmd   job wall(s) user(s) sys(s) cpu(%) max_rss exit status argv
# -------- ----- --- ------- ------- ------ ------ ------- ---- ------ ----
[1](setup) mkdir -   1.50    2.00    0.25   150%   2.00MiB 0    ok     /mkdir
[2](burn)  -     -   1.50    4.00    0.50   300%   2.00MiB -    -      -
        || a     1   1.50    2.00    0.25   150%   2.00MiB 0    ok     /a
        || b     2   1.50    2.00    0.25   150%   2.00MiB 0    ok     /b
";
        assert_eq!(write("golden", Mode::default(), &steps()), expected);
    }

    #[test]
    fn a_step_of_one_keeps_the_first_column_instead_of_a_line_of_its_own() {
        let text = write("collapse", Mode::default(), &steps());
        let rows: Vec<&str> = text.lines().skip(2).collect();

        assert_eq!(
            rows.len(),
            4,
            "one collapsed step plus a step row and two commands"
        );
        assert!(rows[0].starts_with("[1](setup) mkdir"), "{}", rows[0]);
        assert!(rows[1].starts_with("[2](burn)  -"), "{}", rows[1]);
    }

    #[test]
    fn a_batch_marks_its_commands_differently_from_a_serial_one() {
        let mut serial = Step::serial([cmd("/a", "a"), cmd("/b", "b")]).name("s");
        finish(&mut serial, 1);
        let mut batched = Step::batched(2, [cmd("/a", "a"), cmd("/b", "b")]).name("b");
        finish(&mut batched, 2);

        let text = write("markers", Mode::default(), &[serial, batched]);
        assert_eq!(text.matches(" | ").count(), 2, "{text}");
        assert_eq!(text.matches("|| ").count(), 2, "{text}");
    }

    #[test]
    fn a_step_that_never_ran_has_no_numbers_to_report() {
        let mut step = Step::serial([cmd("/a", "a"), cmd("/b", "b")]).name("s");
        step.index = Some(1);

        let text = write("never-ran", Mode::default(), &[step]);
        let step_row = text.lines().nth(2).unwrap();

        assert!(step_row.starts_with("[1](s)"), "{step_row}");
        assert!(
            !step_row.contains('%'),
            "no command finished, so there is no cpu figure: {step_row}"
        );
    }

    #[test]
    fn blocks_with_one_header_writes_it_before_anything_runs() {
        let path = scratch("early");

        let mut table = Table::new(&path).mode(Mode::default());
        table.start(&steps()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(header_lines(&text), 1);
        assert_eq!(text.lines().count(), 2, "header and separator, no rows yet");
    }

    #[test]
    fn one_header_or_one_per_step() {
        let steps = steps();
        assert_eq!(
            header_lines(&write("one-header", Mode::default(), &steps)),
            1
        );

        let text = write(
            "each-header",
            Mode::Blocks {
                headers: Headers::Each,
            },
            &steps,
        );
        assert_eq!(header_lines(&text), steps.len());
    }

    #[test]
    fn whole_holds_everything_back_until_the_run_is_over() {
        let path = scratch("whole");
        let steps = steps();

        let mut table = Table::new(&path).mode(Mode::Whole);
        table.start(&steps).unwrap();
        for step in &steps {
            table.step_done(step).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "",
            "nothing should reach the file before finish"
        );

        table.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(header_lines(&text), 1);
        assert_eq!(text.lines().count(), 6);
    }

    #[test]
    fn ragged_gives_each_block_only_the_columns_its_own_commands_use() {
        let text = write("ragged", Mode::Ragged, &steps());
        let heads: Vec<&str> = text.lines().filter(|l| l.starts_with("# step")).collect();

        assert_eq!(heads.len(), 2, "ragged blocks each need their own header");
        assert!(
            !heads[0].contains("job"),
            "setup has no fields: {}",
            heads[0]
        );
        assert!(heads[1].contains("job"), "burn does: {}", heads[1]);
    }

    /// One pinned command that landed on node 1, on a machine that has more
    /// than one to land on.
    fn placed<'a>(numa: bool) -> Step<'a> {
        let mut step = Step::serial([cmd("/x", "pinned").cores(2)]).name("s");
        for cmd in step.cmds_mut() {
            cmd.numa = numa;
            cmd.cpus = vec![4, 6];
            cmd.nodes = if numa { vec![1] } else { Vec::new() };
            cmd.policy = if numa {
                Policy::Preferred(1)
            } else {
                Policy::Default
            };
        }
        finish(&mut step, 1);
        step
    }

    #[test]
    fn a_node_column_says_where_a_command_landed() {
        let text = write("node-on", Mode::default(), &[placed(true)]);
        let lines: Vec<&str> = text.lines().collect();

        let at = lines[0].find("node").expect("a node heading");
        assert!(lines[2][at..].starts_with('1'), "{}", lines[2]);
        assert!(
            lines[0][at..].starts_with("node policy   cpus"),
            "node, then policy, then cpus: {}",
            lines[0]
        );
        assert!(lines[2][at..].contains(" prefer:1 "), "{}", lines[2]);
    }

    #[test]
    fn a_pooled_step_names_its_pool_once_and_its_commands_say_pool() {
        let mut step = Step::serial([cmd("/x", "a"), cmd("/y", "b")]).name("s");
        step.pooled = true;
        step.pool_cpus = vec![4, 6];
        for cmd in step.cmds_mut() {
            cmd.pooled = true;
            cmd.cpus = vec![4, 6];
        }
        finish(&mut step, 1);
        let text = write("pooled", Mode::default(), &[step]);
        let lines: Vec<&str> = text.lines().collect();

        let at = lines[0].find("cpus").expect("a cpus heading");
        assert!(lines[2][at..].starts_with("4,6"), "{}", lines[2]);
        assert!(lines[3][at..].starts_with("pool"), "{}", lines[3]);
        assert!(lines[4][at..].starts_with("pool"), "{}", lines[4]);
    }

    #[test]
    fn a_closure_alone_in_a_pooled_step_shows_the_pool() {
        let mut step = Step::from_closures([Closure::new("c", || Ok(()))]).name("s");
        step.pooled = true;
        step.pool_cpus = vec![4, 6];
        let text = write("pooled-closure", Mode::default(), &[step]);
        let lines: Vec<&str> = text.lines().collect();

        let at = lines[0].find("cpus").expect("a cpus heading");
        assert!(lines[2][at..].starts_with("4,6"), "{}", lines[2]);
    }

    #[test]
    fn a_dropped_preference_says_why_in_the_policy_column() {
        let mut step = placed(true);
        for cmd in step.cmds_mut() {
            cmd.policy = Policy::Dropped("node 1 not in Mems_allowed".into());
        }
        let text = write("node-dropped", Mode::default(), &[step]);

        assert!(
            text.contains("default (node 1 not in Mems_allowed)"),
            "{text}"
        );
    }

    #[test]
    fn one_node_gets_no_node_column_at_all() {
        let text = write("node-off", Mode::default(), &[placed(false)]);

        assert!(!text.contains("node"), "a column of nothing but 0: {text}");
        assert!(
            text.contains("cpus"),
            "the cpus column still stands: {text}"
        );
    }

    #[test]
    fn columns_are_sorted_and_asked_for_only_once() {
        let step = Step::serial([
            cmd("/a", "a").field("zed", 1).field("alpha", 2).tag("slow"),
            cmd("/b", "b").field("alpha", 3).tag("slow").tag("first"),
        ]);
        let columns = Columns::of(&[step]);

        assert_eq!(columns.keys, ["alpha", "zed"]);
        assert_eq!(columns.tags, ["first", "slow"]);
    }

    #[test]
    fn a_tag_reads_as_present_or_absent_rather_than_as_a_value() {
        let mut step = Step::serial([cmd("/a", "a").tag("setup"), cmd("/b", "b")]).name("s");
        finish(&mut step, 1);

        let text = write("tags", Mode::default(), &[step]);
        let lines: Vec<&str> = text.lines().collect();
        let at = lines[0].find("setup").unwrap();

        assert_eq!(&lines[3][at..at + 1], "x");
        assert_eq!(&lines[4][at..at + 1], "-");
    }

    #[test]
    fn status_and_exit_answer_different_questions() {
        // a command that never started has no exit code beside its "fail", which
        // is how it tells itself apart from one that started and failed
        assert_eq!(status_word(&Status::NotRun), "-");
        assert_eq!(status_word(&Status::Skipped), "skip");
        assert_eq!(status_word(&Status::Failed("x".into())), "fail");
        assert_eq!(status_word(&Status::TimedOut(timing(143))), "time");
        assert_eq!(status_word(&Status::Finished(timing(0))), "ok");
        assert_eq!(status_word(&Status::Finished(timing(1))), "fail");
    }

    #[test]
    fn a_skipped_step_still_gets_a_row() {
        let mut step = Step::serial([cmd("/a", "a")])
            .name("never")
            .on_error(OnError::Continue);
        step.index = Some(1);
        step.cmds_mut()[0].status = Status::Skipped;

        let text = write("skipped", Mode::default(), &[step]);
        let row = text.lines().nth(2).unwrap();

        assert!(row.starts_with("[1](never) a"), "{row}");
        assert!(row.contains("skip"), "{row}");
    }

    #[test]
    fn a_step_of_one_closure_collapses_and_shares_its_columns_with_commands() {
        let mut setup = Step::serial([cmd("/mkdir", "mkdir").field("job", 1)]).name("setup");
        finish(&mut setup, 1);

        // `shard` is the closure's alone, so the column can only come from it;
        // `job` is shared, so the two have to agree about where it sits
        let mut check = Step::from_closures([Closure::new("verify", || Ok(()))
            .field("job", 2)
            .field("shard", 7)])
        .name("check");
        finish(&mut check, 2);

        // the closure row carries a wall clock and a verdict, and dashes every
        // column a thread has no answer for
        let expected = "\
# step     cmd    job shard wall(s) user(s) sys(s) cpu(%) max_rss exit status argv
# -------- ------ --- ----- ------- ------- ------ ------ ------- ---- ------ ----
[1](setup) mkdir  1   -     1.50    2.00    0.25   150%   2.00MiB 0    ok     /mkdir
[2](check) verify 2   7     1.50    -       -      -      -       -    ok     -
";

        assert_eq!(
            write("closure-collapse", Mode::default(), &[setup, check]),
            expected
        );
    }

    #[test]
    fn a_step_of_two_closures_gets_a_line_of_its_own() {
        let mut step = Step::from_closures([
            Closure::new("first", || Ok(())),
            Closure::new("second", || Ok(())),
        ])
        .name("check");
        finish(&mut step, 1);

        let text = write("closure-block", Mode::default(), &[step]);
        let rows: Vec<&str> = text.lines().skip(2).collect();

        assert_eq!(rows.len(), 3, "a step row and two closures: {rows:?}");
        assert!(rows[0].starts_with("[1](check)"), "{}", rows[0]);
        // serial, so the same marker a serial command step gets
        assert!(rows[1].trim_start().starts_with("| first"), "{}", rows[1]);
        assert!(rows[2].trim_start().starts_with("| second"), "{}", rows[2]);

        // the step's own row has a wall clock but no cpu to add up
        let step_row = rows[0];
        assert!(step_row.contains("1.50"), "{step_row}");
    }
}
