//! The untyped tree a `.michi` file parses to.
//!
//! Nothing here knows which attributes exist or what their arguments
//! should be. That is settled after sweeps are expanded, so a `$T` can
//! stand where an integer will be needed.

use crate::error::Span;

/// An identifier with where it was written.
#[derive(Clone, Debug, PartialEq)]
pub struct Name {
    pub text: String,
    pub span: Span,
}

/// Text with the `$x` references picked out of it: a string value, a
/// command, or a raw string.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Text {
    pub parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Lit(String),
    /// `$x`, `${x}` or `${a.b}`, with whether the braces were written.
    Var {
        name: String,
        braced: bool,
    },
}

impl Text {
    /// The text as written, with every reference put back.
    pub fn source(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Lit(s) => out.push_str(s),
                Part::Var { name, braced: true } => {
                    out.push_str("${");
                    out.push_str(name);
                    out.push('}');
                }
                Part::Var {
                    name,
                    braced: false,
                } => {
                    out.push('$');
                    out.push_str(name);
                }
            }
        }
        out
    }
}

#[derive(Debug, PartialEq)]
pub struct File {
    pub items: Vec<Item>,
}

#[derive(Debug, PartialEq)]
pub enum Item {
    Data(Data),
    Pipeline(Pipeline),
}

#[derive(Debug, PartialEq)]
pub struct Data {
    pub name: Name,
    pub entries: Vec<Entry>,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub key: Name,
    pub value: Value,
}

#[derive(Debug, PartialEq)]
pub struct Pipeline {
    pub attrs: Vec<Attr>,
    pub name: Name,
    pub items: Vec<PipelineItem>,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub enum PipelineItem {
    Data(Data),
    Step(Step),
    Sweep(Sweep<PipelineItem>),
}

#[derive(Debug, PartialEq)]
pub struct Step {
    pub attrs: Vec<Attr>,
    pub name: Option<Name>,
    pub params: Option<Params>,
    pub body: Vec<StepItem>,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub enum StepItem {
    Command(Command),
    Sweep(Sweep<StepItem>),
}

#[derive(Debug, PartialEq)]
pub struct Command {
    pub attrs: Vec<Attr>,
    pub text: Text,
    /// Written as a raw string, so it reaches the shell untouched.
    pub raw: bool,
    pub span: Span,
}

/// A parameter list and the block it repeats.
#[derive(Debug, PartialEq)]
pub struct Sweep<T> {
    /// Attributes above the block, applied to each item directly
    /// inside it before the item's own.
    pub attrs: Vec<Attr>,
    pub params: Params,
    pub body: Vec<T>,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub struct Params {
    pub bindings: Vec<Binding>,
    pub span: Span,
}

/// One parameter and the list it ranges over.
#[derive(Debug, PartialEq)]
pub struct Binding {
    pub pattern: Pattern,
    pub list: Value,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub enum Pattern {
    One(Name),
    Tuple(Vec<Name>),
}

#[derive(Debug, PartialEq)]
pub struct Attr {
    pub name: Name,
    pub args: Vec<Arg>,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub struct Arg {
    pub key: Option<Name>,
    pub value: Value,
}

#[derive(Debug, PartialEq)]
pub struct Value {
    pub kind: ValueKind,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub enum ValueKind {
    Int(i64),
    /// A float with its text, which is how it prints.
    Float {
        value: f64,
        text: String,
    },
    Duration {
        value: std::time::Duration,
        text: String,
    },
    Ident(String),
    Str(Text),
    Var(String),
    /// `block.key`.
    Path(Name, Name),
    List(Vec<Elem>),
    Tuple(Vec<Value>),
    /// `name(args)`, as in `file("out.txt")`.
    Call(Name, Vec<Arg>),
}

#[derive(Debug, PartialEq)]
pub enum Elem {
    Value(Value),
    Range(Box<Range>),
}

#[derive(Debug, PartialEq)]
pub struct Range {
    pub start: Value,
    pub end: Value,
    pub inclusive: bool,
    pub step: Option<RangeStep>,
    pub span: Span,
}

#[derive(Debug, PartialEq)]
pub enum RangeStep {
    Add(Value),
    Mul(Value),
}
