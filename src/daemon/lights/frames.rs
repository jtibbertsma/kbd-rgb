//! Spacing out the writes that fades need.

use std::time::{Duration, Instant};

use crate::zone::{PerZone, Zone};

/// Decides when to write a fading zone's next color, so that fades don't send the keyboard
/// controller more writes than it can take. A fading zone is written at most once every `frame`.
#[derive(Debug)]
pub(super) struct Frames {
    /// The shortest time between two writes to the same fading zone.
    frame: Duration,
    /// When each zone was last written or picked by [`Frames::next`].
    drawn: PerZone<Option<Instant>>,
    /// When any zone was last written.
    last_write: Option<Instant>,
    /// When the next fade write is due. `None` means that nothing is fading.
    due: Option<Instant>,
}

impl Frames {
    /// The shortest time between two fade writes, to any zones. It allows 30 writes a second. At
    /// 40 a second, the keyboard controller misses some writes. The numpad is written last, so
    /// its fades look choppy.
    pub(super) const MIN_GAP: Duration = Duration::from_nanos(1_000_000_000 / 30);

    pub(super) fn new(frame: Duration) -> Self {
        Frames {
            frame,
            drawn: PerZone::from_fn(|_| None),
            last_write: None,
            due: None,
        }
    }

    /// Records a write to `zone` at `now`.
    pub(super) fn wrote(&mut self, zone: Zone, now: Instant) {
        self.drawn[zone] = Some(now);
        self.last_write = Some(now);
    }

    /// Works out when the next fade write is due, given the zones in `fading`.
    pub(super) fn schedule(&mut self, fading: &[Zone], now: Instant) {
        let ready = fading
            .iter()
            .map(|&zone| self.drawn[zone].map_or(now, |drawn| drawn + self.frame));
        let gap_over = self.last_write.map_or(now, |write| write + Frames::MIN_GAP);
        self.due = ready.min().map(|ready| ready.max(gap_over));
    }

    pub(super) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Picks the zone to write at `now`: of the zones in `fading` that were last drawn at least a
    /// frame ago, the one that has waited the longest.
    pub(super) fn next(&mut self, fading: &[Zone], now: Instant) -> Option<Zone> {
        let zone = fading
            .iter()
            .copied()
            .filter(|&zone| self.drawn[zone].is_none_or(|drawn| now >= drawn + self.frame))
            .min_by_key(|&zone| self.drawn[zone])?;
        self.drawn[zone] = Some(now);
        Some(zone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zone::Zone::{Center, Left, Right};

    const FRAME: Duration = Duration::from_millis(100);

    #[test]
    fn nothing_fading_means_nothing_is_due() {
        let mut frames = Frames::new(FRAME);
        frames.schedule(&[], Instant::now());
        assert_eq!(frames.due(), None);
    }

    #[test]
    fn a_zone_that_was_never_drawn_is_due_at_once() {
        let now = Instant::now();
        let mut frames = Frames::new(FRAME);
        frames.schedule(&[Left], now);
        assert_eq!(frames.due(), Some(now));
        assert_eq!(frames.next(&[Left], now), Some(Left));
    }

    #[test]
    fn a_zone_is_due_again_a_frame_after_it_was_drawn() {
        let now = Instant::now();
        let mut frames = Frames::new(FRAME);
        frames.wrote(Left, now);
        frames.schedule(&[Left], now);
        assert_eq!(frames.due(), Some(now + FRAME));
        assert_eq!(frames.next(&[Left], now + FRAME / 2), None);
        assert_eq!(frames.next(&[Left], now + FRAME), Some(Left));
    }

    #[test]
    fn writes_to_different_zones_are_at_least_the_minimum_gap_apart() {
        let now = Instant::now();
        let mut frames = Frames::new(FRAME);
        frames.wrote(Left, now);
        frames.schedule(&[Left, Center], now);
        assert_eq!(frames.due(), Some(now + Frames::MIN_GAP));
    }

    #[test]
    fn the_zone_that_has_waited_the_longest_goes_first() {
        let start = Instant::now();
        let mut frames = Frames::new(FRAME);
        frames.wrote(Left, start + FRAME / 2);
        frames.wrote(Center, start);
        frames.wrote(Right, start + FRAME / 4);
        let fading = [Left, Center, Right];
        let later = start + FRAME * 2;
        assert_eq!(frames.next(&fading, later), Some(Center));
        assert_eq!(frames.next(&fading, later), Some(Right));
        assert_eq!(frames.next(&fading, later), Some(Left));
        assert_eq!(frames.next(&fading, later), None);
    }
}
