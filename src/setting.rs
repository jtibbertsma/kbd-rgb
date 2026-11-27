//! What a zone can be set to.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::color::Color;
use crate::ease::Ease;

/// What a zone is set to, as it was asked for. The colors that an effect goes through are worked
/// out from this when a frame is drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Setting {
    Color(Color),
    Effect(Effect),
}

/// A color that changes over time, over and over, until the zone gets a new setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Effect {
    pub(crate) pattern: Pattern,
    pub(crate) period: Period,
}

/// The colors an effect goes through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Pattern {
    /// Fades from each step's color to the next, then back to the first.
    Cycle(Cycle),
    /// Fades through the rainbow's colors. Each zone is further along them than the zone to its
    /// left. `ease` is the curve into every color.
    Rainbow { ease: Ease },
    /// Fades between `color` and a dim copy of it. `ease` is the curve in both directions.
    Pulse { color: Color, ease: Ease },
}

impl Pattern {
    /// The pattern's name in `kbdrgb`'s commands and in the color dialog.
    pub(crate) const fn name(&self) -> &'static str {
        match self {
            Pattern::Cycle(_) => "cycle",
            Pattern::Rainbow { .. } => "rainbow",
            Pattern::Pulse { .. } => "pulse",
        }
    }

    /// The curve that every fade of the pattern has, or `None` if a cycle's steps have different
    /// curves.
    pub(crate) fn common_curve(&self) -> Option<Ease> {
        match self {
            Pattern::Cycle(cycle) => cycle.common_curve(),
            Pattern::Rainbow { ease } | Pattern::Pulse { ease, .. } => Some(*ease),
        }
    }

    /// The colors that the pattern was given, as `kbdrgb` and the color dialog write them.
    pub(crate) fn color_words(&self) -> Vec<String> {
        match self {
            Pattern::Cycle(cycle) => cycle.step_words(),
            Pattern::Rainbow { .. } => vec![],
            Pattern::Pulse { color, .. } => vec![color.to_string()],
        }
    }
}

/// One color of a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct Step {
    pub(crate) color: Color,
    /// The curve for fading into `color` from the step before it.
    pub(crate) curve: Ease,
}

/// The steps of a cycle. There are always 2 to 16 of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Step>", into = "Vec<Step>")]
pub(crate) struct Cycle(Vec<Step>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cycle requires 2 to 16 colors.")]
pub(crate) struct WrongStepCount;

impl Cycle {
    /// The most colors in one cycle.
    pub(crate) const MAX_STEPS: usize = 16;

    pub(crate) fn new(steps: Vec<Step>) -> Result<Self, WrongStepCount> {
        if (2..=Cycle::MAX_STEPS).contains(&steps.len()) {
            Ok(Cycle(steps))
        } else {
            Err(WrongStepCount)
        }
    }

    pub(crate) fn steps(&self) -> &[Step] {
        &self.0
    }

    /// The curve that every step has, or `None` if the steps have different curves.
    pub(crate) fn common_curve(&self) -> Option<Ease> {
        let first = self.0[0].curve;
        self.0
            .iter()
            .all(|step| step.curve == first)
            .then_some(first)
    }

    /// Each step as `COLOR`, or as `COLOR:CURVE` if the steps have different curves and this one's
    /// isn't linear.
    fn step_words(&self) -> Vec<String> {
        let own_curves = self.common_curve().is_none();
        self.0
            .iter()
            .map(|step| {
                if own_curves && step.curve != Ease::LINEAR {
                    format!("{}:{}", step.color, step.curve)
                } else {
                    step.color.to_string()
                }
            })
            .collect()
    }
}

impl TryFrom<Vec<Step>> for Cycle {
    type Error = WrongStepCount;

    fn try_from(steps: Vec<Step>) -> Result<Self, Self::Error> {
        Cycle::new(steps)
    }
}

impl From<Cycle> for Vec<Step> {
    fn from(cycle: Cycle) -> Self {
        cycle.0
    }
}

/// How long an effect takes to fade from one color to the next: 0.1 to 60 seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub(crate) struct Period(Duration);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The time must be from 0.1 to 60 seconds.")]
pub(crate) struct PeriodOutOfRange;

impl Period {
    pub(crate) const MIN: Period = Period(Duration::from_millis(100));
    pub(crate) const MAX: Period = Period(Duration::from_secs(60));

    pub(crate) fn new(time: Duration) -> Result<Self, PeriodOutOfRange> {
        let period = Period(time);
        if (Period::MIN..=Period::MAX).contains(&period) {
            Ok(period)
        } else {
            Err(PeriodOutOfRange)
        }
    }

    /// The period nearest to `time`.
    pub(crate) fn clamped(time: Duration) -> Self {
        Period(time).clamp(Period::MIN, Period::MAX)
    }

    pub(crate) const fn get(self) -> Duration {
        self.0
    }
}

impl TryFrom<f64> for Period {
    type Error = PeriodOutOfRange;

    fn try_from(seconds: f64) -> Result<Self, Self::Error> {
        let time = Duration::try_from_secs_f64(seconds).map_err(|_| PeriodOutOfRange)?;
        Period::new(time)
    }
}

impl From<Period> for f64 {
    fn from(period: Period) -> Self {
        period.0.as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(color: &str, curve: Ease) -> Step {
        Step {
            color: color.parse().unwrap(),
            curve,
        }
    }

    #[test]
    fn a_cycle_has_2_to_16_steps() {
        let steps = |count| vec![step("FF0000", Ease::LINEAR); count];
        assert_eq!(Cycle::new(steps(1)), Err(WrongStepCount));
        assert!(Cycle::new(steps(2)).is_ok());
        assert!(Cycle::new(steps(16)).is_ok());
        assert_eq!(Cycle::new(steps(17)), Err(WrongStepCount));
    }

    #[test]
    fn steps_with_one_curve_print_as_colors() {
        let cycle = Cycle::new(vec![
            step("FF0000", Ease::EASE_IN),
            step("0000FF", Ease::EASE_IN),
        ]);
        let cycle = cycle.unwrap();
        assert_eq!(cycle.common_curve(), Some(Ease::EASE_IN));
        assert_eq!(cycle.step_words(), ["FF0000", "0000FF"]);
    }

    #[test]
    fn steps_with_different_curves_print_the_curves_that_are_not_linear() {
        let cycle = Cycle::new(vec![
            step("FF0000", Ease::LINEAR),
            step("0000FF", Ease::EASE_IN),
        ]);
        let cycle = cycle.unwrap();
        assert_eq!(cycle.common_curve(), None);
        assert_eq!(cycle.step_words(), ["FF0000", "0000FF:ease-in"]);
    }

    #[test]
    fn a_period_is_from_a_tenth_of_a_second_to_a_minute() {
        assert_eq!(
            Period::new(Duration::from_millis(99)),
            Err(PeriodOutOfRange)
        );
        assert!(Period::new(Duration::from_millis(100)).is_ok());
        assert!(Period::new(Duration::from_secs(60)).is_ok());
        assert_eq!(
            Period::new(Duration::from_millis(60_001)),
            Err(PeriodOutOfRange)
        );
    }

    #[test]
    fn clamped_gives_the_nearest_period() {
        assert_eq!(Period::clamped(Duration::from_millis(10)), Period::MIN);
        assert_eq!(Period::clamped(Duration::from_secs(100)), Period::MAX);
        assert_eq!(
            Period::clamped(Duration::from_secs(2)).get(),
            Duration::from_secs(2)
        );
    }
}
