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
use crate::fmt::bytes;
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
    /// One header at the top of the file. Blocks are padded to at least its
    /// widths, so they line up until a value is wider than its heading.
    Once,
    /// A header on every block, so each block reads on its own.
    Each,
}

/// How the table is laid out and when it reaches the file.
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
    /// The columns every block carries, unused by [`Mode::Ragged`].
    //
    // worked out before anything runs, so blocks agree
    // whatever order fields turn up in
    columns: Columns,
    /// Every row so far, for [`Mode::Whole`].
    rows: Vec<Vec<Cell>>,
    /// The widths every block is padded to at least, empty for the modes that
    /// do not share widths between blocks.
    //
    // measured before the run from names, fields, tags and
    // argv. only the numbers are missing, and their headings
    // are usually wider than they are
    floor: Widths,
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
            floor: Widths::default(),
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

        // ragged blocks differ by design, and whole renders in
        // one go, so neither shares widths
        self.floor = match self.mode {
            Mode::Ragged | Mode::Whole => Widths::default(),
            _ => self.columns.measure(steps),
        };

        self.text.clear();
        if matches!(
            self.mode,
            Mode::Blocks {
                headers: Headers::Once
            }
        ) {
            self.text = self.columns.render(&[], Header::Show, &self.floor);
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
            .push_str(&columns.render(&rows, header, &self.floor));
        self.flush()
    }

    fn finish(&mut self) -> Result<(), BoxError> {
        if self.mode == Mode::Whole {
            let rows = std::mem::take(&mut self.rows);
            self.text = self.columns.render(&rows, Header::Show, &Widths::default());
            self.flush()?;
        }
        Ok(())
    }
}

/// The field keys and tags a block carries, and which placement columns it
/// needs.
#[derive(Clone, Debug, Default)]
struct Columns {
    keys: Vec<String>,
    tags: Vec<String>,
    /// Whether anything in the run asks to be pinned.
    //
    // what was requested, not where anything landed: the
    // columns are settled before the run. a run with nothing
    // pinned gets no cpus column rather than one of dashes
    cpus: bool,

    /// Whether to show which memory node each command landed on.
    //
    // a machine with one node gets no column rather than
    // one reading `0` all the way down
    nodes: bool,

    /// How wide the cpus column is reserved before anything has cpus to show.
    //
    // every cpus cell still says `-` before the run, but how
    // many each command gets is settled, so the column can be
    // sized for the widest of those rather than for a dash
    cpus_width: usize,
}

impl Columns {
    /// The columns these steps need, with keys and tags each in sorted order.
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
    fn schema(&self) -> Schema {
        let mut columns = vec![Column::new("step"), Column::new("cmd")];
        columns.extend(self.keys.iter().chain(&self.tags).map(Column::new));
        columns.extend(["wall(s)", "user(s)", "sys(s)"].map(|label| Column::new(label).fixed(2)));
        columns.extend(["cpu(%)", "max_rss", "exit", "status"].map(Column::new));

        // placement sits just before argv because its widths
        // are only guesses, and a wider value then shifts
        // only argv, which is last and unpadded
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

    /// `rows` laid out under these columns, padded to at least `floor`. A
    /// `floor` that is empty or measured for other columns is ignored.
    fn render(&self, rows: &[Vec<Cell>], header: Header, floor: &Widths) -> String {
        let schema = self.schema();
        let mut table = toil::Table::new(schema);
        for row in rows {
            table.row(row.iter().cloned());
        }
        table.render_with(floor, header)
    }

    /// How wide each column has to be for every block to fit under one header.
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

        // a step of one gets no line of its own: its name goes
        // in the first column of its command's row
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
            // summed cpu against measured wall shows how much a
            // batch ran in parallel: a step that ran four at once
            // reads about four times what any one of them did
            //
            // a closure has no cpu figure, and summing Options
            // gives None if any is None, so the step gets no total
            // rather than a partial one that reads as its whole cost
            cost.user_s = timings.iter().map(|t| t.user_s).sum();
            cost.sys_s = timings.iter().map(|t| t.sys_s).sum();
            // the largest any one process reached, not the most the
            // step held at once, which wait4 does not report. a
            // missing figure leaves a max correct, so it is skipped
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

    /// One item's line. `first` is the step name for a collapsed step of one,
    /// and a right-aligned `|` or `||` otherwise.
    fn row(&self, first: Cell, item: Item<'_>, pool: Pool<'_>) -> Vec<Cell> {
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

    /// The field and tag cells.
    fn key_cells(&self, fields: &BTreeMap<String, String>, tags: &BTreeSet<String>) -> Vec<Cell> {
        let fields = self.keys.iter().map(|k| Cell::from(fields.get(k)));
        let tags = self
            .tags
            .iter()
            .map(|t| Cell::from(tags.contains(t).then_some("x")));
        fields.chain(tags).collect()
    }
}

/// Every cell of one line, before [`cells`](Columns::cells) leaves out the
/// placement columns this table does not have.
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

/// What something cost and how it went. A `None` prints as `-`.
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
            (Some(user), Some(sys)) => cpu_pct(user + sys, self.wall_s),
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

/// `time`'s `%P`: cpu time over wall clock, so four cores busy throughout
/// read 400%. `None` when there is no wall clock to divide by.
fn cpu_pct(cpu_s: f64, wall_s: Option<f64>) -> Option<String> {
    // `time` writes `?%` here; `None` prints as `-` like
    // any other missing number
    let wall = wall_s.filter(|wall| *wall > 0.0)?;

    // truncated rather than rounded to match `time`,
    // which divides two integers
    Some(format!("{:.0}%", (cpu_s / wall * 100.0).floor()))
}

/// A cpu or node list, or `-` for an empty one.
fn listed(list: &[usize]) -> Cell {
    match list {
        // a command that asked for cpus and never held any
        // reads the same as one that asked for none
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

/// The status column: whether it worked. What the command reported is left
/// to the exit column.
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

    /// One command pinned to cpus 4 and 6, and placed on node 1 when `numa`.
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
        // a command that could not start reads "fail" with no
        // exit code beside it, which separates it from one that
        // started and failed
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

        // the closure row has a wall clock and a status, and `-`
        // in every column a closure has no figure for
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

    #[test]
    fn cpu_pct_truncates_rather_than_rounding() {
        // gnu time divides two integers, so 199.9% reads 199%, not 200%
        assert_eq!(cpu_pct(1.999, Some(1.0)).as_deref(), Some("199%"));
        assert_eq!(cpu_pct(0.9999, Some(1.0)).as_deref(), Some("99%"));
        assert_eq!(cpu_pct(4.0, Some(1.0)).as_deref(), Some("400%"));
    }

    #[test]
    fn cpu_pct_needs_a_clock_to_divide_by() {
        assert_eq!(cpu_pct(1.0, None), None);
        assert_eq!(cpu_pct(1.0, Some(0.0)), None);
    }
}
