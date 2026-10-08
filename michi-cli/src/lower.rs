//! The flat tree as the library's builders, with every attribute argument
//! typed here and nowhere earlier.

use std::path::PathBuf;
use std::time::Duration;

use michi::{
    Cmd, Headers, Marks, Memory, Mode, OnError, Output, Placement, Progress, Step, Stream, Table,
    When,
};

use crate::error::Error;
use crate::expand::{Flat, FlatArg, FlatAttr, FlatCommand, FlatPipeline, FlatStep, FlatValue};

/// A pipeline ready to run, under the name it was given in the file.
pub struct Built {
    pub name: String,
    pub pipeline: michi::Pipeline<'static>,
}

/// What the command line asks for beyond the file's own sinks.
#[derive(Debug, Default)]
pub struct Sinks {
    /// A default Progress sink when the file declares none, and the
    /// file's own when it does. Off, and neither.
    pub progress: bool,
    /// A Table sink writing here, which a file declaring its own may
    /// not combine with.
    pub table: Option<PathBuf>,
}

/// Build every pipeline in `flat`.
pub fn lower(flat: &Flat, sinks: &Sinks) -> Result<Vec<Built>, Error> {
    flat.pipelines.iter().map(|p| pipeline(p, sinks)).collect()
}

fn pipeline(flat: &FlatPipeline, sinks: &Sinks) -> Result<Built, Error> {
    let mut builder = michi::PipelineBuilder::new();
    let mut declared_progress = false;
    for attr in &flat.attrs {
        let args = Args::new(attr, &[]);
        builder = match attr.name.as_str() {
            "pool" => builder.pool(args.one()?.int()?),
            "placement" => builder.placement(
                args.one()?
                    .variant(&[("pack", Placement::Pack), ("spread", Placement::Spread)])?,
            ),
            "stderr_dir" => builder.stderr_dir(args.one()?.path()?),
            "no_stderr" => {
                args.none()?;
                builder.no_stderr()
            }
            "progress" => {
                declared_progress = true;
                let sink = progress(&args)?;
                if sinks.progress {
                    builder.sink(sink)
                } else {
                    builder
                }
            }
            "table" => {
                if sinks.table.is_some() {
                    return Err(Error::new(
                        format!(
                            "pipeline `{}` declares a table: drop `--table` or the attribute",
                            flat.name
                        ),
                        attr.span,
                    ));
                }
                builder.sink(table(&args)?)
            }
            _ => return Err(unknown(attr, "a pipeline", &[])),
        };
    }
    if sinks.progress && !declared_progress {
        builder = builder.sink(Progress::new());
    }
    if let Some(path) = &sinks.table {
        builder = builder.sink(Table::new(path));
    }
    for step in &flat.steps {
        builder = builder.step(lower_step(step)?);
    }
    let pipeline = builder.build().map_err(|err| build_error(err, flat))?;
    Ok(Built {
        name: flat.name.clone(),
        pipeline,
    })
}

/// A library error from building, pointed at the step it names when it
/// names one.
fn build_error(err: michi::Error, flat: &FlatPipeline) -> Error {
    let step = match &err {
        michi::Error::Cores { step, .. } => Some(step.as_str()),
        michi::Error::Pool {
            step: Some(step), ..
        } => Some(step.as_str()),
        _ => None,
    };
    // the library labels a step `[n](name)` with its one-based index
    let found = step
        .and_then(|label| label.strip_prefix('['))
        .and_then(|rest| rest.split_once(']'))
        .and_then(|(index, _)| index.parse::<usize>().ok())
        .and_then(|index| flat.steps.get(index.checked_sub(1)?));
    match found {
        Some(step) => with_bindings(Error::new(err.to_string(), step.span), &step.bindings),
        None => Error::new(err.to_string(), flat.span),
    }
}

fn progress(args: &Args<'_>) -> Result<Progress, Error> {
    let mut sink = Progress::new();
    for arg in args.attr.args.iter() {
        let value = Arg::new(arg, args.bindings);
        sink = match arg.key.as_deref() {
            Some("marks") => sink.marks(value.variant(&[
                ("unicode", Marks::Unicode),
                ("ascii", Marks::Ascii),
                ("none", Marks::None),
            ])?),
            Some("color") => sink.color(value.when()?),
            Some("rewrite") => sink.rewrite(value.when()?),
            Some("stream") => sink
                .stream(value.variant(&[("stdout", Stream::Stdout), ("stderr", Stream::Stderr)])?),
            Some(other) => return Err(value.err(format!("`progress` has no `{other}`"))),
            None => return Err(value.err("`progress` takes `key = value` arguments")),
        };
    }
    Ok(sink)
}

fn table(args: &Args<'_>) -> Result<Table, Error> {
    let mut path = None;
    let mut mode = None;
    for arg in args.attr.args.iter() {
        let value = Arg::new(arg, args.bindings);
        match arg.key.as_deref() {
            None if path.is_none() => path = Some(value.path()?),
            None => return Err(value.err("`table` takes one path")),
            Some("mode") => mode = Some(table_mode(&value)?),
            Some(other) => return Err(value.err(format!("`table` has no `{other}`"))),
        }
    }
    let Some(path) = path else {
        return Err(Error::new("`table` needs a path", args.attr.span));
    };
    let mut sink = Table::new(path);
    if let Some(mode) = mode {
        sink = sink.mode(mode);
    }
    Ok(sink)
}

fn table_mode(value: &Arg<'_>) -> Result<Mode, Error> {
    match &value.arg.value {
        FlatValue::Ident(s) if s == "whole" => Ok(Mode::Whole),
        FlatValue::Ident(s) if s == "ragged" => Ok(Mode::Ragged),
        FlatValue::Ident(s) if s == "blocks" => Ok(Mode::Blocks {
            headers: Headers::Once,
        }),
        FlatValue::Call(name, args) if name == "blocks" => {
            let mut headers = None;
            for (key, v) in args {
                match (key.as_deref(), v) {
                    (Some("headers"), FlatValue::Ident(s)) => {
                        headers = Some(match s.as_str() {
                            "once" => Headers::Once,
                            "each" => Headers::Each,
                            _ => return Err(value.err("`headers` is once or each")),
                        });
                    }
                    _ => return Err(value.err("`blocks` takes `headers = once|each`")),
                }
            }
            Ok(Mode::Blocks {
                headers: headers.unwrap_or(Headers::Once),
            })
        }
        _ => Err(value.err("`mode` is whole, ragged or blocks(headers = …)")),
    }
}

fn lower_step(flat: &FlatStep) -> Result<Step<'static>, Error> {
    let cmds = flat
        .commands
        .iter()
        .map(command)
        .collect::<Result<Vec<_>, _>>()?;
    let mut jobs = None;
    for attr in &flat.attrs {
        if attr.name == "jobs" {
            jobs = Some(Args::new(attr, &flat.bindings).one()?.int()?);
        }
    }
    let mut step = match jobs {
        Some(jobs) => Step::batched(jobs, cmds),
        None => Step::serial(cmds),
    };
    if !flat.label.is_empty() {
        step = step.name(&flat.label);
    }
    for attr in &flat.attrs {
        let args = Args::new(attr, &flat.bindings);
        step = match attr.name.as_str() {
            "jobs" => step,
            "on_error" => step.on_error(args.one()?.variant(&[
                ("continue", OnError::Continue),
                ("skip", OnError::Skip),
                ("abort", OnError::Abort),
            ])?),
            "cores" => step.cores(args.one()?.int()?),
            "memory" => step.memory(args.one()?.memory()?),
            "pool" => step.pool(args.one()?.int()?),
            _ => return Err(unknown(attr, "a step", &flat.bindings)),
        };
    }
    Ok(step)
}

fn command(flat: &FlatCommand) -> Result<Cmd, Error> {
    let mut cmd = Cmd::new("/bin/sh")
        .arg("-c", flat.text.as_str())
        .name(&flat.label);
    for attr in &flat.attrs {
        let args = Args::new(attr, &flat.bindings);
        cmd = match attr.name.as_str() {
            "name" => cmd.name(args.one()?.string()?),
            "cores" => cmd.cores(args.one()?.int()?),
            "memory" => cmd.memory(args.one()?.memory()?),
            "dir" => cmd.dir(args.one()?.path()?),
            "timeout" => cmd.timeout(args.one()?.duration()?),
            "stdout" => cmd.stdout(args.one()?.output()?),
            "stderr" => cmd.stderr(args.one()?.output()?),
            "stdout_to" => cmd.stdout_to(args.one()?.path()?),
            "stderr_to" => cmd.stderr_to(args.one()?.path()?),
            "tag" => cmd.tag(args.one()?.string()?),
            "env" => {
                let mut cmd = cmd;
                for arg in &attr.args {
                    let value = Arg::new(arg, &flat.bindings);
                    let Some(key) = &arg.key else {
                        return Err(value.err("`env` takes `KEY = value` arguments"));
                    };
                    cmd = cmd.env(key, value.text()?);
                }
                cmd
            }
            "field" => {
                let mut cmd = cmd;
                for arg in &attr.args {
                    let value = Arg::new(arg, &flat.bindings);
                    let Some(key) = &arg.key else {
                        return Err(
                            value.err("`field` takes `key = value` arguments, or a bare `$N`")
                        );
                    };
                    cmd = cmd.field(key, value.text()?);
                }
                cmd
            }
            "sub" => {
                return Err(with_bindings(
                    Error::new("`sub` has no meaning for a shell command", attr.span),
                    &flat.bindings,
                ));
            }
            _ => return Err(unknown(attr, "a command", &flat.bindings)),
        };
    }
    Ok(cmd)
}

fn unknown(attr: &FlatAttr, what: &str, bindings: &[(String, String)]) -> Error {
    with_bindings(
        Error::new(
            format!("`{}` is not an attribute of {what}", attr.name),
            attr.span,
        ),
        bindings,
    )
}

// ---

/// An attribute's arguments, with the bindings its item was made under
/// for the error messages.
struct Args<'a> {
    attr: &'a FlatAttr,
    bindings: &'a [(String, String)],
}

impl<'a> Args<'a> {
    fn new(attr: &'a FlatAttr, bindings: &'a [(String, String)]) -> Args<'a> {
        Args { attr, bindings }
    }

    fn one(&self) -> Result<Arg<'a>, Error> {
        match self.attr.args.as_slice() {
            [arg] if arg.key.is_none() => Ok(Arg::new(arg, self.bindings)),
            _ => Err(self.err(format!("`{}` takes one argument", self.attr.name))),
        }
    }

    fn none(&self) -> Result<(), Error> {
        if self.attr.args.is_empty() {
            Ok(())
        } else {
            Err(self.err(format!("`{}` takes no arguments", self.attr.name)))
        }
    }

    fn err(&self, message: impl Into<String>) -> Error {
        with_bindings(Error::new(message, self.attr.span), self.bindings)
    }
}

struct Arg<'a> {
    arg: &'a FlatArg,
    bindings: &'a [(String, String)],
}

impl<'a> Arg<'a> {
    fn new(arg: &'a FlatArg, bindings: &'a [(String, String)]) -> Arg<'a> {
        Arg { arg, bindings }
    }

    fn err(&self, message: impl Into<String>) -> Error {
        with_bindings(Error::new(message, self.arg.span), self.bindings)
    }

    fn int(&self) -> Result<usize, Error> {
        match self.arg.value {
            FlatValue::Int(n) if n >= 0 => Ok(n as usize),
            _ => Err(self.err("expected a whole number")),
        }
    }

    fn duration(&self) -> Result<Duration, Error> {
        match self.arg.value {
            FlatValue::Duration { value, .. } => Ok(value),
            _ => Err(self.err("expected a duration, such as 30s or 5m")),
        }
    }

    /// A string, or an identifier, or a number, as text.
    fn string(&self) -> Result<String, Error> {
        match &self.arg.value {
            FlatValue::Str(_)
            | FlatValue::Ident(_)
            | FlatValue::Int(_)
            | FlatValue::Float { .. } => Ok(self.arg.value.render()),
            _ => Err(self.err("expected a string")),
        }
    }

    /// A scalar as text, for env and field.
    fn text(&self) -> Result<String, Error> {
        match &self.arg.value {
            FlatValue::List(_) | FlatValue::Tuple(_) | FlatValue::Call(..) => {
                Err(self.err("expected a single value"))
            }
            _ => Ok(self.arg.value.render()),
        }
    }

    fn path(&self) -> Result<PathBuf, Error> {
        Ok(PathBuf::from(self.string()?))
    }

    fn variant<T: Copy>(&self, choices: &[(&str, T)]) -> Result<T, Error> {
        if let FlatValue::Ident(s) = &self.arg.value
            && let Some((_, v)) = choices.iter().find(|(name, _)| name == s)
        {
            return Ok(*v);
        }
        let names: Vec<&str> = choices.iter().map(|(name, _)| *name).collect();
        Err(self.err(format!("expected one of {}", names.join(", "))))
    }

    fn when(&self) -> Result<When, Error> {
        self.variant(&[
            ("auto", When::Auto),
            ("always", When::Always),
            ("never", When::Never),
        ])
    }

    fn memory(&self) -> Result<Memory, Error> {
        self.variant(&[
            ("preferred", Memory::Preferred),
            ("bound", Memory::Bound),
            ("first_touch", Memory::FirstTouch),
        ])
    }

    fn output(&self) -> Result<Output, Error> {
        let path_of = |args: &[(Option<String>, FlatValue)]| -> Result<PathBuf, Error> {
            match args {
                [(None, v @ (FlatValue::Str(_) | FlatValue::Ident(_) | FlatValue::Int(_)))] => {
                    Ok(PathBuf::from(v.render()))
                }
                _ => Err(self.err("expected one path")),
            }
        };
        match &self.arg.value {
            FlatValue::Ident(s) if s == "null" => Ok(Output::Null),
            FlatValue::Ident(s) if s == "inherit" => Ok(Output::Inherit),
            FlatValue::Call(name, args) if name == "file" => Ok(Output::File(path_of(args)?)),
            FlatValue::Call(name, args) if name == "append" => Ok(Output::Append(path_of(args)?)),
            FlatValue::Call(name, args) if name == "on_failure" => {
                Ok(Output::OnFailure(path_of(args)?))
            }
            _ => {
                Err(self
                    .err("expected null, inherit, file(\"p\"), append(\"p\") or on_failure(\"p\")"))
            }
        }
    }
}

fn with_bindings(mut error: Error, bindings: &[(String, String)]) -> Error {
    error.bindings = bindings.to_vec();
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expand::expand;
    use crate::parse::parse;
    use crate::resolve::resolve;

    fn built(src: &str) -> Result<Vec<Built>, Error> {
        let mut file = parse(src).unwrap();
        resolve(&mut file).unwrap();
        lower(&expand(&file).unwrap(), &Sinks::default())
    }

    fn err(src: &str) -> Error {
        built(src).err().expect("lowering fails")
    }

    #[test]
    fn every_attribute_lowers() {
        let src = r#"
            #[pool(2), placement(pack), stderr_dir("err"), progress(marks = ascii, color = never, rewrite = auto, stream = stderr)]
            #[table("t.tbl", mode = blocks(headers = each)), no_stderr]
            pipeline p {
                #[jobs(2), on_error(skip), cores(1), memory(first_touch)]
                step s {
                    #[name("one"), cores(1), memory(bound), dir("."), timeout(5s), stdout(null), stderr(file("e")), tag("t")]
                    #[env(A = 1, B = "x"), field(k = v, n = 2)]
                    true;
                    #[stdout_to("o"), stderr_to("e2"), stdout(append("a")), stderr(on_failure("f"))]
                    true;
                }
                step t { true; }
            }
        "#;
        let built = built(src).unwrap();
        assert_eq!(built.len(), 1);
    }

    #[test]
    fn errors_carry_the_bindings() {
        let e = err("pipeline p { <T = [1, a]> { #[cores($T)] step s { true; } } }");
        assert_eq!(e.message, "expected a whole number");
        assert_eq!(e.bindings, [("T".to_string(), "a".to_string())]);
    }

    #[test]
    fn unknown_attributes_carry_the_bindings_too() {
        let e = err("pipeline p { step s<N = [1]> { #[nope] true; } }");
        assert_eq!(e.message, "`nope` is not an attribute of a command");
        assert_eq!(e.bindings, [("N".to_string(), "1".to_string())]);
        assert!(built("#[table(\"t\", mode = blocks)] pipeline p { step s { true; } }").is_ok());
        assert!(built("pipeline p { step s<P = [o]> { #[stdout(file($P))] true; } }").is_ok());
        assert_eq!(
            err("pipeline p { step s { #[env(A = [1, 2])] true; } }").message,
            "expected a single value"
        );
    }

    #[test]
    fn library_errors_point_at_their_step() {
        let e = err("pipeline p { <N = [1]> { step s { #[cores(100000)] true; } } }");
        assert!(e.message.contains("cores"), "{}", e.message);
        assert_eq!(e.bindings, [("N".to_string(), "1".to_string())]);
    }

    #[test]
    fn sinks_from_the_command_line() {
        let src = "#[table(\"t\")] pipeline p { step s { true; } }";
        let mut file = parse(src).unwrap();
        resolve(&mut file).unwrap();
        let flat = expand(&file).unwrap();
        let sinks = Sinks {
            progress: true,
            table: Some(PathBuf::from("other")),
        };
        let e = lower(&flat, &sinks).err().expect("lowering fails");
        assert_eq!(
            e.message,
            "pipeline `p` declares a table: drop `--table` or the attribute"
        );
        let sinks = Sinks {
            progress: false,
            table: None,
        };
        assert!(lower(&flat, &sinks).is_ok());
    }

    #[test]
    fn wrong_item_and_unknown() {
        assert_eq!(
            err("pipeline p { #[timeout(1s)] step s { true; } }").message,
            "`timeout` is not an attribute of a step"
        );
        assert_eq!(
            err("pipeline p { step s { #[jobs(2)] true; } }").message,
            "`jobs` is not an attribute of a command"
        );
        assert_eq!(
            err("#[nope(1)] pipeline p { step s { true; } }").message,
            "`nope` is not an attribute of a pipeline"
        );
        assert_eq!(
            err("pipeline p { step s { #[sub(\"x\")] true; } }").message,
            "`sub` has no meaning for a shell command"
        );
        assert_eq!(
            err("pipeline p { #[placement(sideways)] step s { true; } }").message,
            "`placement` is not an attribute of a step"
        );
        assert_eq!(
            err("#[placement(sideways)] pipeline p { step s { true; } }").message,
            "expected one of pack, spread"
        );
        assert_eq!(
            err("#[table(\"t\", mode = sideways)] pipeline p { step s { true; } }").message,
            "`mode` is whole, ragged or blocks(headers = …)"
        );
        assert_eq!(
            err("pipeline p { step s { #[timeout(5)] true; } }").message,
            "expected a duration, such as 30s or 5m"
        );
        assert_eq!(
            err("pipeline p { step s { #[env(1)] true; } }").message,
            "`env` takes `KEY = value` arguments"
        );
    }
}
