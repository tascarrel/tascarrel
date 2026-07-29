//! Reusable text-only MCP clients adapted to Tasci's tool interface.
//!
//! [`McpClient`] connects one configured Streamable HTTP server, discovers its
//! advertised tools, and exposes every operation as an [`McpTool`]. Remote
//! definitions provide the model-visible names, descriptions, and input
//! schemas. Rich MCP result content is intentionally rejected.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use reportify::Report;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use rmcp::RoleClient;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::model::ClientCapabilities;
use rmcp::model::ClientInfo;
use rmcp::model::ContentBlock;
use rmcp::model::Implementation;
use rmcp::model::JsonObject;
use rmcp::model::Tool as RemoteTool;
use rmcp::service::RunningService;
use rmcp::service::ServerSink;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::Deserialize;
use thiserror::Error;

use crate::Tool;
use crate::ToolContext;
use crate::ToolDefinition;
use crate::ToolError;
use crate::ToolOutput;
use crate::ToolPrompt;
use crate::ToolResult;

/// A running text-only MCP client connected to one remote server.
pub struct McpClient {
    service: RunningService<RoleClient, ClientInfo>,
    tools: Vec<McpTool>,
    server_label: String,
}

impl McpClient {
    /// Connects to a Streamable HTTP MCP server and discovers all advertised
    /// tools.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration is invalid, the server cannot be
    /// reached, MCP initialization or discovery fails, or no tools are
    /// advertised.
    #[tracing::instrument(level = "debug", skip_all, fields(server = %config.server_label))]
    pub async fn connect(config: McpClientConfig) -> McpResult<Self> {
        ensure_mcp_tls_provider()?;
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(config.endpoint.clone())
                .custom_headers(config.headers.clone())
                .max_sse_event_size(config.sse_event_byte_limit),
        );
        let service =
            tokio::time::timeout(config.connect_timeout, mcp_client_info().serve(transport))
                .await
                .map_err(|_| {
                    Report::new(McpError::ConnectTimeout {
                        server: config.server_label.clone(),
                    })
                })?
                .map_err(|_| {
                    Report::new(McpError::Connect {
                        server: config.server_label.clone(),
                    })
                })?;
        Self::from_service(config, service).await
    }

    /// Returns every tool advertised by this server.
    #[must_use]
    pub fn tools(&self) -> Vec<McpTool> {
        self.tools.clone()
    }

    /// Stops the MCP connection and waits for its transport tasks to finish.
    ///
    /// # Errors
    ///
    /// Returns an error when a transport task cannot be joined cleanly.
    #[tracing::instrument(level = "debug", skip_all, fields(server = %self.server_label))]
    pub async fn shutdown(self) -> McpResult<()> {
        self.service.cancel().await.map(|_| ()).map_err(|_| {
            Report::new(McpError::Shutdown {
                server: self.server_label,
            })
        })
    }

    #[tracing::instrument(level = "debug", skip_all, fields(server = %config.server_label))]
    async fn from_service(
        config: McpClientConfig,
        service: RunningService<RoleClient, ClientInfo>,
    ) -> McpResult<Self> {
        let remote_tools = tokio::time::timeout(config.connect_timeout, service.list_all_tools())
            .await
            .map_err(|_| {
                Report::new(McpError::DiscoveryTimeout {
                    server: config.server_label.clone(),
                })
            })?
            .map_err(|_| {
                Report::new(McpError::Discovery {
                    server: config.server_label.clone(),
                })
            })?;
        if remote_tools.is_empty() {
            return Err(Report::new(McpError::MissingTools {
                server: config.server_label,
            }));
        }
        let execution = Arc::new(McpExecutionConfig {
            server_label: config.server_label.clone(),
            tool_timeout: config.tool_timeout,
            output_byte_limit: config.output_byte_limit,
        });
        let mut model_names = BTreeSet::new();
        let tools = remote_tools
            .into_iter()
            .map(|remote| {
                let tool = McpTool::new(
                    service.peer().clone(),
                    Arc::clone(&execution),
                    &config.server_name,
                    &config.server_label,
                    remote,
                )?;
                if !model_names.insert(tool.definition.name.clone()) {
                    return Err(Report::new(McpError::IncompatibleTools {
                        server: config.server_label.clone(),
                    }));
                }
                Ok(tool)
            })
            .collect::<McpResult<Vec<_>>>()?;
        Ok(Self {
            service,
            tools,
            server_label: config.server_label,
        })
    }
}

/// One discovered, text-returning MCP operation callable by Tasci.
#[derive(Clone)]
pub struct McpTool {
    server: ServerSink,
    execution: Arc<McpExecutionConfig>,
    remote_name: String,
    definition: ToolDefinition,
}

impl McpTool {
    fn new(
        server: ServerSink,
        execution: Arc<McpExecutionConfig>,
        server_name: &str,
        server_label: &str,
        remote: RemoteTool,
    ) -> McpResult<Self> {
        let remote_name = remote.name.into_owned();
        if remote_name.is_empty() {
            return Err(Report::new(McpError::IncompatibleTools {
                server: server_label.to_owned(),
            }));
        }
        let definition = ToolDefinition {
            name: model_tool_name(server_name, &remote_name),
            description: remote.description.map_or_else(
                || format!("{server_label} MCP operation {remote_name}."),
                std::borrow::Cow::into_owned,
            ),
            input_schema: serde_json::to_string(remote.input_schema.as_ref()).map_err(|_| {
                Report::new(McpError::IncompatibleTools {
                    server: server_label.to_owned(),
                })
            })?,
            prompt: ToolPrompt {
                summary: remote
                    .title
                    .unwrap_or_else(|| format!("{server_label}: {remote_name}")),
                guidelines: vec![format!(
                    "Calling this tool sends its arguments to the {server_label} MCP server."
                )],
            },
        };
        Ok(Self {
            server,
            execution,
            remote_name,
            definition,
        })
    }
}

impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        context: ToolContext,
        arguments: String,
    ) -> BoxFuture<'static, ToolResult<ToolOutput>> {
        let server = self.server.clone();
        let execution = Arc::clone(&self.execution);
        let remote_name = self.remote_name.clone();
        let tool_name = self.definition.name.clone();
        async move {
            let McpArguments(arguments) = serde_json::from_str(&arguments).map_err(|source| {
                Report::new(ToolError::InvalidArguments {
                    tool: tool_name.clone(),
                    message: source.to_string(),
                })
            })?;
            let request = CallToolRequestParams::new(remote_name).with_arguments(arguments);
            let call = tokio::time::timeout(execution.tool_timeout, server.call_tool(request));
            let result = context
                .cancellation
                .run_until_cancelled(call)
                .await
                .ok_or_else(|| Report::new(ToolError::Cancelled))?
                .map_err(|_| mcp_tool_error(&tool_name, "request timed out"))?
                .map_err(|_| mcp_tool_error(&tool_name, "request failed"))?;
            let text = limit_mcp_output(
                mcp_text_content(&tool_name, result.content)?,
                execution.output_byte_limit,
                &execution.server_label,
            );
            if result.is_error == Some(true) {
                return Err(mcp_tool_error(&tool_name, text));
            }
            Ok(ToolOutput::text(text))
        }
        .boxed()
    }
}

/// Client connection, headers, and output limits for one Streamable HTTP MCP
/// server.
#[derive(Clone)]
pub struct McpClientConfig {
    server_name: String,
    server_label: String,
    endpoint: String,
    headers: HashMap<HeaderName, HeaderValue>,
    connect_timeout: Duration,
    tool_timeout: Duration,
    output_byte_limit: usize,
    sse_event_byte_limit: usize,
}

impl McpClientConfig {
    /// Creates a configuration with bounded defaults.
    ///
    /// The server name becomes part of every model-visible tool name. The
    /// label is included in prompts, logs, and errors. Neither may contain
    /// secrets.
    ///
    /// The endpoint must use HTTP or HTTPS and must not contain credentials,
    /// query parameters, or a fragment. Header values may contain host-side
    /// secret-injection placeholders.
    ///
    /// # Errors
    ///
    /// Returns an error when a name, label, endpoint, or header is invalid.
    pub fn new(
        server_name: impl Into<String>,
        server_label: impl Into<String>,
        endpoint: impl AsRef<str>,
        headers: BTreeMap<String, String>,
    ) -> McpResult<Self> {
        let server_name = server_name.into();
        let server_label = server_label.into();
        validate_server_name(&server_name, &server_label)?;
        if server_label.trim().is_empty() {
            return Err(invalid_configuration("MCP server", "server label is empty"));
        }
        let endpoint = reqwest::Url::parse(endpoint.as_ref())
            .map_err(|_| invalid_configuration(&server_label, "endpoint is not a valid URL"))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(invalid_configuration(
                &server_label,
                "endpoint must use HTTP or HTTPS",
            ));
        }
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(invalid_configuration(
                &server_label,
                "endpoint must not contain credentials, query parameters, or a fragment",
            ));
        }
        let headers = parse_headers(&server_label, headers)?;
        Ok(Self {
            server_name,
            server_label,
            endpoint: endpoint.to_string(),
            headers,
            connect_timeout: DEFAULT_MCP_CONNECT_TIMEOUT,
            tool_timeout: DEFAULT_MCP_TOOL_TIMEOUT,
            output_byte_limit: DEFAULT_MCP_OUTPUT_BYTE_LIMIT,
            sse_event_byte_limit: DEFAULT_MCP_SSE_EVENT_BYTE_LIMIT,
        })
    }

    /// Overrides the deadline for initialization and tool discovery.
    #[must_use]
    pub const fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Overrides the deadline for one tool call.
    #[must_use]
    pub const fn with_tool_timeout(mut self, timeout: Duration) -> Self {
        self.tool_timeout = timeout;
        self
    }

    /// Overrides the maximum text returned by one tool call.
    #[must_use]
    pub const fn with_output_byte_limit(mut self, byte_limit: usize) -> Self {
        self.output_byte_limit = byte_limit;
        self
    }

    /// Overrides the maximum accepted Streamable HTTP SSE event.
    #[must_use]
    pub const fn with_sse_event_byte_limit(mut self, byte_limit: usize) -> Self {
        self.sse_event_byte_limit = byte_limit;
        self
    }
}

/// Failure while configuring, connecting to, or discovering an MCP server.
#[derive(Debug, Error)]
pub enum McpError {
    /// Local server configuration is invalid.
    #[error("invalid {server} MCP configuration: {message}")]
    InvalidConfiguration {
        /// Secret-safe server label.
        server: String,
        /// Secret-safe validation failure.
        message: String,
    },
    /// Rustls had no process-wide cryptography provider.
    #[error("failed to configure the TLS cryptography provider")]
    TlsProvider,
    /// MCP initialization exceeded its configured deadline.
    #[error("timed out while connecting to the {server} MCP server")]
    ConnectTimeout {
        /// Secret-safe server label.
        server: String,
    },
    /// MCP initialization failed.
    #[error("failed to connect to the {server} MCP server")]
    Connect {
        /// Secret-safe server label.
        server: String,
    },
    /// Tool discovery exceeded its configured deadline.
    #[error("timed out while discovering {server} MCP tools")]
    DiscoveryTimeout {
        /// Secret-safe server label.
        server: String,
    },
    /// Tool discovery failed.
    #[error("failed to discover {server} MCP tools")]
    Discovery {
        /// Secret-safe server label.
        server: String,
    },
    /// The server did not advertise any tools.
    #[error("the {server} MCP server did not advertise tools")]
    MissingTools {
        /// Secret-safe server label.
        server: String,
    },
    /// Advertised tools cannot be represented safely in Tasci.
    #[error("the {server} MCP server advertised incompatible tool definitions")]
    IncompatibleTools {
        /// Secret-safe server label.
        server: String,
    },
    /// The MCP transport tasks could not be joined during shutdown.
    #[error("failed to stop the {server} MCP connection")]
    Shutdown {
        /// Secret-safe server label.
        server: String,
    },
}

/// Result returned while configuring or operating an MCP client.
pub type McpResult<T> = Result<T, Report<McpError>>;

/// Default deadline for MCP initialization and tool discovery.
pub const DEFAULT_MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Default deadline for one remote MCP tool call.
pub const DEFAULT_MCP_TOOL_TIMEOUT: Duration = Duration::from_mins(2);

/// Default maximum text returned to the model by one MCP tool call.
pub const DEFAULT_MCP_OUTPUT_BYTE_LIMIT: usize = 256 * 1_024;

/// Default maximum accepted Streamable HTTP SSE event.
pub const DEFAULT_MCP_SSE_EVENT_BYTE_LIMIT: usize = 2 * 1_024 * 1_024;

struct McpExecutionConfig {
    server_label: String,
    tool_timeout: Duration,
    output_byte_limit: usize,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct McpArguments(JsonObject);

/// Describes Tasci without advertising optional MCP client capabilities.
fn mcp_client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("Tasci", env!("CARGO_PKG_VERSION")),
    )
}

/// Installs Tasci's Rustls provider unless the process already selected one.
fn ensure_mcp_tls_provider() -> McpResult<()> {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return Ok(());
    }
    match rustls::crypto::ring::default_provider().install_default() {
        Ok(()) => Ok(()),
        Err(_) if rustls::crypto::CryptoProvider::get_default().is_some() => Ok(()),
        Err(_) => Err(Report::new(McpError::TlsProvider)),
    }
}

/// Ensures one settings key can safely namespace model-visible tools.
fn validate_server_name(server_name: &str, server_label: &str) -> McpResult<()> {
    if server_name.is_empty()
        || server_name.chars().any(|character| {
            !character.is_ascii_alphanumeric() && character != '-' && character != '_'
        })
    {
        return Err(invalid_configuration(
            server_label,
            "server name must contain only ASCII letters, digits, hyphens, and underscores",
        ));
    }
    Ok(())
}

/// Parses secret-safe header templates into transport values.
fn parse_headers(
    server_label: &str,
    headers: BTreeMap<String, String>,
) -> McpResult<HashMap<HeaderName, HeaderValue>> {
    let mut parsed = HashMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid_configuration(server_label, "HTTP header name is invalid"))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|_| invalid_configuration(server_label, "HTTP header value is invalid"))?;
        if parsed.insert(name, value).is_some() {
            return Err(invalid_configuration(
                server_label,
                "HTTP header name is repeated without regard to case",
            ));
        }
    }
    Ok(parsed)
}

/// Produces a globally namespaced tool name accepted by model providers.
fn model_tool_name(server_name: &str, remote_name: &str) -> String {
    let remote_name = remote_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("mcp__{server_name}__{remote_name}")
}

/// Joins text blocks while rejecting richer MCP content types.
fn mcp_text_content(tool: &str, blocks: Vec<ContentBlock>) -> ToolResult<String> {
    let mut content = String::new();
    for block in blocks {
        let ContentBlock::Text(text) = block else {
            return Err(mcp_tool_error(
                tool,
                "server returned unsupported non-text content",
            ));
        };
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        content.push_str(&text.text);
    }
    if content.is_empty() {
        return Err(mcp_tool_error(tool, "server returned no text content"));
    }
    Ok(content)
}

/// Truncates MCP text on a UTF-8 boundary before it enters model context.
fn limit_mcp_output(mut content: String, byte_limit: usize, server_label: &str) -> String {
    if content.len() <= byte_limit {
        return content;
    }
    let mut end = byte_limit;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    content.truncate(end);
    content.push_str("\n\n[");
    content.push_str(server_label);
    content.push_str(" MCP output was truncated by Tasci.]");
    content
}

/// Creates a secret-safe MCP configuration failure.
fn invalid_configuration(server: &str, message: &str) -> Report<McpError> {
    Report::new(McpError::InvalidConfiguration {
        server: server.to_owned(),
        message: message.to_owned(),
    })
}

/// Creates the common agent-facing error for an MCP tool failure.
fn mcp_tool_error(tool: &str, message: impl Into<String>) -> Report<ToolError> {
    Report::new(ToolError::Mcp {
        tool: tool.to_owned(),
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rmcp::RoleServer;
    use rmcp::ServerHandler;
    use rmcp::model::CallToolResponse;
    use rmcp::model::CallToolResult;
    use rmcp::model::ErrorData;
    use rmcp::model::ListToolsResult;
    use rmcp::model::PaginatedRequestParams;
    use rmcp::model::ServerCapabilities;
    use rmcp::model::ServerInfo;
    use rmcp::model::Tool as RemoteTool;
    use rmcp::service::RequestContext;
    use serde::Deserialize;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::FileWorkspace;

    #[derive(Clone, Default)]
    struct TextToolServer;

    impl ServerHandler for TextToolServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            let mut second_tool = search_tool();
            second_tool.name = "future.remote.tool".to_owned().into();
            Ok(ListToolsResult {
                tools: vec![search_tool(), second_tool],
                ..Default::default()
            })
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            assert_eq!(request.name, "search");
            let encoded = serde_json::to_string(
                &request
                    .arguments
                    .expect("test tool request should contain arguments"),
            )
            .expect("test tool arguments should encode");
            let arguments: SearchArguments =
                serde_json::from_str(&encoded).expect("test tool arguments should be typed");
            assert_eq!(
                arguments,
                SearchArguments {
                    query: "Tasci MCP".to_owned(),
                }
            );
            Ok(CallToolResult::success(vec![ContentBlock::text("A text search result.")]).into())
        }
    }

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct SearchArguments {
        query: String,
    }

    /// Exercises header templates, automatic discovery, namespacing, remote
    /// argument names, and text adaptation over an in-memory transport.
    #[tokio::test]
    async fn discovers_and_executes_all_text_tools() {
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1_024);
        let server = tokio::spawn(async move {
            TextToolServer
                .serve(server_transport)
                .await
                .expect("test MCP server should start")
                .waiting()
                .await
                .expect("test MCP server should stop cleanly");
        });
        let service = mcp_client_info()
            .serve(client_transport)
            .await
            .expect("test MCP client should connect");
        let headers = BTreeMap::from([(
            "X-Workspace-Token".to_owned(),
            "tascarrel-secret:mcp-token".to_owned(),
        )]);
        let config = McpClientConfig::new("test", "Test", "http://127.0.0.1/mcp", headers)
            .expect("test MCP configuration should be valid");
        assert_eq!(
            config
                .headers
                .get(&HeaderName::from_static("x-workspace-token"))
                .expect("test MCP header should be configured")
                .to_str()
                .expect("test MCP header should remain text"),
            "tascarrel-secret:mcp-token"
        );
        let mcp = McpClient::from_service(config, service)
            .await
            .expect("test MCP tools should be discovered");
        let tools = mcp.tools();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].definition().name, "mcp__test__search");
        assert_eq!(tools[1].definition().name, "mcp__test__future_remote_tool");
        assert_eq!(
            tools[0].definition().description,
            "Search public information."
        );

        let directory = tempdir().expect("temporary workspace should be created");
        let workspace = FileWorkspace::open(directory.path())
            .await
            .expect("temporary workspace should open");
        let result = tools[0]
            .execute(
                ToolContext {
                    files: Arc::new(workspace),
                    cancellation: CancellationToken::new(),
                },
                r#"{"query":"Tasci MCP"}"#.to_owned(),
            )
            .await
            .expect("text MCP tool should succeed");

        assert_eq!(result.content, "A text search result.");

        mcp.shutdown()
            .await
            .expect("test MCP client should stop cleanly");
        server.await.expect("test MCP server task should join");
    }

    fn search_tool() -> RemoteTool {
        serde_json::from_str(
            r#"{
                "name": "search",
                "description": "Search public information.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    },
                    "required": ["query"]
                }
            }"#,
        )
        .expect("test search tool should be valid")
    }
}
