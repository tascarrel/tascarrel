//! Scheduling and sequential execution through existing guest APIs.

use std::collections::HashMap;
use std::str::FromStr as _;
use std::time::Duration;

use chrono::SubsecRound as _;
use chrono::Timelike as _;
use chrono::Utc;
use croner::Cron;
use jiff::Timestamp;
use tascarrel_api::ids::AutomationExecutionId;
use tascarrel_api::ids::TraceId;
use tascarrel_api::types::automations as api;
use tascarrel_api::types::chats as chat_api;
use tascarrel_api::types::host_operations as host_operation_api;
use tascarrel_api::types::pods as pod_api;
use tascarrel_api::types::processes as process_api;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::workspaces::WorkspaceName;
use tascarrel_protocol::AUTOMATION_HOST_OPERATION_MARKER_PREFIX;
use tascarrel_protocol::WorkspaceName as ValidatedWorkspaceName;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio::time::MissedTickBehavior;

use super::AutomationService;
use super::AutomationServiceError;
use super::internal;
use super::invalid_request;
use super::is_terminal;
use super::skip_pending_steps;
use super::unavailable;
use crate::GuestClient;
use crate::HostControlService;
use crate::services::host_operations::HostOperationService;

/// Schedules and dispatches admitted executions until the future is dropped.
pub(crate) async fn run(service: AutomationService, host_control: HostControlService) {
    let mut tasks = JoinSet::new();
    let mut ticker = tokio::time::interval(DISPATCH_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = service.inner.dispatch.notified() => {}
            _ = ticker.tick() => {}
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "Automation execution task failed");
                }
            }
        }
        if let Err(error) = admit_schedules(&service, &host_control).await {
            tracing::warn!(%error, "Automation scheduling pass failed");
        }
        if let Err(error) = expire_waiting_executions(&service).await {
            tracing::warn!(%error, "Automation timeout reconciliation failed");
        }
        match claim_queued(&service).await {
            Ok(claimed) => {
                for (id, cancel) in claimed {
                    let service = service.clone();
                    let host_control = host_control.clone();
                    tasks.spawn(async move {
                        let result =
                            run_execution(service.clone(), host_control, id.clone(), cancel).await;
                        service.inner.running.lock().await.remove(&id);
                        service.inner.dispatch.notify_waiters();
                        if let Err(error) = result {
                            tracing::warn!(execution_id = %id.0, %error, "Automation execution failed");
                            let message = error.to_string();
                            if let Err(failure) = fail_execution(&service, &id, message).await {
                                tracing::warn!(
                                    execution_id = %id.0,
                                    %failure,
                                    "failed to retain Automation execution failure"
                                );
                            }
                        }
                    });
                }
            }
            Err(error) => tracing::warn!(%error, "could not dispatch queued Automations"),
        }
    }
}

const DISPATCH_INTERVAL: Duration = Duration::from_secs(15);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PROCESS_TERMINATION_GRACE: Duration = Duration::from_secs(3);
const LOG_DRAIN_QUIET_PERIOD: Duration = Duration::from_millis(100);
// The Automation command contract requires Bash at the standard image path.
const BASH_PROGRAM: &str = "/bin/bash";
// A successful step is a workflow durability boundary. The guest VM may stop
// without unmounting its filesystems, so flush pod writes before publishing
// success to host-owned state.
const SYNC_PROGRAM: &str = "/bin/sync";
// The pod root is durable while `/tmp` is intentionally recreated when the
// pod runtime restarts. Keeping step output relative to the execution user's
// home lets later agent steps inspect earlier output after recovery without
// assuming an image-specific account name.
const AUTOMATION_OUTPUT_RELATIVE_ROOT: &str = ".local/state/tascarrel/automations";
// Pod runtime mounts this helper immutably; using its absolute path prevents
// workspace-controlled PATH entries from replacing the host-command bridge.
const PODCTL_PROGRAM: &str = "/usr/local/bin/podctl";

async fn admit_schedules(
    service: &AutomationService,
    host_control: &HostControlService,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let config = host_control.state().config();
    let root = config.workspaces_directory().to_owned();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(unavailable(format!(
                "could not enumerate workspace configurations: {error}"
            )));
        }
    };
    let now = Utc::now().trunc_subsecs(0);
    let minute = now
        .with_second(0)
        .and_then(|value| value.with_nanosecond(0))
        .ok_or_else(|| internal("failed to normalize Automation scheduler time"))?;
    let scheduled_for = Timestamp::from_str(&minute.to_rfc3339())
        .map_err(|error| internal(format!("could not convert scheduler time: {error}")))?;

    for entry in entries {
        let entry = entry
            .map_err(|error| unavailable(format!("could not inspect a workspace: {error}")))?;
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
            unavailable(format!("could not inspect a workspace directory: {error}"))
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(validated) = ValidatedWorkspaceName::new(name) else {
            continue;
        };
        let workspace = WorkspaceName::new(validated.as_str());
        let catalog = match service.catalog(config, &workspace) {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::warn!(workspace = %workspace.as_str(), %error, "could not load scheduled Automations");
                continue;
            }
        };
        for definition in &*catalog.automations {
            for trigger in &*definition.triggers {
                let api::AutomationTrigger::Schedule(schedule) = trigger else {
                    continue;
                };
                let cron = Cron::from_str(schedule.cron.as_ref())
                    .map_err(|error| internal(format!("validated cron became invalid: {error}")))?;
                if !cron
                    .is_time_matching(&minute)
                    .map_err(|error| internal(format!("cron matching failed: {error}")))?
                {
                    continue;
                }
                let duplicate = service
                    .inner
                    .executions
                    .lock()
                    .await
                    .values()
                    .any(|stored| {
                        stored.execution.workspace == workspace
                            && stored.execution.automation_id == definition.id
                            && matches!(
                                &stored.execution.trigger,
                                api::AutomationExecutionTrigger::Schedule(existing)
                                    if existing.scheduled_for == scheduled_for
                                        && existing.cron == schedule.cron
                            )
                    });
                if duplicate {
                    continue;
                }
                service
                    .admit(
                        workspace.clone(),
                        definition.clone(),
                        api::AutomationExecutionTrigger::Schedule(
                            api::AutomationScheduledExecution {
                                cron: schedule.cron.clone(),
                                scheduled_for,
                            },
                        ),
                        None,
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

async fn expire_waiting_executions(
    service: &AutomationService,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let ids = service
        .inner
        .executions
        .lock()
        .await
        .values()
        .filter(|stored| {
            matches!(
                stored.execution.state,
                api::AutomationExecutionState::WaitingForApproval
                    | api::AutomationExecutionState::WaitingForInput
                    | api::AutomationExecutionState::Queued
            ) && execution_timed_out(&stored.execution)
        })
        .map(|stored| stored.execution.id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        fail_execution(service, &id, "Automation execution timed out".to_owned()).await?;
    }
    Ok(())
}

async fn claim_queued(
    service: &AutomationService,
) -> Result<
    Vec<(AutomationExecutionId, watch::Receiver<bool>)>,
    reportify::Report<AutomationServiceError>,
> {
    let mut executions = service.inner.executions.lock().await;
    let mut candidates = executions
        .values()
        .filter(|stored| stored.execution.state == api::AutomationExecutionState::Queued)
        .map(|stored| {
            (
                stored.execution.created_at,
                stored.execution.id.clone(),
                stored.execution.workspace.clone(),
                stored.execution.automation_id.clone(),
                stored.execution.definition.max_concurrent,
            )
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut active = executions
        .values()
        .filter(|stored| {
            matches!(
                stored.execution.state,
                api::AutomationExecutionState::Running
                    | api::AutomationExecutionState::WaitingForApproval
                    | api::AutomationExecutionState::WaitingForInput
            )
        })
        .fold(
            HashMap::<(WorkspaceName, String), u32>::new(),
            |mut counts, stored| {
                *counts
                    .entry((
                        stored.execution.workspace.clone(),
                        stored.execution.automation_id.to_string(),
                    ))
                    .or_default() += 1;
                counts
            },
        );
    let running = service.inner.running.lock().await;
    let mut running = running;
    let mut claimed = Vec::new();
    for (_, id, workspace, automation_id, limit) in candidates {
        if running.contains_key(&id) {
            continue;
        }
        let key = (workspace, automation_id.to_string());
        let count = active.entry(key).or_default();
        if *count >= limit {
            continue;
        }
        let stored = executions
            .get_mut(&id)
            .ok_or_else(|| internal("queued Automation disappeared during dispatch"))?;
        if stored.cancellation_requested {
            continue;
        }
        stored.execution.state = api::AutomationExecutionState::Running;
        stored
            .execution
            .started_at
            .get_or_insert_with(Timestamp::now);
        service
            .inner
            .storage
            .write_record(stored)
            .map_err(super::storage_error)?;
        *count += 1;
        let (cancel, receiver) = watch::channel(false);
        running.insert(id.clone(), cancel);
        claimed.push((id.clone(), receiver));
    }
    drop(running);
    drop(executions);
    if !claimed.is_empty() {
        service.publish_executions();
    }
    Ok(claimed)
}

#[tracing::instrument(
    level = "info",
    skip_all,
    fields(execution_id = %id.0),
    err
)]
async fn run_execution(
    service: AutomationService,
    host_control: HostControlService,
    id: AutomationExecutionId,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let stored = service.require_execution(&id).await?;
    let workspace = validated_workspace(&stored.execution.workspace)?;
    let guest = host_control
        .state()
        .workspaces()
        .guestd(host_control.clone());
    let host_operations = host_control.state().host_operations().clone();
    let trace_id = TraceId::generate();
    let deadline = execution_deadline(&stored.execution);
    service
        .append_output(
            &id,
            None,
            api::AutomationOutputSource::Automation,
            "execution started",
        )
        .await?;
    let pod_id =
        ensure_execution_pod(&service, &guest, &workspace, &trace_id, &id, &stored).await?;
    RunningExecution {
        service: &service,
        guest: &guest,
        host_operations: &host_operations,
        workspace,
        trace_id: &trace_id,
        id: &id,
        pod_id: &pod_id,
        deadline,
    }
    .run(&mut cancel)
    .await
}

struct RunningExecution<'a> {
    service: &'a AutomationService,
    guest: &'a GuestClient,
    host_operations: &'a HostOperationService,
    workspace: ValidatedWorkspaceName,
    trace_id: &'a TraceId,
    id: &'a AutomationExecutionId,
    pod_id: &'a pod_api::PodId,
    deadline: Option<Instant>,
}

enum StepRun {
    Paused,
    Completed(Result<(), reportify::Report<AutomationServiceError>>),
}

impl RunningExecution<'_> {
    async fn run(
        &self,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(), reportify::Report<AutomationServiceError>> {
        loop {
            if *cancel.borrow() {
                cancel_running_execution(self.service, self.id).await?;
                return Ok(());
            }
            if self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                fail_execution(
                    self.service,
                    self.id,
                    "Automation execution timed out".to_owned(),
                )
                .await?;
                return Ok(());
            }
            let stored = self.service.require_execution(self.id).await?;
            if is_terminal(stored.execution.state) {
                return Ok(());
            }
            let Some(index) = stored
                .execution
                .steps
                .iter()
                .position(|step| step.state == api::AutomationStepState::Pending)
            else {
                self.succeed_execution().await?;
                return Ok(());
            };
            let step = stored.execution.steps[index].definition.clone();
            match self.execute_step(index, &step, cancel).await? {
                StepRun::Paused => return Ok(()),
                StepRun::Completed(outcome) => {
                    if !self.finish_step(index, &step, outcome, cancel).await? {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn execute_step(
        &self,
        index: usize,
        step: &api::AutomationStepDefinition,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<StepRun, reportify::Report<AutomationServiceError>> {
        self.service
            .update_execution(self.id, |stored| {
                stored.execution.state = api::AutomationExecutionState::Running;
                let state = &mut stored.execution.steps[index];
                state.state = api::AutomationStepState::Running;
                state.started_at = Some(Timestamp::now());
                Ok(())
            })
            .await?;
        self.service
            .append_output(
                self.id,
                Some(step.id.as_ref()),
                api::AutomationOutputSource::Automation,
                format!("step started: {}", step.name),
            )
            .await?;

        let outcome = match &step.kind {
            api::AutomationStepKind::Command(command) => {
                run_command_step(
                    self.service,
                    self.guest,
                    self.workspace.clone(),
                    self.trace_id,
                    self.id,
                    self.pod_id,
                    step,
                    command,
                    cancel,
                    self.deadline,
                )
                .await
            }
            api::AutomationStepKind::HostCommand(command) => {
                run_host_command_step(
                    self.service,
                    self.guest,
                    self.workspace.clone(),
                    self.trace_id,
                    self.id,
                    self.pod_id,
                    step,
                    command,
                    self.host_operations,
                    cancel,
                    self.deadline,
                )
                .await
            }
            api::AutomationStepKind::Agent(agent) => {
                run_agent_step(
                    self.service,
                    self.guest,
                    self.workspace.clone(),
                    self.trace_id,
                    self.id,
                    self.pod_id,
                    index,
                    agent,
                    cancel,
                    self.deadline,
                )
                .await
            }
            api::AutomationStepKind::Approval(approval) => {
                self.wait_for_approval(index, step, approval).await?;
                return Ok(StepRun::Paused);
            }
        };
        let outcome = match outcome {
            Ok(()) => {
                sync_pod(
                    self.guest,
                    self.workspace.clone(),
                    self.trace_id,
                    self.id,
                    self.pod_id,
                    cancel,
                    self.deadline,
                )
                .await
            }
            Err(error) => Err(error),
        };
        Ok(StepRun::Completed(outcome))
    }

    async fn finish_step(
        &self,
        index: usize,
        step: &api::AutomationStepDefinition,
        outcome: Result<(), reportify::Report<AutomationServiceError>>,
        cancel: &watch::Receiver<bool>,
    ) -> Result<bool, reportify::Report<AutomationServiceError>> {
        match outcome {
            Ok(()) => {
                self.service
                    .update_execution(self.id, |stored| {
                        let state = &mut stored.execution.steps[index];
                        state.state = api::AutomationStepState::Succeeded;
                        state.finished_at = Some(Timestamp::now());
                        Ok(())
                    })
                    .await?;
                Ok(true)
            }
            Err(_) if *cancel.borrow() => {
                cancel_running_execution(self.service, self.id).await?;
                Ok(false)
            }
            Err(error) => {
                let message = error.to_string();
                self.fail_step(index, step, &message).await?;
                Ok(step.continue_on_error)
            }
        }
    }

    async fn fail_step(
        &self,
        index: usize,
        step: &api::AutomationStepDefinition,
        message: &str,
    ) -> Result<(), reportify::Report<AutomationServiceError>> {
        self.service
            .update_execution(self.id, |stored| {
                let state = &mut stored.execution.steps[index];
                state.state = api::AutomationStepState::Failed;
                state.finished_at = Some(Timestamp::now());
                state.error = Some(message.into());
                if step.continue_on_error {
                    stored.execution.state = api::AutomationExecutionState::Running;
                } else {
                    stored.execution.state = api::AutomationExecutionState::Failed;
                    stored.execution.finished_at = Some(Timestamp::now());
                    stored.execution.error = Some(message.into());
                    skip_pending_steps(&mut stored.execution);
                }
                Ok(())
            })
            .await?;
        self.service
            .append_output(
                self.id,
                Some(step.id.as_ref()),
                api::AutomationOutputSource::Automation,
                format!("step failed: {message}"),
            )
            .await?;
        Ok(())
    }

    async fn wait_for_approval(
        &self,
        index: usize,
        step: &api::AutomationStepDefinition,
        approval: &api::AutomationApprovalStep,
    ) -> Result<(), reportify::Report<AutomationServiceError>> {
        self.service
            .update_execution(self.id, |stored| {
                stored.execution.state = api::AutomationExecutionState::WaitingForApproval;
                stored.execution.steps[index].state = api::AutomationStepState::WaitingForApproval;
                Ok(())
            })
            .await?;
        self.service
            .append_output(
                self.id,
                Some(step.id.as_ref()),
                api::AutomationOutputSource::Automation,
                format!("waiting for approval: {}", approval.prompt),
            )
            .await?;
        Ok(())
    }

    async fn succeed_execution(&self) -> Result<(), reportify::Report<AutomationServiceError>> {
        self.service
            .update_execution(self.id, |stored| {
                stored.execution.state = api::AutomationExecutionState::Succeeded;
                stored.execution.finished_at = Some(Timestamp::now());
                Ok(())
            })
            .await?;
        self.service
            .append_output(
                self.id,
                None,
                api::AutomationOutputSource::Automation,
                "execution succeeded",
            )
            .await?;
        Ok(())
    }
}

async fn ensure_execution_pod(
    service: &AutomationService,
    guest: &GuestClient,
    workspace: &ValidatedWorkspaceName,
    trace_id: &TraceId,
    id: &AutomationExecutionId,
    stored: &super::StoredExecution,
) -> Result<pod_api::PodId, reportify::Report<AutomationServiceError>> {
    if let Some(pod_id) = &stored.execution.pod_id {
        return Ok(pod_id.clone());
    }
    let output = guest
        .execute(
            workspace.clone(),
            request_context(trace_id, id),
            pod_api::CreatePodAction {
                title: Some(format!("Automation: {}", stored.execution.definition.name).into()),
            },
        )
        .await
        .map_err(|error| unavailable(format!("could not create Automation pod: {error}")))?;
    service
        .update_execution(id, |stored| {
            stored.execution.pod_id = Some(output.pod_id.clone());
            Ok(())
        })
        .await?;
    Ok(output.pod_id)
}

#[allow(clippy::too_many_arguments)]
async fn run_command_step(
    service: &AutomationService,
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    pod_id: &pod_api::PodId,
    step: &api::AutomationStepDefinition,
    command: &api::AutomationCommandStep,
    cancel: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let mut environment = command.environment.clone();
    environment.insert("TASCARREL_AUTOMATION_COMMAND".into(), command.run.clone());
    environment.insert(
        "TASCARREL_AUTOMATION_OUTPUT_DIRECTORY".into(),
        execution_output_relative_directory(execution_id).into(),
    );
    environment.insert(
        "TASCARREL_AUTOMATION_OUTPUT_FILE".into(),
        step_output_file(step.id.as_ref()).into(),
    );
    let action = process_api::SpawnProcessAction {
        pod_id: pod_id.clone(),
        start_pod: Some(true),
        title: step.name.clone(),
        executable: BASH_PROGRAM.into(),
        arguments: vec![
            "-o".into(),
            "pipefail".into(),
            "-c".into(),
            "output_directory=\"${HOME:?HOME is not set}/$TASCARREL_AUTOMATION_OUTPUT_DIRECTORY\" && mkdir -p -- \"$output_directory\" && bash -lc \"$TASCARREL_AUTOMATION_COMMAND\" 2>&1 | tee -- \"$output_directory/$TASCARREL_AUTOMATION_OUTPUT_FILE\"".into(),
        ]
        .into(),
        environment,
        working_directory: command.working_directory.clone(),
        terminal: None,
        log_stdout: Some(true),
        profile: process_api::ProcessExecutionProfile::User,
    };
    run_process(
        service,
        guest,
        workspace,
        trace_id,
        execution_id,
        step,
        action,
        None,
        None,
        cancel,
        deadline,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn run_host_command_step(
    service: &AutomationService,
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    pod_id: &pod_api::PodId,
    step: &api::AutomationStepDefinition,
    command: &api::AutomationHostCommandStep,
    host_operations: &HostOperationService,
    cancel: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut arguments = vec![
        "host".into(),
        "operations".into(),
        "run".into(),
        command.command.clone(),
        "--automation-report-id".into(),
        nonce.clone().into(),
    ];
    let mut parameters = command.parameters.iter().collect::<Vec<_>>();
    parameters.sort_by(|left, right| left.0.cmp(right.0));
    for (name, value) in parameters {
        arguments.push("--parameter".into());
        arguments.push(format!("{name}={value}").into());
    }
    let mut wrapped = vec![
        "-o".into(),
        "pipefail".into(),
        "-c".into(),
        "output_directory=\"${HOME:?HOME is not set}/$1\" && output_file=\"$2\" && shift 2 && mkdir -p -- \"$output_directory\" && \"$@\" 2>&1 | tee -- \"$output_directory/$output_file\"".into(),
        "tascarrel-host-command".into(),
        execution_output_relative_directory(execution_id).into(),
        step_output_file(step.id.as_ref()).into(),
        PODCTL_PROGRAM.into(),
    ];
    wrapped.extend(arguments);
    let action = process_api::SpawnProcessAction {
        pod_id: pod_id.clone(),
        start_pod: Some(true),
        title: step.name.clone(),
        executable: BASH_PROGRAM.into(),
        arguments: wrapped.into(),
        environment: HashMap::new(),
        working_directory: None,
        terminal: None,
        log_stdout: Some(true),
        profile: process_api::ProcessExecutionProfile::User,
    };
    let host_operation_id = run_process(
        service,
        guest,
        workspace,
        trace_id,
        execution_id,
        step,
        action,
        Some(&nonce),
        Some(host_operations),
        cancel,
        deadline,
    )
    .await?;
    if host_operation_id.is_none() {
        return Err(internal(
            "podctl completed without reporting its host operation identifier",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_process(
    service: &AutomationService,
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    step: &api::AutomationStepDefinition,
    action: process_api::SpawnProcessAction,
    host_marker_nonce: Option<&str>,
    host_operations: Option<&HostOperationService>,
    cancel: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<Option<host_operation_api::HostOperationId>, reportify::Report<AutomationServiceError>>
{
    let pod_id = action.pod_id.clone();
    let output = guest
        .spawn(
            workspace.clone(),
            request_context(trace_id, execution_id),
            action,
        )
        .await
        .map_err(|error| unavailable(format!("could not start step process: {error}")))?;
    service
        .update_execution(execution_id, |stored| {
            let state = stored
                .execution
                .steps
                .iter_mut()
                .find(|candidate| candidate.definition.id == step.id)
                .ok_or_else(|| internal("running step disappeared"))?;
            state.process_id = Some(output.process_id.clone());
            Ok(())
        })
        .await?;
    let mut logs = guest
        .subscribe_log(
            workspace.clone(),
            request_context(trace_id, execution_id),
            process_api::ProcessLogSubscription {
                process_id: output.process_id.clone(),
                last_line: None,
            },
        )
        .await
        .map_err(|error| unavailable(format!("could not follow step output: {error}")))?;
    let mut last_line = 0;
    let mut log_closed = false;
    let mut poll = tokio::time::interval(PROCESS_POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut host_operation_id = None;

    let terminal = loop {
        tokio::select! {
            event = logs.recv(), if !log_closed => {
                match event.map_err(|error| unavailable(format!("step output stream failed: {error}")))? {
                    Some(event) => {
                        retain_process_lines(
                            service,
                            execution_id,
                            step,
                            &event.lines,
                            host_marker_nonce,
                            &mut host_operation_id,
                        ).await?;
                        if let Some(line) = event.lines.last() {
                            last_line = line.line;
                        }
                    }
                    None => log_closed = true,
                }
            }
            _ = poll.tick() => {
                if let Some(state) = process_state(
                    guest,
                    workspace.clone(),
                    trace_id,
                    execution_id,
                    &pod_id,
                    &output.process_id,
                ).await?
                    && matches!(
                        state,
                        process_api::ProcessState::Exited(_)
                            | process_api::ProcessState::Failed(_)
                    )
                {
                    break state;
                }
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    cancel_host_operation(host_operations, host_operation_id.as_ref()).await;
                    terminate_process(
                        guest,
                        workspace.clone(),
                        trace_id,
                        execution_id,
                        &output.process_id,
                    ).await;
                    if host_operation_id.is_none()
                        && host_marker_nonce.is_some()
                        && let Err(error) = drain_process_log(
                            service,
                            guest,
                            workspace.clone(),
                            trace_id,
                            execution_id,
                            step,
                            &output.process_id,
                            last_line,
                            host_marker_nonce,
                            &mut host_operation_id,
                        ).await
                    {
                        tracing::warn!(
                            execution_id = %execution_id.0,
                            step_id = %step.id,
                            %error,
                            "failed to drain canceled Automation step output"
                        );
                    }
                    cancel_host_operation(host_operations, host_operation_id.as_ref()).await;
                    return Err(invalid_request("Automation execution was canceled"));
                }
            }
            () = sleep_to_deadline(deadline), if deadline.is_some() => {
                cancel_host_operation(host_operations, host_operation_id.as_ref()).await;
                terminate_process(
                    guest,
                    workspace.clone(),
                    trace_id,
                    execution_id,
                    &output.process_id,
                ).await;
                if host_operation_id.is_none()
                    && host_marker_nonce.is_some()
                    && let Err(error) = drain_process_log(
                        service,
                        guest,
                        workspace.clone(),
                        trace_id,
                        execution_id,
                        step,
                        &output.process_id,
                        last_line,
                        host_marker_nonce,
                        &mut host_operation_id,
                    ).await
                {
                    tracing::warn!(
                        execution_id = %execution_id.0,
                        step_id = %step.id,
                        %error,
                        "failed to drain timed-out Automation step output"
                    );
                }
                cancel_host_operation(host_operations, host_operation_id.as_ref()).await;
                return Err(invalid_request("Automation execution timed out"));
            }
        }
    };
    drain_process_log(
        service,
        guest,
        workspace,
        trace_id,
        execution_id,
        step,
        &output.process_id,
        last_line,
        host_marker_nonce,
        &mut host_operation_id,
    )
    .await?;
    match terminal {
        process_api::ProcessState::Exited(exit) if exit.code == Some(0) => Ok(host_operation_id),
        process_api::ProcessState::Exited(exit) => Err(invalid_request(format!(
            "step process exited with code {:?} and signal {:?}",
            exit.code, exit.signal
        ))),
        process_api::ProcessState::Failed(failure) => Err(unavailable(format!(
            "step process failed: {}",
            failure.message
        ))),
        _ => Err(internal("non-terminal process escaped terminal check")),
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_process_log(
    service: &AutomationService,
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    step: &api::AutomationStepDefinition,
    process_id: &process_api::ProcessId,
    mut last_line: u64,
    host_marker_nonce: Option<&str>,
    host_operation_id: &mut Option<host_operation_api::HostOperationId>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let mut logs = guest
        .subscribe_log(
            workspace,
            request_context(trace_id, execution_id),
            process_api::ProcessLogSubscription {
                process_id: process_id.clone(),
                last_line: Some(last_line),
            },
        )
        .await
        .map_err(|error| unavailable(format!("could not drain step output: {error}")))?;
    loop {
        let event = match tokio::time::timeout(LOG_DRAIN_QUIET_PERIOD, logs.recv()).await {
            Ok(event) => {
                event.map_err(|error| unavailable(format!("step output drain failed: {error}")))?
            }
            Err(_) => return Ok(()),
        };
        let Some(event) = event else {
            return Ok(());
        };
        retain_process_lines(
            service,
            execution_id,
            step,
            &event.lines,
            host_marker_nonce,
            host_operation_id,
        )
        .await?;
        if let Some(line) = event.lines.last() {
            if line.line <= last_line {
                return Ok(());
            }
            last_line = line.line;
        }
    }
}

async fn retain_process_lines(
    service: &AutomationService,
    execution_id: &AutomationExecutionId,
    step: &api::AutomationStepDefinition,
    lines: &[process_api::ProcessLogLine],
    host_marker_nonce: Option<&str>,
    host_operation_id: &mut Option<host_operation_api::HostOperationId>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    for line in lines {
        if let Some(nonce) = host_marker_nonce
            && let Some(id) = parse_host_operation_marker(line.content.as_ref(), nonce)
        {
            *host_operation_id = Some(id.clone());
            service
                .update_execution(execution_id, |stored| {
                    let state = stored
                        .execution
                        .steps
                        .iter_mut()
                        .find(|candidate| candidate.definition.id == step.id)
                        .ok_or_else(|| internal("host-command step disappeared"))?;
                    state.host_operation_id = Some(id);
                    Ok(())
                })
                .await?;
            continue;
        }
        service
            .append_output(
                execution_id,
                Some(step.id.as_ref()),
                api::AutomationOutputSource::Process(line.source.clone()),
                line.content.to_string(),
            )
            .await?;
    }
    Ok(())
}

fn parse_host_operation_marker(
    content: &str,
    nonce: &str,
) -> Option<host_operation_api::HostOperationId> {
    let value = content
        .strip_prefix(AUTOMATION_HOST_OPERATION_MARKER_PREFIX)?
        .strip_prefix(nonce)?
        .strip_prefix("::")?;
    host_operation_api::HostOperationId::from_str(value).ok()
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_step(
    service: &AutomationService,
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    pod_id: &pod_api::PodId,
    step_index: usize,
    agent: &api::AutomationAgentStep,
    cancel: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let selection = agent
        .selection
        .as_ref()
        .ok_or_else(|| internal("validated agent step has no harness selection"))?;
    let stored = service.require_execution(execution_id).await?;
    let chat_id = if let Some(chat_id) = stored.execution.chat_id.clone() {
        chat_id
    } else {
        let output = guest
            .execute(
                workspace.clone(),
                request_context(trace_id, execution_id),
                chat_api::CreateChatAction {
                    pod_id: pod_id.clone(),
                    cost_center_id: None,
                    harness: selection.harness.clone(),
                    purpose: Some(chat_api::ChatPurpose::Automation(
                        chat_api::AutomationChatPurpose {
                            execution_id: execution_id.0.clone(),
                        },
                    )),
                    title: Some(format!("Automation: {}", stored.execution.definition.name).into()),
                    model: selection.model.clone(),
                    initial_prompt: None,
                    auto_attach: Some(true),
                },
            )
            .await
            .map_err(|error| unavailable(format!("could not create Automation chat: {error}")))?;
        service
            .update_execution(execution_id, |stored| {
                stored.execution.chat_id = Some(output.chat_id.clone());
                Ok(())
            })
            .await?;
        output.chat_id
    };
    wait_for_chat_idle(
        guest,
        workspace.clone(),
        trace_id,
        execution_id,
        pod_id,
        &chat_id,
        cancel,
        deadline,
    )
    .await?;
    let prompt = automation_agent_prompt(&stored.execution, step_index, agent.prompt.as_ref());
    let output = guest
        .execute(
            workspace.clone(),
            request_context(trace_id, execution_id),
            chat_api::SendChatPromptAction {
                chat_id: chat_id.clone(),
                prompt: chat_api::ChatPrompt {
                    text: Some(prompt.into()),
                    attachments: Vec::new().into(),
                    model: selection.model.clone(),
                },
                mode: chat_api::ChatPromptMode::WhenIdle,
            },
        )
        .await
        .map_err(|error| unavailable(format!("could not start Automation agent turn: {error}")))?;
    let chat_api::ChatPromptDelivery::Started(started) = output.delivery else {
        return Err(internal(
            "exclusive Automation chat unexpectedly queued an agent prompt",
        ));
    };
    service
        .update_execution(execution_id, |stored| {
            stored.execution.steps[step_index].turn_id = Some(started.turn_id.clone());
            Ok(())
        })
        .await?;
    wait_for_turn(
        service,
        guest,
        workspace,
        trace_id,
        execution_id,
        &chat_id,
        step_index,
        &started.turn_id,
        cancel,
        deadline,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_chat_idle(
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    pod_id: &pod_api::PodId,
    chat_id: &chat_api::ChatId,
    cancel: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let mut ticker = tokio::time::interval(PROCESS_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let output = guest.execute(
                    workspace.clone(),
                    request_context(trace_id, execution_id),
                    chat_api::GetPodChatsAction { pod_id: pod_id.clone() },
                ).await.map_err(|error| unavailable(format!("could not inspect Automation chat: {error}")))?;
                let summary = output.chats.iter().find(|summary| &summary.chat_id == chat_id)
                    .ok_or_else(|| internal("Automation chat disappeared"))?;
                if let Some(error) = &summary.last_binding_error {
                    return Err(unavailable(format!("Automation chat binding failed: {}", error.message)));
                }
                if summary.agent_status == chat_api::ChatAgentStatus::Idle
                    && summary.binding.as_ref().is_some_and(|binding| {
                        binding.status == chat_api::ChatBindingStatus::Attached
                    })
                {
                    return Ok(());
                }
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    return Err(invalid_request("Automation execution was canceled"));
                }
            }
            () = sleep_to_deadline(deadline), if deadline.is_some() => {
                return Err(invalid_request("Automation execution timed out"));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_turn(
    service: &AutomationService,
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    chat_id: &chat_api::ChatId,
    step_index: usize,
    turn_id: &chat_api::ChatTurnId,
    cancel: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let mut events = guest
        .subscribe(
            workspace.clone(),
            request_context(trace_id, execution_id),
            chat_api::ChatSubscription {
                chat_id: chat_id.clone(),
                cursor: None,
            },
        )
        .await
        .map_err(|error| unavailable(format!("could not follow Automation chat: {error}")))?;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event.map_err(|error| unavailable(format!("Automation chat stream failed: {error}")))? else {
                    return Err(unavailable("Automation chat stream closed"));
                };
                if let Some(status) = chat_status(&event.change) {
                    let waiting = status == chat_api::ChatAgentStatus::UserInputRequired;
                    service.update_execution(execution_id, |stored| {
                        stored.execution.state = if waiting {
                            api::AutomationExecutionState::WaitingForInput
                        } else {
                            api::AutomationExecutionState::Running
                        };
                        stored.execution.steps[step_index].state = if waiting {
                            api::AutomationStepState::WaitingForInput
                        } else {
                            api::AutomationStepState::Running
                        };
                        Ok(())
                    }).await?;
                }
                if let Some(turn) = matching_turn(&event.change, turn_id) {
                    match turn.state {
                        chat_api::ChatTurnState::Running => {}
                        chat_api::ChatTurnState::Completed => return Ok(()),
                        chat_api::ChatTurnState::Interrupted => {
                            return Err(invalid_request("Automation agent turn was interrupted"));
                        }
                        chat_api::ChatTurnState::Failed => {
                            let message = turn.error.as_ref().map_or(
                                "Automation agent turn failed",
                                |error| error.message.as_ref(),
                            );
                            return Err(invalid_request(message));
                        }
                    }
                }
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    interrupt_agent_turn(
                        guest,
                        workspace.clone(),
                        trace_id,
                        execution_id,
                        chat_id,
                    ).await;
                    return Err(invalid_request("Automation execution was canceled"));
                }
            }
            () = sleep_to_deadline(deadline), if deadline.is_some() => {
                interrupt_agent_turn(
                    guest,
                    workspace.clone(),
                    trace_id,
                    execution_id,
                    chat_id,
                ).await;
                return Err(invalid_request("Automation execution timed out"));
            }
        }
    }
}

async fn interrupt_agent_turn(
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    chat_id: &chat_api::ChatId,
) {
    if let Err(error) = guest
        .execute(
            workspace,
            request_context(trace_id, execution_id),
            chat_api::InterruptChatAction {
                chat_id: chat_id.clone(),
            },
        )
        .await
    {
        tracing::warn!(
            execution_id = %execution_id.0,
            chat_id = %chat_id.0,
            %error,
            "failed to interrupt Automation agent turn"
        );
    }
}

fn matching_turn<'a>(
    change: &'a chat_api::ChatChange,
    turn_id: &chat_api::ChatTurnId,
) -> Option<&'a chat_api::ChatTurn> {
    match change {
        chat_api::ChatChange::BootstrapTurns(turns) => {
            turns.turns.iter().find(|turn| &turn.turn_id == turn_id)
        }
        chat_api::ChatChange::Mutation(mutation) => match &mutation.mutation {
            chat_api::ChatMutation::UpsertTurn(turn) if &turn.turn_id == turn_id => Some(turn),
            _ => None,
        },
        _ => None,
    }
}

fn chat_status(change: &chat_api::ChatChange) -> Option<chat_api::ChatAgentStatus> {
    match change {
        chat_api::ChatChange::BootstrapStarted(started) => Some(started.summary.agent_status),
        chat_api::ChatChange::Mutation(mutation) => match &mutation.mutation {
            chat_api::ChatMutation::UpdateSummary(summary) => Some(summary.agent_status),
            _ => None,
        },
        _ => None,
    }
}

fn automation_agent_prompt(
    execution: &api::AutomationExecution,
    step_index: usize,
    prompt: &str,
) -> String {
    let mut context = format!(
        "You are running Automation {:?}, execution {}.\n\n{}",
        execution.definition.name, execution.id.0, prompt
    );
    let prior_outputs = execution.steps[..step_index]
        .iter()
        .filter(|step| {
            matches!(
                step.definition.kind,
                api::AutomationStepKind::Command(_) | api::AutomationStepKind::HostCommand(_)
            )
        })
        .map(|step| {
            format!(
                "- {}: {}",
                step.definition.name,
                step_output_path(&execution.id, step.definition.id.as_ref())
            )
        })
        .collect::<Vec<_>>();
    if !prior_outputs.is_empty() {
        context.push_str(
            "\n\nFull output from prior command steps is available in the shared pod filesystem:\n",
        );
        context.push_str(&prior_outputs.join("\n"));
    }
    context
}

async fn process_state(
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    pod_id: &pod_api::PodId,
    process_id: &process_api::ProcessId,
) -> Result<Option<process_api::ProcessState>, reportify::Report<AutomationServiceError>> {
    let output = guest
        .execute(
            workspace,
            request_context(trace_id, execution_id),
            process_api::GetPodProcessesAction {
                pod_id: pod_id.clone(),
            },
        )
        .await
        .map_err(|error| unavailable(format!("could not inspect step process: {error}")))?;
    Ok(output
        .processes
        .iter()
        .find(|process| &process.id == process_id)
        .map(|process| process.status.clone()))
}

#[allow(clippy::too_many_arguments)]
async fn sync_pod(
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    pod_id: &pod_api::PodId,
    cancel: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    let output = guest
        .spawn(
            workspace.clone(),
            request_context(trace_id, execution_id),
            process_api::SpawnProcessAction {
                pod_id: pod_id.clone(),
                start_pod: Some(true),
                title: "Persist Automation step state".into(),
                executable: SYNC_PROGRAM.into(),
                arguments: Vec::new().into(),
                environment: HashMap::new(),
                working_directory: None,
                terminal: None,
                log_stdout: Some(false),
                profile: process_api::ProcessExecutionProfile::User,
            },
        )
        .await
        .map_err(|error| unavailable(format!("could not persist step state: {error}")))?;
    let mut poll = tokio::time::interval(PROCESS_POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = poll.tick() => {
                let Some(state) = process_state(
                    guest,
                    workspace.clone(),
                    trace_id,
                    execution_id,
                    pod_id,
                    &output.process_id,
                ).await? else {
                    continue;
                };
                match state {
                    process_api::ProcessState::Exited(exit) if exit.code == Some(0) => {
                        return Ok(());
                    }
                    process_api::ProcessState::Exited(exit) => {
                        return Err(unavailable(format!(
                            "persisting step state exited with code {:?} and signal {:?}",
                            exit.code, exit.signal
                        )));
                    }
                    process_api::ProcessState::Failed(failure) => {
                        return Err(unavailable(format!(
                            "persisting step state failed: {}",
                            failure.message
                        )));
                    }
                    _ => {}
                }
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    terminate_process(
                        guest,
                        workspace,
                        trace_id,
                        execution_id,
                        &output.process_id,
                    ).await;
                    return Err(invalid_request("Automation execution was canceled"));
                }
            }
            () = sleep_to_deadline(deadline), if deadline.is_some() => {
                terminate_process(
                    guest,
                    workspace,
                    trace_id,
                    execution_id,
                    &output.process_id,
                ).await;
                return Err(invalid_request("Automation execution timed out"));
            }
        }
    }
}

async fn cancel_host_operation(
    host_operations: Option<&HostOperationService>,
    operation_id: Option<&host_operation_api::HostOperationId>,
) {
    let (Some(host_operations), Some(operation_id)) = (host_operations, operation_id) else {
        return;
    };
    if let Err(error) = host_operations
        .cancel(
            host_operation_api::CancelHostOperationAction {
                operation_id: operation_id.clone(),
            },
            wire::Actor::Host,
        )
        .await
    {
        tracing::warn!(
            operation_id = %operation_id.0,
            %error,
            "failed to cancel host operation for Automation"
        );
    }
}

async fn terminate_process(
    guest: &GuestClient,
    workspace: ValidatedWorkspaceName,
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
    process_id: &process_api::ProcessId,
) {
    if let Err(error) = guest
        .kill(
            workspace.clone(),
            request_context(trace_id, execution_id),
            process_api::KillProcessAction {
                process_id: process_id.clone(),
                signal: process_api::ProcessSignal::Terminate,
            },
        )
        .await
    {
        tracing::warn!(
            execution_id = %execution_id.0,
            process_id = %process_id.0,
            %error,
            "failed to terminate Automation step process"
        );
    }
    tokio::time::sleep(PROCESS_TERMINATION_GRACE).await;
    if let Err(error) = guest
        .kill(
            workspace,
            request_context(trace_id, execution_id),
            process_api::KillProcessAction {
                process_id: process_id.clone(),
                signal: process_api::ProcessSignal::Kill,
            },
        )
        .await
    {
        tracing::warn!(
            execution_id = %execution_id.0,
            process_id = %process_id.0,
            %error,
            "failed to kill Automation step process"
        );
    }
}

async fn fail_execution(
    service: &AutomationService,
    id: &AutomationExecutionId,
    message: String,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    service
        .update_execution(id, |stored| {
            if is_terminal(stored.execution.state) {
                return Ok(());
            }
            let now = Timestamp::now();
            stored.execution.state = api::AutomationExecutionState::Failed;
            stored.execution.finished_at = Some(now);
            stored.execution.error = Some(message.clone().into());
            for step in &mut *stored.execution.steps {
                if matches!(
                    step.state,
                    api::AutomationStepState::Running
                        | api::AutomationStepState::WaitingForInput
                        | api::AutomationStepState::WaitingForApproval
                ) {
                    step.state = api::AutomationStepState::Failed;
                    step.finished_at = Some(now);
                    step.error = Some(message.clone().into());
                }
            }
            skip_pending_steps(&mut stored.execution);
            Ok(())
        })
        .await?;
    service
        .append_output(
            id,
            None,
            api::AutomationOutputSource::Automation,
            format!("execution failed: {message}"),
        )
        .await?;
    Ok(())
}

async fn cancel_running_execution(
    service: &AutomationService,
    id: &AutomationExecutionId,
) -> Result<(), reportify::Report<AutomationServiceError>> {
    service
        .update_execution(id, |stored| {
            super::cancel_execution(&mut stored.execution);
            Ok(())
        })
        .await?;
    service
        .append_output(
            id,
            None,
            api::AutomationOutputSource::Automation,
            "execution canceled",
        )
        .await?;
    Ok(())
}

fn request_context(
    trace_id: &TraceId,
    execution_id: &AutomationExecutionId,
) -> wire::RequestContext {
    wire::RequestContext {
        origin: wire::Actor::Host,
        caller: wire::Actor::Host,
        trace_id: trace_id.clone(),
        caused_by: Some(execution_id.0.clone()),
    }
}

fn validated_workspace(
    workspace: &WorkspaceName,
) -> Result<ValidatedWorkspaceName, reportify::Report<AutomationServiceError>> {
    ValidatedWorkspaceName::new(workspace.as_str().to_owned())
        .map_err(|error| invalid_request(error.to_string()))
}

fn execution_deadline(execution: &api::AutomationExecution) -> Option<Instant> {
    let timeout = execution.definition.timeout_seconds?;
    let started = execution.started_at?;
    let elapsed = Timestamp::now()
        .as_second()
        .saturating_sub(started.as_second());
    let elapsed = u64::try_from(elapsed).unwrap_or(0);
    Some(Instant::now() + Duration::from_secs(timeout.saturating_sub(elapsed)))
}

fn execution_timed_out(execution: &api::AutomationExecution) -> bool {
    execution
        .definition
        .timeout_seconds
        .zip(execution.started_at)
        .is_some_and(|(timeout, started)| {
            Timestamp::now()
                .as_second()
                .saturating_sub(started.as_second())
                >= i64::try_from(timeout).unwrap_or(i64::MAX)
        })
}

async fn sleep_to_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn step_output_path(execution_id: &AutomationExecutionId, step_id: &str) -> String {
    format!(
        "$HOME/{}/{}/{}",
        AUTOMATION_OUTPUT_RELATIVE_ROOT,
        execution_id.0,
        step_output_file(step_id)
    )
}

fn execution_output_relative_directory(execution_id: &AutomationExecutionId) -> String {
    format!("{AUTOMATION_OUTPUT_RELATIVE_ROOT}/{}", execution_id.0)
}

fn step_output_file(step_id: &str) -> String {
    format!("{step_id}.log")
}

#[cfg(test)]
mod tests {
    use tascarrel_api::ids::AutomationExecutionId;
    use tascarrel_api::ids::HostOperationId;

    use super::AUTOMATION_HOST_OPERATION_MARKER_PREFIX;
    use super::execution_output_relative_directory;
    use super::parse_host_operation_marker;
    use super::step_output_path;

    /// Only the nonce-bound hidden marker reveals a host operation identifier.
    #[test]
    fn host_operation_marker_requires_exact_nonce() {
        let operation_id = HostOperationId::generate();
        let content = format!(
            "{AUTOMATION_HOST_OPERATION_MARKER_PREFIX}expected::{}",
            operation_id.0
        );

        assert_eq!(
            parse_host_operation_marker(&content, "expected"),
            Some(operation_id.clone())
        );
        assert!(parse_host_operation_marker(&content, "different").is_none());
        assert!(parse_host_operation_marker(operation_id.0.as_ref(), "expected").is_none());
    }

    /// Step output lives in the pod's durable root rather than transient
    /// `/tmp`.
    #[test]
    fn step_output_uses_execution_scoped_persistent_path() {
        let execution_id = AutomationExecutionId("execution".into());

        assert_eq!(
            execution_output_relative_directory(&execution_id),
            ".local/state/tascarrel/automations/execution"
        );
        assert_eq!(
            step_output_path(&execution_id, "prepare"),
            "$HOME/.local/state/tascarrel/automations/execution/prepare.log"
        );
    }
}
