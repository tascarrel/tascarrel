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

    send_command(
        &mut input,
        TasciHarnessCommand::Start {
            configuration: TasciHarnessConfiguration {
                base_url: format!("{base_url}/v1"),
                model: "scenario-model".to_owned(),
                authorization: None,
                working_directory: workspace.path().to_string_lossy().into_owned(),
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
                authorization: None,
                working_directory: workspace.path().to_string_lossy().into_owned(),
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_sender, request_receiver) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for response_text in responses {
            let (socket, _) = listener.accept().await.unwrap();
            let request_sender = request_sender.clone();
            let service = service_fn(move |request: Request<Incoming>| {
                let request_sender = request_sender.clone();
                async move {
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    request_sender
                        .send(serde_json::from_slice::<ObservedChatRequest>(&body).unwrap())
                        .unwrap();
                    let response_body = format!(
                        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\ndata: [DONE]\n\n",
                        serde_json::to_string(response_text).unwrap()
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
