//! HTTP header interpretation: ranges, validators, filenames, and retry guidance.

use std::time::{Duration, SystemTime};

use percent_encoding::percent_decode_str;
use reqwest::header::{self, HeaderMap};
use serde::{Deserialize, Serialize};

/// A parsed `Content-Range: bytes first-last/total` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRange {
    pub first: u64,
    pub last: u64,
    pub total: Option<u64>,
}

pub fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (first, last) = range.trim().split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse::<u64>().ok()?),
    };
    if last < first || total.is_some_and(|t| last >= t) {
        return None;
    }
    Some(ContentRange { first, last, total })
}

/// Validators that can bind a resumed request to the same representation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Validators {
    /// A strong entity tag, including its quotes.
    pub strong_etag: Option<String>,
    /// Any entity tag, strong or weak, for change detection.
    pub etag: Option<String>,
    /// `Last-Modified`, only when it qualifies as a strong validator.
    pub strong_last_modified: Option<String>,
}

impl Validators {
    pub fn from_headers(headers: &HeaderMap) -> Validators {
        let text = |name| {
            headers
                .get(name)
                .and_then(|v: &header::HeaderValue| v.to_str().ok())
                .map(|s| s.trim().to_string())
        };
        let etag = text(header::ETAG).filter(|e| !e.is_empty());
        let strong_etag = etag
            .clone()
            .filter(|e| !e.starts_with("W/") && e.starts_with('"'));
        // RFC 9110 §8.8.2.2 treats a modification date as strong when the response `Date` is
        // at least one second later. Require a wider 60 second margin to absorb clock skew and
        // multi-server deployments.
        let strong_last_modified = text(header::LAST_MODIFIED).filter(|lm| {
            let date = text(header::DATE).and_then(|d| httpdate::parse_http_date(&d).ok());
            match (httpdate::parse_http_date(lm).ok(), date) {
                (Some(modified), Some(date)) => date
                    .duration_since(modified)
                    .is_ok_and(|age| age >= Duration::from_secs(60)),
                _ => false,
            }
        });
        Validators {
            strong_etag,
            etag,
            strong_last_modified,
        }
    }

    /// The value to send in `If-Range`, if a strong validator exists. RFC 9110 §13.1.5 allows a
    /// date only when the representation has no entity tag at all.
    pub fn if_range(&self) -> Option<&str> {
        match (&self.strong_etag, &self.etag) {
            (Some(strong), _) => Some(strong),
            (None, Some(_weak)) => None,
            (None, None) => self.strong_last_modified.as_deref(),
        }
    }

    /// Whether `other` (from a later response) contradicts these validators.
    pub fn conflicts_with(&self, other: &Validators) -> bool {
        let differs =
            |a: &Option<String>, b: &Option<String>| matches!((a, b), (Some(a), Some(b)) if a != b);
        differs(&self.etag, &other.etag)
            || differs(&self.strong_last_modified, &other.strong_last_modified)
    }

    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.strong_last_modified.is_none()
    }

    /// Compare a later full response (`other`, `other_total` bytes) with this representation of
    /// `total` bytes.
    pub fn compare(
        &self,
        total: Option<u64>,
        other: &Validators,
        other_total: Option<u64>,
    ) -> Comparison {
        let length_differs = matches!((total, other_total), (Some(a), Some(b)) if a != b);
        if self.conflicts_with(other) {
            Comparison::Changed
        } else if !self.is_empty() && other.is_empty() && (length_differs || other_total.is_none())
        {
            // Some hosts send an error page with `200 OK`. Without the file's validators and at
            // another or unknown length, it says nothing about the file.
            Comparison::Unrelated
        } else if length_differs {
            Comparison::Changed
        } else {
            Comparison::Same
        }
    }
}

/// How a later response relates to the representation being downloaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    Same,
    Changed,
    /// Not a representation of the file at all, such as an error page.
    Unrelated,
}

/// Extract a filename from `Content-Disposition`, preferring the RFC 6266 `filename*` form.
pub fn content_disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for param in split_params(value).into_iter().skip(1) {
        let Some((key, val)) = param.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                // charset'language'percent-encoded
                let val = val.trim();
                let mut parts = val.splitn(3, '\'');
                let charset = parts.next()?;
                let _language = parts.next();
                if let Some(encoded) = parts.next()
                    && charset.eq_ignore_ascii_case("utf-8")
                {
                    extended = percent_decode_str(encoded)
                        .decode_utf8()
                        .ok()
                        .map(|s| s.into_owned());
                }
            }
            "filename" => plain = Some(unquote(val.trim())),
            _ => {}
        }
    }
    extended.or(plain).filter(|s| !s.is_empty())
}

fn split_params(value: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        match c {
            _ if escaped => {
                current.push(c);
                escaped = false;
            }
            '\\' if quoted => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ';' if !quoted => params.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    params.push(current);
    params
}

fn unquote(value: &str) -> String {
    match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => {
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }
        None => value.to_string(),
    }
}

/// Interpret `Retry-After` as delay seconds or an HTTP date.
pub fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn content_ranges() {
        assert_eq!(
            parse_content_range("bytes 100-199/1000"),
            Some(ContentRange {
                first: 100,
                last: 199,
                total: Some(1000)
            })
        );
        assert_eq!(
            parse_content_range("bytes 0-0/*"),
            Some(ContentRange {
                first: 0,
                last: 0,
                total: None
            })
        );
        assert_eq!(parse_content_range("bytes 5-4/10"), None);
        assert_eq!(parse_content_range("bytes 0-10/10"), None);
        assert_eq!(parse_content_range("bytes */1000"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    #[test]
    fn filenames() {
        assert_eq!(
            content_disposition_filename("attachment; filename=\"a b.zip\""),
            Some("a b.zip".into())
        );
        assert_eq!(
            content_disposition_filename("attachment; filename=plain.txt"),
            Some("plain.txt".into())
        );
        assert_eq!(
            content_disposition_filename(
                "attachment; filename=\"fallback.txt\"; filename*=UTF-8''%D9%88%D8%A7%DB%8C%D9%88.txt"
            ),
            Some("وایو.txt".into())
        );
        assert_eq!(
            content_disposition_filename("attachment; filename=\"semi;colon\\\".txt\""),
            Some("semi;colon\".txt".into())
        );
        assert_eq!(content_disposition_filename("inline"), None);
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn validators() {
        let v = Validators::from_headers(&headers(&[("etag", "\"abc\"")]));
        assert_eq!(v.if_range(), Some("\"abc\""));
        let weak = Validators::from_headers(&headers(&[("etag", "W/\"abc\"")]));
        assert_eq!(weak.if_range(), None, "weak ETags never validate If-Range");
        assert!(weak.etag.is_some());

        let old = Validators::from_headers(&headers(&[
            ("last-modified", "Mon, 01 Jan 2024 00:00:00 GMT"),
            ("date", "Tue, 02 Jan 2024 00:00:00 GMT"),
        ]));
        assert_eq!(old.if_range(), Some("Mon, 01 Jan 2024 00:00:00 GMT"));
        let fresh = Validators::from_headers(&headers(&[
            ("last-modified", "Tue, 02 Jan 2024 00:00:00 GMT"),
            ("date", "Tue, 02 Jan 2024 00:00:30 GMT"),
        ]));
        assert_eq!(fresh.if_range(), None);
        let undated = Validators::from_headers(&headers(&[(
            "last-modified",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        )]));
        assert_eq!(undated.if_range(), None);

        let changed = Validators::from_headers(&headers(&[("etag", "\"def\"")]));
        assert!(v.conflicts_with(&changed));
        assert!(!v.conflicts_with(&Validators::default()));
    }

    #[test]
    fn comparison_tells_an_error_page_from_a_new_version() {
        let v = Validators::from_headers(&headers(&[("etag", "\"abc\"")]));
        let none = Validators::default();
        let other = Validators::from_headers(&headers(&[("etag", "\"def\"")]));
        assert_eq!(v.compare(Some(100), &v, Some(100)), Comparison::Same);
        assert_eq!(v.compare(Some(100), &none, Some(100)), Comparison::Same);
        assert_eq!(v.compare(Some(100), &other, Some(100)), Comparison::Changed);
        assert_eq!(v.compare(Some(100), &v, Some(11)), Comparison::Changed);
        assert_eq!(v.compare(Some(100), &none, Some(11)), Comparison::Unrelated);
        assert_eq!(v.compare(Some(100), &none, None), Comparison::Unrelated);
        assert_eq!(v.compare(Some(100), &v, None), Comparison::Same);
        // Without validators of its own, only the length tells the versions apart.
        assert_eq!(
            none.compare(Some(100), &none, Some(11)),
            Comparison::Changed
        );
        assert_eq!(v.compare(None, &none, Some(11)), Comparison::Same);
    }

    #[test]
    fn retry_after_forms() {
        let now = httpdate::parse_http_date("Tue, 02 Jan 2024 00:00:00 GMT").unwrap();
        assert_eq!(
            retry_after(&headers(&[("retry-after", "120")]), now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            retry_after(
                &headers(&[("retry-after", "Tue, 02 Jan 2024 00:00:30 GMT")]),
                now
            ),
            Some(Duration::from_secs(30))
        );
        assert_eq!(retry_after(&headers(&[("retry-after", "soon")]), now), None);
    }
}
