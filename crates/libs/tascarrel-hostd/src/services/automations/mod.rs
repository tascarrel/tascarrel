//! Durable host-owned execution of workspace Automation workflows.
//!
//! Definitions remain workspace configuration, while admitted executions are
//! immutable host-owned records. Human gates are represented as durable state
//! transitions rather than in-memory waiters.

mod catalog;
mod runner;
mod storage;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use tascarrel_api::ids::AutomationExecutionId;
use tascarrel_api::types::automations as api;
use tascarrel_api::types::config as config_api;
use tascarrel_api::types::protocol::Actor;
use tascarrel_api::types::workspaces::WorkspaceName;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::watch;

use self::storage::AutomationStorage;
use crate::HostControlService;
use crate::services::config::ConfigService;
use crate::services::config::ConfigServiceError;
use crate::services::config::ConfigSubscription;

/// Durable state and workspace configuration used by Automations.
#[derive(Clone, Debug)]
pub struct AutomationServiceConfig {
    /// Private host-owned execution state directory.
    pub state_directory: PathBuf,
}

impl AutomationServiceConfig {
    /// Creates Automation service configuration.
    #[must_use]
    pub fn new(state_directory: impl Into<PathBuf>) -> Self {
        Self {
            state_directory: state_directory.into(),
        }
    }
}

/// Durable Automation catalog and execution coordinator.
#[derive(Clone)]
pub struct AutomationService {
    inner: Arc<AutomationServiceInner>,
}

/// Caller-relevant Automation failure categories.
#[derive(Debug, Error)]
pub enum AutomationServiceError {
    /// A definition, identifier, or lifecycle transition is invalid.
    #[error("invalid Automation request: {0}")]
    InvalidRequest(String),
    /// The selected execution does not exist.
    #[error("Automation execution does not exist")]
    NotFound,
    /// Workspace configuration or durable state cannot currently be read.
    #[error("Automation service is unavailable: {0}")]
    Unavailable(String),
    /// An invariant or runner integration failed.
    #[error("Automation service failed: {0}")]
    Internal(String),
}

impl AutomationService {
    /// Opens durable execution state and reconciles non-resumable active work.
    ///
    /// # Errors
    ///
    /// Returns an error when the state path is unsafe or retained state cannot
    /// be decoded and reconciled.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(state_directory = %config.state_directory.display()),
        err
    )]
    pub fn open(config: AutomationServiceConfig) -> Result<Self, Report<AutomationServiceError>> {
        let storage = AutomationStorage::open(config.state_directory).map_err(storage_error)?;
        let mut executions = BTreeMap::new();
        let mut output_sequences = HashMap::new();
        for mut stored in storage.load().map_err(storage_error)? {
            reconcile_after_restart(&mut stored);
            storage.write_record(&stored).map_err(storage_error)?;
            let sequence = storage
                .read_output::<api::AutomationOutputLine>(stored.execution.id.0.as_ref())
                .map_err(storage_error)?
                .last()
                .map_or(0, |line| line.sequence);
            output_sequences.insert(stored.execution.id.clone(), sequence);
            executions.insert(stored.execution.id.clone(), stored);
        }
        let (generation, _) = watch::channel(0);
        let (output_events, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(AutomationServiceInner {
                storage,
                executions: Mutex::new(executions),
                generation,
                output_events,
                output_sequences: Mutex::new(output_sequences),
                running: Mutex::new(HashMap::new()),
                dispatch: tokio::sync::Notify::new(),
            }),
        })
    }

    /// Loads the current validated catalog for one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace name or catalog directory cannot be
    /// inspected. Invalid individual YAML files are returned in the catalog.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(workspace = %workspace.as_str()),
        err
    )]
    pub fn catalog(
        &self,
        config: &ConfigService,
        workspace: &WorkspaceName,
    ) -> Result<api::AutomationCatalog, Report<AutomationServiceError>> {
        let directory = config
            .workspace_directory(workspace)
            .map_err(config_error)?;
        catalog::load(&directory).map_err(|report| {
            report.escalate(AutomationServiceError::Unavailable(
                "could not inspect Automation definitions".to_owned(),
            ))
        })
    }

    /// Opens a catalog subscription driven by the recursive configuration
    /// watcher.
    ///
    /// # Errors
    ///
    /// Returns an error when initial configuration observation fails.
    pub async fn subscribe_catalog(
        &self,
        config: ConfigService,
        input: api::AutomationCatalogSubscription,
    ) -> Result<AutomationCatalogSubscription, Report<AutomationServiceError>> {
        let subscription = config
            .subscribe(config_api::ConfigChangedSubscription {
                workspace_name: input.workspace.clone(),
            })
            .await
            .map_err(config_error)?;
        Ok(AutomationCatalogSubscription {
            service: self.clone(),
            config,
            workspace: input.workspace,
            subscription,
            last: None,
        })
    }

    /// Admits a manual execution from the current validated definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the definition does not exist, has no manual
    /// trigger, or durable admission fails.
    #[tracing::instrument(
        level = "info",
        skip_all,
        fields(
            workspace = %input.workspace.as_str(),
            automation_id = %input.automation_id
        ),
        err
    )]
    pub async fn start(
        &self,
        config: &ConfigService,
        input: api::StartAutomationAction,
        actor: Actor,
    ) -> Result<api::StartAutomationOutput, Report<AutomationServiceError>> {
        let catalog = self.catalog(config, &input.workspace)?;
        let definition = catalog
            .automations
            .iter()
            .find(|definition| definition.id.as_ref() == input.automation_id.as_ref())
            .cloned()
            .ok_or_else(|| invalid_request("the Automation definition does not exist"))?;
        if !definition
            .triggers
            .iter()
            .any(|trigger| matches!(trigger, api::AutomationTrigger::Manual))
        {
            return Err(invalid_request(
                "the Automation does not enable workflow_dispatch",
            ));
        }
        self.admit(
            input.workspace,
            definition,
            api::AutomationExecutionTrigger::Manual,
            Some(actor),
        )
        .await
    }

    /// Requests cancellation and interrupts active runner work.
    ///
    /// # Errors
    ///
    /// Returns an error when the execution is missing or already terminal.
    #[tracing::instrument(
        level = "info",
        skip_all,
        fields(execution_id = %input.execution_id.0),
        err
    )]
    pub async fn cancel(
        &self,
        input: api::CancelAutomationExecutionAction,
    ) -> Result<api::CancelAutomationExecutionOutput, Report<AutomationServiceError>> {
        let mut executions = self.inner.executions.lock().await;
        let stored = executions
            .get_mut(&input.execution_id)
            .ok_or_else(|| AutomationServiceError::NotFound.report())?;
        if is_terminal(stored.execution.state) {
            return Err(invalid_request(
                "the Automation execution is already terminal",
            ));
        }
        stored.cancellation_requested = true;
        if matches!(
            stored.execution.state,
            api::AutomationExecutionState::Queued
                | api::AutomationExecutionState::WaitingForApproval
        ) {
            cancel_execution(&mut stored.execution);
        }
        self.inner
            .storage
            .write_record(stored)
            .map_err(storage_error)?;
        drop(executions);
        if let Some(cancel) = self.inner.running.lock().await.get(&input.execution_id) {
            cancel.send_replace(true);
        }
        self.publish_executions();
        self.inner.dispatch.notify_waiters();
        Ok(api::CancelAutomationExecutionOutput {})
    }

    /// Applies a decision to the current durable approval step.
    ///
    /// # Errors
    ///
    /// Returns an error when the execution is missing or is not waiting for an
    /// approval.
    #[tracing::instrument(
        level = "info",
        skip_all,
        fields(
            execution_id = %input.execution_id.0,
            decision = ?input.decision,
            actor = ?actor
        ),
        err
    )]
    pub async fn resolve_approval(
        &self,
        input: api::ResolveAutomationApprovalAction,
        actor: Actor,
    ) -> Result<api::ResolveAutomationApprovalOutput, Report<AutomationServiceError>> {
        let mut executions = self.inner.executions.lock().await;
        let stored = executions
            .get_mut(&input.execution_id)
            .ok_or_else(|| AutomationServiceError::NotFound.report())?;
        if stored.execution.state != api::AutomationExecutionState::WaitingForApproval {
            return Err(invalid_request(
                "the Automation execution is not waiting for approval",
            ));
        }
        let step = stored
            .execution
            .steps
            .iter_mut()
            .find(|step| step.state == api::AutomationStepState::WaitingForApproval)
            .ok_or_else(|| internal("approval execution has no waiting approval step"))?;
        let now = Timestamp::now();
        step.finished_at = Some(now);
        step.approval_resolution = Some(api::AutomationApprovalResolution {
            decision: input.decision,
            resolved_by: actor,
            resolved_at: now,
        });
        match input.decision {
            api::AutomationApprovalDecision::Approve => {
                step.state = api::AutomationStepState::Succeeded;
                stored.execution.state = api::AutomationExecutionState::Queued;
            }
            api::AutomationApprovalDecision::Reject => {
                step.state = api::AutomationStepState::Failed;
                step.error = Some("approval was rejected".into());
                stored.execution.state = api::AutomationExecutionState::Failed;
                stored.execution.finished_at = Some(now);
                stored.execution.error = Some("approval was rejected".into());
                skip_pending_steps(&mut stored.execution);
            }
        }
        self.inner
            .storage
            .write_record(stored)
            .map_err(storage_error)?;
        drop(executions);
        self.publish_executions();
        self.inner.dispatch.notify_waiters();
        Ok(api::ResolveAutomationApprovalOutput {})
    }

    /// Opens a filtered durable execution-list subscription.
    #[must_use]
    pub fn subscribe_executions(
        &self,
        input: api::AutomationExecutionListSubscription,
    ) -> AutomationExecutionSubscription {
        AutomationExecutionSubscription {
            service: self.clone(),
            workspace: input.workspace,
            cursor: input.cursor,
            generation: self.inner.generation.subscribe(),
            current_pending: true,
        }
    }

    /// Opens permanent output replay followed by live lines.
    ///
    /// # Errors
    ///
    /// Returns an error when the execution is missing or output cannot be read.
    pub async fn subscribe_output(
        &self,
        input: api::AutomationOutputSubscription,
    ) -> Result<AutomationOutputSubscription, Report<AutomationServiceError>> {
        self.require_execution(&input.execution_id).await?;
        // Open the live receiver before reading the replay so a line appended
        // during this admission window is either replayed or received live.
        let receiver = self.inner.output_events.subscribe();
        let lines = self.read_output(&input.execution_id)?;
        let after_sequence = input.after_sequence.unwrap_or(0);
        let replay_boundary = lines.last().map_or(0, |line| line.sequence);
        Ok(AutomationOutputSubscription {
            service: self.clone(),
            execution_id: input.execution_id,
            after_sequence,
            replay: lines
                .into_iter()
                .filter(|line| line.sequence > after_sequence)
                .collect(),
            replay_boundary,
            caught_up: false,
            receiver,
        })
    }

    /// Runs scheduling and queued executions until the future is dropped.
    pub async fn run(&self, host_control: HostControlService) {
        runner::run(self.clone(), host_control).await;
    }

    #[tracing::instrument(
        level = "info",
        skip_all,
        fields(
            workspace = %workspace.as_str(),
            automation_id = %definition.id,
            trigger = ?trigger
        ),
        err
    )]
    async fn admit(
        &self,
        workspace: WorkspaceName,
        definition: api::AutomationDefinition,
        trigger: api::AutomationExecutionTrigger,
        requested_by: Option<Actor>,
    ) -> Result<api::StartAutomationOutput, Report<AutomationServiceError>> {
        let id = AutomationExecutionId::generate();
        let execution = api::AutomationExecution {
            id: id.clone(),
            workspace,
            automation_id: definition.id.clone(),
            steps: definition
                .steps
                .iter()
                .cloned()
                .map(|definition| api::AutomationStepExecution {
                    definition,
                    state: api::AutomationStepState::Pending,
                    started_at: None,
                    finished_at: None,
                    process_id: None,
                    turn_id: None,
                    host_operation_id: None,
                    approval_resolution: None,
                    error: None,
                })
                .collect::<Vec<_>>()
                .into(),
            definition,
            trigger,
            requested_by,
            created_at: Timestamp::now(),
            started_at: None,
            finished_at: None,
            state: api::AutomationExecutionState::Queued,
            pod_id: None,
            chat_id: None,
            error: None,
        };
        let stored = StoredExecution {
            execution,
            cancellation_requested: false,
        };
        self.inner
            .storage
            .prepare_execution(id.0.as_ref())
            .map_err(storage_error)?;
        self.inner
            .storage
            .write_record(&stored)
            .map_err(storage_error)?;
        self.inner
            .output_sequences
            .lock()
            .await
            .insert(id.clone(), 0);
        self.inner
            .executions
            .lock()
            .await
            .insert(id.clone(), stored);
        self.publish_executions();
        self.inner.dispatch.notify_waiters();
        Ok(api::StartAutomationOutput { execution_id: id })
    }

    pub(crate) async fn require_execution(
        &self,
        id: &AutomationExecutionId,
    ) -> Result<StoredExecution, Report<AutomationServiceError>> {
        self.inner
            .executions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| AutomationServiceError::NotFound.report())
    }

    pub(crate) async fn update_execution(
        &self,
        id: &AutomationExecutionId,
        update: impl FnOnce(&mut StoredExecution) -> Result<(), Report<AutomationServiceError>>,
    ) -> Result<StoredExecution, Report<AutomationServiceError>> {
        let mut executions = self.inner.executions.lock().await;
        let stored = executions
            .get_mut(id)
            .ok_or_else(|| AutomationServiceError::NotFound.report())?;
        update(stored)?;
        self.inner
            .storage
            .write_record(stored)
            .map_err(storage_error)?;
        let result = stored.clone();
        drop(executions);
        self.publish_executions();
        Ok(result)
    }

    pub(crate) async fn append_output(
        &self,
        id: &AutomationExecutionId,
        step_id: Option<&str>,
        source: api::AutomationOutputSource,
        content: impl Into<String>,
    ) -> Result<api::AutomationOutputLine, Report<AutomationServiceError>> {
        let mut sequences = self.inner.output_sequences.lock().await;
        let sequence = sequences
            .get(id)
            .copied()
            .ok_or_else(|| internal("Automation output sequence is missing"))?
            .checked_add(1)
            .ok_or_else(|| internal("Automation output sequence overflowed"))?;
        let line = api::AutomationOutputLine {
            sequence,
            step_id: step_id.map(Into::into),
            source,
            content: sanitize_output_content(&content.into()).into(),
            observed_at: Timestamp::now(),
        };
        self.inner
            .storage
            .append_output(id.0.as_ref(), &line)
            .map_err(storage_error)?;
        sequences.insert(id.clone(), sequence);
        drop(sequences);
        if self.inner.output_events.receiver_count() > 0
            && let Err(error) = self.inner.output_events.send((id.clone(), line.clone()))
        {
            tracing::debug!(
                execution_id = %id.0,
                %error,
                "Automation output had no live subscriber"
            );
        }
        Ok(line)
    }

    fn read_output(
        &self,
        id: &AutomationExecutionId,
    ) -> Result<Vec<api::AutomationOutputLine>, Report<AutomationServiceError>> {
        self.inner
            .storage
            .read_output(id.0.as_ref())
            .map_err(storage_error)
    }

    fn publish_executions(&self) {
        let next = self.inner.generation.borrow().wrapping_add(1);
        self.inner.generation.send_replace(next);
    }
}

/// Latest-value catalog stream.
pub struct AutomationCatalogSubscription {
    service: AutomationService,
    config: ConfigService,
    workspace: WorkspaceName,
    subscription: ConfigSubscription,
    last: Option<api::AutomationCatalog>,
}

impl AutomationCatalogSubscription {
    /// Receives the initial catalog and later changed catalogs.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration observation or catalog loading fails.
    pub async fn recv(
        &mut self,
    ) -> Result<api::AutomationCatalogEvent, Report<AutomationServiceError>> {
        loop {
            if self.subscription.recv().await.is_none() {
                return Err(unavailable("configuration observation stopped"));
            }
            let value = self.service.catalog(&self.config, &self.workspace)?;
            if self.last.as_ref() == Some(&value) {
                continue;
            }
            self.last = Some(value.clone());
            return Ok(api::AutomationCatalogEvent { value });
        }
    }
}

/// Latest-value durable execution inventory.
pub struct AutomationExecutionSubscription {
    service: AutomationService,
    workspace: Option<WorkspaceName>,
    cursor: Option<api::AutomationRevision>,
    generation: watch::Receiver<u64>,
    current_pending: bool,
}

impl AutomationExecutionSubscription {
    /// Receives the current state and each later changed state.
    ///
    /// # Errors
    ///
    /// Returns an error if the service stops or a revision cannot be encoded.
    pub async fn recv(
        &mut self,
    ) -> Result<api::AutomationExecutionListEvent, Report<AutomationServiceError>> {
        loop {
            if self.current_pending {
                self.current_pending = false;
            } else {
                self.generation
                    .changed()
                    .await
                    .map_err(|_| unavailable("Automation service stopped"))?;
            }
            let executions = self.service.inner.executions.lock().await;
            let mut values = executions
                .values()
                .filter(|stored| {
                    self.workspace
                        .as_ref()
                        .is_none_or(|workspace| &stored.execution.workspace == workspace)
                })
                .map(|stored| stored.execution.clone())
                .collect::<Vec<_>>();
            values.sort_by(|left, right| {
                right
                    .created_at
                    .cmp(&left.created_at)
                    .then_with(|| right.id.cmp(&left.id))
            });
            let value = api::AutomationExecutionList {
                executions: values.into(),
            };
            let revision = execution_revision(&value)?;
            if self.cursor.as_ref() == Some(&revision) {
                continue;
            }
            self.cursor = Some(revision.clone());
            return Ok(api::AutomationExecutionListEvent { revision, value });
        }
    }
}

/// Permanent replay and live Automation output stream.
pub struct AutomationOutputSubscription {
    service: AutomationService,
    execution_id: AutomationExecutionId,
    after_sequence: u64,
    replay: VecDeque<api::AutomationOutputLine>,
    replay_boundary: u64,
    caught_up: bool,
    receiver: broadcast::Receiver<(AutomationExecutionId, api::AutomationOutputLine)>,
}

impl AutomationOutputSubscription {
    /// Receives the next retained output line or replay boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when retained output cannot be read or the service
    /// stops.
    pub async fn recv(
        &mut self,
    ) -> Result<api::AutomationOutputEvent, Report<AutomationServiceError>> {
        loop {
            if let Some(line) = self.replay.pop_front() {
                self.after_sequence = line.sequence;
                return Ok(api::AutomationOutputEvent {
                    update: api::AutomationOutputUpdate::Line(line),
                });
            }
            if !self.caught_up {
                self.caught_up = true;
                return Ok(api::AutomationOutputEvent {
                    update: api::AutomationOutputUpdate::CaughtUp(api::AutomationOutputCaughtUp {
                        last_sequence: self.replay_boundary,
                    }),
                });
            }
            match self.receiver.recv().await {
                Ok((id, line))
                    if id == self.execution_id && line.sequence > self.after_sequence =>
                {
                    self.after_sequence = line.sequence;
                    return Ok(api::AutomationOutputEvent {
                        update: api::AutomationOutputUpdate::Line(line),
                    });
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.replay = self
                        .service
                        .read_output(&self.execution_id)?
                        .into_iter()
                        .filter(|line| line.sequence > self.after_sequence)
                        .collect();
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(unavailable("Automation output stream closed"));
                }
            }
        }
    }
}

const MAX_AUTOMATION_OUTPUT_CONTENT_BYTES: usize = 64 * 1024;

/// Additional durable runner state which is intentionally absent from the
/// caller-visible execution contract.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StoredExecution {
    /// Caller-visible execution state.
    pub(crate) execution: api::AutomationExecution,
    /// Durable cancellation request observed by active runner work.
    pub(crate) cancellation_requested: bool,
}

struct AutomationServiceInner {
    storage: AutomationStorage,
    executions: Mutex<BTreeMap<AutomationExecutionId, StoredExecution>>,
    generation: watch::Sender<u64>,
    output_events: broadcast::Sender<(AutomationExecutionId, api::AutomationOutputLine)>,
    output_sequences: Mutex<HashMap<AutomationExecutionId, u64>>,
    running: Mutex<HashMap<AutomationExecutionId, watch::Sender<bool>>>,
    dispatch: tokio::sync::Notify,
}

fn reconcile_after_restart(stored: &mut StoredExecution) {
    if matches!(
        stored.execution.state,
        api::AutomationExecutionState::Running | api::AutomationExecutionState::WaitingForInput
    ) {
        let now = Timestamp::now();
        stored.execution.state = api::AutomationExecutionState::Interrupted;
        stored.execution.finished_at = Some(now);
        stored.execution.error =
            Some("hostd restarted while a non-reconcilable step was active".into());
        for step in &mut *stored.execution.steps {
            if matches!(
                step.state,
                api::AutomationStepState::Running | api::AutomationStepState::WaitingForInput
            ) {
                step.state = api::AutomationStepState::Failed;
                step.finished_at = Some(now);
                step.error = Some("hostd restarted while this step was active".into());
            } else if step.state == api::AutomationStepState::Pending {
                step.state = api::AutomationStepState::Skipped;
                step.finished_at = Some(now);
            }
        }
    }
}

fn cancel_execution(execution: &mut api::AutomationExecution) {
    let now = Timestamp::now();
    execution.state = api::AutomationExecutionState::Canceled;
    execution.finished_at = Some(now);
    execution.error = Some("execution was canceled".into());
    for step in &mut *execution.steps {
        if matches!(
            step.state,
            api::AutomationStepState::Running
                | api::AutomationStepState::WaitingForApproval
                | api::AutomationStepState::WaitingForInput
        ) {
            step.state = api::AutomationStepState::Canceled;
            step.finished_at = Some(now);
        } else if step.state == api::AutomationStepState::Pending {
            step.state = api::AutomationStepState::Skipped;
            step.finished_at = Some(now);
        }
    }
}

/// Marks every step not yet started as skipped.
pub(crate) fn skip_pending_steps(execution: &mut api::AutomationExecution) {
    let now = Timestamp::now();
    for step in &mut *execution.steps {
        if step.state == api::AutomationStepState::Pending {
            step.state = api::AutomationStepState::Skipped;
            step.finished_at = Some(now);
        }
    }
}

/// Returns whether an execution state permits no further transitions.
pub(crate) const fn is_terminal(state: api::AutomationExecutionState) -> bool {
    matches!(
        state,
        api::AutomationExecutionState::Succeeded
            | api::AutomationExecutionState::Failed
            | api::AutomationExecutionState::Canceled
            | api::AutomationExecutionState::Interrupted
    )
}

fn execution_revision(
    value: &api::AutomationExecutionList,
) -> Result<api::AutomationRevision, Report<AutomationServiceError>> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| internal(format!("could not encode execution list: {error}")))?;
    Ok(api::AutomationRevision::new(format!(
        "{:x}",
        Sha256::digest(encoded)
    )))
}

fn sanitize_output_content(content: &str) -> String {
    let mut sanitized =
        String::with_capacity(content.len().min(MAX_AUTOMATION_OUTPUT_CONTENT_BYTES));
    for character in content.chars() {
        let character = if character.is_control() {
            char::REPLACEMENT_CHARACTER
        } else {
            character
        };
        if sanitized.len() + character.len_utf8() > MAX_AUTOMATION_OUTPUT_CONTENT_BYTES {
            break;
        }
        sanitized.push(character);
    }
    sanitized
}

fn storage_error(report: Report<storage::StorageError>) -> Report<AutomationServiceError> {
    report.escalate(AutomationServiceError::Unavailable(
        "durable Automation storage failed".to_owned(),
    ))
}

fn config_error(report: Report<ConfigServiceError>) -> Report<AutomationServiceError> {
    report.escalate(AutomationServiceError::Unavailable(
        "workspace configuration observation failed".to_owned(),
    ))
}

/// Creates an invalid Automation request report.
pub(crate) fn invalid_request(message: impl Into<String>) -> Report<AutomationServiceError> {
    AutomationServiceError::InvalidRequest(message.into()).report()
}

/// Creates an unavailable Automation service report.
pub(crate) fn unavailable(message: impl Into<String>) -> Report<AutomationServiceError> {
    AutomationServiceError::Unavailable(message.into()).report()
}

/// Creates an internal Automation service report.
pub(crate) fn internal(message: impl Into<String>) -> Report<AutomationServiceError> {
    AutomationServiceError::Internal(message.into()).report()
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use tascarrel_api::types::automations as api;
    use tascarrel_api::types::protocol::Actor;
    use tascarrel_api::types::workspaces::WorkspaceName;

    use super::AutomationService;
    use super::AutomationServiceConfig;
    use super::sanitize_output_content;

    /// Output subscribers receive a finite replay boundary and later live lines
    /// once.
    #[tokio::test]
    async fn output_subscription_replays_then_follows() {
        let directory = tempfile::tempdir().unwrap();
        let service = AutomationService::open(AutomationServiceConfig::new(
            directory.path().join("automations"),
        ))
        .unwrap();
        let execution_id = admit(&service, approval_definition("output")).await;
        service
            .append_output(
                &execution_id,
                None,
                api::AutomationOutputSource::Automation,
                "first",
            )
            .await
            .unwrap();
        let mut output = service
            .subscribe_output(api::AutomationOutputSubscription {
                execution_id: execution_id.clone(),
                after_sequence: None,
            })
            .await
            .unwrap();
        service
            .append_output(
                &execution_id,
                None,
                api::AutomationOutputSource::Automation,
                "second",
            )
            .await
            .unwrap();

        let first = output.recv().await.unwrap().update;
        assert!(matches!(
            first,
            api::AutomationOutputUpdate::Line(ref line)
                if line.sequence == 1 && line.content.as_ref() == "first"
        ));
        let boundary = output.recv().await.unwrap().update;
        assert!(matches!(
            boundary,
            api::AutomationOutputUpdate::CaughtUp(ref value)
                if value.last_sequence == 1
        ));
        let second = output.recv().await.unwrap().update;
        assert!(matches!(
            second,
            api::AutomationOutputUpdate::Line(ref line)
                if line.sequence == 2 && line.content.as_ref() == "second"
        ));
    }

    /// Approval decisions retain the authenticated actor and survive restart.
    #[tokio::test]
    async fn approval_resolution_is_auditable_and_durable() {
        let directory = tempfile::tempdir().unwrap();
        let state_directory = directory.path().join("automations");
        let service =
            AutomationService::open(AutomationServiceConfig::new(&state_directory)).unwrap();
        let execution_id = admit(&service, approval_definition("approval-audit")).await;
        service
            .update_execution(&execution_id, |stored| {
                stored.execution.state = api::AutomationExecutionState::WaitingForApproval;
                stored.execution.steps[0].state = api::AutomationStepState::WaitingForApproval;
                Ok(())
            })
            .await
            .unwrap();
        service
            .resolve_approval(
                api::ResolveAutomationApprovalAction {
                    execution_id: execution_id.clone(),
                    decision: api::AutomationApprovalDecision::Approve,
                },
                Actor::Host,
            )
            .await
            .unwrap();
        drop(service);

        let reopened =
            AutomationService::open(AutomationServiceConfig::new(state_directory)).unwrap();
        let stored = reopened.require_execution(&execution_id).await.unwrap();
        assert_eq!(
            stored.execution.state,
            api::AutomationExecutionState::Queued
        );
        assert!(matches!(
            &stored.execution.steps[0].approval_resolution,
            Some(resolution)
                if resolution.decision == api::AutomationApprovalDecision::Approve
                    && resolution.resolved_by == Actor::Host
        ));
    }

    /// Restart reconciliation preserves durable gates and interrupts active
    /// effects.
    #[tokio::test]
    async fn restart_preserves_approval_and_interrupts_running_step() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("automations");
        let service = AutomationService::open(AutomationServiceConfig::new(&state)).unwrap();
        let approval = admit(&service, approval_definition("approval")).await;
        service
            .update_execution(&approval, |stored| {
                stored.execution.started_at = Some(Timestamp::now());
                stored.execution.state = api::AutomationExecutionState::WaitingForApproval;
                stored.execution.steps[0].state = api::AutomationStepState::WaitingForApproval;
                Ok(())
            })
            .await
            .unwrap();
        let running = admit(&service, approval_definition("running")).await;
        service
            .update_execution(&running, |stored| {
                stored.execution.started_at = Some(Timestamp::now());
                stored.execution.state = api::AutomationExecutionState::Running;
                stored.execution.steps[0].state = api::AutomationStepState::Running;
                Ok(())
            })
            .await
            .unwrap();
        drop(service);

        let reopened = AutomationService::open(AutomationServiceConfig::new(&state)).unwrap();
        let approval = reopened.require_execution(&approval).await.unwrap();
        assert_eq!(
            approval.execution.state,
            api::AutomationExecutionState::WaitingForApproval
        );
        assert_eq!(
            approval.execution.steps[0].state,
            api::AutomationStepState::WaitingForApproval
        );
        let running = reopened.require_execution(&running).await.unwrap();
        assert_eq!(
            running.execution.state,
            api::AutomationExecutionState::Interrupted
        );
        assert_eq!(
            running.execution.steps[0].state,
            api::AutomationStepState::Failed
        );
    }

    /// Host-owned output cannot inject terminal controls or split a retained
    /// line.
    #[test]
    fn host_output_is_sanitized_as_one_bounded_line() {
        assert_eq!(
            sanitize_output_content("approve?\n\u{1b}[31m"),
            "approve?\u{fffd}\u{fffd}[31m"
        );
    }

    async fn admit(
        service: &AutomationService,
        definition: api::AutomationDefinition,
    ) -> tascarrel_api::ids::AutomationExecutionId {
        service
            .admit(
                WorkspaceName::new("test"),
                definition,
                api::AutomationExecutionTrigger::Manual,
                Some(Actor::Host),
            )
            .await
            .unwrap()
            .execution_id
    }

    fn approval_definition(id: &str) -> api::AutomationDefinition {
        api::AutomationDefinition {
            id: id.into(),
            name: id.into(),
            description: None,
            triggers: vec![api::AutomationTrigger::Manual].into(),
            agent_defaults: None,
            max_concurrent: 1,
            timeout_seconds: Some(3_600),
            steps: vec![api::AutomationStepDefinition {
                id: "approval".into(),
                name: "Approval".into(),
                continue_on_error: false,
                kind: api::AutomationStepKind::Approval(api::AutomationApprovalStep {
                    prompt: "Continue?".into(),
                }),
            }]
            .into(),
        }
    }
}
