//! Recursive descent from tokens to the tree in [`crate::ast`].

use crate::ast::*;
use crate::error::{Error, Span};
use crate::lex::{Keyword, Lexer, Tok, Token, split_vars};

/// Parse a whole file.
pub fn parse(src: &str) -> Result<File, Error> {
    Parser::new(src).file()
}

struct Parser<'a> {
    lex: Lexer<'a>,
    peeked: Option<Token>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Parser<'a> {
        Parser {
            lex: Lexer::new(src),
            peeked: None,
        }
    }

    fn peek(&mut self) -> Result<&Token, Error> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lex.next_token()?);
        }
        Ok(self.peeked.as_ref().unwrap())
    }

    fn bump(&mut self) -> Result<Token, Error> {
        self.peek()?;
        Ok(self.peeked.take().unwrap())
    }

    fn at(&mut self, tok: &Tok) -> Result<bool, Error> {
        Ok(&self.peek()?.tok == tok)
    }

    // by bytes when nothing is peeked, so that a command
    // starting with `~` or `./` is never lexed as michi
    fn at_attr(&mut self) -> bool {
        match &self.peeked {
            Some(token) => token.tok == Tok::HashBracket,
            None => self.lex.at("#["),
        }
    }

    /// Forget a peeked token and put the reader back to its start.
    fn unpeek(&mut self) {
        if let Some(token) = self.peeked.take() {
            self.lex.rewind(token.span.start);
        }
    }

    fn eat(&mut self, tok: &Tok) -> Result<Option<Token>, Error> {
        if self.at(tok)? {
            Ok(Some(self.bump()?))
        } else {
            Ok(None)
        }
    }

    fn expect(&mut self, tok: &Tok) -> Result<Token, Error> {
        let next = self.peek()?;
        if next.tok == *tok {
            self.bump()
        } else {
            Err(Error::new(
                format!("expected {}, found {}", tok.describe(), next.tok.describe()),
                next.span,
            ))
        }
    }

    fn name(&mut self, what: &str) -> Result<Name, Error> {
        let next = self.bump()?;
        match next.tok {
            Tok::Ident(text) => Ok(Name {
                text,
                span: next.span,
            }),
            Tok::Keyword(kw) => Err(Error::new(
                format!("`{}` is a reserved word and cannot be {what}", kw.text()),
                next.span,
            )),
            other => Err(Error::new(
                format!("expected {what}, found {}", other.describe()),
                next.span,
            )),
        }
    }

    // ---

    fn file(&mut self) -> Result<File, Error> {
        let mut items = Vec::new();
        loop {
            let attrs = self.attrs()?;
            let next = self.peek()?;
            match next.tok {
                Tok::Keyword(Keyword::Data) => {
                    reject_attrs(&attrs, "a data block")?;
                    items.push(Item::Data(self.data()?));
                }
                Tok::Keyword(Keyword::Pipeline) => {
                    items.push(Item::Pipeline(self.pipeline(attrs)?));
                }
                Tok::Lt => {
                    return Err(Error::new(
                        "a sweep cannot sit at the top of the file: it may not wrap a \
                         pipeline, and a step belongs inside one",
                        next.span,
                    ));
                }
                Tok::Eof => {
                    reject_dangling(&attrs)?;
                    return Ok(File { items });
                }
                Tok::Keyword(Keyword::Step) => {
                    return Err(Error::new("a step belongs inside a pipeline", next.span));
                }
                _ => {
                    return Err(Error::new(
                        format!(
                            "expected `data` or `pipeline`, found {}",
                            next.tok.describe()
                        ),
                        next.span,
                    ));
                }
            }
        }
    }

    fn attrs(&mut self) -> Result<Vec<Attr>, Error> {
        let mut attrs = Vec::new();
        while self.at_attr() {
            self.bump()?;
            loop {
                let name = self.name("an attribute name")?;
                let (args, end) = if self.eat(&Tok::LParen)?.is_some() {
                    self.args()?
                } else {
                    (Vec::new(), name.span)
                };
                attrs.push(Attr {
                    span: name.span.to(end),
                    name,
                    args,
                });
                if self.eat(&Tok::Comma)?.is_none() || self.at(&Tok::RBracket)? {
                    break;
                }
            }
            self.expect(&Tok::RBracket)?;
        }
        Ok(attrs)
    }

    /// The arguments after a `(`, through the `)`, and that paren's span.
    fn args(&mut self) -> Result<(Vec<Arg>, Span), Error> {
        let mut args = Vec::new();
        if let Some(close) = self.eat(&Tok::RParen)? {
            return Ok((args, close.span));
        }
        loop {
            let arg = match self.peek()?.tok {
                Tok::Ident(_) => {
                    let name = self.name("an argument")?;
                    if self.eat(&Tok::Eq)?.is_some() {
                        Arg {
                            key: Some(name),
                            value: self.value()?,
                        }
                    } else {
                        Arg {
                            key: None,
                            value: self.value_after_ident(name)?,
                        }
                    }
                }
                _ => Arg {
                    key: None,
                    value: self.value()?,
                },
            };
            args.push(arg);
            if self.eat(&Tok::Comma)?.is_none() || self.at(&Tok::RParen)? {
                break;
            }
        }
        let close = self.expect(&Tok::RParen)?.span;
        Ok((args, close))
    }

    fn data(&mut self) -> Result<Data, Error> {
        let start = self.expect(&Tok::Keyword(Keyword::Data))?.span;
        let name = self.name("the data block's name")?;
        self.expect(&Tok::LBrace)?;
        let mut entries = Vec::new();
        loop {
            if let Some(close) = self.eat(&Tok::RBrace)? {
                return Ok(Data {
                    name,
                    entries,
                    span: start.to(close.span),
                });
            }
            let key = self.name("a data key")?;
            self.expect(&Tok::Eq)?;
            let value = self.value()?;
            self.expect(&Tok::Semi)?;
            entries.push(Entry { key, value });
        }
    }

    fn pipeline(&mut self, attrs: Vec<Attr>) -> Result<Pipeline, Error> {
        let start = self.expect(&Tok::Keyword(Keyword::Pipeline))?.span;
        let name = self.name("the pipeline's name")?;
        self.expect(&Tok::LBrace)?;
        let (items, close) = self.pipeline_items()?;
        Ok(Pipeline {
            attrs,
            name,
            items,
            span: start.to(close),
        })
    }

    /// Items through the closing `}`, and that brace's span.
    fn pipeline_items(&mut self) -> Result<(Vec<PipelineItem>, Span), Error> {
        let mut items = Vec::new();
        loop {
            let attrs = self.attrs()?;
            let next = self.peek()?;
            match next.tok {
                Tok::Keyword(Keyword::Data) => {
                    reject_attrs(&attrs, "a data block")?;
                    items.push(PipelineItem::Data(self.data()?));
                }
                Tok::Keyword(Keyword::Step) => {
                    items.push(PipelineItem::Step(self.step(attrs)?));
                }
                Tok::Lt => {
                    let start = next.span;
                    let params = self.params()?;
                    self.expect(&Tok::LBrace)?;
                    let (body, close) = self.pipeline_items()?;
                    items.push(PipelineItem::Sweep(Sweep {
                        attrs,
                        params,
                        body,
                        span: start.to(close),
                    }));
                }
                Tok::RBrace => {
                    reject_dangling(&attrs)?;
                    let close = self.bump()?.span;
                    return Ok((items, close));
                }
                Tok::Keyword(Keyword::Pipeline) => {
                    return Err(Error::new("a pipeline cannot hold a pipeline", next.span));
                }
                _ => {
                    return Err(Error::new(
                        format!(
                            "expected `step`, `data`, `<` or `}}`, found {}",
                            next.tok.describe()
                        ),
                        next.span,
                    ));
                }
            }
        }
    }

    fn step(&mut self, attrs: Vec<Attr>) -> Result<Step, Error> {
        let start = self.expect(&Tok::Keyword(Keyword::Step))?.span;
        let name = match self.peek()?.tok {
            Tok::Ident(_) | Tok::Keyword(_) => Some(self.name("the step's name")?),
            _ => None,
        };
        let params = if self.at(&Tok::Lt)? {
            Some(self.params()?)
        } else {
            None
        };
        self.expect(&Tok::LBrace)?;
        let (body, close) = self.step_items()?;
        Ok(Step {
            attrs,
            name,
            params,
            body,
            span: start.to(close),
        })
    }

    /// Items through the closing `}`, and that brace's span.
    //
    // a command's first word is never lexed as a michi token:
    // it may be `./run` or `~/x`, which the michi lexer rejects.
    // so the reader looks at the bytes to tell a command from
    // the few things that are not one
    fn step_items(&mut self) -> Result<(Vec<StepItem>, Span), Error> {
        let mut items = Vec::new();
        loop {
            let attrs = self.attrs()?;
            self.unpeek();
            self.lex.skip_trivia();
            let at = self.lex.pos();
            if self.lex.at("}") {
                reject_dangling(&attrs)?;
                let close = self.bump()?.span;
                return Ok((items, close));
            }
            if self.lex.at("<") {
                let start = self.bump()?.span;
                let params = self.params_after_lt(start)?;
                self.expect(&Tok::LBrace)?;
                let (body, close) = self.step_items()?;
                items.push(StepItem::Sweep(Sweep {
                    attrs,
                    params,
                    body,
                    span: start.to(close),
                }));
                continue;
            }
            for (word, what) in [
                ("step", "a step cannot hold a step"),
                ("data", "a data block belongs in a pipeline, not a step"),
                ("pipeline", "a pipeline cannot sit inside a step"),
            ] {
                if self.lex.at_statement(word) {
                    return Err(Error::new(what, Span::new(at, at + word.len())));
                }
            }
            if self.lex.at("r#") || self.lex.at("r\"") {
                let token = self.bump()?;
                if let Tok::RawStr(body) = token.tok {
                    self.expect(&Tok::Semi)?;
                    items.push(StepItem::Command(Command {
                        attrs,
                        text: split_vars(&body),
                        raw: true,
                        span: token.span,
                    }));
                    continue;
                }
                return Err(Error::new("expected a raw string", token.span));
            }
            if self.lex.at_end() {
                return Err(Error::new(
                    "expected a command or `}`, found the end of the file",
                    Span::new(at, at),
                ));
            }
            let (text, span) = self.lex.command(at)?;
            items.push(StepItem::Command(Command {
                attrs,
                text,
                raw: false,
                span,
            }));
        }
    }

    fn params(&mut self) -> Result<Params, Error> {
        let open = self.expect(&Tok::Lt)?.span;
        self.params_after_lt(open)
    }

    /// The rest of a parameter list once the `<` at `open` is consumed.
    fn params_after_lt(&mut self, open: Span) -> Result<Params, Error> {
        let start = open;
        let mut bindings = Vec::new();
        loop {
            bindings.push(self.binding()?);
            if self.eat(&Tok::Comma)?.is_none() || self.at(&Tok::Gt)? {
                break;
            }
        }
        let close = self.expect(&Tok::Gt)?.span;
        Ok(Params {
            bindings,
            span: start.to(close),
        })
    }

    fn binding(&mut self) -> Result<Binding, Error> {
        if let Some(open) = self.eat(&Tok::LParen)? {
            let start = open.span;
            let mut names = Vec::new();
            loop {
                names.push(self.name("a parameter name")?);
                if self.eat(&Tok::Comma)?.is_none() || self.at(&Tok::RParen)? {
                    break;
                }
            }
            self.expect(&Tok::RParen)?;
            self.expect(&Tok::Eq)?;
            let list = self.value()?;
            return Ok(Binding {
                span: start.to(list.span),
                pattern: Pattern::Tuple(names),
                list,
            });
        }
        let name = self.name("a parameter name")?;
        if self.eat(&Tok::Eq)?.is_some() {
            let list = self.value()?;
            return Ok(Binding {
                span: name.span.to(list.span),
                pattern: Pattern::One(name),
                list,
            });
        }
        if self.eat(&Tok::Dot)?.is_some() {
            let key = self.name("a data key")?;
            let span = name.span.to(key.span);
            return Ok(Binding {
                span,
                pattern: Pattern::One(key.clone()),
                list: Value {
                    kind: ValueKind::Path(name, key),
                    span,
                },
            });
        }
        Err(Error::new(
            format!(
                "`{}` needs a list: `<{} = […]>` or `<block.{}>`",
                name.text, name.text, name.text
            ),
            name.span,
        ))
    }

    // ---

    fn value(&mut self) -> Result<Value, Error> {
        let token = self.bump()?;
        let span = token.span;
        let kind = match token.tok {
            Tok::Int(n) => ValueKind::Int(n),
            Tok::Float { value, text } => ValueKind::Float { value, text },
            Tok::Duration { value, text } => ValueKind::Duration { value, text },
            Tok::Str(s) => ValueKind::Str(split_vars(&s)),
            Tok::Var(name) => ValueKind::Var(name),
            Tok::Ident(text) => {
                return self.value_after_ident(Name { text, span });
            }
            Tok::LBracket => return self.list(span),
            Tok::LParen => {
                let mut values = Vec::new();
                loop {
                    values.push(self.value()?);
                    if self.eat(&Tok::Comma)?.is_none() || self.at(&Tok::RParen)? {
                        break;
                    }
                }
                let close = self.expect(&Tok::RParen)?.span;
                return Ok(Value {
                    kind: ValueKind::Tuple(values),
                    span: span.to(close),
                });
            }
            Tok::Keyword(kw) => {
                return Err(Error::new(
                    format!("`{}` is a reserved word and cannot be a value", kw.text()),
                    span,
                ));
            }
            other => {
                return Err(Error::new(
                    format!("expected a value, found {}", other.describe()),
                    span,
                ));
            }
        };
        Ok(Value { kind, span })
    }

    /// A value that began with an identifier: the identifier itself, a
    /// `block.key` path, or a `name(args)` call.
    fn value_after_ident(&mut self, name: Name) -> Result<Value, Error> {
        if self.eat(&Tok::Dot)?.is_some() {
            let key = self.name("a data key")?;
            let span = name.span.to(key.span);
            return Ok(Value {
                kind: ValueKind::Path(name, key),
                span,
            });
        }
        if self.eat(&Tok::LParen)?.is_some() {
            let (args, end) = self.args()?;
            let span = name.span.to(end);
            return Ok(Value {
                kind: ValueKind::Call(name, args),
                span,
            });
        }
        Ok(Value {
            span: name.span,
            kind: ValueKind::Ident(name.text),
        })
    }

    /// The rest of a list once the `[` is consumed.
    fn list(&mut self, open: Span) -> Result<Value, Error> {
        let mut elems = Vec::new();
        loop {
            if let Some(close) = self.eat(&Tok::RBracket)? {
                return Ok(Value {
                    kind: ValueKind::List(elems),
                    span: open.to(close.span),
                });
            }
            let start = self.value()?;
            let inclusive = match self.peek()?.tok {
                Tok::DotDot => false,
                Tok::DotDotEq => true,
                _ => {
                    elems.push(Elem::Value(start));
                    if self.eat(&Tok::Comma)?.is_none() {
                        let close = self.expect(&Tok::RBracket)?.span;
                        return Ok(Value {
                            kind: ValueKind::List(elems),
                            span: open.to(close),
                        });
                    }
                    continue;
                }
            };
            self.bump()?;
            let end = self.value()?;
            let mut span = start.span.to(end.span);
            let step = if self.eat(&Tok::Colon)?.is_some() {
                let mul = self.eat(&Tok::Star)?.is_some();
                let by = self.value()?;
                span = span.to(by.span);
                Some(if mul {
                    RangeStep::Mul(by)
                } else {
                    RangeStep::Add(by)
                })
            } else {
                None
            };
            elems.push(Elem::Range(Box::new(Range {
                start,
                end,
                inclusive,
                step,
                span,
            })));
            if self.eat(&Tok::Comma)?.is_none() {
                let close = self.expect(&Tok::RBracket)?.span;
                return Ok(Value {
                    kind: ValueKind::List(elems),
                    span: open.to(close),
                });
            }
        }
    }
}

fn reject_attrs(attrs: &[Attr], what: &str) -> Result<(), Error> {
    match attrs.first() {
        Some(attr) => Err(Error::new(format!("{what} takes no attributes"), attr.span)),
        None => Ok(()),
    }
}

fn reject_dangling(attrs: &[Attr]) -> Result<(), Error> {
    match attrs.first() {
        Some(attr) => Err(Error::new("attributes with nothing below them", attr.span)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> String {
        parse(src).unwrap_err().message
    }

    fn ok(src: &str) -> File {
        match parse(src) {
            Ok(file) => file,
            Err(e) => panic!("{}", e.render("test", src)),
        }
    }

    #[test]
    fn a_small_pipeline() {
        let file = ok(
            "pipeline p {\n  data d { N = [1, 2..=4:*2]; }\n  #[jobs(2)]\n  step s<d.N> {\n    #[timeout(30s)]\n    seq 1 $N;\n    ~/run --x;\n    r#\"a; b\"#;\n  }\n}\n",
        );
        let Item::Pipeline(p) = &file.items[0] else {
            panic!()
        };
        let PipelineItem::Step(step) = &p.items[1] else {
            panic!()
        };
        assert_eq!(step.attrs[0].name.text, "jobs");
        assert_eq!(step.params.as_ref().unwrap().bindings.len(), 1);
        assert_eq!(step.body.len(), 3);
        let StepItem::Command(second) = &step.body[1] else {
            panic!()
        };
        assert_eq!(second.text.source(), "~/run --x");
    }

    #[test]
    fn shorthand_binding_names_the_key() {
        let file = ok("pipeline p { step s<quick.N, M = [1]> { a; } }");
        let Item::Pipeline(p) = &file.items[0] else {
            panic!()
        };
        let PipelineItem::Step(step) = &p.items[0] else {
            panic!()
        };
        let bindings = &step.params.as_ref().unwrap().bindings;
        assert_eq!(
            bindings[0].pattern,
            Pattern::One(Name {
                text: "N".into(),
                span: Span::new(26, 27)
            })
        );
        assert!(matches!(bindings[0].list.kind, ValueKind::Path(..)));
        assert!(matches!(bindings[1].list.kind, ValueKind::List(..)));
    }

    #[test]
    fn attributes_on_a_sweep_block() {
        let file = ok("pipeline p { step s { #[timeout(1m)] <T = [1]> { a $T; } } }");
        let Item::Pipeline(p) = &file.items[0] else {
            panic!()
        };
        let PipelineItem::Step(step) = &p.items[0] else {
            panic!()
        };
        let StepItem::Sweep(sweep) = &step.body[0] else {
            panic!()
        };
        assert_eq!(sweep.attrs.len(), 1);
        assert_eq!(sweep.body.len(), 1);
    }

    #[test]
    fn errors_say_what_was_expected() {
        assert_eq!(err("step s {}"), "a step belongs inside a pipeline");
        assert!(err("<T = [1]> { pipeline p {} }").starts_with("a sweep cannot sit at the top"));
        assert!(err("<N = [1]> { step s { a; } }").starts_with("a sweep cannot sit at the top"));
        assert_eq!(
            err("pipeline p { step s { a } }"),
            "command is missing its `;`"
        );
        assert_eq!(
            err("pipeline p { step s { step-run; data.sh; data/x; a } }"),
            "command is missing its `;`"
        );
        assert_eq!(
            ok("pipeline p { data d { N = [1, 2,]; } #[a(1,), b,] step s<(N, M) = d.N,> { a; } }")
                .items
                .len(),
            1
        );
        assert_eq!(
            err("pipeline p { step s { #[x] } }"),
            "attributes with nothing below them"
        );
        assert_eq!(err("#[x] data d {}"), "a data block takes no attributes");
        assert_eq!(
            err("pipeline step {}"),
            "`step` is a reserved word and cannot be the pipeline's name"
        );
        assert_eq!(
            err("pipeline p { step s<N> { a; } }"),
            "`N` needs a list: `<N = […]>` or `<block.N>`"
        );
        assert_eq!(
            err("pipeline p { step s { step t { a; } } }"),
            "a step cannot hold a step"
        );
        assert_eq!(
            err("pipeline p { step s { a; }"),
            "expected `step`, `data`, `<` or `}`, found the end of the file"
        );
        assert_eq!(
            err("pipeline p { data d { N = [1, 2; } }"),
            "expected `]`, found `;`"
        );
        assert_eq!(
            err("pipeline p { step s { 'a; } }"),
            "command has an unclosed quote, `(` or `{`"
        );
    }
}
