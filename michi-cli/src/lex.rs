//! Tokens of michi syntax, and the scan of a command to its `;`.
//!
//! The two never share a reader. The parser asks for michi tokens until
//! it knows it is looking at a command, then asks for the command text
//! from that byte on.

use std::time::Duration;

use crate::ast::{Part, Text};
use crate::error::{Error, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Keyword(Keyword),
    Int(i64),
    Float {
        value: f64,
        text: String,
    },
    Duration {
        value: Duration,
        text: String,
    },
    Str(String),
    RawStr(String),
    Var(String),
    /// `#[`
    HashBracket,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Lt,
    Gt,
    Comma,
    Eq,
    Semi,
    Dot,
    DotDot,
    DotDotEq,
    Colon,
    Star,
    Eof,
}

/// A word no name may use. Only the first three mean anything yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyword {
    Data,
    Pipeline,
    Step,
    Let,
    Def,
    Fn,
    For,
    In,
    If,
    Else,
    Use,
    Import,
    Include,
    True,
    False,
}

impl Keyword {
    const ALL: [(Keyword, &'static str); 15] = [
        (Keyword::Data, "data"),
        (Keyword::Pipeline, "pipeline"),
        (Keyword::Step, "step"),
        (Keyword::Let, "let"),
        (Keyword::Def, "def"),
        (Keyword::Fn, "fn"),
        (Keyword::For, "for"),
        (Keyword::In, "in"),
        (Keyword::If, "if"),
        (Keyword::Else, "else"),
        (Keyword::Use, "use"),
        (Keyword::Import, "import"),
        (Keyword::Include, "include"),
        (Keyword::True, "true"),
        (Keyword::False, "false"),
    ];

    pub fn from_text(text: &str) -> Option<Keyword> {
        Keyword::ALL
            .iter()
            .find(|(_, word)| *word == text)
            .map(|(kw, _)| *kw)
    }

    pub fn text(self) -> &'static str {
        Keyword::ALL
            .iter()
            .find(|(kw, _)| *kw == self)
            .map(|(_, word)| *word)
            .unwrap_or("")
    }
}

impl Tok {
    /// How the token reads in an error message.
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Keyword(kw) => format!("`{}`", kw.text()),
            Tok::Int(n) => format!("`{n}`"),
            Tok::Float { text, .. } | Tok::Duration { text, .. } => format!("`{text}`"),
            Tok::Str(_) => "a string".into(),
            Tok::RawStr(_) => "a raw string".into(),
            Tok::Var(name) => format!("`${name}`"),
            Tok::HashBracket => "`#[`".into(),
            Tok::LBracket => "`[`".into(),
            Tok::RBracket => "`]`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::Lt => "`<`".into(),
            Tok::Gt => "`>`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Eq => "`=`".into(),
            Tok::Semi => "`;`".into(),
            Tok::Dot => "`.`".into(),
            Tok::DotDot => "`..`".into(),
            Tok::DotDotEq => "`..=`".into(),
            Tok::Colon => "`:`".into(),
            Tok::Star => "`*`".into(),
            Tok::Eof => "the end of the file".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

pub struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Lexer<'a> {
        let mut lexer = Lexer { src, pos: 0 };

        // a shebang is line one starting `#!` and not `#![`
        if src.starts_with("#!") && !src.starts_with("#![") {
            lexer.pos = src.find('\n').map_or(src.len(), |i| i + 1);
        }
        lexer
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Put the reader back to `pos`, which forgets a token lexed from
    /// there.
    pub fn rewind(&mut self, pos: usize) {
        self.pos = pos;
    }

    fn bytes(&self) -> &'a [u8] {
        self.src.as_bytes()
    }

    fn peek_byte(&self, ahead: usize) -> Option<u8> {
        self.bytes().get(self.pos + ahead).copied()
    }

    /// Skip whitespace and `//` comments.
    pub fn skip_trivia(&mut self) {
        loop {
            match self.peek_byte(0) {
                Some(b) if b.is_ascii_whitespace() => self.pos += 1,
                Some(b'/') if self.peek_byte(1) == Some(b'/') => {
                    self.pos = self.src[self.pos..]
                        .find('\n')
                        .map_or(self.src.len(), |i| self.pos + i);
                }
                _ => return,
            }
        }
    }

    /// Whether the text at the reader, after trivia, starts with `word`
    /// used as a statement: followed by whitespace, `{`, `<` or the end,
    /// so that `data-gen` and `step.sh` are not it.
    pub fn at_statement(&mut self, word: &str) -> bool {
        self.skip_trivia();
        let rest = &self.src[self.pos..];
        rest.starts_with(word)
            && rest[word.len()..]
                .bytes()
                .next()
                .is_none_or(|b| b.is_ascii_whitespace() || b == b'{' || b == b'<')
    }

    /// Whether nothing but trivia remains.
    pub fn at_end(&mut self) -> bool {
        self.skip_trivia();
        self.pos >= self.src.len()
    }

    /// Whether the text at the reader, after trivia, starts with `text`.
    pub fn at(&mut self, text: &str) -> bool {
        self.skip_trivia();
        self.src[self.pos..].starts_with(text)
    }

    pub fn next_token(&mut self) -> Result<Token, Error> {
        self.skip_trivia();
        let start = self.pos;
        let Some(b) = self.peek_byte(0) else {
            return Ok(Token {
                tok: Tok::Eof,
                span: Span::new(start, start),
            });
        };
        let simple = |lexer: &mut Lexer<'a>, tok: Tok, len: usize| {
            lexer.pos += len;
            Ok(Token {
                tok,
                span: Span::new(start, start + len),
            })
        };
        match b {
            b'#' if self.peek_byte(1) == Some(b'[') => simple(self, Tok::HashBracket, 2),
            b'#' => Err(Error::new(
                "expected `#[`, since `#` alone is not michi syntax",
                Span::new(start, start + 1),
            )),
            b'[' => simple(self, Tok::LBracket, 1),
            b']' => simple(self, Tok::RBracket, 1),
            b'{' => simple(self, Tok::LBrace, 1),
            b'}' => simple(self, Tok::RBrace, 1),
            b'(' => simple(self, Tok::LParen, 1),
            b')' => simple(self, Tok::RParen, 1),
            b'<' => simple(self, Tok::Lt, 1),
            b'>' => simple(self, Tok::Gt, 1),
            b',' => simple(self, Tok::Comma, 1),
            b'=' => simple(self, Tok::Eq, 1),
            b';' => simple(self, Tok::Semi, 1),
            b':' => simple(self, Tok::Colon, 1),
            b'*' => simple(self, Tok::Star, 1),
            b'.' if self.peek_byte(1) == Some(b'.') && self.peek_byte(2) == Some(b'=') => {
                simple(self, Tok::DotDotEq, 3)
            }
            b'.' if self.peek_byte(1) == Some(b'.') => simple(self, Tok::DotDot, 2),
            b'.' => simple(self, Tok::Dot, 1),
            b'"' => self.string(),
            b'r' if self.peek_byte(1) == Some(b'#') || self.peek_byte(1) == Some(b'"') => {
                self.raw_string()
            }
            b'$' => self.var(),
            b'-' if self.peek_byte(1).is_some_and(|b| b.is_ascii_digit()) => self.number(),
            b if b.is_ascii_digit() => self.number(),
            b if is_ident_start(b) => {
                let end = self.scan_while(is_ident_char);
                let text = &self.src[start..end];
                let tok = match Keyword::from_text(text) {
                    Some(kw) => Tok::Keyword(kw),
                    None => Tok::Ident(text.to_string()),
                };
                Ok(Token {
                    tok,
                    span: Span::new(start, end),
                })
            }
            _ => {
                let ch = self.src[start..].chars().next().unwrap_or('?');
                Err(Error::new(
                    format!("unexpected character `{ch}`"),
                    Span::new(start, start + ch.len_utf8()),
                ))
            }
        }
    }

    /// Advance over bytes matching `pred` and return the new position.
    fn scan_while(&mut self, pred: fn(u8) -> bool) -> usize {
        while self.peek_byte(0).is_some_and(pred) {
            self.pos += 1;
        }
        self.pos
    }

    fn string(&mut self) -> Result<Token, Error> {
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek_byte(0) {
                None => {
                    return Err(Error::new(
                        "unterminated string",
                        Span::new(start, self.src.len()),
                    ));
                }
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(Token {
                        tok: Tok::Str(out),
                        span: Span::new(start, self.pos),
                    });
                }
                // only the quote and the backslash are escapes.
                // any other pair stays as written, so `\$N`
                // reaches the shell with its backslash
                Some(b'\\') => match self.peek_byte(1) {
                    Some(b'"') => {
                        out.push('"');
                        self.pos += 2;
                    }
                    Some(b'\\') => {
                        out.push('\\');
                        self.pos += 2;
                    }
                    _ => {
                        out.push('\\');
                        self.pos += 1;
                    }
                },
                Some(_) => {
                    let ch = self.src[self.pos..].chars().next().unwrap();
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// `r"…"`, or `r#"…"#` with any number of hashes.
    fn raw_string(&mut self) -> Result<Token, Error> {
        let start = self.pos;
        self.pos += 1;
        let hashes = self.scan_while(|b| b == b'#') - start - 1;
        if self.peek_byte(0) != Some(b'"') {
            return Err(Error::new(
                "expected `\"` to open the raw string",
                Span::new(start, self.pos),
            ));
        }
        self.pos += 1;
        let body_start = self.pos;
        let close = format!("\"{}", "#".repeat(hashes));
        let Some(at) = self.src[body_start..].find(&close) else {
            return Err(Error::new(
                "unterminated raw string",
                Span::new(start, self.src.len()),
            ));
        };
        let body = self.src[body_start..body_start + at].to_string();
        self.pos = body_start + at + close.len();
        Ok(Token {
            tok: Tok::RawStr(body),
            span: Span::new(start, self.pos),
        })
    }

    fn var(&mut self) -> Result<Token, Error> {
        let start = self.pos;
        self.pos += 1;
        let braced = self.peek_byte(0) == Some(b'{');
        if braced {
            self.pos += 1;
        }
        let name_start = self.pos;
        if !self.peek_byte(0).is_some_and(is_ident_start) {
            return Err(Error::new(
                "expected a name after `$`",
                Span::new(start, self.pos),
            ));
        }
        let end = self.scan_while(is_ident_char);
        let name = self.src[name_start..end].to_string();
        if braced {
            if self.peek_byte(0) != Some(b'}') {
                return Err(Error::new(
                    "expected `}` to close `${`",
                    Span::new(start, self.pos),
                ));
            }
            self.pos += 1;
        }
        Ok(Token {
            tok: Tok::Var(name),
            span: Span::new(start, self.pos),
        })
    }

    fn number(&mut self) -> Result<Token, Error> {
        let start = self.pos;
        if self.peek_byte(0) == Some(b'-') {
            self.pos += 1;
        }
        self.scan_while(|b| b.is_ascii_digit());

        // a `.` is part of the number only when a digit
        // follows it, so `1..=16` lexes as `1` `..=` `16`
        let mut float = false;
        if self.peek_byte(0) == Some(b'.') && self.peek_byte(1).is_some_and(|b| b.is_ascii_digit())
        {
            float = true;
            self.pos += 1;
            self.scan_while(|b| b.is_ascii_digit());
        }
        let digits_end = self.pos;
        let unit_end = self.scan_while(|b| b.is_ascii_alphabetic());
        let text = self.src[start..unit_end].to_string();
        let span = Span::new(start, unit_end);
        let digits = &self.src[start..digits_end];
        let unit = &self.src[digits_end..unit_end];

        if unit.is_empty() {
            let tok = if float {
                Tok::Float {
                    value: digits.parse().map_err(|_| Error::new("bad number", span))?,
                    text,
                }
            } else {
                Tok::Int(
                    digits
                        .parse()
                        .map_err(|_| Error::new("integer is out of range", span))?,
                )
            };
            return Ok(Token { tok, span });
        }

        let per_unit = match unit {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => {
                return Err(Error::new(
                    format!("unknown unit `{unit}`, expected ms, s, m or h"),
                    span,
                ));
            }
        };
        let amount: f64 = digits.parse().map_err(|_| Error::new("bad number", span))?;
        if amount < 0.0 {
            return Err(Error::new("a duration cannot be negative", span));
        }
        let value = Duration::try_from_secs_f64(amount * per_unit)
            .map_err(|_| Error::new("duration is too long", span))?;
        Ok(Token {
            tok: Tok::Duration { value, text },
            span,
        })
    }

    // ---

    /// Scan a command from byte `start` to its `;`, leave the reader
    /// past the `;`, and return the text ready for the shell and the span
    /// of the command as written.
    pub fn command(&mut self, start: usize) -> Result<(Text, Span), Error> {
        let src = self.src;
        let bytes = src.as_bytes();
        let len = bytes.len();
        let mut i = start;
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let cmd_start = i;
        let mut out = String::new();
        let mut stack: Vec<Ctx> = Vec::new();

        // where the shell would start a word, so a `#` there is
        // a comment, and where a line starts, so a `#[` there is
        // michi's
        let mut word_start = true;
        let mut line_start = true;
        let mut commented = false;

        let missing = |out: &str, commented: bool, at: usize| {
            let what = if out.is_empty() && commented {
                "a `#` comment with no command: michi comments are `//`"
            } else {
                "command is missing its `;`"
            };
            Error::new(what, Span::new(cmd_start, at))
        };
        let char_len = |at: usize| src[at..].chars().next().map_or(1, char::len_utf8);

        // `n` bytes go to `out` as a slice, so a multi-byte
        // character is never split
        fn take(src: &str, out: &mut String, at: usize, n: usize) -> usize {
            out.push_str(&src[at..at + n]);
            at + n
        }

        loop {
            if i >= len {
                if !stack.is_empty() {
                    return Err(Error::new(
                        "command has an unclosed quote, `(` or `{`",
                        Span::new(cmd_start, len),
                    ));
                }
                return Err(missing(&out, commented, len));
            }
            let b = bytes[i];
            let next = bytes.get(i + 1).copied();
            let top = stack.last().copied();

            let n = match top {
                // quotes and backticks take their bytes whole
                Some(Ctx::Single) => {
                    if b == b'\'' {
                        stack.pop();
                    }
                    char_len(i)
                }
                Some(Ctx::Double) => match b {
                    b'\\' if next.is_some() => 1 + char_len(i + 1),
                    b'"' => {
                        stack.pop();
                        1
                    }
                    b'$' if next == Some(b'(') => {
                        stack.push(Ctx::Paren);
                        2
                    }
                    b'$' if next == Some(b'{') => {
                        stack.push(Ctx::Brace);
                        2
                    }
                    b'`' => {
                        stack.push(Ctx::Backtick);
                        1
                    }
                    _ => char_len(i),
                },
                Some(Ctx::Backtick) => match b {
                    b'\\' if next.is_some() => 1 + char_len(i + 1),
                    b'`' => {
                        stack.pop();
                        1
                    }
                    _ => char_len(i),
                },

                // plain text: at the top, or inside `$( )`, `( )`,
                // `${ }` or `{ }`, where a `;` is the shell's until
                // the group closes
                None | Some(Ctx::Paren) | Some(Ctx::Brace) | Some(Ctx::Group) => match b {
                    b';' if top.is_none() => break,
                    b'\'' => {
                        stack.push(Ctx::Single);
                        1
                    }
                    b'"' => {
                        stack.push(Ctx::Double);
                        1
                    }
                    b'`' => {
                        stack.push(Ctx::Backtick);
                        1
                    }
                    b'\\' if next.is_some() => 1 + char_len(i + 1),
                    b'$' if next == Some(b'(') => {
                        stack.push(Ctx::Paren);
                        2
                    }
                    b'$' if next == Some(b'{') => {
                        stack.push(Ctx::Brace);
                        2
                    }
                    b'(' => {
                        stack.push(Ctx::Paren);
                        1
                    }
                    b')' if top == Some(Ctx::Paren) => {
                        stack.pop();
                        1
                    }
                    b'{' => {
                        stack.push(Ctx::Group);
                        1
                    }
                    b'}' if matches!(top, Some(Ctx::Group | Ctx::Brace)) => {
                        stack.pop();
                        1
                    }

                    // a `}` with no `{` open, or a `#[` starting a
                    // line, is michi syntax: the `;` was left out
                    b'}' if top.is_none() => return Err(missing(&out, commented, i)),
                    b'#' if next == Some(b'[') && line_start && top.is_none() => {
                        return Err(missing(&out, commented, i));
                    }

                    // a `#` at the start of a word is a shell comment
                    // to the end of its physical line
                    b'#' if word_start => {
                        commented = true;
                        while i < len && bytes[i] != b'\n' {
                            i += 1;
                        }
                        continue;
                    }

                    // newlines never separate commands here: a
                    // wrapped command is one command, and the `;`
                    // is the only terminator
                    b if b.is_ascii_whitespace() => {
                        if !out.is_empty() && !out.ends_with(' ') {
                            out.push(' ');
                        }
                        word_start = true;
                        if b == b'\n' {
                            line_start = true;
                        }
                        i += 1;
                        continue;
                    }
                    _ => char_len(i),
                },
            };

            // a metacharacter ends a word too, so `|#c` is a comment
            word_start = matches!(top, None | Some(Ctx::Paren | Ctx::Brace | Ctx::Group))
                && matches!(b, b'|' | b'&' | b'(' | b')' | b'<' | b'>' | b'{' | b'}');
            line_start = false;
            i = take(src, &mut out, i, n);
        }
        let span = Span::new(cmd_start, i);
        self.pos = i + 1;

        // the trailing space the scan added, unless a backslash
        // escapes it
        if out.ends_with(' ') {
            let before = &out[..out.len() - 1];
            let slashes = before.len() - before.trim_end_matches('\\').len();
            if slashes.is_multiple_of(2) {
                out.pop();
            }
        }
        if out.is_empty() {
            return Err(missing(&out, commented, i + 1));
        }
        Ok((split_vars(&out), span))
    }
}

/// What the command scanner is inside of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctx {
    Single,
    Double,
    Backtick,
    /// `$(` or `(`
    Paren,
    /// `${`
    Brace,
    /// `{`
    Group,
}

/// Pick the `$x`, `${x}` and `${a.b}` references out of `text`. Any
/// other `$` stays literal, and `\$` keeps its backslash so the shell
/// sees it.
pub fn split_vars(text: &str) -> Text {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            let n = 1 + text[i + 1..].chars().next().map_or(0, char::len_utf8);
            lit.push_str(&text[i..i + n]);
            i += n;
            continue;
        }
        if b == b'$'
            && let Some((name, braced, len)) = var_at(text, i)
        {
            if !lit.is_empty() {
                parts.push(Part::Lit(std::mem::take(&mut lit)));
            }
            parts.push(Part::Var { name, braced });
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        lit.push(ch);
        i += ch.len_utf8();
    }
    if !lit.is_empty() {
        parts.push(Part::Lit(lit));
    }
    Text { parts }
}

/// The reference starting at the `$` at `at`, as its name, whether it
/// was braced, and its length in bytes. `None` when the `$` is the
/// shell's: `$(`, `$$`, `$1`, `${x:-y}` and the rest.
fn var_at(text: &str, at: usize) -> Option<(String, bool, usize)> {
    let bytes = text.as_bytes();
    let braced = bytes.get(at + 1) == Some(&b'{');
    let mut i = at + 1 + usize::from(braced);
    if !bytes.get(i).copied().is_some_and(is_ident_start) {
        return None;
    }
    let name_start = i;
    while bytes.get(i).copied().is_some_and(is_ident_char) {
        i += 1;
    }

    // a dotted path only inside braces, since `$a.b` in a
    // command is `$a` followed by `.b`
    if braced
        && bytes.get(i) == Some(&b'.')
        && bytes.get(i + 1).copied().is_some_and(is_ident_start)
    {
        i += 1;
        while bytes.get(i).copied().is_some_and(is_ident_char) {
            i += 1;
        }
    }
    let name = text[name_start..i].to_string();
    if braced {
        if bytes.get(i) != Some(&b'}') {
            return None;
        }
        i += 1;
    }
    Some((name, braced, i - at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        let mut lexer = Lexer::new(src);
        let mut out = Vec::new();
        loop {
            let token = lexer.next_token().unwrap();
            if token.tok == Tok::Eof {
                return out;
            }
            out.push(token.tok);
        }
    }

    fn command(src: &str) -> String {
        let mut lexer = Lexer::new(src);
        lexer.command(0).unwrap().0.source()
    }

    fn command_err(src: &str) -> String {
        let mut lexer = Lexer::new(src);
        lexer.command(0).unwrap_err().message
    }

    #[test]
    fn ranges_do_not_lex_as_floats() {
        assert_eq!(
            toks("1..=16:*2"),
            vec![
                Tok::Int(1),
                Tok::DotDotEq,
                Tok::Int(16),
                Tok::Colon,
                Tok::Star,
                Tok::Int(2)
            ]
        );
        assert_eq!(toks("0..5"), vec![Tok::Int(0), Tok::DotDot, Tok::Int(5)]);
    }

    #[test]
    fn floats_keep_their_text() {
        assert_eq!(
            toks("10.0"),
            vec![Tok::Float {
                value: 10.0,
                text: "10.0".into()
            }]
        );
    }

    #[test]
    fn durations() {
        assert_eq!(
            toks("30s"),
            vec![Tok::Duration {
                value: Duration::from_secs(30),
                text: "30s".into()
            }]
        );
        assert_eq!(
            toks("500ms"),
            vec![Tok::Duration {
                value: Duration::from_millis(500),
                text: "500ms".into()
            }]
        );
        let err = Lexer::new("3x").next_token().unwrap_err();
        assert!(err.message.contains("unknown unit `x`"));
    }

    #[test]
    fn keywords_and_idents() {
        assert_eq!(
            toks("step burn data"),
            vec![
                Tok::Keyword(Keyword::Step),
                Tok::Ident("burn".into()),
                Tok::Keyword(Keyword::Data)
            ]
        );
    }

    #[test]
    fn vars_strings_and_punctuation() {
        assert_eq!(
            toks("#[name(\"burn-$N\"), env(N = $N)]"),
            vec![
                Tok::HashBracket,
                Tok::Ident("name".into()),
                Tok::LParen,
                Tok::Str("burn-$N".into()),
                Tok::RParen,
                Tok::Comma,
                Tok::Ident("env".into()),
                Tok::LParen,
                Tok::Ident("N".into()),
                Tok::Eq,
                Tok::Var("N".into()),
                Tok::RParen,
                Tok::RBracket,
            ]
        );
        assert_eq!(toks("${N}"), vec![Tok::Var("N".into())]);
    }

    #[test]
    fn strings_keep_unknown_escapes() {
        assert_eq!(
            toks(r#""a\$N \"q\" \\""#),
            vec![Tok::Str(r#"a\$N "q" \"#.into())]
        );
    }

    #[test]
    fn raw_strings() {
        assert_eq!(
            toks(r##"r#"for f in *; do a "$f"; done"#"##),
            vec![Tok::RawStr(r#"for f in *; do a "$f"; done"#.into())]
        );
        assert_eq!(toks(r#"r"a;b""#), vec![Tok::RawStr("a;b".into())]);
    }

    #[test]
    fn comments_and_shebang() {
        assert_eq!(
            toks("#!/usr/bin/env michi\n// hi\nstep // there\n"),
            vec![Tok::Keyword(Keyword::Step)]
        );
    }

    #[test]
    fn command_ends_at_the_semicolon() {
        let mut lexer = Lexer::new("seq 1 2000 > data.txt; rest");
        let (text, span) = lexer.command(0).unwrap();
        assert_eq!(text.source(), "seq 1 2000 > data.txt");
        assert_eq!(span, Span::new(0, 21));
        assert_eq!(lexer.pos(), 22);
    }

    #[test]
    fn command_semicolons_the_shell_owns() {
        assert_eq!(command("a 'x;y' b;"), "a 'x;y' b");
        assert_eq!(command(r#"a "x;y" b;"#), r#"a "x;y" b"#);
        assert_eq!(
            command(r"find . -exec rm {} \; ;"),
            r"find . -exec rm {} \;"
        );
        assert_eq!(command("a $(cd x; ls) b;"), "a $(cd x; ls) b");
        assert_eq!(command("a $(f \"$(g; h)\") b;"), "a $(f \"$(g; h)\") b");
        assert_eq!(command("a `x; y` b;"), "a `x; y` b");
        assert_eq!(command("a ${x:-;} b;"), "a ${x:-;} b");
    }

    #[test]
    fn command_joins_lines_and_strips_comments() {
        assert_eq!(
            command("make-input\n    --seed 42 # the seed\n    --rows 10\n    > in.txt;"),
            "make-input --seed 42 --rows 10 > in.txt"
        );
        assert_eq!(command("echo a#b;"), "echo a#b");
        assert_eq!(
            command("echo '# not' \"# a comment\";"),
            "echo '# not' \"# a comment\""
        );
    }

    #[test]
    fn command_keeps_multibyte_text() {
        assert_eq!(command("echo é 'é' \"é\" \\é;"), "echo é 'é' \"é\" \\é");
        assert_eq!(split_vars("\\é$N").parts.len(), 2);
    }

    #[test]
    fn command_comments_follow_the_shell() {
        assert_eq!(command("echo a |#c\n b;"), "echo a | b");
        assert_eq!(command("echo \\ #x;"), "echo \\ #x");
        assert_eq!(command("echo a #[note]\n;"), "echo a");
        assert_eq!(command("touch a\\ ;"), "touch a\\ ");
        assert_eq!(
            command("echo ${x:-${y}} ${x:-'}'};"),
            "echo ${x:-${y}} ${x:-'}'}"
        );
    }

    #[test]
    fn command_errors() {
        assert_eq!(command_err("seq 1"), "command is missing its `;`");
        assert_eq!(command_err("seq 1\n}"), "command is missing its `;`");
        assert_eq!(
            command_err("seq 1\n#[jobs(2)]\nstep s { a; }"),
            "command is missing its `;`"
        );
        assert_eq!(
            command("echo ${N} '}' \"}\" $(f }) {} { a; b; } ( c; d ) ;"),
            "echo ${N} '}' \"}\" $(f }) {} { a; b; } ( c; d )"
        );
        assert_eq!(
            command_err("echo 'a;"),
            "command has an unclosed quote, `(` or `{`"
        );
        assert_eq!(command_err("  ;"), "command is missing its `;`");
        assert_eq!(
            command_err("# only a comment\n;"),
            "a `#` comment with no command: michi comments are `//`"
        );
        assert_eq!(
            command_err("# only a comment\n}"),
            "a `#` comment with no command: michi comments are `//`"
        );
        let err = Lexer::new("99999999999999999999h")
            .next_token()
            .unwrap_err();
        assert_eq!(err.message, "duration is too long");
    }

    #[test]
    fn command_picks_out_vars() {
        let mut lexer = Lexer::new("seq 1 ${N}000000 > \"in-$T.txt\" $HOME $(x) $$ \\$N;");
        let (text, _) = lexer.command(0).unwrap();
        assert_eq!(
            text.parts,
            vec![
                Part::Lit("seq 1 ".into()),
                Part::Var {
                    name: "N".into(),
                    braced: true
                },
                Part::Lit("000000 > \"in-".into()),
                Part::Var {
                    name: "T".into(),
                    braced: false
                },
                Part::Lit(".txt\" ".into()),
                Part::Var {
                    name: "HOME".into(),
                    braced: false
                },
                Part::Lit(" $(x) $$ \\$N".into()),
            ]
        );
        assert_eq!(
            split_vars("${cfg.ROWS} $a.b").parts,
            vec![
                Part::Var {
                    name: "cfg.ROWS".into(),
                    braced: true
                },
                Part::Lit(" ".into()),
                Part::Var {
                    name: "a".into(),
                    braced: false
                },
                Part::Lit(".b".into()),
            ]
        );
    }
}
