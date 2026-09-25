//! What can go wrong building or running a pipeline.

use std::fmt;
use std::path::PathBuf;

/// Any error a closure or a sink hands back. `?` turns a std error, a
/// `&str` or `String`, or an `anyhow::Error` into one.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Why a pipeline would not build, or why its run ended early.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A command asks for more cores than it could ever be given.
    Cores {
        step: String,
        cmd: String,
        want: usize,
        room: usize,
        within: Within,
    },

    /// A pool asks for none, or for more cores than it is carved from. `step`
    /// is `None` for the pipeline's own pool.
    Pool {
        step: Option<String>,
        size: usize,
        room: usize,
        within: Within,
    },

    /// A file or directory could not be made or written.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    /// A sink returned an error, which stops the run.
    Sink(BoxError),

    /// A step failed and ended the run. `why` names what failed in it.
    Step { step: String, why: String },
}

/// What a command's cores or a pool are carved from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Within {
    Machine,
    Pipeline,
    Step,
}

impl fmt::Display for Within {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Within::Machine => "the machine",
            Within::Pipeline => "the pipeline's pool",
            Within::Step => "its step's pool",
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Cores {
                step,
                cmd,
                want,
                room,
                within,
            } => write!(
                f,
                "{step}.{cmd} wants {want} cores, and {within} has {room}"
            ),
            Error::Pool {
                step: Some(step),
                size,
                room,
                within,
            } => write!(f, "{step} pools {size} cores, and {within} has {room}"),
            Error::Pool {
                step: None,
                size,
                room,
                within,
            } => write!(
                f,
                "the pipeline's pool wants {size} cores, and {within} has {room}"
            ),
            Error::Io { path, .. } => write!(f, "cannot write {}", path.display()),
            // transparent: the sink's own words are the whole story
            Error::Sink(e) => e.fmt(f),
            Error::Step { why, .. } => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Sink(e) => e.source(),
            _ => None,
        }
    }
}

/// An error and everything under it, the way `{:#}` prints an `anyhow::Error`:
/// `outer: inner: innermost`.
pub(crate) fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut out = error.to_string();
    let mut next = error.source();
    while let Some(e) = next {
        out.push_str(": ");
        out.push_str(&e.to_string());
        next = e.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Wrapped(&'static str, Option<Box<Wrapped>>);

    impl fmt::Display for Wrapped {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|e| e as _)
        }
    }

    #[test]
    fn a_sink_error_is_its_own_words_and_its_own_source() {
        let inner = Wrapped("disk full", None);
        let error = Error::Sink(Box::new(Wrapped("table", Some(Box::new(inner)))));
        assert_eq!(error.to_string(), "table");
        assert_eq!(chain(&error), "table: disk full");
    }
}
