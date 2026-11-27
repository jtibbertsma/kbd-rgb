//! Getting colors onto the keyboard.

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use super::frames::Frames;
use crate::color::Color;
use crate::leds::LedWriter;
use crate::zone::{PerZone, Zone};

/// Writes zone colors through a [`LedWriter`]. A color that a change asked for is written at once.
/// The colors of a fade are written one zone at a time, spaced out by [`Frames`]. It knows
/// nothing about scenes or effects. It is told which zones are fading, and it asks for their
/// colors.
#[derive(Debug)]
pub(crate) struct Output<W> {
    leds: W,
    /// The last color written to each zone.
    written: PerZone<Option<Color>>,
    frames: Frames,
    /// A fading color is written again only once a channel has moved 1/`visible_step` of the
    /// brightest channel. Each write costs the kernel a few dozen ACPI interrupts.
    visible_step: u32,
}

impl<W: LedWriter> Output<W> {
    pub(crate) fn new(leds: W, frame: Duration, visible_step: NonZeroU32) -> Self {
        Output {
            leds,
            written: PerZone::from_fn(|_| None),
            frames: Frames::new(frame),
            visible_step: visible_step.get(),
        }
    }

    /// Writes `color` to `zone`, even if it is the color that was written there last. Another
    /// program may have written a different one since.
    pub(crate) fn draw(&mut self, zone: Zone, color: Color, now: Instant) {
        match self.leds.write(zone, color) {
            Ok(()) => {
                self.written[zone] = Some(color);
                self.frames.wrote(zone, now);
            }
            Err(error) => log::warn!("write failed: {zone}: {error}"),
        }
    }

    /// Writes `color` to `zone`, unless it is the color that was written there last.
    pub(crate) fn show(&mut self, zone: Zone, color: Color, now: Instant) {
        if self.written[zone] != Some(color) {
            self.draw(zone, color, now);
        }
    }

    /// Draws the next frame of a fade at `now`: it picks one of the zones in `fading`, asks
    /// `color_of` for its color, and writes the color if it looks different from the last one
    /// written there. It does nothing if every fading zone was drawn less than a frame ago.
    pub(crate) fn frame(
        &mut self,
        fading: &[Zone],
        color_of: impl Fn(Zone) -> Color,
        now: Instant,
    ) {
        let Some(zone) = self.frames.next(fading, now) else {
            return;
        };
        let color = color_of(zone);
        let visible = self.written[zone]
            .is_none_or(|written| color.differs_visibly_from(written, self.visible_step));
        if visible {
            self.draw(zone, color, now);
        }
    }

    /// Works out when the next frame is due, given the zones in `fading`. Call it after a change
    /// and after a frame.
    pub(crate) fn schedule(&mut self, fading: &[Zone], now: Instant) {
        self.frames.schedule(fading, now);
    }

    /// When the next frame is due, or `None` if nothing is fading.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.frames.due()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeLeds;
    use crate::zone::Zone::{Center, Left};

    fn color(text: &str) -> Color {
        text.parse().unwrap()
    }

    fn output(leds: &FakeLeds) -> Output<FakeLeds> {
        Output::new(
            leds.clone(),
            Duration::from_millis(100),
            NonZeroU32::new(32).unwrap(),
        )
    }

    #[test]
    fn draw_writes_a_color_even_if_it_was_written_last() {
        let leds = FakeLeds::default();
        let mut output = output(&leds);
        let now = Instant::now();
        output.draw(Left, color("FF0000"), now);
        output.draw(Left, color("FF0000"), now);
        assert_eq!(
            leds.take(),
            [(Left, color("FF0000")), (Left, color("FF0000"))]
        );
    }

    #[test]
    fn show_skips_a_color_that_was_written_last() {
        let leds = FakeLeds::default();
        let mut output = output(&leds);
        let now = Instant::now();
        output.show(Left, color("FF0000"), now);
        output.show(Left, color("FF0000"), now);
        output.show(Left, color("00FF00"), now);
        assert_eq!(
            leds.take(),
            [(Left, color("FF0000")), (Left, color("00FF00"))]
        );
    }

    #[test]
    fn a_frame_skips_a_color_that_barely_differs_from_the_last_one_written() {
        let leds = FakeLeds::default();
        let mut output = output(&leds);
        let start = Instant::now();
        let frame = Duration::from_millis(100);
        output.draw(Left, color("00FF00"), start);
        output.frame(&[Left], |_| color("00F807"), start + frame);
        assert_eq!(leds.take(), [(Left, color("00FF00"))]);
        output.frame(&[Left], |_| color("00F708"), start + frame * 2);
        assert_eq!(leds.take(), [(Left, color("00F708"))]);
    }

    #[test]
    fn a_frame_draws_one_zone() {
        let leds = FakeLeds::default();
        let mut output = output(&leds);
        output.frame(&[Left, Center], |_| color("123456"), Instant::now());
        assert_eq!(leds.take(), [(Left, color("123456"))]);
    }

    #[test]
    fn a_write_that_failed_is_not_taken_as_written() {
        let leds = FakeLeds::default();
        let mut output = output(&leds);
        let now = Instant::now();
        leds.fail(true);
        output.show(Left, color("FF0000"), now);
        leds.fail(false);
        output.show(Left, color("FF0000"), now);
        assert_eq!(leds.take(), [(Left, color("FF0000"))]);
    }

    #[test]
    fn the_next_frame_is_due_a_frame_after_a_fading_zone_was_drawn() {
        let leds = FakeLeds::default();
        let mut output = output(&leds);
        let now = Instant::now();
        output.draw(Left, color("FF0000"), now);
        output.schedule(&[], now);
        assert_eq!(output.next_due(), None);
        output.schedule(&[Left], now);
        assert_eq!(output.next_due(), Some(now + Duration::from_millis(100)));
    }
}
