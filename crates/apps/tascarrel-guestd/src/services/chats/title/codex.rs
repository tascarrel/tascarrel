//! Title generation through an isolated non-interactive Codex process.

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
use super::process::INSTRUCTIONS;
use super::process::MAX_PROCESS_OUTPUT_BYTES;
use super::process::OUTPUT_SCHEMA;
use super::process::encode_context;
use super::process::read_bounded;
use super::validate_generated_title;
use crate::runtime::command::spawn;
use crate::services::chats::process::ProcessEnvironment;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Efficient model used for title generation unless explicitly overridden.
pub const DEFAULT_TITLE_GENERATION_MODEL: &str = "gpt-5.6-luna";
const TITLE_GENERATION_REASONING_CONFIG: &str = "model_reasoning_effort=\"low\"";

/// Title generator that starts a separate `codex exec` process for every
/// request.
pub struct CodexExecTitleGenerator {
    executable: PathBuf,
    model: String,
    timeout: Duration,
    concurrency: Semaphore,
    process_environment: Option<std::sync::Arc<dyn ProcessEnvironment>>,
    identity: Option<(u32, u32)>,
}

impl CodexExecTitleGenerator {
    /// Creates a title generator using the supplied Codex executable.
    #[must_use]
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            model: DEFAULT_TITLE_GENERATION_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
            concurrency: Semaphore::new(2),
            process_environment: None,
            identity: None,
        }
    }

    /// Applies application-owned environment and credential locations to each
    /// Codex process.
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
            .map_err(|_| error("unavailable", "the Codex title generator is shutting down"))?;
        let temporary = TempDir::new().map_err(|source| {
            error(
                "process_setup",
                format!("unable to create the title-generator directory: {source}"),
            )
        })?;
        let schema = temporary.path().join("title-output.schema.json");
        std::fs::write(&schema, OUTPUT_SCHEMA).map_err(|source| {
            error(
                "process_setup",
                format!("unable to prepare the title output schema: {source}"),
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

        let mut command = Command::new(&self.executable);
        command
            .arg("exec")
            .arg("--ephemeral")
            .arg("--sandbox")
            .arg("read-only")
            .arg("--ignore-user-config")
            .arg("--ignore-rules")
            .arg("--skip-git-repo-check")
            .arg("--config")
            .arg(TITLE_GENERATION_REASONING_CONFIG)
            .arg("--color")
            .arg("never")
            .arg("--output-schema")
            .arg(&schema)
            .arg("-C")
            .arg(temporary.path());
        command.arg("--model").arg(&self.model);
        command
            .arg(INSTRUCTIONS)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(process_environment) = &self.process_environment {
            process_environment.apply(&mut command).map_err(|source| {
                error(
                    "process_setup",
                    format!("unable to configure the Codex title environment: {source}"),
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
                format!("unable to start the Codex title generator: {source}"),
            )
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            error(
                "process_start",
                "the Codex title generator did not expose stdin",
            )
        })?;
        let context = encode_context(&request)?;
        stdin.write_all(&context).await.map_err(|source| {
            error(
                "process_io",
                format!("unable to send the title source to Codex: {source}"),
            )
        })?;
        stdin.shutdown().await.map_err(|source| {
            error(
                "process_io",
                format!("unable to finish the Codex title input: {source}"),
            )
        })?;
        drop(stdin);

        let stdout = child.stdout.take().ok_or_else(|| {
            error(
                "process_start",
                "the Codex title generator did not expose stdout",
            )
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            error(
                "process_start",
                "the Codex title generator did not expose stderr",
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
                    format!("unable to read the Codex title result: {source}"),
                ));
            }
            Err(_) => {
                if let Err(cleanup_error) = child.kill().await {
                    tracing::warn!(%cleanup_error, "failed to kill a timed-out Codex title generator");
                }
                if let Err(cleanup_error) = child.wait().await {
                    tracing::warn!(%cleanup_error, "failed to reap a timed-out Codex title generator");
                }
                return Err(error(
                    "timeout",
                    "the Codex title generator exceeded its time limit",
                ));
            }
        };
        if !status.success() {
            return Err(error(
                "process_exited",
                format!("the Codex title generator exited with {status}"),
            ));
        }
        if stdout.truncated {
            return Err(error(
                "invalid_output",
                "the Codex title generator returned too much output",
            ));
        }
        let response =
            serde_json::from_slice::<CodexTitleOutput>(&stdout.bytes).map_err(|source| {
                error(
                    "invalid_output",
                    format!("unable to decode the Codex title result: {source}"),
                )
            })?;
        validate_generated_title(&response.title)
    }
}

impl TitleGenerationService for CodexExecTitleGenerator {
    fn generate_title(
        &self,
        request: GenerateTitleRequest,
    ) -> BoxFuture<'_, Result<GeneratedTitle, TitleGenerationError>> {
        Box::pin(self.run(request))
    }
}

#[derive(Deserialize)]
struct CodexTitleOutput {
    title: String,
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use tascarrel_api::ArcVec;
    use tascarrel_api::types::chats::ChatHarnessKind;
    use tascarrel_api::types::chats::ChatPrompt;
    use tempfile::TempDir;

    use super::CodexExecTitleGenerator;
    use crate::services::chats::title::GenerateTitleRequest;
    use crate::services::chats::title::TitleGenerationService as _;

    /// Confirms the isolated Codex invocation and structured title contract.
    #[tokio::test]
    async fn reads_a_structured_title_from_a_separate_process() {
        let temporary = TempDir::new().unwrap();
        let executable = temporary.path().join("fake-codex");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
input="$(cat)"
case "$input" in
  *'"promptText":"Fix the tests"'*) ;;
  *) exit 2 ;;
esac
case " $* " in
  *' --model gpt-5.6-luna '*) ;;
  *) exit 3 ;;
esac
case " $* " in
  *' --config model_reasoning_effort="low" '*) ;;
  *) exit 4 ;;
esac
case "$*" in
  *'Summarize the user'*) ;;
  *) exit 5 ;;
esac
printf '%s\n' '{"title":"  Fix   flaky tests  "}'
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let generator = CodexExecTitleGenerator::new(executable);

        let generated = generator
            .generate_title(GenerateTitleRequest {
                harness: ChatHarnessKind::Codex,
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
