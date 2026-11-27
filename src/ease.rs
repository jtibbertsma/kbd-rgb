//! Easing curves.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::seconds::{format_decimal, parse_decimal};

/// How a fade speeds up and slows down. It holds the two control points (`x1`, `y1`) and
/// (`x2`, `y2`) of a cubic bezier from (0, 0) to (1, 1), as in CSS. Each coordinate is from 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Ease {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl Ease {
    pub(crate) const LINEAR: Ease = Ease::at_points(0.0, 0.0, 1.0, 1.0);
    pub(crate) const EASE: Ease = Ease::at_points(0.25, 0.1, 0.25, 1.0);
    pub(crate) const EASE_IN: Ease = Ease::at_points(0.42, 0.0, 1.0, 1.0);
    pub(crate) const EASE_OUT: Ease = Ease::at_points(0.0, 0.0, 0.58, 1.0);
    pub(crate) const EASE_IN_OUT: Ease = Ease::at_points(0.42, 0.0, 0.58, 1.0);

    /// The named curves from CSS, with the same control points.
    const NAMED: [(&'static str, Ease); 5] = [
        ("linear", Ease::LINEAR),
        ("ease", Ease::EASE),
        ("ease-in", Ease::EASE_IN),
        ("ease-out", Ease::EASE_OUT),
        ("ease-in-out", Ease::EASE_IN_OUT),
    ];

    const fn at_points(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
        Ease { x1, y1, x2, y2 }
    }

    /// A curve with the given control points, or `None` if a coordinate is outside 0 to 1.
    pub(crate) fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Option<Self> {
        [x1, y1, x2, y2]
            .iter()
            .all(|coordinate| (0.0..=1.0).contains(coordinate))
            .then_some(Ease::at_points(x1, y1, x2, y2))
    }

    /// Maps how far through its time a fade is (`x`, from 0 to 1) to how far between its colors it
    /// is, like CSS `cubic-bezier()`.
    pub(crate) fn at(self, x: f64) -> f64 {
        #[expect(
            clippy::float_cmp,
            reason = "exactly equal points are exactly a straight line"
        )]
        if self.x1 == self.y1 && self.x2 == self.y2 {
            return x;
        }
        // x grows with t, because x1 and x2 are from 0 to 1, so bisect for the t that gives x.
        let (mut low, mut high) = (0.0, 1.0);
        for _ in 0..30 {
            let t = f64::midpoint(low, high);
            if bezier(self.x1, self.x2, t) < x {
                low = t;
            } else {
                high = t;
            }
        }
        bezier(self.y1, self.y2, f64::midpoint(low, high))
    }
}

/// One coordinate of a cubic bezier whose end points are 0 and 1, with control points `a` and `b`.
fn bezier(a: f64, b: f64, t: f64) -> f64 {
    3.0 * (1.0 - t).powi(2) * t * a + 3.0 * (1.0 - t) * t * t * b + t.powi(3)
}

impl fmt::Display for Ease {
    /// Prints the curve's name if it has one, and its four coordinates otherwise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some((name, _)) = Ease::NAMED.iter().find(|(_, named)| named == self) {
            return f.write_str(name);
        }
        let [x1, y1, x2, y2] = [self.x1, self.y1, self.x2, self.y2].map(format_decimal);
        write!(f, "{x1},{y1},{x2},{y2}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "\"{0}\" isn't an easing curve. Use linear, ease, ease-in, ease-out, ease-in-out, or 4 numbers \
     from 0 to 1 like 0.4,0,0.2,1."
)]
pub(crate) struct NotAnEase(String);

impl FromStr for Ease {
    type Err = NotAnEase;

    /// Reads a curve's name in any case, or `X1,Y1,X2,Y2`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let name = text.to_lowercase();
        if let Some((_, named)) = Ease::NAMED.iter().find(|(named, _)| *named == name) {
            return Ok(*named);
        }
        let coordinates: Option<Vec<f64>> = text.split(',').map(parse_decimal).collect();
        match coordinates.as_deref() {
            Some(&[x1, y1, x2, y2]) => Ease::new(x1, y1, x2, y2),
            _ => None,
        }
        .ok_or_else(|| NotAnEase(text.to_owned()))
    }
}

impl TryFrom<String> for Ease {
    type Error = NotAnEase;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<Ease> for String {
    fn from(ease: Ease) -> Self {
        ease.to_string()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn curves() -> Vec<Ease> {
        let own = [
            (0.4, 0.0, 0.2, 1.0),
            (1.0, 0.0, 0.0, 1.0),
            (0.0, 1.0, 1.0, 0.0),
        ];
        let own = own.map(|(x1, y1, x2, y2)| Ease::new(x1, y1, x2, y2).unwrap());
        Ease::NAMED
            .iter()
            .map(|(_, curve)| *curve)
            .chain(own)
            .collect()
    }

    #[test]
    fn every_curve_starts_at_0_and_ends_at_1() {
        for curve in curves() {
            assert!(curve.at(0.0).abs() < 1e-6, "{curve} at 0");
            assert!((curve.at(1.0) - 1.0).abs() < 1e-6, "{curve} at 1");
        }
    }

    #[test]
    fn no_curve_goes_backwards() {
        for curve in curves() {
            let points: Vec<f64> = (0..=20).map(|i| curve.at(f64::from(i) / 20.0)).collect();
            assert!(
                points.windows(2).all(|pair| pair[0] <= pair[1] + 1e-9),
                "{curve}: {points:?}"
            );
        }
    }

    #[rstest]
    #[case(0.0)]
    #[case(0.1)]
    #[case(0.5)]
    #[case(0.93)]
    fn linear_changes_nothing(#[case] x: f64) {
        assert_eq!(Ease::LINEAR.at(x).to_bits(), x.to_bits());
    }

    #[test]
    fn ease_in_out_is_halfway_at_half_time() {
        assert!((Ease::EASE_IN_OUT.at(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn ease_in_starts_slow_and_ease_out_starts_fast() {
        assert!(Ease::EASE_IN.at(0.3) < 0.3);
        assert!(Ease::EASE_OUT.at(0.3) > 0.3);
    }

    #[test]
    fn ease_matches_css_at_a_quarter_of_the_time() {
        assert!((Ease::EASE.at(0.25) - 0.4085).abs() < 0.002);
    }

    #[rstest]
    #[case("ease-out", Ease::EASE_OUT)]
    #[case("Ease-In", Ease::EASE_IN)]
    #[case("LINEAR", Ease::LINEAR)]
    #[case("0.4,0,0.2,1", Ease::at_points(0.4, 0.0, 0.2, 1.0))]
    #[case("0.42,0,0.58,1", Ease::EASE_IN_OUT)]
    fn an_ease_reads_a_name_or_four_coordinates(#[case] text: &str, #[case] curve: Ease) {
        assert_eq!(text.parse(), Ok(curve));
    }

    #[rstest]
    #[case("")]
    #[case("bouncy")]
    #[case("0.4,0,0.2")]
    #[case("0.4,0,0.2,1.5")]
    #[case("0.4,-1,0.2,1")]
    #[case("1,1,1,1,")]
    #[case("0.4,0,,1")]
    fn other_text_is_not_an_ease(#[case] text: &str) {
        let error = text.parse::<Ease>().unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with(&format!("\"{text}\" isn't an easing curve."))
        );
    }

    #[rstest]
    #[case(Ease::EASE_IN, "ease-in")]
    #[case(Ease::LINEAR, "linear")]
    #[case(Ease::at_points(0.4, 0.0, 0.2, 1.0), "0.4,0,0.2,1")]
    #[case(Ease::at_points(0.9, 0.0, 0.1, 1.0), "0.9,0,0.1,1")]
    fn an_ease_prints_what_it_reads(#[case] curve: Ease, #[case] text: &str) {
        assert_eq!(curve.to_string(), text);
        assert_eq!(text.parse(), Ok(curve));
    }
}
