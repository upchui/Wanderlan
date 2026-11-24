//! Configuration sync and joining over TCP on `<address>:<control_port>`.
//!
//! * `MESH`: the server answers with its confirmed `mesh.toml` (canonical
//!   form) and closes the connection. Only direct peers (and peers removed
//!   within the last minutes) are served.
//! * `JOIN <json>` ([`JoinRequest`]): a new site asks to join with an
//!   invitation code; open to every address, the controller checks the code.
//!   The answer is one JSON line ([`JoinReply`]).

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use wanderlan_core::join::{JoinReply, JoinRequest};

use crate::daemon::Input;

/// Largest accepted mesh.toml.
const MAX_SIZE: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Longest accepted request line.
const MAX_LINE: u64 = 4096;
/// Time the controller may take to answer a join request.
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);

/// What the server offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offer {
    /// Address to listen on.
    pub bind: Option<SocketAddrV4>,
    /// Addresses allowed to fetch.
    pub allowed: BTreeSet<Ipv4Addr>,
    /// The confirmed mesh.toml.
    pub mesh: String,
}

/// Starts the server; it follows changes of the offer.
pub fn spawn(offer: watch::Receiver<Offer>, inputs: mpsc::Sender<Input>) -> JoinHandle<()> {
    tokio::spawn(run(offer, inputs))
}

async fn run(mut offer: watch::Receiver<Offer>, inputs: mpsc::Sender<Input>) {
    loop {
        let bind = offer.borrow().bind;
        let Some(bind) = bind else {
            if offer.changed().await.is_err() {
                return;
            }
            continue;
        };
        let listener = match TcpListener::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                warn!("Cannot listen for configuration sync on {bind}: {e}; retrying in 10s");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                    r = offer.changed() => if r.is_err() { return },
                }
                continue;
            }
        };
        info!("Serving the mesh configuration to peers on {bind} (TCP)");
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((stream, from)) = accepted {
                        let current = offer.borrow().clone();
                        let inputs = inputs.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve(stream, from, &current, &inputs).await {
                                debug!("sync request from {from} failed: {e:#}");
                            }
                        });
                    }
                }
                r = offer.changed() => {
                    if r.is_err() {
                        return;
                    }
                    if offer.borrow().bind != Some(bind) {
                        break;
                    }
                }
            }
        }
    }
}

async fn serve(
    stream: TcpStream,
    from: SocketAddr,
    offer: &Offer,
    inputs: &mpsc::Sender<Input>,
) -> Result<()> {
    let SocketAddr::V4(from) = from else {
        bail!("IPv6 is not supported");
    };
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    tokio::time::timeout(
        TIMEOUT,
        BufReader::new(read.take(MAX_LINE)).read_line(&mut line),
    )
    .await
    .context("timeout")??;
    let line = line.trim();
    if line == "MESH" {
        if !offer.allowed.contains(from.ip()) {
            bail!("{from} is not a peer");
        }
        write.write_all(offer.mesh.as_bytes()).await?;
    } else if let Some(json) = line.strip_prefix("JOIN ") {
        let reply = match serde_json::from_str::<JoinRequest>(json) {
            Ok(request) => ask_controller(*from.ip(), request, inputs).await,
            Err(e) => JoinReply::Error {
                message: format!("invalid join request: {e}"),
            },
        };
        let mut text = serde_json::to_string(&reply).context("cannot encode the answer")?;
        text.push('\n');
        write.write_all(text.as_bytes()).await?;
    } else {
        bail!("unknown request {line:?}");
    }
    write.shutdown().await?;
    Ok(())
}

async fn ask_controller(
    from: Ipv4Addr,
    request: JoinRequest,
    inputs: &mpsc::Sender<Input>,
) -> JoinReply {
    let (reply, answer) = oneshot::channel();
    let unavailable = || JoinReply::Error {
        message: "the inviting site is shutting down".into(),
    };
    if inputs
        .send(Input::JoinRequest {
            from,
            request,
            reply,
        })
        .await
        .is_err()
    {
        return unavailable();
    }
    match tokio::time::timeout(JOIN_TIMEOUT, answer).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(_)) => unavailable(),
        Err(_) => JoinReply::Error {
            message: "the inviting site did not answer in time".into(),
        },
    }
}

/// Asks the inviting site to let this site join.
pub async fn join(target: SocketAddrV4, request: &JoinRequest) -> Result<JoinReply> {
    let mut stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(target))
        .await
        .with_context(|| format!("timeout while connecting to {target}"))?
        .with_context(|| {
            format!("cannot connect to {target} (is wanderland running there, and is the control port reachable?)")
        })?;
    let mut line = serde_json::to_string(request).context("cannot encode the request")?;
    line.insert_str(0, "JOIN ");
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    let mut text = String::new();
    tokio::time::timeout(
        JOIN_TIMEOUT + TIMEOUT,
        BufReader::new((&mut stream).take(MAX_SIZE)).read_line(&mut text),
    )
    .await
    .with_context(|| format!("timeout while waiting for {target}"))??;
    if text.trim().is_empty() {
        bail!(
            "{target} closed the connection without an answer (is it an older wanderland without invitations?)"
        );
    }
    serde_json::from_str(text.trim()).with_context(|| format!("{target} sent an invalid answer"))
}

/// Fetches the mesh configuration from a peer.
pub async fn fetch(peer: SocketAddrV4) -> Result<String> {
    let mut stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(peer))
        .await
        .context("timeout while connecting")?
        .with_context(|| format!("cannot connect to {peer}"))?;
    stream.write_all(b"MESH\n").await?;
    let mut text = String::new();
    tokio::time::timeout(
        TIMEOUT,
        (&mut stream).take(MAX_SIZE).read_to_string(&mut text),
    )
    .await
    .context("timeout while reading")??;
    if text.trim().is_empty() {
        bail!("{peer} sent an empty configuration");
    }
    Ok(text)
}
