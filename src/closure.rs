//! A closure the pipeline runs as a step of its own.

use std::collections::{BTreeMap, BTreeSet};

use crate::cmd::Value;
use crate::error::BoxError;
use crate::execute::Status;

/// The body of a closure step.
//
// `Send` because the pipeline moves a step into a scoped
// thread to run a batch, so everything a step holds has to
// be `Send`, though a closure only runs on the thread that
// reached it. `FnOnce`, so values can be moved in and out
pub(crate) type Call<'a> = Box<dyn FnOnce() -> Result<(), BoxError> + Send + 'a>;

/// Rust to run in place of a command. Only its wall clock is measured, and it
/// takes neither a timeout nor a core count.
pub struct Closure<'a> {
    pub(crate) name: String,
    pub(crate) fields: BTreeMap<String, String>,
    pub(crate) tags: BTreeSet<String>,
    pub(crate) status: Status,
    /// The body, `None` once it has run.
    pub(crate) f: Option<Call<'a>>,
}

impl std::fmt::Debug for Closure<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Closure")
            .field("name", &self.name)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl<'a> Closure<'a> {
    /// A closure named `name` that runs `f`.
    ///
    /// `f` returns a boxed [`std::error::Error`], so `?` works on any std error
    /// or on an `anyhow::Error`, and `Err("why".into())` gives a plain message.
    pub fn new(
        name: impl Into<String>,
        f: impl FnOnce() -> Result<(), Box<dyn std::error::Error + Send + Sync>> + Send + 'a,
    ) -> Closure<'a> {
        Closure {
            name: name.into(),
            fields: BTreeMap::new(),
            tags: BTreeSet::new(),
            status: Status::NotRun,
            f: Some(Box::new(f)),
        }
    }

    pub fn field(mut self, key: impl Into<String>, value: impl Value) -> Self {
        self.fields.insert(key.into(), value.render());
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.insert(tag.into());
        self
    }

    pub fn label(&self) -> &str {
        &self.name
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }

    pub fn tags(&self) -> &BTreeSet<String> {
        &self.tags
    }
}
