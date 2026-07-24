//! Pod-local client for authenticated Tascarrel control and data planes.
//!
//! The `podctl` command exposes pod-scoped actions and live-state snapshots.
//! The same executable also implements the managed Git transport adapters
//! selected through its `git-remote-tascarrel` and `tascarrel-git-receive-pack`
//! invocation names.

mod client;
mod device;
mod error;
mod git;

use std::io;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use reportify::ErrorExt as _;
use reportify::ResultExt as _;
use serde::Serialize;
use tascarrel_api::types::chats;
use tascarrel_api::types::network;
use tascarrel_api::types::pods;
use tascarrel_api::types::processes;

use crate::client::PodClient;
use crate::device::create_device_link;
use crate::device::remove_device_node;
use crate::error::PodctlError;
use crate::error::PodctlResult;
use crate::git::run_git_receive_pack;
use crate::git::run_git_remote_helper;

const CONTROL_SOCKET: &str = "/run/tascarrel/guestd-control.sock";

#[derive(Debug, Parser)]
#[command(name = "podctl", about = "Control the current Tascarrel pod")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show the authenticated workspace and pod identity.
    Identity,
    /// Set the human-readable name of this pod.
    Title { title: String },
    /// Destroy this pod and all of its persistent resources.
    Destroy,
    /// Inspect and control processes belonging to this pod.
    Processes {
        #[command(subcommand)]
        command: ProcessCommand,
    },
    /// Inspect chats belonging to this pod.
    Chats {
        #[command(subcommand)]
        command: ChatCommand,
    },
    /// Manage dynamic host-loopback forwards for this pod.
    Ports {
        #[command(subcommand)]
        command: PortCommand,
    },
    /// Manage HTTP routes for this pod.
    Http {
        #[command(subcommand)]
        command: HttpCommand,
    },
    /// Internal guestd helper run as VM root inside a pod's namespaces.
    #[command(hide = true)]
    DeviceLink {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        source: PathBuf,
    },
    /// Internal guestd helper for revoking one pod-private node.
    #[command(hide = true)]
    DeviceRemove {
        #[arg(long)]
        path: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ProcessCommand {
    /// List processes belonging to this pod.
    List,
    /// Capture the current screen of a terminal process.
    Snapshot { process_id: processes::ProcessId },
    /// Signal a process belonging to this pod.
    Kill {
        process_id: processes::ProcessId,
        #[arg(long, value_enum, default_value_t = ProcessSignal::Terminate)]
        signal: ProcessSignal,
    },
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum ProcessSignal {
    #[default]
    Terminate,
    Kill,
    Hangup,
    Interrupt,
}

impl From<ProcessSignal> for processes::ProcessSignal {
    fn from(value: ProcessSignal) -> Self {
        match value {
            ProcessSignal::Terminate => Self::Terminate,
            ProcessSignal::Kill => Self::Kill,
            ProcessSignal::Hangup => Self::Hangup,
            ProcessSignal::Interrupt => Self::Interrupt,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ChatCommand {
    /// List chats belonging to this pod.
    List,
    /// Show one chat and its current timeline.
    Show { chat_id: chats::ChatId },
}

#[derive(Debug, Subcommand)]
enum PortCommand {
    /// Publish a pod TCP port on a dynamic host-loopback port.
    Publish {
        port: u16,
        #[arg(long)]
        title: Option<String>,
        /// Also create a visible HTTP route for the port.
        #[arg(long)]
        tab: bool,
    },
    /// List this pod's dynamic port forwards.
    List,
    /// Delete the forward for a pod port.
    Unpublish { port: u16 },
}

#[derive(Debug, Subcommand)]
enum HttpCommand {
    /// Create or update an HTTP route to a pod port.
    Publish {
        port: u16,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        internal: bool,
    },
    /// List this pod's HTTP routes.
    List,
    /// Delete the route for a pod port or route identifier.
    Unpublish { route: String },
}

fn main() -> PodctlResult<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(io::stderr)
        .try_init()
        .map_err(|source| PodctlError::Logging { source }.report())?;
    let invoked_as = std::env::args_os()
        .next()
        .as_deref()
        .and_then(|argument| Path::new(argument).file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned);
    match invoked_as.as_deref() {
        Some("git-remote-tascarrel") => return run_git_remote_helper(),
        Some("tascarrel-git-receive-pack") => {
            return runtime()?.block_on(run_git_receive_pack());
        }
        _ => {}
    }
    runtime()?.block_on(run_cli())
}

/// Creates the runtime shared by command and Git adapter entry points.
fn runtime() -> PodctlResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .escalate(PodctlError::Runtime)
}

/// Parses and executes one ordinary podctl command.
async fn run_cli() -> PodctlResult<()> {
    let args = Args::parse();
    match args.command {
        Command::DeviceLink { path, source } => {
            return create_device_link(&path, &source);
        }
        Command::DeviceRemove { path } => return remove_device_node(&path),
        _ => {}
    }

    let client = PodClient::connect(Path::new(CONTROL_SOCKET)).await?;
    match args.command {
        Command::Identity => print_json(client.identity())?,
        Command::Title { title } => {
            client
                .invoke_pod(pods::SetPodTitleAction {
                    pod_id: client.identity().pod_id.clone(),
                    title: title.into(),
                })
                .await?;
        }
        Command::Destroy => {
            client
                .invoke_pod(pods::DestroyPodAction {
                    pod_id: client.identity().pod_id.clone(),
                })
                .await?;
        }
        Command::Processes { command } => run_process_command(&client, command).await?,
        Command::Chats { command } => run_chat_command(&client, command).await?,
        Command::Ports { command } => run_port_command(&client, command).await?,
        Command::Http { command } => run_http_command(&client, command).await?,
        Command::DeviceLink { .. } | Command::DeviceRemove { .. } => unreachable!(),
    }
    Ok(())
}

/// Executes one process inspection or control command.
async fn run_process_command(client: &PodClient, command: ProcessCommand) -> PodctlResult<()> {
    match command {
        ProcessCommand::List => {
            let output = client
                .invoke_pod(processes::GetPodProcessesAction {
                    pod_id: client.identity().pod_id.clone(),
                })
                .await?;
            print_json(&output)?;
        }
        ProcessCommand::Snapshot { process_id } => {
            let output = client
                .invoke_pod(processes::SnapshotProcessTerminalAction { process_id })
                .await?;
            write!(io::stdout().lock(), "{}", output.snapshot)
                .escalate(PodctlError::WriteOutput)?;
        }
        ProcessCommand::Kill { process_id, signal } => {
            client
                .invoke_pod(processes::KillProcessAction {
                    process_id,
                    signal: signal.into(),
                })
                .await?;
        }
    }
    Ok(())
}

/// Executes one chat inspection command.
async fn run_chat_command(client: &PodClient, command: ChatCommand) -> PodctlResult<()> {
    match command {
        ChatCommand::List => {
            let output = client
                .invoke_pod(chats::GetPodChatsAction {
                    pod_id: client.identity().pod_id.clone(),
                })
                .await?;
            print_json(&output)?;
        }
        ChatCommand::Show { chat_id } => {
            let event = client
                .first_pod_event(chats::ChatSubscription {
                    chat_id,
                    cursor: None,
                })
                .await?;
            let tascarrel_api::types::store::StoreEvent::Snapshot(snapshot) = event.change else {
                return Err(PodctlError::InitialEventNotSnapshot { resource: "chat" }.report());
            };
            print_json(&snapshot.value)?;
        }
    }
    Ok(())
}

/// Executes one dynamic host-loopback forwarding command.
async fn run_port_command(client: &PodClient, command: PortCommand) -> PodctlResult<()> {
    match command {
        PortCommand::Publish { port, title, tab } => {
            require_port(port)?;
            let output = client
                .invoke_host(network::CreatePortForwardAction {
                    workspace: client.identity().workspace.clone(),
                    pod_id: client.identity().pod_id.clone(),
                    pod_port: port,
                    title: title.clone().map(Into::into),
                })
                .await?;
            if tab
                && let Err(error) = client
                    .invoke_host(network::CreateHttpRouteAction {
                        workspace: client.identity().workspace.clone(),
                        pod_id: client.identity().pod_id.clone(),
                        pod_port: port,
                        title: title.unwrap_or_else(|| format!("Port {port}")).into(),
                        internal: false,
                    })
                    .await
            {
                if let Err(rollback) = client
                    .invoke_host(network::DeletePortForwardAction {
                        port_forward_id: output.port_forward_id.clone(),
                    })
                    .await
                {
                    tracing::error!(error = ?rollback, "failed to roll back pod port forward");
                    return Err(error.escalate(PodctlError::PortForwardRollback));
                }
                return Err(error);
            }
            write_line(output.host_port)?;
        }
        PortCommand::List => print_json(&port_forwards(client).await?)?,
        PortCommand::Unpublish { port } => {
            require_port(port)?;
            let forwards = port_forwards(client).await?;
            let forward = forwards
                .port_forwards
                .iter()
                .find(|forward| forward.pod_port == port)
                .ok_or_else(|| PodctlError::PortNotPublished.report())?;
            client
                .invoke_host(network::DeletePortForwardAction {
                    port_forward_id: forward.id.clone(),
                })
                .await?;
        }
    }
    Ok(())
}

/// Executes one HTTP route publication command.
async fn run_http_command(client: &PodClient, command: HttpCommand) -> PodctlResult<()> {
    match command {
        HttpCommand::Publish {
            port,
            title,
            internal,
        } => {
            require_port(port)?;
            let output = client
                .invoke_host(network::CreateHttpRouteAction {
                    workspace: client.identity().workspace.clone(),
                    pod_id: client.identity().pod_id.clone(),
                    pod_port: port,
                    title: title.unwrap_or_else(|| format!("Port {port}")).into(),
                    internal,
                })
                .await?;
            write_line(output.hostname_prefix)?;
        }
        HttpCommand::List => print_json(&http_routes(client).await?)?,
        HttpCommand::Unpublish { route } => {
            let routes = http_routes(client).await?;
            let requested_port = route.parse::<u16>().ok();
            let selected = routes
                .http_routes
                .iter()
                .find(|candidate| {
                    candidate.id.0.as_ref() == route || requested_port == Some(candidate.pod_port)
                })
                .ok_or_else(|| PodctlError::HttpRouteNotFound.report())?;
            client
                .invoke_host(network::DeleteHttpRouteAction {
                    http_route_id: selected.id.clone(),
                })
                .await?;
        }
    }
    Ok(())
}

/// Reads the current pod-scoped dynamic forwards.
async fn port_forwards(client: &PodClient) -> PodctlResult<network::GetPodPortForwardsOutput> {
    client
        .invoke_host(network::GetPodPortForwardsAction {
            workspace: client.identity().workspace.clone(),
            pod_id: client.identity().pod_id.clone(),
        })
        .await
}

/// Reads the current pod-scoped HTTP routes.
async fn http_routes(client: &PodClient) -> PodctlResult<network::GetPodHttpRoutesOutput> {
    client
        .invoke_host(network::GetPodHttpRoutesAction {
            workspace: client.identity().workspace.clone(),
            pod_id: client.identity().pod_id.clone(),
        })
        .await
}

/// Writes one typed value as pretty JSON followed by a newline.
fn print_json(value: &impl Serialize) -> PodctlResult<()> {
    let value = serde_json::to_string_pretty(value).escalate(PodctlError::EncodeOutput)?;
    writeln!(io::stdout().lock(), "{value}").escalate(PodctlError::WriteOutput)?;
    Ok(())
}

/// Writes one display value followed by a newline.
fn write_line(value: impl std::fmt::Display) -> PodctlResult<()> {
    writeln!(io::stdout().lock(), "{value}").escalate(PodctlError::WriteOutput)?;
    Ok(())
}

/// Rejects the reserved TCP port zero.
fn require_port(port: u16) -> PodctlResult<()> {
    if port == 0 {
        Err(PodctlError::InvalidPort.report())
    } else {
        Ok(())
    }
}
