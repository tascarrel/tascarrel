//! Workspace-wide process launch, supervision, and observable state.
//!
//! [`ProcessSupervisor`] owns process lifecycle state and resumable
//! subscriptions. Control-plane dispatch is outside this feature.

mod log;
mod supervisor;
mod terminal;

pub(crate) use log::LogSubscription;
pub(crate) use supervisor::OwnedProcessOutput;
pub(crate) use supervisor::OwnedProcessParts;
pub(crate) use supervisor::ProcessListSubscription;
pub use supervisor::ProcessSupervisor;
pub use supervisor::ProcessSupervisorConfig;
pub use supervisor::ProcessSupervisorError;
pub(crate) use terminal::TerminalSubscription;
