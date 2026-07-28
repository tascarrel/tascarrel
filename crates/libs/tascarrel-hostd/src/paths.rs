//! Resolution of Tascarrel's unified host-side data directory.

use std::env;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use reportify::ErrorExt as _;
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
    /// Resolves `TASCARREL_HOME`, defaulting to `.tascarrel` in the user's home
    /// directory.
    ///
    /// Relative configured paths are resolved against the process's current
    /// directory so downstream VM and socket configuration always receives
    /// absolute paths.
    ///
    /// # Errors
    ///
    /// Returns an error when the user home is unavailable or relative, or when
    /// a relative configured path cannot be resolved.
    #[tracing::instrument(
        name = "tascarrel_host.paths.discover",
        level = "debug",
        skip_all,
        fields(tascarrel_home = ?env::var_os("TASCARREL_HOME")),
        err
    )]
    pub fn discover() -> Result<Self, Report<TascarrelHomeError>> {
        let configured = env::var_os("TASCARREL_HOME");
        let user_home = env::var_os("HOME");
        discover_from_environment(configured.as_deref(), user_home.as_deref())
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

    /// Returns the host-wide server configuration file.
    #[must_use]
    pub fn server_config(&self) -> PathBuf {
        self.config().join("server.toml")
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
    /// The user home environment variable is unavailable.
    #[error("failed to resolve Tascarrel home: HOME is unset or empty")]
    MissingUserHome,
    /// The user home environment variable does not name an absolute path.
    #[error("failed to resolve Tascarrel home: HOME is not an absolute path: {path}")]
    RelativeUserHome {
        /// Configured user home.
        path: PathBuf,
    },
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

/// Resolves a Tascarrel home from the relevant process environment.
fn discover_from_environment(
    configured: Option<&OsStr>,
    user_home: Option<&OsStr>,
) -> Result<TascarrelHome, Report<TascarrelHomeError>> {
    if let Some(configured) = configured.filter(|value| !value.is_empty()) {
        return TascarrelHome::from_path(configured);
    }
    let user_home = user_home
        .filter(|value| !value.is_empty())
        .ok_or_else(|| TascarrelHomeError::MissingUserHome.report())?;
    let user_home = Path::new(user_home);
    if !user_home.is_absolute() {
        return Err(TascarrelHomeError::RelativeUserHome {
            path: user_home.to_owned(),
        }
        .report());
    }
    TascarrelHome::from_path(user_home.join(DEFAULT_TASCARREL_HOME))
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
            home.server_config(),
            Path::new("/srv/tascarrel-data/config/server.toml")
        );
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

    /// Verifies the default home and control socket do not depend on the
    /// process working directory.
    #[test]
    fn defaults_to_the_user_home_directory() {
        let home = discover_from_environment(None, Some(OsStr::new("/home/tascarrel"))).unwrap();
        assert_eq!(home.root(), Path::new("/home/tascarrel/.tascarrel"));
        assert_eq!(
            home.control_socket(),
            Path::new("/home/tascarrel/.tascarrel/state/runtime/control.sock")
        );
    }

    /// Verifies an explicit relative home can be represented as an absolute
    /// root.
    #[test]
    fn resolves_relative_home_paths_against_the_current_directory() {
        let current = env::current_dir().unwrap();
        let home = TascarrelHome::from_path(DEFAULT_TASCARREL_HOME).unwrap();
        assert_eq!(home.root(), current.join(DEFAULT_TASCARREL_HOME));
    }
}
