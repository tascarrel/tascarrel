//! Observable lifecycle state for the host server bootstrap process.
//!
//! [`StartupReporter`] publishes one versioned snapshot to the bootstrap HTTP
//! service. Startup producers update it while checking the host, preparing the
//! embedded payload, and initializing the control plane.

use tascarrel_api::types::host::HostInstanceId;
use tascarrel_api::types::host::PayloadExtractionProgress;
use tascarrel_api::types::host::ServerFailure;
use tascarrel_api::types::host::ServerIssue;
use tascarrel_api::types::host::ServerIssueSeverity;
use tascarrel_api::types::host::ServerReady;
use tascarrel_api::types::host::ServerStarting;
use tascarrel_api::types::host::ServerStartupPhase;
use tascarrel_api::types::host::ServerState;
use tascarrel_api::types::host::ServerStatus;
use thiserror::Error;
use tokio::sync::watch;

/// Publishes the current lifecycle of one running Tascarrel server.
#[derive(Clone, Debug)]
pub struct StartupReporter {
    status: watch::Sender<ServerStatus>,
}

impl StartupReporter {
    /// Creates the initial host-checking status for one server process.
    #[must_use]
    pub fn new() -> Self {
        let status = ServerStatus {
            server_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: tascarrel_protocol::PROTOCOL_VERSION,
            instance_id: HostInstanceId::generate(),
            state: starting_state(
                ServerStartupPhase::CheckingHost,
                "Checking required host capabilities",
                None,
            ),
        };
        let (status, _) = watch::channel(status);
        Self { status }
    }

    /// Returns the current server status snapshot.
    #[must_use]
    pub fn current(&self) -> ServerStatus {
        self.status.borrow().clone()
    }

    /// Subscribes to the current snapshot and later lifecycle changes.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<ServerStatus> {
        self.status.subscribe()
    }

    /// Publishes one startup phase without payload byte progress.
    pub fn starting(&self, phase: ServerStartupPhase, detail: impl Into<String>) {
        self.publish_starting(phase, detail, None);
    }

    /// Publishes compressed payload extraction progress.
    pub fn extracting_payload(&self, completed_bytes: u64, total_bytes: u64) {
        self.publish_starting(
            ServerStartupPhase::ExtractingPayload,
            "Extracting the Tascarrel runtime payload",
            Some(PayloadExtractionProgress {
                completed_bytes,
                total_bytes,
            }),
        );
    }

    /// Retains an actionable startup failure while the bootstrap server stays
    /// available.
    pub fn failed(&self, failure: &StartupFailure) {
        self.status.send_replace(ServerStatus {
            state: ServerState::Failed(ServerFailure {
                code: failure.code.clone().into(),
                summary: failure.summary.clone().into(),
                issues: failure.issues.clone().into(),
                retryable: failure.retryable,
            }),
            ..self.current()
        });
    }

    /// Publishes that the application and control plane are ready.
    pub fn ready(&self, warnings: Vec<ServerIssue>) {
        self.status.send_replace(ServerStatus {
            state: ServerState::Ready(ServerReady {
                warnings: warnings.into(),
            }),
            ..self.current()
        });
    }

    fn publish_starting(
        &self,
        phase: ServerStartupPhase,
        detail: impl Into<String>,
        payload: Option<PayloadExtractionProgress>,
    ) {
        self.status.send_replace(ServerStatus {
            state: starting_state(phase, detail, payload),
            ..self.current()
        });
    }
}

impl Default for StartupReporter {
    fn default() -> Self {
        Self::new()
    }
}

/// Actionable failure returned by one server startup attempt.
#[derive(Debug, Error)]
#[error("Tascarrel server startup failed: {summary}")]
pub struct StartupFailure {
    code: String,
    summary: String,
    issues: Vec<ServerIssue>,
    retryable: bool,
}

impl StartupFailure {
    /// Creates a retryable startup failure with explicit structured issues.
    #[must_use]
    pub fn retryable_with_issues(
        code: impl Into<String>,
        summary: impl Into<String>,
        issues: Vec<ServerIssue>,
    ) -> Self {
        Self {
            code: code.into(),
            summary: summary.into(),
            issues,
            retryable: true,
        }
    }

    /// Creates a retryable startup failure with one error issue.
    #[must_use]
    pub fn retryable(
        code: impl Into<String>,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let code = code.into();
        let summary = summary.into();
        Self {
            issues: vec![server_issue(
                code.clone(),
                ServerIssueSeverity::Error,
                summary.clone(),
                detail,
                None,
            )],
            code,
            summary,
            retryable: true,
        }
    }
}

/// Constructs one typed server issue for startup reports.
#[must_use]
pub fn server_issue(
    code: impl Into<String>,
    severity: ServerIssueSeverity,
    title: impl Into<String>,
    detail: impl Into<String>,
    remediation: Option<String>,
) -> ServerIssue {
    ServerIssue {
        code: code.into().into(),
        severity,
        title: title.into().into(),
        detail: detail.into().into(),
        remediation: remediation.map(Into::into),
    }
}

fn starting_state(
    phase: ServerStartupPhase,
    detail: impl Into<String>,
    payload: Option<PayloadExtractionProgress>,
) -> ServerState {
    ServerState::Starting(ServerStarting {
        phase,
        detail: detail.into().into(),
        payload,
    })
}
