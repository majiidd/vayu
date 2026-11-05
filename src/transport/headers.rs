//! User-supplied request headers.

use std::path::Path;

use reqwest::header::{HeaderName, HeaderValue};

use crate::error::{Failure, describe_io};

/// A validated custom header. Sent only to the origin of the URL given on the command line.
#[derive(Clone, Debug)]
pub struct UserHeader {
    pub name: HeaderName,
    pub value: HeaderValue,
}

/// Headers the engine controls because they decide which bytes arrive and how they are encoded.
const RESERVED: &[&str] = &[
    "range",
    "if-range",
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
    "accept-encoding",
    "te",
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
    "keep-alive",
];

pub fn parse_header(raw: &str) -> Result<UserHeader, String> {
    let Some((name, value)) = raw.split_once(':') else {
        return Err(format!(
            "header `{}` must look like `Name: value`",
            first_word(raw)
        ));
    };
    let name = name.trim();
    let value = value.trim();
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("invalid header name `{name}`"))?;
    if RESERVED.contains(&header_name.as_str()) {
        return Err(format!(
            "header `{name}` is managed by vayu to keep downloads and resume correct"
        ));
    }
    let mut header_value = HeaderValue::from_str(value)
        .map_err(|_| format!("header `{name}` has an invalid value"))?;
    if is_sensitive(&header_name) {
        header_value.set_sensitive(true);
    }
    Ok(UserHeader {
        name: header_name,
        value: header_value,
    })
}

/// Read `Name: value` lines. Blank lines and lines starting with `#` are ignored.
pub fn read_header_file(path: &Path) -> Result<Vec<UserHeader>, Failure> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        Failure::usage(format!(
            "cannot read header file {}: {}",
            path.display(),
            describe_io(&err)
        ))
    })?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('#')
        })
        .map(|(index, line)| {
            parse_header(line.trim_end_matches('\r'))
                .map_err(|err| Failure::usage(format!("{}:{}: {err}", path.display(), index + 1)))
        })
        .collect()
}

/// Headers whose values must never appear in output.
pub fn is_sensitive(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(name, "authorization" | "proxy-authorization" | "cookie")
        || [
            "token",
            "secret",
            "key",
            "auth",
            "session",
            "password",
            "signature",
        ]
        .iter()
        .any(|part| name.contains(part))
}

fn first_word(raw: &str) -> String {
    raw.split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .take(40)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_trims() {
        let h = parse_header("X-Test:  hello world ").unwrap();
        assert_eq!(h.name.as_str(), "x-test");
        assert_eq!(h.value, "hello world");
        assert!(!h.value.is_sensitive());
    }

    #[test]
    fn marks_secrets_sensitive() {
        assert!(
            parse_header("Authorization: Bearer x")
                .unwrap()
                .value
                .is_sensitive()
        );
        assert!(parse_header("X-Api-Key: x").unwrap().value.is_sensitive());
    }

    #[test]
    fn rejects_reserved_and_malformed() {
        assert!(parse_header("Range: bytes=0-").is_err());
        assert!(parse_header("accept-encoding: gzip").is_err());
        assert!(parse_header("NoColon").is_err());
        assert!(parse_header("Bad Name: x").is_err());
        assert!(parse_header("X-Test: a\u{7}b").is_err());
    }

    #[test]
    fn reads_header_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("headers.txt");
        std::fs::write(&path, "# comment\n\nUser-Agent: test\r\nX-A: 1\n").unwrap();
        let headers = read_header_file(&path).unwrap();
        assert_eq!(headers.len(), 2);
        std::fs::write(&path, "X-A: 1\nRange: bytes=1-\n").unwrap();
        let err = read_header_file(&path).unwrap_err();
        assert!(err.message.contains(":2:"), "{}", err.message);
    }
}
