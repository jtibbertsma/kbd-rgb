//! Colors and effects that a Menu combo, the color dialog, or `kbdrgb` asks for.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::color::Color;
use crate::ease::Ease;
use crate::seconds::format_seconds;
use crate::setting::{Cycle, Pattern, Period, PeriodOutOfRange, Step, WrongStepCount};
use crate::zone::Target;

/// A change to what part of the keyboard is set to. It prints as the arguments you'd type after
/// `kbdrgb` to get it, such as `cycle -z all -t 2 FF0000 0000FF`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Action {
    pub(crate) target: Target,
    pub(crate) change: Change,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum Change {
    /// Sets `color`, fading to it over `fade` along `curve`.
    Color {
        color: Color,
        fade: FadeTime,
        curve: Ease,
    },
    /// Starts an effect. Without a `period`, the effect uses the default from the settings file.
    /// Without a `time_limit`, it runs until its zone gets a new setting.
    Effect {
        pattern: Pattern,
        period: Option<Period>,
        time_limit: Option<TimeLimit>,
    },
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = self.target;
        let (name, period, time_limit, curve, colors) = match &self.change {
            Change::Color { color, fade, curve } => {
                write!(f, "color -z {target}")?;
                if !fade.get().is_zero() {
                    write!(f, " -t {}", format_seconds(fade.get()))?;
                }
                if *curve != Ease::LINEAR {
                    write!(f, " -e {curve}")?;
                }
                return write!(f, " {color}");
            }
            Change::Effect {
                pattern,
                period,
                time_limit,
            } => match pattern {
                Pattern::Cycle(cycle) => (
                    "cycle",
                    period,
                    time_limit,
                    cycle.common_curve(),
                    cycle.step_words(),
                ),
                Pattern::Rainbow { ease } => ("rainbow", period, time_limit, Some(*ease), vec![]),
                Pattern::Pulse { color, ease } => (
                    "pulse",
                    period,
                    time_limit,
                    Some(*ease),
                    vec![color.to_string()],
                ),
            },
        };
        write!(f, "{name} -z {target}")?;
        if let Some(period) = period {
            write!(f, " -t {}", format_seconds(period.get()))?;
        }
        if let Some(time_limit) = time_limit {
            write!(f, " -d {}", format_seconds(time_limit.get()))?;
        }
        if let Some(curve) = curve.filter(|curve| *curve != Ease::LINEAR) {
            write!(f, " -e {curve}")?;
        }
        colors.iter().try_for_each(|color| write!(f, " {color}"))
    }
}

/// How long a zone takes to fade to a new color: 0 to 60 seconds. Zero sets the color at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub(crate) struct FadeTime(Duration);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The time must be from 0 to 60 seconds.")]
pub(crate) struct FadeTimeOutOfRange;

impl FadeTime {
    pub(crate) const MAX: FadeTime = FadeTime(Duration::from_secs(60));

    pub(crate) fn new(time: Duration) -> Result<Self, FadeTimeOutOfRange> {
        if FadeTime(time) <= FadeTime::MAX {
            Ok(FadeTime(time))
        } else {
            Err(FadeTimeOutOfRange)
        }
    }

    pub(crate) const fn get(self) -> Duration {
        self.0
    }
}

impl TryFrom<f64> for FadeTime {
    type Error = FadeTimeOutOfRange;

    fn try_from(seconds: f64) -> Result<Self, Self::Error> {
        let time = Duration::try_from_secs_f64(seconds).map_err(|_| FadeTimeOutOfRange)?;
        FadeTime::new(time)
    }
}

impl From<FadeTime> for f64 {
    fn from(fade: FadeTime) -> Self {
        fade.0.as_secs_f64()
    }
}

/// How long a timed effect runs: 0.1 seconds to a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub(crate) struct TimeLimit(Duration);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("The duration must be from 0.1 seconds to a day (86400 seconds).")]
pub(crate) struct TimeLimitOutOfRange;

impl TimeLimit {
    pub(crate) const MIN: TimeLimit = TimeLimit(Duration::from_millis(100));
    pub(crate) const MAX: TimeLimit = TimeLimit(Duration::from_hours(24));

    pub(crate) fn new(time: Duration) -> Result<Self, TimeLimitOutOfRange> {
        let time_limit = TimeLimit(time);
        if (TimeLimit::MIN..=TimeLimit::MAX).contains(&time_limit) {
            Ok(time_limit)
        } else {
            Err(TimeLimitOutOfRange)
        }
    }

    pub(crate) const fn get(self) -> Duration {
        self.0
    }
}

impl TryFrom<f64> for TimeLimit {
    type Error = TimeLimitOutOfRange;

    fn try_from(seconds: f64) -> Result<Self, Self::Error> {
        let time = Duration::try_from_secs_f64(seconds).map_err(|_| TimeLimitOutOfRange)?;
        TimeLimit::new(time)
    }
}

impl From<TimeLimit> for f64 {
    fn from(time_limit: TimeLimit) -> Self {
        time_limit.0.as_secs_f64()
    }
}

/// Which kind of action a [`Draft`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Color,
    Cycle,
    Rainbow,
    Pulse,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Color => "color",
            Kind::Cycle => "cycle",
            Kind::Rainbow => "rainbow",
            Kind::Pulse => "pulse",
        })
    }
}

/// An action as the user typed it, before it has been checked. The color dialog's text and
/// `kbdrgb`'s arguments both become a `Draft`, and [`Draft::build`] is the only way to turn one into
/// an [`Action`], so an `Action` is always valid.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Draft {
    pub(crate) kind: Kind,
    /// Each color, with the curve it was given as `COLOR:CURVE`.
    pub(crate) steps: Vec<(Color, Option<Ease>)>,
    /// The time to fade to a color, or an effect's time from one color to the next.
    pub(crate) time: Option<Duration>,
    /// The curve for every fade, except into a color that has its own.
    pub(crate) ease: Option<Ease>,
    pub(crate) time_limit: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ActionError {
    #[error("{0} doesn't have color arguments.")]
    TakesNoColors(Kind),
    #[error("{0} requires one color.")]
    TakesOneColor(Kind),
    #[error(transparent)]
    StepCount(#[from] WrongStepCount),
    #[error(transparent)]
    FadeTime(#[from] FadeTimeOutOfRange),
    #[error(transparent)]
    Period(#[from] PeriodOutOfRange),
    #[error("A duration doesn't work with {0}.")]
    TakesNoTimeLimit(Kind),
    #[error(transparent)]
    TimeLimit(#[from] TimeLimitOutOfRange),
    #[error("{0} uses one curve for every step, so COLOR:CURVE doesn't work with it.")]
    TakesOneCurve(Kind),
}

impl Draft {
    pub(crate) fn new(kind: Kind) -> Self {
        Draft {
            kind,
            steps: vec![],
            time: None,
            ease: None,
            time_limit: None,
        }
    }

    /// Checks the draft and turns it into an action for `target`.
    pub(crate) fn build(self, target: Target) -> Result<Action, ActionError> {
        let kind = self.kind;
        let curve = self.ease.unwrap_or(Ease::LINEAR);
        let change = match kind {
            Kind::Color => {
                let [(color, own_curve)] = self.steps[..] else {
                    return Err(ActionError::TakesOneColor(kind));
                };
                let fade = FadeTime::new(self.time.unwrap_or_default())?;
                if self.time_limit.is_some() {
                    return Err(ActionError::TakesNoTimeLimit(kind));
                }
                Change::Color {
                    color,
                    fade,
                    curve: own_curve.unwrap_or(curve),
                }
            }
            Kind::Cycle => {
                let steps = self.steps.iter().map(|&(color, own_curve)| Step {
                    color,
                    curve: own_curve.unwrap_or(curve),
                });
                let pattern = Pattern::Cycle(Cycle::new(steps.collect())?);
                Change::Effect {
                    pattern,
                    period: self.period()?,
                    time_limit: self.time_limit()?,
                }
            }
            Kind::Rainbow => {
                if !self.steps.is_empty() {
                    return Err(ActionError::TakesNoColors(kind));
                }
                let pattern = Pattern::Rainbow { ease: curve };
                Change::Effect {
                    pattern,
                    period: self.period()?,
                    time_limit: self.time_limit()?,
                }
            }
            Kind::Pulse => {
                let [(color, own_curve)] = self.steps[..] else {
                    return Err(ActionError::TakesOneColor(kind));
                };
                let (period, time_limit) = (self.period()?, self.time_limit()?);
                if own_curve.is_some() {
                    return Err(ActionError::TakesOneCurve(kind));
                }
                Change::Effect {
                    pattern: Pattern::Pulse { color, ease: curve },
                    period,
                    time_limit,
                }
            }
        };
        Ok(Action { target, change })
    }

    fn period(&self) -> Result<Option<Period>, PeriodOutOfRange> {
        self.time.map(Period::new).transpose()
    }

    fn time_limit(&self) -> Result<Option<TimeLimit>, TimeLimitOutOfRange> {
        self.time_limit.map(TimeLimit::new).transpose()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::zone::Zone;

    fn draft(kind: Kind, colors: &[&str]) -> Draft {
        let steps = colors
            .iter()
            .map(|color| (color.parse().unwrap(), None))
            .collect();
        Draft {
            steps,
            ..Draft::new(kind)
        }
    }

    fn built(draft: Draft) -> String {
        match draft.build(Target::Zone(Zone::Left)) {
            Ok(action) => action.to_string(),
            Err(error) => error.to_string(),
        }
    }

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn a_color_is_set_at_once_and_along_a_straight_line_by_default() {
        assert_eq!(
            built(draft(Kind::Color, &["FF8800"])),
            "color -z left FF8800"
        );
    }

    #[test]
    fn a_color_can_have_a_fade_time_and_a_curve() {
        let draft = Draft {
            time: Some(2 * SECOND),
            ease: Some(Ease::EASE_IN),
            ..draft(Kind::Color, &["FF8800"])
        };
        assert_eq!(built(draft), "color -z left -t 2 -e ease-in FF8800");
    }

    #[test]
    fn a_colors_own_curve_takes_the_place_of_the_curve_for_every_fade() {
        let mut draft = draft(Kind::Color, &["FF8800"]);
        draft.steps[0].1 = Some(Ease::EASE_OUT);
        draft.ease = Some(Ease::EASE_IN);
        assert_eq!(built(draft), "color -z left -e ease-out FF8800");
    }

    #[test]
    fn a_cycle_prints_one_curve_for_all_steps_as_an_option() {
        let draft = Draft {
            ease: Some(Ease::EASE_IN),
            ..draft(Kind::Cycle, &["FF0000", "00FF00"])
        };
        assert_eq!(built(draft), "cycle -z left -e ease-in FF0000 00FF00");
    }

    #[test]
    fn a_cycle_prints_different_curves_with_their_colors() {
        let mut draft = draft(Kind::Cycle, &["FF0000", "FFFF00", "0000FF"]);
        draft.steps[1].1 = Some(Ease::EASE_IN);
        draft.time = Some(2 * SECOND);
        draft.time_limit = Some(30 * SECOND);
        assert_eq!(
            built(draft),
            "cycle -z left -t 2 -d 30 FF0000 FFFF00:ease-in 0000FF"
        );
    }

    #[test]
    fn rainbow_and_pulse_print_their_options() {
        let rainbow = Draft {
            time_limit: Some(20 * SECOND),
            ..draft(Kind::Rainbow, &[])
        };
        assert_eq!(built(rainbow), "rainbow -z left -d 20");
        let pulse = Draft {
            time: Some(SECOND * 3 / 2),
            ..draft(Kind::Pulse, &["00FFAA"])
        };
        assert_eq!(built(pulse), "pulse -z left -t 1.5 00FFAA");
    }

    #[rstest]
    #[case(Kind::Color, &[], "color requires one color.")]
    #[case(Kind::Color, &["FF0000", "00FF00"], "color requires one color.")]
    #[case(Kind::Cycle, &["FF0000"], "cycle requires 2 to 16 colors.")]
    #[case(Kind::Cycle, &["FF0000"; 17], "cycle requires 2 to 16 colors.")]
    #[case(Kind::Rainbow, &["FF0000"], "rainbow doesn't have color arguments.")]
    #[case(Kind::Pulse, &[], "pulse requires one color.")]
    #[case(Kind::Pulse, &["FF0000", "00FF00"], "pulse requires one color.")]
    fn each_kind_requires_its_number_of_colors(
        #[case] kind: Kind,
        #[case] colors: &[&str],
        #[case] problem: &str,
    ) {
        assert_eq!(built(draft(kind, colors)), problem);
    }

    #[rstest]
    #[case(Kind::Color, &["FF0000"], 61.0, "The time must be from 0 to 60 seconds.")]
    #[case(Kind::Cycle, &["FF0000", "00FF00"], 0.0, "The time must be from 0.1 to 60 seconds.")]
    #[case(Kind::Cycle, &["FF0000", "00FF00"], 0.05, "The time must be from 0.1 to 60 seconds.")]
    #[case(Kind::Rainbow, &[], 61.0, "The time must be from 0.1 to 60 seconds.")]
    #[case(Kind::Pulse, &["FF0000"], 0.0, "The time must be from 0.1 to 60 seconds.")]
    fn the_time_has_limits(
        #[case] kind: Kind,
        #[case] colors: &[&str],
        #[case] seconds: f64,
        #[case] problem: &str,
    ) {
        let draft = Draft {
            time: Some(Duration::from_secs_f64(seconds)),
            ..draft(kind, colors)
        };
        assert_eq!(built(draft), problem);
    }

    #[rstest]
    #[case(0.05)]
    #[case(86401.0)]
    fn the_duration_has_limits(#[case] seconds: f64) {
        let time_limit = Some(Duration::from_secs_f64(seconds));
        let draft = Draft {
            time_limit,
            ..draft(Kind::Rainbow, &[])
        };
        assert_eq!(
            built(draft),
            "The duration must be from 0.1 seconds to a day (86400 seconds)."
        );
    }

    #[test]
    fn the_longest_and_shortest_times_work() {
        let draft = Draft {
            time: Some(Duration::from_secs(60)),
            time_limit: Some(Duration::from_hours(24)),
            ..draft(Kind::Rainbow, &[])
        };
        assert_eq!(built(draft), "rainbow -z left -t 60 -d 86400");
    }

    #[test]
    fn a_color_has_no_duration() {
        let draft = Draft {
            time_limit: Some(3 * SECOND),
            ..draft(Kind::Color, &["FF0000"])
        };
        assert_eq!(built(draft), "A duration doesn't work with color.");
    }

    #[test]
    fn a_pulse_color_has_no_curve_of_its_own() {
        let mut draft = draft(Kind::Pulse, &["00FFAA"]);
        draft.steps[0].1 = Some(Ease::EASE_IN);
        assert_eq!(
            built(draft),
            "pulse uses one curve for every step, so COLOR:CURVE doesn't work with it."
        );
    }
}
