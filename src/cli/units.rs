//! Parsers for durations, transfer rates, and retry modes.

use std::time::Duration;

/// How many times a failed transfer is retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryMode {
    /// Keep retrying until no useful progress has been made for the recovery budget.
    Auto,
    /// Give up after this many consecutive attempts without useful progress.
    Count(u32),
    /// Retry retryable failures until cancelled.
    Forever,
}

pub fn parse_retry(input: &str) -> Result<RetryMode, String> {
    match input.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(RetryMode::Auto),
        "forever" | "inf" | "infinite" => Ok(RetryMode::Forever),
        other => other
            .parse::<u32>()
            .map(RetryMode::Count)
            .map_err(|_| format!("expected `auto`, `forever`, or a retry count, got `{input}`")),
    }
}

/// Parse `90`, `90s`, `500ms`, `5m`, `1h`, or combinations such as `1m30s`.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    let invalid =
        || format!("invalid duration `{input}`; use values such as 30s, 500ms, 5m, or 1h");
    if s.is_empty() {
        return Err(invalid());
    }
    if let Ok(secs) = s.parse::<f64>() {
        return seconds(secs).ok_or_else(invalid);
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(invalid)?;
        if num_len == 0 {
            return Err(invalid());
        }
        let value: f64 = rest[..num_len].parse().map_err(|_| invalid())?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(invalid()),
        };
        rest = &rest[unit_len..];
        total += seconds(value * scale).ok_or_else(invalid)?;
    }
    if total.is_zero() {
        return Err(format!("duration `{input}` must be greater than zero"));
    }
    Ok(total)
}

fn seconds(value: f64) -> Option<Duration> {
    (value.is_finite() && value > 0.0 && value < 1e9).then(|| Duration::from_secs_f64(value))
}

/// Parse a rate in bytes per second: `500K`, `2M`, `1.5MiB/s`, `800kB`.
///
/// `K`, `M`, `G` and the `KiB` forms are binary (1024-based); `kB`, `MB`, `GB` are decimal.
pub fn parse_rate(input: &str) -> Result<u64, String> {
    let s = input.trim();
    let s = s.strip_suffix("/s").unwrap_or(s);
    let invalid = || format!("invalid rate `{input}`; use values such as 500K, 2M, or 1.5MiB");
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let value: f64 = s[..split].parse().map_err(|_| invalid())?;
    let scale: f64 = match s[split..].trim() {
        "" | "B" => 1.0,
        "K" | "k" | "KiB" => 1024.0,
        "M" | "MiB" => 1024.0 * 1024.0,
        "G" | "GiB" => 1024.0 * 1024.0 * 1024.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        _ => return Err(invalid()),
    };
    let bytes = value * scale;
    if !bytes.is_finite() || bytes < 1.0 || bytes > u64::MAX as f64 {
        return Err(format!("rate `{input}` must be at least 1 byte per second"));
    }
    Ok(bytes as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("1m30s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1.5"), Ok(Duration::from_millis(1500)));
        assert!(parse_duration("").is_err());
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("-3s").is_err());
        assert!(parse_duration("5 minutes").is_err());
        assert!(parse_duration("s").is_err());
    }

    #[test]
    fn rates() {
        assert_eq!(parse_rate("500K"), Ok(512_000));
        assert_eq!(parse_rate("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_rate("1.5MiB/s"), Ok(1_572_864));
        assert_eq!(parse_rate("800kB"), Ok(800_000));
        assert_eq!(parse_rate("1000"), Ok(1000));
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("0").is_err());
        assert!(parse_rate("10X").is_err());
    }

    #[test]
    fn retry_modes() {
        assert_eq!(parse_retry("auto"), Ok(RetryMode::Auto));
        assert_eq!(parse_retry("Forever"), Ok(RetryMode::Forever));
        assert_eq!(parse_retry("0"), Ok(RetryMode::Count(0)));
        assert_eq!(parse_retry("12"), Ok(RetryMode::Count(12)));
        assert!(parse_retry("-1").is_err());
        assert!(parse_retry("lots").is_err());
    }
}
