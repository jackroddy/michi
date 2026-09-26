use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::execute::{Policy, Status};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Output {
    #[default]
    Null,
    Inherit,
    File(PathBuf),
    Append(PathBuf),
    OnFailure(PathBuf),
}

/// Something that can stand in as the value of an option.
pub trait Value {
    fn render(self) -> String;
}

macro_rules! value_via_display {
    ($($t:ty),* $(,)?) => {
        $(impl Value for $t {
            fn render(self) -> String {
                self.to_string()
            }
        })*
    };
}

value_via_display!(
    &str,
    String,
    &String,
    char,
    bool,
    u8,
    u16,
    u32,
    u64,
    usize,
    i8,
    i16,
    i32,
    i64,
    isize,
    f32,
    f64,
    std::fmt::Arguments<'_>,
);

macro_rules! value_via_path {
    ($($t:ty),*) => {
        $(impl Value for $t {
            fn render(self) -> String {
                self.display().to_string()
            }
        })*
    };
}

// Path has no Display impl, and a Debug print would
// wrap it in quotes
value_via_path!(&Path, PathBuf, &PathBuf);

/// An option: a flag on its own, or a flag with a value after it.
#[derive(Clone, Debug)]
pub(crate) struct Opt {
    flag: String,
    value: Option<String>,
}

/// One level of a command: the program itself, or a subcommand under it.
#[derive(Clone, Debug)]
pub(crate) struct Level {
    /// The subcommand word, or `None` for the program's own level.
    sub: Option<String>,
    opts: Vec<Opt>,
    positionals: Vec<String>,
}

impl Level {
    fn new(sub: Option<String>) -> Level {
        Level {
            sub,
            opts: Vec::new(),
            positionals: Vec::new(),
        }
    }
}

/// Where a command's pages come from, once its cores sit on a memory node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Memory {
    /// Prefer the node the cores are on, and spill onto another when it fills.
    /// `MPOL_PREFERRED`.
    #[default]
    Preferred,

    /// That node and no other. `MPOL_BIND`, so a command needing more than the
    /// node has left is killed rather than slowed.
    Bound,

    /// Ask for nothing, and let each page land on the node of the thread that
    /// touched it first, which is what the kernel does unasked.
    FirstTouch,
}

/// A command, built but not run.
#[derive(Clone, Debug)]
pub struct Cmd {
    pub(crate) name: Option<String>,
    pub(crate) program: PathBuf,
    /// How many cores this asks for. Resolved when the pipeline is built, since
    /// it can come from the step instead.
    pub(crate) cores: Option<usize>,
    /// The cpus it was given, filled in when it runs and only for as long as it
    /// held them.
    pub(crate) cpus: Vec<usize>,

    /// The memory nodes those cpus sit on, and nothing on a machine with one
    /// node.
    pub(crate) nodes: Vec<usize>,

    /// Where its pages should come from, or `None` until [`Step`](crate::Step)
    /// or [`Pipeline`](crate::Pipeline) settles it.
    pub(crate) memory: Option<Memory>,

    /// The memory policy it was given once placed.
    pub(crate) policy: Policy,

    /// Whether the machine has more than one memory node.
    //
    // a fact about the machine rather than the command, kept
    // here because a sink is handed items and never the pool,
    // and the table has to settle its columns before the run
    // when no command has landed anywhere yet
    pub(crate) numa: bool,

    /// Whether its cpus are the whole of a pool it shares, rather than cores
    /// leased to it alone.
    pub(crate) pooled: bool,
    /// The program's own level, then one per subcommand.
    //
    // never empty: new pushes the program's level
    //
    // kept apart rather than as one argv so an option added
    // after a positional still comes out in front of it.
    // several tools take their query and target as trailing
    // positionals and would read a trailing option as
    // another file
    pub(crate) levels: Vec<Level>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) dir: Option<PathBuf>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) stdout: Output,
    pub(crate) stderr: Output,
    pub(crate) fields: BTreeMap<String, String>,
    pub(crate) tags: BTreeSet<String>,
    pub(crate) status: Status,
}

impl Cmd {
    pub fn new(program: impl AsRef<Path>) -> Self {
        Cmd {
            name: None,
            program: program.as_ref().to_owned(),
            cores: None,
            cpus: Vec::new(),
            nodes: Vec::new(),
            memory: None,
            policy: Policy::Default,
            numa: false,
            pooled: false,
            levels: vec![Level::new(None)],
            env: BTreeMap::new(),
            dir: None,
            timeout: None,
            stdout: Output::Null,
            stderr: Output::Null,
            fields: BTreeMap::new(),
            tags: BTreeSet::new(),
            status: Status::NotRun,
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Pin this command to `cores` physical cores, whichever are free.
    /// Overrides whatever its step asked for.
    pub fn cores(mut self, cores: usize) -> Self {
        self.cores = Some(cores);
        self
    }

    /// A subcommand, like the `search` in `mmseqs search`, which may nest.
    /// Options and paths added after it go on that subcommand.
    #[expect(
        clippy::should_implement_trait,
        reason = "a subcommand, not subtraction"
    )]
    pub fn sub(mut self, sub: impl Into<String>) -> Self {
        self.levels.push(Level::new(Some(sub.into())));
        self
    }

    /// Where its pages should come from. The default is
    /// [`Memory::Preferred`], and a command asking for no cores is never
    /// placed, so this does nothing to it.
    pub fn memory(mut self, memory: Memory) -> Self {
        self.memory = Some(memory);
        self
    }

    /// An option that stands alone, like `--allow-overwrite`.
    pub fn flag(mut self, flag: impl Into<String>) -> Self {
        self.current().opts.push(Opt {
            flag: flag.into(),
            value: None,
        });
        self
    }

    /// An option and the value that follows it, like `-E 10`.
    pub fn arg(mut self, flag: impl Into<String>, value: impl Value) -> Self {
        self.current().opts.push(Opt {
            flag: flag.into(),
            value: Some(value.render()),
        });
        self
    }

    /// A positional. These come out in the order they were added, after
    /// everything else on their level.
    pub fn path(mut self, path: impl AsRef<Path>) -> Self {
        self.current()
            .positionals
            .push(path.as_ref().display().to_string());
        self
    }

    /// The level the next option or path goes on: the last `sub`, or the
    /// program's own if there has not been one.
    fn current(&mut self) -> &mut Level {
        self.levels
            .last_mut()
            .expect("a Cmd always has its program's level")
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Value) -> Self {
        self.env.insert(key.into(), value.render());
        self
    }

    pub fn dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    pub fn timeout(mut self, after: Duration) -> Self {
        self.timeout = Some(after);
        self
    }

    pub fn stdout(mut self, out: Output) -> Self {
        self.stdout = out;
        self
    }

    pub fn stderr(mut self, out: Output) -> Self {
        self.stderr = out;
        self
    }

    pub fn stdout_to(self, path: impl Into<PathBuf>) -> Self {
        self.stdout(Output::File(path.into()))
    }

    pub fn stderr_to(self, path: impl Into<PathBuf>) -> Self {
        self.stderr(Output::File(path.into()))
    }

    pub fn field(mut self, key: impl Into<String>, value: impl Value) -> Self {
        self.fields.insert(key.into(), value.render());
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.insert(tag.into());
        self
    }

    /// The command's name, or failing that its program's file name. Not
    /// unique.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => match self.program.file_name() {
                Some(file) => file.to_string_lossy().into_owned(),
                None => self.program.display().to_string(),
            },
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }

    pub fn tags(&self) -> &BTreeSet<String> {
        &self.tags
    }

    pub fn stderr_path(&self) -> Option<&Path> {
        match &self.stderr {
            Output::Null | Output::Inherit => None,
            Output::File(p) | Output::Append(p) | Output::OnFailure(p) => Some(p),
        }
    }

    /// Everything after the program, in the order it gets handed to the shell.
    pub(crate) fn args(&self) -> Vec<String> {
        let mut out = Vec::new();

        for level in &self.levels {
            out.extend(level.sub.iter().cloned());

            for opt in &level.opts {
                out.push(opt.flag.clone());
                if let Some(value) = &opt.value {
                    out.push(value.clone());
                }
            }

            out.extend(level.positionals.iter().cloned());
        }

        out
    }

    /// The command as a shell line, without its pinning.
    pub fn line(&self) -> String {
        let mut parts: Vec<String> = self
            .env
            .iter()
            .map(|(key, value)| format!("{key}={}", quote(value)))
            .collect();

        parts.push(quote(&self.program.display().to_string()));
        parts.extend(self.args().iter().map(|a| quote(a)));

        let mut line = parts.join(" ");

        // a subshell, so pasting the line leaves the user's
        // shell where it was. the redirects go outside it:
        // the files are opened before the child changes
        // directory, so a relative one lands in the same place
        if let Some(dir) = &self.dir {
            line = format!("(cd {} && {line})", quote(&dir.display().to_string()));
        }

        for redirect in [redirect(&self.stdout, ""), redirect(&self.stderr, "2")]
            .into_iter()
            .flatten()
        {
            line.push(' ');
            line.push_str(&redirect);
        }

        line
    }
}

fn redirect(out: &Output, fd: &str) -> Option<String> {
    let (op, path) = match out {
        Output::Inherit => return None,
        Output::Null => (">", "/dev/null".to_string()),
        // OnFailure writes the file too, and report may delete
        // it after the run
        Output::File(p) | Output::OnFailure(p) => (">", quote(&p.display().to_string())),
        Output::Append(p) => (">>", quote(&p.display().to_string())),
    };

    Some(format!("{fd}{op} {path}"))
}

fn quote(arg: &str) -> String {
    const SAFE_PUNCT: &str = "_-./=:+,@%^";

    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || SAFE_PUNCT.contains(c));

    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_leaves_alone_what_a_shell_would_read_the_same_way() {
        for plain in [
            "nail",
            "--allow-overwrite",
            "/home/jack/tools/bin/nail",
            "12.0",
            "a_b-c.d/e=f:g+h,i@j%k^l",
        ] {
            assert_eq!(quote(plain), plain);
        }
    }

    #[test]
    fn quote_wraps_anything_a_shell_would_read_differently() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("two words"), "'two words'");
        assert_eq!(quote("a*b"), "'a*b'");
        assert_eq!(quote("$HOME"), "'$HOME'");
        assert_eq!(quote("a\nb"), "'a\nb'");
        assert_eq!(quote("a;rm -rf /"), "'a;rm -rf /'");
        assert_eq!(quote("~/x"), "'~/x'");
        assert_eq!(quote("café"), "'café'");
    }

    #[test]
    fn quote_closes_and_reopens_around_a_single_quote() {
        // 'it'\''s' is the only way to get a literal quote inside single quotes
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("'"), r"''\'''");
    }

    #[test]
    fn positionals_come_last_however_they_were_added() {
        // several tools read a trailing option as another
        // input file
        let cmd = Cmd::new("/bin/mmseqs")
            .sub("search")
            .path("query.fa")
            .arg("-s", "7.5")
            .path("target.fa")
            .flag("--quiet");

        assert_eq!(
            cmd.args(),
            ["search", "-s", "7.5", "--quiet", "query.fa", "target.fa"]
        );
    }

    #[test]
    fn options_keep_the_order_they_were_given_in() {
        let cmd = Cmd::new("/x").arg("-a", 1).flag("-b").arg("-c", 3);
        assert_eq!(cmd.args(), ["-a", "1", "-b", "-c", "3"]);
    }

    #[test]
    fn subcommands_nest_in_front() {
        let cmd = Cmd::new("/x").sub("outer").sub("inner").flag("-q");
        assert_eq!(cmd.args(), ["outer", "inner", "-q"]);
    }

    #[test]
    fn options_before_the_first_sub_stay_in_front_of_it() {
        let cmd = Cmd::new("/git")
            .arg("-C", "dir")
            .sub("commit")
            .arg("-m", "msg");
        assert_eq!(cmd.args(), ["-C", "dir", "commit", "-m", "msg"]);
    }

    #[test]
    fn every_level_keeps_its_own_options_and_positionals() {
        let cmd = Cmd::new("/docker")
            .sub("run")
            .flag("-it")
            .path("img")
            .sub("cmd")
            .flag("--opt")
            .path("in");

        assert_eq!(cmd.args(), ["run", "-it", "img", "cmd", "--opt", "in"]);
    }

    #[test]
    fn a_positional_on_the_program_level_comes_before_the_first_sub() {
        let cmd = Cmd::new("/x").path("a").sub("s").path("b");
        assert_eq!(cmd.args(), ["a", "s", "b"]);
    }

    #[test]
    fn pinning_stays_out_of_the_argv() {
        let mut cmd = Cmd::new("/bin/nail").sub("search");
        cmd.cpus = vec![0, 2];
        assert_eq!(cmd.args(), ["search"]);
        assert_eq!(cmd.line(), "/bin/nail search > /dev/null 2> /dev/null");
    }

    #[test]
    fn a_line_carries_its_environment_in_front() {
        let line = Cmd::new("/usr/bin/wc")
            .env("LC_ALL", "C")
            .flag("-l")
            .stdout(Output::Inherit)
            .stderr(Output::Inherit)
            .line();

        assert_eq!(line, "LC_ALL=C /usr/bin/wc -l");
    }

    #[test]
    fn a_working_directory_becomes_a_subshell_with_the_redirects_outside_it() {
        let line = Cmd::new("/usr/bin/wc")
            .flag("-l")
            .path("data.txt")
            .dir("/tmp/work")
            .stdout_to("out.txt")
            .stderr(Output::Inherit)
            .line();

        assert_eq!(line, "(cd /tmp/work && /usr/bin/wc -l data.txt) > out.txt");
    }

    #[test]
    fn a_line_says_where_each_stream_went() {
        let base = || Cmd::new("/x").stderr(Output::Inherit);

        assert_eq!(base().line(), "/x > /dev/null");
        assert_eq!(base().stdout(Output::Inherit).line(), "/x");
        assert_eq!(base().stdout_to("o").line(), "/x > o");
        assert_eq!(base().stdout(Output::Append("o".into())).line(), "/x >> o");
        assert_eq!(
            base().stdout(Output::OnFailure("o".into())).line(),
            "/x > o"
        );
        assert_eq!(
            Cmd::new("/x").stderr_to("e").line(),
            "/x > /dev/null 2> e",
            "stdout comes before stderr"
        );
    }

    #[test]
    fn a_label_falls_back_to_the_program_with_the_path_dropped() {
        assert_eq!(Cmd::new("/home/jack/tools/bin/nail").label(), "nail");
        assert_eq!(Cmd::new("mkdir").label(), "mkdir");
        assert_eq!(Cmd::new("/x").name("prep").label(), "prep");
    }
}
