//! What went wrong reading a `.michi` file, and where.

use std::fmt;

/// A byte range in the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }

    /// The span from the start of this one to the end of `other`.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start,
            end: other.end,
        }
    }
}

/// Why the file could not be read, at which span, and under which
/// sweep bindings if any.
#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub span: Span,
    pub bindings: Vec<(String, String)>,
}

impl Error {
    pub fn new(message: impl Into<String>, span: Span) -> Error {
        Error {
            message: message.into(),
            span,
            bindings: Vec::new(),
        }
    }

    /// The message with its position, then the source line with a
    /// caret under the span.
    pub fn render(&self, path: &str, source: &str) -> String {
        let (line, col) = line_col(source, self.span.start);
        let text = source.lines().nth(line - 1).unwrap_or("");
        let mut out = format!("{path}:{line}:{col}: ");
        if !self.bindings.is_empty() {
            let pairs: Vec<String> = self
                .bindings
                .iter()
                .map(|(name, value)| format!("{name} = {value}"))
                .collect();
            out.push_str(&format!("for {}: ", pairs.join(", ")));
        }
        out.push_str(&self.message);
        out.push('\n');
        out.push_str("    ");
        out.push_str(text);
        out.push('\n');

        // the caret runs the span's width, held within the
        // line, and never shorter than one
        let width = self.span.end.saturating_sub(self.span.start);
        let room = text.chars().count().saturating_sub(col - 1);
        let width = width.clamp(1, room.max(1));
        out.push_str("    ");
        out.push_str(&" ".repeat(col - 1));
        out.push_str(&"^".repeat(width));
        out
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The one-based line and column holding a byte offset.
fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = before[line_start..].chars().count() + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_points_at_the_span() {
        let source = "step a {\n    cores(x)\n}\n";
        let start = source.find("cores").unwrap();
        let err = Error::new("cores takes an integer", Span::new(start, start + 5));
        let text = err.render("f.michi", source);
        assert_eq!(
            text,
            "f.michi:2:5: cores takes an integer\n        cores(x)\n        ^^^^^"
        );
    }

    #[test]
    fn render_names_the_bindings() {
        let mut err = Error::new("cores takes an integer", Span::new(0, 1));
        err.bindings.push(("T".into(), "a".into()));
        let text = err.render("f.michi", "x");
        assert!(text.starts_with("f.michi:1:1: for T = a: cores takes an integer"));
    }

    #[test]
    fn caret_stays_within_the_line() {
        let err = Error::new("missing its `;`", Span::new(0, 500));
        let text = err.render("f.michi", "seq 1");
        assert!(text.ends_with("\n    ^^^^^"));
    }
}
