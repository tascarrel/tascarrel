//! Host-owned secret-provider access and interpolation.
//!
//! [`SecretsService`] binds configured provider instances to workspace
//! directories, exposes safe provider metadata, and is the only service that
//! returns plaintext secret values. Provider implementations remain private so
//! their interface can evolve with concrete host consumers.

mod interpolation;
mod service;
mod sops;

pub(crate) use service::SecretReference;
pub use service::SecretsService;
pub use service::SecretsServiceConfig;
pub use service::SecretsServiceError;
pub use service::SecretsSubscription;
