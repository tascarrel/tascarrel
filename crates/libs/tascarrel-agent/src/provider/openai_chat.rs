//! `OpenAI` Chat Completions transport for caller-configured endpoints.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::pin::Pin;

use eventsource_stream::Event;
use eventsource_stream::EventStreamError;
use eventsource_stream::Eventsource as _;
use futures_core::Stream;
use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use futures_util::stream;
use reportify::Report;
use reqwest::Url;
use reqwest::header::ACCEPT;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use serde::Deserialize;
use serde::Serialize;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::FinishReason;
use crate::ModelBackend;
use crate::ModelError;
use crate::ModelEventStream;
use crate::ModelMessage;
use crate::ModelRequest;
use crate::ModelResult;
use crate::ModelStreamEvent;

/// `OpenAI` Chat Completions backend with normalized streaming tool calls.
pub struct OpenAiChatBackend {
    client: reqwest::Client,
    endpoint: Url,
    model: String,
    authorization: Option<(HeaderName, HeaderValue)>,
}

impl OpenAiChatBackend {
    /// Creates a backend for a custom compatible endpoint.
    ///
    /// An absent token supports local APIs that do not authenticate requests.
    ///
    /// # Errors
    ///
    /// Returns an error when the base URL, model, or bearer token is invalid.
    pub fn new(
        base_url: &str,
        model: impl Into<String>,
        authorization: Option<HttpAuthorization>,
    ) -> ModelResult<Self> {
        Self::with_client(reqwest::Client::new(), base_url, model, authorization)
    }

    /// Creates a backend with a caller-owned HTTP client.
    ///
    /// # Errors
    ///
    /// Returns an error when the base URL, model, or bearer token is invalid.
    pub fn with_client(
        client: reqwest::Client,
        base_url: &str,
        model: impl Into<String>,
        authorization: Option<HttpAuthorization>,
    ) -> ModelResult<Self> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(protocol_error("model identifier must not be empty"));
        }
        let endpoint = chat_completions_endpoint(base_url)?;
        let authorization = authorization
            .map(|authorization| {
                let name = HeaderName::from_bytes(authorization.header.as_bytes())
                    .map_err(|_| protocol_error("authorization header name is invalid"))?;
                let mut value = HeaderValue::from_str(&authorization.value)
                    .map_err(|_| protocol_error("authorization header value is invalid"))?;
                value.set_sensitive(true);
                Ok((name, value))
            })
            .transpose()?;
        Ok(Self {
            client,
            endpoint,
            model,
            authorization,
        })
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn stream_inner(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelResult<ModelEventStream> {
        if cancellation.is_cancelled() {
            return Err(Report::new(ModelError::Cancelled));
        }
        let request = build_chat_request(&self.model, request)?;
        let mut builder = self
            .client
            .post(self.endpoint.clone())
            .header(ACCEPT, "text/event-stream")
            .json(&request);
        if let Some((name, value)) = &self.authorization {
            builder = builder.header(name, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|source| transport_error(format!("request failed: {source}")))?;
        if !response.status().is_success() {
            return Err(request_error(format!(
                "provider returned HTTP {}",
                response.status()
            )));
        }
        Ok(normalize_stream(response))
    }
}

impl ModelBackend for OpenAiChatBackend {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, ModelResult<ModelEventStream>> {
        self.stream_inner(request, cancellation).boxed()
    }
}

/// One complete HTTP authorization header.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HttpAuthorization {
    /// Header name, such as `authorization` or `x-api-key`.
    pub header: String,
    /// Complete header value, including any scheme prefix.
    pub value: String,
}

impl HttpAuthorization {
    /// Creates a complete authorization header.
    #[must_use]
    pub fn new(header: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            value: value.into(),
        }
    }

    /// Creates the common `Authorization: Bearer …` form.
    #[must_use]
    pub fn bearer(token: impl Into<String>) -> Self {
        Self::new("authorization", format!("Bearer {}", token.into()))
    }
}

/// Projects normalized conversation state into one provider request.
fn build_chat_request(model: &str, request: ModelRequest) -> ModelResult<ChatRequest> {
    let messages = request
        .messages
        .into_iter()
        .map(ChatMessage::from)
        .collect();
    let tools = request
        .tools
        .into_iter()
        .map(|definition| {
            let parameters = RawValue::from_string(definition.input_schema)
                .map_err(|_| protocol_error("tool input schema is not valid JSON"))?;
            Ok(ChatTool {
                kind: "function",
                function: ChatToolDefinition {
                    name: definition.name,
                    description: definition.description,
                    parameters,
                },
            })
        })
        .collect::<ModelResult<Vec<_>>>()?;
    Ok(ChatRequest {
        model: model.to_owned(),
        messages,
        tools,
        stream: true,
    })
}

/// Converts an SSE response into the normalized model stream.
fn normalize_stream(response: reqwest::Response) -> ModelEventStream {
    let source = response.bytes_stream().eventsource().boxed();
    let state = DecoderState {
        source,
        pending: VecDeque::new(),
        tool_calls: BTreeMap::new(),
        terminal: false,
        finished: false,
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.pending.pop_front() {
                return Some((event, state));
            }
            if state.finished {
                return None;
            }
            match state.source.next().await {
                Some(Ok(event)) => {
                    if let Err(error) = state.accept_event(&event) {
                        state.pending.push_back(Err(error));
                        state.finished = true;
                    }
                }
                Some(Err(error)) => {
                    state.pending.push_back(Err(transport_error(format!(
                        "failed to decode the provider event stream: {error}"
                    ))));
                    state.finished = true;
                }
                None => {
                    if !state.terminal {
                        state.pending.push_back(Err(transport_error(
                            "provider response stream ended before a finish reason",
                        )));
                    }
                    state.finished = true;
                }
            }
        }
    })
    .boxed()
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool>,
    stream: bool,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ChatAssistantToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl From<ModelMessage> for ChatMessage {
    fn from(message: ModelMessage) -> Self {
        match message {
            ModelMessage::System { content } => Self::System { content },
            ModelMessage::User { content } => Self::User { content },
            ModelMessage::Assistant(message) => {
                let content = (!message.content.is_empty()).then_some(message.content);
                let tool_calls = message
                    .tool_calls
                    .into_iter()
                    .map(|call| ChatAssistantToolCall {
                        id: call.id,
                        kind: "function",
                        function: ChatAssistantFunction {
                            name: call.name,
                            arguments: call.arguments,
                        },
                    })
                    .collect();
                Self::Assistant {
                    content,
                    tool_calls,
                }
            }
            ModelMessage::Tool {
                tool_call_id,
                content,
                ..
            } => Self::Tool {
                tool_call_id,
                content,
            },
        }
    }
}

#[derive(Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatToolDefinition,
}

#[derive(Serialize)]
struct ChatToolDefinition {
    name: String,
    description: String,
    parameters: Box<RawValue>,
}

#[derive(Serialize)]
struct ChatAssistantToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatAssistantFunction,
}

#[derive(Serialize)]
struct ChatAssistantFunction {
    name: String,
    arguments: String,
}

type ProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<Event, EventStreamError<reqwest::Error>>> + Send + 'static>>;

struct DecoderState {
    source: ProviderEventStream,
    pending: VecDeque<ModelResult<ModelStreamEvent>>,
    tool_calls: BTreeMap<usize, ProviderToolCall>,
    terminal: bool,
    finished: bool,
}

impl DecoderState {
    /// Decodes one complete SSE event and queues normalized events.
    fn accept_event(&mut self, event: &Event) -> ModelResult<()> {
        let data = event.data.trim();
        if data == "[DONE]" {
            if !self.terminal {
                let inferred = if self.tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                self.finish(inferred)?;
            }
            self.finished = true;
            return Ok(());
        }
        if self.terminal {
            return Err(protocol_error(
                "provider emitted data after the finish reason",
            ));
        }
        let chunk: ChatCompletionChunk = serde_json::from_str(data)
            .map_err(|source| protocol_error(format!("invalid response chunk: {source}")))?;
        if let Some(error) = chunk.error {
            return Err(request_error(error.message));
        }
        for choice in chunk.choices {
            if choice.index != 0 {
                continue;
            }
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                self.pending
                    .push_back(Ok(ModelStreamEvent::TextDelta { delta: content }));
            }
            for tool_delta in choice.delta.tool_calls {
                self.accept_tool_delta(tool_delta)?;
            }
            if let Some(reason) = choice.finish_reason {
                self.finish(&reason)?;
            }
        }
        Ok(())
    }

    /// Merges one indexed provider delta into its normalized tool call.
    fn accept_tool_delta(&mut self, delta: ChatToolCallDelta) -> ModelResult<()> {
        let call = self.tool_calls.entry(delta.index).or_default();
        set_stable_field(&mut call.id, delta.id, "tool call identifier")?;
        if let Some(function) = delta.function {
            set_stable_field(&mut call.name, function.name, "tool name")?;
            if let Some(arguments) = function.arguments {
                if call.started {
                    if !arguments.is_empty() {
                        let id = call
                            .id
                            .clone()
                            .ok_or_else(|| protocol_error("started tool call has no identifier"))?;
                        self.pending
                            .push_back(Ok(ModelStreamEvent::ToolCallArgumentsDelta {
                                id,
                                delta: arguments,
                            }));
                    }
                } else {
                    call.buffered_arguments.push_str(&arguments);
                }
            }
        }
        if !call.started
            && let (Some(id), Some(name)) = (&call.id, &call.name)
        {
            if id.is_empty() || name.is_empty() {
                return Err(protocol_error(
                    "provider emitted an empty tool identifier or name",
                ));
            }
            call.started = true;
            self.pending
                .push_back(Ok(ModelStreamEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.clone(),
                }));
            if !call.buffered_arguments.is_empty() {
                self.pending
                    .push_back(Ok(ModelStreamEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        delta: std::mem::take(&mut call.buffered_arguments),
                    }));
            }
        }
        Ok(())
    }

    /// Completes every accumulated tool call before the terminal event.
    fn finish(&mut self, reason: &str) -> ModelResult<()> {
        for call in self.tool_calls.values() {
            if !call.started || !call.buffered_arguments.is_empty() {
                return Err(protocol_error(
                    "provider finished with an incomplete tool call",
                ));
            }
            let id = call
                .id
                .clone()
                .ok_or_else(|| protocol_error("completed tool call has no identifier"))?;
            self.pending
                .push_back(Ok(ModelStreamEvent::ToolCallCompleted { id }));
        }
        let finish_reason = match reason {
            "stop" => FinishReason::Stop,
            "tool_calls" => FinishReason::ToolCalls,
            "length" => FinishReason::Length,
            "content_filter" => {
                return Err(request_error(
                    "provider stopped generation because of a content filter",
                ));
            }
            _ => {
                return Err(protocol_error(format!(
                    "provider returned unsupported finish reason {reason:?}"
                )));
            }
        };
        self.pending
            .push_back(Ok(ModelStreamEvent::Completed { finish_reason }));
        self.terminal = true;
        Ok(())
    }
}

#[derive(Default)]
struct ProviderToolCall {
    id: Option<String>,
    name: Option<String>,
    buffered_arguments: String,
    started: bool,
}

#[derive(Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    error: Option<ChatError>,
}

#[derive(Deserialize)]
struct ChatError {
    message: String,
}

#[derive(Deserialize)]
struct ChatChoice {
    index: usize,
    #[serde(default)]
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct ChatDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCallDelta>,
}

#[derive(Deserialize)]
struct ChatToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<ChatFunctionDelta>,
}

#[derive(Deserialize)]
struct ChatFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

fn chat_completions_endpoint(base_url: &str) -> ModelResult<Url> {
    let mut base_url =
        Url::parse(base_url).map_err(|_| protocol_error("provider base URL is invalid"))?;
    if !base_url.username().is_empty()
        || base_url.password().is_some()
        || base_url.query().is_some()
        || base_url.fragment().is_some()
    {
        return Err(protocol_error(
            "provider base URL must not contain credentials, a query, or a fragment",
        ));
    }
    if !base_url.path().ends_with('/') {
        let path = format!("{}/", base_url.path());
        base_url.set_path(&path);
    }
    base_url
        .join("chat/completions")
        .map_err(|_| protocol_error("provider base URL cannot form a chat endpoint"))
}

/// Accepts an optional field once and rejects contradictory later deltas.
fn set_stable_field(
    target: &mut Option<String>,
    update: Option<String>,
    label: &'static str,
) -> ModelResult<()> {
    let Some(update) = update else {
        return Ok(());
    };
    if let Some(current) = target {
        if current != &update {
            return Err(protocol_error(format!(
                "provider changed the {label} while streaming"
            )));
        }
    } else {
        *target = Some(update);
    }
    Ok(())
}

fn protocol_error(message: impl Into<String>) -> Report<ModelError> {
    Report::new(ModelError::Protocol {
        message: message.into(),
    })
}

fn transport_error(message: impl Into<String>) -> Report<ModelError> {
    Report::new(ModelError::Transport {
        message: message.into(),
    })
}

fn request_error(message: impl Into<String>) -> Report<ModelError> {
    Report::new(ModelError::Request {
        message: message.into(),
    })
}
