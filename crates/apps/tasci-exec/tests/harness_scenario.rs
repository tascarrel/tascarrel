use std::convert::Infallible;
use std::process::Stdio;

use bytes::Bytes;
use http_body_util::BodyExt as _;
use http_body_util::Full;
use hyper::Request;
use hyper::Response;
use hyper::body::Incoming;
use hyper::header::CONNECTION;
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tascarrel_agent::AgentEvent;
use tascarrel_agent::TasciHarnessCommand;
use tascarrel_agent::TasciHarnessConfiguration;
use tascarrel_agent::TasciHarnessEvent;
use tempfile::tempdir;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufReader;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Deserialize)]
struct ObservedChatRequest {
    model: String,
    messages: Vec<ObservedChatMessage>,
}

#[derive(Deserialize)]
struct ObservedChatMessage {
    role: String,
    content: String,
}

/// Exercises a complete harness session with a model change between turns.
#[tokio::test]
async fn harness_scenario_switches_models_and_retains_context_across_turns() {
    let (base_url, mut requests, server) =
        serve_model_scenario(["The first answer.", "The second answer."]).await;
    let workspace = tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tasci-exec"))
        .arg("--harness")
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stderr = child.stderr.take().unwrap();
    let stderr_reader = tokio::spawn(async move {
        let mut logs = String::new();
        stderr.read_to_string(&mut logs).await.unwrap();
        logs
    });

    send_command(
        &mut input,
        TasciHarnessCommand::Start {
            configuration: TasciHarnessConfiguration {
                base_url: format!("{base_url}/v1"),
                model: "scenario-model".to_owned(),
                context_window: None,
                max_output_tokens: None,
                authorization: None,
                working_directory: workspace.path().to_string_lossy().into_owned(),
                mcp_servers: Vec::new(),
            },
        },
    )
    .await;
    assert_eq!(read_event(&mut output).await, TasciHarnessEvent::Started);

    send_command(
        &mut input,
        TasciHarnessCommand::Prompt {
            prompt: "First question.".to_owned(),
            configuration: None,
        },
    )
    .await;
    assert_eq!(
        read_turn(&mut output).await,
        vec!["The first answer.".to_owned()]
    );

    send_command(
        &mut input,
        TasciHarnessCommand::Prompt {
            prompt: "Second question.".to_owned(),
            configuration: Some(TasciHarnessConfiguration {
                base_url: format!("{base_url}/v1"),
                model: "scenario-model-two".to_owned(),
                context_window: None,
                max_output_tokens: None,
                authorization: None,
                working_directory: workspace.path().to_string_lossy().into_owned(),
                mcp_servers: Vec::new(),
            }),
        },
    )
    .await;
    assert_eq!(
        read_turn(&mut output).await,
        vec!["The second answer.".to_owned()]
    );

    send_command(&mut input, TasciHarnessCommand::Stop).await;
    assert_eq!(read_event(&mut output).await, TasciHarnessEvent::Stopped);
    assert!(child.wait().await.unwrap().success());
    let logs = stderr_reader.await.unwrap();
    assert!(logs.contains("Tasci harness started"));
    assert!(logs.contains("Tasci turn started"));
    assert!(logs.contains("Tasci turn completed"));
    assert!(logs.contains("Tasci harness stopped"));

    let first_request = requests.recv().await.unwrap();
    let second_request = requests.recv().await.unwrap();
    assert_eq!(first_request.model, "scenario-model");
    assert_eq!(second_request.model, "scenario-model-two");
    assert_eq!(
        message_projection(&first_request),
        vec![
            ("system", first_request.messages[0].content.as_str()),
            ("user", "First question."),
        ]
    );
    assert_eq!(
        message_projection(&second_request),
        vec![
            ("system", second_request.messages[0].content.as_str()),
            ("user", "First question."),
            ("assistant", "The first answer."),
            ("user", "Second question."),
        ]
    );
    server.await.unwrap();
}

/// Exercises propagation and logging of a provider completion containing no
/// model output.
#[tokio::test]
async fn harness_scenario_reports_an_empty_model_response() {
    let (base_url, _requests, server) = serve_model_scenario([""]).await;
    let workspace = tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tasci-exec"))
        .arg("--harness")
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stderr = child.stderr.take().unwrap();
    let stderr_reader = tokio::spawn(async move {
        let mut logs = String::new();
        stderr.read_to_string(&mut logs).await.unwrap();
        logs
    });

    send_command(
        &mut input,
        TasciHarnessCommand::Start {
            configuration: TasciHarnessConfiguration {
                base_url: format!("{base_url}/v1"),
                model: "empty-model".to_owned(),
                context_window: None,
                max_output_tokens: None,
                authorization: None,
                working_directory: workspace.path().to_string_lossy().into_owned(),
                mcp_servers: Vec::new(),
            },
        },
    )
    .await;
    assert_eq!(read_event(&mut output).await, TasciHarnessEvent::Started);

    send_command(
        &mut input,
        TasciHarnessCommand::Prompt {
            prompt: "Return a response.".to_owned(),
            configuration: None,
        },
    )
    .await;
    loop {
        match read_event(&mut output).await {
            TasciHarnessEvent::TurnFinished {
                error: Some(error),
                cancelled: false,
            } => {
                assert!(error.contains("no text or tool calls"));
                break;
            }
            TasciHarnessEvent::Agent { .. } => {}
            event => panic!("unexpected harness event: {event:?}"),
        }
    }

    send_command(&mut input, TasciHarnessCommand::Stop).await;
    assert_eq!(read_event(&mut output).await, TasciHarnessEvent::Stopped);
    assert!(child.wait().await.unwrap().success());
    let logs = stderr_reader.await.unwrap();
    assert!(logs.contains("Tasci turn failed"));
    assert!(logs.contains("no text or tool calls"));
    server.await.unwrap();
}

/// Exercises manual compaction, split-turn summaries, and subsequent context
/// reconstruction through the complete harness protocol.
#[tokio::test]
async fn harness_scenario_compacts_context_and_continues_from_the_checkpoint() {
    let (base_url, requests, server) = serve_owned_model_scenario(compaction_responses()).await;
    let workspace = tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tasci-exec"))
        .arg("--harness")
        .current_dir(workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();

    send_command(
        &mut input,
        TasciHarnessCommand::Start {
            configuration: TasciHarnessConfiguration {
                base_url: format!("{base_url}/v1"),
                model: "compaction-model".to_owned(),
                context_window: None,
                max_output_tokens: Some(2_000),
                authorization: None,
                working_directory: workspace.path().to_string_lossy().into_owned(),
                mcp_servers: Vec::new(),
            },
        },
    )
    .await;
    assert_eq!(read_event(&mut output).await, TasciHarnessEvent::Started);

    for index in 1..=9 {
        send_command(
            &mut input,
            TasciHarnessCommand::Prompt {
                prompt: format!("Question {index}."),
                configuration: None,
            },
        )
        .await;
        read_turn(&mut output).await;
    }

    send_command(&mut input, TasciHarnessCommand::Compact).await;
    let mut completed_compaction = None;
    loop {
        match read_event(&mut output).await {
            TasciHarnessEvent::Agent {
                value:
                    AgentEvent::ContextCompactionCompleted {
                        tokens_before,
                        estimated_tokens_after,
                        ..
                    },
            } => completed_compaction = Some((tokens_before, estimated_tokens_after)),
            TasciHarnessEvent::Agent { .. } => {}
            TasciHarnessEvent::TurnFinished {
                error: None,
                cancelled: false,
            } => break,
            event => panic!("unexpected compaction event: {event:?}"),
        }
    }
    let (tokens_before, tokens_after) = completed_compaction.unwrap();
    assert!(tokens_after < tokens_before);

    send_command(
        &mut input,
        TasciHarnessCommand::Prompt {
            prompt: "Continue now.".to_owned(),
            configuration: None,
        },
    )
    .await;
    assert_eq!(
        read_turn(&mut output).await,
        vec!["Continued after compaction.".to_owned()]
    );
    send_command(&mut input, TasciHarnessCommand::Stop).await;
    assert_eq!(read_event(&mut output).await, TasciHarnessEvent::Stopped);
    assert!(child.wait().await.unwrap().success());

    assert_compaction_requests(requests);
    server.await.unwrap();
}

fn compaction_responses() -> Vec<String> {
    let mut responses = vec!["old implementation detail ".repeat(600); 9];
    responses.push(
        "## Goal\nContinue the task.\n\n## Constraints & Preferences\n- (none)\n\n\
         ## Progress\n### Done\n- [x] Reviewed old work.\n\n### In Progress\n- [ ] Continue.\n\n\
         ### Blocked\n- (none)\n\n## Key Decisions\n- **Keep context**: Preserve the plan.\n\n\
         ## Next Steps\n1. Continue.\n\n## Critical Context\n- checkpoint marker"
            .to_owned(),
    );
    responses.push(
        "## Original Request\nContinue the old turn.\n\n## Early Progress\n- Started it.\n\n\
         ## Context for Suffix\n- Retained work follows."
            .to_owned(),
    );
    responses.push("Continued after compaction.".to_owned());
    responses
}

fn assert_compaction_requests(mut requests: mpsc::UnboundedReceiver<ObservedChatRequest>) {
    let mut observed = Vec::new();
    while let Ok(request) = requests.try_recv() {
        observed.push(request);
    }
    assert_eq!(observed.len(), 12);
    let history_summary = &observed[9];
    assert!(
        history_summary.messages[0]
            .content
            .contains("context summarization assistant")
    );
    assert!(
        history_summary.messages[1]
            .content
            .contains("<conversation>")
    );
    let continuation = &observed[11];
    assert_eq!(continuation.messages[0].role, "system");
    assert!(
        continuation.messages[1]
            .content
            .contains("<context-summary>")
    );
    assert!(
        continuation.messages[1]
            .content
            .contains("checkpoint marker")
    );
    assert!(
        continuation
            .messages
            .iter()
            .all(|message| !message.content.contains("Question 1."))
    );
    assert!(
        continuation
            .messages
            .iter()
            .any(|message| message.content == "Question 9.")
    );
    assert_eq!(
        continuation.messages.last().unwrap().content,
        "Continue now."
    );
}

async fn send_command(input: &mut tokio::process::ChildStdin, command: TasciHarnessCommand) {
    let mut line = serde_json::to_vec(&command).unwrap();
    line.push(b'\n');
    input.write_all(&line).await.unwrap();
    input.flush().await.unwrap();
}

async fn read_event(
    output: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> TasciHarnessEvent {
    let line = output.next_line().await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn read_turn(
    output: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> Vec<String> {
    let mut text = Vec::new();
    loop {
        match read_event(output).await {
            TasciHarnessEvent::Agent {
                value: AgentEvent::TextDelta { delta },
            } => text.push(delta),
            TasciHarnessEvent::Agent { .. } => {}
            TasciHarnessEvent::TurnFinished {
                error: None,
                cancelled: false,
            } => return text,
            event => panic!("unexpected harness event: {event:?}"),
        }
    }
}

fn message_projection(request: &ObservedChatRequest) -> Vec<(&str, &str)> {
    request
        .messages
        .iter()
        .map(|message| (message.role.as_str(), message.content.as_str()))
        .collect()
}

async fn serve_model_scenario<const N: usize>(
    responses: [&'static str; N],
) -> (
    String,
    mpsc::UnboundedReceiver<ObservedChatRequest>,
    tokio::task::JoinHandle<()>,
) {
    serve_owned_model_scenario(responses.into_iter().map(str::to_owned).collect()).await
}

async fn serve_owned_model_scenario(
    responses: Vec<String>,
) -> (
    String,
    mpsc::UnboundedReceiver<ObservedChatRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_sender, request_receiver) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for response_text in responses {
            let (socket, _) = listener.accept().await.unwrap();
            let request_sender = request_sender.clone();
            let service = service_fn(move |request: Request<Incoming>| {
                let request_sender = request_sender.clone();
                let response_text = response_text.clone();
                async move {
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    request_sender
                        .send(serde_json::from_slice::<ObservedChatRequest>(&body).unwrap())
                        .unwrap();
                    let response_body = format!(
                        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n",
                        serde_json::to_string(&response_text).unwrap()
                    );
                    Ok::<_, Infallible>(
                        Response::builder()
                            .header(CONTENT_TYPE, "text/event-stream")
                            .header(CONNECTION, "close")
                            .body(Full::new(Bytes::from(response_body)))
                            .unwrap(),
                    )
                }
            });
            http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .await
                .unwrap();
        }
    });
    (format!("http://{address}"), request_receiver, server)
}
