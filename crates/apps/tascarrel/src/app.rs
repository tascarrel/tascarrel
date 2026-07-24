use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::doctor;
use crate::embedded;
use crate::install;

const APP_BROWSER_ENV: &str = "TASCARREL_APP_BROWSER";
const APP_HOSTNAME: &str = "tascarrel.localhost";
const APP_READY_TIMEOUT: Duration = Duration::from_secs(20);
const APP_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const APP_PROBE_INTERVAL: Duration = Duration::from_millis(50);

const BROWSER_CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "brave-browser",
    "brave",
    "microsoft-edge",
    "microsoft-edge-stable",
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppLauncher {
    executable: PathBuf,
}

enum Stop {
    Window(io::Result<ExitStatus>),
    Daemon(std::result::Result<Result<()>, tokio::task::JoinError>),
    Signal(io::Result<()>),
}

impl AppLauncher {
    fn discover() -> Result<Self> {
        let override_browser = env::var_os(APP_BROWSER_ENV).filter(|value| !value.is_empty());
        let search_path = env::var_os("PATH");
        let home = env::var_os("HOME").map(PathBuf::from);
        discover_launcher(
            override_browser.as_deref(),
            search_path.as_deref(),
            home.as_deref(),
        )
    }

    fn arguments(url: &str, profile: &Path) -> Vec<OsString> {
        vec![
            OsString::from(format!("--app={url}")),
            OsString::from(format!("--user-data-dir={}", profile.display())),
            OsString::from("--window-size=1440,900"),
            OsString::from("--disable-background-mode"),
            OsString::from("--no-first-run"),
            OsString::from("--no-default-browser-check"),
        ]
    }

    fn launch(&self, url: &str, profile: &Path) -> Result<Child> {
        let mut command = Command::new(&self.executable);
        command
            .args(Self::arguments(url, profile))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command.spawn().with_context(|| {
            format!(
                "launch Tascarrel app window with {}",
                self.executable.display()
            )
        })
    }
}

/// Prepares the embedded distribution, runs hostd in this process, and owns a
/// dedicated browser app window until that window or the process is closed.
pub async fn run(socket: Option<PathBuf>) -> Result<()> {
    let dependencies = doctor::inspect_runtime().require()?;
    let launcher = AppLauncher::discover()?;
    let payload = embedded::payload().ok_or_else(|| {
        anyhow!(
            "this development build carries no guest payload; use an architecture-specific Tascarrel distribution"
        )
    })?;
    let payload = install::prepare(payload).context("prepare embedded Tascarrel payload")?;
    let profile = app_profile_directory()?;
    let address = available_web_address()?;
    let url = format!("http://{APP_HOSTNAME}:{}/", address.port());
    let options = tascarrel_host::daemon::DaemonOptions::for_payload(
        payload.guest(),
        dependencies.qemu,
        dependencies.git,
        dependencies.sops,
        socket,
    )
    .with_web_address(address);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut daemon = tokio::spawn(async move {
        tascarrel_host::daemon::run_until_shutdown(options, async {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let ready = tokio::select! {
        ready = wait_for_http(address) => ready,
        daemon_result = &mut daemon => {
            return daemon_stopped(daemon_result, "before the app UI became ready");
        }
    };
    if let Err(error) = ready {
        let _ = shutdown_tx.send(());
        join_daemon(&mut daemon).await?;
        return Err(error);
    }

    let mut window = match launcher.launch(&url, &profile) {
        Ok(window) => window,
        Err(error) => {
            let _ = shutdown_tx.send(());
            join_daemon(&mut daemon).await?;
            return Err(error);
        }
    };

    let stop = tokio::select! {
        status = window.wait() => Stop::Window(status),
        result = &mut daemon => Stop::Daemon(result),
        result = shutdown_signal() => Stop::Signal(result),
    };
    match stop {
        Stop::Window(status) => {
            let _ = shutdown_tx.send(());
            join_daemon(&mut daemon).await?;
            let status = status.context("wait for Tascarrel app window")?;
            if !status.success() {
                bail!("Tascarrel app window exited with {status}");
            }
            Ok(())
        }
        Stop::Daemon(result) => {
            let _ = window.kill().await;
            daemon_stopped(result, "while the app window was open")
        }
        Stop::Signal(signal) => {
            let _ = window.kill().await;
            let _ = shutdown_tx.send(());
            let daemon_result = join_daemon(&mut daemon).await;
            signal.context("listen for Tascarrel app shutdown signals")?;
            daemon_result
        }
    }
}

fn discover_launcher(
    override_browser: Option<&OsStr>,
    search_path: Option<&OsStr>,
    home: Option<&Path>,
) -> Result<AppLauncher> {
    if let Some(browser) = override_browser {
        let path = resolve_program(browser, search_path).ok_or_else(|| {
            anyhow!(
                "{APP_BROWSER_ENV} does not name an executable Chromium-family browser: {}",
                Path::new(browser).display()
            )
        })?;
        return Ok(AppLauncher { executable: path });
    }

    if let Some(path) = search_path
        && let Some(executable) = BROWSER_CANDIDATES
            .iter()
            .find_map(|name| find_in_path(name, path))
    {
        return Ok(AppLauncher { executable });
    }

    #[cfg(target_os = "macos")]
    for executable in macos_browser_candidates(home) {
        if let Some(executable) = executable_file(executable) {
            return Ok(AppLauncher { executable });
        }
    }

    let _ = home;
    bail!(
        "no Chromium-family app browser was found; install Chromium, Chrome, Brave, or Edge, or set {APP_BROWSER_ENV}"
    )
}

fn resolve_program(program: &OsStr, search_path: Option<&OsStr>) -> Option<PathBuf> {
    let path = PathBuf::from(program);
    if path.components().count() == 1 {
        return search_path.and_then(|search_path| find_in_path(program, search_path));
    }
    executable_file(path)
}

fn find_in_path(name: impl AsRef<OsStr>, search_path: &OsStr) -> Option<PathBuf> {
    env::split_paths(search_path)
        .map(|directory| directory.join(name.as_ref()))
        .find_map(executable_file)
}

fn executable_file(path: PathBuf) -> Option<PathBuf> {
    let metadata = fs::metadata(&path).ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .then(|| fs::canonicalize(&path).unwrap_or(path))
}

#[cfg(target_os = "macos")]
fn macos_browser_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    const APPS: &[(&str, &str)] = &[
        ("Google Chrome.app", "Google Chrome"),
        ("Chromium.app", "Chromium"),
        ("Brave Browser.app", "Brave Browser"),
        ("Microsoft Edge.app", "Microsoft Edge"),
    ];
    let mut roots = Vec::new();
    if let Some(home) = home {
        roots.push(home.join("Applications"));
    }
    roots.push(PathBuf::from("/Applications"));
    roots
        .into_iter()
        .flat_map(|root| {
            APPS.iter()
                .map(move |(bundle, binary)| root.join(bundle).join("Contents/MacOS").join(binary))
        })
        .collect()
}

fn app_profile_directory() -> Result<PathBuf> {
    let path = install::InstallPaths::discover()?.state.join("app-browser");
    install::create_directory(&path, 0o700)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure Tascarrel app browser profile {}", path.display()))?;
    Ok(path)
}

fn available_web_address() -> Result<SocketAddr> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("reserve a loopback address for the Tascarrel app UI")?;
    let address = listener.local_addr()?;
    drop(listener);
    Ok(address)
}

async fn wait_for_http(address: SocketAddr) -> Result<()> {
    let deadline = tokio::time::Instant::now() + APP_READY_TIMEOUT;
    loop {
        if probe_http(address).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("Tascarrel app UI did not become ready at http://{address}/ within 20 seconds");
        }
        tokio::time::sleep(APP_PROBE_INTERVAL).await;
    }
}

async fn probe_http(address: SocketAddr) -> bool {
    tokio::time::timeout(APP_PROBE_TIMEOUT, async {
        let mut stream = TcpStream::connect(address).await?;
        stream
            .write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = [0_u8; 256];
        let read = stream.read(&mut response).await?;
        Ok::<_, io::Error>(response[..read].starts_with(b"HTTP/1.1 200"))
    })
    .await
    .is_ok_and(|result| result.unwrap_or(false))
}

async fn join_daemon(daemon: &mut JoinHandle<Result<()>>) -> Result<()> {
    (&mut *daemon)
        .await
        .context("join in-process Tascarrel host daemon")?
}

fn daemon_stopped(result: Result<Result<()>, tokio::task::JoinError>, context: &str) -> Result<()> {
    match result {
        Ok(Ok(())) => bail!("in-process Tascarrel host daemon stopped {context}"),
        Ok(Err(error)) => {
            Err(error).with_context(|| format!("in-process Tascarrel host daemon failed {context}"))
        }
        Err(error) => Err(error)
            .with_context(|| format!("in-process Tascarrel host daemon panicked {context}")),
    }
}

async fn shutdown_signal() -> io::Result<()> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_uses_a_dedicated_app_window_and_profile() {
        let arguments =
            AppLauncher::arguments("http://tascarrel.localhost:8272/", Path::new("/profile"));
        assert!(arguments.contains(&OsString::from("--app=http://tascarrel.localhost:8272/")));
        assert!(arguments.contains(&OsString::from("--user-data-dir=/profile")));
        assert!(arguments.contains(&OsString::from("--disable-background-mode")));
    }

    #[test]
    fn launcher_discovery_accepts_an_override_or_known_path_entry() {
        let directory = tempfile::tempdir().unwrap();
        let override_browser = directory.path().join("custom-browser");
        fs::write(&override_browser, b"browser").unwrap();
        fs::set_permissions(&override_browser, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            discover_launcher(Some(override_browser.as_os_str()), None, None)
                .unwrap()
                .executable,
            override_browser
        );

        let chromium = directory.path().join("chromium");
        fs::write(&chromium, b"browser").unwrap();
        fs::set_permissions(&chromium, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            discover_launcher(None, Some(directory.path().as_os_str()), None)
                .unwrap()
                .executable,
            chromium
        );
    }

    #[test]
    fn launcher_discovery_rejects_non_executable_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let browser = directory.path().join("browser");
        fs::write(&browser, b"browser").unwrap();
        assert!(discover_launcher(Some(browser.as_os_str()), None, None).is_err());
    }

    #[tokio::test]
    async fn app_health_probe_requires_an_http_success() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 256];
            let read = stream.read(&mut request).await.unwrap();
            assert!(request[..read].starts_with(b"GET /api/health HTTP/1.1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .await
                .unwrap();
        });
        assert!(probe_http(address).await);
        server.await.unwrap();
    }

    #[test]
    fn app_web_address_is_ephemeral_loopback() {
        let address = available_web_address().unwrap();
        assert_eq!(address.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(address.port(), 0);
    }
}
