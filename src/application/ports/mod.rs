//! The traits the use cases reach the queue, Git, the agent, cmux, the
//! service manager, processes, time and IDs through. The infrastructure
//! implements them and the entry points inject the implementations.
//!
//! Each port is in the module of the context that owns it, with the types
//! of its arguments and results, and the ports every context uses are in
//! [`shared`] (docs/design/architecture.md, section "portのmodule"). The
//! modules are private: every port keeps its path (`application::X`, and
//! `ports::X` inside `application`) through the re-exports below.

mod execution;
mod host;
mod observation;
mod planning;
mod shared;

pub use execution::*;
pub use host::*;
pub use observation::*;
pub use planning::*;
pub use shared::*;
