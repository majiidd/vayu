//! Import of Netscape/Mozilla `cookies.txt` files into a domain-scoped cookie jar.
//!
//! Each line holds seven tab-separated fields: domain, include-subdomains flag, path, secure flag,
//! expiry (Unix seconds, 0 for a session cookie), name, and value. Lines starting with `#` are
//! comments, except the `#HttpOnly_` prefix that browsers use to mark HTTP-only cookies.
//! The jar applies domain, path, secure, and expiry rules, so cookies are never sent to unrelated
//! origins, including after redirects.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::cookie::Jar;
use url::Url;

use crate::error::{Failure, describe_io};

#[derive(Debug, PartialEq, Eq)]
pub struct CookieLine {
    /// `Set-Cookie`-style string for the jar.
    pub set_cookie: String,
    /// URL the cookie is attributed to.
    pub origin: Url,
}

pub fn load(path: &Path, jar: &Jar) -> Result<usize, Failure> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        Failure::usage(format!(
            "cannot read cookie file {}: {}",
            path.display(),
            describe_io(&err)
        ))
    })?;
    let cookies = parse(&text, SystemTime::now())
        .map_err(|(line, err)| Failure::usage(format!("{}:{line}: {err}", path.display())))?;
    for cookie in &cookies {
        jar.add_cookie_str(&cookie.set_cookie, &cookie.origin);
    }
    Ok(cookies.len())
}

/// Parse a cookie file, skipping expired cookies. Errors carry a 1-based line number.
pub fn parse(text: &str, now: SystemTime) -> Result<Vec<CookieLine>, (usize, String)> {
    let mut cookies = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let (line, http_only) = match line.strip_prefix("#HttpOnly_") {
            Some(rest) => (rest, true),
            None => (line, false),
        };
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 7 {
            return Err((
                index + 1,
                "expected 7 tab-separated fields (Netscape cookies.txt format)".into(),
            ));
        }
        let [domain, subdomains, path, secure, expiry, name, value] = fields[..] else {
            unreachable!()
        };
        let host = domain.trim_start_matches('.');
        if host.is_empty() || name.is_empty() {
            return Err((index + 1, "cookie domain and name must not be empty".into()));
        }
        let include_subdomains = flag(subdomains).ok_or((index + 1, bad_flag(subdomains)))?;
        let secure = flag(secure).ok_or((index + 1, bad_flag(secure)))?;
        let expiry: i64 = expiry
            .trim()
            .parse()
            .map_err(|_| (index + 1, format!("invalid expiry `{expiry}`")))?;
        if expiry > 0 && UNIX_EPOCH + Duration::from_secs(expiry as u64) <= now {
            continue;
        }

        let path = if path.starts_with('/') { path } else { "/" };
        let mut set_cookie = format!("{name}={value}; Path={path}");
        if include_subdomains {
            set_cookie.push_str("; Domain=");
            set_cookie.push_str(host);
        }
        if secure {
            set_cookie.push_str("; Secure");
        }
        if http_only {
            set_cookie.push_str("; HttpOnly");
        }
        if expiry > 0 {
            let at = UNIX_EPOCH + Duration::from_secs(expiry as u64);
            set_cookie.push_str("; Expires=");
            set_cookie.push_str(&httpdate::fmt_http_date(at));
        }
        let scheme = if secure { "https" } else { "http" };
        let origin = Url::parse(&format!("{scheme}://{host}{path}"))
            .map_err(|_| (index + 1, format!("invalid cookie domain `{domain}`")))?;
        cookies.push(CookieLine { set_cookie, origin });
    }
    Ok(cookies)
}

fn flag(value: &str) -> Option<bool> {
    match value.trim() {
        v if v.eq_ignore_ascii_case("true") => Some(true),
        v if v.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn bad_flag(value: &str) -> String {
    format!("expected TRUE or FALSE, got `{value}`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore;

    const NOW: u64 = 1_800_000_000;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(NOW)
    }

    #[test]
    fn parses_netscape_format() {
        let text = "# Netscape HTTP Cookie File\n\
            .example.com\tTRUE\t/\tTRUE\t0\tsid\tabc\n\
            #HttpOnly_files.example.org\tFALSE\t/dl\tFALSE\t1900000000\ttok\txyz\n\
            old.example.com\tFALSE\t/\tFALSE\t1000\tgone\t1\n";
        let cookies = parse(text, now()).unwrap();
        assert_eq!(cookies.len(), 2, "expired cookie is skipped");
        assert_eq!(
            cookies[0].set_cookie,
            "sid=abc; Path=/; Domain=example.com; Secure"
        );
        assert_eq!(cookies[0].origin.as_str(), "https://example.com/");
        assert!(
            cookies[1]
                .set_cookie
                .starts_with("tok=xyz; Path=/dl; HttpOnly; Expires=")
        );
    }

    #[test]
    fn reports_line_numbers() {
        let err = parse("# c\nexample.com\tTRUE\t/\n", now()).unwrap_err();
        assert_eq!(err.0, 2);
        let err = parse("example.com\tyes\t/\tFALSE\t0\ta\tb\n", now()).unwrap_err();
        assert!(err.1.contains("TRUE or FALSE"));
    }

    #[test]
    fn jar_scopes_cookies() {
        let text = ".example.com\tTRUE\t/\tTRUE\t0\tsid\tabc\n\
                    host.example.net\tFALSE\t/dl\tFALSE\t0\thost\t1\n";
        let jar = Jar::default();
        for cookie in parse(text, now()).unwrap() {
            jar.add_cookie_str(&cookie.set_cookie, &cookie.origin);
        }
        let get = |url: &str| {
            jar.cookies(&Url::parse(url).unwrap())
                .map(|v| v.to_str().unwrap().to_string())
        };
        assert_eq!(get("https://cdn.example.com/a").as_deref(), Some("sid=abc"));
        assert_eq!(
            get("http://cdn.example.com/a"),
            None,
            "secure cookie over http"
        );
        assert_eq!(get("https://other.org/"), None);
        assert_eq!(
            get("http://host.example.net/dl/f").as_deref(),
            Some("host=1")
        );
        assert_eq!(
            get("http://sub.host.example.net/dl/f"),
            None,
            "host-only cookie"
        );
        assert_eq!(get("http://host.example.net/other"), None, "path rule");
    }
}
