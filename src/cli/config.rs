//! Optional TOML configuration in the user's configuration directory.
//!
//! Location: `$VAYU_CONFIG` if set; otherwise `$XDG_CONFIG_HOME/vayu/config.toml`, falling back
//! to `~/.config/vayu/config.toml` on Linux and `~/Library/Application Support/vayu/config.toml`
//! on macOS.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer};

use super::units::{self, RetryMode};
use super::{ColorChoice, Glyphs, HttpVersion, Theme, UiMode};
use crate::error::{Failure, describe_io};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub min_connections: Option<u16>,
    pub max_connections: Option<u16>,
    /// A proxy URL, or `system` / `none`.
    pub proxy: Option<String>,
    #[serde(default, deserialize_with = "retry")]
    pub retry: Option<RetryMode>,
    #[serde(default, deserialize_with = "duration")]
    pub connect_timeout: Option<Duration>,
    #[serde(default, deserialize_with = "duration")]
    pub stall_timeout: Option<Duration>,
    #[serde(default, deserialize_with = "rate")]
    pub limit_rate: Option<u64>,
    #[serde(default, deserialize_with = "duration")]
    pub checkpoint_interval: Option<Duration>,
    pub http_version: Option<HttpVersion>,
    pub ca_cert: Option<Vec<PathBuf>>,
    pub ui: Option<UiMode>,
    pub color: Option<ColorChoice>,
    pub theme: Option<Theme>,
    pub glyphs: Option<Glyphs>,
    pub animation: Option<bool>,
}

pub fn path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("VAYU_CONFIG").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("vayu/config.toml"));
    }
    let home = PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?);
    if cfg!(target_vendor = "apple") {
        Some(home.join("Library/Application Support/vayu/config.toml"))
    } else {
        Some(home.join(".config/vayu/config.toml"))
    }
}

pub fn load() -> Result<Config, Failure> {
    let Some(path) = path() else {
        return Ok(Config::default());
    };
    let explicit = std::env::var_os("VAYU_CONFIG").is_some_and(|v| !v.is_empty());
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, &path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !explicit => {
            Ok(Config::default())
        }
        Err(err) => Err(Failure::usage(format!(
            "cannot read configuration {}: {}",
            path.display(),
            describe_io(&err)
        ))),
    }
}

fn parse(text: &str, path: &Path) -> Result<Config, Failure> {
    toml::from_str(text).map_err(|err| {
        Failure::usage(format!(
            "invalid configuration {}: {}",
            path.display(),
            err.message()
        ))
    })
}

fn retry<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RetryMode>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Count(u32),
        Text(String),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        None => None,
        Some(Raw::Count(n)) => Some(RetryMode::Count(n)),
        Some(Raw::Text(s)) => Some(units::parse_retry(&s).map_err(serde::de::Error::custom)?),
    })
}

fn duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|s| units::parse_duration(&s).map_err(serde::de::Error::custom))
        .transpose()
}

fn rate<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|s| units::parse_rate(&s).map_err(serde::de::Error::custom))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let config = parse(
            r#"
            min-connections = 2
            max-connections = 4
            proxy = "socks5h://127.0.0.1:1080"
            retry = "forever"
            connect-timeout = "10s"
            stall-timeout = "2m"
            limit-rate = "1M"
            checkpoint-interval = "500ms"
            http-version = "1.1"
            ui = "compact"
            color = "always"
            theme = "light"
            glyphs = "ascii"
            animation = false
            "#,
            Path::new("test.toml"),
        )
        .unwrap();
        assert_eq!(config.min_connections, Some(2));
        assert_eq!(config.max_connections, Some(4));
        assert_eq!(config.retry, Some(RetryMode::Forever));
        assert_eq!(config.stall_timeout, Some(Duration::from_secs(120)));
        assert_eq!(config.limit_rate, Some(1024 * 1024));
        assert_eq!(config.checkpoint_interval, Some(Duration::from_millis(500)));
        assert_eq!(config.http_version, Some(HttpVersion::Http11));
        assert_eq!(config.color, Some(ColorChoice::Always));
        assert_eq!(config.animation, Some(false));
    }

    #[test]
    fn numeric_retry_and_unknown_keys() {
        let config = parse("retry = 5", Path::new("t")).unwrap();
        assert_eq!(config.retry, Some(RetryMode::Count(5)));
        assert!(parse("max-conections = 4", Path::new("t")).is_err());
        assert!(parse("stall-timeout = \"soon\"", Path::new("t")).is_err());
    }
}
