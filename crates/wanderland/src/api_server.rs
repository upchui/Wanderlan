//! Unix socket server for the TUI and CLI (JSON lines).

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use wanderlan_core::api::{
    ApiError, DEFAULT_NETWORK, NetworkInfo, Request, RequestEnvelope, ResetReport,
    ResponseEnvelope, ServerMessage,
};

use crate::daemon::Input;
use crate::events::EventLog;

/// How the API reaches one network of the box.
#[derive(Debug, Clone)]
pub struct Route {
    /// The name of the network.
    pub name: String,
    /// Its tag in interface names.
    pub tag: char,
    /// Its controller.
    pub inputs: mpsc::Sender<Input>,
    /// Its events.
    pub events: Arc<EventLog>,
    /// The name of this box's site in it, once it is configured.
    pub site: Arc<std::sync::RwLock<Option<String>>>,
}

impl Route {
    fn info(&self) -> NetworkInfo {
        NetworkInfo {
            name: self.name.clone(),
            tag: self.tag,
            site: self
                .site
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}

/// What the daemon itself does for the API: add and remove networks.
#[derive(Debug)]
pub enum Control {
    /// Adds a network.
    Add {
        /// Its name.
        name: String,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<NetworkInfo, ApiError>>,
    },
    /// Resets and removes a network.
    Remove {
        /// Its name.
        name: String,
        /// Where the answer goes.
        reply: oneshot::Sender<Result<ResetReport, ApiError>>,
    },
}

/// The networks of the box, as the API reaches them.
#[derive(Debug)]
pub struct Registry {
    routes: std::sync::RwLock<Vec<Route>>,
    control: mpsc::Sender<Control>,
}

impl Registry {
    /// The registry with the networks in `routes` (the default one first).
    pub fn new(routes: Vec<Route>, control: mpsc::Sender<Control>) -> Arc<Self> {
        Arc::new(Self {
            routes: std::sync::RwLock::new(routes),
            control,
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Route>> {
        self.routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds the route of a new network.
    pub fn add(&self, route: Route) {
        self.routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(route);
    }

    /// Removes the route of a network.
    pub fn remove(&self, name: &str) {
        self.routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|r| r.name != name);
    }

    /// The route to `network` (`None`: the default network).
    fn route(&self, network: Option<&str>) -> Result<Route, ApiError> {
        let name = network.unwrap_or(DEFAULT_NETWORK);
        self.read()
            .iter()
            .find(|r| r.name == name)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(format!(
                    "this box has no network \"{name}\" (`wanderlan networks` lists them)"
                ))
            })
    }

    /// The networks of the box.
    fn infos(&self) -> Vec<NetworkInfo> {
        self.read().iter().map(Route::info).collect()
    }
}

/// Group whose members may use the socket.
const GROUP: &str = "wanderlan";

/// How often the server checks that its socket file is still there.
const SOCKET_CHECK: std::time::Duration = std::time::Duration::from_secs(5);
/// Longest request line.
const MAX_REQUEST: u64 = 1024 * 1024;
/// Messages waiting for a client at most; a client that does not read is
/// dropped instead of filling the memory.
const OUT_QUEUE: usize = 1024;

/// Identity of a socket file (device and inode).
type FileId = (u64, u64);

fn file_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

/// Creates the socket and serves connections. If the socket file disappears
/// or is replaced (someone deleted it, or a daemon started by mistake took
/// it over), the server creates it again within a few seconds.
pub fn spawn(path: PathBuf, registry: Arc<Registry>) -> Result<JoinHandle<()>> {
    let (mut listener, mut id) = bind(&path)?;
    Ok(tokio::spawn(async move {
        let mut check = tokio::time::interval(SOCKET_CHECK);
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        tokio::spawn(serve(stream, registry.clone()));
                    }
                    Err(e) => {
                        warn!("API socket accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                },
                _ = check.tick() => {
                    if file_id(&path) == Some(id) {
                        continue;
                    }
                    warn!(
                        "The API socket {} was removed or replaced; creating it again",
                        path.display()
                    );
                    match bind(&path) {
                        Ok((l, i)) => {
                            listener = l;
                            id = i;
                        }
                        Err(e) => warn!("{e:#}; retrying in {} s", SOCKET_CHECK.as_secs()),
                    }
                }
            }
        }
    }))
}

/// Creates the socket file (replacing a stale one) with its permissions.
fn bind(path: &Path) -> Result<(UnixListener, FileId)> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("cannot listen on {}", path.display()))?;
    restrict(path)?;
    let id = file_id(path).with_context(|| format!("{} disappeared", path.display()))?;
    Ok((listener, id))
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

async fn serve(stream: UnixStream, registry: Arc<Registry>) {
    let (read, mut write) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(OUT_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();
    let mut forwarder: Option<JoinHandle<()>> = None;
    let invalid = |why: String| {
        ServerMessage::Response(ResponseEnvelope {
            id: 0,
            result: None,
            error: Some(ApiError::new(format!("invalid request: {why}"))),
        })
    };
    loop {
        buf.clear();
        match (&mut reader)
            .take(MAX_REQUEST + 1)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.len() as u64 > MAX_REQUEST {
            let _ = send(
                &out_tx,
                &invalid(format!("longer than {MAX_REQUEST} bytes")),
            );
            break;
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            if !send(&out_tx, &invalid("not UTF-8".into())) {
                break;
            }
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let envelope: RequestEnvelope = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(e) => {
                if !send(&out_tx, &invalid(e.to_string())) {
                    break;
                }
                continue;
            }
        };
        let id = envelope.id;
        let route = registry.route(envelope.network.as_deref());
        let result = match (envelope.request, route) {
            (Request::Networks, _) => {
                serde_json::to_value(registry.infos()).map_err(|e| ApiError::new(e.to_string()))
            }
            (Request::AddNetwork(params), _) => {
                let (reply, answer) = oneshot::channel();
                let asked = registry.control.send(Control::Add {
                    name: params.name,
                    reply,
                });
                control_answer(asked.await.is_ok(), answer).await
            }
            (Request::RemoveNetwork(params), _) => {
                let (reply, answer) = oneshot::channel();
                let asked = registry.control.send(Control::Remove {
                    name: params.name,
                    reply,
                });
                control_answer(asked.await.is_ok(), answer).await
            }
            (_, Err(e)) => Err(e),
            (Request::Subscribe, Ok(route)) => {
                let (history, mut rx) = route.events.subscribe();
                let tx = out_tx.clone();
                if let Some(f) = forwarder.take() {
                    f.abort();
                }
                forwarder = Some(tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            // A client that does not read gets no more events.
                            Ok(event) => {
                                if !send(&tx, &ServerMessage::Event { event }) {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                debug!("subscriber lagged by {n} events");
                            }
                            Err(_) => break,
                        }
                    }
                }));
                serde_json::to_value(history).map_err(|e| ApiError::new(e.to_string()))
            }
            (request, Ok(route)) => {
                let (reply_tx, reply_rx) = oneshot::channel();
                let input = Input::Api {
                    request,
                    reply: reply_tx,
                };
                if route.inputs.send(input).await.is_err() {
                    Err(ApiError::new("the daemon is shutting down"))
                } else {
                    reply_rx
                        .await
                        .unwrap_or_else(|_| Err(ApiError::new("the daemon is shutting down")))
                }
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
        if !send(&out_tx, &ServerMessage::Response(resp)) {
            break;
        }
    }
    if let Some(f) = forwarder {
        f.abort();
    }
    drop(out_tx);
    let _ = writer.await;
}

/// The answer of the daemon to a [`Control`] request.
async fn control_answer<T: serde::Serialize>(
    asked: bool,
    answer: oneshot::Receiver<Result<T, ApiError>>,
) -> Result<serde_json::Value, ApiError> {
    if !asked {
        return Err(ApiError::new("the daemon is shutting down"));
    }
    match answer.await {
        Ok(Ok(value)) => serde_json::to_value(value).map_err(|e| ApiError::new(e.to_string())),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ApiError::new("the daemon is shutting down")),
    }
}

/// Queues a message for the client; false if the client does not keep up
/// (or is gone): then it is dropped.
fn send(tx: &mpsc::Sender<String>, msg: &ServerMessage) -> bool {
    match serde_json::to_string(msg) {
        Ok(line) => tx.try_send(line).is_ok(),
        Err(_) => true,
    }
}
