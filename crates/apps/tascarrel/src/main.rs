//! Primary Tascarrel server executable.

use std::process::ExitCode;

use clap::Parser;
use reportify::ErrorExt as _;
use tascarrel_api::types::host::ServerIssue;
use tascarrel_api::types::host::ServerIssueSeverity;
use tascarrel_api::types::host::ServerStartupPhase;
use tascarrel_cli::doctor;
use tascarrel_cli::embedded;
use tascarrel_cli::install;
use tascarrel_cli::install::PayloadPreparationProgress;
use tascarrel_host::StartupFailure;
use tascarrel_host::StartupReporter;
use tascarrel_host::daemon::DaemonOptions;
use tascarrel_host::daemon::StartupPreparation;

#[derive(Debug, Parser)]
#[command(name = "tascarrel", version, about = "Run the Tascarrel server")]
struct ServerCli {
    #[command(flatten)]
    options: DaemonOptions,
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
    let options = ServerCli::parse().options.with_default_web_address();
    let payload = embedded::payload();
    let result =
        tascarrel_host::daemon::run_with_startup(options, move |options, reporter| async move {
            tokio::task::spawn_blocking(move || prepare_server(options, payload, &reporter))
                .await
                .map_err(|error| {
                    let detail = error.to_string();
                    error.escalate(StartupFailure::retryable(
                        "startup-task-failed",
                        "The Tascarrel startup task stopped unexpectedly",
                        detail,
                    ))
                })?
        })
        .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tascarrel: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[tracing::instrument(
    name = "tascarrel.server.prepare",
    level = "info",
    skip_all,
    err(Debug)
)]
fn prepare_server(
    options: DaemonOptions,
    payload: Option<embedded::EmbeddedPayload>,
    reporter: &StartupReporter,
) -> std::result::Result<StartupPreparation, reportify::Report<StartupFailure>> {
    reporter.starting(
        ServerStartupPhase::CheckingHost,
        "Checking required host capabilities",
    );
    let report = doctor::inspect_runtime();
    let issues = report
        .checks
        .iter()
        .filter(|check| check.status != doctor::CheckStatus::Ok)
        .map(startup_issue)
        .collect::<Vec<_>>();
    if !report.is_healthy() {
        return Err(StartupFailure::retryable_with_issues(
            "host-check-failed",
            "This computer does not satisfy Tascarrel's host requirements",
            issues,
        )
        .report());
    }
    let warnings = issues;

    let Some(payload) = payload else {
        return Ok(StartupPreparation { options, warnings });
    };
    let progress_reporter = reporter.clone();
    let prepared = install::prepare_with_progress(payload, move |progress| match progress {
        PayloadPreparationProgress::Validating => progress_reporter.starting(
            ServerStartupPhase::ValidatingPayload,
            "Validating the embedded Tascarrel payload",
        ),
        PayloadPreparationProgress::WaitingForLock => progress_reporter.starting(
            ServerStartupPhase::ValidatingPayload,
            "Waiting for another payload preparation to finish",
        ),
        PayloadPreparationProgress::Extracting {
            completed_bytes,
            total_bytes,
        } => progress_reporter.extracting_payload(completed_bytes, total_bytes),
        PayloadPreparationProgress::Activating => progress_reporter.starting(
            ServerStartupPhase::ActivatingPayload,
            "Validating and activating the Tascarrel payload",
        ),
        PayloadPreparationProgress::Pruning => progress_reporter.starting(
            ServerStartupPhase::ActivatingPayload,
            "Cleaning up superseded Tascarrel payloads",
        ),
    })
    .map_err(|error| {
        let detail = format!("{error:#}");
        StartupFailure::retryable_with_issues(
            "payload-preparation-failed",
            "The Tascarrel runtime payload could not be prepared",
            warnings
                .iter()
                .cloned()
                .chain(std::iter::once(tascarrel_host::server_issue(
                    "payload-preparation-failed",
                    ServerIssueSeverity::Error,
                    "Runtime payload preparation failed",
                    detail.clone(),
                    Some(
                        "Check available disk space and permissions for TASCARREL_HOME, then retry."
                            .to_owned(),
                    ),
                )))
                .collect(),
        )
        .report()
        .message(detail)
    })?;
    Ok(StartupPreparation {
        options: options.with_payload_defaults(prepared.guest()),
        warnings,
    })
}

fn startup_issue(check: &doctor::DependencyCheck) -> ServerIssue {
    let severity = match check.status {
        doctor::CheckStatus::Ok | doctor::CheckStatus::Warning => ServerIssueSeverity::Warning,
        doctor::CheckStatus::Error => ServerIssueSeverity::Error,
    };
    let normalized_name = check.name.to_ascii_lowercase();
    let code = if normalized_name.starts_with("qemu-system") && check.message.contains("not found")
    {
        "qemu-not-found"
    } else if normalized_name.starts_with("qemu") {
        "qemu-unavailable"
    } else if normalized_name == "kvm access" {
        "kvm-unavailable"
    } else if normalized_name == "hvf access" {
        "hvf-unavailable"
    } else if normalized_name.starts_with("git") {
        "git-unavailable"
    } else if normalized_name.starts_with("sops") {
        "sops-unavailable"
    } else {
        "host-requirement-unavailable"
    };
    tascarrel_host::server_issue(
        code,
        severity,
        check.name.clone(),
        check.message.clone(),
        remediation(code),
    )
}

fn remediation(code: &str) -> Option<String> {
    match code {
        "qemu-not-found" => Some(
            "Install QEMU and make it available to graphical applications, or set TASCARREL_QEMU to its absolute path."
                .to_owned(),
        ),
        "qemu-unavailable" => {
            Some("Install a QEMU build with the required accelerator and virtio devices.".to_owned())
        }
        "kvm-unavailable" => Some(
            "Enable KVM and grant this user read/write access to /dev/kvm, then sign out and back in if group membership changed."
                .to_owned(),
        ),
        "git-unavailable" => {
            Some("Install Git or set TASCARREL_GIT to its absolute path.".to_owned())
        }
        "sops-unavailable" => Some(
            "Install SOPS or set TASCARREL_SOPS to enable SOPS-backed secrets.".to_owned(),
        ),
        _ => None,
    }
}
