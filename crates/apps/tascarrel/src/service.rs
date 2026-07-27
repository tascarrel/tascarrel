//! Per-user service-manager integration for the Tascarrel server.
//!
//! The public operations install, start, stop, inspect, and read logs from the
//! `systemd` user service on Linux or `LaunchAgent` on macOS.

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;

use crate::doctor::ResolvedDependencies;
use crate::install::atomic_write;
use crate::install::create_directory;

#[cfg(target_os = "linux")]
const LINUX_UNIT: &str = "tascarrel.service";
#[cfg(target_os = "macos")]
const MACOS_LABEL: &str = "dev.tascarrel.host";

/// Writes the per-user service definition for a server executable.
///
/// # Errors
///
/// Returns an error when state paths cannot be discovered or the service
/// definition cannot be written.
pub fn install(binary: &Path, dependencies: &ResolvedDependencies) -> Result<()> {
    let tascarrel_home =
        tascarrel_host::TascarrelHome::discover().map_err(|error| anyhow!(error.to_string()))?;
    #[cfg(target_os = "linux")]
    {
        let path = linux_unit_path()?;
        let contents = linux_unit(binary, dependencies, tascarrel_home.root())?;
        atomic_write(&path, contents.as_bytes(), 0o600)?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let path = macos_plist_path()?;
        let contents = macos_plist(binary, dependencies, tascarrel_home.root())?;
        atomic_write(&path, contents.as_bytes(), 0o600)?;
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        bail!("Tascarrel daemon services support only Linux and macOS")
    }
}

/// Starts the installed per-user Tascarrel service.
///
/// # Errors
///
/// Returns an error when the platform service manager is unavailable or
/// rejects the operation.
pub fn start() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        reload_linux_user_manager()?;
        run_checked(
            Command::new("systemctl")
                .args(["--user", "start", LINUX_UNIT])
                .stdin(Stdio::null()),
            "start the Tascarrel user service",
        )
    }
    #[cfg(target_os = "macos")]
    {
        let domain = launchd_domain()?;
        let service = format!("{domain}/{MACOS_LABEL}");
        let plist = macos_plist_path()?;
        let bootstrapped = Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(&plist)
            .status()
            .context("bootstrap the Tascarrel LaunchAgent")?;
        if !bootstrapped.success() {
            let printed = Command::new("launchctl")
                .arg("print")
                .arg(&service)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("inspect the Tascarrel LaunchAgent")?;
            if !printed.success() {
                bail!("could not bootstrap the Tascarrel LaunchAgent");
            }
        }
        run_checked(
            Command::new("launchctl")
                .args(["kickstart", "-k"])
                .arg(service),
            "start the Tascarrel LaunchAgent",
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        bail!("Tascarrel daemon services support only Linux and macOS")
    }
}

/// Stops the installed per-user Tascarrel service.
///
/// # Errors
///
/// Returns an error when the platform service manager is unavailable or
/// rejects the operation.
pub fn stop() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        run_checked(
            Command::new("systemctl")
                .args(["--user", "stop", LINUX_UNIT])
                .stdin(Stdio::null()),
            "stop the Tascarrel user service",
        )
    }
    #[cfg(target_os = "macos")]
    {
        run_checked(
            Command::new("launchctl")
                .arg("bootout")
                .arg(format!("{}/{MACOS_LABEL}", launchd_domain()?)),
            "stop the Tascarrel LaunchAgent",
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        bail!("Tascarrel daemon services support only Linux and macOS")
    }
}

#[cfg(target_os = "linux")]
fn reload_linux_user_manager() -> Result<()> {
    run_checked(
        Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .stdin(Stdio::null()),
        "reload the systemd user manager",
    )
}

/// Restarts the installed per-user Tascarrel service.
///
/// # Errors
///
/// Returns an error when the platform service manager is unavailable or
/// rejects the operation.
pub fn restart() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        reload_linux_user_manager()?;
        run_checked(
            Command::new("systemctl")
                .args(["--user", "restart", LINUX_UNIT])
                .stdin(Stdio::null()),
            "restart the Tascarrel user service",
        )
    }
    #[cfg(target_os = "macos")]
    {
        stop()?;
        start()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        bail!("Tascarrel daemon services support only Linux and macOS")
    }
}

/// Reports whether the installed per-user service is active.
///
/// # Errors
///
/// Returns an error when the platform service manager cannot be queried.
pub fn status() -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        let status = Command::new("systemctl")
            .args(["--user", "status", LINUX_UNIT, "--no-pager"])
            .status()
            .context("inspect the Tascarrel user service")?;
        Ok(status.success())
    }
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("launchctl")
            .arg("print")
            .arg(format!("{}/{MACOS_LABEL}", launchd_domain()?))
            .status()
            .context("inspect the Tascarrel LaunchAgent")?;
        Ok(status.success())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        bail!("Tascarrel daemon services support only Linux and macOS")
    }
}

/// Displays recent service logs, optionally following new records.
///
/// # Errors
///
/// Returns an error when the platform log reader is unavailable or exits
/// unsuccessfully.
pub fn logs(follow: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("journalctl");
        command.args(["--user", "--unit", LINUX_UNIT]);
        if follow {
            command.arg("--follow");
        } else {
            command.args(["--no-pager", "--lines", "200"]);
        }
        run_checked(&mut command, "read Tascarrel service logs")
    }
    #[cfg(target_os = "macos")]
    {
        let log = macos_log_path()?;
        let mut command = Command::new("tail");
        if follow {
            command.arg("-f");
        } else {
            command.args(["-n", "200"]);
        }
        command.arg(log);
        run_checked(&mut command, "read Tascarrel service logs")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        bail!("Tascarrel daemon services support only Linux and macOS")
    }
}

#[cfg(target_os = "linux")]
fn linux_unit(
    binary: &Path,
    dependencies: &ResolvedDependencies,
    tascarrel_home: &Path,
) -> Result<String> {
    Ok(format!(
        "[Unit]\nDescription=Tascarrel host daemon\n\n\
         [Service]\nType=simple\nExecStart={}\nRestart=on-failure\n\
         Environment=TASCARREL_HOME={}\nEnvironment=TASCARREL_QEMU={}\nEnvironment=TASCARREL_GIT={}\nEnvironment=TASCARREL_SOPS={}\n\n\
         [Install]\nWantedBy=default.target\n",
        systemd_quote(binary)?,
        systemd_quote(tascarrel_home)?,
        systemd_quote(&dependencies.qemu)?,
        systemd_quote(&dependencies.git)?,
        systemd_quote(&dependencies.sops)?,
    ))
}

#[cfg(target_os = "macos")]
fn macos_plist(
    binary: &Path,
    dependencies: &ResolvedDependencies,
    tascarrel_home: &Path,
) -> Result<String> {
    let log = tascarrel_home.join("state/daemon.log");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{MACOS_LABEL}</string>\n\
         <key>ProgramArguments</key><array><string>{}</string></array>\n\
         <key>EnvironmentVariables</key><dict>\n\
         <key>TASCARREL_HOME</key><string>{}</string>\n\
         <key>TASCARREL_QEMU</key><string>{}</string>\n\
         <key>TASCARREL_GIT</key><string>{}</string>\n\
         <key>TASCARREL_SOPS</key><string>{}</string>\n\
         </dict><key>RunAtLoad</key><false/><key>KeepAlive</key><false/>\n\
         <key>StandardOutPath</key><string>{}</string>\n\
         <key>StandardErrorPath</key><string>{}</string>\n\
         </dict></plist>\n",
        xml(binary)?,
        xml(tascarrel_home)?,
        xml(&dependencies.qemu)?,
        xml(&dependencies.git)?,
        xml(&dependencies.sops)?,
        xml(&log)?,
        xml(&log)?,
    ))
}

#[cfg(target_os = "linux")]
fn systemd_quote(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| anyhow!("systemd service path is not UTF-8: {}", path.display()))?;
    if value.contains(['\n', '\r', '\0']) {
        bail!("systemd service path contains control characters");
    }
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

#[cfg(target_os = "macos")]
fn xml(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| anyhow!("LaunchAgent path is not UTF-8: {}", path.display()))?;
    if value.contains(['\0']) {
        bail!("LaunchAgent path contains NUL");
    }
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

#[cfg(target_os = "linux")]
fn linux_unit_path() -> Result<PathBuf> {
    let config = absolute_environment("XDG_CONFIG_HOME")
        .or_else(|| absolute_environment("HOME").map(|home| home.join(".config")))
        .ok_or_else(|| anyhow!("HOME or XDG_CONFIG_HOME is required to install a user service"))?;
    let directory = config.join("systemd/user");
    create_directory(&directory, 0o700)?;
    Ok(directory.join(LINUX_UNIT))
}

#[cfg(target_os = "macos")]
fn macos_plist_path() -> Result<PathBuf> {
    let directory = absolute_environment("HOME")
        .ok_or_else(|| anyhow!("HOME is required to install a LaunchAgent"))?
        .join("Library/LaunchAgents");
    create_directory(&directory, 0o700)?;
    Ok(directory.join(format!("{MACOS_LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn macos_log_path() -> Result<PathBuf> {
    let directory = tascarrel_host::TascarrelHome::discover()
        .map_err(|error| anyhow!(error.to_string()))?
        .state();
    create_directory(&directory, 0o700)?;
    Ok(directory.join("daemon.log"))
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> Result<String> {
    Ok(format!("gui/{}", nix::unistd::Uid::effective().as_raw()))
}

fn absolute_environment(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn run_checked(command: &mut Command, purpose: &str) -> Result<()> {
    let status = command.status().with_context(|| purpose.to_owned())?;
    if !status.success() {
        bail!("{purpose}: command exited with {status}");
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Verifies installed service paths are escaped and retain the resolved
    /// home.
    #[test]
    fn systemd_unit_quotes_every_installed_path() {
        let dependencies = ResolvedDependencies {
            qemu: PathBuf::from("/opt/QEMU bin/qemu%system"),
            git: PathBuf::from("/usr/bin/git"),
            sops: PathBuf::from("/usr/bin/sops"),
        };
        let unit = linux_unit(
            Path::new("/home/test/bin/tascarrel"),
            &dependencies,
            Path::new("/home/test/tascarrel data"),
        )
        .unwrap();
        assert!(unit.contains("ExecStart=\"/home/test/bin/tascarrel\""));
        assert!(unit.contains("Environment=TASCARREL_HOME=\"/home/test/tascarrel data\""));
        assert!(unit.contains("\"/opt/QEMU bin/qemu%%system\""));
        assert!(unit.contains("Environment=TASCARREL_SOPS=\"/usr/bin/sops\""));
    }
}
