//! Unix socket server for the TUI and CLI (JSON lines).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use wanderlan_core::api::{ApiError, Request, RequestEnvelope, ResponseEnvelope, ServerMessage};

use crate::daemon::Input;
use crate::events::EventLog;

/// Group whose members may use the socket.
const GROUP: &str = "wanderlan";

/// Creates the socket and serves connections.
pub fn spawn(
    path: PathBuf,
    events: Arc<EventLog>,
    inputs: mpsc::Sender<Input>,
) -> Result<JoinHandle<()>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("cannot listen on {}", path.display()))?;
    restrict(&path)?;
    Ok(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(serve(stream, events.clone(), inputs.clone()));
                }
                Err(e) => {
                    warn!("API socket accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }))
}

/// Socket permissions: owner root, group wanderlan, mode 0660.
fn restrict(path: &Path) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("cannot set permissions of {}", path.display()))?;
    if let Some(gid) = group_id(GROUP) {
        if let Some(dir) = path.parent() {
            let _ = std::os::unix::fs::chown(dir, None, Some(gid));
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750));
        }
        std::os::unix::fs::chown(path, None, Some(gid))
            .with_context(|| format!("cannot give group {GROUP} access to {}", path.display()))?;
    }
    Ok(())
}

/// Looks up a group id in /etc/group.
fn group_id(name: &str) -> Option<u32> {
    let groups = std::fs::read_to_string("/etc/group").ok()?;
    groups.lines().find_map(|line| {
        let mut parts = line.split(':');
        (parts.next()? == name).then_some(())?;
        parts.nth(1)?.parse().ok()
    })
}

async fn serve(stream: UnixStream, events: Arc<EventLog>, inputs: mpsc::Sender<Input>) {
    let (read, mut write) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(read).lines();
    let mut forwarder: Option<JoinHandle<()>> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let envelope: RequestEnvelope = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                let resp = ResponseEnvelope {
                    id: 0,
                    result: None,
                    error: Some(ApiError::new(format!("invalid request: {e}"))),
                };
                send(&out_tx, &ServerMessage::Response(resp));
                continue;
            }
        };
        let id = envelope.id;
        let result = if envelope.request == Request::Subscribe {
            let (history, mut rx) = events.subscribe();
            let tx = out_tx.clone();
            if let Some(f) = forwarder.take() {
                f.abort();
            }
            forwarder = Some(tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(event) => send(&tx, &ServerMessage::Event { event }),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!("subscriber lagged by {n} events");
                        }
                        Err(_) => break,
                    }
                }
            }));
            serde_json::to_value(history).map_err(|e| ApiError::new(e.to_string()))
        } else {
            let (reply_tx, reply_rx) = oneshot::channel();
            let input = Input::Api {
                request: envelope.request,
                reply: reply_tx,
            };
            if inputs.send(input).await.is_err() {
                Err(ApiError::new("the daemon is shutting down"))
            } else {
                reply_rx
                    .await
                    .unwrap_or_else(|_| Err(ApiError::new("the daemon is shutting down")))
            }
        };
        let resp = match result {
            Ok(value) => ResponseEnvelope {
                id,
                result: Some(value),
                error: None,
            },
            Err(error) => ResponseEnvelope {
                id,
                result: None,
                error: Some(error),
            },
        };
        send(&out_tx, &ServerMessage::Response(resp));
    }
    if let Some(f) = forwarder {
        f.abort();
    }
    drop(out_tx);
    let _ = writer.await;
}

fn send(tx: &mpsc::UnboundedSender<String>, msg: &ServerMessage) {
    if let Ok(line) = serde_json::to_string(msg) {
        let _ = tx.send(line);
    }
}
