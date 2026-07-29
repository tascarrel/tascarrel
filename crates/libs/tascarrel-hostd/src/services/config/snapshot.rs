//! Safe synchronous loading of one workspace configuration snapshot.
//!
//! [`load`] decodes the generated configuration contract and computes the
//! recursive modification times published by the configuration service.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::SystemTime;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::config as api;
use thiserror::Error;

use super::settings;
use super::settings::SettingsValidationError;

/// Configuration contents and modification times observed in one scan.
pub(crate) struct WorkspaceSnapshot {
    config: Result<api::WorkspaceConfig, api::ConfigError>,
    settings: Result<Option<api::WorkspaceSettings>, api::ConfigError>,
    agents_modified_at: Option<Timestamp>,
    image_modified_at: Timestamp,
    modified_at: Timestamp,
}

impl WorkspaceSnapshot {
    /// Builds an event while independently retaining preceding valid inputs
    /// after load errors.
    pub(crate) fn into_event(
        self,
        previous: Option<&api::ConfigChangedEvent>,
    ) -> api::ConfigChangedEvent {
        let (config, last_config_error) = match self.config {
            Ok(config) => (Some(config), None),
            Err(error) => (previous.and_then(|event| event.config.clone()), Some(error)),
        };
        let (settings, last_settings_error) = match self.settings {
            Ok(settings) => (settings, None),
            Err(error) => (
                previous.and_then(|event| event.settings.clone()),
                Some(error),
            ),
        };
        api::ConfigChangedEvent {
            config_instance_id: api::ConfigInstanceId::generate(),
            config,
            last_config_error,
            settings,
            last_settings_error,
            agents_modified_at: self.agents_modified_at,
            image_modified_at: self.image_modified_at,
            modified_at: self.modified_at,
        }
    }
}

/// Reads the current typed configuration and recursive modification times.
#[tracing::instrument(level = "debug", skip_all, fields(workspace = %workspace.display()))]
pub(crate) fn load(
    workspace: &Path,
    max_config_bytes: u64,
) -> Result<WorkspaceSnapshot, Report<SnapshotError>> {
    let times = scan_times(workspace)?;
    let config =
        load_config_file(&workspace.join("config.toml"), max_config_bytes).map_err(|report| {
            api::ConfigError {
                message: report.error().client_message().into(),
            }
        });
    let settings =
        load_settings_file(&workspace.join("settings.json"), max_config_bytes).map_err(|report| {
            api::ConfigError {
                message: report.error().client_message().into(),
            }
        });
    Ok(WorkspaceSnapshot {
        config,
        settings,
        agents_modified_at: times.agents,
        image_modified_at: times.image,
        modified_at: times.workspace,
    })
}

/// Failure to inspect the workspace configuration source safely.
#[derive(Clone, Copy, Debug, Error)]
pub(crate) enum SnapshotError {
    /// A required path or modification time is unavailable.
    #[error("workspace configuration input is unavailable")]
    Unavailable,
}

/// Recursive modification-time maxima captured during one tree scan.
struct WorkspaceTimes {
    agents: Option<Timestamp>,
    image: Timestamp,
    workspace: Timestamp,
}

/// Reserved subtree associated with one traversed directory.
#[derive(Clone, Copy)]
enum TreeScope {
    Agents,
    Image,
    Other,
}

/// Scans one workspace tree without following directory symlinks.
fn scan_times(workspace: &Path) -> Result<WorkspaceTimes, Report<SnapshotError>> {
    let root_metadata = fs::symlink_metadata(workspace).map_err(|error| {
        inspect_error(
            "inspect workspace configuration directory",
            workspace,
            &error,
        )
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(SnapshotError::Unavailable
            .report()
            .message("workspace configuration root is not a real directory"));
    }
    let mut modified_at = metadata_time(workspace, &root_metadata)?;
    let mut agents_modified_at = None;
    let mut image_modified_at = None;
    let mut directories = vec![(workspace.to_owned(), TreeScope::Other)];

    while let Some((directory, scope)) = directories.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| inspect_error("read configuration directory", &directory, &error))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                inspect_error("read configuration directory entry", &directory, &error)
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| inspect_error("inspect configuration entry", &path, &error))?;
            let entry_modified_at = metadata_time(&path, &metadata)?;
            modified_at = modified_at.max(entry_modified_at);
            let entry_scope = if directory == workspace {
                root_scope(entry.file_name().as_os_str(), &path, &metadata)?
            } else {
                scope
            };
            match entry_scope {
                TreeScope::Agents => {
                    agents_modified_at = Some(
                        agents_modified_at.map_or(entry_modified_at, |current: SystemTime| {
                            current.max(entry_modified_at)
                        }),
                    );
                }
                TreeScope::Image => {
                    image_modified_at = Some(
                        image_modified_at.map_or(entry_modified_at, |current: SystemTime| {
                            current.max(entry_modified_at)
                        }),
                    );
                }
                TreeScope::Other => {}
            }
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                directories.push((path, entry_scope));
            }
        }
    }

    let image_modified_at = image_modified_at.ok_or_else(|| {
        SnapshotError::Unavailable
            .report()
            .message("workspace image directory does not exist")
    })?;
    Ok(WorkspaceTimes {
        agents: agents_modified_at
            .map(|time| convert_time("agents directory", time))
            .transpose()?,
        image: convert_time("image directory", image_modified_at)?,
        workspace: convert_time("workspace configuration directory", modified_at)?,
    })
}

/// Classifies and validates a reserved top-level input directory.
fn root_scope(
    name: &OsStr,
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<TreeScope, Report<SnapshotError>> {
    let scope = if name == "agents" {
        TreeScope::Agents
    } else if name == "image" {
        TreeScope::Image
    } else {
        return Ok(TreeScope::Other);
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(SnapshotError::Unavailable.report().message(format!(
            "configuration input is not a real directory: {}",
            path.display()
        )));
    }
    Ok(scope)
}

/// Reads one entry's modification time with path context.
fn metadata_time(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<SystemTime, Report<SnapshotError>> {
    metadata
        .modified()
        .map_err(|error| inspect_error("read configuration modification time", path, &error))
}

/// Converts a filesystem time into the API timestamp representation.
fn convert_time(input: &str, time: SystemTime) -> Result<Timestamp, Report<SnapshotError>> {
    Timestamp::try_from(time).map_err(|error| {
        SnapshotError::Unavailable.report().message(format!(
            "{input} has an unsupported modification time: {error}"
        ))
    })
}

/// Attaches filesystem operation context to an unavailable-input report.
fn inspect_error(operation: &str, path: &Path, error: &io::Error) -> Report<SnapshotError> {
    SnapshotError::Unavailable
        .report()
        .message(format!("{operation} {}: {error}", path.display()))
}

/// Reads one configuration file through the canonical generated type.
pub(crate) fn load_config_file(
    path: &Path,
    max_bytes: u64,
) -> Result<api::WorkspaceConfig, Report<ConfigLoadError>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return decode_config(""),
        Err(error) => {
            return Err(ConfigLoadError::Open.report().message(error.to_string()));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| ConfigLoadError::Inspect.report().message(error.to_string()))?;
    if !metadata.is_file() {
        return Err(ConfigLoadError::NotRegular.report());
    }
    if metadata.len() > max_bytes {
        return Err(ConfigLoadError::TooLarge { max_bytes }.report());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ConfigLoadError::Read.report().message(error.to_string()))?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > max_bytes) {
        return Err(ConfigLoadError::TooLarge { max_bytes }.report());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| ConfigLoadError::NotUtf8.report())?;
    decode_config(text)
}

/// Decodes the generated TOML shape and rejects unknown fields.
pub(crate) fn decode_config(text: &str) -> Result<api::WorkspaceConfig, Report<ConfigLoadError>> {
    let mut unknown = BTreeSet::new();
    let deserializer =
        toml::Deserializer::parse(text).map_err(|_| ConfigLoadError::Decode.report())?;
    let config = serde_ignored::deserialize(deserializer, |path| {
        unknown.insert(path.to_string());
    })
    .map_err(|_| ConfigLoadError::Decode.report())?;
    if !unknown.is_empty() {
        return Err(ConfigLoadError::UnknownFields(unknown.into_iter().collect()).report());
    }
    validate_slash_commands(&config)?;
    Ok(config)
}

/// Validates the user-facing identifiers and contents of workspace slash
/// commands.
fn validate_slash_commands(config: &api::WorkspaceConfig) -> Result<(), Report<ConfigLoadError>> {
    let Some(commands) = config.chat.as_ref().and_then(|chat| chat.commands.as_ref()) else {
        return Ok(());
    };
    for (name, command) in commands {
        if !valid_slash_command_name(name) {
            return Err(ConfigLoadError::InvalidSlashCommand {
                name: name.to_string(),
                reason: "name must start with a lowercase ASCII letter and contain only lowercase ASCII letters, numbers, or hyphens",
            }
            .report());
        }
        if command.text.trim().is_empty() {
            return Err(ConfigLoadError::InvalidSlashCommand {
                name: name.to_string(),
                reason: "text must not be empty",
            }
            .report());
        }
    }
    Ok(())
}

/// Returns whether a name has one unambiguous slash-command spelling.
fn valid_slash_command_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_lowercase())
        && characters.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

/// Reads optional portable workspace settings through the generated type.
fn load_settings_file(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<api::WorkspaceSettings>, Report<SettingsLoadError>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SettingsLoadError::Open.report().message(error.to_string()));
        }
    };
    let metadata = file.metadata().map_err(|error| {
        SettingsLoadError::Inspect
            .report()
            .message(error.to_string())
    })?;
    if !metadata.is_file() {
        return Err(SettingsLoadError::NotRegular.report());
    }
    if metadata.len() > max_bytes {
        return Err(SettingsLoadError::TooLarge { max_bytes }.report());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| SettingsLoadError::Read.report().message(error.to_string()))?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > max_bytes) {
        return Err(SettingsLoadError::TooLarge { max_bytes }.report());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| SettingsLoadError::NotUtf8.report())?;
    decode_settings(text).map(Some)
}

/// Decodes the generated JSON shape and rejects unknown fields.
fn decode_settings(text: &str) -> Result<api::WorkspaceSettings, Report<SettingsLoadError>> {
    let mut unknown = BTreeSet::new();
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let settings = serde_ignored::deserialize(&mut deserializer, |path| {
        unknown.insert(path.to_string());
    })
    .map_err(|_| SettingsLoadError::Decode.report())?;
    deserializer
        .end()
        .map_err(|_| SettingsLoadError::Decode.report())?;
    if !unknown.is_empty() {
        return Err(SettingsLoadError::UnknownFields(unknown.into_iter().collect()).report());
    }
    settings::validate(&settings)
        .map_err(|report| SettingsLoadError::Invalid(*report.error()).report())?;
    Ok(settings)
}

/// Value-safe configuration loading failure categories.
#[derive(Debug, Error)]
pub(crate) enum ConfigLoadError {
    #[error("failed to open config.toml")]
    Open,
    #[error("failed to inspect config.toml")]
    Inspect,
    #[error("config.toml is not a regular file")]
    NotRegular,
    #[error("config.toml exceeds {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
    #[error("failed to read config.toml")]
    Read,
    #[error("config.toml is not UTF-8")]
    NotUtf8,
    #[error("failed to decode config.toml")]
    Decode,
    #[error("config.toml contains unknown fields")]
    UnknownFields(Vec<String>),
    #[error("config.toml contains invalid slash command {name:?}: {reason}")]
    InvalidSlashCommand { name: String, reason: &'static str },
}

impl ConfigLoadError {
    fn client_message(&self) -> String {
        match self {
            Self::UnknownFields(fields) => {
                format!("config.toml contains unknown fields: {}", fields.join(", "))
            }
            _ => self.to_string(),
        }
    }
}

/// Value-safe settings loading failure categories.
#[derive(Debug, Error)]
enum SettingsLoadError {
    #[error("failed to open settings.json")]
    Open,
    #[error("failed to inspect settings.json")]
    Inspect,
    #[error("settings.json is not a regular file")]
    NotRegular,
    #[error("settings.json exceeds {max_bytes} bytes")]
    TooLarge { max_bytes: u64 },
    #[error("failed to read settings.json")]
    Read,
    #[error("settings.json is not UTF-8")]
    NotUtf8,
    #[error("failed to decode settings.json")]
    Decode,
    #[error("settings.json contains unknown fields")]
    UnknownFields(Vec<String>),
    #[error("settings.json is invalid: {0}")]
    Invalid(SettingsValidationError),
}

impl SettingsLoadError {
    fn client_message(&self) -> String {
        match self {
            Self::UnknownFields(fields) => {
                format!(
                    "settings.json contains unknown fields: {}",
                    fields.join(", ")
                )
            }
            _ => self.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// Verifies optional agents and recursive image and workspace timestamps.
    #[test]
    fn snapshot_observes_recursive_times_without_agents() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("demo");
        fs::create_dir_all(workspace.join("image/nested")).unwrap();
        fs::write(workspace.join("image/nested/Dockerfile"), "FROM scratch\n").unwrap();
        fs::write(workspace.join("config.toml"), "[vm]\ncores = 4\n").unwrap();

        let snapshot = load(&workspace, 4 * 1024 * 1024).unwrap();
        let event = snapshot.into_event(None);

        assert_eq!(event.config.unwrap().vm.unwrap().cores, Some(4));
        assert!(event.agents_modified_at.is_none());
        assert!(event.image_modified_at <= event.modified_at);
        assert!(event.last_config_error.is_none());
        assert!(event.settings.is_none());
        assert!(event.last_settings_error.is_none());
    }

    /// Verifies unknown fields become value-safe API configuration errors.
    #[test]
    fn snapshot_reports_unknown_configuration_fields() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("demo");
        fs::create_dir_all(workspace.join("image")).unwrap();
        fs::write(workspace.join("config.toml"), "secret-value = 'hidden'\n").unwrap();

        let event = load(&workspace, 4 * 1024 * 1024).unwrap().into_event(None);

        assert!(event.config.is_none());
        let message = event.last_config_error.unwrap().message;
        assert!(message.contains("secret-value"));
        assert!(!message.contains("hidden"));
    }

    /// Verifies slash-command definitions are decoded and invalid names are
    /// reported without text.
    #[test]
    fn snapshot_validates_workspace_slash_commands() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("demo");
        fs::create_dir_all(workspace.join("image")).unwrap();
        fs::write(
            workspace.join("config.toml"),
            "[chat.commands.review]\ntext = 'Inspect the secret implementation'\n",
        )
        .unwrap();

        let valid = load(&workspace, 4 * 1024 * 1024).unwrap().into_event(None);
        let commands = valid
            .config
            .as_ref()
            .unwrap()
            .chat
            .as_ref()
            .unwrap()
            .commands
            .as_ref()
            .unwrap();
        assert_eq!(
            commands.get("review").unwrap().text,
            "Inspect the secret implementation"
        );

        fs::write(
            workspace.join("config.toml"),
            "[chat.commands.Bad_Name]\ntext = 'Do not expose this text'\n",
        )
        .unwrap();

        let invalid = load(&workspace, 4 * 1024 * 1024)
            .unwrap()
            .into_event(Some(&valid));
        assert_eq!(invalid.config, valid.config);
        let message = invalid.last_config_error.unwrap().message;
        assert!(message.contains("Bad_Name"));
        assert!(!message.contains("Do not expose this text"));
    }

    /// Verifies settings are decoded and a later invalid value retains the
    /// last valid preferences without exposing JSON values.
    #[test]
    fn snapshot_retains_valid_settings_after_safe_decode_error() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("demo");
        fs::create_dir_all(workspace.join("image")).unwrap();
        fs::write(workspace.join("config.toml"), "").unwrap();
        fs::write(
            workspace.join("settings.json"),
            r#"{
                "chat": {
                    "harnesses": {
                        "claudeCode": {
                            "defaultModel": {
                                "model": "claude-sonnet-5",
                                "options": []
                            },
                            "modelOrder": ["claude-opus-4-8"]
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let valid = load(&workspace, 4 * 1024 * 1024).unwrap().into_event(None);
        let valid_instance = valid.config_instance_id.clone();
        let preferences = valid
            .settings
            .as_ref()
            .unwrap()
            .chat
            .as_ref()
            .unwrap()
            .harnesses
            .as_ref()
            .unwrap()
            .claude_code
            .as_ref()
            .unwrap();
        assert_eq!(
            preferences.default_model.as_ref().unwrap().model.as_ref(),
            "claude-sonnet-5"
        );

        fs::write(
            workspace.join("settings.json"),
            r#"{"unknown":"secret-value"}"#,
        )
        .unwrap();
        let invalid = load(&workspace, 4 * 1024 * 1024)
            .unwrap()
            .into_event(Some(&valid));

        assert_ne!(invalid.config_instance_id, valid_instance);
        assert_eq!(invalid.settings, valid.settings);
        let message = invalid.last_settings_error.unwrap().message;
        assert!(message.contains("unknown"));
        assert!(!message.contains("secret-value"));
    }

    /// Verifies a complete Tasci catalog is loaded and a later dangling model
    /// reference retains that last valid catalog.
    #[test]
    fn snapshot_validates_tasci_endpoint_and_model_references() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("demo");
        fs::create_dir_all(workspace.join("image")).unwrap();
        fs::write(workspace.join("config.toml"), "").unwrap();
        fs::write(workspace.join("settings.json"), valid_tasci_settings()).unwrap();

        let valid = load(&workspace, 4 * 1024 * 1024).unwrap().into_event(None);
        let tasci = valid
            .settings
            .as_ref()
            .unwrap()
            .chat
            .as_ref()
            .unwrap()
            .tasci
            .as_ref()
            .unwrap();
        assert_eq!(tasci.default_model.as_deref(), Some("qwen"));
        assert_eq!(
            tasci
                .models
                .as_ref()
                .unwrap()
                .get("qwen")
                .unwrap()
                .endpoint
                .as_ref(),
            "local"
        );

        fs::write(
            workspace.join("settings.json"),
            valid_tasci_settings().replace(r#""endpoint": "local""#, r#""endpoint": "missing""#),
        )
        .unwrap();
        let invalid = load(&workspace, 4 * 1024 * 1024)
            .unwrap()
            .into_event(Some(&valid));

        assert_eq!(invalid.settings, valid.settings);
        assert!(
            invalid
                .last_settings_error
                .unwrap()
                .message
                .contains("references an endpoint")
        );
    }

    fn valid_tasci_settings() -> &'static str {
        r#"{
            "chat": {
                "tasci": {
                    "defaultModel": "qwen",
                    "endpoints": {
                        "local": {
                            "displayName": "Local llama.cpp",
                            "protocol": "OpenAiChatCompletions",
                            "baseUrl": "http://host.tascarrel.internal:18080/v1",
                            "authorization": {
                                "header": "Authorization",
                                "value": "Bearer tascarrel-secret:local-token"
                            }
                        }
                    },
                    "models": {
                        "qwen": {
                            "endpoint": "local",
                            "model": "qwen3.6-35b-a3b-q6",
                            "displayName": "Qwen 35B A3B",
                            "contextWindow": 131072,
                            "maxOutputTokens": 32768,
                            "toolCalls": true,
                            "parallelToolCalls": true
                        }
                    }
                }
            }
        }"#
    }
}
