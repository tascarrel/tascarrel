//! Application-owned environment configuration for harness child processes.

use std::collections::HashMap;
use std::io;

use tokio::process::Command;

/// Applies application-owned environment and credential locations to a harness
/// process.
///
/// Implementations may read credentials at process-launch time so
/// authentication changes take effect without recreating the chat engine.
/// Implementations must not log credential values.
pub trait ProcessEnvironment: Send + Sync {
    /// Returns application-owned environment variables for a new process.
    fn variables(&self) -> io::Result<HashMap<String, String>>;

    /// Applies environment changes to a command immediately before it is
    /// spawned.
    fn apply(&self, command: &mut Command) -> io::Result<()> {
        command.envs(self.variables()?);
        Ok(())
    }
}
