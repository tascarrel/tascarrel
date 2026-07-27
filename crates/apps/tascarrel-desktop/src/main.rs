//! Native desktop window for the loopback Tascarrel web application.

use std::env;
use std::ffi::OsString;
use std::io;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use tauri::AppHandle;
use tauri::Manager as _;
use tauri::RunEvent;
use tauri::WebviewUrl;
use tauri::WebviewWindowBuilder;
use thiserror::Error;

const SERVER_ADDRESS: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 8_272);
const SERVER_PROBE_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Error)]
enum DesktopError {
    #[error("failed to initialize Tascarrel Desktop logging")]
    Logging {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("failed to build Tascarrel Desktop")]
    BuildApplication,
    #[error("failed to locate Tascarrel state")]
    LocateState,
    #[error("failed to locate the bundled Tascarrel server")]
    LocateServer,
    #[error("failed to start the bundled Tascarrel server")]
    StartServer,
    #[error("failed to inspect the bundled Tascarrel server process")]
    InspectServer,
    #[error("bundled Tascarrel server failed before listening: {status}")]
    ServerExited { status: ExitStatus },
    #[error("failed to construct the graphical application PATH")]
    ConstructApplicationPath,
    #[error("failed to open the Tascarrel window")]
    OpenWindow,
    #[error("failed to show the existing Tascarrel window")]
    ShowWindow,
    #[error("failed to restore the existing Tascarrel window")]
    RestoreWindow,
    #[error("failed to focus the existing Tascarrel window")]
    FocusWindow,
}

type DesktopResult<T> = Result<T, Report<DesktopError>>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error.eprint(reportify::render::RenderOptions::default());
            ExitCode::FAILURE
        }
    }
}

fn run() -> DesktopResult<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(io::stderr)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .map_err(|source| DesktopError::Logging { source }.report())?;

    let mut builder = tauri::Builder::default();
    builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| {
        focus_main_window(app);
    }));
    let app = builder
        .setup(|app| {
            if start_application(app.handle()).log_error().is_none() {
                app.handle().exit(1);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .escalate(DesktopError::BuildApplication)?;
    app.run(handle_run_event);
    Ok(())
}

#[tracing::instrument(level = "debug", skip(app), err(Debug))]
fn start_application(app: &AppHandle) -> DesktopResult<()> {
    ensure_server()?;
    open_main_window(app)
}

#[tracing::instrument(level = "debug", skip(app), err(Debug))]
fn open_main_window(app: &AppHandle) -> DesktopResult<()> {
    let url = format!(
        "http://tascarrel.localhost:8272/startup?desktopVersion={}&desktopProtocolVersion={}",
        env!("CARGO_PKG_VERSION"),
        tascarrel_protocol::PROTOCOL_VERSION,
    )
    .parse()
    .map_err(tauri::Error::InvalidUrl)
    .escalate(DesktopError::OpenWindow)?;
    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title("Tascarrel")
        .inner_size(1440.0, 900.0)
        .min_inner_size(900.0, 600.0)
        .build()
        .escalate(DesktopError::OpenWindow)?;
    Ok(())
}

#[tracing::instrument(level = "info", err(Debug))]
fn ensure_server() -> DesktopResult<()> {
    if server_is_listening() {
        return Ok(());
    }
    let tascarrel_home = tascarrel_home()?;
    let mut command = Command::new(server_executable()?);
    command
        .env("TASCARREL_HOME", tascarrel_home)
        .env("PATH", graphical_application_path()?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut server = command.spawn().escalate(DesktopError::StartServer)?;
    loop {
        if server_is_listening() {
            return Ok(());
        }
        if let Some(status) = server.try_wait().escalate(DesktopError::InspectServer)? {
            return Err(DesktopError::ServerExited { status }.report());
        }
        std::thread::sleep(SERVER_PROBE_INTERVAL);
    }
}

fn server_is_listening() -> bool {
    TcpStream::connect_timeout(&SERVER_ADDRESS, Duration::from_millis(250)).is_ok()
}

fn server_executable() -> DesktopResult<PathBuf> {
    let desktop = env::current_exe().escalate(DesktopError::LocateServer)?;
    let directory = desktop.parent().ok_or_else(|| {
        DesktopError::LocateServer
            .report()
            .message("the desktop executable has no parent directory")
    })?;
    let server = directory.join("tascarrel");
    let metadata = server.metadata().map_err(|error| {
        error
            .escalate(DesktopError::LocateServer)
            .message(format!("expected server at {}", server.display()))
    })?;
    if !metadata.is_file() {
        return Err(DesktopError::LocateServer
            .report()
            .message(format!("{} is not a regular file", server.display())));
    }
    Ok(server)
}

fn tascarrel_home() -> DesktopResult<PathBuf> {
    if let Some(configured) = env::var_os("TASCARREL_HOME").filter(|value| !value.is_empty()) {
        let configured = PathBuf::from(configured);
        if configured.is_absolute() {
            return Ok(configured);
        }
        return Err(DesktopError::LocateState
            .report()
            .message("TASCARREL_HOME must be absolute when launching Tascarrel Desktop"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .map(|home| home.join(".tascarrel"))
        .ok_or_else(|| {
            DesktopError::LocateState
                .report()
                .message("HOME must name an absolute directory")
        })
}

fn graphical_application_path() -> DesktopResult<OsString> {
    let mut paths = match env::var_os("PATH") {
        Some(path) => env::split_paths(&path).collect::<Vec<_>>(),
        None => Vec::new(),
    };
    for path in [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ] {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    env::join_paths(paths).escalate(DesktopError::ConstructApplicationPath)
}

#[tracing::instrument(level = "debug", skip(app))]
fn focus_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        open_main_window(app).log_warning();
        return;
    };
    window
        .show()
        .escalate(DesktopError::ShowWindow)
        .log_warning();
    window
        .unminimize()
        .escalate(DesktopError::RestoreWindow)
        .log_warning();
    window
        .set_focus()
        .escalate(DesktopError::FocusWindow)
        .log_warning();
}

#[cfg(target_os = "macos")]
fn handle_run_event(app: &AppHandle, event: RunEvent) {
    if matches!(event, RunEvent::Reopen { .. }) {
        focus_main_window(app);
    }
}

#[cfg(not(target_os = "macos"))]
fn handle_run_event(_app: &AppHandle, _event: RunEvent) {}
