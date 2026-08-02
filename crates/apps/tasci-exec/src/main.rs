//! One-shot Tasci runner for exercising the local coding-agent loop.

use std::env;
use std::fmt;
use std::io;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::Mutex;

use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use tascarrel_agent::Agent;
use tascarrel_agent::AgentConfig;
use tascarrel_agent::AgentEvent;
use tascarrel_agent::AgentEventHandler;
use tascarrel_agent::AgentRun;
use tascarrel_agent::AgentSession;
use tascarrel_agent::AgentSessionStore;
use tascarrel_agent::BashTool;
use tascarrel_agent::EditTool;
use tascarrel_agent::FileWorkspace;
use tascarrel_agent::McpClient;
use tascarrel_agent::McpClientConfig;
use tascarrel_agent::McpServerConfiguration;
use tascarrel_agent::OpenAiChatBackend;
use tascarrel_agent::ProcessRuntime;
use tascarrel_agent::ProcessTool;
use tascarrel_agent::ReadTool;
use tascarrel_agent::TasciHarnessCommand;
use tascarrel_agent::TasciHarnessConfiguration;
use tascarrel_agent::TasciHarnessEvent;
use tascarrel_agent::TasciHarnessSession;
use tascarrel_agent::ToolArtifact;
use tascarrel_agent::ToolRegistry;
use tascarrel_agent::WriteTool;
use thiserror::Error;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

const LOCAL_API_BASE_URL: &str = "http://host.tascarrel.internal:18080/v1";
const LOCAL_MODEL: &str = "qwen3.6-35b-a3b-q6";
const TASCI_SESSION_DIRECTORY_ENVIRONMENT: &str = "TASCI_SESSION_DIR";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:?}");
            ExitCode::FAILURE
        }
    }
}

/// Builds and runs one local agent invocation.
async fn run() -> TasciExecResult<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(io::stderr)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .map_err(|source| TasciExecError::Logging { source }.report())?;

    let argument = env::args_os()
        .nth(1)
        .ok_or_else(|| TasciExecError::MissingPrompt.report())?;
    if argument == "--harness" {
        return run_harness().await;
    }
    run_once(argument.to_string_lossy().into_owned()).await
}

async fn run_once(prompt: String) -> TasciExecResult<()> {
    let workspace = env::current_dir().escalate(TasciExecError::CurrentDirectory)?;
    let configuration = TasciHarnessConfiguration {
        base_url: LOCAL_API_BASE_URL.to_owned(),
        model: LOCAL_MODEL.to_owned(),
        context_window: None,
        max_output_tokens: None,
        authorization: None,
        working_directory: workspace.to_string_lossy().into_owned(),
        mcp_servers: Vec::new(),
    };
    let runtime = open_agent_runtime(&configuration).await?;
    let agent = build_agent(&configuration, &runtime)?;
    tracing::info!(model = %configuration.model, "Tasci run started");
    let cancellation = CancellationToken::new();
    let printer = Arc::new(Mutex::new(EventPrinter::default()));
    let handler_printer = Arc::clone(&printer);
    let event_handler: AgentEventHandler = Arc::new(move |event| {
        handler_printer
            .lock()
            .expect(
                "the event printer mutex remains unpoisoned because event rendering does not panic",
            )
            .handle(event);
    });
    let run = agent.run_with_event_handler(prompt, cancellation.clone(), event_handler);
    tokio::pin!(run);

    let result = tokio::select! {
        result = &mut run => result,
        interrupt = tokio::signal::ctrl_c() => {
            interrupt.escalate(TasciExecError::Interrupt)?;
            cancellation.cancel();
            run.await
        }
    };

    let output_error = {
        let mut printer = printer.lock().expect(
            "the event printer mutex remains unpoisoned because event rendering does not panic",
        );
        printer.finish();
        printer.take_error()
    };
    result.map_err(|error| error.escalate(TasciExecError::Agent))?;
    if let Some(source) = output_error {
        return Err(TasciExecError::Output { source }.report());
    }
    tracing::info!("Tasci run completed");
    Ok(())
}

async fn run_harness() -> TasciExecResult<()> {
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    let TasciHarnessCommand::Start {
        configuration,
        session: requested_session,
    } = read_harness_command(&mut input).await?
    else {
        return Err(TasciExecError::ProtocolInput {
            message: "the first harness command must be start".to_owned(),
        }
        .report());
    };
    let runtime = open_agent_runtime(&configuration).await?;
    let mut agent = Arc::new(build_agent(&configuration, &runtime)?);
    let Some((mut session_store, mut session)) =
        initialize_protocol_session(&configuration, requested_session, &mut output).await?
    else {
        return Ok(());
    };
    announce_harness_started(&mut output, &configuration.model, &runtime.warnings).await?;

    loop {
        match read_harness_command(&mut input).await? {
            TasciHarnessCommand::Prompt {
                prompt,
                configuration,
            } => {
                if let Some(configuration) = configuration {
                    if configuration.working_directory != runtime.working_directory {
                        return Err(TasciExecError::ProtocolInput {
                            message: "Tasci cannot change working directories within a session"
                                .to_owned(),
                        }
                        .report());
                    }
                    if configuration.mcp_servers != runtime.mcp_servers {
                        return Err(TasciExecError::ProtocolInput {
                            message: "Tasci cannot change MCP servers within a session".to_owned(),
                        }
                        .report());
                    }
                    agent = Arc::new(build_agent(&configuration, &runtime)?);
                    tracing::info!(model = %configuration.model, "Tasci harness model changed");
                }
                let turn = run_harness_turn(
                    Arc::clone(&agent),
                    session,
                    prompt,
                    &mut session_store,
                    &mut input,
                    &mut output,
                )
                .await?;
                session = turn.session;
                if turn.stop {
                    tracing::info!("Tasci harness stopped");
                    write_harness_event(&mut output, TasciHarnessEvent::Stopped).await?;
                    return Ok(());
                }
            }
            TasciHarnessCommand::Compact => {
                let compaction = run_harness_compaction(
                    Arc::clone(&agent),
                    session,
                    &mut session_store,
                    &mut input,
                    &mut output,
                )
                .await?;
                session = compaction.session;
                if compaction.stop {
                    tracing::info!("Tasci harness stopped");
                    write_harness_event(&mut output, TasciHarnessEvent::Stopped).await?;
                    return Ok(());
                }
            }
            TasciHarnessCommand::Interrupt => {
                write_harness_event(
                    &mut output,
                    TasciHarnessEvent::TurnFinished {
                        error: Some("no Tasci turn is active".to_owned()),
                        cancelled: false,
                    },
                )
                .await?;
            }
            TasciHarnessCommand::Stop => {
                tracing::info!("Tasci harness stopped");
                write_harness_event(&mut output, TasciHarnessEvent::Stopped).await?;
                return Ok(());
            }
            TasciHarnessCommand::Start { .. } => {
                return Err(TasciExecError::ProtocolInput {
                    message: "Tasci harness start may be sent only once".to_owned(),
                }
                .report());
            }
        }
    }
}

struct HarnessTurn {
    session: AgentSession,
    stop: bool,
}

async fn run_harness_turn(
    agent: Arc<Agent>,
    session: AgentSession,
    prompt: String,
    session_store: &mut Option<AgentSessionStore>,
    input: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    output: &mut tokio::io::Stdout,
) -> TasciExecResult<HarnessTurn> {
    tracing::info!("Tasci turn started");
    let cancellation = CancellationToken::new();
    let fallback_session = session.clone();
    let (events, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
    let event_handler: AgentEventHandler = Arc::new(move |event| {
        if events.send(event.clone()).is_err() {
            tracing::debug!("Tasci turn event receiver closed before event delivery");
        }
    });
    let run =
        agent.continue_with_event_handler(session, prompt, cancellation.clone(), event_handler);
    tokio::pin!(run);
    let mut stop = false;
    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            event = event_receiver.recv() => {
                if let Some(event) = event {
                    write_harness_event(output, TasciHarnessEvent::Agent { value: event }).await?;
                }
            }
            command = read_harness_command(input) => {
                match command? {
                    TasciHarnessCommand::Interrupt => cancellation.cancel(),
                    TasciHarnessCommand::Stop => {
                        stop = true;
                        cancellation.cancel();
                    }
                    TasciHarnessCommand::Prompt { .. }
                    | TasciHarnessCommand::Compact
                    | TasciHarnessCommand::Start { .. } => {
                        return Err(TasciExecError::ProtocolInput {
                            message: "a Tasci turn accepts only interrupt or stop commands".to_owned(),
                        }.report());
                    }
                }
            }
        }
    };
    while let Ok(event) = event_receiver.try_recv() {
        write_harness_event(output, TasciHarnessEvent::Agent { value: event }).await?;
    }
    match result {
        Ok(AgentRun { session, .. }) => {
            finish_successful_harness_operation(
                session_store,
                session,
                fallback_session,
                stop,
                output,
                "turn",
            )
            .await
        }
        Err(_error) if cancellation.is_cancelled() => {
            tracing::info!("Tasci turn cancelled");
            write_harness_event(
                output,
                TasciHarnessEvent::TurnFinished {
                    error: None,
                    cancelled: true,
                },
            )
            .await?;
            Ok(HarnessTurn {
                session: fallback_session,
                stop,
            })
        }
        Err(error) => {
            let message = error.error().to_string();
            tracing::warn!(%error, "Tasci turn failed");
            write_harness_event(
                output,
                TasciHarnessEvent::TurnFinished {
                    error: Some(message),
                    cancelled: false,
                },
            )
            .await?;
            Ok(HarnessTurn {
                session: fallback_session,
                stop,
            })
        }
    }
}

async fn run_harness_compaction(
    agent: Arc<Agent>,
    session: AgentSession,
    session_store: &mut Option<AgentSessionStore>,
    input: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
    output: &mut tokio::io::Stdout,
) -> TasciExecResult<HarnessTurn> {
    tracing::info!("Tasci context compaction started");
    let cancellation = CancellationToken::new();
    let fallback_session = session.clone();
    let (events, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
    let event_handler: AgentEventHandler = Arc::new(move |event| {
        if events.send(event.clone()).is_err() {
            tracing::debug!("Tasci compaction event receiver closed before event delivery");
        }
    });
    let run = agent.compact_with_event_handler(session, cancellation.clone(), event_handler);
    tokio::pin!(run);
    let mut stop = false;
    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            event = event_receiver.recv() => {
                if let Some(event) = event {
                    write_harness_event(output, TasciHarnessEvent::Agent { value: event }).await?;
                }
            }
            command = read_harness_command(input) => {
                match command? {
                    TasciHarnessCommand::Interrupt => cancellation.cancel(),
                    TasciHarnessCommand::Stop => {
                        stop = true;
                        cancellation.cancel();
                    }
                    TasciHarnessCommand::Prompt { .. }
                    | TasciHarnessCommand::Compact
                    | TasciHarnessCommand::Start { .. } => {
                        return Err(TasciExecError::ProtocolInput {
                            message: "Tasci context compaction accepts only interrupt or stop commands".to_owned(),
                        }.report());
                    }
                }
            }
        }
    };
    while let Ok(event) = event_receiver.try_recv() {
        write_harness_event(output, TasciHarnessEvent::Agent { value: event }).await?;
    }
    match result {
        Ok(AgentRun { session, .. }) => {
            finish_successful_harness_operation(
                session_store,
                session,
                fallback_session,
                stop,
                output,
                "context compaction",
            )
            .await
        }
        Err(_error) if cancellation.is_cancelled() => {
            tracing::info!("Tasci context compaction cancelled");
            write_harness_event(
                output,
                TasciHarnessEvent::TurnFinished {
                    error: None,
                    cancelled: true,
                },
            )
            .await?;
            Ok(HarnessTurn {
                session: fallback_session,
                stop,
            })
        }
        Err(error) => {
            let message = error.error().to_string();
            tracing::warn!(%error, "Tasci context compaction failed");
            write_harness_event(
                output,
                TasciHarnessEvent::TurnFinished {
                    error: Some(message),
                    cancelled: false,
                },
            )
            .await?;
            Ok(HarnessTurn {
                session: fallback_session,
                stop,
            })
        }
    }
}

/// Opens the ephemeral, new, or restored native session requested at startup.
async fn initialize_harness_session(
    configuration: &TasciHarnessConfiguration,
    requested_session: Option<TasciHarnessSession>,
) -> TasciExecResult<(Option<AgentSessionStore>, AgentSession)> {
    let Some(requested_session) = requested_session else {
        return Ok((None, AgentSession::new()));
    };
    let directory = session_directory()?;
    match requested_session {
        TasciHarnessSession::Create { session_id } => {
            let store =
                AgentSessionStore::create(directory, &session_id, &configuration.working_directory)
                    .await
                    .map_err(|error| error.escalate(TasciExecError::SessionStorage))?;
            Ok((Some(store), AgentSession::new()))
        }
        TasciHarnessSession::Resume { session_id } => {
            let (store, session) =
                AgentSessionStore::open(directory, &session_id, &configuration.working_directory)
                    .await
                    .map_err(|error| error.escalate(TasciExecError::SessionStorage))?;
            Ok((Some(store), session))
        }
    }
}

/// Announces readiness and any optional integrations unavailable at startup.
async fn announce_harness_started(
    output: &mut tokio::io::Stdout,
    model: &str,
    warnings: &[String],
) -> TasciExecResult<()> {
    tracing::info!(model, "Tasci harness started");
    write_harness_event(output, TasciHarnessEvent::Started).await?;
    for warning in warnings {
        write_harness_event(
            output,
            TasciHarnessEvent::Warning {
                code: "mcp_server_unavailable".to_owned(),
                message: warning.clone(),
            },
        )
        .await?;
    }
    Ok(())
}

/// Reports session initialization failures through the harness protocol.
async fn initialize_protocol_session(
    configuration: &TasciHarnessConfiguration,
    requested_session: Option<TasciHarnessSession>,
    output: &mut tokio::io::Stdout,
) -> TasciExecResult<Option<(Option<AgentSessionStore>, AgentSession)>> {
    match initialize_harness_session(configuration, requested_session).await {
        Ok(session) => Ok(Some(session)),
        Err(error) => {
            tracing::warn!(%error, "Tasci session initialization failed");
            write_harness_event(
                output,
                TasciHarnessEvent::Failed {
                    message: error.error().to_string(),
                },
            )
            .await?;
            Ok(None)
        }
    }
}

/// Resolves the configured session root or its pod-home default.
fn session_directory() -> TasciExecResult<PathBuf> {
    let directory = env::var_os(TASCI_SESSION_DIRECTORY_ENVIRONMENT)
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".tasci/sessions")))
        .ok_or_else(|| TasciExecError::MissingSessionDirectory.report())?;
    if !directory.is_absolute() {
        return Err(TasciExecError::RelativeSessionDirectory { path: directory }.report());
    }
    Ok(directory)
}

/// Commits a successful operation before acknowledging it to the caller.
async fn finish_successful_harness_operation(
    session_store: &mut Option<AgentSessionStore>,
    session: AgentSession,
    fallback_session: AgentSession,
    stop: bool,
    output: &mut tokio::io::Stdout,
    operation: &'static str,
) -> TasciExecResult<HarnessTurn> {
    if let Some(store) = session_store
        && let Err(error) = store.persist(&session).await
    {
        let error = error.escalate(TasciExecError::SessionStorage);
        tracing::warn!(%error, operation, "Tasci session persistence failed");
        write_harness_event(
            output,
            TasciHarnessEvent::TurnFinished {
                error: Some(error.error().to_string()),
                cancelled: false,
            },
        )
        .await?;
        return Ok(HarnessTurn {
            session: fallback_session,
            stop: true,
        });
    }
    tracing::info!(operation, "Tasci {operation} completed");
    write_harness_event(
        output,
        TasciHarnessEvent::TurnFinished {
            error: None,
            cancelled: false,
        },
    )
    .await?;
    Ok(HarnessTurn { session, stop })
}

async fn read_harness_command(
    input: &mut tokio::io::Lines<BufReader<tokio::io::Stdin>>,
) -> TasciExecResult<TasciHarnessCommand> {
    let line = input
        .next_line()
        .await
        .escalate(TasciExecError::ProtocolRead)?
        .ok_or_else(|| {
            TasciExecError::ProtocolInput {
                message: "Tasci harness input closed".to_owned(),
            }
            .report()
        })?;
    serde_json::from_str(&line).map_err(|error| {
        TasciExecError::ProtocolInput {
            message: error.to_string(),
        }
        .report()
    })
}

async fn write_harness_event(
    output: &mut tokio::io::Stdout,
    event: TasciHarnessEvent,
) -> TasciExecResult<()> {
    let mut encoded = serde_json::to_vec(&event)
        .map_err(|source| TasciExecError::ProtocolEncode { source }.report())?;
    encoded.push(b'\n');
    output
        .write_all(&encoded)
        .await
        .escalate(TasciExecError::ProtocolWrite)?;
    output.flush().await.escalate(TasciExecError::ProtocolWrite)
}

struct AgentRuntime {
    working_directory: String,
    mcp_servers: Vec<McpServerConfiguration>,
    files: Arc<FileWorkspace>,
    tools: ToolRegistry,
    _mcp_clients: Vec<McpClient>,
    warnings: Vec<String>,
}

#[tracing::instrument(level = "debug", skip_all)]
async fn open_agent_runtime(
    configuration: &TasciHarnessConfiguration,
) -> TasciExecResult<AgentRuntime> {
    let workspace = std::path::Path::new(&configuration.working_directory);
    let files = Arc::new(
        FileWorkspace::open(workspace)
            .await
            .map_err(|error| error.escalate(TasciExecError::FileWorkspace))?,
    );
    let processes = Arc::new(
        ProcessRuntime::open(workspace)
            .await
            .map_err(|error| error.escalate(TasciExecError::ProcessRuntime))?,
    );
    let mut tools = tools(processes)?;
    let mut mcp_clients = Vec::new();
    let mut warnings = Vec::new();
    for server in &configuration.mcp_servers {
        let client_config = match McpClientConfig::new(
            server.name.clone(),
            server.display_name.clone(),
            &server.endpoint,
            server.headers.clone(),
        ) {
            Ok(config) => config,
            Err(error) => {
                let message = error.error().to_string();
                tracing::warn!(
                    server = %server.name,
                    %error,
                    "failed to configure MCP server"
                );
                warnings.push(format!(
                    "{} MCP tools are unavailable: {message}",
                    server.display_name
                ));
                continue;
            }
        };
        match McpClient::connect(client_config).await {
            Ok(client) => {
                let discovered_tools = client.tools();
                let tool_count = discovered_tools.len();
                for tool in discovered_tools {
                    tools
                        .register(tool)
                        .map_err(|error| error.escalate(TasciExecError::Tools))?;
                }
                tracing::info!(
                    server = %server.name,
                    tools = tool_count,
                    "MCP server connected"
                );
                mcp_clients.push(client);
            }
            Err(error) => {
                let message = error.error().to_string();
                tracing::warn!(
                    server = %server.name,
                    %error,
                    "failed to connect MCP server"
                );
                warnings.push(format!(
                    "{} MCP tools are unavailable: {message}",
                    server.display_name
                ));
            }
        }
    }
    Ok(AgentRuntime {
        working_directory: configuration.working_directory.clone(),
        mcp_servers: configuration.mcp_servers.clone(),
        files,
        tools,
        _mcp_clients: mcp_clients,
        warnings,
    })
}

fn build_agent(
    configuration: &TasciHarnessConfiguration,
    runtime: &AgentRuntime,
) -> TasciExecResult<Agent> {
    let model = Arc::new(
        OpenAiChatBackend::new(
            &configuration.base_url,
            &configuration.model,
            configuration.authorization.clone(),
        )
        .map_err(|error| error.escalate(TasciExecError::Model))?,
    );
    Ok(Agent::new(
        model,
        runtime.tools.clone(),
        Arc::clone(&runtime.files),
        AgentConfig {
            context_window: configuration.context_window,
            max_output_tokens: configuration.max_output_tokens,
            ..AgentConfig::default()
        },
    ))
}

/// Registers the complete standalone Tasci tool set.
fn tools(processes: Arc<ProcessRuntime>) -> TasciExecResult<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    tools
        .register(BashTool::new(Arc::clone(&processes)))
        .map_err(|error| error.escalate(TasciExecError::Tools))?;
    tools
        .register(ReadTool::default())
        .map_err(|error| error.escalate(TasciExecError::Tools))?;
    tools
        .register(WriteTool)
        .map_err(|error| error.escalate(TasciExecError::Tools))?;
    tools
        .register(EditTool)
        .map_err(|error| error.escalate(TasciExecError::Tools))?;
    tools
        .register(ProcessTool::new(processes))
        .map_err(|error| error.escalate(TasciExecError::Tools))?;
    Ok(tools)
}

/// Stateful terminal projection of live agent events.
#[derive(Default)]
struct EventPrinter {
    started: bool,
    line_open: bool,
    error: Option<io::Error>,
}

impl EventPrinter {
    fn handle(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ModelRequestStarted { step } => {
                self.ensure_newline();
                if self.started {
                    self.write(format_args!("\n"));
                }
                self.started = true;
                self.write(format_args!("[model {}]\n", step + 1));
            }
            AgentEvent::ModelRequestRetrying {
                step,
                attempt,
                delay_ms,
            } => {
                self.ensure_newline();
                self.write(format_args!(
                    "[model {} interrupted; retrying attempt {attempt} in {delay_ms} ms]\n",
                    step + 1
                ));
            }
            AgentEvent::ContextCompactionStarted { reason } => {
                self.ensure_newline();
                self.write(format_args!("[context compaction {reason:?}]\n"));
            }
            AgentEvent::ContextCompactionCompleted {
                tokens_before,
                estimated_tokens_after,
                ..
            } => {
                self.ensure_newline();
                self.write(format_args!(
                    "[context compacted: {tokens_before} -> about {estimated_tokens_after} tokens]\n"
                ));
            }
            AgentEvent::ContextCompactionFailed {
                reason, message, ..
            } => {
                self.ensure_newline();
                self.write(format_args!(
                    "[context compaction {reason:?} failed: {message}]\n"
                ));
            }
            AgentEvent::TextDelta { delta } => {
                self.write(format_args!("{delta}"));
                self.line_open = !delta.ends_with('\n');
            }
            AgentEvent::ToolExecutionStarted {
                name, arguments, ..
            } => {
                self.ensure_newline();
                self.write(format_args!("[tool {name}] {arguments}\n"));
            }
            AgentEvent::ToolExecutionCompleted {
                name,
                content,
                artifacts,
                is_error,
                ..
            } => {
                let status = if *is_error { "error" } else { "ok" };
                self.ensure_newline();
                self.write(format_args!("[tool {name} {status}]\n{content}"));
                self.ensure_newline();
                self.write_artifacts(artifacts);
            }
            AgentEvent::Completed { .. } => self.ensure_newline(),
            AgentEvent::ModelUsage { .. }
            | AgentEvent::ContextUsageUpdated { .. }
            | AgentEvent::ReasoningDelta { .. }
            | AgentEvent::ToolCallStarted { .. }
            | AgentEvent::ToolCallCompleted { .. } => {}
        }
    }

    fn finish(&mut self) {
        self.ensure_newline();
    }

    fn take_error(&mut self) -> Option<io::Error> {
        self.error.take()
    }

    fn write_artifacts(&mut self, artifacts: &[ToolArtifact]) {
        for artifact in artifacts {
            match artifact {
                ToolArtifact::FileChanges { changes } => {
                    for change in changes {
                        self.write(format_args!("{}\n", change.unified_diff));
                        self.line_open = false;
                    }
                }
            }
        }
    }

    fn ensure_newline(&mut self) {
        if self.line_open {
            self.write(format_args!("\n"));
            self.line_open = false;
        }
    }

    fn write(&mut self, arguments: fmt::Arguments<'_>) {
        if self.error.is_some() {
            return;
        }
        let mut output = io::stdout().lock();
        if let Err(error) = output.write_fmt(arguments).and_then(|()| output.flush()) {
            self.error = Some(error);
        }
    }
}

type TasciExecResult<T> = Result<T, Report<TasciExecError>>;

/// Failure while preparing or running the standalone local agent.
#[derive(Debug, Error)]
enum TasciExecError {
    /// The required prompt argument was absent.
    #[error("missing prompt; usage: tasci-exec '<prompt>'")]
    MissingPrompt,
    /// Process-wide tracing could not be initialized.
    #[error("failed to initialize tasci-exec logging")]
    Logging {
        /// Subscriber installation failure reported by tracing.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The current workspace directory could not be determined.
    #[error("failed to determine the current workspace directory")]
    CurrentDirectory,
    /// The revision-aware file workspace could not be opened.
    #[error("failed to open the Tasci file workspace")]
    FileWorkspace,
    /// The local process supervisor could not be opened.
    #[error("failed to open the Tasci process runtime")]
    ProcessRuntime,
    /// The local model backend could not be created.
    #[error("failed to configure the local llama.cpp model")]
    Model,
    /// A built-in tool could not be registered.
    #[error("failed to register the Tasci tools")]
    Tools,
    /// The agentic loop failed.
    #[error("Tasci agent run failed")]
    Agent,
    /// A persistent session was requested without a usable storage root.
    #[error("Tasci session storage requires HOME or TASCI_SESSION_DIR")]
    MissingSessionDirectory,
    /// A configured session root could escape the harness account's storage.
    #[error("Tasci session storage directory must be absolute: {path}")]
    RelativeSessionDirectory {
        /// Rejected session root.
        path: PathBuf,
    },
    /// A native session journal could not be opened or extended.
    #[error("failed to access the Tasci session journal")]
    SessionStorage,
    /// Interrupt handling failed.
    #[error("failed to wait for an interrupt")]
    Interrupt,
    /// Harness input could not be read.
    #[error("failed to read the Tasci harness protocol")]
    ProtocolRead,
    /// Harness input violated the line protocol.
    #[error("invalid Tasci harness protocol input: {message}")]
    ProtocolInput {
        /// Secret-safe decoding or ordering failure.
        message: String,
    },
    /// A harness event could not be encoded.
    #[error("failed to encode the Tasci harness protocol")]
    ProtocolEncode {
        /// JSON serialization failure.
        #[source]
        source: serde_json::Error,
    },
    /// Harness output could not be written.
    #[error("failed to write the Tasci harness protocol")]
    ProtocolWrite,
    /// Live agent output could not be written.
    #[error("failed to write Tasci output")]
    Output {
        /// Standard-output failure.
        #[source]
        source: io::Error,
    },
}
