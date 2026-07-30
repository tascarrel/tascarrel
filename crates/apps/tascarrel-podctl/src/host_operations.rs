//! Request, capture, transfer, and observe approval-gated host operations.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::ResultExt as _;
use tascarrel_api::types::host_operations as api;
use tascarrel_protocol::Framed;
use tascarrel_protocol::HostOperationInputResponse;
use tascarrel_protocol::MUX_POD_HOST_OPERATION_INPUT_ENDPOINT;
use tascarrel_protocol::PodHostOperationInputRequest;
use tokio::io::AsyncWriteExt as _;

use crate::client::PodClient;
use crate::error::PodctlError;
use crate::error::PodctlResult;

/// Returns the current caller-visible host-command catalog.
pub(crate) async fn commands(client: &PodClient) -> PodctlResult<api::HostCommandList> {
    client
        .first_host_event(api::HostCommandListChangedSubscription {
            workspace: client.identity().workspace.clone(),
        })
        .await
        .map(|event| event.value)
}

/// Requests a command, transfers its immutable inputs, and follows it to
/// completion.
#[tracing::instrument(level = "debug", skip_all, fields(command = %command), err)]
pub(crate) async fn run(
    client: &PodClient,
    command: String,
    parameters: Vec<String>,
) -> PodctlResult<()> {
    let parameters = parse_parameters(parameters)?;
    let requested = client
        .invoke_host(api::RequestHostOperationAction {
            workspace: client.identity().workspace.clone(),
            command: command.into(),
            parameters,
        })
        .await?;
    for input in requested.inputs {
        let operation_id = requested.operation_id.clone();
        let input_name = input.name.clone();
        let capture =
            tokio::task::spawn_blocking(move || capture_repository(&operation_id, &input))
                .await
                .map_err(|error| error.escalate(PodctlError::CaptureHostInput))??;
        upload_input(
            client,
            &requested.operation_id,
            input_name.as_ref(),
            capture,
        )
        .await?;
    }
    follow_operation(client, requested.operation_id).await
}

/// Prints operations initiated by the current pod.
pub(crate) async fn list(client: &PodClient) -> PodctlResult<api::HostOperationList> {
    let event = client
        .first_host_event(api::HostOperationListChangedSubscription {
            workspace: Some(client.identity().workspace.clone()),
            pod_id: Some(client.identity().pod_id.clone()),
            cursor: None,
        })
        .await?;
    Ok(event.value)
}

/// Withdraws or stops one operation owned by the current pod.
#[tracing::instrument(level = "debug", skip_all, fields(operation_id = %operation_id.0), err)]
pub(crate) async fn cancel(
    client: &PodClient,
    operation_id: api::HostOperationId,
) -> PodctlResult<()> {
    client
        .invoke_host(api::CancelHostOperationAction { operation_id })
        .await?;
    Ok(())
}

fn parse_parameters(
    values: Vec<String>,
) -> PodctlResult<HashMap<tascarrel_api::ArcStr, tascarrel_api::ArcStr>> {
    let mut parameters = HashMap::new();
    for value in values {
        let Some((name, value)) = value.split_once('=') else {
            return Err(PodctlError::InvalidHostParameter.report());
        };
        if name.is_empty() || parameters.insert(name.into(), value.into()).is_some() {
            return Err(PodctlError::InvalidHostParameter.report());
        }
    }
    Ok(parameters)
}

struct CapturedInput {
    temporary: tempfile::TempDir,
    bundle: PathBuf,
    revision: String,
    base_revision: Option<String>,
}

fn capture_repository(
    operation_id: &api::HostOperationId,
    input: &api::HostOperationPendingInput,
) -> PodctlResult<CapturedInput> {
    capture_repository_at(Path::new("/workspace"), operation_id, input)
}

fn capture_repository_at(
    workspace_root: &Path,
    operation_id: &api::HostOperationId,
    input: &api::HostOperationPendingInput,
) -> PodctlResult<CapturedInput> {
    validate_repository_path(input.repository.as_ref())?;
    let repository = workspace_root.join(input.repository.as_ref());
    let canonical = fs::canonicalize(&repository).map_err(|error| {
        error
            .escalate(PodctlError::CaptureHostInput)
            .message("resolve repository path")
    })?;
    if canonical != repository {
        return Err(PodctlError::CaptureHostInput
            .report()
            .message("repository path must not contain symbolic links"));
    }
    let root = git_output(&canonical, ["rev-parse", "--show-toplevel"], None)?;
    if Path::new(root.trim()) != canonical {
        return Err(PodctlError::CaptureHostInput
            .report()
            .message("configured repository is not the Git worktree root"));
    }
    let head = git_output(&canonical, ["rev-parse", "HEAD"], None)?
        .trim()
        .to_owned();
    let temporary = tempfile::Builder::new()
        .prefix("tascarrel-host-operation-")
        .tempdir()
        .escalate(PodctlError::CaptureHostInput)?;
    let revision = match input.capture {
        api::HostOperationCapture::WorkingTree => {
            let index = temporary.path().join("index");
            let index_value = index.as_os_str();
            git_output(&canonical, ["read-tree", &head], Some(index_value))?;
            git_output(&canonical, ["add", "-A", "--", "."], Some(index_value))?;
            let tree = git_output(&canonical, ["write-tree"], Some(index_value))?;
            let tree = tree.trim();
            git_commit_tree(&canonical, tree, &head)?
        }
        api::HostOperationCapture::CleanHead => {
            let status = git_output(
                &canonical,
                ["status", "--porcelain=v1", "--untracked-files=normal"],
                None,
            )?;
            if !status.is_empty() {
                return Err(PodctlError::CaptureHostInput
                    .report()
                    .message("repository must have a clean working tree"));
            }
            head.clone()
        }
        api::HostOperationCapture::Commit => head.clone(),
        api::HostOperationCapture::PublishedRef => {
            let containing = git_output(
                &canonical,
                ["branch", "--remotes", "--contains", &head],
                None,
            )?;
            if containing.trim().is_empty() {
                return Err(PodctlError::CaptureHostInput
                    .report()
                    .message("HEAD is not reachable from a remote-tracking ref"));
            }
            head.clone()
        }
    };
    let reference = format!(
        "refs/tascarrel/host-operations/{}/{}",
        operation_id.0, input.name
    );
    git_output(&canonical, ["update-ref", &reference, &revision], None)?;
    let bundle = temporary.path().join("input.bundle");
    let bundle_value = bundle.to_string_lossy().into_owned();
    let bundled = git_output(
        &canonical,
        ["bundle", "create", &bundle_value, &reference],
        None,
    );
    let cleanup = git_output(&canonical, ["update-ref", "-d", &reference], None);
    if let Err(error) = cleanup {
        if bundled.is_ok() {
            return Err(error);
        }
        tracing::warn!(%error, %reference, "failed to remove host operation capture reference");
    }
    bundled?;
    Ok(CapturedInput {
        temporary,
        bundle,
        revision,
        base_revision: Some(head),
    })
}

fn git_commit_tree(repository: &Path, tree: &str, parent: &str) -> PodctlResult<String> {
    let output = Command::new("git")
        .current_dir(repository)
        .args(["commit-tree", tree, "-p", parent, "-m"])
        .arg("Tascarrel host operation working-state capture")
        .env("GIT_AUTHOR_NAME", "Tascarrel")
        .env("GIT_AUTHOR_EMAIL", "host-operation@tascarrel.invalid")
        .env("GIT_COMMITTER_NAME", "Tascarrel")
        .env("GIT_COMMITTER_EMAIL", "host-operation@tascarrel.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| error.escalate(PodctlError::CaptureHostInput))?;
    git_result(output).map(|revision| revision.trim().to_owned())
}

async fn upload_input(
    client: &PodClient,
    operation_id: &api::HostOperationId,
    input_name: &str,
    capture: CapturedInput,
) -> PodctlResult<()> {
    let length = fs::metadata(&capture.bundle)
        .map_err(|error| error.escalate(PodctlError::CaptureHostInput))?
        .len();
    let channel = client
        .open_channel(MUX_POD_HOST_OPERATION_INPUT_ENDPOINT)
        .await?;
    let mut framed = Framed::new(channel);
    framed
        .write(&PodHostOperationInputRequest {
            operation_id: operation_id.0.to_string(),
            input_name: input_name.to_owned(),
            revision: capture.revision,
            base_revision: capture.base_revision,
            length,
        })
        .await
        .escalate(PodctlError::HostInputTransfer)?;
    let response = framed
        .read::<HostOperationInputResponse>()
        .await
        .escalate(PodctlError::HostInputTransfer)?
        .ok_or_else(|| PodctlError::HostInputTransfer.report())?;
    match response {
        HostOperationInputResponse::Ready => {}
        HostOperationInputResponse::Error { error } => {
            return Err(PodctlError::HostInputRejected(error).report());
        }
        HostOperationInputResponse::Completed => {
            return Err(PodctlError::HostInputTransfer.report());
        }
    }
    let mut channel = framed.into_inner();
    let mut file = tokio::fs::File::open(&capture.bundle)
        .await
        .escalate(PodctlError::HostInputTransfer)?;
    let copied = tokio::io::copy(&mut file, &mut channel)
        .await
        .escalate(PodctlError::HostInputTransfer)?;
    if copied != length {
        return Err(PodctlError::HostInputTransfer.report());
    }
    channel
        .flush()
        .await
        .escalate(PodctlError::HostInputTransfer)?;
    let response = Framed::new(channel)
        .read::<HostOperationInputResponse>()
        .await
        .escalate(PodctlError::HostInputTransfer)?
        .ok_or_else(|| PodctlError::HostInputTransfer.report())?;
    drop(capture.temporary);
    match response {
        HostOperationInputResponse::Completed => Ok(()),
        HostOperationInputResponse::Error { error } => {
            Err(PodctlError::HostInputRejected(error).report())
        }
        HostOperationInputResponse::Ready => Err(PodctlError::HostInputTransfer.report()),
    }
}

async fn follow_operation(
    client: &PodClient,
    operation_id: api::HostOperationId,
) -> PodctlResult<()> {
    let mut after_sequence = None;
    loop {
        let output = client
            .first_host_event(api::HostOperationOutputSubscription {
                operation_id: operation_id.clone(),
                after_sequence,
            })
            .await?;
        if let api::HostOperationOutputUpdate::Chunk(chunk) = output.update {
            after_sequence = Some(chunk.sequence);
            let bytes = chunk.data.as_bytes();
            match chunk.source {
                api::HostOperationOutputSource::Stdout => {
                    std::io::stdout()
                        .lock()
                        .write_all(bytes)
                        .escalate(PodctlError::WriteOutput)?;
                }
                api::HostOperationOutputSource::Stderr => {
                    std::io::stderr()
                        .lock()
                        .write_all(bytes)
                        .escalate(PodctlError::WriteOutput)?;
                }
            }
            continue;
        }
        let list = list(client).await?;
        let operation = list
            .operations
            .iter()
            .find(|operation| operation.id == operation_id)
            .ok_or_else(|| PodctlError::HostOperationUnavailable.report())?;
        match &operation.state {
            api::HostOperationState::Succeeded(_) => return Ok(()),
            api::HostOperationState::Failed(failure) => {
                return Err(PodctlError::HostOperationFailed(failure.message.to_string()).report());
            }
            api::HostOperationState::Rejected(_) => {
                return Err(PodctlError::HostOperationRejected.report());
            }
            api::HostOperationState::Canceled(_) | api::HostOperationState::Interrupted(_) => {
                return Err(PodctlError::HostOperationCanceled.report());
            }
            api::HostOperationState::Preparing
            | api::HostOperationState::AwaitingApproval(_)
            | api::HostOperationState::Starting(_)
            | api::HostOperationState::Running(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

fn validate_repository_path(path: &str) -> PodctlResult<()> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PodctlError::CaptureHostInput.report());
    }
    Ok(())
}

fn git_output<const N: usize>(
    repository: &Path,
    arguments: [&str; N],
    index: Option<&OsStr>,
) -> PodctlResult<String> {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command
        .output()
        .map_err(|error| error.escalate(PodctlError::CaptureHostInput))?;
    git_result(output)
}

fn git_result(output: std::process::Output) -> PodctlResult<String> {
    if !output.status.success() {
        return Err(PodctlError::CaptureHostInput
            .report()
            .message(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    String::from_utf8(output.stdout).map_err(|_| {
        PodctlError::CaptureHostInput
            .report()
            .message("Git returned non-UTF-8 output")
    })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    /// Verifies that parameters use unique, explicit key-value pairs.
    #[test]
    fn parameters_are_explicit_key_value_pairs() {
        let values = parse_parameters(vec!["target=staging".to_owned()]).unwrap();
        assert_eq!(
            values.get("target").map(AsRef::<str>::as_ref),
            Some("staging")
        );
        assert!(parse_parameters(vec!["target".to_owned()]).is_err());
        assert!(parse_parameters(vec!["a=1".to_owned(), "a=2".to_owned()]).is_err());
    }

    /// Verifies that working-tree capture does not mutate the branch or index.
    #[test]
    fn working_tree_capture_is_synthetic_and_leaves_head_and_index_unchanged() {
        let workspace = tempdir().unwrap();
        let repository = workspace.path().join("infrastructure");
        fs::create_dir(&repository).unwrap();
        git_output(&repository, ["init"], None).unwrap();
        fs::write(repository.join("tracked.txt"), "original\n").unwrap();
        git_output(&repository, ["add", "tracked.txt"], None).unwrap();
        git_output(
            &repository,
            [
                "-c",
                "user.name=Tascarrel",
                "-c",
                "user.email=test@tascarrel.invalid",
                "commit",
                "-m",
                "initial",
            ],
            None,
        )
        .unwrap();
        let head = git_output(&repository, ["rev-parse", "HEAD"], None)
            .unwrap()
            .trim()
            .to_owned();
        fs::write(repository.join("tracked.txt"), "modified\n").unwrap();
        fs::write(repository.join("untracked.txt"), "untracked\n").unwrap();

        let captured = capture_repository_at(
            workspace.path(),
            &api::HostOperationId::generate(),
            &api::HostOperationPendingInput {
                name: "source".into(),
                repository: "infrastructure".into(),
                capture: api::HostOperationCapture::WorkingTree,
            },
        )
        .unwrap();

        assert_eq!(
            git_output(&repository, ["rev-parse", "HEAD"], None)
                .unwrap()
                .trim(),
            head
        );
        assert!(
            git_output(&repository, ["diff", "--cached", "--name-only"], None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            git_output(
                &repository,
                ["show", &format!("{}:tracked.txt", captured.revision)],
                None,
            )
            .unwrap(),
            "modified\n"
        );
        assert_eq!(
            git_output(
                &repository,
                ["show", &format!("{}:untracked.txt", captured.revision)],
                None,
            )
            .unwrap(),
            "untracked\n"
        );
        assert!(captured.bundle.is_file());
    }
}
