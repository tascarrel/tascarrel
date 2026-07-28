//! Administrative client for installation, service, and workspace operations.

use std::io;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use clap::Subcommand;
use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_api::types::auth;
use tascarrel_api::types::store::StoreEvent;
use tascarrel_api::types::workspaces;
use tascarrel_cli::control;
use tascarrel_cli::doctor;
use tascarrel_cli::install;
use tascarrel_cli::service;
use tascarrel_protocol::WorkspaceName;
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    name = "tascarrelctl",
    version,
    about = "Maintain the Tascarrel server"
)]
struct Cli {
    /// Host daemon control socket.
    #[arg(long, env = "TASCARREL_SOCKET", global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install a Tascarrel server executable and its per-user service.
    Install {
        /// Server executable to install.
        #[arg(long)]
        server: Option<PathBuf>,
    },
    /// Check host dependencies and virtualization support.
    Doctor {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Maintain workspace lifecycle.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Manage the per-user Tascarrel host daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Manage browser pairing and remote sessions.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Create a short-lived, single-use browser pairing key.
    Pair {
        /// Suggested browser or device label.
        #[arg(long)]
        label: Option<String>,
    },
    /// List active browser sessions.
    Sessions {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Revoke one browser session and its HTTP route grants.
    Revoke {
        /// Browser session identifier shown by `auth sessions`.
        session_id: auth::BrowserSessionId,
    },
}

#[derive(Clone, Copy, Debug, Subcommand)]
enum DaemonCommand {
    /// Start the Tascarrel user service.
    Start,
    /// Stop the Tascarrel user service.
    Stop,
    /// Restart the Tascarrel user service.
    Restart,
    /// Show service-manager status.
    Status,
    /// Show recent daemon logs.
    Logs {
        /// Continue following new log messages.
        #[arg(short, long)]
        follow: bool,
    },
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// List configured workspaces.
    List,
    /// Create a workspace with the default Debian development image.
    Create {
        /// Name of the new workspace.
        name: WorkspaceName,
    },
    /// Start a workspace VM and wait until it is ready.
    Start {
        /// Workspace to start.
        name: WorkspaceName,
    },
    /// Stop a workspace VM while preserving its pods and configuration.
    Stop {
        /// Workspace to stop.
        name: WorkspaceName,
    },
    /// Show the current VM lifecycle or startup failure.
    Info {
        name: WorkspaceName,
        #[arg(long)]
        json: bool,
    },
    /// Stop and permanently delete a workspace and all of its pods.
    Delete {
        /// Workspace to delete.
        name: WorkspaceName,
        /// Delete without an interactive confirmation.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Error)]
enum ClientError {
    #[error("failed to print Tascarrel host diagnostics")]
    PrintDiagnostics,
    #[error("failed to resolve required Tascarrel host dependencies")]
    ResolveHostDependencies,
    #[error("failed to locate the Tascarrel server executable")]
    LocateServer,
    #[error("failed to install the Tascarrel server")]
    InstallServer,
    #[error("failed to resolve the Tascarrel control socket")]
    ResolveControlSocket,
    #[error("failed to connect to the Tascarrel control socket")]
    ConnectControlSocket,
    #[error("failed to read Tascarrel workspace state")]
    ReadWorkspaceState,
    #[error("failed to read Tascarrel workspace state: the subscription had no initial snapshot")]
    MissingWorkspaceSnapshot,
    #[error("workspace does not exist: {name}")]
    WorkspaceNotFound { name: String },
    #[error("failed to serialize Tascarrel workspace state")]
    SerializeWorkspaceState,
    #[error("failed to create workspace {name}")]
    CreateWorkspace { name: String },
    #[error("failed to start workspace {name}")]
    StartWorkspace { name: String },
    #[error("failed to stop workspace {name}")]
    StopWorkspace { name: String },
    #[error("failed to delete workspace {name}")]
    DeleteWorkspace { name: String },
    #[error("failed to confirm deletion of workspace {name}")]
    ConfirmWorkspaceDeletion { name: String },
    #[error("failed to start the Tascarrel user service")]
    StartService,
    #[error("failed to stop the Tascarrel user service")]
    StopService,
    #[error("failed to restart the Tascarrel user service")]
    RestartService,
    #[error("failed to inspect the Tascarrel user service")]
    InspectService,
    #[error("failed to read Tascarrel user service logs")]
    ReadServiceLogs,
    #[error("failed to create a Tascarrel browser pairing key")]
    CreatePairingKey,
    #[error("failed to read Tascarrel browser sessions")]
    ReadBrowserSessions,
    #[error("failed to serialize Tascarrel browser sessions")]
    SerializeBrowserSessions,
    #[error("failed to revoke Tascarrel browser session {session_id}")]
    RevokeBrowserSession { session_id: String },
}

type ClientResult<T> = Result<T, Report<ClientError>>;

#[tokio::main]
async fn main() -> ExitCode {
    let result = run(Cli::parse()).await;
    match result {
        Ok(code) => u8::try_from(code.clamp(0, 255)).map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => {
            error.eprint(reportify::render::RenderOptions::default());
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)] // Top-level CLI dispatch keeps command behavior easy to scan.
async fn run(cli: Cli) -> ClientResult<i32> {
    match &cli.command {
        Command::Install { server } => {
            let report = doctor::inspect();
            report_anyhow(doctor::print(&report, false), ClientError::PrintDiagnostics)?;
            let dependencies =
                report_anyhow(report.require(), ClientError::ResolveHostDependencies)?;
            let source = match server {
                Some(server) => server.clone(),
                None => report_anyhow(
                    install::sibling_server_executable(),
                    ClientError::LocateServer,
                )?,
            };
            let binary = report_anyhow(
                install::install_server(&source, &dependencies),
                ClientError::InstallServer,
            )?;
            println!("Installed Tascarrel at {}", binary.display());
            println!("Start it with `tascarrelctl daemon start`.");
            Ok(0)
        }
        Command::Doctor { json } => {
            let report = doctor::inspect();
            report_anyhow(doctor::print(&report, *json), ClientError::PrintDiagnostics)?;
            Ok(i32::from(!report.is_healthy()))
        }
        Command::Workspace { command } => {
            let socket = resolved_control_socket(cli.socket.as_deref())?;
            let client = report_anyhow(
                control::ControlClient::connect(&socket).await,
                ClientError::ConnectControlSocket,
            )?;
            match command {
                WorkspaceCommand::List => {
                    for workspace in workspace_snapshot(&client).await?.workspaces {
                        println!("{}", workspace.name);
                    }
                }
                WorkspaceCommand::Create { name } => {
                    report_anyhow(
                        client
                            .invoke(workspaces::CreateWorkspaceAction {
                                name: api_workspace_name(name),
                            })
                            .await,
                        ClientError::CreateWorkspace {
                            name: name.to_string(),
                        },
                    )?;
                    println!("Created workspace {name}");
                }
                WorkspaceCommand::Start { name } => {
                    report_anyhow(
                        client
                            .invoke(workspaces::StartWorkspaceAction {
                                workspace: api_workspace_name(name),
                            })
                            .await,
                        ClientError::StartWorkspace {
                            name: name.to_string(),
                        },
                    )?;
                    println!("Started workspace {name}");
                }
                WorkspaceCommand::Stop { name } => {
                    report_anyhow(
                        client
                            .invoke(workspaces::StopWorkspaceAction {
                                workspace: api_workspace_name(name),
                            })
                            .await,
                        ClientError::StopWorkspace {
                            name: name.to_string(),
                        },
                    )?;
                    println!("Stopped workspace {name}");
                }
                WorkspaceCommand::Info { name, json } => {
                    let workspace = workspace_snapshot(&client)
                        .await?
                        .workspaces
                        .into_iter()
                        .find(|workspace| workspace.name.as_str() == name.as_str())
                        .ok_or_else(|| {
                            ClientError::WorkspaceNotFound {
                                name: name.to_string(),
                            }
                            .report()
                        })?;
                    if *json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&workspace)
                                .escalate(ClientError::SerializeWorkspaceState)?
                        );
                    } else {
                        println!("Name:  {}", workspace.name);
                        println!("State: {}", workspace_state_label(&workspace.state));
                        if let workspaces::WorkspaceState::Failed(failure) = workspace.state {
                            println!("Error: {}", failure.message);
                        }
                    }
                }
                WorkspaceCommand::Delete { name, force } => {
                    if !*force && !confirm_workspace_delete(name)? {
                        println!("Workspace {name} was not deleted");
                        return Ok(0);
                    }
                    report_anyhow(
                        client
                            .invoke(workspaces::DestroyWorkspaceAction {
                                workspace: api_workspace_name(name),
                            })
                            .await,
                        ClientError::DeleteWorkspace {
                            name: name.to_string(),
                        },
                    )?;
                    println!("Deleted workspace {name}");
                }
            }
            Ok(0)
        }
        Command::Auth { command } => {
            let socket = resolved_control_socket(cli.socket.as_deref())?;
            let client = report_anyhow(
                control::ControlClient::connect(&socket).await,
                ClientError::ConnectControlSocket,
            )?;
            match command {
                AuthCommand::Pair { label } => {
                    let pairing = report_anyhow(
                        client
                            .invoke(auth::CreatePairingKeyAction {
                                label: label.clone().map(Into::into),
                            })
                            .await,
                        ClientError::CreatePairingKey,
                    )?;
                    println!("{}", pairing.pairing_key);
                    eprintln!("Expires at {}", pairing.expires_at);
                }
                AuthCommand::Sessions { json } => {
                    let event = report_anyhow(
                        client
                            .first_event(auth::BrowserSessionsChangedSubscription {})
                            .await,
                        ClientError::ReadBrowserSessions,
                    )?;
                    if *json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&event.sessions)
                                .escalate(ClientError::SerializeBrowserSessions)?
                        );
                    } else if event.sessions.is_empty() {
                        println!("No active browser sessions");
                    } else {
                        for session in event.sessions {
                            println!(
                                "{}\t{}\t{}\tlast seen {}",
                                session.id.0, session.label, session.origin, session.last_seen_at
                            );
                        }
                    }
                }
                AuthCommand::Revoke { session_id } => {
                    report_anyhow(
                        client
                            .invoke(auth::RevokeBrowserSessionAction {
                                session_id: session_id.clone(),
                            })
                            .await,
                        ClientError::RevokeBrowserSession {
                            session_id: session_id.0.to_string(),
                        },
                    )?;
                    println!("Revoked browser session {}", session_id.0);
                }
            }
            Ok(0)
        }
        Command::Daemon { command } => run_daemon_command(*command),
    }
}

fn run_daemon_command(command: DaemonCommand) -> ClientResult<i32> {
    match command {
        DaemonCommand::Start => {
            report_anyhow(service::start(), ClientError::StartService)?;
            Ok(0)
        }
        DaemonCommand::Stop => {
            report_anyhow(service::stop(), ClientError::StopService)?;
            Ok(0)
        }
        DaemonCommand::Restart => {
            report_anyhow(service::restart(), ClientError::RestartService)?;
            Ok(0)
        }
        DaemonCommand::Status => Ok(i32::from(!report_anyhow(
            service::status(),
            ClientError::InspectService,
        )?)),
        DaemonCommand::Logs { follow } => {
            report_anyhow(service::logs(follow), ClientError::ReadServiceLogs)?;
            Ok(0)
        }
    }
}

async fn workspace_snapshot(
    client: &control::ControlClient,
) -> ClientResult<workspaces::WorkspaceList> {
    let event = report_anyhow(
        client
            .first_event(workspaces::WorkspaceListChangedSubscription { cursor: None })
            .await,
        ClientError::ReadWorkspaceState,
    )?;
    let StoreEvent::Snapshot(snapshot) = event.change else {
        return Err(ClientError::MissingWorkspaceSnapshot.report());
    };
    Ok(snapshot.value)
}

const fn workspace_state_label(state: &workspaces::WorkspaceState) -> &'static str {
    match state {
        workspaces::WorkspaceState::Stopped => "stopped",
        workspaces::WorkspaceState::Starting(_) => "starting",
        workspaces::WorkspaceState::Running(_) => "running",
        workspaces::WorkspaceState::Stopping(_) => "stopping",
        workspaces::WorkspaceState::Destroying => "destroying",
        workspaces::WorkspaceState::Failed(_) => "failed",
    }
}

fn api_workspace_name(workspace: &WorkspaceName) -> workspaces::WorkspaceName {
    workspaces::WorkspaceName::new(workspace.as_str())
}

fn confirm_workspace_delete(workspace: &WorkspaceName) -> ClientResult<bool> {
    let confirmation_error = || ClientError::ConfirmWorkspaceDeletion {
        name: workspace.to_string(),
    };
    if !io::stdin().is_terminal() {
        return Err(confirmation_error()
            .report()
            .message("pass --force when standard input is not interactive"));
    }
    eprint!("Delete workspace \"{workspace}\", including all pods and persistent state? [y/N] ");
    io::stderr().flush().escalate(confirmation_error())?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .escalate(confirmation_error())?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn resolved_control_socket(configured: Option<&Path>) -> ClientResult<PathBuf> {
    configured.map_or_else(
        || {
            tascarrel_host::TascarrelHome::discover()
                .map(|home| home.control_socket())
                .map_err(|error| {
                    ClientError::ResolveControlSocket
                        .report()
                        .message(error.to_string())
                })
        },
        |path| Ok(path.to_owned()),
    )
}

/// Retains diagnostics from modules that still expose `anyhow` results.
fn report_anyhow<T>(result: anyhow::Result<T>, error: ClientError) -> ClientResult<T> {
    result.map_err(|source| error.report().message(format!("{source:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the complete workspace maintenance surface and name validation.
    #[test]
    fn workspace_commands_do_not_require_a_selected_workspace() {
        let list = Cli::try_parse_from(["tascarrelctl", "workspace", "list"]).unwrap();
        assert!(matches!(
            list.command,
            Command::Workspace {
                command: WorkspaceCommand::List
            }
        ));

        let create =
            Cli::try_parse_from(["tascarrelctl", "workspace", "create", "team-alpha"]).unwrap();
        assert!(matches!(
            create.command,
            Command::Workspace {
                command: WorkspaceCommand::Create { .. }
            }
        ));
        assert!(Cli::try_parse_from(["tascarrelctl", "workspace", "create", "../escape"]).is_err());

        let start =
            Cli::try_parse_from(["tascarrelctl", "workspace", "start", "team-alpha"]).unwrap();
        assert!(matches!(
            start.command,
            Command::Workspace {
                command: WorkspaceCommand::Start { .. }
            }
        ));
        let stop =
            Cli::try_parse_from(["tascarrelctl", "workspace", "stop", "team-alpha"]).unwrap();
        assert!(matches!(
            stop.command,
            Command::Workspace {
                command: WorkspaceCommand::Stop { .. }
            }
        ));
        assert!(Cli::try_parse_from(["tascarrelctl", "workspace", "up", "team-alpha"]).is_err());
        assert!(Cli::try_parse_from(["tascarrelctl", "workspace", "down", "team-alpha"]).is_err());
        let info =
            Cli::try_parse_from(["tascarrelctl", "workspace", "info", "team-alpha", "--json"])
                .unwrap();
        assert!(matches!(
            info.command,
            Command::Workspace {
                command: WorkspaceCommand::Info { json: true, .. }
            }
        ));
        assert!(Cli::try_parse_from(["tascarrelctl", "workspace", "logs", "team-alpha"]).is_err());
        let delete = Cli::try_parse_from([
            "tascarrelctl",
            "workspace",
            "delete",
            "team-alpha",
            "--force",
        ])
        .unwrap();
        assert!(matches!(
            delete.command,
            Command::Workspace {
                command: WorkspaceCommand::Delete { force: true, .. }
            }
        ));
    }
}
