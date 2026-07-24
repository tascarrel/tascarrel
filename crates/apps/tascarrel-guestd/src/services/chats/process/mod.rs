//! Environment and transport abstractions for coding-harness processes.

mod environment;
mod transport;

pub(crate) use environment::ProcessEnvironment;
pub(crate) use transport::HarnessProcessControl;
pub(crate) use transport::HarnessProcessLauncher;
pub(crate) use transport::HarnessProcessSpec;
pub(crate) use transport::LocalHarnessProcessLauncher;
pub(crate) use transport::PodHarnessProcessLauncher;
