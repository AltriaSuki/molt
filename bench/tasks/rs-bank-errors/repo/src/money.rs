//! Amounts of money. The ledger works in whole cents held in a `u64`; these
//! helpers convert to and from the `dollars.cents` form people write.

/// Formats an amount in cents as dollars and cents: `1250` is `"12.50"`,
/// `5` is `"0.05"`.
pub fn format_cents(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// Parses an amount written as whole dollars with an optional one or two
/// digit fraction (`"12"`, `"12.5"`, `"12.50"`) into cents.
///
/// Returns `None` for anything else: signs, empty parts (`"12."`, `".5"`),
/// more than two decimals, or an amount too large for a `u64` of cents.
pub fn parse_cents(s: &str) -> Option<u64> {
    let (dollars, fraction) = match s.split_once('.') {
        Some((dollars, fraction)) if !fraction.is_empty() => (dollars, fraction),
        Some(_) => return None,
        None => (s, ""),
    };
    if !is_digits(dollars) || fraction.len() > 2 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let dollars: u64 = dollars.parse().ok()?;
    let fraction = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction.parse::<u64>().ok()?,
    };
    dollars.checked_mul(100)?.checked_add(fraction)
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dollars_and_cents() {
        assert_eq!(format_cents(0), "0.00");
        assert_eq!(format_cents(5), "0.05");
        assert_eq!(format_cents(1250), "12.50");
        assert_eq!(format_cents(100_000), "1000.00");
        assert_eq!(format_cents(u64::MAX), "184467440737095516.15");
    }

    #[test]
    fn parses_dollars_and_cents() {
        assert_eq!(parse_cents("0"), Some(0));
        assert_eq!(parse_cents("12"), Some(1200));
        assert_eq!(parse_cents("12.5"), Some(1250));
        assert_eq!(parse_cents("12.50"), Some(1250));
        assert_eq!(parse_cents("0.05"), Some(5));
        assert_eq!(parse_cents("007.10"), Some(710));
        assert_eq!(parse_cents("184467440737095516.15"), Some(u64::MAX));
    }

    #[test]
    fn rejects_malformed_amounts() {
        for bad in [
            "",
            "-1",
            "+1",
            "1.",
            ".5",
            "1.234",
            "1,50",
            "1.5x",
            "abc",
            "1 2",
            "1.-5",
            "184467440737095516.16",
            "99999999999999999999",
        ] {
            assert_eq!(parse_cents(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn round_trips() {
        for cents in [0, 1, 99, 100, 101, 123_456, u64::MAX] {
            assert_eq!(parse_cents(&format_cents(cents)), Some(cents));
        }
    }
}
