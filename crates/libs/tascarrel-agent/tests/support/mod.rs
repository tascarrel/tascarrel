use std::collections::VecDeque;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use futures_util::stream;
use reportify::Report;
use serde::Deserialize;
use tascarrel_agent::Agent;
use tascarrel_agent::AgentConfig;
use tascarrel_agent::AgentEvent;
use tascarrel_agent::AgentEventHandler;
use tascarrel_agent::BashTool;
use tascarrel_agent::EditTool;
use tascarrel_agent::FileChangeOperation;
use tascarrel_agent::FileWorkspace;
use tascarrel_agent::ModelBackend;
use tascarrel_agent::ModelError;
use tascarrel_agent::ModelEventStream;
use tascarrel_agent::ModelMessage;
use tascarrel_agent::ModelRequest;
use tascarrel_agent::ModelResult;
use tascarrel_agent::ModelStreamEvent;
use tascarrel_agent::ProcessRuntime;
use tascarrel_agent::ProcessTool;
use tascarrel_agent::ReadTool;
use tascarrel_agent::ToolArtifact;
use tascarrel_agent::ToolRegistry;
use tascarrel_agent::WriteTool;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub async fn run_scenario(file_name: &str) {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/scenarios")
        .join(file_name);
    let fixture = tokio::fs::read_to_string(&fixture_path)
        .await
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", fixture_path.display()));
    let scenario: Scenario = serde_json::from_str(&fixture)
        .unwrap_or_else(|error| panic!("failed to decode {}: {error}", fixture_path.display()));
    let result = ScenarioEngine::new(scenario).run().await;
    if let Err(error) = result {
        panic!("{file_name}: {error}");
    }
}

struct ScenarioEngine {
    scenario: Scenario,
    directory: TempDir,
}

impl ScenarioEngine {
    fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            directory: tempfile::tempdir().expect("scenario workspace should be created"),
        }
    }

    async fn run(self) -> Result<(), String> {
        self.write_initial_files().await?;
        let files = Arc::new(
            FileWorkspace::open(self.directory.path())
                .await
                .map_err(|error| error.error().to_string())?,
        );
        let cancellation = CancellationToken::new();
        let processes = Arc::new(
            ProcessRuntime::open(self.directory.path())
                .await
                .map_err(|error| error.error().to_string())?,
        );
        let backend = Arc::new(ScenarioModelBackend {
            workspace: self.directory.path().to_path_buf(),
            responses: Mutex::new(self.scenario.responses.clone().into()),
            requests: Mutex::new(Vec::new()),
            cancellation: cancellation.clone(),
        });
        let mut tools = ToolRegistry::new();
        if self.tool_enabled("bash") {
            tools
                .register(BashTool::new(Arc::clone(&processes)))
                .map_err(|error| error.error().to_string())?;
        }
        if self.tool_enabled("read") {
            tools
                .register(ReadTool::default())
                .map_err(|error| error.error().to_string())?;
        }
        if self.tool_enabled("write") {
            tools
                .register(WriteTool)
                .map_err(|error| error.error().to_string())?;
        }
        if self.tool_enabled("edit") {
            tools
                .register(EditTool)
                .map_err(|error| error.error().to_string())?;
        }
        if self.tool_enabled("process") {
            tools
                .register(ProcessTool::new(processes))
                .map_err(|error| error.error().to_string())?;
        }
        let agent = Agent::new(
            backend.clone(),
            tools,
            files,
            AgentConfig {
                max_steps: self.scenario.max_steps.unwrap_or(16),
                model_retry_delay: std::time::Duration::ZERO,
                ..AgentConfig::default()
            },
        );
        let (observed_events, event_handler) = recording_event_handler();
        let run = agent
            .run_with_event_handler(self.scenario.prompt.clone(), cancellation, event_handler)
            .await;

        match (&self.scenario.expected.error_contains, run) {
            (Some(expected), Err(error)) => {
                let actual = error.error().to_string();
                require_contains(&self.scenario.name, "agent error", &actual, expected)?;
            }
            (Some(expected), Ok(_)) => {
                return Err(format!(
                    "{}: expected agent error containing {expected:?}",
                    self.scenario.name
                ));
            }
            (None, Err(error)) => {
                return Err(format!(
                    "{}: unexpected agent error: {}",
                    self.scenario.name,
                    error.error()
                ));
            }
            (None, Ok(run)) => {
                check_recorded_events(&self.scenario.name, &observed_events, &run.events)?;
                self.check_run(&run.messages, &run.events)?;
            }
        }

        self.check_requests(&backend.requests.lock().await)?;
        if let Some(delay_ms) = self.scenario.check_after_ms {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        self.check_files().await
    }

    async fn write_initial_files(&self) -> Result<(), String> {
        for file in &self.scenario.initial_files {
            let path = checked_scenario_path(self.directory.path(), &file.path)?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
            }
            tokio::fs::write(&path, &file.content)
                .await
                .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
        }
        for file in &self.scenario.initial_repeated_files {
            let path = checked_scenario_path(self.directory.path(), &file.path)?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
            }
            let mut content = file.unit.repeat(file.count);
            content.push_str(&file.suffix);
            tokio::fs::write(&path, content)
                .await
                .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
        }
        Ok(())
    }

    fn tool_enabled(&self, name: &str) -> bool {
        self.scenario
            .enabled_tools
            .as_ref()
            .is_none_or(|enabled| enabled.iter().any(|candidate| candidate == name))
    }

    fn check_run(&self, messages: &[ModelMessage], events: &[AgentEvent]) -> Result<(), String> {
        let actual_results = messages
            .iter()
            .filter_map(|message| match message {
                ModelMessage::Tool {
                    tool_name,
                    content,
                    is_error,
                    ..
                } => Some((tool_name, content, *is_error)),
                ModelMessage::System { .. }
                | ModelMessage::User { .. }
                | ModelMessage::Assistant(_) => None,
            })
            .collect::<Vec<_>>();
        if actual_results.len() != self.scenario.expected.tool_results.len() {
            return Err(format!(
                "{}: expected {} tool results, found {}",
                self.scenario.name,
                self.scenario.expected.tool_results.len(),
                actual_results.len()
            ));
        }
        for (index, (expected, (name, content, is_error))) in self
            .scenario
            .expected
            .tool_results
            .iter()
            .zip(actual_results)
            .enumerate()
        {
            if expected.name != *name || expected.is_error != is_error {
                return Err(format!(
                    "{}: tool result {index} expected {} error={}, found {} error={}: {content}",
                    self.scenario.name, expected.name, expected.is_error, name, is_error
                ));
            }
            require_contains(
                &self.scenario.name,
                &format!("tool result {index}"),
                content,
                &expected.content_contains,
            )?;
            for additional in &expected.additional_content_contains {
                require_contains(
                    &self.scenario.name,
                    &format!("tool result {index}"),
                    content,
                    additional,
                )?;
            }
            for unexpected in &expected.content_not_contains {
                if content.contains(unexpected) {
                    return Err(format!(
                        "{}: tool result {index} unexpectedly contained {unexpected:?}",
                        self.scenario.name
                    ));
                }
            }
        }

        let actual_event_kinds = events.iter().map(event_kind).collect::<Vec<_>>();
        if actual_event_kinds != self.scenario.expected.event_kinds {
            return Err(format!(
                "{}: event kinds differ\nexpected: {:?}\nactual:   {:?}",
                self.scenario.name, self.scenario.expected.event_kinds, actual_event_kinds
            ));
        }
        self.check_file_changes(events)?;
        Ok(())
    }

    fn check_file_changes(&self, events: &[AgentEvent]) -> Result<(), String> {
        let Some(expected_changes) = &self.scenario.expected.file_changes else {
            return Ok(());
        };
        let serialized_events = serde_json::to_string(events)
            .map_err(|error| format!("failed to serialize agent events: {error}"))?;
        for parser_field in [
            "\"artifacts\"",
            "\"changes\"",
            "\"path\"",
            "\"unified_diff\"",
        ] {
            require_contains(
                &self.scenario.name,
                "serialized file-change parser contract",
                &serialized_events,
                parser_field,
            )?;
        }
        let actual = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolExecutionCompleted { artifacts, .. } => Some(artifacts),
                _ => None,
            })
            .flatten()
            .flat_map(|artifact| match artifact {
                ToolArtifact::FileChanges { changes } => changes.iter(),
            })
            .collect::<Vec<_>>();
        if actual.len() != expected_changes.len() {
            return Err(format!(
                "{}: expected {} file-change artifacts, found {}",
                self.scenario.name,
                expected_changes.len(),
                actual.len()
            ));
        }
        for (index, (actual, expected)) in actual.into_iter().zip(expected_changes).enumerate() {
            if actual.path != expected.path
                || actual.operation != expected.operation
                || actual.additions != expected.additions
                || actual.deletions != expected.deletions
            {
                return Err(format!(
                    "{}: file change {index} did not match: {actual:?}",
                    self.scenario.name
                ));
            }
            require_contains(
                &self.scenario.name,
                &format!("file change {index} unified diff"),
                &actual.unified_diff,
                &expected.unified_diff_contains,
            )?;
        }
        Ok(())
    }

    fn check_requests(&self, requests: &[ModelRequest]) -> Result<(), String> {
        if requests.len() != self.scenario.expected.request_count {
            return Err(format!(
                "{}: expected {} model requests, found {}",
                self.scenario.name,
                self.scenario.expected.request_count,
                requests.len()
            ));
        }
        for (index, request) in requests.iter().enumerate() {
            self.check_request_contract(index, request)?;
        }
        if !self.scenario.expected.request_last_messages.is_empty() {
            if requests.len() != self.scenario.expected.request_last_messages.len() {
                return Err(format!(
                    "{}: expected {} last-message assertions, found {} requests",
                    self.scenario.name,
                    self.scenario.expected.request_last_messages.len(),
                    requests.len()
                ));
            }
            for (index, (request, expected)) in requests
                .iter()
                .zip(&self.scenario.expected.request_last_messages)
                .enumerate()
            {
                let actual = request.messages.last().ok_or_else(|| {
                    format!("{}: request {index} had no messages", self.scenario.name)
                })?;
                check_message(&self.scenario.name, index, actual, expected)?;
            }
        }
        Ok(())
    }

    fn check_request_contract(&self, index: usize, request: &ModelRequest) -> Result<(), String> {
        let actual_tools = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        let mut expected_tools = self.scenario.enabled_tools.clone().unwrap_or_else(|| {
            vec![
                "bash".to_owned(),
                "edit".to_owned(),
                "process".to_owned(),
                "read".to_owned(),
                "write".to_owned(),
            ]
        });
        expected_tools.sort();
        if actual_tools != expected_tools {
            return Err(format!(
                "{}: request {index} had unexpected tools {actual_tools:?}",
                self.scenario.name
            ));
        }
        let system = request
            .messages
            .first()
            .ok_or_else(|| format!("{}: request {index} had no messages", self.scenario.name))?;
        let ModelMessage::System { content } = system else {
            return Err(format!(
                "{}: request {index} did not start with a system message",
                self.scenario.name
            ));
        };
        for expected in &self.scenario.expected.system_prompt_contains {
            require_contains(
                &self.scenario.name,
                &format!("request {index} system prompt"),
                content,
                expected,
            )?;
        }
        for unexpected in &self.scenario.expected.system_prompt_not_contains {
            if content.contains(unexpected) {
                return Err(format!(
                    "{}: request {index} system prompt unexpectedly contained {unexpected:?}",
                    self.scenario.name
                ));
            }
        }
        for expected in &self.scenario.expected.tool_contracts {
            check_tool_contract(&self.scenario.name, index, request, expected)?;
        }
        Ok(())
    }

    async fn check_files(&self) -> Result<(), String> {
        for file in &self.scenario.expected.files {
            let path = checked_scenario_path(self.directory.path(), &file.path)?;
            let actual = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            if actual != file.content {
                return Err(format!(
                    "{}: unexpected content for {}\nexpected: {:?}\nactual:   {:?}",
                    self.scenario.name,
                    file.path.display(),
                    file.content,
                    actual
                ));
            }
        }
        for relative in &self.scenario.expected.absent_files {
            let path = checked_scenario_path(self.directory.path(), relative)?;
            if tokio::fs::try_exists(&path)
                .await
                .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?
            {
                return Err(format!(
                    "{}: expected {} to remain absent",
                    self.scenario.name,
                    relative.display()
                ));
            }
        }
        Ok(())
    }
}

/// Creates an observer that retains a copy of every live event.
fn recording_event_handler() -> (Arc<StdMutex<Vec<AgentEvent>>>, AgentEventHandler) {
    let observed_events = Arc::new(StdMutex::new(Vec::new()));
    let handler_events = Arc::clone(&observed_events);
    let event_handler = Arc::new(move |event: &AgentEvent| {
        handler_events
            .lock()
            .expect("the scenario event handler does not panic while holding its mutex")
            .push(event.clone());
    });
    (observed_events, event_handler)
}

/// Verifies that live projection did not lose or reorder retained events.
fn check_recorded_events(
    scenario: &str,
    observed_events: &StdMutex<Vec<AgentEvent>>,
    retained_events: &[AgentEvent],
) -> Result<(), String> {
    let observed_events = observed_events
        .lock()
        .expect("the scenario event handler does not panic while holding its mutex");
    if retained_events != observed_events.as_slice() {
        return Err(format!(
            "{scenario}: live event handler output differed from retained events"
        ));
    }
    Ok(())
}

struct ScenarioModelBackend {
    workspace: PathBuf,
    responses: Mutex<VecDeque<ScenarioResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
    cancellation: CancellationToken,
}

impl ModelBackend for ScenarioModelBackend {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, ModelResult<ModelEventStream>> {
        async move {
            if cancellation.is_cancelled() {
                return Err(Report::new(ModelError::Cancelled));
            }
            self.requests.lock().await.push(request);
            let response = self.responses.lock().await.pop_front().ok_or_else(|| {
                Report::new(ModelError::Protocol {
                    message: "scenario has no remaining model response".to_owned(),
                })
            })?;
            if response.cancel_before_response {
                self.cancellation.cancel();
            }
            if let Some(delay_ms) = response.cancel_after_ms {
                let cancellation = self.cancellation.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    cancellation.cancel();
                });
            }
            for mutation in response.before_response {
                apply_external_mutation(&self.workspace, mutation).await?;
            }
            let transport_error = response
                .transport_error
                .map(|message| Err(Report::new(ModelError::Transport { message })));
            let events = response.events.into_iter().map(Ok).chain(transport_error);
            Ok(stream::iter(events).boxed())
        }
        .boxed()
    }
}

async fn apply_external_mutation(workspace: &Path, mutation: ExternalMutation) -> ModelResult<()> {
    match mutation {
        ExternalMutation::Write { path, content } => {
            let path = checked_scenario_path(workspace, &path)
                .map_err(|message| Report::new(ModelError::Protocol { message }))?;
            tokio::fs::write(&path, content).await.map_err(|error| {
                Report::new(ModelError::Request {
                    message: format!("failed to apply external write: {error}"),
                })
            })?;
        }
    }
    Ok(())
}

fn checked_scenario_path(root: &Path, path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("invalid scenario path {}", path.display()));
    }
    Ok(root.join(path))
}

fn require_contains(
    scenario: &str,
    label: &str,
    actual: &str,
    expected: &str,
) -> Result<(), String> {
    if actual.contains(expected) {
        return Ok(());
    }
    Err(format!(
        "{scenario}: {label} did not contain {expected:?}: {actual:?}"
    ))
}

fn check_tool_contract(
    scenario: &str,
    request_index: usize,
    request: &ModelRequest,
    expected: &ToolContractExpectation,
) -> Result<(), String> {
    let actual = request
        .tools
        .iter()
        .find(|tool| tool.name == expected.name)
        .ok_or_else(|| {
            format!(
                "{scenario}: request {request_index} did not contain tool {}",
                expected.name
            )
        })?;
    require_contains(
        scenario,
        &format!("request {request_index} {} description", expected.name),
        &actual.description,
        &expected.description_contains,
    )?;
    for schema_fragment in &expected.schema_contains {
        require_contains(
            scenario,
            &format!("request {request_index} {} schema", expected.name),
            &actual.input_schema,
            schema_fragment,
        )?;
    }
    for guideline in &expected.guideline_contains {
        if !actual
            .prompt
            .guidelines
            .iter()
            .any(|actual| actual.contains(guideline))
        {
            return Err(format!(
                "{scenario}: request {request_index} {} guidelines did not contain {guideline:?}",
                expected.name
            ));
        }
    }
    Ok(())
}

fn check_message(
    scenario: &str,
    request_index: usize,
    actual: &ModelMessage,
    expected: &MessageExpectation,
) -> Result<(), String> {
    match (actual, expected) {
        (ModelMessage::System { content }, MessageExpectation::System { content_contains }) => {
            require_contains(
                scenario,
                &format!("request {request_index} system message"),
                content,
                content_contains,
            )
        }
        (ModelMessage::User { content }, MessageExpectation::User { content_contains }) => {
            require_contains(
                scenario,
                &format!("request {request_index} user message"),
                content,
                content_contains,
            )
        }
        (ModelMessage::Assistant(message), MessageExpectation::Assistant { tool_names }) => {
            let actual_names = message
                .tool_calls
                .iter()
                .map(|call| &call.name)
                .collect::<Vec<_>>();
            let expected_names = tool_names.iter().collect::<Vec<_>>();
            if actual_names == expected_names {
                Ok(())
            } else {
                Err(format!(
                    "{scenario}: request {request_index} expected assistant tools {expected_names:?}, found {actual_names:?}"
                ))
            }
        }
        (
            ModelMessage::Tool {
                tool_name,
                content,
                is_error,
                ..
            },
            MessageExpectation::Tool {
                name,
                is_error: expected_error,
                content_contains,
            },
        ) => {
            if tool_name != name || is_error != expected_error {
                return Err(format!(
                    "{scenario}: request {request_index} expected tool {name} error={expected_error}, found {tool_name} error={is_error}"
                ));
            }
            require_contains(
                scenario,
                &format!("request {request_index} tool result"),
                content,
                content_contains,
            )
        }
        _ => Err(format!(
            "{scenario}: request {request_index} last-message role did not match"
        )),
    }
}

fn event_kind(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::ModelRequestStarted { .. } => "model_request_started",
        AgentEvent::ModelRequestRetrying { .. } => "model_request_retrying",
        AgentEvent::ReasoningDelta { .. } => "reasoning_delta",
        AgentEvent::TextDelta { .. } => "text_delta",
        AgentEvent::ToolCallStarted { .. } => "tool_call_started",
        AgentEvent::ToolCallCompleted { .. } => "tool_call_completed",
        AgentEvent::ToolExecutionStarted { .. } => "tool_execution_started",
        AgentEvent::ToolExecutionCompleted { .. } => "tool_execution_completed",
        AgentEvent::Completed { .. } => "completed",
    }
}

#[derive(Clone, Deserialize)]
struct Scenario {
    name: String,
    prompt: String,
    #[serde(default)]
    initial_files: Vec<ScenarioFile>,
    #[serde(default)]
    initial_repeated_files: Vec<RepeatedScenarioFile>,
    responses: Vec<ScenarioResponse>,
    expected: ScenarioExpectation,
    max_steps: Option<usize>,
    check_after_ms: Option<u64>,
    enabled_tools: Option<Vec<String>>,
}

#[derive(Clone, Deserialize)]
struct ScenarioResponse {
    #[serde(default)]
    before_response: Vec<ExternalMutation>,
    #[serde(default)]
    cancel_before_response: bool,
    cancel_after_ms: Option<u64>,
    transport_error: Option<String>,
    events: Vec<ModelStreamEvent>,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExternalMutation {
    Write { path: PathBuf, content: String },
}

#[derive(Clone, Deserialize)]
struct ScenarioFile {
    path: PathBuf,
    content: String,
}

#[derive(Clone, Deserialize)]
struct RepeatedScenarioFile {
    path: PathBuf,
    unit: String,
    count: usize,
    #[serde(default)]
    suffix: String,
}

#[derive(Clone, Deserialize)]
struct ScenarioExpectation {
    request_count: usize,
    #[serde(default)]
    tool_results: Vec<ToolResultExpectation>,
    #[serde(default)]
    event_kinds: Vec<String>,
    #[serde(default)]
    files: Vec<ScenarioFile>,
    #[serde(default)]
    absent_files: Vec<PathBuf>,
    #[serde(default)]
    request_last_messages: Vec<MessageExpectation>,
    #[serde(default)]
    system_prompt_contains: Vec<String>,
    #[serde(default)]
    system_prompt_not_contains: Vec<String>,
    #[serde(default)]
    tool_contracts: Vec<ToolContractExpectation>,
    file_changes: Option<Vec<FileChangeExpectation>>,
    error_contains: Option<String>,
}

#[derive(Clone, Deserialize)]
struct ToolResultExpectation {
    name: String,
    is_error: bool,
    content_contains: String,
    #[serde(default)]
    additional_content_contains: Vec<String>,
    #[serde(default)]
    content_not_contains: Vec<String>,
}

#[derive(Clone, Deserialize)]
struct FileChangeExpectation {
    path: PathBuf,
    operation: FileChangeOperation,
    additions: usize,
    deletions: usize,
    unified_diff_contains: String,
}

#[derive(Clone, Deserialize)]
struct ToolContractExpectation {
    name: String,
    description_contains: String,
    #[serde(default)]
    schema_contains: Vec<String>,
    #[serde(default)]
    guideline_contains: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum MessageExpectation {
    System {
        content_contains: String,
    },
    User {
        content_contains: String,
    },
    Assistant {
        tool_names: Vec<String>,
    },
    Tool {
        name: String,
        is_error: bool,
        content_contains: String,
    },
}
