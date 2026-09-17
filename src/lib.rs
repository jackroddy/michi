// the pinning and accounting syscalls are reached through argument widths this
// crate has only ever built and run against on 64-bit linux. a narrower target
// is not a configuration anything here has been checked on, so it stops at the
// build rather than at a mask written half off the end of itself
#[cfg(not(target_pointer_width = "64"))]
compile_error!("michi supports 64-bit targets only");

mod closure;
mod cmd;
mod cpu;
mod execute;
mod fmt;
mod item;
mod label;
mod pipeline;
mod progress;
mod sink;
mod step;
mod table;

pub use closure::Closure;
pub use cmd::{Cmd, Memory, Output, Value};
pub use execute::{Status, Timing};
pub use item::Item;
pub use pipeline::{Pipeline, PipelineBuilder};
pub use progress::{Marks, Progress, Stream, When};
pub use sink::Sink;
pub use step::{OnError, Step, Strategy};
pub use table::{Headers, Mode, Table};
