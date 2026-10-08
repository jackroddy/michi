//! Reading a `.michi` file: the lexer, the parser, the resolve pass, the
//! bash script a resolved file compiles to, and the pipelines it builds.

pub mod ast;
pub mod error;
pub mod expand;
pub mod lex;
pub mod lower;
pub mod parse;
pub mod resolve;
pub mod sh;

pub use error::{Error, Span};
pub use expand::expand;
pub use lower::{Built, lower};
pub use parse::parse;
pub use resolve::resolve;

/// Parse and resolve `source`, then compile it to a bash script whose
/// header names it `name`.
pub fn compile(source: &str, name: &str) -> Result<String, Error> {
    let mut file = parse(source)?;
    resolve(&mut file)?;
    Ok(sh::script(&file, name))
}

/// Parse, resolve, expand and lower `source` to its pipelines, ready to
/// run.
pub fn build(source: &str) -> Result<Vec<Built>, Error> {
    let mut file = parse(source)?;
    resolve(&mut file)?;
    lower(&expand(&file)?)
}
