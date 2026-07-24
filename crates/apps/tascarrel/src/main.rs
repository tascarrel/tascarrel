use std::ffi::OsStr;
use std::io::IsTerminal;
use std::io::Write;
use std::io::{self};
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;
use tascarrel_api::types::store::StoreEvent;
use tascarrel_api::types::workspaces;
use tascarrel_protocol::WorkspaceName;

mod app;
mod control;
mod doctor;
mod embedded;
mod install;
mod service;

#[derive(Debug, Parser)]
#[command(name = "tascarrel", version, about = "Run and maintain Tascarrel")]
struct Cli {
    /// Host daemon control socket.
    #[arg(long, env = "TASCARREL_SOCKET", global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
#[command(
    name = "tascarrel host",
    version,
    about = "Run the Tascarrel host daemon",
    no_binary_name = true
)]
struct HostCli {
    #[command(flatten)]
    options: tascarrel_host::daemon::DaemonOptions,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open Tascarrel in a dedicated desktop app window.
    App,
    /// Install this executable, its guest image, and the per-user daemon
    /// service.
    Install,
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

#[tokio::main]
async fn main() -> ExitCode {
    match tascarrel_host::daemon::run_git_remote_helper_if_invoked() {
        Ok(true) => return ExitCode::SUCCESS,
        Ok(false) => {}
        Err(error) => {
            eprintln!("tascarrel: {error:#}");
            return ExitCode::FAILURE;
        }
    }
    let result = match host_options_if_invoked() {
        Some(options) => run_host(options).await.map(|()| 0),
        None => run(Cli::parse()).await,
    };
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1)),
        Err(error) => {
            eprintln!("tascarrel: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn host_options_if_invoked() -> Option<tascarrel_host::daemon::DaemonOptions> {
    let mut arguments = std::env::args_os();
    arguments.next()?;
    if arguments.next().as_deref() != Some(OsStr::new("host")) {
        return None;
    }
    Some(HostCli::parse_from(arguments).options)
}

async fn run_host(options: tascarrel_host::daemon::DaemonOptions) -> Result<()> {
    let options = if let Some(payload) = embedded::payload() {
        let payload = install::prepare(payload).context("prepare embedded Tascarrel payload")?;
        options.with_payload_defaults(payload.guest())
    } else {
        options
    };
    tascarrel_host::daemon::run(options).await
}

#[allow(clippy::too_many_lines)] // Top-level CLI dispatch keeps command behavior easy to scan.
async fn run(cli: Cli) -> Result<i32> {
    match &cli.command {
        Command::App => {
            app::run(cli.socket.clone()).await?;
            Ok(0)
        }
        Command::Install => {
            let report = doctor::inspect();
            doctor::print(&report, false)?;
            let dependencies = report.require()?;
            let payload = embedded::payload().ok_or_else(|| {
                anyhow!(
                    "this development build carries no guest payload; use an architecture-specific Tascarrel distribution"
                )
            })?;
            let binary = install::install(payload, &dependencies)?;
            println!("Installed Tascarrel at {}", binary.display());
            println!("Start it with `tascarrel daemon start`.");
            Ok(0)
        }
        Command::Doctor { json } => {
            let report = doctor::inspect();
            doctor::print(&report, *json)?;
            Ok(i32::from(!report.is_healthy()))
        }
        Command::Workspace { command } => {
            let socket = resolved_control_socket(cli.socket.as_deref())?;
            let client = control::ControlClient::connect(&socket).await?;
            match command {
                WorkspaceCommand::List => {
                    for workspace in workspace_snapshot(&client).await?.workspaces {
                        println!("{}", workspace.name);
                    }
                }
                WorkspaceCommand::Create { name } => {
                    client
                        .invoke(workspaces::CreateWorkspaceAction {
                            name: api_workspace_name(name),
                        })
                        .await?;
                    println!("Created workspace {name}");
                }
                WorkspaceCommand::Start { name } => {
                    client
                        .invoke(workspaces::StartWorkspaceAction {
                            workspace: api_workspace_name(name),
                        })
                        .await?;
                    println!("Started workspace {name}");
                }
                WorkspaceCommand::Stop { name } => {
                    client
                        .invoke(workspaces::StopWorkspaceAction {
                            workspace: api_workspace_name(name),
                        })
                        .await?;
                    println!("Stopped workspace {name}");
                }
                WorkspaceCommand::Info { name, json } => {
                    let workspace = workspace_snapshot(&client)
                        .await?
                        .workspaces
                        .into_iter()
                        .find(|workspace| workspace.name.as_str() == name.as_str())
                        .ok_or_else(|| anyhow!("workspace does not exist: {name}"))?;
                    if *json {
                        println!("{}", serde_json::to_string_pretty(&workspace)?);
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
                    client
                        .invoke(workspaces::DestroyWorkspaceAction {
                            workspace: api_workspace_name(name),
                        })
                        .await?;
                    println!("Deleted workspace {name}");
                }
            }
            Ok(0)
        }
        Command::Daemon { command } => run_daemon_command(*command),
    }
}

fn run_daemon_command(command: DaemonCommand) -> Result<i32> {
    match command {
        DaemonCommand::Start => {
            service::start()?;
            Ok(0)
        }
        DaemonCommand::Stop => {
            service::stop()?;
            Ok(0)
        }
        DaemonCommand::Restart => {
            service::restart()?;
            Ok(0)
        }
        DaemonCommand::Status => Ok(i32::from(!service::status()?)),
        DaemonCommand::Logs { follow } => {
            service::logs(follow)?;
            Ok(0)
        }
    }
}

async fn workspace_snapshot(client: &control::ControlClient) -> Result<workspaces::WorkspaceList> {
    let event = client
        .first_event(workspaces::WorkspaceListChangedSubscription { cursor: None })
        .await?;
    let StoreEvent::Snapshot(snapshot) = event.change else {
        bail!("workspace subscription did not begin with a snapshot");
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

fn confirm_workspace_delete(workspace: &WorkspaceName) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("workspace deletion requires confirmation; pass --force in non-interactive use");
    }
    eprint!("Delete workspace \"{workspace}\", including all pods and persistent state? [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn resolved_control_socket(configured: Option<&Path>) -> Result<PathBuf> {
    configured.map_or_else(
        || {
            tascarrel_host::TascarrelHome::discover()
                .map(|home| home.control_socket())
                .map_err(|error| anyhow!(error.to_string()))
        },
        |path| Ok(path.to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the complete workspace maintenance surface and name validation.
    #[test]
    fn workspace_commands_do_not_require_a_selected_workspace() {
        let list = Cli::try_parse_from(["tascarrel", "workspace", "list"]).unwrap();
        assert!(matches!(
            list.command,
            Command::Workspace {
                command: WorkspaceCommand::List
            }
        ));

        let create =
            Cli::try_parse_from(["tascarrel", "workspace", "create", "team-alpha"]).unwrap();
        assert!(matches!(
            create.command,
            Command::Workspace {
                command: WorkspaceCommand::Create { .. }
            }
        ));
        assert!(Cli::try_parse_from(["tascarrel", "workspace", "create", "../escape"]).is_err());

        let start = Cli::try_parse_from(["tascarrel", "workspace", "start", "team-alpha"]).unwrap();
        assert!(matches!(
            start.command,
            Command::Workspace {
                command: WorkspaceCommand::Start { .. }
            }
        ));
        let stop = Cli::try_parse_from(["tascarrel", "workspace", "stop", "team-alpha"]).unwrap();
        assert!(matches!(
            stop.command,
            Command::Workspace {
                command: WorkspaceCommand::Stop { .. }
            }
        ));
        assert!(Cli::try_parse_from(["tascarrel", "workspace", "up", "team-alpha"]).is_err());
        assert!(Cli::try_parse_from(["tascarrel", "workspace", "down", "team-alpha"]).is_err());
        let info = Cli::try_parse_from(["tascarrel", "workspace", "info", "team-alpha", "--json"])
            .unwrap();
        assert!(matches!(
            info.command,
            Command::Workspace {
                command: WorkspaceCommand::Info { json: true, .. }
            }
        ));
        assert!(Cli::try_parse_from(["tascarrel", "workspace", "logs", "team-alpha"]).is_err());
        let delete =
            Cli::try_parse_from(["tascarrel", "workspace", "delete", "team-alpha", "--force"])
                .unwrap();
        assert!(matches!(
            delete.command,
            Command::Workspace {
                command: WorkspaceCommand::Delete { force: true, .. }
            }
        ));
    }

    /// Verifies that app mode remains an independent top-level command.
    #[test]
    fn app_mode_does_not_require_a_workspace_or_service_subcommand() {
        let cli = Cli::try_parse_from(["tascarrel", "app"]).unwrap();
        assert!(matches!(cli.command, Command::App));
    }
}
