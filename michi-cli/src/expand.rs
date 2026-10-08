//! Sweeps applied: the resolved tree as a flat one, every `$N` replaced,
//! every item labelled, and the bindings it was made under kept for the
//! errors that come later.

use crate::ast::*;
use crate::error::{Error, Span};
use crate::resolve::Scope;

#[derive(Debug)]
pub struct Flat {
    pub pipelines: Vec<FlatPipeline>,
}

#[derive(Debug)]
pub struct FlatPipeline {
    pub name: String,
    pub attrs: Vec<FlatAttr>,
    pub steps: Vec<FlatStep>,
    pub span: Span,
}

#[derive(Debug)]
pub struct FlatStep {
    /// The name as written, with the instantiation when a sweep made it.
    pub label: String,
    pub attrs: Vec<FlatAttr>,
    pub commands: Vec<FlatCommand>,
    pub span: Span,
    pub bindings: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct FlatCommand {
    /// The command's first word, with the instantiation when a sweep
    /// made it.
    pub label: String,
    pub attrs: Vec<FlatAttr>,
    /// The text for `sh -c`, every reference replaced.
    pub text: String,
    pub span: Span,
    pub bindings: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct FlatAttr {
    pub name: String,
    pub args: Vec<FlatArg>,
    pub span: Span,
}

#[derive(Debug)]
pub struct FlatArg {
    pub key: Option<String>,
    pub value: FlatValue,
    pub span: Span,
}

/// A value with nothing left to look up.
#[derive(Debug, Clone, PartialEq)]
pub enum FlatValue {
    Int(i64),
    Float {
        value: f64,
        text: String,
    },
    Duration {
        value: std::time::Duration,
        text: String,
    },
    Ident(String),
    Str(String),
    List(Vec<FlatValue>),
    Tuple(Vec<FlatValue>),
    Call(String, Vec<(Option<String>, FlatValue)>),
}

impl FlatValue {
    /// The value as it reads when spliced into text.
    pub fn render(&self) -> String {
        match self {
            FlatValue::Int(n) => n.to_string(),
            FlatValue::Float { text, .. } | FlatValue::Duration { text, .. } => text.clone(),
            FlatValue::Ident(s) | FlatValue::Str(s) => s.clone(),
            FlatValue::List(values) => {
                let words: Vec<String> = values.iter().map(FlatValue::render).collect();
                format!("[{}]", words.join(", "))
            }
            FlatValue::Tuple(values) => {
                let words: Vec<String> = values.iter().map(FlatValue::render).collect();
                format!("({})", words.join(", "))
            }
            FlatValue::Call(name, args) => {
                let words: Vec<String> = args
                    .iter()
                    .map(|(key, value)| match key {
                        Some(key) => format!("{key} = {}", value.render()),
                        None => value.render(),
                    })
                    .collect();
                format!("{name}({})", words.join(", "))
            }
        }
    }
}

/// One sweep variable and the value it holds at this point.
#[derive(Clone, Debug)]
struct Binding {
    name: String,
    value: FlatValue,
}

/// Expand a resolved file.
pub fn expand(file: &File) -> Result<Flat, Error> {
    let mut pipelines = Vec::new();
    for item in &file.items {
        let Item::Pipeline(pipeline) = item else {
            continue;
        };
        let scope = Scope::new(file, pipeline);
        let ex = Expander {
            scope: &scope,
            made: std::cell::Cell::new(0),
        };
        let attrs = ex.attrs(&pipeline.attrs, &[])?;
        let mut steps = Vec::new();
        ex.pipeline_items(&pipeline.items, &mut Vec::new(), &[], &mut steps)?;
        pipelines.push(FlatPipeline {
            name: pipeline.name.text.clone(),
            attrs,
            steps,
            span: pipeline.span,
        });
    }
    Ok(Flat { pipelines })
}

struct Expander<'a> {
    scope: &'a Scope<'a>,
    /// Steps and commands made so far.
    made: std::cell::Cell<usize>,
}

/// The most steps and commands one file may expand to.
const MOST_ITEMS: usize = 1_000_000;

/// One parameter list's axis: the names a point sets and the values it
/// draws them from, several names per value for a tuple pattern.
struct Axis {
    names: Vec<String>,
    values: Vec<FlatValue>,
}

/// The product of some axes, walked like an odometer with the last axis
/// turning fastest, one point at a time.
struct Points {
    axes: Vec<Axis>,
    index: Vec<usize>,
    done: bool,
}

impl Points {
    fn new(axes: Vec<Axis>) -> Points {
        let done = axes.iter().any(|axis| axis.values.is_empty());
        Points {
            index: vec![0; axes.len()],
            axes,
            done,
        }
    }
}

impl Iterator for Points {
    type Item = Vec<Binding>;

    fn next(&mut self) -> Option<Vec<Binding>> {
        if self.done {
            return None;
        }
        let mut point = Vec::new();
        for (axis, &i) in self.axes.iter().zip(&self.index) {
            let value = &axis.values[i];
            match (axis.names.as_slice(), value) {
                ([name], _) => point.push(Binding {
                    name: name.clone(),
                    value: value.clone(),
                }),
                (names, FlatValue::Tuple(items)) => {
                    for (name, item) in names.iter().zip(items) {
                        point.push(Binding {
                            name: name.clone(),
                            value: item.clone(),
                        });
                    }
                }
                _ => unreachable!("resolve checked the pattern"),
            }
        }

        // advance, carrying from the last axis down to the first
        let mut k = self.axes.len();
        loop {
            if k == 0 {
                self.done = true;
                break;
            }
            k -= 1;
            self.index[k] += 1;
            if self.index[k] < self.axes[k].values.len() {
                break;
            }
            self.index[k] = 0;
        }
        Some(point)
    }
}

impl Expander<'_> {
    /// `bound` are the sweep variables in force, outermost first.
    /// `inherited` are the attributes of the sweep blocks around these
    /// items. Steps made here go onto `out`.
    fn pipeline_items(
        &self,
        items: &[PipelineItem],
        bound: &mut Vec<Binding>,
        inherited: &[Attr],
        out: &mut Vec<FlatStep>,
    ) -> Result<(), Error> {
        for item in items {
            match item {
                PipelineItem::Data(_) => {}
                PipelineItem::Step(step) => {
                    self.made(step.span, bound)?;
                    out.push(self.step(step, bound, inherited)?);
                }
                PipelineItem::Sweep(sweep) => {
                    let merged = merge(inherited, &sweep.attrs);
                    let depth = bound.len();
                    for point in self.points(&sweep.params) {
                        bound.truncate(depth);
                        bound.extend(point);
                        self.pipeline_items(&sweep.body, bound, &merged, out)?;
                    }
                    bound.truncate(depth);
                }
            }
        }
        Ok(())
    }

    /// `bound` here holds only the pipeline-level sweeps, which label
    /// the step. The step's own parameters label its commands.
    fn step(
        &self,
        step: &Step,
        bound: &mut Vec<Binding>,
        inherited: &[Attr],
    ) -> Result<FlatStep, Error> {
        let merged = merge(inherited, &step.attrs);
        let attrs = self.attrs(&merged, bound)?;
        let name = step.name.as_ref().map_or("", |n| n.text.as_str());
        let label = format!("{name}{}", instantiation(bound));
        let bindings = pairs(bound);

        let mut commands = Vec::new();
        let outer = bound.len();
        match &step.params {
            Some(params) => {
                for point in self.points(params) {
                    bound.truncate(outer);
                    bound.extend(point);
                    self.step_items(&step.body, bound, outer, &[], &mut commands)?;
                }
            }
            None => self.step_items(&step.body, bound, outer, &[], &mut commands)?,
        }
        bound.truncate(outer);
        Ok(FlatStep {
            label,
            attrs,
            commands,
            span: step.span,
            bindings,
        })
    }

    /// `outer` is how many of `bound` belong to the pipeline level, so
    /// that a command's label shows the rest.
    fn step_items(
        &self,
        items: &[StepItem],
        bound: &mut Vec<Binding>,
        outer: usize,
        inherited: &[Attr],
        out: &mut Vec<FlatCommand>,
    ) -> Result<(), Error> {
        for item in items {
            match item {
                StepItem::Command(command) => {
                    let merged = merge(inherited, &command.attrs);
                    let attrs = self.attrs(&merged, bound)?;
                    let text = self.text(&command.text, bound);
                    let first = text.split_whitespace().next().unwrap_or("").to_string();
                    let label = format!("{first}{}", instantiation(&bound[outer..]));
                    self.made(command.span, bound)?;
                    out.push(FlatCommand {
                        label,
                        attrs,
                        text,
                        span: command.span,
                        bindings: pairs(bound),
                    });
                }
                StepItem::Sweep(sweep) => {
                    let merged = merge(inherited, &sweep.attrs);
                    let depth = bound.len();
                    for point in self.points(&sweep.params) {
                        bound.truncate(depth);
                        bound.extend(point);
                        self.step_items(&sweep.body, bound, outer, &merged, out)?;
                    }
                    bound.truncate(depth);
                }
            }
        }
        Ok(())
    }

    /// The points of a parameter list, first parameter outermost.
    fn points(&self, params: &Params) -> Points {
        let axes = params
            .bindings
            .iter()
            .map(|binding| {
                let list = match &binding.list.kind {
                    ValueKind::Path(block, key) => self
                        .scope
                        .lookup(&block.text, &key.text)
                        .expect("resolve checked the reference"),
                    _ => &binding.list,
                };
                let ValueKind::List(elems) = &list.kind else {
                    unreachable!("resolve checked the list");
                };
                let values: Vec<FlatValue> = elems
                    .iter()
                    .map(|elem| match elem {
                        Elem::Value(v) => literal(v),
                        Elem::Range(_) => unreachable!("resolve evaluated the ranges"),
                    })
                    .collect();
                let names: Vec<String> = match &binding.pattern {
                    Pattern::One(name) => vec![name.text.clone()],
                    Pattern::Tuple(names) => names.iter().map(|n| n.text.clone()).collect(),
                };
                Axis { names, values }
            })
            .collect();
        Points::new(axes)
    }

    /// One more step or command made. The file may not expand past
    /// [`MOST_ITEMS`], which a product of long lists would.
    fn made(&self, span: Span, bound: &[Binding]) -> Result<(), Error> {
        let n = self.made.get() + 1;
        self.made.set(n);
        if n > MOST_ITEMS {
            let mut error = Error::new(
                format!("the file expands to more than {MOST_ITEMS} steps and commands"),
                span,
            );
            error.bindings = pairs(bound);
            return Err(error);
        }
        Ok(())
    }

    fn attrs(&self, attrs: &[Attr], bound: &[Binding]) -> Result<Vec<FlatAttr>, Error> {
        attrs
            .iter()
            .map(|attr| {
                let args = attr
                    .args
                    .iter()
                    .map(|arg| {
                        let key = arg.key.as_ref().map(|k| k.text.clone());

                        // a bare `$N` in `field` records N under its
                        // own name
                        let key = match (&key, &arg.value.kind) {
                            (None, ValueKind::Var(name)) if attr.name.text == "field" => {
                                Some(name.clone())
                            }
                            _ => key,
                        };
                        Ok(FlatArg {
                            key,
                            value: self.value(&arg.value, bound)?,
                            span: arg.value.span,
                        })
                    })
                    .collect::<Result<Vec<_>, Error>>()?;
                Ok(FlatAttr {
                    name: attr.name.text.clone(),
                    args,
                    span: attr.span,
                })
            })
            .collect()
    }

    fn value(&self, value: &Value, bound: &[Binding]) -> Result<FlatValue, Error> {
        Ok(match &value.kind {
            ValueKind::Int(n) => FlatValue::Int(*n),
            ValueKind::Float { value, text } => FlatValue::Float {
                value: *value,
                text: text.clone(),
            },
            ValueKind::Duration { value, text } => FlatValue::Duration {
                value: *value,
                text: text.clone(),
            },
            ValueKind::Ident(s) => FlatValue::Ident(s.clone()),
            ValueKind::Str(text) => FlatValue::Str(self.text(text, bound)),
            ValueKind::Var(name) => bound
                .iter()
                .rev()
                .find(|b| b.name == *name)
                .map(|b| b.value.clone())
                .ok_or_else(|| {
                    Error::new(
                        format!("`${name}` is not bound by a sweep here"),
                        value.span,
                    )
                })?,
            ValueKind::Path(block, key) => literal(
                self.scope
                    .lookup(&block.text, &key.text)
                    .expect("resolve checked the reference"),
            ),
            ValueKind::List(elems) => FlatValue::List(
                elems
                    .iter()
                    .map(|elem| match elem {
                        Elem::Value(v) => self.value(v, bound),
                        Elem::Range(_) => unreachable!("resolve evaluated the ranges"),
                    })
                    .collect::<Result<_, _>>()?,
            ),
            ValueKind::Tuple(values) => FlatValue::Tuple(
                values
                    .iter()
                    .map(|v| self.value(v, bound))
                    .collect::<Result<_, _>>()?,
            ),
            ValueKind::Call(name, args) => FlatValue::Call(
                name.text.clone(),
                args.iter()
                    .map(|arg| {
                        Ok((
                            arg.key.as_ref().map(|k| k.text.clone()),
                            self.value(&arg.value, bound)?,
                        ))
                    })
                    .collect::<Result<_, Error>>()?,
            ),
        })
    }

    /// Text with every bound `$N` and every `${block.KEY}` replaced. Any
    /// other `$x` stays for the shell.
    fn text(&self, text: &Text, bound: &[Binding]) -> String {
        let mut out = String::new();
        for part in &text.parts {
            match part {
                Part::Lit(s) => out.push_str(s),
                Part::Var { name, braced } => {
                    if let Some(b) = bound.iter().rev().find(|b| b.name == *name) {
                        out.push_str(&b.value.render());
                    } else if let Some((block, key)) = name.split_once('.')
                        && let Some(target) = self.scope.lookup(block, key)
                    {
                        out.push_str(&plain(target));
                    } else if *braced {
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    } else {
                        out.push('$');
                        out.push_str(name);
                    }
                }
            }
        }
        out
    }
}

/// A literal data value as text.
fn plain(value: &Value) -> String {
    literal(value).render()
}

/// A data value or a list element, which resolve made sure holds no
/// reference. A string in it stays as written, `$x` and all.
fn literal(value: &Value) -> FlatValue {
    match &value.kind {
        ValueKind::Int(n) => FlatValue::Int(*n),
        ValueKind::Float { value, text } => FlatValue::Float {
            value: *value,
            text: text.clone(),
        },
        ValueKind::Duration { value, text } => FlatValue::Duration {
            value: *value,
            text: text.clone(),
        },
        ValueKind::Ident(s) => FlatValue::Ident(s.clone()),
        ValueKind::Str(t) => FlatValue::Str(t.source()),
        ValueKind::List(elems) => FlatValue::List(
            elems
                .iter()
                .map(|e| match e {
                    Elem::Value(v) => literal(v),
                    Elem::Range(_) => unreachable!("resolve evaluated the ranges"),
                })
                .collect(),
        ),
        ValueKind::Tuple(values) => FlatValue::Tuple(values.iter().map(literal).collect()),
        ValueKind::Var(_) | ValueKind::Path(..) | ValueKind::Call(..) => {
            unreachable!("resolve allowed literals only")
        }
    }
}

/// `outer` then `inner`, so an item's own attributes apply last.
fn merge(outer: &[Attr], inner: &[Attr]) -> Vec<Attr> {
    outer.iter().chain(inner).map(clone_attr).collect()
}

fn clone_attr(attr: &Attr) -> Attr {
    Attr {
        name: attr.name.clone(),
        args: attr
            .args
            .iter()
            .map(|arg| Arg {
                key: arg.key.clone(),
                value: clone_value(&arg.value),
            })
            .collect(),
        span: attr.span,
    }
}

fn clone_value(value: &Value) -> Value {
    let kind = match &value.kind {
        ValueKind::Int(n) => ValueKind::Int(*n),
        ValueKind::Float { value, text } => ValueKind::Float {
            value: *value,
            text: text.clone(),
        },
        ValueKind::Duration { value, text } => ValueKind::Duration {
            value: *value,
            text: text.clone(),
        },
        ValueKind::Ident(s) => ValueKind::Ident(s.clone()),
        ValueKind::Str(t) => ValueKind::Str(t.clone()),
        ValueKind::Var(s) => ValueKind::Var(s.clone()),
        ValueKind::Path(a, b) => ValueKind::Path(a.clone(), b.clone()),
        ValueKind::List(elems) => ValueKind::List(
            elems
                .iter()
                .map(|e| match e {
                    Elem::Value(v) => Elem::Value(clone_value(v)),
                    Elem::Range(_) => unreachable!("resolve evaluated the ranges"),
                })
                .collect(),
        ),
        ValueKind::Tuple(values) => ValueKind::Tuple(values.iter().map(clone_value).collect()),
        ValueKind::Call(name, args) => ValueKind::Call(
            name.clone(),
            args.iter()
                .map(|arg| Arg {
                    key: arg.key.clone(),
                    value: clone_value(&arg.value),
                })
                .collect(),
        ),
    };
    Value {
        kind,
        span: value.span,
    }
}

/// `<5,a>` for the values an item is instantiated under, with no spaces,
/// since the table pads its columns with them. Nothing when there are
/// none.
fn instantiation(bound: &[Binding]) -> String {
    if bound.is_empty() {
        return String::new();
    }
    let values: Vec<String> = bound
        .iter()
        .map(|b| {
            b.value
                .render()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("_")
        })
        .collect();
    format!("<{}>", values.join(","))
}

fn pairs(bound: &[Binding]) -> Vec<(String, String)> {
    bound
        .iter()
        .map(|b| (b.name.clone(), b.value.render()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;
    use crate::resolve::resolve;

    fn flat(src: &str) -> Flat {
        let mut file = parse(src).unwrap();
        resolve(&mut file).unwrap();
        expand(&file).unwrap()
    }

    fn labels(src: &str) -> Vec<String> {
        let flat = flat(src);
        let mut out = Vec::new();
        for p in &flat.pipelines {
            for s in &p.steps {
                out.push(format!("step {}", s.label));
                for c in &s.commands {
                    out.push(format!("{}: {}", c.label, c.text));
                }
            }
        }
        out
    }

    #[test]
    fn products_and_tuples() {
        assert_eq!(
            labels("pipeline p { step s<N = [1, 2], K = [a, b]> { echo $N$K; } }"),
            [
                "step s",
                "echo<1,a>: echo 1a",
                "echo<1,b>: echo 1b",
                "echo<2,a>: echo 2a",
                "echo<2,b>: echo 2b",
            ]
        );
        assert_eq!(
            labels(
                "data d { P = [(1, x), (2, y)]; } pipeline p { step s<(N, L) = d.P> { echo $N-$L; } }"
            ),
            ["step s", "echo<1,x>: echo 1-x", "echo<2,y>: echo 2-y"]
        );
    }

    #[test]
    fn step_sweeps_label_steps_and_nest() {
        assert_eq!(
            labels("pipeline p { <T = [1, 2]> { step s { a $T; <K = [x]> { b $T$K; } } } }"),
            [
                "step s<1>",
                "a: a 1",
                "b<x>: b 1x",
                "step s<2>",
                "a: a 2",
                "b<x>: b 2x",
            ]
        );
    }

    #[test]
    fn references_in_text_and_attributes() {
        let flat = flat(
            "data cfg { ROWS = 10; } pipeline p { <T = [3]> { #[cores($T)] step s { #[name(\"x-$T\"), field($T)] seq ${cfg.ROWS} $T $HOME; } } }",
        );
        let step = &flat.pipelines[0].steps[0];
        assert_eq!(step.attrs[0].name, "cores");
        assert_eq!(step.attrs[0].args[0].value, FlatValue::Int(3));
        assert_eq!(step.bindings, [("T".to_string(), "3".to_string())]);
        let cmd = &step.commands[0];
        assert_eq!(cmd.text, "seq 10 3 $HOME");
        assert_eq!(cmd.attrs[0].args[0].value, FlatValue::Str("x-3".into()));
        assert_eq!(cmd.attrs[1].args[0].key.as_deref(), Some("T"));
        assert_eq!(cmd.attrs[1].args[0].value, FlatValue::Int(3));
    }

    #[test]
    fn labels_hold_no_spaces_and_data_strings_stay_literal() {
        assert_eq!(
            labels(
                "data d { E = \"x$T y\"; } pipeline p { step s<S = [\"a b\"], T = [1]> { echo $S ${d.E}; } }"
            ),
            ["step s", "echo<a_b,1>: echo a b x$T y"]
        );
        let flat =
            flat("data d { E = \"x$T\"; } pipeline p { <T = [1]> { step s { #[dir(d.E)] a; } } }");
        assert_eq!(
            flat.pipelines[0].steps[0].commands[0].attrs[0].args[0].value,
            FlatValue::Str("x$T".into())
        );
    }

    #[test]
    fn a_huge_product_is_refused() {
        let mut file =
            parse("data d { X = [0..2000]; } pipeline p { step s<A = d.X, B = d.X> { echo; } }")
                .unwrap();
        resolve(&mut file).unwrap();
        let err = expand(&file).unwrap_err();
        assert_eq!(
            err.message,
            "the file expands to more than 1000000 steps and commands"
        );
        assert_eq!(err.bindings.len(), 2);
    }

    #[test]
    fn inherited_attributes_come_first() {
        let flat = flat("pipeline p { #[on_error(skip)] <T = [1]> { #[cores(2)] step s { a; } } }");
        let names: Vec<&str> = flat.pipelines[0].steps[0]
            .attrs
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, ["on_error", "cores"]);
    }
}
