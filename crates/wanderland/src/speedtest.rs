//! Speed test between two sites (`wanderlan speedtest <site>`): TCP between
//! the boxes over the transport network, and frames through the tunnel.
//!
//! Every measurement is a connection of its own to the control port of the
//! peer, authenticated like `SYNC` (see [`crate::sync_net`]) with the sync
//! key of the tunnel:
//!
//! ```text
//! C: SPEED <client id> <client nonce> <what> <seconds> <frame size> <magic>
//! S: OK <server id> <server nonce>              (or: NO <reason>)
//! C: <tag> 0                                     (over both nonces and the request)
//! ```
//!
//! then, by `<what>` (S answers tagged, like `SYNC`):
//!
//! * `tcp-up`: C sends for `<seconds>`, then closes its side; S answers
//!   `<bytes> <micros> <core> <share>`.
//! * `tcp-down`: S sends for `<seconds>`, then closes.
//! * `tunnel-up`: S opens a tap on its tunnel interface to C and answers
//!   `READY`; C sends frames through its tunnel interface for `<seconds>`,
//!   then `DONE`; S answers `<arrived> <core> <share>`.
//! * `tunnel-down`: C has a tap open; S sends frames for `<seconds>` and
//!   answers `<sent> <micros>`.
//! * `rps-on`, `rps-off`: S sets RPS of the interface the tunnel to C comes
//!   in by, for a measurement ([`LEASE`]); answers `OK <interface>` or
//!   `ERR <why>`.
//! * `keep-rps-on`, `keep-rps-off` (with `<off bit/s>/<on bit/s>` after the
//!   magic): S keeps that setting, as measured; answers like `rps-on`.
//!
//! The frames are untagged IPv4/UDP to port 9 with the magic of the test at
//! the start of the payload, in [`FLOWS`] flows (source ports): the tunnel
//! packets of a flow get a source port of their own, as those of real
//! traffic do. The bridge of the receiver drops them at the tunnel port (no
//! VLAN), the tap counts them before (in the kernel, also those it has no
//! room for). They are sent as fast as the sender can: what arrives per
//! second is what the way takes. `<core> <share>`: the busiest core of the
//! receiver and its share of network work (softirq) in percent.
//!
//! A calibration ([`calibrate`]) measures both ways with RPS off and on, on
//! both boxes, a few times each; each box keeps the setting it received
//! more with (on only if clearly more).

use std::net::SocketAddrV4;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::io::{
    AsyncBufRead, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::Instant;
use tracing::info;
use wanderlan_core::api::{SpeedRun, Speedtest};
use wanderlan_core::identity::{PairKeys, SiteId};
use wanderlan_core::mtu;

use crate::perf::CpuTimes;
use crate::rawframe::{Sender, TEST_PORT, Tap};
use crate::sync_net::{
    LINE_TIMEOUT, NONCE_LEN, Offer, TIMEOUT, hex_decode, hex_encode, read_line, read_tagged,
};

const LABEL_REQUEST: &[u8] = b"wanderlan speed request";
const LABEL_REPLY: &[u8] = b"wanderlan speed reply";
/// The flows of the test frames.
const FLOWS: u16 = 64;
/// How long a setting for a measurement holds on the peer at most.
const LEASE: Duration = Duration::from_secs(60);
/// Measurements per setting and direction in a calibration.
const RUNS: usize = 3;
/// Seconds per measurement of a calibration.
const RUN_SECONDS: u32 = 2;
/// Destination of the test frames: locally administered unicast nobody
/// sends from.
const FRAME_DST: [u8; 6] = [0x02, 0x77, 0x6c, 0x73, 0x74, 0x00];
/// Source of the test frames.
const FRAME_SRC: [u8; 6] = [0x06, 0x77, 0x6c, 0x73, 0x74, 0x01];
/// The largest test frame: a full Ethernet frame without VLAN tag.
pub const FRAME_MAX: u16 = 1514;
/// The smallest test frame.
const FRAME_MIN: u16 = 64;
/// How long frames may still be on the way after the sender stopped.
const SETTLE: Duration = Duration::from_millis(300);
/// What TCP writes at a time.
const CHUNK: usize = 64 * 1024;

/// What a box answers while it runs another test.
const BUSY_TEXT: &str = "another speed test is running";
/// How often a client asks again while the peer is busy, and the pause.
const BUSY_TRIES: u32 = 20;
const BUSY_PAUSE: Duration = Duration::from_millis(250);

/// One test at a time on a box (it would measure the other one too).
static BUSY: AtomicBool = AtomicBool::new(false);

/// This box runs a test; given back when dropped.
struct Busy;

impl Busy {
    fn take() -> Option<Self> {
        BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

/// What is measured (or set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    TcpUp,
    TcpDown,
    TunnelUp,
    TunnelDown,
    Rps(bool),
    KeepRps(bool),
}

impl What {
    const ALL: [Self; 8] = [
        Self::TcpUp,
        Self::TcpDown,
        Self::TunnelUp,
        Self::TunnelDown,
        Self::Rps(true),
        Self::Rps(false),
        Self::KeepRps(true),
        Self::KeepRps(false),
    ];

    fn name(self) -> &'static str {
        match self {
            Self::TcpUp => "tcp-up",
            Self::TcpDown => "tcp-down",
            Self::TunnelUp => "tunnel-up",
            Self::TunnelDown => "tunnel-down",
            Self::Rps(true) => "rps-on",
            Self::Rps(false) => "rps-off",
            Self::KeepRps(true) => "keep-rps-on",
            Self::KeepRps(false) => "keep-rps-off",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|w| w.name() == text)
    }
}

/// One measurement as the client asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ask {
    what: What,
    seconds: u32,
    size: u16,
    magic: [u8; 4],
    /// What was measured (`<off bit/s>/<on bit/s>`, `keep-rps-*` only).
    note: Option<(u64, u64)>,
}

impl Ask {
    /// A setting of RPS (no measurement).
    fn rps(what: What, note: Option<(u64, u64)>) -> Self {
        Self {
            what,
            seconds: 1,
            size: FRAME_MAX,
            magic: [0; 4],
            note,
        }
    }

    fn line(&self) -> String {
        let mut line = format!(
            "{} {} {} {}",
            self.what.name(),
            self.seconds,
            self.size,
            hex_encode(&self.magic)
        );
        if let Some((off, on)) = self.note {
            line.push_str(&format!(" {off}/{on}"));
        }
        line
    }

    fn parse<'a>(mut parts: impl Iterator<Item = &'a str>) -> Option<Self> {
        let what = What::parse(parts.next()?)?;
        let seconds: u32 = parts.next()?.parse().ok()?;
        let size: u16 = parts.next()?.parse().ok()?;
        let magic = hex_decode::<4>(parts.next()?)?;
        let note = match parts.next() {
            Some(note) => {
                let (off, on) = note.split_once('/')?;
                Some((off.parse().ok()?, on.parse().ok()?))
            }
            None => None,
        };
        ((1..=wanderlan_core::api::SPEEDTEST_MAX_SECONDS).contains(&seconds)
            && (FRAME_MIN..=FRAME_MAX).contains(&size))
        .then_some(Self {
            what,
            seconds,
            size,
            magic,
            note,
        })
    }

    fn duration(&self) -> Duration {
        Duration::from_secs(u64::from(self.seconds))
    }
}

/// The test frames of `size` bytes with `magic`: one per flow (source
/// port), IPv4/UDP to [`TEST_PORT`] (addresses of the benchmark range,
/// RFC 2544).
fn frames(size: u16, magic: [u8; 4]) -> Vec<Vec<u8>> {
    (0..FLOWS)
        .map(|flow| frame(size, magic, 40_000 + flow))
        .collect()
}

fn frame(size: u16, magic: [u8; 4], source_port: u16) -> Vec<u8> {
    let mut frame = vec![0u8; usize::from(size)];
    frame[..6].copy_from_slice(&FRAME_DST);
    frame[6..12].copy_from_slice(&FRAME_SRC);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    // IPv4: no options, no fragment, TTL 64, UDP.
    let ip_len = size - 14;
    frame[14] = 0x45;
    frame[16..18].copy_from_slice(&ip_len.to_be_bytes());
    frame[22] = 64;
    frame[23] = 17;
    frame[26..30].copy_from_slice(&[198, 18, 0, 1]);
    frame[30..34].copy_from_slice(&[198, 18, 0, 2]);
    let sum = frame[14..34]
        .chunks(2)
        .map(|w| u32::from(u16::from_be_bytes([w[0], w[1]])))
        .sum::<u32>();
    let sum = (sum & 0xffff) + (sum >> 16);
    let sum = (sum & 0xffff) + (sum >> 16);
    frame[24..26].copy_from_slice(&(!u16::try_from(sum).unwrap_or(0)).to_be_bytes());
    // UDP without checksum, the magic first.
    frame[34..36].copy_from_slice(&source_port.to_be_bytes());
    frame[36..38].copy_from_slice(&TEST_PORT.to_be_bytes());
    frame[38..40].copy_from_slice(&(ip_len - 20).to_be_bytes());
    frame[42..46].copy_from_slice(&magic);
    frame
}

/// Frames per call of the sender.
const BATCH: usize = 32;
/// Threads that send at most.
const SENDERS: usize = 4;

/// Sends `frames` through `ifname` for `seconds`, as fast as it can, from up
/// to [`SENDERS`] threads, each going through the flows (blocks): how many,
/// and how long it took.
fn blast(ifname: &str, frames: &[Vec<u8>], seconds: u32) -> std::io::Result<(u64, Duration)> {
    let threads = crate::tuning::online_cpus().clamp(1, SENDERS);
    let senders = (0..threads)
        .map(|_| Sender::open(ifname, FRAME_DST))
        .collect::<std::io::Result<Vec<Sender>>>()?;
    let start = std::time::Instant::now();
    let until = start + Duration::from_secs(u64::from(seconds));
    let results: Vec<std::io::Result<u64>> = std::thread::scope(|scope| {
        let handles: Vec<_> = senders
            .iter()
            .enumerate()
            .map(|(i, sender)| scope.spawn(move || send_until(sender, frames, i, until)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(std::io::Error::other("a sender died")))
            })
            .collect()
    });
    let took = start.elapsed();
    let mut sent = 0;
    for result in results {
        sent += result?;
    }
    Ok((sent, took))
}

/// One sender of [`blast`], starting with flow `first`.
fn send_until(
    sender: &Sender,
    frames: &[Vec<u8>],
    first: usize,
    until: std::time::Instant,
) -> std::io::Result<u64> {
    let mut sent = 0u64;
    let mut flow = first;
    while std::time::Instant::now() < until {
        flow = (flow + 1) % frames.len().max(1);
        let Some(frame) = frames.get(flow) else {
            break;
        };
        match sender.send_many(frame, BATCH) {
            Ok(n) => sent += n as u64,
            // The queue is full: a moment.
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOBUFS | libc::EAGAIN)) => {
                std::thread::yield_now();
            }
            Err(e) => return Err(e),
        }
    }
    Ok(sent)
}

/// Reads until the other side closes (within `limit`): the bytes and the
/// time from the first one.
async fn receive<R: AsyncRead + Unpin>(reader: &mut R, limit: Duration) -> Result<(u64, Duration)> {
    let mut buf = vec![0u8; CHUNK];
    let (mut bytes, mut first) = (0u64, None);
    let deadline = Instant::now() + limit;
    loop {
        let n = tokio::time::timeout_at(deadline, reader.read(&mut buf))
            .await
            .context("the measurement took too long")??;
        if n == 0 {
            break;
        }
        first.get_or_insert_with(Instant::now);
        bytes += n as u64;
    }
    Ok((bytes, first.map_or(Duration::ZERO, |f| f.elapsed())))
}

/// Writes for `duration`, then closes the sending side.
async fn send_for<W: AsyncWrite + Unpin>(write: &mut W, duration: Duration) -> Result<()> {
    let buf = vec![0u8; CHUNK];
    let until = Instant::now() + duration;
    while Instant::now() < until {
        write.write_all(&buf).await?;
    }
    write.shutdown().await?;
    Ok(())
}

/// `<core> <share>` of the busiest core, `- -` without.
fn core_text(core: Option<(u32, u32, u32)>) -> String {
    core.map_or_else(|| "- -".into(), |(cpu, share, _)| format!("{cpu} {share}"))
}

fn parse_core<'a>(mut parts: impl Iterator<Item = &'a str>) -> Option<(u32, u32)> {
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Bits per second of `bytes` in `took`.
fn rate(bytes: u64, took: Duration) -> u64 {
    let micros = took.as_micros().max(1);
    u64::try_from(u128::from(bytes) * 8 * 1_000_000 / micros).unwrap_or(u64::MAX)
}

// ---- the side that is asked -------------------------------------------------

/// What a peer that calibrates the tunnel to this box did here (for the
/// status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunedBy {
    /// It set RPS here for a measurement.
    Measuring,
    /// It kept the result here (what this box keeps, in words).
    Kept(String),
    /// RPS cannot be changed here.
    Refused(String),
}

/// Serves `SPEED <client id> <client nonce> <measurement>`.
pub async fn serve<R, W>(
    args: &str,
    from: SocketAddrV4,
    offer: &Offer,
    inputs: &tokio::sync::mpsc::Sender<crate::daemon::Input>,
    reader: &mut R,
    write: &mut W,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut parts = args.split_whitespace();
    let client = parts.next().and_then(|t| t.parse::<SiteId>().ok());
    let client_nonce = parts.next().and_then(hex_decode::<NONCE_LEN>);
    let ask = Ask::parse(&mut parts);
    let keyed = client.and_then(|id| offer.keyed.get(&id));
    let (Some(me), Some(client_nonce), Some(ask), Some(keyed)) =
        (offer.me, client_nonce, ask, keyed)
    else {
        write
            .write_all(b"NO unknown identity or measurement\n")
            .await?;
        bail!("{from} asked for a speed test with an unknown identity or measurement");
    };
    if !keyed.client && keyed.address != *from.ip() {
        write
            .write_all(b"NO not from the address of that site\n")
            .await?;
        bail!("{from} asked for a speed test as {}", keyed.name);
    }
    let Some(busy) = Busy::take() else {
        write
            .write_all(format!("NO {BUSY_TEXT}\n").as_bytes())
            .await?;
        return Ok(());
    };
    let server_nonce = crate::random::bytes::<NONCE_LEN>()?;
    write
        .write_all(format!("OK {me} {}\n", hex_encode(&server_nonce)).as_bytes())
        .await?;
    let (tag, _) = read_tagged(reader, LINE_TIMEOUT).await?;
    let keys = &keyed.keys;
    if !keys.sync_verify(
        &[
            LABEL_REQUEST,
            &client_nonce,
            &server_nonce,
            ask.line().as_bytes(),
        ],
        &tag,
    ) {
        bail!("{from} sent a wrong tag for a speed test");
    }
    let reply = |text: String| {
        let tag = keys.sync_tag(&[LABEL_REPLY, &client_nonce, &server_nonce, text.as_bytes()]);
        format!("{} {}\n{text}", hex_encode(&tag), text.len())
    };
    // The last answer, given after this box is free again: the client asks
    // for the next measurement as soon as it has it.
    let last = match ask.what {
        What::TcpUp => {
            let before = CpuTimes::now();
            let (bytes, took) = receive(reader, ask.duration() + TIMEOUT).await?;
            let core = CpuTimes::now().busiest_since(&before);
            Some(format!("{bytes} {} {}", took.as_micros(), core_text(core)))
        }
        What::TcpDown => {
            send_for(write, ask.duration()).await?;
            None
        }
        What::TunnelUp => {
            let tunnel = keyed.tunnel.clone().context("no tunnel to that site")?;
            let magic = ask.magic;
            let tap = tokio::task::spawn_blocking(move || Tap::open_test(&tunnel, magic)).await??;
            tap.taken()?;
            let before = CpuTimes::now();
            write.write_all(reply("READY".into()).as_bytes()).await?;
            let line = read_line(reader, ask.duration() + TIMEOUT).await?;
            if line.trim() != "DONE" {
                bail!("{from} did not finish the speed test");
            }
            tokio::time::sleep(SETTLE).await;
            let arrived = tap.taken()?;
            let core = CpuTimes::now().busiest_since(&before);
            Some(format!("{arrived} {}", core_text(core)))
        }
        What::TunnelDown => {
            let tunnel = keyed.tunnel.clone().context("no tunnel to that site")?;
            let (frames, seconds) = (frames(ask.size, ask.magic), ask.seconds);
            let (sent, took) =
                tokio::task::spawn_blocking(move || blast(&tunnel, &frames, seconds)).await??;
            Some(format!("{sent} {}", took.as_micros()))
        }
        What::Rps(on) | What::KeepRps(on) => {
            let result = match &keyed.underlay {
                None => Err("no transport interface to that site known".to_string()),
                Some(nic) if matches!(ask.what, What::Rps(_)) => {
                    crate::tuning::try_setting(nic, on, LEASE).map(|()| nic.clone())
                }
                // What this tunnel received here, with the others of the
                // NIC: the NIC keeps what all of them speak for (the peer's
                // `on` is only its view of this one tunnel).
                Some(nic) => {
                    let (off_bps, on_bps) = ask.note.unwrap_or_default();
                    let peer = client.map(|c| c.to_string()).unwrap_or_default();
                    crate::tuning::record(nic, &peer, off_bps, on_bps).map(|d| {
                        let text = side_text(nic, d.on, (off_bps, on_bps), d);
                        info!(
                            "Calibrated the receive work with \"{}\": here {text}",
                            keyed.name
                        );
                        format!(
                            "{nic} {} {} {} {}",
                            if d.on { "on" } else { "off" },
                            d.tunnels,
                            d.off_bps,
                            d.on_bps
                        )
                    })
                }
            };
            let event = match (&result, ask.what) {
                (Err(why), _) => TunedBy::Refused(why.clone()),
                (Ok(_), What::Rps(_)) => TunedBy::Measuring,
                (Ok(kept), _) => {
                    let (off_bps, on_bps) = ask.note.unwrap_or_default();
                    TunedBy::Kept(parse_kept(kept).map_or_else(String::new, |(nic, d)| {
                        side_text(&nic, d.on, (off_bps, on_bps), d)
                    }))
                }
            };
            if let Some(peer) = client {
                let _ = inputs
                    .send(crate::daemon::Input::TunedBy { peer, event })
                    .await;
            }
            Some(match result {
                Ok(nic) => format!("OK {nic}"),
                Err(why) => format!("ERR {why}"),
            })
        }
    };
    drop(busy);
    if let Some(text) = last {
        write.write_all(reply(text).as_bytes()).await?;
    }
    write.shutdown().await?;
    Ok(())
}

// ---- the side that asks -----------------------------------------------------

/// The peer of a speed test.
#[derive(Clone)]
pub struct Peer {
    /// Its name.
    pub name: String,
    /// Its control port.
    pub control: SocketAddrV4,
    /// This box.
    pub me: SiteId,
    /// Its identity.
    pub id: SiteId,
    /// The keys of the tunnel to it.
    pub keys: Arc<PairKeys>,
    /// The tunnel interface to it.
    pub tunnel: String,
    /// The measured path MTU to it.
    pub pmtu: Option<u16>,
}

/// An authenticated connection for one measurement.
struct Conn {
    reader: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
    client_nonce: [u8; NONCE_LEN],
    server_nonce: [u8; NONCE_LEN],
}

impl Peer {
    /// [`Peer::connect_once`], again for a moment while the peer still
    /// finishes the measurement before.
    async fn connect(&self, ask: &Ask) -> Result<Conn> {
        let mut tries = 0;
        loop {
            match self.connect_once(ask).await {
                Err(e) if tries < BUSY_TRIES && format!("{e}").contains(BUSY_TEXT) => {
                    tries += 1;
                    tokio::time::sleep(BUSY_PAUSE).await;
                }
                result => return result,
            }
        }
    }

    async fn connect_once(&self, ask: &Ask) -> Result<Conn> {
        let stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(self.control))
            .await
            .context("timeout while connecting")?
            .with_context(|| format!("cannot connect to {}", self.control))?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let client_nonce = crate::random::bytes::<NONCE_LEN>()?;
        write
            .write_all(
                format!(
                    "SPEED {} {} {}\n",
                    self.me,
                    hex_encode(&client_nonce),
                    ask.line()
                )
                .as_bytes(),
            )
            .await?;
        let line = read_line(&mut reader, TIMEOUT).await?;
        let line = line.trim();
        if line.is_empty() {
            bail!(
                "\"{}\" does not know the speed test (an older version of wanderlan?)",
                self.name
            );
        }
        if let Some(reason) = line.strip_prefix("NO ") {
            bail!("\"{}\" refused: {reason}", self.name);
        }
        let mut parts = line
            .strip_prefix("OK ")
            .unwrap_or_default()
            .split_whitespace();
        let server = parts.next().and_then(|t| t.parse::<SiteId>().ok());
        let server_nonce = parts.next().and_then(hex_decode::<NONCE_LEN>);
        let (Some(server), Some(server_nonce)) = (server, server_nonce) else {
            bail!("\"{}\" answered something else: {line}", self.name);
        };
        if server != self.id {
            bail!(
                "{} answers with another identity: another box at that address?",
                self.control
            );
        }
        let tag = self.keys.sync_tag(&[
            LABEL_REQUEST,
            &client_nonce,
            &server_nonce,
            ask.line().as_bytes(),
        ]);
        write
            .write_all(format!("{} 0\n", hex_encode(&tag)).as_bytes())
            .await?;
        Ok(Conn {
            reader,
            write,
            client_nonce,
            server_nonce,
        })
    }

    async fn reply(&self, conn: &mut Conn, wait: Duration) -> Result<String> {
        let (tag, text) = read_tagged(&mut conn.reader, wait).await?;
        if !self.keys.sync_verify(
            &[LABEL_REPLY, &conn.client_nonce, &conn.server_nonce, &text],
            &tag,
        ) {
            bail!("\"{}\" answered with a wrong tag", self.name);
        }
        String::from_utf8(text).context("the answer is not text")
    }

    /// Sets RPS on the peer: for a measurement (`keep` none), or keeps it
    /// with what was measured. The interface there, or why not.
    async fn rps(&self, on: bool, keep: Option<(u64, u64)>) -> Result<Result<String, String>> {
        let what = if keep.is_some() {
            What::KeepRps(on)
        } else {
            What::Rps(on)
        };
        let mut conn = self.connect(&Ask::rps(what, keep)).await?;
        let text = self.reply(&mut conn, TIMEOUT).await?;
        Ok(
            match (text.strip_prefix("OK "), text.strip_prefix("ERR ")) {
                (Some(nic), _) => Ok(nic.to_string()),
                (None, Some(why)) => Err(why.to_string()),
                (None, None) => Err(text),
            },
        )
    }

    async fn tcp(&self, up: bool, seconds: u32) -> Result<SpeedRun> {
        let ask = Ask {
            what: if up { What::TcpUp } else { What::TcpDown },
            seconds,
            size: FRAME_MAX,
            magic: [0; 4],
            note: None,
        };
        let mut conn = self.connect(&ask).await?;
        let (bytes, took, core) = if up {
            send_for(&mut conn.write, ask.duration()).await?;
            let text = self.reply(&mut conn, TIMEOUT).await?;
            let mut parts = text.split_whitespace();
            let bytes: u64 = parts
                .next()
                .and_then(|b| b.parse().ok())
                .context("no bytes")?;
            let micros: u64 = parts
                .next()
                .and_then(|m| m.parse().ok())
                .context("no time")?;
            (bytes, Duration::from_micros(micros), parse_core(parts))
        } else {
            let before = CpuTimes::now();
            let (bytes, took) = receive(&mut conn.reader, ask.duration() + TIMEOUT).await?;
            let core = CpuTimes::now()
                .busiest_since(&before)
                .map(|(c, s, _)| (c, s));
            (bytes, took, core)
        };
        Ok(SpeedRun {
            frame: None,
            up,
            bits_per_second: rate(bytes, took),
            sent: None,
            arrived: None,
            busiest_core: core,
            error: None,
        })
    }

    async fn tunnel(&self, up: bool, seconds: u32, size: u16) -> Result<SpeedRun> {
        let magic = crate::random::bytes::<4>()?;
        let ask = Ask {
            what: if up { What::TunnelUp } else { What::TunnelDown },
            seconds,
            size,
            magic,
            note: None,
        };
        let (sent, arrived, took, core) = if up {
            let mut conn = self.connect(&ask).await?;
            if self.reply(&mut conn, TIMEOUT).await? != "READY" {
                bail!("\"{}\" is not ready", self.name);
            }
            let (tunnel, frames) = (self.tunnel.clone(), frames(size, magic));
            let (sent, took) =
                tokio::task::spawn_blocking(move || blast(&tunnel, &frames, seconds)).await??;
            conn.write.write_all(b"DONE\n").await?;
            let text = self.reply(&mut conn, TIMEOUT).await?;
            let mut parts = text.split_whitespace();
            let arrived: u64 = parts
                .next()
                .and_then(|a| a.parse().ok())
                .context("no count")?;
            (sent, arrived, took, parse_core(parts))
        } else {
            let tunnel = self.tunnel.clone();
            let tap = tokio::task::spawn_blocking(move || Tap::open_test(&tunnel, magic)).await??;
            tap.taken()?;
            let before = CpuTimes::now();
            let mut conn = self.connect(&ask).await?;
            let text = self.reply(&mut conn, ask.duration() + TIMEOUT).await?;
            tokio::time::sleep(SETTLE).await;
            let arrived = tap.taken()?;
            let core = CpuTimes::now()
                .busiest_since(&before)
                .map(|(c, s, _)| (c, s));
            let mut parts = text.split_whitespace();
            let sent: u64 = parts
                .next()
                .and_then(|s| s.parse().ok())
                .context("no count")?;
            let micros: u64 = parts
                .next()
                .and_then(|m| m.parse().ok())
                .context("no time")?;
            (sent, arrived, Duration::from_micros(micros), core)
        };
        Ok(SpeedRun {
            frame: Some(size),
            up,
            bits_per_second: rate(arrived * u64::from(size), took),
            sent: Some(sent),
            arrived: Some(arrived),
            busiest_core: core,
            error: None,
        })
    }
}

/// A measurement that failed.
fn failed(frame: Option<u16>, up: bool, error: &anyhow::Error) -> SpeedRun {
    SpeedRun {
        frame,
        up,
        bits_per_second: 0,
        sent: None,
        arrived: None,
        busiest_core: None,
        error: Some(format!("{error:#}")),
    }
}

/// The speed test with `peer`: TCP and full frames through the tunnel both
/// ways, and frames that fit the path where full ones are split.
pub async fn run(peer: Peer, seconds: u32) -> Result<Speedtest, String> {
    let Some(_busy) = Busy::take() else {
        return Err("another speed test is running on this box".into());
    };
    let mut runs = Vec::new();
    for up in [true, false] {
        runs.push(
            peer.tcp(up, seconds)
                .await
                .unwrap_or_else(|e| failed(None, up, &e)),
        );
    }
    let mut sizes = vec![FRAME_MAX];
    if let Some(pmtu) = peer.pmtu
        && pmtu < mtu::PROBE_MAX
    {
        // What fits: the path minus the outer IP, UDP and VXLAN headers.
        let fits = pmtu.saturating_sub(mtu::OUTER_IP + mtu::UDP + mtu::VXLAN);
        if (FRAME_MIN..FRAME_MAX).contains(&fits) {
            sizes.push(fits);
        }
    }
    for size in sizes {
        for up in [true, false] {
            runs.push(
                peer.tunnel(up, seconds, size)
                    .await
                    .unwrap_or_else(|e| failed(Some(size), up, &e)),
            );
        }
    }
    Ok(Speedtest {
        peer: peer.name.clone(),
        address: peer.control.ip().to_string(),
        seconds,
        pmtu: peer.pmtu,
        runs,
    })
}

/// One measurement of a calibration: what arrived (bit/s). Tried twice; a
/// measurement that fails does not count as nothing arriving.
async fn measure(peer: &Peer, up: bool) -> Result<u64, String> {
    match peer.tunnel(up, RUN_SECONDS, FRAME_MAX).await {
        Ok(run) => Ok(run.bits_per_second),
        Err(_) => peer
            .tunnel(up, RUN_SECONDS, FRAME_MAX)
            .await
            .map(|run| run.bits_per_second)
            .map_err(|e| format!("a measurement failed: {e:#}")),
    }
}

/// What was measured for one box in a calibration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    /// The interface its tunnel packets come in by.
    pub nic: Option<String>,
    /// RPS on (none: it could not be changed there).
    pub rps: Option<bool>,
    /// What it received with RPS off and on (median, bit/s).
    pub off_bps: u64,
    /// See `off_bps`.
    pub on_bps: u64,
    /// Why RPS could not be changed there.
    pub why_not: Option<String>,
    /// What its NIC keeps from all of its tunnels.
    pub card: Option<crate::tuning::Decision>,
}

/// What a box keeps, in words: the NIC's setting, what this tunnel
/// measured, and with several tunnels on the NIC what all of them did.
pub fn side_text(
    nic: &str,
    on: bool,
    (off_bps, on_bps): (u64, u64),
    card: crate::tuning::Decision,
) -> String {
    let rps = if on { "on" } else { "off" };
    if card.tunnels > 1 {
        format!(
            "{nic} RPS {rps} (this tunnel: {}; {nic} over {} tunnels: {})",
            summary(off_bps, on_bps),
            card.tunnels,
            summary(card.off_bps, card.on_bps)
        )
    } else {
        format!("{nic} RPS {rps} ({})", summary(off_bps, on_bps))
    }
}

/// `<nic> <on|off> <tunnels> <off bit/s> <on bit/s>`: what the peer's NIC
/// keeps (the answer to `keep-rps-*`).
fn parse_kept(text: &str) -> Option<(String, crate::tuning::Decision)> {
    let mut parts = text.split_whitespace();
    let nic = parts.next()?.to_string();
    let on = match parts.next()? {
        "on" => true,
        "off" => false,
        _ => return None,
    };
    Some((
        nic,
        crate::tuning::Decision {
            on,
            tunnels: parts.next()?.parse().ok()?,
            off_bps: parts.next()?.parse().ok()?,
            on_bps: parts.next()?.parse().ok()?,
        },
    ))
}

/// The result of a calibration: this box and the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Calibration {
    /// This box.
    pub here: Side,
    /// The peer.
    pub there: Side,
}

/// The middle of the measurements.
fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0)
}

/// What this one tunnel speaks for (what an older peer keeps).
fn keep_on(off_bps: u64, on_bps: u64) -> bool {
    crate::tuning::choose(&[(off_bps, on_bps)])
}

/// What was measured, in words.
pub fn summary(off_bps: u64, on_bps: u64) -> String {
    let words = |bps: u64| {
        #[allow(clippy::cast_precision_loss)]
        let v = bps as f64;
        if v >= 1e9 {
            format!("{:.2} Gbit/s", v / 1e9)
        } else {
            format!("{:.0} Mbit/s", v / 1e6)
        }
    };
    format!("off {}, on {}", words(off_bps), words(on_bps))
}

/// Measures RPS off and on on the interface the tunnel packets of `peer`
/// come in by, here (`nic`) and there, both ways, [`RUNS`] times each
/// (alternating, so that a change of the load hits both alike); each box
/// keeps what it received more with. About 30 s of full load.
pub async fn calibrate(peer: Peer, nic: Option<String>) -> Result<Calibration, String> {
    let Some(_busy) = Busy::take() else {
        return Err("another speed test is running on this box".into());
    };
    let here_why = match &nic {
        Some(nic) => crate::tuning::cannot(nic),
        None => Some("no transport interface to that site known".into()),
    };
    let there = peer.rps(false, None).await.map_err(|e| format!("{e:#}"))?;
    let (there_nic, there_why) = match there {
        Ok(nic) => (Some(nic), None),
        Err(why) => (None, Some(why)),
    };
    if let (Some(here), Some(there)) = (&here_why, &there_why) {
        return Err(format!(
            "RPS cannot be changed here ({here}) nor at \"{}\" ({there})",
            peer.name
        ));
    }
    // [off, on] × runs: what this box received (down) and the peer (up).
    let mut down = [[0u64; RUNS]; 2];
    let mut up = [[0u64; RUNS]; 2];
    for run in 0..RUNS {
        for (i, on) in [false, true].into_iter().enumerate() {
            if let (Some(nic), None) = (&nic, &here_why) {
                crate::tuning::try_setting(nic, on, LEASE)?;
            }
            if there_why.is_none() {
                match peer.rps(on, None).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(why)) => return Err(format!("\"{}\": {why}", peer.name)),
                    Err(e) => return Err(format!("{e:#}")),
                }
            }
            up[i][run] = measure(&peer, true).await?;
            down[i][run] = measure(&peer, false).await?;
        }
    }
    let [mut down_off, mut down_on] = down;
    let [mut up_off, mut up_on] = up;
    let (here_off, here_on) = (median(&mut down_off), median(&mut down_on));
    let (there_off, there_on) = (median(&mut up_off), median(&mut up_on));
    if here_off.max(here_on).max(there_off).max(there_on) == 0 {
        return Err(format!(
            "no test frames arrived: the tunnel to \"{}\" is not up at both ends yet",
            peer.name
        ));
    }
    // Each box keeps what all the tunnels of its NIC speak for, this one
    // among them.
    let mut here = Side {
        nic: nic.clone(),
        rps: None,
        off_bps: here_off,
        on_bps: here_on,
        why_not: here_why.clone(),
        card: None,
    };
    if let (Some(nic), None) = (&nic, &here_why) {
        let decided = crate::tuning::record(nic, &peer.id.to_string(), here_off, here_on)?;
        here.rps = Some(decided.on);
        here.card = Some(decided);
    }
    let mut there = Side {
        nic: there_nic,
        rps: None,
        off_bps: there_off,
        on_bps: there_on,
        why_not: there_why.clone(),
        card: None,
    };
    if there_why.is_none() {
        let alone = keep_on(there_off, there_on);
        match peer.rps(alone, Some((there_off, there_on))).await {
            Ok(Ok(kept)) => match parse_kept(&kept) {
                Some((nic, decided)) => {
                    there.nic = Some(nic);
                    there.rps = Some(decided.on);
                    there.card = Some(decided);
                }
                // An older peer keeps what this tunnel speaks for.
                None => there.rps = Some(alone),
            },
            Ok(Err(why)) => there.why_not = Some(why),
            Err(e) => there.why_not = Some(format!("{e:#}")),
        }
    }
    Ok(Calibration { here, there })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_measurement_reads_back_and_frames_carry_the_magic() {
        let ask = Ask {
            what: What::TunnelUp,
            seconds: 5,
            size: 1464,
            magic: [1, 2, 3, 4],
            note: None,
        };
        assert_eq!(ask.line(), "tunnel-up 5 1464 01020304");
        assert_eq!(Ask::parse(ask.line().split_whitespace()), Some(ask));
        let keep = Ask::rps(What::KeepRps(false), Some((1_400_000_000, 900_000_000)));
        assert_eq!(
            keep.line(),
            "keep-rps-off 1 1514 00000000 1400000000/900000000"
        );
        assert_eq!(Ask::parse(keep.line().split_whitespace()), Some(keep));
        assert_eq!(
            Ask::parse("tunnel-up 60 1464 01020304".split_whitespace()),
            None
        );
        assert_eq!(
            Ask::parse("tcp-up 5 9000 01020304".split_whitespace()),
            None
        );
        // One frame per flow, where the filter of the tap looks.
        let all = frames(1464, [1, 2, 3, 4]);
        assert_eq!(all.len(), usize::from(FLOWS));
        let f = &all[5];
        assert_eq!(f.len(), 1464);
        assert_eq!(&f[12..14], &[0x08, 0x00]);
        assert_eq!(f[23], 17);
        assert_eq!(u16::from_be_bytes([f[34], f[35]]), 40_005);
        assert_eq!(u16::from_be_bytes([f[36], f[37]]), TEST_PORT);
        assert_eq!(&f[42..46], &[1, 2, 3, 4]);
        // A valid IPv4 header checksum: the sum over the header is 0xffff.
        let sum: u32 = f[14..34]
            .chunks(2)
            .map(|w| u32::from(u16::from_be_bytes([w[0], w[1]])))
            .sum();
        let folded = (sum & 0xffff) + (sum >> 16);
        assert_eq!((folded & 0xffff) + (folded >> 16), 0xffff);
        assert_ne!(all[0][34..36], all[1][34..36]);
        assert_eq!(rate(1_000_000, Duration::from_secs(1)), 8_000_000);
    }

    #[test]
    fn rps_stays_on_only_if_clearly_better() {
        assert_eq!(median(&mut [5, 1, 3]), 3);
        assert!(!keep_on(1_000, 1_050), "within the noise");
        assert!(keep_on(1_000, 1_100));
        assert!(!keep_on(1_000, 0));
        assert!(!keep_on(1_400, 900));
        assert_eq!(
            summary(1_400_000_000, 900_000_000),
            "off 1.40 Gbit/s, on 900 Mbit/s"
        );
    }
}
