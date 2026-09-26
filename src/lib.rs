// the pinning and accounting syscalls have only been built
// and run with 64-bit linux argument widths, so a narrower
// target fails at the build rather than at a syscall
#[cfg(not(target_pointer_width = "64"))]
compile_error!("michi supports 64-bit targets only");

mod closure;
mod cmd;
mod cpu;
mod error;
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
pub use cpu::Placement;
pub use error::{Error, Within};
pub use execute::{Status, Timing};
pub use item::Item;
pub use pipeline::{Pipeline, PipelineBuilder};
pub use progress::{Marks, Progress, Stream, When};
pub use sink::Sink;
pub use step::{OnError, Step, Strategy};
pub use table::{Headers, Mode, Table};
