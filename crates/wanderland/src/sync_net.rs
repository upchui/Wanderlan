//! Configuration sync and joining over TCP on `<address>:<control_port>`.
//!
//! * `NETWORK`: the server answers with the confirmed `network.toml` as the asking
//!   peer may see it (canonical form; a client gets only its view, see
//!   `wanderlan_core::view`) and closes the connection. Only direct peers
//!   (and peers removed within the last minutes) are served.
//! * `JOIN <json>` ([`JoinRequest`]): a new site asks to join with an
//!   invitation code; open to every address, the controller checks the code.
//!   The answer is one JSON line ([`JoinReply`]).
//! * `SYNC <id> <nonce>`: like `NETWORK`, authenticated with the sync key of
//!   the tunnel ([`wanderlan_core::identity::PairKeys`]), between sites that
//!   know each other's identity. Both send a nonce; each side proves its key
//!   with a tag over both nonces and its body, so neither a replay nor a box
//!   at a peer's address gets or gives a network:
//!
//!   ```text
//!   C: SYNC <client id> <client nonce>
//!   S: OK <server id> <server nonce>        (or: NO <reason>)
//!   C: <tag> <length>\n<body>               (body: empty, for now)
//!   S: <tag> <length>\n<body>               (the network as the client may see it)
//!   ```

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use wanderlan_core::identity::{PairKeys, SiteId, TAG_LEN};
use wanderlan_core::join::{JoinReply, JoinRequest};

use crate::daemon::Input;

/// Largest accepted network.toml.
const MAX_SIZE: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Longest accepted request line.
const MAX_LINE: u64 = 4096;
/// Time the controller may take to answer a join request.
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Connections served at the same time at most; more are closed at once.
const MAX_CONNECTIONS: usize = 64;
/// The longest one connection may take, whatever it does.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);

/// Bytes of a sync nonce.
const NONCE_LEN: usize = 16;
const LABEL_REQUEST: &[u8] = b"wanderlan sync request";
const LABEL_REPLY: &[u8] = b"wanderlan sync reply";

/// What the server offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offer {
    /// Address to listen on.
    pub bind: Option<SocketAddrV4>,
    /// What each allowed address gets: the confirmed network.toml as that
    /// peer may see it (shared, so every connection does not copy it).
    pub views: Arc<BTreeMap<Ipv4Addr, String>>,
    /// This box's identity, for `SYNC`.
    pub me: Option<SiteId>,
    /// The peers whose identity is known, by identity: where they are and
    /// the keys towards them. They also get their view with `SYNC`.
    pub keyed: Arc<BTreeMap<SiteId, Keyed>>,
}

/// A peer whose identity is known.
#[derive(Debug, Clone)]
pub struct Keyed {
    /// Its address (its view in [`Offer::views`]).
    pub address: Ipv4Addr,
    /// The keys of the tunnel to it.
    pub keys: Arc<PairKeys>,
}

impl PartialEq for Keyed {
    // The keys follow from the identities.
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
    }
}

impl Eq for Keyed {}

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
        info!("Serving the network configuration to peers on {bind} (TCP)");
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, from)) => {
                        // Too many at once (a flood): close this one.
                        let Ok(slot) = slots.clone().try_acquire_owned() else {
                            debug!("sync request from {from} refused: too many connections");
                            continue;
                        };
                        let current = offer.borrow().clone();
                        let inputs = inputs.clone();
                        tokio::spawn(async move {
                            let _slot = slot;
                            match tokio::time::timeout(
                                CONNECTION_TIMEOUT,
                                serve(stream, from, &current, &inputs),
                            )
                            .await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) => debug!("sync request from {from} failed: {e:#}"),
                                Err(_) => debug!("sync request from {from} took too long"),
                            }
                        });
                    }
                    // E.g. out of file descriptors: no busy loop.
                    Err(e) => {
                        debug!("sync accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                },
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
    let mut reader = BufReader::new(read);
    let line = read_line(&mut reader).await?;
    let line = line.trim();
    if let Some(args) = line.strip_prefix("SYNC ") {
        return serve_sync(args, from, offer, &mut reader, &mut write).await;
    }
    if line == "NETWORK" {
        let Some(view) = offer.views.get(from.ip()) else {
            bail!("{from} is not a peer");
        };
        write.write_all(view.as_bytes()).await?;
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

/// One line of at most [`MAX_LINE`] bytes.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<String> {
    let mut line = String::new();
    tokio::time::timeout(TIMEOUT, reader.take(MAX_LINE).read_line(&mut line))
        .await
        .context("timeout")??;
    Ok(line)
}

/// `<tag> <length>` and the body after it.
async fn read_tagged<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<([u8; TAG_LEN], Vec<u8>)> {
    let line = read_line(reader).await?;
    let mut parts = line.split_whitespace();
    let tag = parts
        .next()
        .and_then(hex_decode::<TAG_LEN>)
        .context("no valid tag")?;
    let len: u64 = parts
        .next()
        .and_then(|l| l.parse().ok())
        .filter(|l| *l <= MAX_SIZE)
        .context("no valid length")?;
    let mut body = Vec::new();
    tokio::time::timeout(TIMEOUT, reader.take(len).read_to_end(&mut body))
        .await
        .context("timeout while reading")??;
    if body.len() as u64 != len {
        bail!("the connection ended early");
    }
    Ok((tag, body))
}

/// Serves `SYNC <client id> <client nonce>`.
async fn serve_sync<R, W>(
    args: &str,
    from: SocketAddrV4,
    offer: &Offer,
    reader: &mut R,
    write: &mut W,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut parts = args.split_whitespace();
    let client = parts.next().and_then(|t| t.parse::<SiteId>().ok());
    let client_nonce = parts.next().and_then(hex_decode::<NONCE_LEN>);
    let found = client.and_then(|id| offer.keyed.get(&id).map(|k| (id, k)));
    let (Some(me), Some(client_nonce), Some((client, keyed))) = (offer.me, client_nonce, found)
    else {
        write.write_all(b"NO unknown identity\n").await?;
        bail!("{from} asked with an unknown identity");
    };
    if keyed.address != *from.ip() {
        write
            .write_all(b"NO not from the address of that site\n")
            .await?;
        bail!(
            "{from} asked as {}, which is at {}",
            client.fingerprint(),
            keyed.address
        );
    }
    let server_nonce = crate::random::bytes::<NONCE_LEN>()?;
    write
        .write_all(format!("OK {me} {}\n", hex_encode(&server_nonce)).as_bytes())
        .await?;
    let (tag, body) = read_tagged(reader).await?;
    if !keyed
        .keys
        .sync_verify(&[LABEL_REQUEST, &client_nonce, &server_nonce, &body], &tag)
    {
        bail!("{from} sent a wrong sync tag");
    }
    let Some(view) = offer.views.get(from.ip()) else {
        bail!("{from} is not a peer");
    };
    let tag = keyed
        .keys
        .sync_tag(&[LABEL_REPLY, &client_nonce, &server_nonce, view.as_bytes()]);
    write
        .write_all(format!("{} {}\n", hex_encode(&tag), view.len()).as_bytes())
        .await?;
    write.write_all(view.as_bytes()).await?;
    write.shutdown().await?;
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn hex_decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
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

/// Fetches the network configuration from a peer whose identity is known
/// (`SYNC`): only that peer can answer, and nothing replayed counts.
pub async fn fetch_sync(
    peer: SocketAddrV4,
    me: SiteId,
    expect: SiteId,
    keys: &PairKeys,
) -> Result<String> {
    let stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(peer))
        .await
        .context("timeout while connecting")?
        .with_context(|| format!("cannot connect to {peer}"))?;
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let client_nonce = crate::random::bytes::<NONCE_LEN>()?;
    write
        .write_all(format!("SYNC {me} {}\n", hex_encode(&client_nonce)).as_bytes())
        .await?;
    let line = read_line(&mut reader).await?;
    let line = line.trim();
    if let Some(reason) = line.strip_prefix("NO ") {
        bail!("{peer} refused the authenticated sync: {reason}");
    }
    let mut parts = line
        .strip_prefix("OK ")
        .unwrap_or_default()
        .split_whitespace();
    let server = parts.next().and_then(|t| t.parse::<SiteId>().ok());
    let server_nonce = parts.next().and_then(hex_decode::<NONCE_LEN>);
    let (Some(server), Some(server_nonce)) = (server, server_nonce) else {
        bail!("{peer} does not speak the authenticated sync (an older wanderland?)");
    };
    if server != expect {
        bail!(
            "{peer} answers with identity {}, not {}: another box at that address?",
            server.fingerprint(),
            expect.fingerprint()
        );
    }
    let tag = keys.sync_tag(&[LABEL_REQUEST, &client_nonce, &server_nonce, b""]);
    write
        .write_all(format!("{} 0\n", hex_encode(&tag)).as_bytes())
        .await?;
    let (tag, body) = read_tagged(&mut reader).await?;
    if !keys.sync_verify(&[LABEL_REPLY, &client_nonce, &server_nonce, &body], &tag) {
        bail!("{peer} sent the network with a wrong tag; not used");
    }
    let text = String::from_utf8(body).context("the network is not text")?;
    if text.trim().is_empty() {
        bail!("{peer} sent an empty configuration");
    }
    Ok(text)
}

/// Fetches the network configuration from a peer.
pub async fn fetch(peer: SocketAddrV4) -> Result<String> {
    let mut stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(peer))
        .await
        .context("timeout while connecting")?
        .with_context(|| format!("cannot connect to {peer}"))?;
    stream.write_all(b"NETWORK\n").await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use wanderlan_core::identity::Identity;

    #[tokio::test]
    async fn the_authenticated_sync_needs_both_keys() {
        let a = Identity::from_secret([1; 32]);
        let b = Identity::from_secret([2; 32]);
        let c = Identity::from_secret([3; 32]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        let offer = Offer {
            bind: None,
            views: Arc::new([(Ipv4Addr::LOCALHOST, "the network\n".to_string())].into()),
            me: Some(b.id()),
            keyed: Arc::new(
                [(
                    a.id(),
                    Keyed {
                        address: Ipv4Addr::LOCALHOST,
                        keys: Arc::new(b.pair(&a.id()).unwrap()),
                    },
                )]
                .into(),
            ),
        };
        let (inputs, _rx) = mpsc::channel(1);
        let server = tokio::spawn(async move {
            loop {
                let (stream, from) = listener.accept().await.unwrap();
                let _ = serve(stream, from, &offer, &inputs).await;
            }
        });
        let ab = a.pair(&b.id()).unwrap();
        assert_eq!(
            fetch_sync(addr, a.id(), b.id(), &ab).await.unwrap(),
            "the network\n"
        );
        // Another box answers at b's address.
        let err = fetch_sync(addr, a.id(), c.id(), &a.pair(&c.id()).unwrap())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("another box"), "{err:#}");
        // c claims to be a, without a's key.
        assert!(
            fetch_sync(addr, a.id(), b.id(), &c.pair(&b.id()).unwrap())
                .await
                .is_err()
        );
        // An identity the server does not know.
        let err = fetch_sync(addr, c.id(), b.id(), &c.pair(&b.id()).unwrap())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown identity"), "{err:#}");
        // The plain request still works for peers without identity.
        assert_eq!(fetch(addr).await.unwrap(), "the network\n");
        server.abort();
    }

    #[test]
    fn hex_round_trip() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex_encode(&bytes), "0001abff");
        assert_eq!(hex_decode::<4>("0001abff"), Some(bytes));
        assert_eq!(hex_decode::<4>("0001abf"), None);
        assert_eq!(hex_decode::<4>("0001abfg"), None);
    }
}
