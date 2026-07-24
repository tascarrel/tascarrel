//! Workspace-wide Code editor lifecycle and observable session state.
//!
//! [`CodeService`] owns the shared code-server profile, supervised editor
//! processes, and host-issued HTTP route bindings for one workspace.

mod service;

/// Reserved storage identity used for the workspace-wide Code profile.
pub const CODE_EDITOR_CACHE_NAME: &str = "tascarrel-code-editor";

/// Home-relative writable profile path mounted into every workspace pod.
pub const CODE_EDITOR_PROFILE_PATH: &str = "~/.tascarrel/editors/code/profile";

pub use service::CodeService;
pub use service::CodeServiceConfig;
pub use service::CodeServiceError;
pub(crate) use service::CodeSessionListSubscription;
