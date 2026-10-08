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

use ast::{File, Item};

pub use error::{Error, Span};
pub use expand::expand;
pub use lower::{Built, Sinks, lower};
pub use parse::parse;
pub use resolve::resolve;

/// Parse and resolve `source`, keeping only the pipelines named in
/// `only`, in that order, when it names any.
pub fn load(source: &str, only: &[String]) -> Result<File, Error> {
    let mut file = parse(source)?;
    resolve(&mut file)?;
    select(&mut file, only)?;
    Ok(file)
}

/// Keep the data blocks and the pipelines named in `only`, in the order
/// named. Nothing changes when `only` is empty.
pub fn select(file: &mut File, only: &[String]) -> Result<(), Error> {
    if only.is_empty() {
        return Ok(());
    }
    let mut items: Vec<Item> = Vec::new();
    let mut pipelines: Vec<Item> = Vec::new();
    for item in file.items.drain(..) {
        match item {
            Item::Data(_) => items.push(item),
            Item::Pipeline(_) => pipelines.push(item),
        }
    }
    for name in only {
        let at = pipelines
            .iter()
            .position(|item| matches!(item, Item::Pipeline(p) if p.name.text == *name))
            .ok_or_else(|| Error::new(format!("no pipeline named `{name}`"), Span::new(0, 0)))?;
        items.push(pipelines.remove(at));
    }
    file.items = items;
    Ok(())
}

/// The bash script for a loaded file, named `name` in its header.
pub fn script(file: &File, name: &str) -> String {
    sh::script(file, name)
}

/// Load `source` whole and compile it to a bash script.
pub fn compile(source: &str, name: &str) -> Result<String, Error> {
    Ok(script(&load(source, &[])?, name))
}

/// Expand and lower a loaded file to its pipelines, ready to run.
pub fn build(file: &File, sinks: &Sinks) -> Result<Vec<Built>, Error> {
    lower(&expand(file)?, sinks)
}
