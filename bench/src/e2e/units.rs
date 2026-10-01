//! Rates, durations and latencies as people write and read them: `100k`, `1M`, `30s`,
//! `250us` on the command line; `51.3 µs`, `1.25 ms`, `6,500,000` in reports.
//!
//! **Contract.** Parsing accepts a number (decimals allowed) and an optional suffix, and
//! rounds to whole units; anything else is an error that quotes the input. Formatting never
//! loses the unit, prints "no data" for a histogram with nothing in it and "∞" for a command
//! that was never served (15.3, 15.4).
//!
//! **Complexity.** O(length of the text).

/// Nanoseconds in a second.
pub const SECOND_NS: u64 = 1_000_000_000;

/// A rate in commands a second: `2500`, `20k`, `1.5M`.
pub fn parse_rate(text: &str) -> Result<u64, String> {
    let (number, multiplier) = match text.trim().char_indices().last() {
        Some((i, 'k' | 'K')) => (&text[..i], 1e3),
        Some((i, 'M' | 'm')) => (&text[..i], 1e6),
        _ => (text, 1.0),
    };
    let value: f64 = number.trim().parse().map_err(|_| format!("not a rate: {text:?} (e.g. 20k, 1M)"))?;
    if !(value.is_finite() && value > 0.0) {
        return Err(format!("a rate must be above zero: {text:?}"));
    }
    Ok((value * multiplier).round() as u64)
}

/// A comma-separated list of rates: `5k,10k,20k`.
pub fn parse_rates(text: &str) -> Result<Vec<u64>, String> {
    text.split(',').map(parse_rate).collect()
}

/// A duration in nanoseconds: `0`, `250us`, `1ms`, `1.5s`, `2m`.
pub fn parse_duration(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic() || c == 'µ').unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let per_unit = match unit {
        "ns" => 1.0,
        "us" | "µs" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        "m" | "min" => 60e9,
        "" if number.trim() == "0" => 0.0,
        _ => return Err(format!("not a duration: {text:?} (e.g. 250us, 1ms, 30s)")),
    };
    let value: f64 = number.trim().parse().map_err(|_| format!("not a duration: {text:?}"))?;
    if !(value.is_finite() && value >= 0.0) {
        return Err(format!("a duration can't be negative: {text:?}"));
    }
    Ok((value * per_unit).round() as u64)
}

/// A size in bytes: `4096`, `1M` (MiB), `1G` (GiB).
pub fn parse_bytes(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let (number, multiplier) = match text.char_indices().last() {
        Some((i, 'K' | 'k')) => (&text[..i], 1u64 << 10),
        Some((i, 'M')) => (&text[..i], 1 << 20),
        Some((i, 'G')) => (&text[..i], 1 << 30),
        _ => (text, 1),
    };
    let value: u64 = number.parse().map_err(|_| format!("not a size: {text:?} (e.g. 4096, 1M, 1G)"))?;
    value.checked_mul(multiplier).ok_or_else(|| format!("too large: {text:?}"))
}

/// A rate for names and short cells: `5k`, `100k`, `1M`, `2500`.
pub fn short_rate(rate: u64) -> String {
    if rate >= 1_000_000 && rate.is_multiple_of(100_000) {
        trim_decimal(rate as f64 / 1e6, "M")
    } else if rate >= 1_000 && rate.is_multiple_of(100) {
        trim_decimal(rate as f64 / 1e3, "k")
    } else {
        rate.to_string()
    }
}

/// `2.5` with suffix: `2.5k`; `20.0`: `20k`.
fn trim_decimal(value: f64, suffix: &str) -> String {
    let text = format!("{value:.1}");
    format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
}

/// A duration for names: `0`, `250us`, `1ms`, `30s`.
pub fn short_duration(ns: u64) -> String {
    match ns {
        0 => "0".to_string(),
        ns if ns.is_multiple_of(SECOND_NS) => format!("{}s", ns / SECOND_NS),
        ns if ns.is_multiple_of(1_000_000) => format!("{}ms", ns / 1_000_000),
        ns if ns.is_multiple_of(1_000) => format!("{}us", ns / 1_000),
        ns => format!("{ns}ns"),
    }
}

/// A count with thousands separators: `6,500,000`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A signed amount with thousands separators: `-1,234`.
pub fn signed_count(n: i128) -> String {
    let magnitude = count(u64::try_from(n.unsigned_abs()).unwrap_or(u64::MAX));
    if n < 0 { format!("-{magnitude}") } else { magnitude }
}

/// A latency: "no data", "∞", `812 ns`, `51.3 µs`, `1.25 ms`, `2.10 s`, with three
/// significant digits.
pub fn latency(ns: Option<u64>) -> String {
    match ns {
        None => "no data".to_string(),
        Some(u64::MAX) => "∞".to_string(),
        Some(ns) if ns < 1_000 => format!("{ns} ns"),
        Some(ns) if ns < 1_000_000 => format!("{} µs", three_digits(ns as f64 / 1e3)),
        Some(ns) if ns < SECOND_NS => format!("{} ms", three_digits(ns as f64 / 1e6)),
        Some(ns) => format!("{} s", three_digits(ns as f64 / 1e9)),
    }
}

/// `1.2345` as `1.23`, `51.34` as `51.3`, `812.4` as `812`.
fn three_digits(value: f64) -> String {
    if value < 10.0 {
        format!("{value:.2}")
    } else if value < 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    }
}

/// A share in parts per million, as a percentage: `0.026%`, `12.5%`.
pub fn percent_ppm(ppm: u64) -> String {
    let percent = ppm as f64 / 10_000.0;
    if percent != 0.0 && percent < 0.1 { format!("{percent:.3}%") } else { format!("{percent:.1}%") }
}

/// `part / whole` in parts per million; 0 when `whole` is 0.
pub fn ppm(part: u64, whole: u64) -> u64 {
    if whole == 0 { 0 } else { (u128::from(part) * 1_000_000 / u128::from(whole)) as u64 }
}

/// Micro-dollars as dollars: `$1,000`, `-$553.87`.
pub fn dollars(micros: i128) -> String {
    let cents = micros / 10_000;
    let sign = if cents < 0 { "-" } else { "" };
    let whole = count(u64::try_from((cents / 100).unsigned_abs()).unwrap_or(u64::MAX));
    let fraction = (cents % 100).unsigned_abs();
    if fraction == 0 { format!("{sign}${whole}") } else { format!("{sign}${whole}.{fraction:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_parse_with_suffixes_and_print_short() {
        assert_eq!(parse_rate("2500"), Ok(2_500));
        assert_eq!(parse_rate("20k"), Ok(20_000));
        assert_eq!(parse_rate("1.5M"), Ok(1_500_000));
        assert_eq!(parse_rates("5k,10k,1M"), Ok(vec![5_000, 10_000, 1_000_000]));
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("0").is_err());
        for rate in [5_000, 100_000, 1_000_000, 2_500, 1_500_000, 999] {
            assert_eq!(parse_rate(&short_rate(rate)), Ok(rate), "{rate}");
        }
        assert_eq!(short_rate(100_000), "100k");
        assert_eq!(short_rate(2_500), "2.5k");
    }

    #[test]
    fn durations_parse_with_units_and_print_short() {
        assert_eq!(parse_duration("0"), Ok(0));
        assert_eq!(parse_duration("250us"), Ok(250_000));
        assert_eq!(parse_duration("1ms"), Ok(1_000_000));
        assert_eq!(parse_duration("1.5s"), Ok(1_500_000_000));
        assert_eq!(parse_duration("2m"), Ok(120 * SECOND_NS));
        assert!(parse_duration("5").is_err(), "a unit is needed");
        assert!(parse_duration("-1s").is_err());
        for ns in [0, 250_000, 1_000_000, 30 * SECOND_NS, 17] {
            assert_eq!(parse_duration(&short_duration(ns)), Ok(ns));
        }
        assert_eq!(parse_bytes("1M"), Ok(1 << 20));
        assert_eq!(parse_bytes("4096"), Ok(4_096));
    }

    #[test]
    fn numbers_print_for_people() {
        assert_eq!(count(6_500_000), "6,500,000");
        assert_eq!(count(999), "999");
        assert_eq!(signed_count(-1_234), "-1,234");
        assert_eq!(latency(None), "no data");
        assert_eq!(latency(Some(u64::MAX)), "∞");
        assert_eq!(latency(Some(812)), "812 ns");
        assert_eq!(latency(Some(51_340)), "51.3 µs");
        assert_eq!(latency(Some(1_250_000)), "1.25 ms");
        assert_eq!(percent_ppm(260), "0.026%");
        assert_eq!(percent_ppm(125_000), "12.5%");
        assert_eq!(ppm(1, 4), 250_000);
        assert_eq!(ppm(1, 0), 0);
        assert_eq!(dollars(1_000_000_000), "$1,000");
        assert_eq!(dollars(-553_870_000), "-$553.87");
    }
}
