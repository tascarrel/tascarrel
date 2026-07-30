//! Local managed-VM integration coverage for overlay host shares.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use reportify::Report;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tascarrel_api::GuestAction;
use tascarrel_api::GuestSubscription;
use tascarrel_api::HostAction;
use tascarrel_api::HostSubscription;
use tascarrel_api::types::pods;
use tascarrel_api::types::processes;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::shares;
use tascarrel_api::types::workspaces;
use tascarrel_protocol::control_plane;
use tascarrel_protocol::control_plane::StreamTransport;
use tascarrel_protocol::control_plane::policy::DenyAll;
use tascarrel_protocol::control_plane::server;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

const WORKSPACE: &str = "overlay-e2e";
const SHARE: &str = "source";
const SERVER_START_TIMEOUT: Duration = Duration::from_mins(1);
const PROCESS_TIMEOUT: Duration = Duration::from_mins(3);

/// Verifies the complete local hostd, managed guestd, pod, `ShareFS`, and
/// approval path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires KVM and TASCARREL_E2E_* guest artifact paths"]
async fn managed_vm_overlay_share_round_trip() -> Result<()> {
    let home = tempfile::tempdir().context("create isolated Tascarrel home")?;
    let host_share = tempfile::tempdir().context("create isolated host share")?;
    let payload = tempfile::tempdir().context("create guest payload directory")?;
    let local_binaries = tempfile::tempdir().context("create local binary directory")?;

    prepare_host_share(host_share.path())?;
    prepare_workspace(
        home.path(),
        host_share.path(),
        &e2e_path("TASCARREL_E2E_BUSYBOX")?,
    )?;
    extract_payload(&e2e_path("TASCARREL_E2E_GUEST_PAYLOAD")?, payload.path())?;
    prepare_local_binaries(local_binaries.path())?;

    let socket = home.path().join("state/runtime/control.sock");
    let mut host = ServerGuard::start(
        home.path(),
        payload.path(),
        local_binaries.path(),
        &e2e_path("TASCARREL_E2E_QEMU")?,
    )?;
    let client = connect_ready(&socket).await?;
    let workspace = workspaces::WorkspaceName::new(WORKSPACE);
    let pod_id = create_mutated_pod(&client, &workspace, host_share.path()).await?;
    inspect_conflict_and_apply(&client, &workspace, &pod_id, host_share.path()).await?;
    verify_cleared_overlay(&client, &workspace, &pod_id).await?;
    client
        .invoke_guest(
            &workspace,
            pods::StopPodAction {
                pod_id: pod_id.clone(),
            },
        )
        .await
        .context("stop verified pod")?;
    client
        .invoke_guest(&workspace, pods::DestroyPodAction { pod_id })
        .await
        .context("destroy verified pod")?;
    client
        .invoke_host(workspaces::StopWorkspaceAction {
            workspace: workspace.clone(),
        })
        .await
        .context("stop managed workspace")?;
    host.shutdown()
        .context("shut down local Tascarrel server")?;
    Ok(())
}

async fn create_mutated_pod(
    client: &LocalClient,
    workspace: &workspaces::WorkspaceName,
    host_share: &Path,
) -> Result<pods::PodId> {
    let pod_id = client
        .invoke_guest(
            workspace,
            pods::CreatePodAction {
                title: Some("Overlay end-to-end".into()),
            },
        )
        .await
        .context("create overlay test pod")?
        .pod_id;
    run_in_pod(
        client,
        workspace,
        &pod_id,
        "mutate overlay share",
        "printf 'pod version\\n' > /mnt/source/conflict.txt; \
         /bin/busybox mkdir -p /mnt/source/added/nested; \
         printf 'added from pod\\n' > /mnt/source/added/nested/new.txt; \
         /bin/busybox rm /mnt/source/delete.txt",
    )
    .await?;

    client
        .invoke_guest(
            workspace,
            pods::StopPodAction {
                pod_id: pod_id.clone(),
            },
        )
        .await
        .context("stop pod with retained overlay state")?;
    fs::write(host_share.join("dynamic.txt"), b"dynamic lower\n")
        .context("add dynamic lower entry")?;
    fs::write(host_share.join("conflict.txt"), b"host version\n")
        .context("make concurrent host change")?;
    client
        .invoke_guest(
            workspace,
            pods::StartPodAction {
                pod_id: pod_id.clone(),
            },
        )
        .await
        .context("restart pod with retained overlay state")?;
    run_in_pod(
        client,
        workspace,
        &pod_id,
        "verify retained and dynamic views",
        "/bin/busybox test \"$(/bin/busybox cat /mnt/source/conflict.txt)\" = 'pod version'; \
         /bin/busybox test \"$(/bin/busybox cat /mnt/source/dynamic.txt)\" = 'dynamic lower'; \
         /bin/busybox test \"$(/bin/busybox cat /mnt/source/added/nested/new.txt)\" = 'added from pod'; \
         /bin/busybox test ! -e /mnt/source/delete.txt",
    )
    .await?;
    Ok(pod_id)
}

async fn inspect_conflict_and_apply(
    client: &LocalClient,
    workspace: &workspaces::WorkspaceName,
    pod_id: &pods::PodId,
    host_share: &Path,
) -> Result<()> {
    let inspected = client
        .invoke_host(shares::InspectShareOverlayAction {
            workspace: workspace.clone(),
            pod_id: pod_id.clone(),
            share: SHARE.into(),
        })
        .await
        .context("inspect retained overlay revision")?;
    let changed_paths = inspected
        .changes
        .iter()
        .map(|change| change.path.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        changed_paths,
        [
            "added",
            "added/nested",
            "added/nested/new.txt",
            "conflict.txt",
            "delete.txt",
        ]
    );

    let conflicted = client
        .invoke_host(shares::ApplyShareOverlayAction {
            workspace: workspace.clone(),
            pod_id: pod_id.clone(),
            share: SHARE.into(),
            revision: inspected.revision.clone(),
        })
        .await
        .context("apply overlay revision with concurrent host change")?;
    let shares::ShareOverlayApplyResult::Conflicts(conflicts) = conflicted.result else {
        bail!("concurrent host edit did not produce an overlay conflict");
    };
    assert_eq!(conflicts.conflicts.len(), 1);
    assert_eq!(conflicts.conflicts[0].path.as_ref(), "conflict.txt");
    assert!(conflicts.conflicts[0].text_diff.is_some());
    assert_eq!(
        fs::read(host_share.join("conflict.txt"))?,
        b"host version\n"
    );
    assert!(host_share.join("delete.txt").exists());
    assert!(!host_share.join("added").exists());

    fs::write(host_share.join("conflict.txt"), b"base version\n")
        .context("resolve host conflict")?;
    let applied = client
        .invoke_host(shares::ApplyShareOverlayAction {
            workspace: workspace.clone(),
            pod_id: pod_id.clone(),
            share: SHARE.into(),
            revision: inspected.revision,
        })
        .await
        .context("apply resolved overlay revision")?;
    assert!(matches!(
        applied.result,
        shares::ShareOverlayApplyResult::Applied
    ));
    assert_eq!(fs::read(host_share.join("conflict.txt"))?, b"pod version\n");
    assert_eq!(
        fs::read(host_share.join("added/nested/new.txt"))?,
        b"added from pod\n"
    );
    assert!(!host_share.join("delete.txt").exists());
    assert_eq!(
        fs::read(host_share.join("dynamic.txt"))?,
        b"dynamic lower\n"
    );
    Ok(())
}

async fn verify_cleared_overlay(
    client: &LocalClient,
    workspace: &workspaces::WorkspaceName,
    pod_id: &pods::PodId,
) -> Result<()> {
    let cleared = client
        .invoke_host(shares::InspectShareOverlayAction {
            workspace: workspace.clone(),
            pod_id: pod_id.clone(),
            share: SHARE.into(),
        })
        .await
        .context("inspect cleared overlay state")?;
    assert!(cleared.changes.is_empty());
    run_in_pod(
        client,
        workspace,
        pod_id,
        "verify applied lower after clear",
        "/bin/busybox test \"$(/bin/busybox cat /mnt/source/conflict.txt)\" = 'pod version'; \
         /bin/busybox test \"$(/bin/busybox cat /mnt/source/added/nested/new.txt)\" = 'added from pod'; \
         /bin/busybox test \"$(/bin/busybox cat /mnt/source/dynamic.txt)\" = 'dynamic lower'; \
         /bin/busybox test ! -e /mnt/source/delete.txt",
    )
    .await?;
    Ok(())
}

fn prepare_host_share(root: &Path) -> Result<()> {
    fs::write(root.join("conflict.txt"), b"base version\n").context("write conflict fixture")?;
    fs::write(root.join("delete.txt"), b"delete from pod\n").context("write deletion fixture")?;
    Ok(())
}

fn prepare_workspace(home: &Path, host_share: &Path, busybox: &Path) -> Result<()> {
    let workspace = home.join("config/workspaces").join(WORKSPACE);
    let image = workspace.join("image");
    fs::create_dir_all(&image).context("create workspace image directory")?;
    fs::write(
        workspace.join("config.toml"),
        format!(
            "[vm]\ncores = 2\nmemory = \"2GiB\"\ndisk = \"2GiB\"\n\n\
             [shares.{SHARE}]\npath = \"{}\"\nmode = \"Overlay\"\n",
            host_share.display()
        ),
    )
    .context("write workspace configuration")?;
    fs::write(
        image.join("Dockerfile"),
        b"FROM scratch\nCOPY --chmod=0755 busybox /bin/busybox\n",
    )
    .context("write scratch image Dockerfile")?;
    fs::copy(busybox, image.join("busybox")).context("copy static BusyBox into image context")?;
    Ok(())
}

fn extract_payload(guest_payload: &Path, destination: &Path) -> Result<()> {
    let status = Command::new("tar")
        .args(["-xJf"])
        .arg(guest_payload.join("payload.tar.xz"))
        .args(["-C"])
        .arg(destination)
        .status()
        .context("run tar for guest payload")?;
    ensure!(
        status.success(),
        "guest payload extraction failed: {status}"
    );
    Ok(())
}

fn prepare_local_binaries(destination: &Path) -> Result<()> {
    for (environment, installed, exposed) in [
        (
            "TASCARREL_E2E_GUEST",
            "bin/tascarrel-guest",
            "tascarrel-guest",
        ),
        ("TASCARREL_E2E_PODD", "bin/tascarrel-podd", "tascarrel-podd"),
        ("TASCARREL_E2E_PODCTL", "bin/podctl", "podctl"),
        ("TASCARREL_E2E_TASCI", "bin/tasci-exec", "tasci-exec"),
    ] {
        fs::copy(
            e2e_path(environment)?.join(installed),
            destination.join(exposed),
        )
        .with_context(|| format!("copy local binary {exposed}"))?;
    }
    Ok(())
}

fn e2e_path(name: &str) -> Result<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("{name} is required for the ignored local integration test"))
}

async fn connect_ready(socket: &Path) -> Result<LocalClient> {
    let deadline = Instant::now() + SERVER_START_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match LocalClient::connect(socket).await {
            Ok(client) => {
                match client
                    .first_host_event(workspaces::WorkspaceListChangedSubscription { cursor: None })
                    .await
                {
                    Ok(_) => return Ok(client),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(last_error
        .unwrap_or_else(|| anyhow!("host control socket did not become ready"))
        .context("wait for local Tascarrel server"))
}

async fn run_in_pod(
    client: &LocalClient,
    workspace: &workspaces::WorkspaceName,
    pod_id: &pods::PodId,
    title: &str,
    script: &str,
) -> Result<()> {
    let spawned = client
        .invoke_guest(
            workspace,
            processes::SpawnProcessAction {
                pod_id: pod_id.clone(),
                start_pod: Some(true),
                title: title.into(),
                executable: "/bin/busybox".into(),
                arguments: vec!["sh".into(), "-ec".into(), script.into()].into(),
                environment: HashMap::new(),
                working_directory: Some("/workspace".into()),
                terminal: None,
                log_stdout: Some(true),
                profile: processes::ProcessExecutionProfile::User,
            },
        )
        .await
        .with_context(|| format!("spawn process {title:?}"))?;
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while Instant::now() < deadline {
        let processes = client
            .invoke_guest(
                workspace,
                processes::GetPodProcessesAction {
                    pod_id: pod_id.clone(),
                },
            )
            .await
            .with_context(|| format!("poll process {title:?}"))?;
        let process = processes
            .processes
            .iter()
            .find(|process| process.id == spawned.process_id)
            .ok_or_else(|| anyhow!("spawned process disappeared: {title}"))?;
        match &process.status {
            processes::ProcessState::Exited(exit) if exit.code == Some(0) => return Ok(()),
            processes::ProcessState::Exited(exit) => {
                let log = process_log(client, workspace, &spawned.process_id).await;
                bail!(
                    "process {title:?} exited unsuccessfully: code {:?}, signal {:?}; {log}",
                    exit.code,
                    exit.signal
                );
            }
            processes::ProcessState::Failed(failure) => {
                let log = process_log(client, workspace, &spawned.process_id).await;
                bail!("process {title:?} failed: {}; {log}", failure.message);
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    bail!("process {title:?} did not finish within {PROCESS_TIMEOUT:?}");
}

async fn process_log(
    client: &LocalClient,
    workspace: &workspaces::WorkspaceName,
    process_id: &processes::ProcessId,
) -> String {
    match client
        .first_guest_event(
            workspace,
            processes::ProcessLogSubscription {
                process_id: process_id.clone(),
                last_line: None,
            },
        )
        .await
    {
        Ok(event) => event
            .lines
            .iter()
            .map(|line| line.content.as_ref())
            .collect::<Vec<_>>()
            .join(" | "),
        Err(error) => format!("failed to read process log: {error:#}"),
    }
}

struct ServerGuard {
    child: Option<Child>,
    log: PathBuf,
}

impl ServerGuard {
    fn start(home: &Path, payload: &Path, local_binaries: &Path, qemu: &Path) -> Result<Self> {
        let log = home.join("server.log");
        let stdout = File::create(&log).context("create local server log")?;
        let stderr = stdout.try_clone().context("clone local server log")?;
        let kernel_append =
            fs::read_to_string(payload.join("kernel-append")).context("read kernel parameters")?;
        let child = Command::new(env!("CARGO_BIN_EXE_tascarrel"))
            .env("TASCARREL_HOME", home)
            .env("RUST_LOG", "tascarrel_host=debug,tascarrel_guest=debug")
            .arg("--image")
            .arg(payload.join("system.erofs"))
            .arg("--kernel")
            .arg(payload.join("kernel"))
            .arg("--initrd")
            .arg(payload.join("initrd"))
            .arg("--kernel-append")
            .arg(kernel_append.trim())
            .arg("--local-binaries")
            .arg(local_binaries)
            .arg("--qemu")
            .arg(qemu)
            .args([
                "--architecture",
                "x86_64",
                "--acceleration",
                "kvm",
                "--memory",
                "2048",
                "--cpus",
                "2",
                "--state-disk-size",
                "2GiB",
                "--startup-timeout",
                "300",
                "--shutdown-timeout",
                "30",
                "--web-address",
                "127.0.0.1:0",
            ])
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .context("start local Tascarrel server")?;
        Ok(Self {
            child: Some(child),
            log,
        })
    }

    fn shutdown(&mut self) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        terminate(child)?;
        self.child = None;
        Ok(())
    }

    fn print_log(&self) {
        let Ok(mut file) = File::open(&self.log) else {
            return;
        };
        let mut contents = String::new();
        if file.read_to_string(&mut contents).is_ok() {
            eprintln!("local Tascarrel server log:\n{contents}");
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if let Err(error) = terminate(child) {
            eprintln!("failed to stop local Tascarrel server: {error:#}");
        }
        self.print_log();
    }
}

fn terminate(child: &mut Child) -> Result<()> {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .context("send SIGTERM to local Tascarrel server")?;
    ensure!(status.success(), "kill command failed: {status}");
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if child
            .try_wait()
            .context("poll local Tascarrel server")?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    child
        .kill()
        .context("kill unresponsive local Tascarrel server")?;
    child.wait().context("reap local Tascarrel server")?;
    bail!("local Tascarrel server did not stop after SIGTERM");
}

struct LocalClient {
    peer: server::Peer,
    connection: JoinHandle<control_plane::Result<()>>,
}

impl LocalClient {
    async fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connect to {}", path.display()))?;
        let server = server::Server::new(RejectService, RejectRouter);
        let (peer, connection) = server
            .connect(
                StreamTransport::new(stream),
                DenyAll,
                control_plane::Config::default(),
            )
            .map_err(|error| anyhow!(error.to_string()))?;
        Ok(Self {
            peer,
            connection: tokio::spawn(connection),
        })
    }

    async fn invoke_host<A>(&self, input: A) -> Result<A::Output>
    where
        A: HostAction,
    {
        self.invoke(wire::Address::Host, A::NAME, input).await
    }

    async fn invoke_guest<A>(
        &self,
        workspace: &workspaces::WorkspaceName,
        input: A,
    ) -> Result<A::Output>
    where
        A: GuestAction,
    {
        self.invoke(
            wire::Address::Workspace(wire::WorkspaceAddress {
                workspace: workspace.clone(),
            }),
            A::NAME,
            input,
        )
        .await
    }

    async fn invoke<I, O>(&self, target: wire::Address, procedure: &str, input: I) -> Result<O>
    where
        I: Serialize,
        O: DeserializeOwned,
    {
        let input = serde_json::to_value(input).context("encode action")?;
        let mut rpc = self
            .peer
            .invoke(wire::RpcInvocation {
                id: wire::InvocationId::generate(),
                target,
                context: None,
                procedure: procedure.into(),
                input,
                timeout_ms: None,
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        match rpc.recv().await {
            Some(wire::RpcMessage::Completed(completed)) => {
                serde_json::from_value(completed.output).context("decode action output")
            }
            Some(wire::RpcMessage::Failed(failed)) => Err(anyhow!(failed.error.to_string())),
            Some(wire::RpcMessage::Canceled(_)) => bail!("action was canceled"),
            Some(wire::RpcMessage::Invoke(_) | wire::RpcMessage::Cancel(_)) => {
                bail!("server returned an invalid action response");
            }
            None => bail!("server closed the control plane before replying"),
        }
    }

    async fn first_host_event<S>(&self, input: S) -> Result<S::Event>
    where
        S: HostSubscription,
    {
        self.first_event(wire::Address::Host, S::NAME, input).await
    }

    async fn first_guest_event<S>(
        &self,
        workspace: &workspaces::WorkspaceName,
        input: S,
    ) -> Result<S::Event>
    where
        S: GuestSubscription,
    {
        self.first_event(
            wire::Address::Workspace(wire::WorkspaceAddress {
                workspace: workspace.clone(),
            }),
            S::NAME,
            input,
        )
        .await
    }

    async fn first_event<S, E>(&self, target: wire::Address, name: &str, input: S) -> Result<E>
    where
        S: Serialize,
        E: DeserializeOwned,
    {
        let input = serde_json::to_value(input).context("encode subscription")?;
        let mut subscription = self
            .peer
            .subscribe(wire::SubscriptionStart {
                id: wire::SubscriptionId::generate(),
                target,
                context: None,
                subscription: name.into(),
                input,
            })
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        subscription
            .grant_credit(1)
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
        match subscription.recv().await {
            Some(wire::SubscriptionMessage::Event(event)) => {
                serde_json::from_value(event.event).context("decode subscription event")
            }
            Some(wire::SubscriptionMessage::Failed(failed)) => {
                Err(anyhow!(failed.error.to_string()))
            }
            Some(wire::SubscriptionMessage::Completed(_)) => {
                bail!("subscription completed before its first event");
            }
            Some(
                wire::SubscriptionMessage::Subscribe(_)
                | wire::SubscriptionMessage::GrantCredit(_)
                | wire::SubscriptionMessage::Unsubscribe(_),
            ) => bail!("server returned an invalid subscription response"),
            None => bail!("server closed the subscription before its first event"),
        }
    }
}

impl Drop for LocalClient {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

#[derive(Clone, Copy)]
struct RejectService;

impl server::Service for RejectService {
    fn invoke(
        &self,
        _invocation: wire::RpcInvocation,
    ) -> server::OperationFuture<'static, serde_json::Value> {
        Box::pin(async { Err(forbidden()) })
    }

    fn subscribe(
        &self,
        _subscription: wire::SubscriptionStart,
    ) -> server::OperationFuture<'static, Box<dyn server::EventSource>> {
        Box::pin(async { Err(forbidden()) })
    }
}

#[derive(Clone, Copy)]
struct RejectRouter;

impl server::Router for RejectRouter {
    fn resolve(&self, _target: wire::Address) -> server::OperationFuture<'static, server::Route> {
        Box::pin(async { Err(forbidden()) })
    }
}

fn forbidden() -> Report<wire::OperationError> {
    wire::OperationError::forbidden()
}
