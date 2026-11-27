//! Working out a zone's color at a point in time, for effects, transitions, and caps lock.

use std::time::{Duration, Instant};

use crate::color::Color;
use crate::daemon::config::{CapsLock, Effects};
use crate::ease::Ease;
use crate::setting::{Effect, Pattern};
use crate::zone::Zone;

/// The color after `steps` fades through `count` colors that loop back to the first. `color(i)` is
/// color `i`, and `curve(i)` is the curve for fading into it. For example, 1.5 steps is halfway
/// between the second color and the third.
fn blend(
    count: usize,
    color: impl Fn(usize) -> Color,
    curve: impl Fn(usize) -> Ease,
    steps: f64,
) -> Color {
    let whole = steps.floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "steps is never negative, and a cast of a number that is too large gives the largest usize"
    )]
    let here = whole as usize % count;
    let next = (here + 1) % count;
    color(here).mix(color(next), curve(next).at(steps - whole))
}

/// How many times `period` fits in `time`, with the fraction.
fn steps(time: Duration, period: Duration) -> f64 {
    time.as_secs_f64() / period.as_secs_f64()
}

/// The color that `effect` gives `zone` once it has run for `elapsed`. Every effect counts from
/// the same moment, so effects with the same colors and period stay in step, however far apart
/// they were started.
pub(super) fn effect_color(
    effect: &Effect,
    zone: Zone,
    elapsed: Duration,
    settings: &Effects,
) -> Color {
    let steps = steps(elapsed, effect.period.get());
    match &effect.pattern {
        Pattern::Cycle(cycle) => {
            let cycle = cycle.steps();
            blend(cycle.len(), |i| cycle[i].color, |i| cycle[i].curve, steps)
        }
        Pattern::Rainbow { ease } => {
            let colors = &settings.rainbow.colors;
            // A zone's place in `Zone::ALL` is at most 3, so the cast is exact.
            #[expect(clippy::cast_precision_loss, reason = "the index is at most 3")]
            let offset = zone.index() as f64 * settings.rainbow.lag;
            blend(colors.len(), |i| colors[i], |_| *ease, steps + offset)
        }
        Pattern::Pulse { color, ease } => {
            let colors = [*color, Color::BLACK.mix(*color, settings.pulse.dim)];
            blend(2, |i| colors[i], |_| *ease, steps)
        }
    }
}

/// The left zone's color once caps lock has been on for `elapsed`.
pub(super) fn caps_color(settings: &CapsLock, elapsed: Duration) -> Color {
    let colors = &settings.colors;
    blend(
        colors.len(),
        |i| colors[i],
        |_| settings.ease,
        steps(elapsed, settings.delay),
    )
}

/// A one-time fade from the color a zone had to its new color.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Transition {
    pub(super) from: Color,
    pub(super) to: Color,
    pub(super) start: Instant,
    pub(super) length: Duration,
    pub(super) curve: Ease,
}

impl Transition {
    pub(super) fn end(&self) -> Instant {
        self.start + self.length
    }

    pub(super) fn color_at(&self, now: Instant) -> Color {
        if now >= self.end() {
            return self.to;
        }
        let progress = steps(now.saturating_duration_since(self.start), self.length);
        self.from.mix(self.to, self.curve.at(progress))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::setting::{Cycle, Period, Step};

    fn color(text: &str) -> Color {
        text.parse().unwrap()
    }

    fn cycle(steps: &[(&str, Ease)], period: f64) -> Effect {
        let steps = steps
            .iter()
            .map(|&(text, curve)| Step {
                color: color(text),
                curve,
            })
            .collect();
        Effect {
            pattern: Pattern::Cycle(Cycle::new(steps).unwrap()),
            period: Period::new(Duration::from_secs_f64(period)).unwrap(),
        }
    }

    fn at(effect: &Effect, zone: Zone, seconds: f64) -> Color {
        effect_color(
            effect,
            zone,
            Duration::from_secs_f64(seconds),
            &Effects::default(),
        )
    }

    #[rstest]
    #[case(0.0, "FF0000")]
    #[case(1.0, "FF8080")]
    #[case(2.0, "FFFFFF")]
    #[case(4.0, "0000FF")]
    #[case(5.0, "800080")]
    #[case(6.0, "FF0000")]
    fn a_cycle_takes_its_period_from_each_color_to_the_next_and_loops(
        #[case] seconds: f64,
        #[case] expected: &str,
    ) {
        let linear = Ease::LINEAR;
        let effect = cycle(
            &[("FF0000", linear), ("FFFFFF", linear), ("0000FF", linear)],
            2.0,
        );
        assert_eq!(at(&effect, Zone::Left, seconds), color(expected));
    }

    #[test]
    fn each_step_uses_the_curve_into_the_color_it_goes_to() {
        let snap = Ease::new(0.9, 0.0, 0.1, 1.0).unwrap();
        let effect = cycle(
            &[
                ("FF0000", Ease::LINEAR),
                ("FFFF00", Ease::EASE_IN),
                ("0000FF", snap),
            ],
            1.0,
        );
        let (red, yellow, blue) = (color("FF0000"), color("FFFF00"), color("0000FF"));
        assert_eq!(
            at(&effect, Zone::Left, 0.5),
            red.mix(yellow, Ease::EASE_IN.at(0.5))
        );
        assert_eq!(at(&effect, Zone::Left, 1.5), yellow.mix(blue, snap.at(0.5)));
        assert_eq!(at(&effect, Zone::Left, 2.5), blue.mix(red, 0.5));
    }

    #[test]
    fn a_cycle_is_the_same_in_every_zone() {
        let effect = cycle(&[("FF0000", Ease::LINEAR), ("0000FF", Ease::LINEAR)], 1.0);
        assert_eq!(at(&effect, Zone::Left, 0.3), at(&effect, Zone::Extra, 0.3));
    }

    fn rainbow() -> Effect {
        let period = Effects::default().rainbow.delay;
        Effect {
            pattern: Pattern::Rainbow { ease: Ease::LINEAR },
            period,
        }
    }

    #[test]
    fn a_rainbow_puts_each_zone_further_along_its_colors_than_the_last() {
        let colors: Vec<Color> = Zone::ALL
            .iter()
            .map(|&zone| at(&rainbow(), zone, 0.0))
            .collect();
        // The default lag is two thirds of a color, and the first two colors are red and yellow.
        assert_eq!(colors[0], color("FF0000"));
        assert_eq!(colors[1], color("FF0000").mix(color("FFFF00"), 2.0 / 3.0));
        let mut distinct = colors.clone();
        distinct.dedup();
        assert_eq!(distinct.len(), 4);
    }

    #[test]
    fn a_rainbow_loops_around_its_colors() {
        let once_around = 6.0 * 0.6;
        for zone in Zone::ALL {
            assert_eq!(
                at(&rainbow(), zone, 0.25),
                at(&rainbow(), zone, 0.25 + once_around)
            );
        }
    }

    #[test]
    fn a_pulse_fades_to_a_dim_copy_of_its_color_and_back() {
        let period = Period::new(Duration::from_secs(1)).unwrap();
        let bright = color("00FF88");
        let effect = Effect {
            pattern: Pattern::Pulse {
                color: bright,
                ease: Ease::LINEAR,
            },
            period,
        };
        let dim = Color::BLACK.mix(bright, 0.08);
        assert_eq!(at(&effect, Zone::Left, 0.0), bright);
        assert_eq!(at(&effect, Zone::Left, 0.5), bright.mix(dim, 0.5));
        assert_eq!(at(&effect, Zone::Left, 1.0), dim);
        assert_eq!(at(&effect, Zone::Left, 2.0), bright);
    }

    #[test]
    fn caps_lock_keeps_the_left_zone_red_most_of_the_time() {
        let settings = CapsLock::default();
        let around = settings.delay * u32::try_from(settings.colors.len()).unwrap();
        let samples = 1000;
        let red = (0..samples)
            .filter(|&i| caps_color(&settings, around * i / samples) == color("FF0000"))
            .count();
        assert!(red > 500, "red in {red} of 1000 samples");
    }

    /// The left zone gets a new color once per frame. Wherever the frames fall, at least two of
    /// them have to land in the white part for the flash to be seen.
    #[test]
    fn caps_lock_flashes_white_for_at_least_2_frames_however_the_frames_line_up() {
        let settings = CapsLock::default();
        let frame = Duration::from_millis(100);
        let frames_around = 14;
        for phase in 0..10 {
            let first = Duration::from_millis(phase * 10);
            let white = (0..frames_around)
                .map(|k| caps_color(&settings, first + frame * k))
                .filter(|shown| !shown.differs_visibly_from(Color::WHITE, 4))
                .count();
            assert!(white >= 2, "phase {phase}: {white} white frames");
        }
    }

    fn transition(curve: Ease) -> (Transition, Instant) {
        let start = Instant::now();
        let length = Duration::from_secs(2);
        (
            Transition {
                from: Color::BLACK,
                to: Color::WHITE,
                start,
                length,
                curve,
            },
            start,
        )
    }

    #[test]
    fn a_transition_starts_from_the_old_color_and_ends_exactly_on_the_new_one() {
        let (transition, start) = transition(Ease::LINEAR);
        assert_eq!(transition.color_at(start), Color::BLACK);
        assert_eq!(
            transition.color_at(start + Duration::from_secs(1)),
            color("808080")
        );
        assert_eq!(
            transition.color_at(start + Duration::from_millis(1500)),
            color("BFBFBF")
        );
        assert_eq!(
            transition.color_at(start + Duration::from_secs(2)),
            Color::WHITE
        );
        assert_eq!(
            transition.color_at(start + Duration::from_secs(9)),
            Color::WHITE
        );
    }

    #[test]
    fn an_ease_in_transition_is_less_than_halfway_there_at_half_time() {
        let (transition, start) = transition(Ease::EASE_IN);
        let halfway = transition.color_at(start + Duration::from_secs(1));
        assert_eq!(
            halfway,
            Color::BLACK.mix(Color::WHITE, Ease::EASE_IN.at(0.5))
        );
        assert!(halfway.to_string().as_str() < "808080");
    }
}
