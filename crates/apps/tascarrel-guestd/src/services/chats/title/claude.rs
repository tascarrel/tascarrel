//! Title generation through an isolated non-interactive Claude Code process.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Deserialize;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use super::GenerateTitleRequest;
use super::GeneratedTitle;
use super::TitleGenerationError;
use super::TitleGenerationService;
use super::error;
use super::process::MAX_PROCESS_OUTPUT_BYTES;
use super::process::OUTPUT_SCHEMA;
use super::process::claude_prompt;
use super::process::read_bounded;
use super::validate_generated_title;
use crate::runtime::command::spawn;
use crate::services::chats::process::ProcessEnvironment;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Efficient Claude model used for title generation unless explicitly
/// overridden.
pub const DEFAULT_CLAUDE_TITLE_GENERATION_MODEL: &str = "claude-haiku-4-5";

/// Title generator that starts a separate `claude -p` process for every
/// request.
pub struct ClaudeExecTitleGenerator {
    executable: PathBuf,
    model: String,
    timeout: Duration,
    concurrency: Semaphore,
    process_environment: Option<std::sync::Arc<dyn ProcessEnvironment>>,
    identity: Option<(u32, u32)>,
}

impl ClaudeExecTitleGenerator {
    /// Creates a title generator using the supplied Claude Code executable.
    #[must_use]
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            model: DEFAULT_CLAUDE_TITLE_GENERATION_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
            concurrency: Semaphore::new(2),
            process_environment: None,
            identity: None,
        }
    }

    /// Applies application-owned environment and credential locations to each
    /// Claude process.
    #[must_use]
    pub fn with_process_environment(
        mut self,
        process_environment: std::sync::Arc<dyn ProcessEnvironment>,
    ) -> Self {
        self.process_environment = Some(process_environment);
        self
    }

    /// Runs title processes as the selected unprivileged VM account.
    #[must_use]
    pub(crate) fn with_identity(mut self, uid: u32, gid: u32) -> Self {
        self.identity = Some((uid, gid));
        self
    }

    #[allow(clippy::too_many_lines)] // The short-lived provider process has one ordered lifecycle.
    async fn run(
        &self,
        request: GenerateTitleRequest,
    ) -> Result<GeneratedTitle, TitleGenerationError> {
        let _permit = self
            .concurrency
            .acquire()
            .await
            .map_err(|_| error("unavailable", "the Claude title generator is shutting down"))?;
        let temporary = TempDir::new().map_err(|source| {
            error(
                "process_setup",
                format!("unable to create the title-generator directory: {source}"),
            )
        })?;
        if let Some((uid, gid)) = self.identity {
            nix::unistd::chown(
                temporary.path(),
                Some(nix::unistd::Uid::from_raw(uid)),
                Some(nix::unistd::Gid::from_raw(gid)),
            )
            .map_err(|source| {
                error(
                    "process_setup",
                    format!("unable to assign the title-generator directory: {source}"),
                )
            })?;
        }
        let prompt = claude_prompt(&request)?;

        let mut command = Command::new(&self.executable);
        command
            .arg("-p")
            .arg("--output-format")
            .arg("json")
            .arg("--json-schema")
            .arg(OUTPUT_SCHEMA)
            .arg("--model")
            .arg(&self.model)
            .arg("--tools")
            .arg("")
            .arg("--permission-mode")
            .arg("dontAsk")
            .arg("--setting-sources=")
            .arg("--no-session-persistence")
            .current_dir(temporary.path())
            .env_remove("NODE_OPTIONS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(process_environment) = &self.process_environment {
            process_environment.apply(&mut command).map_err(|source| {
                error(
                    "process_setup",
                    format!("unable to configure the Claude title environment: {source}"),
                )
            })?;
        }
        if let Some((uid, gid)) = self.identity {
            // The UID transition prevents privileged supplementary groups
            // from crossing into the title process.
            command.uid(uid).gid(gid);
        }

        let mut child = spawn(&mut command).await.map_err(|source| {
            error(
                "process_start",
                format!("unable to start the Claude title generator: {source}"),
            )
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            error(
                "process_start",
                "the Claude title generator did not expose stdin",
            )
        })?;
        stdin.write_all(&prompt).await.map_err(|source| {
            error(
                "process_io",
                format!("unable to send the title source to Claude: {source}"),
            )
        })?;
        stdin.shutdown().await.map_err(|source| {
            error(
                "process_io",
                format!("unable to finish the Claude title input: {source}"),
            )
        })?;
        drop(stdin);

        let stdout = child.stdout.take().ok_or_else(|| {
            error(
                "process_start",
                "the Claude title generator did not expose stdout",
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            error(
                "process_start",
                "the Claude title generator did not expose stderr",
            )
        })?;
        let completion = timeout(self.timeout, async {
            tokio::try_join!(
                child.wait(),
                read_bounded(stdout, MAX_PROCESS_OUTPUT_BYTES),
                read_bounded(stderr, MAX_PROCESS_OUTPUT_BYTES),
            )
        })
        .await;
        let (status, stdout, _stderr) = match completion {
            Ok(Ok(completion)) => completion,
            Ok(Err(source)) => {
                return Err(error(
                    "process_io",
                    format!("unable to read the Claude title result: {source}"),
                ));
            }
            Err(_) => {
                if let Err(cleanup_error) = child.kill().await {
                    tracing::warn!(%cleanup_error, "failed to kill a timed-out Claude title generator");
                }
                if let Err(cleanup_error) = child.wait().await {
                    tracing::warn!(%cleanup_error, "failed to reap a timed-out Claude title generator");
                }
                return Err(error(
                    "timeout",
                    "the Claude title generator exceeded its time limit",
                ));
            }
        };
        if !status.success() {
            return Err(error(
                "process_exited",
                format!("the Claude title generator exited with {status}"),
            ));
        }
        if stdout.truncated {
            return Err(error(
                "invalid_output",
                "the Claude title generator returned too much output",
            ));
        }
        let response =
            serde_json::from_slice::<ClaudeTitleEnvelope>(&stdout.bytes).map_err(|source| {
                error(
                    "invalid_output",
                    format!("unable to decode the Claude title result: {source}"),
                )
            })?;
        validate_generated_title(&response.structured_output.title)
    }
}

impl TitleGenerationService for ClaudeExecTitleGenerator {
    fn generate_title(
        &self,
        request: GenerateTitleRequest,
    ) -> BoxFuture<'_, Result<GeneratedTitle, TitleGenerationError>> {
        Box::pin(self.run(request))
    }
}

#[derive(Deserialize)]
struct ClaudeTitleEnvelope {
    structured_output: ClaudeTitleOutput,
}

#[derive(Deserialize)]
struct ClaudeTitleOutput {
    title: String,
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use tascarrel_api::ArcVec;
    use tascarrel_api::types::chats::ChatHarnessKind;
    use tascarrel_api::types::chats::ChatPrompt;
    use tempfile::TempDir;

    use super::ClaudeExecTitleGenerator;
    use crate::services::chats::title::GenerateTitleRequest;
    use crate::services::chats::title::TitleGenerationService as _;

    /// Confirms the isolated Claude invocation and structured title contract.
    #[tokio::test]
    async fn reads_a_structured_title_from_a_separate_process() {
        let temporary = TempDir::new().unwrap();
        let executable = temporary.path().join("fake-claude");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
input="$(cat)"
case "$input" in
  *'Summarize the user'*) ;;
  *) exit 2 ;;
esac
case "$input" in
  *'"promptText":"Fix the tests"'*) ;;
  *) exit 3 ;;
esac
while [ "$#" -gt 0 ]; do
  case "$1" in
    -p) saw_print=1 ;;
    --model)
      shift
      [ "$1" = "claude-haiku-4-5" ] || exit 4
      ;;
    --tools)
      shift
      [ -z "$1" ] || exit 5
      saw_tools=1
      ;;
    --no-session-persistence) saw_ephemeral=1 ;;
  esac
  shift
done
[ "$saw_print" = 1 ] || exit 6
[ "$saw_tools" = 1 ] || exit 7
[ "$saw_ephemeral" = 1 ] || exit 8
printf '%s\n' '{"structured_output":{"title":"  Fix   flaky tests  "}}'
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let generator = ClaudeExecTitleGenerator::new(executable);

        let generated = generator
            .generate_title(GenerateTitleRequest {
                harness: ChatHarnessKind::ClaudeCode,
                prompt: ChatPrompt {
                    text: Some("Fix the tests".into()),
                    attachments: ArcVec::new(),
                    model: None,
                },
            })
            .await
            .unwrap();

        assert_eq!(generated.title, "Fix flaky tests");
    }
}
