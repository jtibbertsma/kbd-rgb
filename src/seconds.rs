//! Decimal numbers as the user types them, and lengths of time in decimal seconds.

use std::time::Duration;

/// Reads an unsigned decimal like `2` or `0.5`, with up to 6 digits on each side of the point.
/// Unlike `str::parse::<f64>`, it rejects signs, exponents, `inf` and `nan`, which times, settings
/// and curve points never need.
pub(crate) fn parse_decimal(text: &str) -> Option<f64> {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    let is_digits =
        |part: &str| (1..=6).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit());
    if is_digits(whole) && fraction.is_none_or(is_digits) {
        text.parse().ok()
    } else {
        None
    }
}

/// Prints a number with up to 6 significant digits and no trailing zeros, like `%g` in C. For
/// example, 2.0 prints as `2` and 2/3 prints as `0.666667`.
pub(crate) fn format_decimal(number: f64) -> String {
    if number == 0.0 {
        return "0".to_owned();
    }
    // Scientific notation gives the exponent after rounding to 6 significant digits.
    let scientific = format!("{number:.5e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("{:e} always prints an exponent");
    let exponent: i32 = exponent
        .parse()
        .expect("{:e} always prints a whole exponent");
    let trim = |digits: &str| -> String {
        if digits.contains('.') {
            digits
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        } else {
            digits.to_owned()
        }
    };
    if (-4..6).contains(&exponent) {
        let decimals = usize::try_from(5 - exponent).unwrap_or(0);
        trim(&format!("{number:.decimals$}"))
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exponent.abs())
    }
}

/// Reads a length of time in decimal seconds, like `2` or `0.5`.
pub(crate) fn parse_seconds(text: &str) -> Option<Duration> {
    parse_decimal(text).map(Duration::from_secs_f64)
}

/// The inverse of [`parse_seconds`].
pub(crate) fn format_seconds(time: Duration) -> String {
    format_decimal(time.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("2", 2.0)]
    #[case("0.5", 0.5)]
    #[case("86400", 86400.0)]
    #[case("123456.654321", 123_456.654_321)]
    fn parse_decimal_reads_plain_decimals(#[case] text: &str, #[case] number: f64) {
        assert_eq!(parse_decimal(text), Some(number));
    }

    #[rstest]
    #[case("")]
    #[case("-1")]
    #[case("+1")]
    #[case("1e3")]
    #[case("nan")]
    #[case("inf")]
    #[case(".5")]
    #[case("5.")]
    #[case("1.2.3")]
    #[case("1234567")]
    #[case("0.1234567")]
    #[case("abc")]
    fn parse_decimal_rejects_everything_else(#[case] text: &str) {
        assert_eq!(parse_decimal(text), None);
    }

    #[rstest]
    #[case(0.0, "0")]
    #[case(2.0, "2")]
    #[case(0.5, "0.5")]
    #[case(1.25, "1.25")]
    #[case(60.0, "60")]
    #[case(86400.0, "86400")]
    #[case(2.0 / 3.0, "0.666667")]
    #[case(0.1 + 0.2, "0.3")]
    #[case(0.0001, "0.0001")]
    #[case(0.00001, "1e-05")]
    #[case(1_000_000.0, "1e+06")]
    #[case(999_999.5, "1e+06")]
    fn format_decimal_prints_like_percent_g(#[case] number: f64, #[case] text: &str) {
        assert_eq!(format_decimal(number), text);
    }

    #[rstest]
    #[case("2")]
    #[case("0.5")]
    #[case("0.1")]
    #[case("1.25")]
    fn seconds_read_back_what_they_print(#[case] text: &str) {
        let time = parse_seconds(text).unwrap();
        assert_eq!(format_seconds(time), text);
    }
}
