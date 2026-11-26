//! Compact formatting of values for tables.

use std::time::{Duration, UNIX_EPOCH};

/// An age in seconds: `12s`, `5m`, `3h`, `2d`.
pub fn age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A round-trip time in milliseconds.
pub fn rtt(ms: f64) -> String {
    if ms < 10.0 {
        format!("{ms:.1}ms")
    } else {
        format!("{ms:.0}ms")
    }
}

/// A bit rate without unit suffix: `950`, `12.5k`, `1.2M`, `3.4G`. The
/// unit follows the rounded value (999 999 is `1.0M`, never `1000.0k`).
pub fn rate(bps: u64) -> String {
    if bps < 1000 {
        return bps.to_string();
    }
    #[allow(clippy::cast_precision_loss)]
    let v = bps as f64;
    let units = [(1e3, "k"), (1e6, "M"), (1e9, "G"), (1e12, "T")];
    for (i, (div, unit)) in units.iter().enumerate() {
        let x = v / div;
        if x < 999.95 || i == units.len() - 1 {
            return format!("{x:.1}{unit}");
        }
    }
    String::new()
}

/// A bit rate with its unit: `950 bit/s`, `12.5 kbit/s`, `1.2 Mbit/s`.
pub fn rate_unit(bps: u64) -> String {
    let short = rate(bps);
    match short.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => format!("{} {c}bit/s", &short[..i]),
        _ => format!("{short} bit/s"),
    }
}

/// A byte count: `950 B`, `12.5 KB`, `1.2 MB`, `3.4 GB`, `1.1 TB`.
pub fn bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let v = n as f64;
    if n < 1000 {
        format!("{n} B")
    } else if v < 1e6 {
        format!("{:.1} KB", v / 1e3)
    } else if v < 1e9 {
        format!("{:.1} MB", v / 1e6)
    } else if v < 1e12 {
        format!("{:.1} GB", v / 1e9)
    } else {
        format!("{:.1} TB", v / 1e12)
    }
}

/// A duration as written in the configuration (`5m`, `90s`).
pub fn duration(d: Duration) -> String {
    humantime::format_duration(d).to_string()
}

/// First 12 characters of a fingerprint.
pub fn short_fingerprint(fingerprint: &str) -> &str {
    fingerprint.get(..12).unwrap_or(fingerprint)
}

/// Date (`2026-11-24`) and time of day (`10:00:01`) of a Unix time in
/// milliseconds, in UTC.
pub fn clock(time_ms: u64) -> (String, String) {
    let stamp =
        humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_millis(time_ms)).to_string();
    let date = stamp.get(..10).unwrap_or_default().to_string();
    let time = stamp.get(11..19).unwrap_or_default().to_string();
    (date, time)
}

/// Like [`clock`], for a time `offset` seconds away from UTC.
pub fn clock_at(time_ms: u64, offset: i32) -> (String, String) {
    let shifted = i64::try_from(time_ms).unwrap_or(i64::MAX) + i64::from(offset) * 1000;
    clock(u64::try_from(shifted).unwrap_or_default())
}

/// Splits a text into lines of at most `width` characters, breaking at
/// spaces where possible.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        loop {
            let used = line.chars().count();
            let len = word.chars().count();
            let sep = usize::from(used > 0);
            if used + sep + len <= width {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(&word);
                break;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            // A single word longer than the line: hard break.
            let head: String = word.chars().take(width).collect();
            word = word.chars().skip(width).collect();
            lines.push(head);
            if word.is_empty() {
                break;
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// `server` or `client`, without the uplink (where the site name is next to
/// it, e.g. in the VLAN mappings).
pub fn kind(uplink: Option<&str>) -> &'static str {
    if uplink.is_some() { "client" } else { "server" }
}

/// A site name with its kind: `marcel (client)`.
pub fn site_with_kind(site: &wanderlan_core::config::Site) -> String {
    format!("{} ({})", site.name, kind(site.uplink.as_deref()))
}

/// The role of a site: `server`, or `client of <uplink>`.
pub fn role(uplink: Option<&str>, has_clients: bool) -> String {
    wanderlan_core::topology::role(uplink, has_clients)
}

/// True if a site of the status has clients of its own.
pub fn serves(sites: &[wanderlan_core::api::SiteSummary], name: &str) -> bool {
    sites.iter().any(|s| s.uplink.as_deref() == Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_and_rates() {
        assert_eq!(age(5), "5s");
        assert_eq!(age(125), "2m");
        assert_eq!(age(7300), "2h");
        assert_eq!(age(200_000), "2d");
        assert_eq!(rate(950), "950");
        assert_eq!(rate(12_500), "12.5k");
        assert_eq!(rate(1_200_000), "1.2M");
        assert_eq!(rate(3_400_000_000), "3.4G");
        // The unit follows the rounded value.
        assert_eq!(rate(999_999), "1.0M");
        assert_eq!(rate(999_949), "999.9k");
        assert_eq!(rate_unit(12_500), "12.5 kbit/s");
        assert_eq!(rate_unit(950), "950 bit/s");
        assert_eq!(rtt(1.234), "1.2ms");
        assert_eq!(rtt(23.6), "24ms");
    }

    #[test]
    fn clock_is_utc() {
        assert_eq!(
            clock(1_795_000_000_000),
            ("2026-11-18".to_string(), "11:06:40".to_string())
        );
    }

    #[test]
    fn wraps_at_spaces() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
    }
}
