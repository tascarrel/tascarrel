//! Host capability inspection for interactive diagnostics and server startup.
//!
//! [`inspect`] includes service-manager checks for administrative commands,
//! while [`inspect_runtime`] limits checks to dependencies required by the
//! running server.

use std::env;
use std::fs::OpenOptions;
use std::fs::{self};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde::Serialize;
use tascarrel_vm::Architecture;

#[derive(Clone, Debug)]
pub struct ResolvedDependencies {
    pub qemu: PathBuf,
    pub git: PathBuf,
    pub sops: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct DependencyCheck {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub architecture: String,
    pub checks: Vec<DependencyCheck>,
    #[serde(skip)]
    resolved: Option<ResolvedDependencies>,
}

impl DoctorReport {
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.status != CheckStatus::Error)
    }

    /// Returns resolved required executables when all required checks passed.
    ///
    /// # Errors
    ///
    /// Returns an error summarizing failed checks or incomplete dependency
    /// resolution.
    pub fn require(self) -> Result<ResolvedDependencies> {
        if !self.is_healthy() {
            let failures = self
                .checks
                .iter()
                .filter(|check| check.status == CheckStatus::Error)
                .map(|check| format!("{}: {}", check.name, check.message))
                .collect::<Vec<_>>()
                .join("; ");
            bail!("host dependency checks failed: {failures}");
        }
        self.resolved
            .ok_or_else(|| anyhow!("host dependency resolution was incomplete"))
    }
}

/// Inspects runtime dependencies and the per-user service manager.
#[must_use]
pub fn inspect() -> DoctorReport {
    inspect_with_service_manager(true)
}

/// Checks only the dependencies needed by an in-process host daemon.
#[must_use]
pub fn inspect_runtime() -> DoctorReport {
    inspect_with_service_manager(false)
}

fn inspect_with_service_manager(include_service_manager: bool) -> DoctorReport {
    let architecture = match Architecture::host() {
        Ok(architecture) => architecture,
        Err(error) => {
            return DoctorReport {
                architecture: env::consts::ARCH.to_owned(),
                checks: vec![error_check("architecture", error.to_string())],
                resolved: None,
            };
        }
    };
    let mut checks = Vec::new();

    let qemu = resolve_executable("TASCARREL_QEMU", architecture.qemu_binary());
    let qemu = record_executable(&mut checks, architecture.qemu_binary(), qemu, true);
    let git = resolve_executable("TASCARREL_GIT", "git");
    let git = record_executable(&mut checks, "git", git, true);
    let sops = resolve_executable("TASCARREL_SOPS", "sops");
    let sops = record_executable(&mut checks, "sops", sops, false);

    if let Some(qemu) = &qemu {
        check_command_version(&mut checks, architecture.qemu_binary(), qemu, true);
        check_qemu_capabilities(&mut checks, qemu);
    }
    if let Some(git) = &git {
        check_command_version(&mut checks, "git", git, true);
    }
    if let Some(sops) = &sops {
        check_command_version(&mut checks, "sops", sops, false);
    }
    check_acceleration(&mut checks, qemu.as_deref());
    if include_service_manager {
        check_service_manager(&mut checks);
    }

    let resolved = qemu.zip(git).map(|(qemu, git)| ResolvedDependencies {
        qemu,
        git,
        sops: sops.unwrap_or_else(|| PathBuf::from("sops")),
    });
    DoctorReport {
        architecture: architecture.to_string(),
        checks,
        resolved,
    }
}

#[cfg(target_os = "linux")]
fn check_service_manager(checks: &mut Vec<DependencyCheck>) {
    let systemctl = find_in_path("systemctl");
    record_executable(checks, "systemctl", systemctl.clone(), true);
    if let Some(systemctl) = systemctl {
        check_command_version(checks, "systemctl", &systemctl, true);
    }
    record_executable(checks, "journalctl", find_in_path("journalctl"), true);
}

#[cfg(target_os = "macos")]
fn check_service_manager(checks: &mut Vec<DependencyCheck>) {
    record_executable(checks, "launchctl", find_in_path("launchctl"), true);
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn check_service_manager(_checks: &mut Vec<DependencyCheck>) {}

fn record_executable(
    checks: &mut Vec<DependencyCheck>,
    name: &str,
    path: Option<PathBuf>,
    required: bool,
) -> Option<PathBuf> {
    if let Some(path) = path {
        checks.push(ok_check(name, format!("found {}", path.display())));
        Some(path)
    } else {
        let message = dependency_env(name).map_or_else(
            || "not found in PATH".to_owned(),
            |environment| format!("not found in PATH; set {environment} to override"),
        );
        checks.push(if required {
            error_check(name, message)
        } else {
            let feature = if name == "sops" {
                "SOPS secret providers"
            } else {
                "optional features"
            };
            warning_check(name, format!("{message}; {feature} will be unavailable"))
        });
        None
    }
}

fn dependency_env(name: &str) -> Option<&'static str> {
    match name {
        "git" => Some("TASCARREL_GIT"),
        "sops" => Some("TASCARREL_SOPS"),
        name if name.starts_with("qemu-system-") => Some("TASCARREL_QEMU"),
        _ => None,
    }
}

fn check_command_version(
    checks: &mut Vec<DependencyCheck>,
    name: &str,
    command: &Path,
    required: bool,
) {
    let result = Command::new(command).arg("--version").output();
    match result {
        Ok(output) if output.status.success() => {
            let detail = first_output_line(&output.stdout, &output.stderr);
            checks.push(ok_check(format!("{name} invocation"), detail));
        }
        Ok(output) => {
            let detail = format!(
                "exited with {}; {}",
                output.status,
                first_output_line(&output.stdout, &output.stderr)
            );
            checks.push(if required {
                error_check(format!("{name} invocation"), detail)
            } else {
                warning_check(format!("{name} invocation"), detail)
            });
        }
        Err(error) => checks.push(if required {
            error_check(format!("{name} invocation"), error.to_string())
        } else {
            warning_check(format!("{name} invocation"), error.to_string())
        }),
    }
}

fn check_qemu_capabilities(checks: &mut Vec<DependencyCheck>, qemu: &Path) {
    check_qemu_query(checks, qemu, &["-accel", "help"], required_accelerator());
    check_qemu_query(checks, qemu, &["-device", "help"], "virtio-serial-pci");
    #[cfg(target_os = "linux")]
    for device in ["qemu-xhci", "usb-host"] {
        check_qemu_query(checks, qemu, &["-device", "help"], device);
    }
}

fn check_qemu_query(
    checks: &mut Vec<DependencyCheck>,
    qemu: &Path,
    arguments: &[&str],
    capability: &str,
) {
    match Command::new(qemu).args(arguments).output() {
        Ok(output)
            if output.status.success()
                && combined_output(&output.stdout, &output.stderr).contains(capability) =>
        {
            checks.push(ok_check(
                format!("QEMU {capability}"),
                "supported".to_owned(),
            ));
        }
        Ok(output) if output.status.success() => checks.push(error_check(
            format!("QEMU {capability}"),
            "capability was not reported by QEMU",
        )),
        Ok(output) => checks.push(error_check(
            format!("QEMU {capability}"),
            format!(
                "capability query exited with {}; {}",
                output.status,
                first_output_line(&output.stdout, &output.stderr)
            ),
        )),
        Err(error) => checks.push(error_check(format!("QEMU {capability}"), error.to_string())),
    }
}

#[cfg(target_os = "linux")]
fn required_accelerator() -> &'static str {
    "kvm"
}

#[cfg(target_os = "macos")]
fn required_accelerator() -> &'static str {
    "hvf"
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn required_accelerator() -> &'static str {
    "tcg"
}

#[cfg(target_os = "linux")]
fn check_acceleration(checks: &mut Vec<DependencyCheck>, _qemu: Option<&Path>) {
    let path = Path::new("/dev/kvm");
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => checks.push(ok_check("KVM access", "read/write access to /dev/kvm")),
        Err(error) => checks.push(error_check(
            "KVM access",
            format!("cannot open /dev/kvm read/write: {error}"),
        )),
    }
}

#[cfg(target_os = "macos")]
fn check_acceleration(checks: &mut Vec<DependencyCheck>, _qemu: Option<&Path>) {
    checks.push(ok_check(
        "HVF access",
        "QEMU reports the host accelerator".to_owned(),
    ));
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn check_acceleration(checks: &mut Vec<DependencyCheck>, _qemu: Option<&Path>) {
    checks.push(error_check(
        "host operating system",
        "Tascarrel supports only Linux and macOS hosts",
    ));
}

fn resolve_executable(environment: &str, fallback: &str) -> Option<PathBuf> {
    match env::var_os(environment).filter(|value| !value.is_empty()) {
        Some(value) => executable(PathBuf::from(value)),
        None => find_in_path(fallback),
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find_map(executable)
}

fn executable(path: PathBuf) -> Option<PathBuf> {
    let metadata = fs::metadata(&path).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .then(|| fs::canonicalize(&path).unwrap_or(path))
}

fn combined_output(stdout: &[u8], stderr: &[u8]) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    )
}

fn first_output_line(stdout: &[u8], stderr: &[u8]) -> String {
    combined_output(stdout, stderr)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_owned()
}

fn ok_check(name: impl Into<String>, message: impl Into<String>) -> DependencyCheck {
    DependencyCheck {
        name: name.into(),
        status: CheckStatus::Ok,
        message: message.into(),
    }
}

fn warning_check(name: impl Into<String>, message: impl Into<String>) -> DependencyCheck {
    DependencyCheck {
        name: name.into(),
        status: CheckStatus::Warning,
        message: message.into(),
    }
}

fn error_check(name: impl Into<String>, message: impl Into<String>) -> DependencyCheck {
    DependencyCheck {
        name: name.into(),
        status: CheckStatus::Error,
        message: message.into(),
    }
}

/// Prints a human-readable or JSON diagnostic report.
///
/// # Errors
///
/// Returns an error when JSON serialization or output fails.
pub fn print(report: &DoctorReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!("Host architecture: {}", report.architecture);
    for check in &report.checks {
        let marker = match check.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warning => "warning",
            CheckStatus::Error => "error",
        };
        println!("[{marker}] {}: {}", check.name, check.message);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_health_depends_only_on_hard_errors() {
        let report = DoctorReport {
            architecture: "test".to_owned(),
            checks: vec![warning_check("git", "missing")],
            resolved: None,
        };
        assert!(report.is_healthy());
        let report = DoctorReport {
            architecture: "test".to_owned(),
            checks: vec![error_check("qemu", "missing")],
            resolved: None,
        };
        assert!(!report.is_healthy());
    }

    #[test]
    fn executable_resolution_rejects_non_executable_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tool");
        fs::write(&path, b"tool").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(executable(path.clone()), None);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(executable(path).is_some());
    }
}
