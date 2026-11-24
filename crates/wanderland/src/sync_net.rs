//! Configuration sync over TCP on `<address>:<control_port>`.
//!
//! Protocol: the client sends the line `MESH`, the server answers with its
//! confirmed `mesh.toml` (canonical form) and closes the connection. Only
//! direct peers (and peers removed within the last minutes) are served.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Largest accepted mesh.toml.
const MAX_SIZE: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

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
pub fn spawn(offer: watch::Receiver<Offer>) -> JoinHandle<()> {
    tokio::spawn(run(offer))
}

async fn run(mut offer: watch::Receiver<Offer>) {
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
                        tokio::spawn(async move {
                            if let Err(e) = serve(stream, from, &current).await {
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

async fn serve(stream: TcpStream, from: SocketAddr, offer: &Offer) -> Result<()> {
    let SocketAddr::V4(from) = from else {
        bail!("IPv6 is not supported");
    };
    if !offer.allowed.contains(from.ip()) {
        bail!("{from} is not a peer");
    }
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    tokio::time::timeout(TIMEOUT, BufReader::new(read).read_line(&mut line))
        .await
        .context("timeout")??;
    if line.trim() != "MESH" {
        bail!("unknown request {line:?}");
    }
    write.write_all(offer.mesh.as_bytes()).await?;
    write.shutdown().await?;
    Ok(())
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
