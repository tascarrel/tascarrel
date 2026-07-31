use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::{self};
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use tascarrel_api::MAX_WORKSPACE_CONFIG_BYTES;
use tascarrel_api::parse_memory_mib;
use tascarrel_api::parse_size_bytes;
use tascarrel_api::types::config as config_api;
use tascarrel_protocol::MAX_WORKSPACE_HOST_SHARES;
use tascarrel_protocol::valid_workspace_share_name;

use crate::CODE_EDITOR_CACHE_NAME;
use crate::CODE_EDITOR_PROFILE_PATH;

const MAX_WORKSPACE_CACHES: usize = 64;
const MAX_LIFECYCLE_STEPS: usize = 64;
const MAX_ENVIRONMENT_ENTRIES: usize = 128;
const MAX_ENVIRONMENT_BYTES: usize = 64 * 1024;
const MAX_CODE_EDITOR_EXTENSIONS: usize = 127;
const CODE_EXTENSION_ID_MAX_BYTES: usize = 256;
const HOST_SHARE_MOUNT_TAG_PREFIX: &str = "tascarrel-share-";

/// Settings loaded once from a workspace's read-only `config.toml`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceConfig {
    pub vm: WorkspaceVm,
    /// Names declared by the host-only share configuration.
    ///
    /// Guest runtime details come from the host-pinned VM manifest instead of
    /// the potentially newer workspace input snapshot.
    pub host_shares: BTreeSet<String>,
    pub features: WorkspaceFeatures,
    pub nix: WorkspaceNix,
    pub editors: WorkspaceEditors,
    pub env: BTreeMap<String, String>,
    pub setup: WorkspaceSetup,
    pub init: WorkspaceInit,
    pub caches: Vec<WorkspaceCache>,
    pub repos: BTreeMap<String, WorkspaceRepository>,
    /// Shared network policy. Hostd enforces egress and resolves secrets;
    /// guestd consumes only the host port declarations.
    pub network: WorkspaceNetwork,
}

/// Host-consumed resources for this workspace's dedicated VM.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceVm {
    /// Virtual CPU override; omitted means every CPU available to hostd.
    pub cores: Option<u16>,
    /// Binary memory size override such as `"16G"` or `"1536MiB"`.
    pub memory: Option<String>,
    /// Virtual capacity of the sparse persistent state disk, such as `"1T"`.
    pub disk: Option<String>,
}

impl WorkspaceConfig {
    /// Loads a workspace policy without making guest availability depend on it.
    ///
    /// Invalid input yields safe defaults plus the diagnostic that should mark
    /// the workspace degraded.
    #[must_use]
    pub fn load_degraded(path: &Path) -> (Self, Option<String>) {
        match Self::load(path) {
            Ok(config) => (config, None),
            Err(error) => (
                Self::default(),
                Some(format!("workspace configuration is invalid: {error:#}")),
            ),
        }
    }

    /// Returns whether Docker requires the broader nested-container surface.
    #[must_use]
    pub const fn nested_containers(&self) -> bool {
        self.features.docker
    }

    /// Returns whether Podman requires subordinate user-namespace facilities.
    #[must_use]
    pub const fn rootless_containers(&self) -> bool {
        self.features.podman
    }

    /// Loads a bounded regular file without following a final symlink.
    ///
    /// A missing file is equivalent to an empty config so newly-created
    /// workspaces receive the documented defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be safely read as a bounded,
    /// regular UTF-8 file or its TOML does not match the supported schema.
    pub fn load(path: &Path) -> Result<Self> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open workspace config {}", path.display()));
            }
        };
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect workspace config {}", path.display()))?;
        if !metadata.is_file() {
            bail!("workspace config is not a regular file: {}", path.display());
        }
        if metadata.len() > MAX_WORKSPACE_CONFIG_BYTES {
            bail!(
                "workspace config exceeds {MAX_WORKSPACE_CONFIG_BYTES} bytes: {}",
                path.display()
            );
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.by_ref()
            .take(MAX_WORKSPACE_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read workspace config {}", path.display()))?;
        if bytes.len() as u64 > MAX_WORKSPACE_CONFIG_BYTES {
            bail!(
                "workspace config grew beyond {MAX_WORKSPACE_CONFIG_BYTES} bytes while reading: {}",
                path.display()
            );
        }
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("workspace config is not UTF-8: {}", path.display()))?;
        let config = Self::decode(text)
            .with_context(|| format!("decode workspace config {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("validate workspace config {}", path.display()))?;
        Ok(config)
    }

    /// Decodes the generated config shape and rejects fields outside it.
    fn decode(text: &str) -> Result<Self> {
        let mut unknown = BTreeSet::new();
        let deserializer = toml::Deserializer::parse(text).context("parse TOML")?;
        let raw: config_api::WorkspaceConfig = serde_ignored::deserialize(deserializer, |field| {
            unknown.insert(field.to_string());
        })
        .context("decode generated workspace config")?;
        if !unknown.is_empty() {
            bail!(
                "workspace config contains unknown fields: {}",
                unknown.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        Self::from_api(raw)
    }

    /// Applies product defaults and converts the generated raw config shape.
    fn from_api(raw: config_api::WorkspaceConfig) -> Result<Self> {
        let vm = raw.vm.map_or_else(WorkspaceVm::default, |vm| WorkspaceVm {
            cores: vm.cores,
            memory: vm.memory.map(|value| value.to_string()),
            disk: vm.disk.map(|value| value.to_string()),
        });
        let host_shares = raw
            .shares
            .unwrap_or_default()
            .into_keys()
            .map(|name| name.to_string())
            .collect();
        let features = workspace_features_from_api(raw.features);
        let nix = workspace_nix_from_api(raw.nix);
        let editors = workspace_editors_from_api(raw.editors);
        let env = raw
            .env
            .unwrap_or_default()
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let setup = WorkspaceSetup {
            steps: raw
                .setup
                .and_then(|setup| setup.steps)
                .unwrap_or_default()
                .iter()
                .map(|step| WorkspaceSetupStep {
                    script: step.script.to_string(),
                })
                .collect(),
        };
        let init = WorkspaceInit {
            steps: raw
                .init
                .and_then(|init| init.steps)
                .unwrap_or_default()
                .iter()
                .map(|step| WorkspaceInitStep {
                    script: step.script.to_string(),
                    wait: step.wait.unwrap_or(false),
                })
                .collect(),
        };
        let caches = raw
            .caches
            .unwrap_or_default()
            .iter()
            .map(|cache| WorkspaceCache {
                name: cache.name.to_string(),
                path: cache.path.to_string(),
            })
            .collect();
        let repos = raw
            .repos
            .unwrap_or_default()
            .into_iter()
            .map(|(path, repository)| {
                (
                    path.to_string(),
                    WorkspaceRepository {
                        source: repository.source.to_string(),
                        branch: repository.branch.map(|branch| branch.to_string()),
                    },
                )
            })
            .collect();
        let network = raw
            .network
            .map(workspace_network_from_api)
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            vm,
            host_shares,
            features,
            nix,
            editors,
            env,
            setup,
            init,
            caches,
            repos,
            network,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.vm.cores == Some(0) {
            bail!("VM cores must be greater than zero");
        }
        if self.host_shares.len() > MAX_WORKSPACE_HOST_SHARES
            || self
                .host_shares
                .iter()
                .any(|name| !valid_workspace_share_name(name))
        {
            bail!(
                "workspace host shares must use at most {MAX_WORKSPACE_HOST_SHARES} portable names"
            );
        }
        if let Some(memory) = &self.vm.memory {
            parse_memory_mib(memory)
                .map_err(|error| anyhow::anyhow!(error.to_string()))
                .context("invalid VM memory")?;
        }
        if let Some(disk) = &self.vm.disk {
            let bytes = parse_size_bytes(disk)
                .map_err(|error| anyhow::anyhow!(error.to_string()))
                .context("invalid VM disk")?;
            if bytes < 256 * 1024 * 1024 {
                bail!("VM disk must be at least 256 MiB");
            }
        }
        validate_environment(&self.env)?;
        if self.setup.steps.len() > MAX_LIFECYCLE_STEPS {
            bail!("workspace defines more than {MAX_LIFECYCLE_STEPS} setup steps");
        }
        for step in &self.setup.steps {
            validate_hook(&step.script, "setup step")?;
        }
        if self.init.steps.len() > MAX_LIFECYCLE_STEPS {
            bail!("workspace defines more than {MAX_LIFECYCLE_STEPS} init steps");
        }
        for step in &self.init.steps {
            validate_hook(&step.script, "init step")?;
        }
        if self.network.host_ports.len() > 64
            || self
                .network
                .host_ports
                .iter()
                .map(|mapping| mapping.pod_port)
                .collect::<BTreeSet<_>>()
                .len()
                != self.network.host_ports.len()
        {
            bail!("network host port pod-side ports must be unique and limited to 64 entries");
        }
        if self.caches.len() > MAX_WORKSPACE_CACHES {
            bail!("workspace defines more than {MAX_WORKSPACE_CACHES} caches");
        }
        validate_editor_config(&self.editors)?;
        validate_caches(&self.caches)?;
        let mut repository_paths = Vec::new();
        for (path, repository) in &self.repos {
            validate_repository_path(path)
                .with_context(|| format!("invalid workspace repository path {path:?}"))?;
            if repository.source.is_empty()
                || repository.source.len() > 4096
                || repository.source.chars().any(char::is_control)
                || repository.source.starts_with('-')
            {
                bail!("invalid source for workspace repository {path:?}");
            }
            if let Some(branch) = &repository.branch {
                validate_repository_branch(branch)
                    .with_context(|| format!("invalid branch for workspace repository {path:?}"))?;
            }
            let path = Path::new(path);
            if repository_paths
                .iter()
                .any(|existing: &&Path| path.starts_with(existing) || existing.starts_with(path))
            {
                bail!("workspace repository paths overlap at {}", path.display());
            }
            repository_paths.push(path);
        }
        Ok(())
    }
}

/// Converts generated feature settings into effective guest runtime values.
fn workspace_features_from_api(
    features: Option<config_api::WorkspaceFeaturesConfig>,
) -> WorkspaceFeatures {
    features.map_or_else(WorkspaceFeatures::default, |features| WorkspaceFeatures {
        docker: features.docker.unwrap_or(false),
        podman: features.podman.unwrap_or(false),
        virtualization: features.virtualization.unwrap_or(false),
        usb: features.usb.unwrap_or(false),
    })
}

/// Converts generated Nix settings into effective guest runtime values.
fn workspace_nix_from_api(nix: Option<config_api::WorkspaceNixConfig>) -> WorkspaceNix {
    nix.map_or_else(WorkspaceNix::default, |nix| WorkspaceNix {
        daemon: nix.daemon.unwrap_or(false),
    })
}

/// Converts generated editor settings into guest runtime values.
fn workspace_editors_from_api(
    editors: Option<config_api::WorkspaceEditorsConfig>,
) -> WorkspaceEditors {
    editors.map_or_else(WorkspaceEditors::default, |editors| WorkspaceEditors {
        code: editors
            .code
            .map_or_else(WorkspaceCodeEditor::default, |code| WorkspaceCodeEditor {
                extensions: code
                    .extensions
                    .unwrap_or_default()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            }),
    })
}

/// Validates the bounded workspace-wide editor configuration.
fn validate_editor_config(editors: &WorkspaceEditors) -> Result<()> {
    if editors.code.extensions.len() > MAX_CODE_EDITOR_EXTENSIONS
        || editors
            .code
            .extensions
            .iter()
            .any(|extension| !valid_extension_identifier(extension))
    {
        bail!(
            "Code editor extensions must be Marketplace publisher.name identifiers limited to {MAX_CODE_EDITOR_EXTENSIONS} entries"
        );
    }
    Ok(())
}

/// Validates cache identities, paths, and editor-profile isolation.
fn validate_caches(caches: &[WorkspaceCache]) -> Result<()> {
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for cache in caches {
        crate::runtime::pod::PodId::new(&cache.name)
            .with_context(|| format!("invalid cache name {:?}", cache.name))?;
        if !names.insert(&cache.name) {
            bail!("duplicate workspace cache name {:?}", cache.name);
        }
        if cache.name == CODE_EDITOR_CACHE_NAME {
            bail!("workspace cache name {CODE_EDITOR_CACHE_NAME:?} is reserved");
        }
        if cache
            .name
            .strip_prefix(HOST_SHARE_MOUNT_TAG_PREFIX)
            .and_then(|index| index.parse::<usize>().ok())
            .is_some_and(|index| index < MAX_WORKSPACE_HOST_SHARES)
        {
            bail!("workspace cache name {:?} is reserved", cache.name);
        }
        validate_cache_path(&cache.path)
            .with_context(|| format!("invalid path for workspace cache {:?}", cache.name))?;
        let cache_path = Path::new(&cache.path);
        let editor_path = Path::new(CODE_EDITOR_PROFILE_PATH);
        if cache_path.starts_with(editor_path) || editor_path.starts_with(cache_path) {
            bail!(
                "workspace cache path {:?} overlaps the shared Code editor profile",
                cache.path
            );
        }
        if !paths.insert(&cache.path) {
            bail!("duplicate workspace cache path {:?}", cache.path);
        }
    }
    Ok(())
}

/// Converts the generated network shape into guest runtime values.
fn workspace_network_from_api(
    network: config_api::WorkspaceNetworkConfig,
) -> Result<WorkspaceNetwork> {
    let parse_addresses = |values: Option<tascarrel_api::ArcVec<tascarrel_api::ArcStr>>,
                           purpose: &str|
     -> Result<Vec<IpAddr>> {
        values
            .unwrap_or_default()
            .iter()
            .map(|value| {
                value
                    .parse()
                    .with_context(|| format!("invalid {purpose} address {value:?}"))
            })
            .collect()
    };
    Ok(WorkspaceNetwork {
        host_ports: network
            .host_ports
            .unwrap_or_default()
            .iter()
            .map(|mapping| match mapping {
                config_api::WorkspaceHostPort::SamePort(port) if *port != 0 => {
                    Ok(WorkspaceHostPort {
                        host_port: *port,
                        pod_port: *port,
                    })
                }
                config_api::WorkspaceHostPort::SamePort(_) => {
                    bail!("network host port shorthand must be nonzero")
                }
                config_api::WorkspaceHostPort::Mapping(mapping) => {
                    let (host_port, pod_port) = mapping
                        .ports()
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    Ok(WorkspaceHostPort {
                        host_port,
                        pod_port,
                    })
                }
            })
            .collect::<Result<Vec<_>>>()?,
        default: network
            .default
            .map_or_else(String::new, |value| value.to_string()),
        allow_local: network.allow_local.unwrap_or(false),
        allow_addresses: parse_addresses(network.allow_addresses, "allowed")?,
        deny_addresses: parse_addresses(network.deny_addresses, "denied")?,
        allow_hosts: arc_strings(network.allow_hosts),
        deny_hosts: arc_strings(network.deny_hosts),
        allow_ports: network
            .allow_ports
            .unwrap_or_default()
            .iter()
            .copied()
            .collect(),
        secret_injection: network
            .secret_injection
            .unwrap_or_default()
            .iter()
            .map(|secret| WorkspaceSecretInjection {
                host: secret.host.to_string(),
                paths: secret
                    .paths
                    .as_ref()
                    .map(|paths| paths.iter().map(ToString::to_string).collect()),
                methods: secret.methods.iter().map(ToString::to_string).collect(),
                graphql_queries_only: secret.graphql.as_ref().is_some_and(|policy| {
                    matches!(policy, config_api::WorkspaceGraphQlPolicy::QueriesOnly)
                }),
                header: secret.header.as_ref().map(ToString::to_string),
                placeholder: secret.placeholder.as_ref().map(ToString::to_string),
                secret: secret.secret.to_string(),
            })
            .collect(),
        http_ports: network
            .http_ports
            .unwrap_or_default()
            .iter()
            .copied()
            .collect(),
        https_ports: network
            .https_ports
            .unwrap_or_default()
            .iter()
            .copied()
            .collect(),
    })
}

/// Copies optional generated strings into the runtime-owned representation.
fn arc_strings(values: Option<tascarrel_api::ArcVec<tascarrel_api::ArcStr>>) -> Vec<String> {
    values
        .unwrap_or_default()
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// Host-enforced policy mirrored here only to keep strict parsing of the shared
/// workspace file. Guestd never resolves or receives the referenced secrets.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceNetwork {
    pub host_ports: Vec<WorkspaceHostPort>,
    pub default: String,
    pub allow_local: bool,
    pub allow_addresses: Vec<IpAddr>,
    pub deny_addresses: Vec<IpAddr>,
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
    pub allow_ports: Vec<u16>,
    pub secret_injection: Vec<WorkspaceSecretInjection>,
    pub http_ports: Vec<u16>,
    pub https_ports: Vec<u16>,
}

/// One host-loopback port exposed at a pod-visible virtual port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkspaceHostPort {
    pub host_port: u16,
    pub pod_port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSecretInjection {
    pub host: String,
    pub paths: Option<Vec<String>>,
    pub methods: Vec<String>,
    pub graphql_queries_only: bool,
    pub header: Option<String>,
    pub placeholder: Option<String>,
    pub secret: String,
}

fn validate_hook(script: &str, name: &str) -> Result<()> {
    const MAX_BYTES: usize = 64 * 1024;
    if script.len() > MAX_BYTES {
        bail!("workspace {name} script exceeds {MAX_BYTES} bytes");
    }
    if script.contains('\0') {
        bail!("workspace {name} script contains a NUL byte");
    }
    Ok(())
}

fn valid_extension_identifier(extension: &str) -> bool {
    extension.len() <= CODE_EXTENSION_ID_MAX_BYTES
        && extension.split_once('.').is_some_and(|(publisher, name)| {
            !name.contains('.')
                && valid_extension_component(publisher)
                && valid_extension_component(name)
        })
}

fn valid_extension_component(component: &str) -> bool {
    !component.is_empty()
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && component
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && component
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

/// Ordered steps used to prepare the reusable workspace seed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceSetup {
    pub steps: Vec<WorkspaceSetupStep>,
}

/// One readiness-blocking workspace setup step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSetupStep {
    pub script: String,
}

/// Ordered steps run whenever a pod starts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceInit {
    pub steps: Vec<WorkspaceInitStep>,
}

/// One per-pod init step. Waiting is opt-in and applies only to this step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceInitStep {
    pub script: String,
    pub wait: bool,
}

fn validate_environment(environment: &BTreeMap<String, String>) -> Result<()> {
    if environment.len() > MAX_ENVIRONMENT_ENTRIES {
        bail!("workspace environment has more than {MAX_ENVIRONMENT_ENTRIES} entries");
    }
    let mut size = 0_usize;
    for (name, value) in environment {
        let mut bytes = name.bytes();
        if !bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || value.contains('\0')
        {
            bail!("invalid workspace environment variable {name:?}");
        }
        size = size
            .checked_add(name.len() + value.len() + 1)
            .ok_or_else(|| anyhow::anyhow!("workspace environment size overflowed"))?;
    }
    if size > MAX_ENVIRONMENT_BYTES {
        bail!("workspace environment exceeds {MAX_ENVIRONMENT_BYTES} bytes");
    }
    Ok(())
}

fn validate_relative_path(value: &str, purpose: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 1024
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("{purpose} must be a non-empty normalized relative path");
    }
    Ok(())
}

/// One Git repository materialized below `/workspace`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRepository {
    pub source: String,
    /// Short branch name selected for the managed checkout.
    pub branch: Option<String>,
}

fn validate_repository_path(value: &str) -> Result<()> {
    validate_relative_path(value, "repository path")
}

fn validate_repository_branch(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 1024 || value.starts_with('-') {
        bail!("repository branch must be a valid short branch name");
    }
    tascarrel_git::ReferenceName::new(format!("refs/heads/{value}"))
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// One persistent workspace-level cache mounted into every pod.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceCache {
    /// Stable backing-subvolume name. Removing a declaration does not delete
    /// it.
    pub name: String,
    /// Absolute pod path, or `~`/`~/...` relative to the image user's home.
    pub path: String,
}

fn validate_cache_path(value: &str) -> Result<()> {
    let path = if value == "~" {
        return Ok(());
    } else if let Some(relative) = value.strip_prefix("~/") {
        if relative.is_empty() {
            bail!("use `~` rather than `~/`");
        }
        Path::new(relative)
    } else {
        if value.starts_with('~') {
            bail!("only `~` and `~/...` home expansion are supported");
        }
        let path = Path::new(value);
        if !path.is_absolute() {
            bail!("cache path must be absolute or begin with `~/`");
        }
        path
    };
    if path.components().any(|component| {
        matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::Prefix(_)
        )
    }) {
        bail!("cache path must be normalized and may not contain `.` or `..`");
    }
    Ok(())
}

/// Turnkey development features made available inside every pod.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "workspace features are independent configuration switches"
)]
pub struct WorkspaceFeatures {
    /// Start a confined rootful Docker daemon in every pod and inject its
    /// client. This implicitly enables the nested-container capability.
    pub docker: bool,
    /// Inject Podman and implicitly enable rootless-container facilities.
    pub podman: bool,
    /// Expose the workspace VM's KVM device for virtualization.
    pub virtualization: bool,
    /// Allow clients to forward host USB devices into the workspace VM.
    pub usb: bool,
}

/// Workspace-wide Nix integration made available inside every pod.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceNix {
    /// Expose the VM's Nix daemon socket and read-only store to every pod.
    pub daemon: bool,
}

/// Workspace-wide editor integration configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceEditors {
    /// Browser-hosted Code editor configuration.
    pub code: WorkspaceCodeEditor,
}

/// Workspace-wide browser-hosted Code editor configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceCodeEditor {
    /// Additional Marketplace extensions installed into the shared profile.
    pub extensions: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::*;

    /// Verifies absent configuration leaves every optional facility disabled.
    #[test]
    fn missing_and_empty_configs_disable_optional_features() {
        let directory = tempdir().unwrap();
        assert_eq!(
            WorkspaceConfig::load(&directory.path().join("missing.toml"))
                .unwrap()
                .features,
            WorkspaceFeatures::default()
        );
        let empty = directory.path().join("empty.toml");
        fs::write(&empty, "# defaults\n").unwrap();
        let defaults = WorkspaceConfig::load(&empty).unwrap();
        assert_eq!(defaults.features, WorkspaceFeatures::default());
        assert!(!defaults.rootless_containers());
        assert!(!defaults.nested_containers());
        assert_eq!(defaults.nix, WorkspaceNix::default());
        assert!(defaults.editors.code.extensions.is_empty());
        assert!(defaults.env.is_empty());
        assert!(defaults.setup.steps.is_empty());
        assert!(defaults.init.steps.is_empty());
        assert!(defaults.caches.is_empty());
        assert!(defaults.repos.is_empty());
        assert!(defaults.host_shares.is_empty());
    }

    /// Verifies the named host-share table is accepted while host paths remain
    /// outside the guest runtime model.
    #[test]
    fn accepts_named_host_shares() {
        let configured = WorkspaceConfig::decode(
            "[shares.source]\npath = \"~/src\"\nmode = \"Overlay\"\n\n[shares.output]\npath = \"/srv/output\"\nmode = \"ReadWrite\"\n",
        )
        .unwrap();
        configured.validate().unwrap();
        assert_eq!(
            configured.host_shares,
            BTreeSet::from(["output".to_owned(), "source".to_owned()])
        );
        assert!(
            WorkspaceConfig::decode("[shares.source]\npath = \"~/src\"\n").is_err(),
            "the breaking mode field must be explicit"
        );
    }

    #[test]
    fn invalid_configuration_degrades_to_safe_defaults() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(&config, "[[cache]]\nname = 'wrong'\npath = '~/cache'\n").unwrap();
        let (loaded, diagnostic) = WorkspaceConfig::load_degraded(&config);
        assert!(loaded.caches.is_empty());
        assert!(
            diagnostic
                .unwrap()
                .contains("workspace config contains unknown fields: cache")
        );
    }

    /// Verifies Code extensions are workspace-wide Marketplace identifiers.
    #[test]
    fn code_editor_extensions_are_workspace_wide_and_validated() {
        let configured = WorkspaceConfig::decode(
            "[editors.code]\nextensions = [\"jnoortheen.nix-ide\", \"rust-lang.rust-analyzer\"]\n",
        )
        .unwrap();
        configured.validate().unwrap();
        assert_eq!(
            configured.editors.code.extensions,
            ["jnoortheen.nix-ide", "rust-lang.rust-analyzer"]
        );

        let invalid = WorkspaceConfig::decode(
            "[editors.code]\nextensions = [\"not-a-marketplace-identifier\"]\n",
        )
        .unwrap();
        assert!(invalid.validate().is_err());
    }

    /// Verifies the generated config shape is authoritative at nested paths.
    #[test]
    fn rejects_unknown_and_legacy_lifecycle_fields() {
        assert!(WorkspaceConfig::decode("[[setup.step]]\nscript = 'legacy'\n").is_err());
        let error = WorkspaceConfig::decode("[features]\npodman = true\nextra = 1\n")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("features") && error.contains("extra"),
            "{error}"
        );
    }

    /// Verifies guestd accepts configs above the former 64 KiB limit while
    /// retaining the authoritative four MiB bound.
    #[test]
    fn workspace_config_uses_shared_size_limit() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(&config, format!("#{}\n", "x".repeat(128 * 1024))).unwrap();
        WorkspaceConfig::load(&config).unwrap();

        fs::write(
            &config,
            format!(
                "#{}\n",
                "x".repeat(
                    usize::try_from(MAX_WORKSPACE_CONFIG_BYTES)
                        .expect("the test configuration limit fits in usize"),
                )
            ),
        )
        .unwrap();
        assert!(WorkspaceConfig::load(&config).is_err());
    }

    #[test]
    fn vm_resources_use_kebab_case_and_must_be_positive() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[vm]\ncores = 8\nmemory = \"16G\"\ndisk = \"1T\"\n",
        )
        .unwrap();
        let parsed = WorkspaceConfig::load(&config).unwrap();
        assert_eq!(parsed.vm.cores, Some(8));
        assert_eq!(parsed.vm.memory.as_deref(), Some("16G"));
        assert_eq!(parsed.vm.disk.as_deref(), Some("1T"));

        for contents in [
            "[vm]\ncores = 0\n",
            "[vm]\nmemory = \"0G\"\n",
            "[vm]\nmemory = \"16384\"\n",
            "[vm]\nmemory-mib = 16384\n",
            "[vm]\ndisk = \"128M\"\n",
            "[vm]\ndisk = \"1024\"\n",
        ] {
            fs::write(&config, contents).unwrap();
            assert!(WorkspaceConfig::load(&config).is_err());
        }
    }

    #[test]
    fn setup_steps_are_ordered_bounded_shell_scripts() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[[setup.steps]]\nscript = '''\nprintf ready > marker\ngit status\n'''\n\
             [[setup.steps]]\nscript = \"printf second\"\n",
        )
        .unwrap();
        let parsed = WorkspaceConfig::load(&config).unwrap();
        assert_eq!(parsed.setup.steps.len(), 2);
        assert_eq!(
            parsed.setup.steps[0].script,
            "printf ready > marker\ngit status\n"
        );

        fs::write(&config, "[[setup.steps]]\nscript = \"bad\\u0000script\"\n").unwrap();
        assert!(WorkspaceConfig::load(&config).is_err());
    }

    #[test]
    fn init_steps_have_independent_opt_in_waiting() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[[init.steps]]\nscript = 'printf first'\n\
             [[init.steps]]\nscript = 'printf initialized > marker'\nwait = true\n",
        )
        .unwrap();
        let parsed = WorkspaceConfig::load(&config).unwrap();
        assert_eq!(parsed.init.steps.len(), 2);
        assert!(!parsed.init.steps[0].wait);
        assert!(parsed.init.steps[1].wait);

        fs::write(&config, "[[init.steps]]\nscript = \"bad\\u0000script\"\n").unwrap();
        assert!(WorkspaceConfig::load(&config).is_err());
    }

    /// Verifies static host-port configuration accepts same-port shorthands
    /// and explicit host-to-pod mappings while rejecting duplicate pod ports.
    #[test]
    fn network_host_ports_accept_mappings_and_unique_pod_ports() {
        let config =
            WorkspaceConfig::decode("[network]\nhost-ports = [3000, \"5432:15432\"]\n").unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.network.host_ports,
            [
                WorkspaceHostPort {
                    host_port: 3000,
                    pod_port: 3000,
                },
                WorkspaceHostPort {
                    host_port: 5432,
                    pod_port: 15432,
                },
            ]
        );
        let duplicate =
            WorkspaceConfig::decode("[network]\nhost-ports = [3000, \"8080:3000\"]\n").unwrap();
        assert!(duplicate.validate().is_err());
        let zero = WorkspaceConfig::decode("[network]\nhost-ports = [0]\n");
        assert!(zero.is_err());
    }

    #[test]
    fn env_is_validated_and_legacy_environment_is_rejected() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[env]\nEDITOR = \"vim\"\nAPI_TOKEN = \"placeholder\"\n",
        )
        .unwrap();
        let parsed = WorkspaceConfig::load(&config).unwrap();
        assert_eq!(parsed.env["EDITOR"], "vim");

        for (name, contents) in [
            ("bad-env", "[env]\n\"NOT-VALID\" = \"x\"\n"),
            ("legacy-environment", "[environment]\nEDITOR = \"vim\"\n"),
            ("legacy-workspace", "[workspace]\noverlay = \"overlay\"\n"),
        ] {
            let path = directory.path().join(format!("{name}.toml"));
            fs::write(&path, contents).unwrap();
            assert!(WorkspaceConfig::load(&path).is_err(), "accepted {name}");
        }
    }

    /// Verifies the guest accepts path-scoped, provider-qualified secret
    /// references in shared network policy.
    #[test]
    fn shared_network_section_is_accepted_by_the_guest_parser() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[network]\ndefault = \"deny\"\nallow-hosts = [\"api.example\"]\n\
             allow-ports = [80, 443]\n\
             host-ports = [3000, \"5432:15432\"]\n\
             [secrets.providers.project]\nkind = \"sops\"\n\
             [[network.secret-injection]]\nhost = \"api.example\"\n\
             paths = [\"/graphql\"]\n\
             methods = [\"POST\"]\n\
             graphql = \"QueriesOnly\"\n\
             secret = \"project.TOKEN\"\n",
        )
        .unwrap();
        let parsed = WorkspaceConfig::load(&config).unwrap();
        assert_eq!(parsed.network.default, "deny");
        assert_eq!(parsed.network.allow_ports, [80, 443]);
        assert_eq!(
            parsed.network.host_ports,
            [
                WorkspaceHostPort {
                    host_port: 3000,
                    pod_port: 3000,
                },
                WorkspaceHostPort {
                    host_port: 5432,
                    pod_port: 15432,
                },
            ]
        );
        assert_eq!(
            parsed.network.secret_injection[0].paths.as_deref(),
            Some(["/graphql".to_owned()].as_slice())
        );
        assert_eq!(parsed.network.secret_injection[0].methods, ["POST"]);
        assert!(parsed.network.secret_injection[0].graphql_queries_only);
        assert!(parsed.network.secret_injection[0].header.is_none());
        assert_eq!(parsed.network.secret_injection[0].secret, "project.TOKEN");
    }

    #[test]
    fn repositories_use_quoted_relative_paths() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[repos.\"src/project\"]\nsource = \"https://example.invalid/project.git\"\nbranch = \"release/next\"\n",
        )
        .unwrap();
        let parsed = WorkspaceConfig::load(&config).unwrap();
        assert_eq!(
            parsed.repos["src/project"].source,
            "https://example.invalid/project.git"
        );
        assert_eq!(
            parsed.repos["src/project"].branch.as_deref(),
            Some("release/next")
        );
    }

    /// Host-owned Git policy remains valid in guestd's strict shared parser.
    #[test]
    fn repository_git_policies_are_accepted_as_host_owned_configuration() {
        let config = WorkspaceConfig::decode(
            "[git]\ndefault-policy = 'allow'\n\
             [[git.tags]]\npattern = '**'\npolicy = 'require-approval'\n\
             [repos.project]\nsource = 'https://example.invalid/project.git'\n\
             [repos.project.git]\ndefault-policy = 'deny'\n",
        )
        .unwrap();

        assert_eq!(
            config.repos["project"].source,
            "https://example.invalid/project.git"
        );
    }

    #[test]
    fn repository_paths_may_not_escape_or_overlap() {
        let directory = tempdir().unwrap();
        for (name, contents) in [
            (
                "escape",
                "[repos.\"../escape\"]\nsource = \"https://example.invalid/a.git\"\n",
            ),
            (
                "overlap",
                "[repos.\"src\"]\nsource = \"https://example.invalid/a.git\"\n\
                 [repos.\"src/nested\"]\nsource = \"https://example.invalid/b.git\"\n",
            ),
            (
                "branch",
                "[repos.project]\nsource = \"https://example.invalid/a.git\"\nbranch = \"../escape\"\n",
            ),
        ] {
            let path = directory.path().join(format!("{name}.toml"));
            fs::write(&path, contents).unwrap();
            assert!(WorkspaceConfig::load(&path).is_err());
        }
    }

    /// Verifies only the Podman feature enables rootless-container facilities.
    #[test]
    fn podman_is_the_only_rootless_container_feature() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(&config, "[features]\npodman = true\n").unwrap();
        let config = WorkspaceConfig::load(&config).unwrap();
        assert!(config.features.podman);
        assert!(config.rootless_containers());
        assert!(!config.nested_containers());
    }

    /// Verifies virtualization is disabled unless explicitly enabled.
    #[test]
    fn virtualization_is_opt_in() {
        let default = WorkspaceConfig::decode("").unwrap();
        assert!(!default.features.virtualization);

        let enabled = WorkspaceConfig::decode("[features]\nvirtualization = true\n").unwrap();
        assert!(enabled.features.virtualization);
    }

    /// Verifies Nix daemon selection is independent of container features.
    #[test]
    fn nix_daemon_and_container_features_are_independent() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(&config, "[nix]\ndaemon = true\n").unwrap();
        let config = WorkspaceConfig::load(&config).unwrap();
        assert!(!config.features.docker);
        assert!(!config.features.podman);
        assert!(config.nix.daemon);
        assert!(!config.nested_containers());
        assert!(!config.rootless_containers());
    }

    /// Verifies features are the sole source of their low-level facilities.
    #[test]
    fn container_features_derive_their_runtime_facilities() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[features]\ndocker = true\npodman = true\nvirtualization = true\n",
        )
        .unwrap();
        let config = WorkspaceConfig::load(&config).unwrap();
        assert!(config.features.docker);
        assert!(config.features.podman);
        assert!(config.features.virtualization);
        assert!(config.nested_containers());
        assert!(config.rootless_containers());
    }

    /// Verifies unsafe files and removed or unknown fields are rejected.
    #[test]
    fn unsafe_or_unknown_config_is_rejected() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("target.toml");
        fs::write(&target, "").unwrap();
        let link = directory.path().join("config.toml");
        symlink(&target, &link).unwrap();
        assert!(WorkspaceConfig::load(&link).is_err());

        let unknown = directory.path().join("unknown.toml");
        fs::write(&unknown, "unknown = true\n").unwrap();
        assert!(WorkspaceConfig::load(&unknown).is_err());

        for (name, contents) in [
            (
                "removed-capabilities",
                "[capabilities]\nrootless-containers = true\n",
            ),
            (
                "removed-feature",
                "[features]\nnested-virtualization = true\n",
            ),
        ] {
            let path = directory.path().join(format!("{name}.toml"));
            fs::write(&path, contents).unwrap();
            assert!(WorkspaceConfig::load(&path).is_err());
        }

        for (name, contents) in [
            (
                "legacy-docker-service",
                "[services]\ndocker-daemon = true\n",
            ),
            ("legacy-nix-service", "[services]\nnix-daemon = true\n"),
            ("legacy-nix-spelling", "[nix]\nnix_daemon = true\n"),
        ] {
            let path = directory.path().join(format!("{name}.toml"));
            fs::write(&path, contents).unwrap();
            assert!(WorkspaceConfig::load(&path).is_err());
        }

        for (name, contents) in [
            (
                "legacy-shares",
                "[[shares]]\nname = \"cargo\"\npath = \"~/.cache/cargo\"\n",
            ),
            ("legacy-setup", "setup = \"printf old\"\n"),
            ("legacy-init", "init = \"printf old\"\ninit-wait = true\n"),
            ("legacy-egress", "[egress]\ndefault = \"deny\"\n"),
            ("legacy-ingress", "[ingress]\npublish = [3000]\n"),
        ] {
            let path = directory.path().join(format!("{name}.toml"));
            fs::write(&path, contents).unwrap();
            assert!(WorkspaceConfig::load(&path).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn caches_accept_absolute_and_image_home_paths() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(
            &config,
            "[[caches]]\nname = \"cargo\"\npath = \"~/.cargo/registry\"\n\
             [[caches]]\nname = \"compiler-cache\"\npath = \"/var/cache/compiler\"\n",
        )
        .unwrap();
        let caches = WorkspaceConfig::load(&config).unwrap().caches;
        assert_eq!(caches.len(), 2);
        assert_eq!(caches[0].path, "~/.cargo/registry");
    }

    #[test]
    fn unsafe_or_duplicate_caches_are_rejected() {
        let directory = tempdir().unwrap();
        for (name, contents) in [
            (
                "duplicate-name",
                "[[caches]]\nname = \"cache\"\npath = \"/cache/a\"\n\
                 [[caches]]\nname = \"cache\"\npath = \"/cache/b\"\n",
            ),
            (
                "duplicate-path",
                "[[caches]]\nname = \"one\"\npath = \"~/cache\"\n\
                 [[caches]]\nname = \"two\"\npath = \"~/cache\"\n",
            ),
            (
                "parent-traversal",
                "[[caches]]\nname = \"cache\"\npath = \"~/../escape\"\n",
            ),
            (
                "named-home",
                "[[caches]]\nname = \"cache\"\npath = \"~root/cache\"\n",
            ),
            (
                "editor-profile-overlap",
                "[[caches]]\nname = \"cache\"\npath = \"~/.tascarrel/editors/code\"\n",
            ),
            (
                "host-share-runtime-name",
                "[[caches]]\nname = \"tascarrel-share-0\"\npath = \"~/.cache/runtime\"\n",
            ),
        ] {
            let path = directory.path().join(format!("{name}.toml"));
            fs::write(&path, contents).unwrap();
            assert!(WorkspaceConfig::load(&path).is_err(), "accepted {name}");
        }
    }

    /// Verifies USB forwarding is a boolean feature and legacy selectors are
    /// rejected.
    #[test]
    fn usb_is_an_opt_in_workspace_feature() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        fs::write(&config, "[features]\nusb = true\n").unwrap();
        assert!(WorkspaceConfig::load(&config).unwrap().features.usb);

        fs::write(&config, "[[usb]]\nname = \"board\"\n").unwrap();
        assert!(WorkspaceConfig::load(&config).is_err());
    }
}
