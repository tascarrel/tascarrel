//! Local authenticated control-plane listener for host clients.
//!
//! Every accepted Unix socket carries the same typed control-plane protocol
//! served to the browser. The socket boundary authenticates a fresh client
//! identity; normal control-plane routing selects host and workspace targets.

use std::io;
use std::num::NonZeroUsize;

use tascarrel_api::types::protocol::ClientId;
use tascarrel_protocol::control_plane::StreamTransport;
use thiserror::Error;
use tokio::net::UnixListener;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::debug;
use tracing::warn;

use crate::HostControlService;
use crate::control_plane::BootstrapControlService;
use crate::services::auth::AuthService;

const DEFAULT_MAX_CLIENTS: usize = 64;

/// Failure while accepting local host clients.
#[derive(Debug, Error)]
pub enum BrokerError {
    /// The host control socket stopped accepting clients.
    #[error("failed to accept a host client: {0}")]
    Accept(#[source] io::Error),
}

/// Serves authenticated control-plane clients on the local host socket.
pub struct Broker {
    listener: UnixListener,
    auth: AuthService,
    control: watch::Receiver<Option<HostControlService>>,
    max_clients: usize,
}

impl Broker {
    /// Creates a broker for an already bound private Unix socket.
    #[must_use]
    pub fn new(
        listener: UnixListener,
        auth: AuthService,
        control: watch::Receiver<Option<HostControlService>>,
    ) -> Self {
        Self {
            listener,
            auth,
            control,
            max_clients: DEFAULT_MAX_CLIENTS,
        }
    }

    /// Sets the maximum number of concurrent local control-plane clients.
    #[must_use]
    pub fn with_max_clients(mut self, max_clients: NonZeroUsize) -> Self {
        self.max_clients = max_clients.get();
        self
    }

    /// Runs until the listener fails or this future is canceled.
    ///
    /// # Errors
    ///
    /// Returns an error when accepting a local host client fails.
    pub async fn run(self) -> Result<(), BrokerError> {
        let Self {
            listener,
            auth,
            control,
            max_clients,
        } = self;
        let mut clients = JoinSet::new();

        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted.map_err(BrokerError::Accept)?;
                    if clients.len() >= max_clients {
                        warn!(limit = max_clients, "rejecting client above host connection limit");
                        drop(stream);
                        continue;
                    }
                    let control = control.borrow().clone();
                    let auth = auth.clone();
                    clients.spawn(async move {
                        let result = match control {
                            Some(control) => control
                                .serve(StreamTransport::new(stream), ClientId::generate())
                                .await,
                            None => {
                                BootstrapControlService::new(auth)
                                    .serve(StreamTransport::new(stream), ClientId::generate())
                                    .await
                            }
                        };
                        if let Err(error) = result {
                            debug!(%error, "local control-plane client closed");
                        }
                    });
                }
                Some(result) = clients.join_next(), if !clients.is_empty() => {
                    if let Err(error) = result {
                        warn!(%error, "local control-plane client task failed");
                    }
                }
            }
        }
    }
}
