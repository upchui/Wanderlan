//! What limits the throughput of this box (`wanderlan perf`), and the load
//! of its cores during a speed test.
//!
//! Watched for a few seconds: the share of each core spent on network work
//! (softirq, from `/proc/stat`), the tunnel packets split outside (IP
//! fragments, from `/proc/net/snmp`) and the tunnel traffic. Looked at once:
//! the receive queues of the NICs the traffic comes in at, the path MTU to
//! the peers, connection tracking and bridge netfilter.

use std::collections::BTreeMap;
use std::time::Duration;

use wanderlan_core::api::{Finding, FindingLevel, Perf};
use wanderlan_core::mtu;

use crate::tuning;

/// A core spending at least this share on network work limits the box.
const BUSY_CORE: u32 = 70;
/// Tunnel packets per second below which the load says little.
const LITTLE_TRAFFIC: u64 = 2000;

/// Time and network work (softirq) per core, in clock ticks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CpuTimes(BTreeMap<u32, (u64, u64)>);

impl CpuTimes {
    /// Now, from `/proc/stat`.
    pub fn now() -> Self {
        std::fs::read_to_string("/proc/stat")
            .map(|s| Self::parse(&s))
            .unwrap_or_default()
    }

    /// From the text of `/proc/stat`: the lines `cpuN user nice system idle
    /// iowait irq softirq steal …`.
    pub fn parse(text: &str) -> Self {
        let mut out = BTreeMap::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(cpu) = parts
                .next()
                .and_then(|c| c.strip_prefix("cpu"))
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let values: Vec<u64> = parts.take(8).filter_map(|v| v.parse().ok()).collect();
            if values.len() < 7 {
                continue;
            }
            out.insert(cpu, (values.iter().sum(), values[6]));
        }
        Self(out)
    }

    /// The busiest core since `before` (its number and its share of network
    /// work in percent) and the share of all cores.
    pub fn busiest_since(&self, before: &Self) -> Option<(u32, u32, u32)> {
        let mut busiest: Option<(u32, u32)> = None;
        let (mut soft, mut total) = (0u64, 0u64);
        for (cpu, (t1, s1)) in &self.0 {
            let Some((t0, s0)) = before.0.get(cpu) else {
                continue;
            };
            let (dt, ds) = (t1.saturating_sub(*t0), s1.saturating_sub(*s0));
            if dt == 0 {
                continue;
            }
            soft += ds;
            total += dt;
            let share = u32::try_from(ds * 100 / dt).unwrap_or(100);
            if busiest.is_none_or(|(_, b)| share > b) {
                busiest = Some((*cpu, share));
            }
        }
        let (cpu, share) = busiest?;
        Some((
            cpu,
            share,
            u32::try_from(soft * 100 / total.max(1)).unwrap_or(100),
        ))
    }
}

/// IP fragments made (`FragCreates`) and to be put together (`ReasmReqds`).
pub fn fragments() -> (u64, u64) {
    std::fs::read_to_string("/proc/net/snmp")
        .map(|s| parse_fragments(&s))
        .unwrap_or_default()
}

fn parse_fragments(text: &str) -> (u64, u64) {
    let ip: Vec<&str> = text.lines().filter(|l| l.starts_with("Ip:")).collect();
    let [names, values] = ip.as_slice() else {
        return (0, 0);
    };
    let pairs: BTreeMap<&str, u64> = names
        .split_whitespace()
        .zip(values.split_whitespace())
        .filter_map(|(n, v)| Some((n, v.parse().ok()?)))
        .collect();
    (
        pairs.get("FragCreates").copied().unwrap_or(0),
        pairs.get("ReasmReqds").copied().unwrap_or(0),
    )
}

/// Packets received and sent by interface `name`.
pub fn packets(name: &str) -> (u64, u64) {
    let read = |what: &str| {
        std::fs::read_to_string(format!("/sys/class/net/{name}/statistics/{what}"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    (read("rx_packets"), read("tx_packets"))
}

/// What `perf` looks at besides the counters.
#[derive(Debug, Clone, Default)]
pub struct Input {
    /// The trunk ports.
    pub trunks: Vec<String>,
    /// The interfaces the tunnel packets leave and come in by.
    pub underlay: Vec<String>,
    /// The tunnel interfaces.
    pub tunnels: Vec<String>,
    /// Peer → measured path MTU.
    pub pmtu: BTreeMap<String, u16>,
    /// Connection tracking is loaded.
    pub conntrack: bool,
    /// The tunnel packets skip it.
    pub notrack: bool,
    /// The warning about bridge netfilter, if it is on.
    pub bridge_netfilter: Option<String>,
}

/// Watches for `seconds` and says what it found.
pub async fn run(input: Input, seconds: u32) -> Perf {
    let count = |tunnels: &[String]| {
        tunnels
            .iter()
            .map(|t| {
                let (rx, tx) = packets(t);
                rx + tx
            })
            .sum::<u64>()
    };
    let (cpu0, frag0, packets0) = (CpuTimes::now(), fragments(), count(&input.tunnels));
    tokio::time::sleep(Duration::from_secs(u64::from(seconds))).await;
    let (cpu1, frag1, packets1) = (CpuTimes::now(), fragments(), count(&input.tunnels));
    let mut findings = Vec::new();
    let cpus = u32::try_from(tuning::online_cpus()).unwrap_or(1);
    let per_second = packets1.saturating_sub(packets0) / u64::from(seconds.max(1));

    // The cores.
    match cpu1.busiest_since(&cpu0) {
        Some((cpu, share, all)) if per_second < LITTLE_TRAFFIC => findings.push(Finding {
            level: FindingLevel::Hint,
            text: format!(
                "Little tunnel traffic while watching ({per_second} packets/s; busiest core {cpu} at {share} % network work): the load says little"
            ),
            fix: Some(format!(
                "watch during a transfer, e.g. `wanderlan speedtest <site>` in another shell (all cores: {all} %)"
            )),
        }),
        Some((cpu, share, all)) if share >= BUSY_CORE && cpus > 1 => findings.push(Finding {
            level: FindingLevel::Problem,
            text: format!(
                "Core {cpu} spends {share} % on network work, all {cpus} cores on average {all} %: one core limits the throughput"
            ),
            fix: Some(format!(
                "more receive queues: in a Proxmox VM \"Multiqueue\" = {cpus} on the network devices (Hardware → Network Device → Advanced); see the queues below"
            )),
        }),
        Some((cpu, share, all)) => findings.push(Finding {
            level: FindingLevel::Ok,
            text: format!(
                "Network work: busiest core {cpu} at {share} %, all {cpus} cores {all} % ({per_second} tunnel packets/s)"
            ),
            fix: None,
        }),
        None => {}
    }

    // The receive queues.
    let spread = tuning::spread();
    let mut nics: Vec<&String> = input.trunks.iter().chain(&input.underlay).collect();
    nics.sort();
    nics.dedup();
    for nic in nics {
        let Ok(queues) = tuning::rx_queues(nic) else {
            continue;
        };
        let n = queues.len();
        if n > 1 || cpus < 2 {
            findings.push(Finding {
                level: FindingLevel::Ok,
                text: format!("{nic}: {n} receive queues"),
                fix: None,
            });
        } else if spread.contains(nic) {
            findings.push(Finding {
                level: FindingLevel::Hint,
                text: format!(
                    "{nic}: one receive queue; wanderlan spreads its work over the {cpus} cores (RPS)"
                ),
                fix: Some(format!(
                    "cheaper with more queues: in a Proxmox VM \"Multiqueue\" = {cpus} on this network device"
                )),
            });
        } else {
            findings.push(Finding {
                level: FindingLevel::Problem,
                text: format!("{nic}: one receive queue for {cpus} cores"),
                fix: Some(format!(
                    "in a Proxmox VM \"Multiqueue\" = {cpus} on this network device, or RPS: `echo {} > /sys/class/net/{nic}/queues/rx-0/rps_cpus`",
                    tuning::usable_mask()
                )),
            });
        }
    }

    // Split outside.
    let made = frag1.0.saturating_sub(frag0.0);
    let joined = frag1.1.saturating_sub(frag0.1);
    for (peer, pmtu) in &input.pmtu {
        if *pmtu >= mtu::PROBE_MAX {
            continue;
        }
        let fits = pmtu.saturating_sub(mtu::OUTER_IP + mtu::UDP + mtu::VXLAN + mtu::ETHERNET_VLAN);
        findings.push(Finding {
            level: FindingLevel::Hint,
            text: format!(
                "Path MTU to \"{peer}\" is {pmtu}: packets over {fits} bytes are split in two outside (TCP is clamped below that, UDP such as streaming or games is split)"
            ),
            fix: Some(format!(
                "a transport MTU of at least {} where it can be had (on a LAN: jumbo frames on the switch, the Proxmox bridge and the VM network device)",
                mtu::PROBE_MAX
            )),
        });
    }
    if made + joined > 0 {
        findings.push(Finding {
            level: if (made + joined) * 10 > packets1.saturating_sub(packets0) {
                FindingLevel::Problem
            } else {
                FindingLevel::Hint
            },
            text: format!(
                "{made} IP fragments made and {joined} put together while watching (tunnel packets: {})",
                packets1.saturating_sub(packets0)
            ),
            fix: Some("a larger transport MTU (see above)".into()),
        });
    }

    // Connection tracking and bridge netfilter.
    if input.conntrack {
        findings.push(if input.notrack {
            Finding {
                level: FindingLevel::Ok,
                text: "Connection tracking is loaded; the tunnel packets skip it".into(),
                fix: None,
            }
        } else {
            Finding {
                level: FindingLevel::Hint,
                text: "Connection tracking looks at every tunnel packet (a client keeps it for its uplink: its firewall may need it)".into(),
                fix: None,
            }
        });
    }
    if let Some(warning) = input.bridge_netfilter {
        findings.push(Finding {
            level: FindingLevel::Problem,
            text: warning,
            fix: None,
        });
    }
    findings.sort_by_key(|f| f.level);
    Perf { seconds, findings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_busiest_core_and_the_fragments_are_read() {
        let before = CpuTimes::parse(
            "cpu  10 0 10 100 0 0 10 0 0 0\ncpu0 5 0 5 50 0 0 0 0 0 0\ncpu1 5 0 5 50 0 0 10 0 0 0\nintr 1",
        );
        let after = CpuTimes::parse(
            "cpu  10 0 10 100 0 0 10 0 0 0\ncpu0 5 0 5 140 0 0 10 0 0 0\ncpu1 5 0 5 60 0 0 100 0 0 0\n",
        );
        // cpu0: 10 of 100 ticks, cpu1: 90 of 100.
        assert_eq!(after.busiest_since(&before), Some((1, 90, 50)));
        let snmp = "Ip: Forwarding DefaultTTL InReceives ReasmReqds ReasmOKs FragCreates\nIp: 1 64 100 7 3 12\nIcmp: InMsgs\nIcmp: 0\n";
        assert_eq!(parse_fragments(snmp), (12, 7));
    }
}
