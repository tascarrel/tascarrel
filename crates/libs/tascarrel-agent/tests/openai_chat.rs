use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt as _;
use http_body_util::BodyExt as _;
use http_body_util::Full;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::header::CONNECTION;
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tascarrel_agent::AssistantMessage;
use tascarrel_agent::FinishReason;
use tascarrel_agent::HttpAuthorization;
use tascarrel_agent::ModelBackend;
use tascarrel_agent::ModelError;
use tascarrel_agent::ModelMessage;
use tascarrel_agent::ModelRequest;
use tascarrel_agent::ModelStreamEvent;
use tascarrel_agent::OpenAiChatBackend;
use tascarrel_agent::ToolDefinition;
use tascarrel_agent::ToolPrompt;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

// Exercises typed request encoding and fragmented OpenAI-compatible streaming
// tool calls.
#[tokio::test]
async fn openai_chat_transport_normalizes_tool_calls() {
    let response = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"src/lib.rs\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, observed_request, server) = serve_once(response).await;
    let backend = OpenAiChatBackend::new(
        &format!("{base_url}/v1"),
        "test-model",
        Some(HttpAuthorization::bearer("test-token")),
    )
    .unwrap();
    let stream = backend
        .stream(
            ModelRequest {
                messages: vec![
                    ModelMessage::System {
                        content: "Use the read tool.".to_owned(),
                    },
                    ModelMessage::User {
                        content: "Read the source.".to_owned(),
                    },
                ],
                tools: vec![ToolDefinition {
                    name: "read".to_owned(),
                    description: "Read one file.".to_owned(),
                    input_schema: r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#.to_owned(),
                    prompt: ToolPrompt::default(),
                }],
                max_output_tokens: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = stream.map(|event| event.unwrap()).collect::<Vec<_>>().await;

    assert_eq!(
        events,
        vec![
            ModelStreamEvent::ToolCallStarted {
                id: "call-1".to_owned(),
                name: "read".to_owned(),
            },
            ModelStreamEvent::ToolCallArgumentsDelta {
                id: "call-1".to_owned(),
                delta: r#"{"pa"#.to_owned(),
            },
            ModelStreamEvent::ToolCallArgumentsDelta {
                id: "call-1".to_owned(),
                delta: r#"th":"src/lib.rs"}"#.to_owned(),
            },
            ModelStreamEvent::ToolCallCompleted {
                id: "call-1".to_owned(),
            },
            ModelStreamEvent::Completed {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    );

    let observed = observed_request.await.unwrap();
    assert_eq!(observed.path, "/v1/chat/completions");
    assert_eq!(observed.authorization.as_deref(), Some("Bearer test-token"));
    let request: ObservedChatRequest = serde_json::from_slice(&observed.body).unwrap();
    assert_eq!(request.model, "test-model");
    assert!(request.stream);
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.messages[0].role, "system");
    assert_eq!(request.messages[0].content, "Use the read tool.");
    assert_eq!(request.messages[1].role, "user");
    assert_eq!(request.messages[1].content, "Read the source.");
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].kind, "function");
    assert_eq!(request.tools[0].function.name, "read");
    server.await.unwrap();
}

// Exercises classification of an HTTP response that closes after partial text
// without a terminal completion chunk.
#[tokio::test]
async fn interrupted_openai_stream_is_a_transport_failure() {
    let response = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
    let (base_url, observed_request, server) = serve_once(response).await;
    let backend = OpenAiChatBackend::new(&format!("{base_url}/v1"), "test-model", None).unwrap();
    let mut stream = backend
        .stream(
            ModelRequest {
                messages: vec![ModelMessage::User {
                    content: "Respond completely.".to_owned(),
                }],
                tools: Vec::new(),
                max_output_tokens: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        ModelStreamEvent::TextDelta {
            delta: "partial".to_owned(),
        }
    );
    let error = stream.next().await.unwrap().unwrap_err();
    assert!(matches!(error.error(), ModelError::Transport { .. }));
    assert!(error.error().to_string().contains("before a finish reason"));
    assert!(stream.next().await.is_none());

    observed_request.await.unwrap();
    server.await.unwrap();
}

// Exercises reasoning, nullable OpenAI-compatible delta fields, and a
// usage-only chunk between the finish reason and terminal marker.
#[tokio::test]
async fn openai_compatible_stream_accepts_nullable_fields_and_trailing_usage() {
    let response = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"current reasoning\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"complete\",\"tool_calls\":null},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":null,\"tool_calls\":null},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":null,\"tool_calls\":null},\"finish_reason\":null}],\"usage\":{\"completion_tokens\":1}}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, observed_request, server) = serve_once(response).await;
    let backend = OpenAiChatBackend::new(&format!("{base_url}/v1"), "test-model", None).unwrap();
    let stream = backend
        .stream(
            ModelRequest {
                messages: vec![
                    ModelMessage::Assistant(AssistantMessage {
                        reasoning: "prior reasoning".to_owned(),
                        content: "Prior answer.".to_owned(),
                        tool_calls: Vec::new(),
                        usage: None,
                    }),
                    ModelMessage::User {
                        content: "Respond completely.".to_owned(),
                    },
                ],
                tools: Vec::new(),
                max_output_tokens: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        stream.map(|event| event.unwrap()).collect::<Vec<_>>().await,
        vec![
            ModelStreamEvent::ReasoningDelta {
                delta: "current reasoning".to_owned(),
            },
            ModelStreamEvent::TextDelta {
                delta: "complete".to_owned(),
            },
            ModelStreamEvent::Completed {
                finish_reason: FinishReason::Stop,
            },
            ModelStreamEvent::Usage {
                usage: tascarrel_agent::ModelUsage {
                    input_tokens: 0,
                    output_tokens: 1,
                    cache_read_input_tokens: None,
                    reasoning_output_tokens: None,
                },
            },
        ]
    );
    let observed = observed_request.await.unwrap();
    let request: ObservedChatRequest = serde_json::from_slice(&observed.body).unwrap();
    assert_eq!(
        request.messages[0].reasoning_content.as_deref(),
        Some("prior reasoning")
    );
    server.await.unwrap();
}

// Exercises the llama.cpp-compatible terminal marker used when the final
// choice omits a finish reason.
#[tokio::test]
async fn done_marker_completes_a_stream_without_a_finish_reason() {
    let response = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"complete\"},\"finish_reason\":null}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, observed_request, server) = serve_once(response).await;
    let backend = OpenAiChatBackend::new(&format!("{base_url}/v1"), "test-model", None).unwrap();
    let stream = backend
        .stream(
            ModelRequest {
                messages: vec![ModelMessage::User {
                    content: "Respond completely.".to_owned(),
                }],
                tools: Vec::new(),
                max_output_tokens: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        stream.map(|event| event.unwrap()).collect::<Vec<_>>().await,
        vec![
            ModelStreamEvent::TextDelta {
                delta: "complete".to_owned(),
            },
            ModelStreamEvent::Completed {
                finish_reason: FinishReason::Stop,
            },
        ]
    );
    observed_request.await.unwrap();
    server.await.unwrap();
}

// Exercises secret-safe validation for provider URLs.
#[test]
fn provider_url_rejects_embedded_credentials() {
    let result = OpenAiChatBackend::new("https://user:password@example.com/v1", "test-model", None);
    let Err(error) = result else {
        panic!("credential-bearing provider URL should be rejected");
    };
    assert!(
        error
            .error()
            .to_string()
            .contains("must not contain credentials")
    );
    assert!(!error.error().to_string().contains("password"));
}

/// Exercises context-overflow classification without exposing the provider's
/// response body in the public error.
#[tokio::test]
async fn context_length_http_error_is_classified_for_compaction_recovery() {
    let body = r#"{"error":{"message":"maximum context length exceeded"}}"#;
    let (base_url, observed_request, server) =
        serve_once_with_status(StatusCode::BAD_REQUEST, body).await;
    let backend = OpenAiChatBackend::new(&format!("{base_url}/v1"), "test-model", None).unwrap();
    let result = backend
        .stream(
            ModelRequest {
                messages: vec![ModelMessage::User {
                    content: "oversized".to_owned(),
                }],
                tools: Vec::new(),
                max_output_tokens: None,
            },
            CancellationToken::new(),
        )
        .await;
    let Err(error) = result else {
        panic!("context overflow should reject the request");
    };

    assert!(matches!(error.error(), ModelError::ContextOverflow));
    assert!(!error.error().to_string().contains("maximum context"));
    observed_request.await.unwrap();
    server.await.unwrap();
}

async fn serve_once(
    response_body: &'static str,
) -> (
    String,
    oneshot::Receiver<ObservedHttpRequest>,
    tokio::task::JoinHandle<()>,
) {
    serve_once_with_status(StatusCode::OK, response_body).await
}

async fn serve_once_with_status(
    status: StatusCode,
    response_body: &'static str,
) -> (
    String,
    oneshot::Receiver<ObservedHttpRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_sender, request_receiver) = oneshot::channel();
    let request_sender = Arc::new(std::sync::Mutex::new(Some(request_sender)));
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let service = service_fn(move |request: Request<Incoming>| {
            let request_sender = Arc::clone(&request_sender);
            async move {
                let path = request.uri().path().to_owned();
                let authorization = request
                    .headers()
                    .get(hyper::header::AUTHORIZATION)
                    .map(|value| value.to_str().unwrap().to_owned());
                let body = request.into_body().collect().await.unwrap().to_bytes();
                request_sender
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(ObservedHttpRequest {
                        path,
                        authorization,
                        body: body.to_vec(),
                    })
                    .unwrap();
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(status)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .header(CONNECTION, "close")
                        .body(Full::new(Bytes::from_static(response_body.as_bytes())))
                        .unwrap(),
                )
            }
        });
        http1::Builder::new()
            .serve_connection(TokioIo::new(socket), service)
            .await
            .unwrap();
    });
    (format!("http://{address}"), request_receiver, server)
}

#[derive(Debug)]
struct ObservedHttpRequest {
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

#[derive(Deserialize)]
struct ObservedChatRequest {
    model: String,
    messages: Vec<ObservedMessage>,
    #[serde(default)]
    tools: Vec<ObservedTool>,
    stream: bool,
}

#[derive(Deserialize)]
struct ObservedMessage {
    role: String,
    content: String,
    reasoning_content: Option<String>,
}

#[derive(Deserialize)]
struct ObservedTool {
    #[serde(rename = "type")]
    kind: String,
    function: ObservedFunction,
}

#[derive(Deserialize)]
struct ObservedFunction {
    name: String,
}
