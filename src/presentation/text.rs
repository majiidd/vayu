//! Text helpers: sanitizing, display-width truncation, unit formatting, and redaction.

use std::time::Duration;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use url::Url;

/// Bidirectional overrides and similar invisible formatting characters that can disguise text.
pub fn is_invisible_control(c: char) -> bool {
    matches!(c,
        '\u{200b}'..='\u{200f}'
        | '\u{2028}'..='\u{202e}'
        | '\u{2060}'..='\u{2069}'
        | '\u{feff}'
        | '\u{fff9}'..='\u{fffb}')
}

/// Replace control and invisible formatting characters so untrusted text cannot drive the
/// terminal (escape sequences, carriage returns, bidi tricks).
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || is_invisible_control(c) {
                '?'
            } else {
                c
            }
        })
        .collect()
}

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Truncate to at most `max` columns, ending with `ellipsis` when shortened.
pub fn truncate_end(text: &str, max: usize, ellipsis: &str) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let budget = max.saturating_sub(width(ellipsis));
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    if max >= width(ellipsis) {
        out.push_str(ellipsis);
    }
    out
}

/// Truncate to at most `max` columns by removing the middle, keeping the start and the end
/// (typically the file extension) visible.
pub fn truncate_middle(text: &str, max: usize, ellipsis: &str) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let ell = width(ellipsis);
    if max <= ell + 2 {
        return truncate_end(text, max, ellipsis);
    }
    let budget = max - ell;
    let tail_budget = budget / 2;
    let head_budget = budget - tail_budget;

    let mut head = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = char_width(c);
        if used + w > head_budget {
            break;
        }
        used += w;
        head.push(c);
    }
    let mut tail = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = char_width(c);
        if used + w > tail_budget {
            break;
        }
        used += w;
        tail.push(c);
    }
    let tail: String = tail.into_iter().rev().collect();
    format!("{head}{ellipsis}{tail}")
}

/// Pad on the right to exactly `cols` display columns (no truncation).
pub fn pad(text: &str, cols: usize) -> String {
    let w = width(text);
    if w >= cols {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(cols - w))
    }
}

/// Binary units with three significant digits: `512 B`, `1.50 KiB`, `640 MiB`, `1.00 GiB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1023.995 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let digits = if value < 9.995 {
        2
    } else if value < 99.95 {
        1
    } else {
        0
    };
    format!("{value:.digits$} {}", UNITS[unit])
}

pub fn rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec < 0.5 {
        return "0 B/s".into();
    }
    format!("{}/s", bytes(bytes_per_sec.round() as u64))
}

/// `08:12`, `1:02:03`, or `3d 04h` for very long durations.
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (days, hours, mins, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}:{mins:02}:{s:02}")
    } else {
        format!("{mins:02}:{s:02}")
    }
}

/// Coarse age for status lines: `2s ago`, `5m ago`, `3h ago`.
pub fn ago(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// Remove credentials, fragment, and query values from a URL for display and logs.
/// Query keys stay visible; signed links carry their secrets in the values.
pub fn redact_url(url: &Url) -> String {
    let mut shown = url.clone();
    let _ = shown.set_username("");
    let _ = shown.set_password(None);
    shown.set_fragment(None);
    if let Some(query) = url.query().filter(|q| !q.is_empty()) {
        let redacted: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((key, _)) => format!("{key}=REDACTED"),
                None => pair.to_string(),
            })
            .collect();
        shown.set_query(Some(&redacted.join("&")));
    }
    shown.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_terminal_controls() {
        assert_eq!(sanitize("ok\u{1b}[2Jname\r\n"), "ok?[2Jname??");
        assert_eq!(sanitize("evil\u{202e}gpj.exe"), "evil?gpj.exe");
        assert_eq!(sanitize("وایو.zip"), "وایو.zip");
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate_end("abcdef", 4, "…"), "abc…");
        assert_eq!(truncate_end("abc", 4, "…"), "abc");
        // Wide characters take two columns each.
        assert_eq!(truncate_end("日本語ファイル", 7, "…"), "日本語…");
        assert!(width(&truncate_end("日本語ファイル", 6, "…")) <= 6);
        let mid = truncate_middle("linux-image-amd64.iso", 13, "...");
        assert_eq!(mid, "linux...4.iso");
        let wide = truncate_middle("日本語ファイル名.zip", 10, "…");
        assert!(width(&wide) <= 10 && wide.ends_with(".zip"), "{wide}");
        assert_eq!(truncate_middle("short", 10, "…"), "short");
    }

    #[test]
    fn formats_units() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.50 KiB");
        assert_eq!(bytes(640 * 1024 * 1024), "640 MiB");
        assert_eq!(bytes(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(bytes(1024 * 1024 - 1), "1.00 MiB");
        assert_eq!(rate(800.0 * 1024.0), "800 KiB/s");
        assert_eq!(rate(0.0), "0 B/s");
        assert_eq!(duration(Duration::from_secs(492)), "08:12");
        assert_eq!(duration(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(
            duration(Duration::from_secs(3 * 86_400 + 4 * 3600)),
            "3d 04h"
        );
    }

    #[test]
    fn redacts_urls() {
        let url = Url::parse(
            "https://user:pw@cdn.example.com/f.zip?X-Amz-Signature=abc&token=xyz&flag#frag",
        )
        .unwrap();
        assert_eq!(
            redact_url(&url),
            "https://cdn.example.com/f.zip?X-Amz-Signature=REDACTED&token=REDACTED&flag"
        );
        let plain = Url::parse("https://example.com/f.zip").unwrap();
        assert_eq!(redact_url(&plain), "https://example.com/f.zip");
    }
}
