//! Typed QEMU Machine Protocol support for Tascarrel-managed device hotplug.
//!
//! [`QmpClient`] owns the private QMP connection used by [`crate::Vm`] for USB
//! attach and detach operations.

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use reportify::Report;
use reportify::ResultExt as _;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::IgnoredAny;
use thiserror::Error;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::unix::OwnedWriteHalf;
use tokio::time::Instant;

/// A negotiated private QMP connection.
#[derive(Debug)]
pub(crate) struct QmpClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    command_timeout: Duration,
    next_id: u64,
    events: Vec<QmpEvent>,
}

impl QmpClient {
    /// Connects to a QMP socket and negotiates command capabilities.
    #[tracing::instrument(
        name = "tascarrel_vm.qmp.connect",
        level = "debug",
        skip_all,
        fields(socket = %path.display(), ?timeout),
        err
    )]
    pub(crate) async fn connect(path: &Path, timeout: Duration) -> Result<Self, Report<QmpError>> {
        let deadline = Instant::now() + timeout;
        let stream = loop {
            match UnixStream::connect(path).await {
                Ok(stream) => break stream,
                Err(error) if transient(&error) => {}
                Err(source) => {
                    return Err(Report::new(QmpError::Connect {
                        path: path.to_owned(),
                        source,
                    }));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Report::new(QmpError::ReadinessTimeout {
                    timeout,
                    path: path.to_owned(),
                }));
            }
            tokio::time::sleep(CONNECT_POLL_INTERVAL.min(deadline - now)).await;
        };
        let command_timeout = timeout.min(Duration::from_secs(5));
        Self::from_stream(stream, command_timeout).await
    }

    async fn from_stream(
        stream: UnixStream,
        command_timeout: Duration,
    ) -> Result<Self, Report<QmpError>> {
        let (reader, writer) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(reader),
            writer,
            command_timeout,
            next_id: 1,
            events: Vec::new(),
        };
        let _: QmpGreeting = client.read_json().await?;
        client
            .execute("qmp_capabilities", None::<NoArguments>)
            .await?;
        Ok(client)
    }

    /// Sends the typed command that attaches one host USB device.
    pub(crate) async fn attach_usb(
        &mut self,
        id: &str,
        host_bus: u8,
        host_address: u8,
        port: u8,
    ) -> Result<(), Report<QmpError>> {
        self.execute(
            "device_add",
            Some(DeviceAddArguments {
                driver: "usb-host",
                id,
                host_bus,
                host_address,
                bus: "tascarrel-xhci.0",
                port: port.to_string(),
            }),
        )
        .await
    }

    /// Requests USB detachment and waits for QEMU's completion event.
    pub(crate) async fn detach_usb(&mut self, id: &str) -> Result<(), Report<QmpError>> {
        self.execute("device_del", Some(DeviceDeleteArguments { id }))
            .await?;
        self.wait_for_device_deleted(id).await
    }

    /// Executes one typed QMP command and matches its response identifier.
    #[tracing::instrument(
        name = "tascarrel_vm.qmp.execute",
        level = "debug",
        skip_all,
        fields(command = %command, request_id = self.next_id),
        err
    )]
    async fn execute<A>(
        &mut self,
        command: &str,
        arguments: Option<A>,
    ) -> Result<(), Report<QmpError>>
    where
        A: Serialize,
    {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let request = QmpRequest {
            execute: command,
            arguments,
            id,
        };
        let mut bytes = serde_json::to_vec(&request)
            .map_err(QmpError::Json)
            .report()?;
        bytes.extend_from_slice(b"\r\n");
        let write = async {
            self.writer.write_all(&bytes).await?;
            self.writer.flush().await
        };
        tokio::time::timeout(self.command_timeout, write)
            .await
            .map_err(|_| QmpError::OperationTimeout {
                timeout: self.command_timeout,
            })
            .report()?
            .map_err(QmpError::Io)
            .report()?;

        loop {
            match self.read_json::<QmpMessage>().await? {
                QmpMessage::Event(event) => self.events.push(event),
                QmpMessage::Success(response) if response.id == id => return Ok(()),
                QmpMessage::Failure(response) if response.id == id => {
                    return Err(Report::new(QmpError::Command {
                        command: command.to_owned(),
                        class: response.error.class,
                        description: response.error.description,
                    }));
                }
                QmpMessage::Success(response) => {
                    return Err(Report::new(QmpError::UnexpectedCommandResponse {
                        expected_id: id,
                        actual_id: response.id,
                    }));
                }
                QmpMessage::Failure(response) => {
                    return Err(Report::new(QmpError::UnexpectedCommandResponse {
                        expected_id: id,
                        actual_id: response.id,
                    }));
                }
            }
        }
    }

    /// Waits for the matching asynchronous device-deleted event.
    async fn wait_for_device_deleted(&mut self, id: &str) -> Result<(), Report<QmpError>> {
        if let Some(index) = self
            .events
            .iter()
            .position(|event| event.device_deleted(id))
        {
            self.events.swap_remove(index);
            return Ok(());
        }
        loop {
            match self.read_json::<QmpMessage>().await? {
                QmpMessage::Event(event) if event.device_deleted(id) => return Ok(()),
                QmpMessage::Event(event) => self.events.push(event),
                QmpMessage::Success(response) => {
                    return Err(Report::new(QmpError::UnexpectedEventResponse {
                        response_id: response.id,
                    }));
                }
                QmpMessage::Failure(response) => {
                    return Err(Report::new(QmpError::UnexpectedEventResponse {
                        response_id: response.id,
                    }));
                }
            }
        }
    }

    /// Reads one non-empty timeout-bounded JSON message.
    async fn read_json<T>(&mut self) -> Result<T, Report<QmpError>>
    where
        T: DeserializeOwned,
    {
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(self.command_timeout, self.reader.read_line(&mut line))
                .await
                .map_err(|_| QmpError::OperationTimeout {
                    timeout: self.command_timeout,
                })
                .report()?
                .map_err(QmpError::Io)
                .report()?;
            if read == 0 {
                return Err(Report::new(QmpError::Protocol(
                    "QMP socket closed".to_owned(),
                )));
            }
            if !line.trim().is_empty() {
                return serde_json::from_str(&line).map_err(QmpError::Json).report();
            }
        }
    }
}

/// A QMP transport, protocol, or command failure.
#[derive(Debug, Error)]
pub(crate) enum QmpError {
    #[error("QMP socket did not become ready within {timeout:?}: {path}")]
    ReadinessTimeout { timeout: Duration, path: PathBuf },
    #[error("failed to connect to QMP socket {path}: {source}")]
    Connect { path: PathBuf, source: io::Error },
    #[error("QMP transport failed: {0}")]
    Io(#[source] io::Error),
    #[error("QMP operation did not complete within {timeout:?}")]
    OperationTimeout { timeout: Duration },
    #[error("QMP returned malformed JSON: {0}")]
    Json(#[source] serde_json::Error),
    #[error("QMP protocol error: {0}")]
    Protocol(String),
    #[error("QMP command {command} failed ({class}): {description}")]
    Command {
        command: String,
        class: String,
        description: String,
    },
    #[error("received QMP response {actual_id} while waiting for response {expected_id}")]
    UnexpectedCommandResponse { expected_id: u64, actual_id: u64 },
    #[error("received unexpected QMP response {response_id} while waiting for an event")]
    UnexpectedEventResponse { response_id: u64 },
}

impl QmpError {
    /// Reports whether QEMU rejected a command because its device was absent.
    pub(crate) fn is_device_not_found(&self) -> bool {
        matches!(self, Self::Command { class, .. } if class == "DeviceNotFound")
    }
}

const CONNECT_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Deserialize)]
struct QmpGreeting {
    #[serde(rename = "QMP")]
    _qmp: QmpGreetingData,
}

#[derive(Deserialize)]
struct QmpGreetingData {}

#[derive(Serialize)]
struct QmpRequest<'a, A> {
    execute: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<A>,
    id: u64,
}

#[derive(Serialize)]
struct NoArguments;

#[derive(Serialize)]
struct DeviceAddArguments<'a> {
    driver: &'static str,
    id: &'a str,
    #[serde(rename = "hostbus")]
    host_bus: u8,
    #[serde(rename = "hostaddr")]
    host_address: u8,
    bus: &'static str,
    port: String,
}

#[derive(Serialize)]
struct DeviceDeleteArguments<'a> {
    id: &'a str,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum QmpMessage {
    Event(QmpEvent),
    Failure(QmpFailure),
    Success(QmpSuccess),
}

#[derive(Debug, Deserialize, Serialize)]
struct QmpEvent {
    event: String,
    #[serde(default)]
    data: QmpEventData,
}

impl QmpEvent {
    fn device_deleted(&self, id: &str) -> bool {
        self.event == "DEVICE_DELETED" && self.data.device.as_deref() == Some(id)
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct QmpEventData {
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<String>,
}

#[derive(Deserialize)]
struct QmpFailure {
    error: QmpCommandError,
    id: u64,
}

#[derive(Deserialize)]
struct QmpCommandError {
    class: String,
    #[serde(rename = "desc")]
    description: String,
}

#[derive(Deserialize)]
struct QmpSuccess {
    #[serde(rename = "return")]
    _result: IgnoredAny,
    id: u64,
}

/// Classifies connection errors that can resolve before the readiness deadline.
fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

#[cfg(test)]
mod tests {
    use std::io::BufRead as _;
    use std::io::BufReader;
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;
    use std::thread;

    use tempfile::tempdir;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct CapturedRequest {
        execute: String,
        arguments: Option<CapturedArguments>,
        id: u64,
    }

    #[derive(Debug, Deserialize)]
    struct CapturedArguments {
        hostbus: Option<u8>,
        hostaddr: Option<u8>,
        port: Option<String>,
    }

    #[derive(Serialize)]
    struct SuccessfulResponse {
        #[serde(rename = "return")]
        result: EmptyResult,
        id: u64,
    }

    #[derive(Serialize)]
    struct EmptyResult {}

    /// Negotiates QMP and preserves typed USB arguments and event ordering.
    #[tokio::test]
    async fn negotiates_and_sends_typed_usb_commands() {
        let temporary = tempdir().unwrap();
        let socket = temporary.path().join("qmp.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"{\"QMP\":{\"version\":{}}}\r\n").unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut commands = Vec::new();
            for index in 0..3 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: CapturedRequest = serde_json::from_str(&line).unwrap();
                let id = request.id;
                commands.push(request);
                serde_json::to_writer(
                    &mut stream,
                    &SuccessfulResponse {
                        result: EmptyResult {},
                        id,
                    },
                )
                .unwrap();
                stream.write_all(b"\r\n").unwrap();
                if index == 2 {
                    serde_json::to_writer(
                        &mut stream,
                        &QmpEvent {
                            event: "DEVICE_DELETED".to_owned(),
                            data: QmpEventData {
                                device: Some("tascarrel-usb-probe".to_owned()),
                            },
                        },
                    )
                    .unwrap();
                    stream.write_all(b"\r\n").unwrap();
                }
            }
            commands
        });

        let mut client = QmpClient::connect(&socket, Duration::from_secs(1))
            .await
            .unwrap();
        client
            .attach_usb("tascarrel-usb-probe", 1, 7, 3)
            .await
            .unwrap();
        client.detach_usb("tascarrel-usb-probe").await.unwrap();
        let commands = server.join().unwrap();
        assert_eq!(commands[0].execute, "qmp_capabilities");
        assert_eq!(commands[1].execute, "device_add");
        let arguments = commands[1].arguments.as_ref().unwrap();
        assert_eq!(arguments.hostbus, Some(1));
        assert_eq!(arguments.hostaddr, Some(7));
        assert_eq!(arguments.port.as_deref(), Some("3"));
        assert_eq!(commands[2].execute, "device_del");
    }
}
