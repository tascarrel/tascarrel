//! Typed Codex app-server authentication transport.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::BufReader;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::services::chats::harness::protocol::HarnessError;
use crate::services::chats::harness::protocol::HarnessErrorKind;
use crate::services::chats::process::HarnessProcessControl;
use crate::services::chats::process::HarnessProcessLauncher;
use crate::services::chats::process::HarnessProcessSpec;

const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Running Codex app-server used only for authentication operations.
pub(crate) struct CodexAuthServer {
    process: Arc<dyn HarnessProcessControl>,
    pending: PendingResponses,
    notifications: broadcast::Sender<CodexNotification>,
    next_request_id: AtomicU64,
}

impl CodexAuthServer {
    /// Starts and initializes an authentication app-server.
    pub(crate) async fn launch(
        executable: PathBuf,
        environment: HashMap<String, String>,
        working_directory: PathBuf,
        launcher: Arc<dyn HarnessProcessLauncher>,
    ) -> Result<Arc<Self>, HarnessError> {
        let process = launcher
            .launch(HarnessProcessSpec {
                title: "Codex authentication".to_owned(),
                executable,
                arguments: vec!["app-server".to_owned()],
                environment,
                working_directory,
            })
            .await?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (notifications, _) = broadcast::channel(32);
        let server = Arc::new(Self {
            process: process.control,
            pending: Arc::clone(&pending),
            notifications: notifications.clone(),
            next_request_id: AtomicU64::new(1),
        });
        tokio::spawn(read_messages(process.stdout, pending, notifications));
        let initialized = match server
            .request::<_, EmptyResult>(
                "initialize",
                InitializeParams {
                    client_info: ClientInfo {
                        name: "tascarrel",
                        title: "Tascarrel",
                        version: env!("CARGO_PKG_VERSION"),
                    },
                    capabilities: ClientCapabilities {
                        experimental_api: true,
                    },
                },
            )
            .await
        {
            Ok(_) => server.notify("initialized", EmptyParams {}).await,
            Err(error) => Err(error),
        };
        if let Err(error) = initialized {
            if let Err(stop_error) = server.stop().await {
                tracing::warn!(
                    message = %stop_error.message,
                    "failed to stop an uninitialized Codex authentication process"
                );
            }
            return Err(error);
        }
        Ok(server)
    }

    /// Subscribes to typed app-server notifications.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<CodexNotification> {
        self.notifications.subscribe()
    }

    /// Starts the device-code login flow.
    pub(crate) async fn start_device_code(&self) -> Result<StartLoginResult, HarnessError> {
        self.request(
            "account/login/start",
            StartLoginParams {
                login_type: "chatgptDeviceCode",
            },
        )
        .await
    }

    /// Cancels one pending login.
    pub(crate) async fn cancel_login(&self, login_id: &str) -> Result<(), HarnessError> {
        self.request::<_, EmptyResult>("account/login/cancel", LoginIdParams { login_id })
            .await
            .map(drop)
    }

    /// Reads the current Codex account.
    pub(crate) async fn read_account(&self) -> Result<AccountReadResult, HarnessError> {
        self.request("account/read", EmptyParams {}).await
    }

    /// Removes Codex credentials through the provider-supported operation.
    pub(crate) async fn logout(&self) -> Result<(), HarnessError> {
        self.request::<_, EmptyResult>("account/logout", EmptyParams {})
            .await
            .map(drop)
    }

    /// Stops the app-server process.
    pub(crate) async fn stop(&self) -> Result<(), HarnessError> {
        self.process.stop().await
    }

    async fn request<P, R>(&self, method: &'static str, params: P) -> Result<R, HarnessError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, sender);
        let request = RpcRequest { id, method, params };
        if let Err(error) = self.send(&request).await {
            lock(&self.pending).remove(&id);
            return Err(error);
        }
        let bytes = match timeout(RPC_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => {
                return Err(auth_error("Codex authentication process stopped"));
            }
            Err(_) => {
                lock(&self.pending).remove(&id);
                return Err(auth_error("Codex authentication request timed out"));
            }
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            auth_error(format!(
                "Codex returned an invalid authentication response: {error}"
            ))
        })
    }

    async fn notify<P>(&self, method: &'static str, params: P) -> Result<(), HarnessError>
    where
        P: Serialize,
    {
        self.send(&RpcNotification { method, params }).await
    }

    async fn send(&self, message: &impl Serialize) -> Result<(), HarnessError> {
        let mut bytes = serde_json::to_vec(message).map_err(|error| {
            auth_error(format!(
                "failed to encode Codex authentication request: {error}"
            ))
        })?;
        bytes.push(b'\n');
        self.process.write(bytes).await
    }
}

/// Provider notification emitted during a Codex authentication flow.
#[derive(Clone, Debug)]
pub(crate) struct CodexNotification {
    /// Native notification method.
    pub(crate) method: String,
    /// Serialized typed parameters.
    pub(crate) params: Vec<u8>,
}

impl CodexNotification {
    /// Decodes this notification's parameters into an expected provider type.
    pub(crate) fn decode<T: DeserializeOwned>(&self) -> Result<T, HarnessError> {
        serde_json::from_slice(&self.params).map_err(|error| {
            auth_error(format!(
                "Codex returned invalid authentication notification parameters: {error}"
            ))
        })
    }
}

/// Device-code login challenge returned by Codex.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StartLoginResult {
    /// Provider login identifier.
    pub(crate) login_id: String,
    /// Provider authorization page.
    #[serde(alias = "verificationUrl")]
    pub(crate) auth_url: String,
    /// One-time code entered on the authorization page.
    pub(crate) user_code: Option<String>,
}

/// Completion parameters for a Codex login attempt.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoginCompletedParams {
    /// Provider login identifier when supplied.
    pub(crate) login_id: Option<String>,
    /// Whether authentication succeeded.
    pub(crate) success: bool,
    /// Provider diagnostic for a failed attempt.
    pub(crate) error: Option<String>,
}

/// Current Codex account response.
#[derive(Deserialize)]
pub(crate) struct AccountReadResult {
    /// Authenticated account, or none when signed out.
    pub(crate) account: Option<CodexAccount>,
}

/// Secret-free Codex account metadata.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexAccount {
    /// Provider authentication mechanism.
    #[serde(rename = "type")]
    pub(crate) method: String,
    /// Account email when available.
    pub(crate) email: Option<String>,
    /// Subscription plan when available.
    pub(crate) plan_type: Option<String>,
}

type PendingResponses = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Vec<u8>, HarnessError>>>>>;

#[derive(Serialize)]
struct RpcRequest<P> {
    id: u64,
    method: &'static str,
    params: P,
}

#[derive(Serialize)]
struct RpcNotification<P> {
    method: &'static str,
    params: P,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingMessage {
    id: Option<u64>,
    method: Option<String>,
    result: Option<Box<RawValue>>,
    error: Option<RpcError>,
    params: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    client_info: ClientInfo,
    capabilities: ClientCapabilities,
}

#[derive(Serialize)]
struct ClientInfo {
    name: &'static str,
    title: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientCapabilities {
    experimental_api: bool,
}

#[derive(Serialize)]
struct EmptyParams {}

#[derive(Deserialize)]
struct EmptyResult {}

#[derive(Serialize)]
struct StartLoginParams {
    #[serde(rename = "type")]
    login_type: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginIdParams<'a> {
    login_id: &'a str,
}

async fn read_messages(
    stdout: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    pending: PendingResponses,
    notifications: broadcast::Sender<CodexNotification>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "failed to read Codex authentication output");
                break;
            }
        };
        let message = match serde_json::from_str::<IncomingMessage>(&line) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(%error, "Codex emitted invalid authentication protocol JSON");
                continue;
            }
        };
        if let Some(id) = message.id {
            if let Some(sender) = lock(&pending).remove(&id) {
                let response = match (message.result, message.error) {
                    (Some(result), _) => Ok(result.get().as_bytes().to_vec()),
                    (_, Some(error)) => Err(auth_error(error.message)),
                    _ => Err(auth_error(
                        "Codex returned an empty authentication response",
                    )),
                };
                if sender.send(response).is_err() {
                    tracing::debug!(
                        "Codex authentication response receiver closed before delivery"
                    );
                }
            }
        } else if let Some(method) = message.method {
            let params = message
                .params
                .map_or_else(|| b"{}".to_vec(), |params| params.get().as_bytes().to_vec());
            if notifications
                .send(CodexNotification { method, params })
                .is_err()
            {
                tracing::debug!("Codex authentication notification had no receiver");
            }
        }
    }
    for (_, sender) in std::mem::take(&mut *lock(&pending)) {
        if sender
            .send(Err(auth_error("Codex authentication process stopped")))
            .is_err()
        {
            tracing::debug!("Codex authentication response receiver closed during shutdown");
        }
    }
}

fn auth_error(message: impl Into<String>) -> HarnessError {
    HarnessError {
        kind: HarnessErrorKind::RequestFailed,
        message: message.into(),
        retryable: false,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
