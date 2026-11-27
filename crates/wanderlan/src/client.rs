//! Client for the daemon API (Unix socket, JSON lines).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, mpsc, oneshot};
use wanderlan_core::api::{
    ApiError, Event, Request, RequestEnvelope, ResponseEnvelope, ServerMessage,
};

/// Errors talking to the daemon.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The socket does not exist: the daemon is not running.
    #[error(
        "the wanderlan daemon is not running ({path} does not exist); start it with `systemctl start wanderland`; why it stopped: `systemctl status wanderland` and `journalctl -u wanderland`"
    )]
    NotRunning {
        /// Socket path.
        path: PathBuf,
    },
    /// No permission to use the socket.
    #[error(
        "no permission to use {path}; add your user to the group wanderlan (`sudo usermod -aG wanderlan $USER`) and log in again"
    )]
    PermissionDenied {
        /// Socket path.
        path: PathBuf,
    },
    /// The socket exists but nobody listens.
    #[error(
        "cannot connect to {path}: {source}; is the daemon running? `systemctl status wanderland`, `journalctl -u wanderland`"
    )]
    Connect {
        /// Socket path.
        path: PathBuf,
        /// Cause.
        source: std::io::Error,
    },
    /// The connection broke.
    #[error("the connection to the daemon was closed")]
    Closed,
    /// The daemon answered with an error.
    #[error("{0}")]
    Api(#[from] ApiError),
    /// Unexpected data.
    #[error("unexpected answer from the daemon: {0}")]
    Protocol(String),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseEnvelope>>>>;

/// A connection to the daemon. Cheap to clone; requests can be issued
/// concurrently.
#[derive(Debug, Clone)]
pub struct Client {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    /// The network of the box the requests are for (`None`: the default).
    network: Option<String>,
}

impl Client {
    /// Connects to the daemon. Events (after [`Request::Subscribe`]) are
    /// delivered to the returned receiver.
    pub async fn connect(
        path: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), ClientError> {
        let stream = match UnixStream::connect(path).await {
            Ok(s) => s,
            Err(e) => {
                return Err(match e.kind() {
                    std::io::ErrorKind::NotFound => ClientError::NotRunning { path: path.into() },
                    std::io::ErrorKind::PermissionDenied => {
                        ClientError::PermissionDenied { path: path.into() }
                    }
                    _ => ClientError::Connect {
                        path: path.into(),
                        source: e,
                    },
                });
            }
        };
        let (read, write) = stream.into_split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ServerMessage>(&line) {
                    Ok(ServerMessage::Event { event }) => {
                        let _ = events_tx.send(event);
                    }
                    Ok(ServerMessage::Response(resp)) => {
                        let mut pending = reader_pending.lock().await;
                        if resp.id == 0 {
                            // The daemon could not read a request (an older
                            // version, say, that does not know it) and cannot
                            // tell which one: every waiting request gets it.
                            for (_, tx) in pending.drain() {
                                let _ = tx.send(resp.clone());
                            }
                        } else if let Some(tx) = pending.remove(&resp.id) {
                            let _ = tx.send(resp);
                        }
                    }
                    Err(_) => {}
                }
            }
            // Dropping the senders wakes all waiting requests with `Closed`.
            reader_pending.lock().await.clear();
        });
        Ok((
            Self {
                writer: Arc::new(Mutex::new(write)),
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
                network: None,
            },
            events_rx,
        ))
    }

    /// The same connection, for the requests about `network` (`None`: the
    /// default network).
    pub fn for_network(mut self, network: Option<String>) -> Self {
        self.network = network;
        self
    }

    /// Sends a request and decodes the result.
    pub async fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let envelope = RequestEnvelope {
            id,
            network: self.network.clone(),
            request,
        };
        let mut line =
            serde_json::to_string(&envelope).map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        {
            let mut writer = self.writer.lock().await;
            if writer.write_all(line.as_bytes()).await.is_err() {
                self.pending.lock().await.remove(&id);
                return Err(ClientError::Closed);
            }
        }
        let resp = rx.await.map_err(|_| ClientError::Closed)?;
        if let Some(err) = resp.error {
            return Err(ClientError::Api(err));
        }
        let value = resp.result.unwrap_or(serde_json::Value::Null);
        serde_json::from_value(value).map_err(|e| ClientError::Protocol(e.to_string()))
    }
}
