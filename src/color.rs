//! Colors.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A color the keyboard can have. It reads and prints as `RRGGBB` in uppercase hex, which is what
/// the driver's color files hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Color {
    red: u8,
    green: u8,
    blue: u8,
}

impl Color {
    pub(crate) const BLACK: Color = Color::new(0x00, 0x00, 0x00);
    pub(crate) const WHITE: Color = Color::new(0xFF, 0xFF, 0xFF);

    pub(crate) const fn new(red: u8, green: u8, blue: u8) -> Self {
        Color { red, green, blue }
    }

    const fn channels(self) -> [u8; 3] {
        [self.red, self.green, self.blue]
    }

    /// Blends two colors channel by channel. `fraction` = 0 gives `self`, and `fraction` = 1 gives
    /// `other`.
    pub(crate) fn mix(self, other: Color, fraction: f64) -> Color {
        let blend = |from: u8, to: u8| {
            let (from, to) = (f64::from(from), f64::from(to));
            // A float-to-integer cast drops the fraction, so adding 0.5 first rounds to the nearest.
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the cast is the rounding"
            )]
            let channel = (from + (to - from) * fraction + 0.5) as u8;
            channel
        };
        Color::new(
            blend(self.red, other.red),
            blend(self.green, other.green),
            blend(self.blue, other.blue),
        )
    }

    /// Whether `self` differs enough from `other` to be worth writing to the keyboard: some channel
    /// differs by at least 1/`steps` of the brightest channel of either color, and by at least 1.
    pub(crate) fn differs_visibly_from(self, other: Color, steps: u32) -> bool {
        let pairs = self.channels().into_iter().zip(other.channels());
        let brightest = pairs.clone().map(|(a, b)| a.max(b)).max().unwrap_or(0);
        let step = (f64::from(brightest) / f64::from(steps)).max(1.0);
        pairs
            .into_iter()
            .any(|(a, b)| f64::from(a.abs_diff(b)) >= step)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02X}{:02X}{:02X}", self.red, self.green, self.blue)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("\"{0}\" isn't a color. Use 6 hex digits like FF8800.")]
pub(crate) struct NotAColor(String);

impl FromStr for Color {
    type Err = NotAColor;

    /// Reads 6 hex digits, or 3 hex digits that are each doubled, so `F80` is `FF8800`. A leading
    /// `#` and surrounding spaces are ignored.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let digits: Vec<u8> = text
            .trim()
            .trim_start_matches('#')
            .chars()
            .map(|c| c.to_digit(16).and_then(|digit| u8::try_from(digit).ok()))
            .collect::<Option<_>>()
            .ok_or_else(|| NotAColor(text.to_owned()))?;
        match digits[..] {
            [r, g, b] => Ok(Color::new(r * 0x11, g * 0x11, b * 0x11)),
            [r1, r0, g1, g0, b1, b0] => Ok(Color::new(r1 << 4 | r0, g1 << 4 | g0, b1 << 4 | b0)),
            _ => Err(NotAColor(text.to_owned())),
        }
    }
}

impl TryFrom<String> for Color {
    type Error = NotAColor;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<Color> for String {
    fn from(color: Color) -> Self {
        color.to_string()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn color(text: &str) -> Color {
        text.parse().unwrap()
    }

    #[rstest]
    #[case("FF8800", "FF8800")]
    #[case("#ff8800", "FF8800")]
    #[case("f80", "FF8800")]
    #[case(" 00aaff\n", "00AAFF")]
    fn a_color_reads_hex_digits_and_prints_them_in_uppercase(
        #[case] text: &str,
        #[case] printed: &str,
    ) {
        assert_eq!(color(text).to_string(), printed);
    }

    #[rstest]
    #[case("")]
    #[case("xyz")]
    #[case("12345")]
    #[case("1234567")]
    #[case("FF 800")]
    #[case("+F8800")]
    fn other_text_is_not_a_color(#[case] text: &str) {
        let error = text.parse::<Color>().unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("\"{text}\" isn't a color. Use 6 hex digits like FF8800.")
        );
    }

    #[rstest]
    #[case("000000", "FFFFFF", 0.0, "000000")]
    #[case("000000", "FFFFFF", 1.0, "FFFFFF")]
    #[case("000000", "FFFFFF", 0.5, "808080")]
    #[case("FF0000", "0000FF", 0.25, "BF0040")]
    fn mix_blends_each_channel(
        #[case] from: &str,
        #[case] to: &str,
        #[case] fraction: f64,
        #[case] mixed: &str,
    ) {
        assert_eq!(color(from).mix(color(to), fraction), color(mixed));
    }

    #[rstest]
    #[case("00FF00", "00FE01", false)]
    #[case("00FF00", "00F807", false)]
    #[case("00FF00", "00F708", true)]
    #[case("000000", "000001", true)]
    #[case("080808", "090808", true)]
    #[case("123456", "123456", false)]
    fn a_visible_difference_is_a_32nd_of_the_brightest_channel_and_at_least_1(
        #[case] before: &str,
        #[case] after: &str,
        #[case] visible: bool,
    ) {
        assert_eq!(
            color(after).differs_visibly_from(color(before), 32),
            visible
        );
    }
}
