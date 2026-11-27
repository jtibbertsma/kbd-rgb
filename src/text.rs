//! The text syntax of the color dialog and of `kbdrgb zones`: reading it and printing it.

use std::time::Duration;

use crate::action::{Action, ActionError, Draft, Kind};
use crate::color::{Color, NotAColor};
use crate::ease::{Ease, NotAnEase};
use crate::seconds::{format_seconds, parse_seconds};
use crate::setting::{Pattern, Setting};
use crate::zone::Target;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TextError {
    #[error(transparent)]
    Color(#[from] NotAColor),
    #[error(transparent)]
    Ease(#[from] NotAnEase),
    #[error("Type a color like FF8800.")]
    NoColor,
    #[error(transparent)]
    Action(#[from] ActionError),
}

/// A word of the text. A `COLOR:CURVE` is read as soon as it's seen. Other words wait until the
/// keyword and the time have been taken from the two ends, and what is left must be colors.
enum Item<'a> {
    Word(&'a str),
    Step(Color, Ease),
}

/// Reads `text` as an action for `target`.
pub(crate) fn read(text: &str, target: Target) -> Result<Action, TextError> {
    Ok(parse(text)?.build(target)?)
}

/// Reads `text` as a draft of an action.
///
/// Words are separated by spaces or commas. The first word can be `rainbow`, `pulse` or `cycle` to
/// start that effect. Otherwise, a single color sets that color, and two or more colors start a
/// cycle.
///
/// A number at the end is the time, unless it has 6 digits. A 6-digit number is read as a color.
///
/// A curve on its own can go anywhere, and it applies to every step. If there is more than one, the
/// last one is used. `COLOR:CURVE` applies only to fading into that color.
pub(crate) fn parse(text: &str) -> Result<Draft, TextError> {
    let mut ease = None;
    let mut items = Vec::new();
    for token in text.split_whitespace() {
        let token = token.trim_matches(',');
        if let Some((color, curve)) = token.split_once(':') {
            items.push(Item::Step(color.parse()?, curve.parse()?));
        } else if let Ok(curve) = token.parse() {
            ease = Some(curve);
        } else if looks_like_a_curve(token) {
            token.parse::<Ease>()?; // It isn't one, and the error says why.
        } else {
            items.extend(
                token
                    .split(',')
                    .filter(|word| !word.is_empty())
                    .map(Item::Word),
            );
        }
    }

    let keyword = match items.first() {
        Some(Item::Word(word)) => match word.to_lowercase().as_str() {
            "rainbow" => Some(Kind::Rainbow),
            "pulse" => Some(Kind::Pulse),
            "cycle" => Some(Kind::Cycle),
            _ => None,
        },
        _ => None,
    };
    if keyword.is_some() {
        items.remove(0);
    }

    let time = match items.last() {
        Some(Item::Word(word)) if looks_like_a_time(word) => {
            let seconds = if word.starts_with('.') {
                format!("0{word}")
            } else {
                (*word).to_owned()
            };
            items.pop();
            // A number with too many digits to read is longer than any time that is allowed.
            Some(parse_seconds(&seconds).unwrap_or(Duration::MAX))
        }
        _ => None,
    };

    let steps = items
        .into_iter()
        .map(|item| match item {
            Item::Word(word) => Ok((word.parse()?, None)),
            Item::Step(color, curve) => Ok((color, Some(curve))),
        })
        .collect::<Result<Vec<_>, NotAColor>>()?;
    let kind = match (keyword, steps.len()) {
        (Some(kind), _) => kind,
        (None, 0) => return Err(TextError::NoColor),
        (None, 1) => Kind::Color,
        (None, _) => Kind::Cycle,
    };
    Ok(Draft {
        kind,
        steps,
        time,
        ease,
        time_limit: None,
    })
}

/// Whether `token` is made of numbers with three commas between them, as a curve's four
/// coordinates are.
fn looks_like_a_curve(token: &str) -> bool {
    token
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b'.' || b == b',')
        && token.bytes().filter(|&b| b == b',').count() == 3
}

/// Whether `word` is a number of seconds like `2`, `0.5` or `.5`, and not a 6-digit color.
fn looks_like_a_time(word: &str) -> bool {
    let is_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    let (whole, fraction) = word.split_once('.').unwrap_or(("", word));
    word.len() != 6 && is_digits(whole) && is_digits(fraction) && !fraction.is_empty()
}

/// Prints `setting` the way [`parse`] reads it.
pub(crate) fn describe(setting: &Setting) -> String {
    let effect = match setting {
        Setting::Color(color) => return color.to_string(),
        Setting::Effect(effect) => effect,
    };
    let pattern = &effect.pattern;
    // Two or more colors are a cycle without the word.
    let mut words = match pattern {
        Pattern::Cycle(_) => vec![],
        Pattern::Rainbow { .. } | Pattern::Pulse { .. } => vec![pattern.name().to_owned()],
    };
    words.extend(pattern.color_words());
    words.push(format_seconds(effect.period.get()));
    if let Some(curve) = pattern
        .common_curve()
        .filter(|curve| *curve != Ease::LINEAR)
    {
        words.push(curve.to_string());
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::action::Change;
    use crate::setting::{Effect, Period};
    use crate::zone::Zone;

    const LEFT: Target = Target::Zone(Zone::Left);

    #[rstest]
    #[case("FF8800", "color -z left FF8800")]
    #[case("#ff8800", "color -z left FF8800")]
    #[case("  FF8800  ", "color -z left FF8800")]
    #[case("FF8800 2", "color -z left -t 2 FF8800")]
    #[case("FF8800 .5", "color -z left -t 0.5 FF8800")]
    #[case("FF0000 0000FF", "cycle -z left FF0000 0000FF")]
    #[case("FF0000, 0000FF, 3", "cycle -z left -t 3 FF0000 0000FF")]
    #[case("FF0000,0000FF,0.5", "cycle -z left -t 0.5 FF0000 0000FF")]
    #[case("cycle FF0000 0000FF 1.25", "cycle -z left -t 1.25 FF0000 0000FF")]
    #[case("rainbow", "rainbow -z left")]
    #[case("Rainbow 0.3", "rainbow -z left -t 0.3")]
    #[case("pulse 00ffaa", "pulse -z left 00FFAA")]
    #[case("pulse,8800FF", "pulse -z left 8800FF")]
    #[case("pulse 00FFAA 1.5", "pulse -z left -t 1.5 00FFAA")]
    #[case("112233 445566", "cycle -z left 112233 445566")]
    #[case(
        "FF0000 0000FF 2 ease-in-out",
        "cycle -z left -t 2 -e ease-in-out FF0000 0000FF"
    )]
    #[case("pulse Ease-Out 00FFAA", "pulse -z left -e ease-out 00FFAA")]
    #[case("FF8800 3 0.4,0,0.2,1", "color -z left -t 3 -e 0.4,0,0.2,1 FF8800")]
    #[case("rainbow 0.4 linear", "rainbow -z left -t 0.4")]
    #[case(
        "ease-in FF0000 0000FF ease-out",
        "cycle -z left -e ease-out FF0000 0000FF"
    )]
    #[case(
        "FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1 2",
        "cycle -z left -t 2 FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1"
    )]
    #[case(
        "FF0000, ffff00:Ease-Out, 2 ease-in",
        "cycle -z left -t 2 FF0000:ease-in FFFF00:ease-out"
    )]
    #[case("FF8800:ease-in 3", "color -z left -t 3 -e ease-in FF8800")]
    fn text_is_read_as_an_action(#[case] text: &str, #[case] action: &str) {
        assert_eq!(read(text, LEFT).unwrap().to_string(), action);
    }

    #[test]
    fn the_action_is_for_the_target_that_was_given() {
        assert_eq!(
            read("FF8800 2", Target::All).unwrap().to_string(),
            "color -z all -t 2 FF8800"
        );
    }

    #[rstest]
    #[case("", "Type a color")]
    #[case("2", "Type a color")]
    #[case("zzz", "\"zzz\" isn't a color")]
    #[case("FF8800 nope", "\"nope\" isn't a color")]
    #[case("<b>&", "\"<b>&\" isn't a color")]
    #[case("FF0000 bouncy", "\"bouncy\" isn't a color")]
    #[case("zzz:ease-in FF0000", "\"zzz\" isn't a color")]
    #[case("FF8800 61", "from 0 to 60")]
    #[case("FF8800 1234567", "from 0 to 60")]
    #[case("FF0000 0000FF 0", "from 0.1 to 60")]
    #[case("rainbow FF0000", "doesn't have color arguments")]
    #[case("pulse", "one color")]
    #[case("pulse FF0000 00FF00", "one color")]
    #[case("cycle FF0000", "2 to 16")]
    #[case(&"FF0000 ".repeat(17), "2 to 16")]
    #[case(
        "FF0000 0000FF 0.4,0,0.2,1.5",
        "\"0.4,0,0.2,1.5\" isn't an easing curve"
    )]
    #[case("FF0000 0000FF 0.4,0,,1", "\"0.4,0,,1\" isn't an easing curve")]
    #[case("FF0000 FFFF00:bouncy", "\"bouncy\" isn't an easing curve")]
    #[case("FF0000: 00FF00", "\"\" isn't an easing curve")]
    #[case("pulse 00FFAA:ease-in", "one curve for every step")]
    fn other_text_is_rejected_with_the_reason(#[case] text: &str, #[case] reason: &str) {
        let error = read(text, LEFT).unwrap_err().to_string();
        assert!(error.contains(reason), "{text:?} gave {error:?}");
    }

    /// The setting that `text` asks for, with `period` where the text gives no time.
    fn setting(text: &str, period: &str) -> Setting {
        match read(text, LEFT).unwrap().change {
            Change::Color { color, .. } => Setting::Color(color),
            Change::Effect {
                pattern,
                period: own_period,
                ..
            } => {
                let default = Period::new(parse_seconds(period).unwrap()).unwrap();
                Setting::Effect(Effect {
                    pattern,
                    period: own_period.unwrap_or(default),
                })
            }
        }
    }

    #[rstest]
    #[case("123456", "123456")]
    #[case("AA00AA 00FF00 2", "AA00AA 00FF00 2")]
    #[case("rainbow 0.3", "rainbow 0.3")]
    #[case("pulse 00FFAA", "pulse 00FFAA 0.5")]
    #[case("AA00AA 00FF00 2 ease-in", "AA00AA 00FF00 2 ease-in")]
    #[case("pulse 00FFAA 0.4,0,0.2,1", "pulse 00FFAA 0.5 0.4,0,0.2,1")]
    #[case(
        "FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1",
        "FF0000 FFFF00:ease-in 0000FF:0.9,0,0.1,1 0.5"
    )]
    #[case("FF0000:ease-out 0000FF ease-out", "FF0000 0000FF 0.5 ease-out")]
    #[case("rainbow linear", "rainbow 0.5")]
    fn a_setting_prints_as_text_that_reads_back_as_the_same_setting(
        #[case] text: &str,
        #[case] printed: &str,
    ) {
        let setting = setting(text, "0.5");
        assert_eq!(describe(&setting), printed);
        assert_eq!(self::setting(printed, "9"), setting);
    }
}
