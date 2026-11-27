//! Local time for the log, read from the system's zone file (`TZ`, else
//! `/etc/localtime`, TZif format) without a dependency: the transitions in
//! the file, and its POSIX rule (e.g. `CET-1CEST,M3.5.0,M10.5.0/3`) for
//! the years after them.

use std::path::Path;

/// A time zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// Transition times (Unix seconds) and the type that starts there.
    transitions: Vec<(i64, usize)>,
    /// Offset from UTC in seconds, daylight saving, abbreviation.
    types: Vec<(i32, bool, String)>,
    /// The rule after the last transition.
    rule: Option<Rule>,
}

/// A POSIX TZ rule.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    std: (String, i32),
    /// Daylight saving: name, offset, start and end (local, see [`When`]).
    dst: Option<(String, i32, When, When)>,
}

/// `Mm.w.d/time`: day `d` (0 = Sunday) of week `w` (5 = the last) of month
/// `m`, at `time` seconds after local midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct When {
    month: u32,
    week: u32,
    day: u32,
    time: i64,
}

/// The zone of this host, if one is set and readable; `None` means UTC.
pub fn load() -> Option<Zone> {
    match std::env::var("TZ") {
        Ok(tz) => {
            let tz = tz.trim_start_matches(':');
            if tz.is_empty() || tz == "UTC" {
                return None;
            }
            let path = if tz.starts_with('/') {
                tz.to_string()
            } else {
                format!("/usr/share/zoneinfo/{tz}")
            };
            read(Path::new(&path)).or_else(|| from_rule(tz))
        }
        Err(_) => read(Path::new("/etc/localtime")),
    }
}

/// A zone given by a POSIX rule alone, e.g. `CET-1CEST,M3.5.0,M10.5.0/3`.
pub fn from_rule(text: &str) -> Option<Zone> {
    parse_rule(text).map(|rule| Zone {
        transitions: Vec::new(),
        types: Vec::new(),
        rule: Some(rule),
    })
}

fn read(path: &Path) -> Option<Zone> {
    parse(&std::fs::read(path).ok()?)
}

/// Parses a TZif file (version 1, 2 or 3).
pub fn parse(bytes: &[u8]) -> Option<Zone> {
    let header = |at: usize| -> Option<(u8, [usize; 6])> {
        if bytes.get(at..at + 4)? != b"TZif" {
            return None;
        }
        let version = *bytes.get(at + 4)?;
        let mut counts = [0usize; 6];
        for (i, c) in counts.iter_mut().enumerate() {
            let b = bytes.get(at + 20 + i * 4..at + 24 + i * 4)?;
            *c = u32::from_be_bytes(b.try_into().ok()?) as usize;
        }
        Some((version, counts))
    };
    let (version, counts) = header(0)?;
    let block = |counts: [usize; 6], time: usize| {
        let [isut, isstd, leap, times, types, chars] = counts;
        times * (time + 1) + types * 6 + chars + leap * (time + 4) + isstd + isut
    };
    let (start, counts, time_size) = if version >= b'2' {
        let second = 44 + block(counts, 4);
        let (_, counts) = header(second)?;
        (second + 44, counts, 8)
    } else {
        (44, counts, 4)
    };
    let [_, _, _, times, type_count, chars] = counts;
    let mut at = start;
    // (Never more than the file holds: a broken header may say anything.)
    let mut transitions = Vec::with_capacity(times.min(bytes.len()));
    let mut stamps = Vec::with_capacity(times.min(bytes.len()));
    for _ in 0..times {
        let b = bytes.get(at..at + time_size)?;
        stamps.push(if time_size == 8 {
            i64::from_be_bytes(b.try_into().ok()?)
        } else {
            i64::from(i32::from_be_bytes(b.try_into().ok()?))
        });
        at += time_size;
    }
    for stamp in stamps {
        transitions.push((stamp, usize::from(*bytes.get(at)?)));
        at += 1;
    }
    let mut raw_types = Vec::with_capacity(type_count.min(bytes.len()));
    for _ in 0..type_count {
        let b = bytes.get(at..at + 6)?;
        let offset = i32::from_be_bytes(b[..4].try_into().ok()?);
        raw_types.push((offset, b[4] != 0, usize::from(b[5])));
        at += 6;
    }
    let names = bytes.get(at..at + chars)?;
    let types = raw_types
        .into_iter()
        .map(|(offset, dst, index)| {
            let name = names.get(index..).unwrap_or_default();
            let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
            (
                offset,
                dst,
                String::from_utf8_lossy(&name[..end]).into_owned(),
            )
        })
        .collect();
    // The footer of version 2+: "\n<rule>\n" after the data.
    let rule = if version >= b'2' {
        let data_end = start + block(counts, 8);
        bytes
            .get(data_end..)
            .and_then(|f| std::str::from_utf8(f).ok())
            .map(|f| f.trim_matches('\n'))
            .and_then(parse_rule)
    } else {
        None
    };
    Some(Zone {
        transitions,
        types,
        rule,
    })
}

impl Zone {
    /// Offset from UTC (seconds) and abbreviation at the Unix time `t`.
    pub fn at(&self, t: i64) -> (i32, String) {
        let after_last = self.transitions.last().is_none_or(|(last, _)| t >= *last);
        if after_last && let Some(rule) = &self.rule {
            return rule.at(t);
        }
        let index = match self.transitions.partition_point(|(at, _)| *at <= t) {
            0 => self.types.iter().position(|(_, dst, _)| !dst).unwrap_or(0),
            n => self.transitions[n - 1].1,
        };
        self.types
            .get(index)
            .map_or((0, "UTC".into()), |(o, _, n)| (*o, n.clone()))
    }
}

impl Rule {
    fn at(&self, t: i64) -> (i32, String) {
        let Some((dst_name, dst_offset, start, end)) = &self.dst else {
            return (self.std.1, self.std.0.clone());
        };
        let year = civil(t + i64::from(self.std.1)).0;
        let begins = start.utc(year, self.std.1);
        let ends = end.utc(year, *dst_offset);
        let summer = if begins < ends {
            t >= begins && t < ends
        } else {
            // Southern hemisphere: summer spans the turn of the year.
            t >= begins || t < ends
        };
        if summer {
            (*dst_offset, dst_name.clone())
        } else {
            (self.std.1, self.std.0.clone())
        }
    }
}

impl When {
    /// The moment in `year`, as Unix time, for a local `offset`.
    fn utc(self, year: i64, offset: i32) -> i64 {
        let first = days_from_civil(year, self.month, 1);
        // 1970-01-01 was a Thursday (4).
        let weekday = (first + 4).rem_euclid(7);
        let mut day = first + (i64::from(self.day) - weekday).rem_euclid(7);
        day += 7 * (i64::from(self.week) - 1);
        let next_month = if self.month == 12 {
            days_from_civil(year + 1, 1, 1)
        } else {
            days_from_civil(year, self.month + 1, 1)
        };
        while day >= next_month {
            day -= 7;
        }
        day * 86_400 + self.time - i64::from(offset)
    }
}

/// Parses a POSIX TZ rule such as `CET-1CEST,M3.5.0,M10.5.0/3`.
fn parse_rule(text: &str) -> Option<Rule> {
    let mut rest = text;
    let std_name = name(&mut rest)?;
    let std_offset = -offset(&mut rest)?;
    if rest.is_empty() {
        return Some(Rule {
            std: (std_name, std_offset),
            dst: None,
        });
    }
    let dst_name = name(&mut rest)?;
    let dst_offset = if rest.starts_with(',') {
        std_offset + 3600
    } else {
        -offset(&mut rest)?
    };
    let mut parts = rest.strip_prefix(',')?.split(',');
    let start = when(parts.next()?)?;
    let end = when(parts.next()?)?;
    Some(Rule {
        std: (std_name, std_offset),
        dst: Some((dst_name, dst_offset, start, end)),
    })
}

fn name(rest: &mut &str) -> Option<String> {
    if let Some(quoted) = rest.strip_prefix('<') {
        let end = quoted.find('>')?;
        let name = quoted[..end].to_string();
        *rest = &quoted[end + 1..];
        return Some(name);
    }
    let end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    if end < 3 {
        return None;
    }
    let name = rest[..end].to_string();
    *rest = &rest[end..];
    Some(name)
}

/// `[+-]hh[:mm[:ss]]` in seconds (as written: west of UTC is positive).
fn offset(rest: &mut &str) -> Option<i32> {
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, ':' | '+' | '-')))
        .unwrap_or(rest.len());
    let (text, tail) = rest.split_at(end);
    *rest = tail;
    i32::try_from(duration(text)?).ok()
}

fn duration(text: &str) -> Option<i64> {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(d) => (-1, d),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let mut secs = 0;
    for (i, part) in digits.split(':').enumerate() {
        let v: i64 = part.parse().ok()?;
        secs += v * [3600, 60, 1].get(i)?;
    }
    Some(sign * secs)
}

fn when(text: &str) -> Option<When> {
    let (date, time) = match text.split_once('/') {
        Some((d, t)) => (d, duration(t)?),
        None => (text, 7200),
    };
    let mut parts = date.strip_prefix('M')?.split('.');
    let month = parts.next()?.parse().ok()?;
    let week = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    Some(When {
        month,
        week,
        day,
        time,
    })
}

/// Days since 1970-01-01 of a date (proleptic Gregorian).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) of a Unix time.
fn civil(t: i64) -> (i64, u32, u32) {
    let z = t.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-03-29 01:00 UTC and 2026-10-25 01:00 UTC (Berlin switches).
    const SPRING: i64 = 1_774_746_000;
    const AUTUMN: i64 = 1_792_890_000;

    /// A TZif v2 file for Berlin with transitions in 2026 and the rule.
    fn berlin() -> Vec<u8> {
        let mut out = Vec::new();
        let header = |out: &mut Vec<u8>, counts: [u32; 6]| {
            out.extend(b"TZif2");
            out.extend([0u8; 15]);
            for c in counts {
                out.extend(c.to_be_bytes());
            }
        };
        // Version 1 block: one type, no transitions.
        header(&mut out, [0, 0, 0, 0, 1, 4]);
        out.extend(3600i32.to_be_bytes());
        out.extend([0, 0]);
        out.extend(b"CET\0");
        // Version 2 block.
        header(&mut out, [0, 0, 0, 2, 2, 9]);
        for t in [SPRING, AUTUMN] {
            out.extend(t.to_be_bytes());
        }
        out.extend([1u8, 0]);
        out.extend(3600i32.to_be_bytes());
        out.extend([0, 0]);
        out.extend(7200i32.to_be_bytes());
        out.extend([1, 4]);
        out.extend(b"CET\0CEST\0");
        out.extend(b"\nCET-1CEST,M3.5.0,M10.5.0/3\n");
        out
    }

    #[test]
    fn transitions_and_the_rule_after_them() {
        let zone = parse(&berlin()).expect("parses");
        assert_eq!(zone.at(SPRING - 1), (3600, "CET".into()));
        assert_eq!(zone.at(SPRING), (7200, "CEST".into()));
        assert_eq!(zone.at(AUTUMN - 1), (7200, "CEST".into()));
        assert_eq!(zone.at(AUTUMN), (3600, "CET".into()));
        // 2027 comes from the rule: 2027-03-28 01:00 UTC.
        let spring_2027 = 1_806_195_600;
        assert_eq!(zone.at(spring_2027 - 1).1, "CET");
        assert_eq!(zone.at(spring_2027).1, "CEST");
        // Before the first transition: the standard time.
        assert_eq!(zone.at(0), (3600, "CET".into()));
    }

    #[test]
    fn a_rule_alone() {
        let rule = parse_rule("EST5EDT,M3.2.0,M11.1.0").expect("rule");
        // 2026-07-01 12:00 UTC: summer in New York.
        assert_eq!(rule.at(1_782_907_200), (-4 * 3600, "EDT".into()));
        assert_eq!(rule.at(1_767_268_800), (-5 * 3600, "EST".into()), "January");
        let fixed = parse_rule("<+0530>-5:30").expect("rule");
        assert_eq!(fixed.at(0).0, 5 * 3600 + 30 * 60);
        assert!(parse("not a zone file".as_bytes()).is_none());
    }

    #[test]
    fn civil_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil(SPRING), (2026, 3, 29));
        assert_eq!(days_from_civil(2026, 3, 29), SPRING / 86_400);
    }
}
