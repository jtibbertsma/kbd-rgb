//! The keyboard's color zones.

use std::fmt;
use std::ops::{Index, IndexMut};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// One of the keyboard's color zones. `Extra` is the numpad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Zone {
    Left,
    Center,
    Right,
    Extra,
}

impl Zone {
    /// Every zone, from left to right across the keyboard.
    pub(crate) const ALL: [Zone; 4] = [Zone::Left, Zone::Center, Zone::Right, Zone::Extra];

    /// The zone's name in the driver's color files, in `kbdrgb --zone`, and in the state file.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Zone::Left => "left",
            Zone::Center => "center",
            Zone::Right => "right",
            Zone::Extra => "extra",
        }
    }

    /// The zone's place in [`Zone::ALL`].
    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A zone, or the whole keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) enum Target {
    Zone(Zone),
    All,
}

impl Target {
    pub(crate) fn zones(self) -> impl Iterator<Item = Zone> {
        Zone::ALL
            .into_iter()
            .filter(move |&zone| self == Target::All || self == Target::Zone(zone))
    }
}

impl From<Zone> for Target {
    fn from(zone: Zone) -> Self {
        Target::Zone(zone)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Zone(zone) => zone.fmt(f),
            Target::All => f.write_str("all"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("\"{0}\" isn't a zone. Use left, center, right, extra (the numpad), or all.")]
pub(crate) struct NotAZone(String);

impl FromStr for Target {
    type Err = NotAZone;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "all" {
            return Ok(Target::All);
        }
        Zone::ALL
            .into_iter()
            .find(|zone| zone.name() == s)
            .map(Target::Zone)
            .ok_or_else(|| NotAZone(s.to_owned()))
    }
}

impl TryFrom<String> for Target {
    type Error = NotAZone;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Target> for String {
    fn from(target: Target) -> Self {
        target.to_string()
    }
}

/// A value for each zone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PerZone<T>([T; 4]);

impl<T> PerZone<T> {
    pub(crate) fn from_fn(value_of: impl FnMut(Zone) -> T) -> Self {
        PerZone(Zone::ALL.map(value_of))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (Zone, &T)> {
        Zone::ALL.into_iter().zip(&self.0)
    }
}

impl<T: Clone> PerZone<T> {
    pub(crate) fn filled(value: &T) -> Self {
        PerZone::from_fn(|_| value.clone())
    }
}

impl<T> Index<Zone> for PerZone<T> {
    type Output = T;

    fn index(&self, zone: Zone) -> &T {
        &self.0[zone.index()]
    }
}

impl<T> IndexMut<Zone> for PerZone<T> {
    fn index_mut(&mut self, zone: Zone) -> &mut T {
        &mut self.0[zone.index()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zone_target_covers_that_zone() {
        let zones: Vec<Zone> = Target::Zone(Zone::Right).zones().collect();
        assert_eq!(zones, [Zone::Right]);
    }

    #[test]
    fn the_all_target_covers_every_zone_from_left_to_right() {
        let zones: Vec<Zone> = Target::All.zones().collect();
        assert_eq!(zones, Zone::ALL);
    }

    #[test]
    fn a_target_reads_back_what_it_prints() {
        for target in [Target::All, Target::Zone(Zone::Extra)] {
            assert_eq!(target.to_string().parse(), Ok(target));
        }
    }

    #[test]
    fn an_unknown_zone_name_is_rejected_with_the_names_that_work() {
        let error = "top".parse::<Target>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "\"top\" isn't a zone. Use left, center, right, extra (the numpad), or all."
        );
    }

    #[test]
    fn per_zone_holds_a_separate_value_for_each_zone() {
        let mut names = PerZone::from_fn(Zone::name);
        names[Zone::Center] = "middle";
        assert_eq!(names[Zone::Left], "left");
        assert_eq!(names[Zone::Center], "middle");
    }
}
