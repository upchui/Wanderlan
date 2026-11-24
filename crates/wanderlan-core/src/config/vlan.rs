//! Sets of VLAN IDs, e.g. `"10-50"`, `"native,10,20,30-40"` or `"all"`.
//!
//! VLAN 1 is the native VLAN, like on most switches: untagged traffic on the
//! trunks. It is written `native`; between the sites it travels as VLAN 1.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

/// Lowest usable VLAN ID.
pub const VID_MIN: u16 = 1;
/// Highest usable VLAN ID.
pub const VID_MAX: u16 = 4094;
/// The native VLAN: untagged traffic on the trunks.
pub const NATIVE_VID: u16 = 1;

/// A set of VLAN IDs in the range 1..=4094.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct VlanSet(BTreeSet<u16>);

/// Error returned when parsing a VLAN set fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VlanSetError {
    #[error("empty VLAN list")]
    Empty,
    #[error("\"{0}\" is not a VLAN ID or range")]
    Syntax(String),
    #[error("VLAN ID {0} is outside 1-4094")]
    OutOfRange(i64),
    #[error("range {0}-{1} is reversed")]
    Reversed(u16, u16),
}

impl VlanSet {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a set from an inclusive range.
    pub fn from_range(first: u16, last: u16) -> Self {
        Self((first..=last).collect())
    }

    /// Every VLAN and the native VLAN (untagged traffic).
    pub fn all() -> Self {
        Self::from_range(VID_MIN, VID_MAX)
    }

    /// True if the set holds every VLAN and the native VLAN.
    pub fn is_all(&self) -> bool {
        self.0.len() == usize::from(VID_MAX - VID_MIN + 1)
    }

    /// True if the native VLAN (untagged traffic) is part of the set.
    pub fn has_native(&self) -> bool {
        self.contains(NATIVE_VID)
    }

    /// Adds a single VLAN ID after checking its range.
    pub fn insert_checked(&mut self, vid: i64) -> Result<(), VlanSetError> {
        let vid = check_vid(vid)?;
        self.0.insert(vid);
        Ok(())
    }

    /// Adds one item of the textual syntax: `"20"`, `"10-50"`, `"native"`
    /// or `"all"`.
    pub fn insert_item(&mut self, item: &str) -> Result<(), VlanSetError> {
        let item = item.trim();
        if item.is_empty() {
            return Err(VlanSetError::Syntax(item.to_string()));
        }
        if item.eq_ignore_ascii_case("native") || item.eq_ignore_ascii_case("untagged") {
            self.0.insert(NATIVE_VID);
            return Ok(());
        }
        if item.eq_ignore_ascii_case("all") {
            self.0.extend(VID_MIN..=VID_MAX);
            return Ok(());
        }
        match item.split_once('-') {
            Some((a, b)) => {
                let first = parse_vid(a, item)?;
                let last = parse_vid(b, item)?;
                if first > last {
                    return Err(VlanSetError::Reversed(first, last));
                }
                self.0.extend(first..=last);
            }
            None => {
                let vid = parse_vid(item, item)?;
                self.0.insert(vid);
            }
        }
        Ok(())
    }

    /// Returns true if the VLAN ID is part of the set.
    pub fn contains(&self, vid: u16) -> bool {
        self.0.contains(&vid)
    }

    /// Number of VLAN IDs in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns true if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates over the VLAN IDs in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.0.iter().copied()
    }

    /// Removes a VLAN ID.
    pub fn remove(&mut self, vid: u16) -> bool {
        self.0.remove(&vid)
    }

    /// VLAN IDs contained in both sets.
    pub fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).copied().collect())
    }

    /// Returns the set as a list of inclusive ranges in ascending order.
    pub fn ranges(&self) -> Vec<(u16, u16)> {
        let mut out: Vec<(u16, u16)> = Vec::new();
        for vid in self.iter() {
            match out.last_mut() {
                Some((_, last)) if last.checked_add(1) == Some(vid) => *last = vid,
                _ => out.push((vid, vid)),
            }
        }
        out
    }
}

fn check_vid(vid: i64) -> Result<u16, VlanSetError> {
    if (i64::from(VID_MIN)..=i64::from(VID_MAX)).contains(&vid) {
        u16::try_from(vid).map_err(|_| VlanSetError::OutOfRange(vid))
    } else {
        Err(VlanSetError::OutOfRange(vid))
    }
}

fn parse_vid(text: &str, item: &str) -> Result<u16, VlanSetError> {
    let n: i64 = text
        .trim()
        .parse()
        .map_err(|_| VlanSetError::Syntax(item.to_string()))?;
    check_vid(n)
}

impl FromStr for VlanSet {
    type Err = VlanSetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut set = Self::new();
        if s.trim().is_empty() {
            return Err(VlanSetError::Empty);
        }
        for item in s.split(',') {
            set.insert_item(item)?;
        }
        Ok(set)
    }
}

impl FromIterator<u16> for VlanSet {
    fn from_iter<I: IntoIterator<Item = u16>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl fmt::Display for VlanSet {
    /// `all`, or `native` first and then the tagged VLANs as ranges.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_all() {
            return f.write_str("all");
        }
        let mut first = true;
        if self.has_native() {
            f.write_str("native")?;
            first = false;
        }
        let tagged: Self = self.iter().filter(|v| *v != NATIVE_VID).collect();
        for (a, b) in tagged.ranges() {
            if !first {
                f.write_str(",")?;
            }
            first = false;
            if a == b {
                write!(f, "{a}")?;
            } else {
                write!(f, "{a}-{b}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for VlanSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VlanSet({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_range_and_list() {
        let set: VlanSet = "10-12, 20,30-31".parse().unwrap();
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![10, 11, 12, 20, 30, 31]);
        assert_eq!(set.to_string(), "10-12,20,30-31");
    }

    #[test]
    fn native_and_all() {
        let set: VlanSet = "native,10-50".parse().unwrap();
        assert!(set.has_native());
        assert_eq!(set.len(), 42);
        assert_eq!(set.to_string(), "native,10-50");
        assert_eq!(
            "1-50".parse::<VlanSet>().unwrap().to_string(),
            "native,2-50"
        );
        assert_eq!("Untagged".parse::<VlanSet>().unwrap().to_string(), "native");
        let all: VlanSet = "all".parse().unwrap();
        assert!(all.is_all() && all.has_native());
        assert_eq!(all, VlanSet::all());
        assert_eq!(all.to_string(), "all");
        assert_eq!("1-4094".parse::<VlanSet>().unwrap().to_string(), "all");
        assert_eq!(all.ranges(), vec![(1, 4094)]);
        assert!(!"2-4094".parse::<VlanSet>().unwrap().is_all());
    }

    #[test]
    fn merges_adjacent_ranges_in_display() {
        let set: VlanSet = "10-20,21,22-30".parse().unwrap();
        assert_eq!(set.to_string(), "10-30");
        assert_eq!(set.ranges(), vec![(10, 30)]);
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!("".parse::<VlanSet>(), Err(VlanSetError::Empty));
        assert_eq!("0".parse::<VlanSet>(), Err(VlanSetError::OutOfRange(0)));
        assert_eq!(
            "4095".parse::<VlanSet>(),
            Err(VlanSetError::OutOfRange(4095))
        );
        assert_eq!(
            "50-10".parse::<VlanSet>(),
            Err(VlanSetError::Reversed(50, 10))
        );
        assert_eq!(
            "ten".parse::<VlanSet>(),
            Err(VlanSetError::Syntax("ten".to_string()))
        );
        assert_eq!(
            "10,,20".parse::<VlanSet>(),
            Err(VlanSetError::Syntax(String::new()))
        );
    }
}
