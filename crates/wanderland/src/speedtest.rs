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
//!
//! The frames are untagged, of [`ETHERTYPE`] with the magic of the test: the
//! bridge of the receiver drops them at the tunnel port (no VLAN), the tap
//! counts them before (in the kernel, also those it has no room for). They
//! are sent as fast as the sender can: what arrives per second is what the
//! way takes. `<core> <share>`: the busiest core of the receiver and its
//! share of network work (softirq) in percent.

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
use wanderlan_core::api::{SpeedRun, Speedtest};
use wanderlan_core::identity::{PairKeys, SiteId};
use wanderlan_core::mtu;

use crate::perf::CpuTimes;
use crate::rawframe::{Sender, Tap};
use crate::sync_net::{
    LINE_TIMEOUT, NONCE_LEN, Offer, TIMEOUT, hex_decode, hex_encode, read_line, read_tagged,
};

const LABEL_REQUEST: &[u8] = b"wanderlan speed request";
const LABEL_REPLY: &[u8] = b"wanderlan speed reply";
/// Ethertype of the test frames (IEEE "local experimental 2"): not the one
/// of the keepalives, so the learn gate of a server does not look at them.
const ETHERTYPE: u16 = 0x88b6;
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

/// What is measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    TcpUp,
    TcpDown,
    TunnelUp,
    TunnelDown,
}

impl What {
    fn name(self) -> &'static str {
        match self {
            Self::TcpUp => "tcp-up",
            Self::TcpDown => "tcp-down",
            Self::TunnelUp => "tunnel-up",
            Self::TunnelDown => "tunnel-down",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [Self::TcpUp, Self::TcpDown, Self::TunnelUp, Self::TunnelDown]
            .into_iter()
            .find(|w| w.name() == text)
    }
}

/// One measurement as the client asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ask {
    what: What,
    seconds: u32,
    size: u16,
    magic: [u8; 4],
}

impl Ask {
    fn line(&self) -> String {
        format!(
            "{} {} {} {}",
            self.what.name(),
            self.seconds,
            self.size,
            hex_encode(&self.magic)
        )
    }

    fn parse<'a>(mut parts: impl Iterator<Item = &'a str>) -> Option<Self> {
        let what = What::parse(parts.next()?)?;
        let seconds: u32 = parts.next()?.parse().ok()?;
        let size: u16 = parts.next()?.parse().ok()?;
        let magic = hex_decode::<4>(parts.next()?)?;
        ((1..=wanderlan_core::api::SPEEDTEST_MAX_SECONDS).contains(&seconds)
            && (FRAME_MIN..=FRAME_MAX).contains(&size))
        .then_some(Self {
            what,
            seconds,
            size,
            magic,
        })
    }

    fn duration(&self) -> Duration {
        Duration::from_secs(u64::from(self.seconds))
    }
}

/// A test frame of `size` bytes with `magic`.
fn frame(size: u16, magic: [u8; 4]) -> Vec<u8> {
    let mut frame = vec![0u8; usize::from(size)];
    frame[..6].copy_from_slice(&FRAME_DST);
    frame[6..12].copy_from_slice(&FRAME_SRC);
    frame[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
    frame[14..18].copy_from_slice(&magic);
    frame
}

/// Frames per call of the sender.
const BATCH: usize = 32;
/// Threads that send at most.
const SENDERS: usize = 4;

/// Sends `frame` through `ifname` for `seconds`, as fast as it can, from up
/// to [`SENDERS`] threads (blocks): how many, and how long it took.
fn blast(ifname: &str, frame: &[u8], seconds: u32) -> std::io::Result<(u64, Duration)> {
    let threads = crate::tuning::online_cpus().clamp(1, SENDERS);
    let senders = (0..threads)
        .map(|_| Sender::open(ifname, FRAME_DST))
        .collect::<std::io::Result<Vec<Sender>>>()?;
    let start = std::time::Instant::now();
    let until = start + Duration::from_secs(u64::from(seconds));
    let results: Vec<std::io::Result<u64>> = std::thread::scope(|scope| {
        let handles: Vec<_> = senders
            .iter()
            .map(|sender| scope.spawn(move || send_until(sender, frame, until)))
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

/// One sender of [`blast`].
fn send_until(sender: &Sender, frame: &[u8], until: std::time::Instant) -> std::io::Result<u64> {
    let mut sent = 0u64;
    while std::time::Instant::now() < until {
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

/// Serves `SPEED <client id> <client nonce> <measurement>`.
pub async fn serve<R, W>(
    args: &str,
    from: SocketAddrV4,
    offer: &Offer,
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
    let Some(_busy) = Busy::take() else {
        write
            .write_all(b"NO another speed test is running\n")
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
    match ask.what {
        What::TcpUp => {
            let before = CpuTimes::now();
            let (bytes, took) = receive(reader, ask.duration() + TIMEOUT).await?;
            let core = CpuTimes::now().busiest_since(&before);
            let text = format!("{bytes} {} {}", took.as_micros(), core_text(core));
            write.write_all(reply(text).as_bytes()).await?;
        }
        What::TcpDown => send_for(write, ask.duration()).await?,
        What::TunnelUp => {
            let tunnel = keyed.tunnel.clone().context("no tunnel to that site")?;
            let magic = ask.magic;
            let tap =
                tokio::task::spawn_blocking(move || Tap::open(&tunnel, ETHERTYPE, magic, None))
                    .await??;
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
            write
                .write_all(reply(format!("{arrived} {}", core_text(core))).as_bytes())
                .await?;
        }
        What::TunnelDown => {
            let tunnel = keyed.tunnel.clone().context("no tunnel to that site")?;
            let (frame, seconds) = (frame(ask.size, ask.magic), ask.seconds);
            let (sent, took) =
                tokio::task::spawn_blocking(move || blast(&tunnel, &frame, seconds)).await??;
            write
                .write_all(reply(format!("{sent} {}", took.as_micros())).as_bytes())
                .await?;
        }
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
    async fn connect(&self, ask: &Ask) -> Result<Conn> {
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

    async fn tcp(&self, up: bool, seconds: u32) -> Result<SpeedRun> {
        let ask = Ask {
            what: if up { What::TcpUp } else { What::TcpDown },
            seconds,
            size: FRAME_MAX,
            magic: [0; 4],
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
        };
        let (sent, arrived, took, core) = if up {
            let mut conn = self.connect(&ask).await?;
            if self.reply(&mut conn, TIMEOUT).await? != "READY" {
                bail!("\"{}\" is not ready", self.name);
            }
            let (tunnel, frame) = (self.tunnel.clone(), frame(size, magic));
            let (sent, took) =
                tokio::task::spawn_blocking(move || blast(&tunnel, &frame, seconds)).await??;
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
            let tap =
                tokio::task::spawn_blocking(move || Tap::open(&tunnel, ETHERTYPE, magic, None))
                    .await??;
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
        };
        assert_eq!(ask.line(), "tunnel-up 5 1464 01020304");
        assert_eq!(Ask::parse(ask.line().split_whitespace()), Some(ask));
        assert_eq!(
            Ask::parse("tunnel-up 60 1464 01020304".split_whitespace()),
            None
        );
        assert_eq!(
            Ask::parse("tcp-up 5 9000 01020304".split_whitespace()),
            None
        );
        let f = frame(1464, [1, 2, 3, 4]);
        assert_eq!(f.len(), 1464);
        assert_eq!(&f[12..18], &[0x88, 0xb6, 1, 2, 3, 4]);
        assert_eq!(rate(1_000_000, Duration::from_secs(1)), 8_000_000);
    }
}
