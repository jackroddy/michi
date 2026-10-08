//! Reading a `.michi` file: the lexer, the parser, and the tree between
//! them.

pub mod ast;
pub mod error;
pub mod lex;
pub mod parse;

pub use error::{Error, Span};
pub use parse::parse;
