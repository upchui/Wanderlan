//! FOU receive ports: GRE inside UDP, over generic netlink (family `fou`,
//! what `ip fou` uses).
//!
//! A FOU port is a UDP socket of the kernel: packets to it are taken out of
//! their UDP header and handed to GRE (IP protocol 47), which finds the
//! tunnel by its endpoints and key. The tunnels between a client and its
//! server use it, so that they pass NAT. A port carries no owner mark; the
//! FOU ports for GRE are taken as wanderlan's.

use std::future::Future;
use std::sync::Arc;

use futures_util::TryStreamExt as _;
use genetlink::GenetlinkHandle;
use netlink_packet_core::{
    DecodeError, DefaultNla, Emitable, ErrorContext as _, NLM_F_ACK, NLM_F_DUMP, NLM_F_REQUEST,
    NetlinkHeader, NetlinkMessage, NetlinkPayload, Nla, NlaBuffer, NlasIterator, Parseable,
    ParseableParametrized,
};
use netlink_packet_generic::{GenlFamily, GenlHeader, GenlMessage};

use super::{SysError, SysResult};

/// IP protocol of GRE.
pub const IPPROTO_GRE: u8 = 47;

const FOU_GENL_VERSION: u8 = 1;
const FOU_CMD_ADD: u8 = 1;
const FOU_CMD_DEL: u8 = 2;
const FOU_CMD_GET: u8 = 3;
const FOU_ATTR_PORT: u16 = 1;
const FOU_ATTR_AF: u16 = 2;
const FOU_ATTR_IPPROTO: u16 = 3;
const FOU_ATTR_TYPE: u16 = 4;
const FOU_ENCAP_DIRECT: u8 = 1;
const AF_INET: u8 = 2;

/// One FOU receive port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FouPort {
    /// The UDP port.
    pub port: u16,
    /// The protocol inside (47: GRE).
    pub ipproto: u8,
}

/// FOU receive ports of the kernel.
pub trait FouOps: Send + Sync {
    /// The IPv4 FOU ports (none if the module `fou` is not loaded).
    fn fou_ports(&self) -> impl Future<Output = SysResult<Vec<FouPort>>> + Send;

    /// Opens a FOU port for GRE.
    fn add_fou_port(&self, port: u16) -> impl Future<Output = SysResult<()>> + Send;

    /// Closes a FOU port for GRE.
    fn del_fou_port(&self, port: u16) -> impl Future<Output = SysResult<()>> + Send;
}

/// An attribute of a FOU message.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FouAttr {
    Port(u16),
    Af(u8),
    IpProto(u8),
    Type(u8),
    Other(DefaultNla),
}

impl Nla for FouAttr {
    fn value_len(&self) -> usize {
        match self {
            Self::Port(_) => 2,
            Self::Af(_) | Self::IpProto(_) | Self::Type(_) => 1,
            Self::Other(nla) => nla.value_len(),
        }
    }

    fn kind(&self) -> u16 {
        match self {
            Self::Port(_) => FOU_ATTR_PORT,
            Self::Af(_) => FOU_ATTR_AF,
            Self::IpProto(_) => FOU_ATTR_IPPROTO,
            Self::Type(_) => FOU_ATTR_TYPE,
            Self::Other(nla) => nla.kind(),
        }
    }

    fn emit_value(&self, buffer: &mut [u8]) {
        match self {
            // The port in network byte order.
            Self::Port(port) => buffer[..2].copy_from_slice(&port.to_be_bytes()),
            Self::Af(v) | Self::IpProto(v) | Self::Type(v) => buffer[0] = *v,
            Self::Other(nla) => nla.emit_value(buffer),
        }
    }
}

impl<'a, T: AsRef<[u8]> + ?Sized> Parseable<NlaBuffer<&'a T>> for FouAttr {
    fn parse(buf: &NlaBuffer<&'a T>) -> Result<Self, DecodeError> {
        let value = buf.value();
        let byte = || {
            value
                .first()
                .copied()
                .ok_or_else(|| DecodeError::from("empty attribute"))
        };
        Ok(match buf.kind() {
            FOU_ATTR_PORT => {
                let bytes: [u8; 2] = value
                    .get(..2)
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| DecodeError::from("short FOU port"))?;
                Self::Port(u16::from_be_bytes(bytes))
            }
            FOU_ATTR_AF => Self::Af(byte()?),
            FOU_ATTR_IPPROTO => Self::IpProto(byte()?),
            FOU_ATTR_TYPE => Self::Type(byte()?),
            _ => Self::Other(DefaultNla::parse(buf).context("FOU attribute")?),
        })
    }
}

/// A message of the `fou` family.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FouMessage {
    cmd: u8,
    attrs: Vec<FouAttr>,
}

impl GenlFamily for FouMessage {
    fn family_name() -> &'static str {
        "fou"
    }

    fn command(&self) -> u8 {
        self.cmd
    }

    fn version(&self) -> u8 {
        FOU_GENL_VERSION
    }
}

impl Emitable for FouMessage {
    fn buffer_len(&self) -> usize {
        self.attrs.as_slice().buffer_len()
    }

    fn emit(&self, buffer: &mut [u8]) {
        self.attrs.as_slice().emit(buffer);
    }
}

impl ParseableParametrized<[u8], GenlHeader> for FouMessage {
    fn parse_with_param(buf: &[u8], header: GenlHeader) -> Result<Self, DecodeError> {
        let attrs = NlasIterator::new(buf)
            .map(|nla| nla.and_then(|nla| FouAttr::parse(&nla)))
            .collect::<Result<Vec<_>, _>>()
            .context("FOU message")?;
        Ok(Self {
            cmd: header.cmd,
            attrs,
        })
    }
}

/// The message to open or close `port` for GRE.
fn port_message(cmd: u8, port: u16) -> FouMessage {
    FouMessage {
        cmd,
        attrs: vec![
            FouAttr::Port(port),
            FouAttr::Af(AF_INET),
            FouAttr::IpProto(IPPROTO_GRE),
            FouAttr::Type(FOU_ENCAP_DIRECT),
        ],
    }
}

/// FOU over a generic netlink connection, opened when first needed.
#[derive(Debug, Clone, Default)]
pub struct Fou {
    handle: Arc<tokio::sync::Mutex<Option<GenetlinkHandle>>>,
}

impl Fou {
    async fn handle(&self) -> SysResult<GenetlinkHandle> {
        let mut slot = self.handle.lock().await;
        if let Some(handle) = slot.as_ref() {
            return Ok(handle.clone());
        }
        let (connection, handle, _) = genetlink::new_connection()
            .map_err(|e| SysError::new("open a generic netlink socket", e))?;
        tokio::spawn(connection);
        *slot = Some(handle.clone());
        Ok(handle)
    }

    /// Sends `message` with `flags` and collects the answers.
    async fn request(&self, message: FouMessage, flags: u16) -> SysResult<Vec<FouMessage>> {
        let what = match message.cmd {
            FOU_CMD_ADD => "open a FOU port",
            FOU_CMD_DEL => "close a FOU port",
            _ => "list the FOU ports",
        };
        let mut handle = self.handle().await?;
        let mut request = NetlinkMessage::new(
            NetlinkHeader::default(),
            NetlinkPayload::InnerMessage(GenlMessage::from_payload(message)),
        );
        request.header.flags = flags;
        request.finalize();
        let mut answers = handle.request(request).await.map_err(|e| {
            // The family exists only while the module is loaded.
            SysError::new(
                what,
                format!("{e} (is the kernel module fou loaded? modprobe fou)"),
            )
        })?;
        let mut out = Vec::new();
        while let Some(answer) = answers
            .try_next()
            .await
            .map_err(|e| SysError::new(what, e))?
        {
            match answer.payload {
                NetlinkPayload::InnerMessage(genl) => out.push(genl.payload),
                NetlinkPayload::Error(e) if e.code.is_some() => {
                    return Err(SysError::new(what, std::io::Error::from(e)));
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

impl FouOps for Fou {
    async fn fou_ports(&self) -> SysResult<Vec<FouPort>> {
        let message = FouMessage {
            cmd: FOU_CMD_GET,
            attrs: Vec::new(),
        };
        let answers = match self.request(message, NLM_F_REQUEST | NLM_F_DUMP).await {
            Ok(a) => a,
            // Without the module there is no family and no port.
            Err(e) if e.to_string().contains("modprobe fou") => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut ports: Vec<FouPort> = answers
            .iter()
            .filter(|m| m.attrs.contains(&FouAttr::Af(AF_INET)))
            .filter_map(|m| {
                let port = m.attrs.iter().find_map(|a| match a {
                    FouAttr::Port(p) => Some(*p),
                    _ => None,
                })?;
                let ipproto = m.attrs.iter().find_map(|a| match a {
                    FouAttr::IpProto(p) => Some(*p),
                    _ => None,
                })?;
                Some(FouPort { port, ipproto })
            })
            .collect();
        ports.sort_unstable();
        Ok(ports)
    }

    async fn add_fou_port(&self, port: u16) -> SysResult<()> {
        self.request(port_message(FOU_CMD_ADD, port), NLM_F_REQUEST | NLM_F_ACK)
            .await
            .map(|_| ())
    }

    async fn del_fou_port(&self, port: u16) -> SysResult<()> {
        self.request(port_message(FOU_CMD_DEL, port), NLM_F_REQUEST | NLM_F_ACK)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_message_emits_and_parses_back() {
        let message = port_message(FOU_CMD_ADD, 51822);
        let mut buf = vec![0u8; message.buffer_len()];
        message.emit(&mut buf);
        // The port in network byte order right after its attribute header.
        assert_eq!(&buf[4..6], &51822u16.to_be_bytes());
        let header = GenlHeader {
            cmd: FOU_CMD_ADD,
            version: FOU_GENL_VERSION,
        };
        let parsed = FouMessage::parse_with_param(&buf, header).unwrap();
        assert_eq!(parsed, message);
    }
}
