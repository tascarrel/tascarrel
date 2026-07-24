//! Resolution of Tascarrel's unified host-side data directory.

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use reportify::Report;
use reportify::ResultExt as _;
use thiserror::Error;

const DEFAULT_TASCARREL_HOME: &str = ".tascarrel";

/// Resolved paths below the unified Tascarrel data directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TascarrelHome {
    root: PathBuf,
}

impl TascarrelHome {
    /// Resolves `TASCARREL_HOME`, defaulting to `.tascarrel` in the current
    /// directory.
    ///
    /// Relative configured paths are resolved against the process's current
    /// directory so downstream VM and socket configuration always receives
    /// absolute paths.
    ///
    /// # Errors
    ///
    /// Returns an error when the current directory cannot be resolved.
    #[tracing::instrument(
        name = "tascarrel_host.paths.discover",
        level = "debug",
        skip_all,
        fields(tascarrel_home = ?env::var_os("TASCARREL_HOME")),
        err
    )]
    pub fn discover() -> Result<Self, Report<TascarrelHomeError>> {
        let configured = env::var_os("TASCARREL_HOME")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from(DEFAULT_TASCARREL_HOME));
        Self::from_path(configured)
    }

    /// Resolves an explicit Tascarrel home path.
    ///
    /// # Errors
    ///
    /// Returns an error when a relative path cannot be resolved against the
    /// current directory.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, Report<TascarrelHomeError>> {
        let configured = path.as_ref();
        let root = std::path::absolute(configured)
            .map_err(|source| TascarrelHomeError::Resolve {
                path: configured.to_owned(),
                source,
            })
            .report()?;
        Ok(Self { root })
    }

    /// Returns the absolute Tascarrel home directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the configuration directory.
    #[must_use]
    pub fn config(&self) -> PathBuf {
        self.root.join("config")
    }

    /// Returns the configured-workspace directory.
    #[must_use]
    pub fn workspaces(&self) -> PathBuf {
        self.config().join("workspaces")
    }

    /// Returns the persistent state directory.
    #[must_use]
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// Returns the transient runtime directory inside state.
    #[must_use]
    pub fn runtime(&self) -> PathBuf {
        self.state().join("runtime")
    }

    /// Returns the host control socket path.
    #[must_use]
    pub fn control_socket(&self) -> PathBuf {
        self.runtime().join("control.sock")
    }
}

/// Failure to resolve the unified Tascarrel data directory.
#[derive(Debug, Error)]
pub enum TascarrelHomeError {
    /// A relative path could not be made absolute.
    #[error("failed to resolve Tascarrel home path {path}")]
    Resolve {
        /// Configured path.
        path: PathBuf,
        /// Underlying current-directory failure.
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies every default host path belongs to one resolved home tree.
    #[test]
    fn derives_config_state_runtime_and_socket_from_one_root() {
        let home = TascarrelHome::from_path("/srv/tascarrel-data").unwrap();
        assert_eq!(home.config(), Path::new("/srv/tascarrel-data/config"));
        assert_eq!(
            home.workspaces(),
            Path::new("/srv/tascarrel-data/config/workspaces")
        );
        assert_eq!(home.state(), Path::new("/srv/tascarrel-data/state"));
        assert_eq!(
            home.control_socket(),
            Path::new("/srv/tascarrel-data/state/runtime/control.sock")
        );
    }

    /// Verifies the `.tascarrel` default can be represented as an absolute
    /// root.
    #[test]
    fn resolves_relative_home_paths_against_the_current_directory() {
        let current = env::current_dir().unwrap();
        let home = TascarrelHome::from_path(DEFAULT_TASCARREL_HOME).unwrap();
        assert_eq!(home.root(), current.join(DEFAULT_TASCARREL_HOME));
    }
}
