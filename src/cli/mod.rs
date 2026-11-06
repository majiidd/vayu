//! Command-line parsing, configuration, and resolved settings.

mod config;
pub mod units;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use serde::Deserialize;
use url::Url;

use crate::error::Failure;
use crate::integrity::Checksum;
use crate::transport::headers::{self, UserHeader};
pub use units::RetryMode;

const EXIT_CODES: &str = "\
Exit codes:
  0    success
  1    internal error
  2    invalid arguments or configuration
  3    network failure or retry budget exhausted
  4    server, proxy, authentication, or TLS failure
  5    local storage failure
  6    integrity failure (checksum mismatch)
  7    conflict: existing files, a changed source, or progress that cannot be resumed safely
  129  hangup (SIGHUP)    130  interrupted (SIGINT)
  141  JSON event pipe closed    143  terminated (SIGTERM)";

/// `--version`: the crate name lets installers tell this `vayu` from unrelated programs.
const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (vayu)");

#[derive(Parser, Debug)]
#[command(
    name = "vayu",
    version,
    long_version = LONG_VERSION,
    about = "Download files reliably over slow, unstable, and high-latency networks.",
    after_help = EXIT_CODES
)]
pub struct Args {
    /// HTTP or HTTPS URL to download
    pub url: String,

    /// Destination file, or an existing directory to save into
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Lower bound on simultaneous connections when the download is segmented [default: 1]
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=64))]
    pub min_connections: Option<u16>,

    /// Upper bound on simultaneous connections [default: 15, or the minimum if higher]
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=64))]
    pub max_connections: Option<u16>,

    /// Proxy URL: http://, https://, socks5://, or socks5h:// (DNS at the proxy)
    #[arg(long, value_name = "URL", conflicts_with = "no_proxy")]
    pub proxy: Option<String>,

    /// Bypass all proxies, including proxy environment variables and system settings
    #[arg(long)]
    pub no_proxy: bool,

    /// Add a request header (repeatable)
    #[arg(short = 'H', long = "header", value_name = "NAME:VALUE")]
    pub headers: Vec<String>,

    /// Read request headers from a file, one `Name: value` per line
    #[arg(long, value_name = "PATH")]
    pub header_file: Vec<PathBuf>,

    /// Import cookies from a Netscape/Mozilla cookies.txt file
    #[arg(long, value_name = "PATH")]
    pub cookie_file: Option<PathBuf>,

    /// Also trust the PEM certificates in this file (repeatable)
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Vec<PathBuf>,

    /// Require the final file to match this digest
    #[arg(long, value_name = "sha256:HEX", value_parser = Checksum::parse)]
    pub checksum: Option<Checksum>,

    /// Discard an incomplete download of the same destination and start again
    #[arg(long, conflicts_with = "resume_from")]
    pub restart: bool,

    /// Continue the download saved in this state file (NAME.vayu), e.g. with a fresh link
    #[arg(long, value_name = "PATH")]
    pub resume_from: Option<PathBuf>,

    /// Retry policy: auto, forever, or a count of consecutive failures [default: auto]
    #[arg(long, value_name = "MODE", value_parser = units::parse_retry)]
    pub retry: Option<RetryMode>,

    /// Limit for establishing a connection [default: 30s]
    #[arg(long, value_name = "DURATION", value_parser = units::parse_duration)]
    pub connect_timeout: Option<Duration>,

    /// Give up on a connection after this long without receiving data [default: 60s]
    #[arg(long, value_name = "DURATION", value_parser = units::parse_duration)]
    pub stall_timeout: Option<Duration>,

    /// Bandwidth ceiling in bytes per second, e.g. 500K or 2M
    #[arg(long, value_name = "RATE", value_parser = units::parse_rate)]
    pub limit_rate: Option<u64>,

    /// HTTP protocol version
    #[arg(long, value_name = "VERSION")]
    pub http_version: Option<HttpVersion>,

    /// Progress presentation
    #[arg(long, value_name = "MODE")]
    pub ui: Option<UiMode>,

    /// When to use color
    #[arg(long, value_name = "WHEN")]
    pub color: Option<ColorChoice>,

    /// Color palette
    #[arg(long, value_name = "THEME")]
    pub theme: Option<Theme>,

    /// Character set for progress symbols
    #[arg(long, value_name = "SET")]
    pub glyphs: Option<Glyphs>,

    /// Disable spinners and animated decoration
    #[arg(long)]
    pub no_animation: bool,

    /// Print only actionable failures
    #[arg(short, long, conflicts_with = "json")]
    pub quiet: bool,

    /// Emit versioned JSON Lines events on stdout
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HttpVersion {
    Auto,
    #[value(name = "1.1")]
    #[serde(rename = "1.1")]
    Http11,
    #[value(name = "2")]
    #[serde(rename = "2")]
    Http2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiMode {
    Auto,
    Compact,
    Detailed,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Auto,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Glyphs {
    Auto,
    Unicode,
    Ascii,
}

/// Proxy selection after applying precedence rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxySetting {
    /// Proxy environment variables and system settings.
    System,
    /// Connect directly.
    Direct,
    /// An explicit proxy URL from the command line or configuration.
    Explicit(Url),
}

/// Presentation preferences before terminal capability detection.
#[derive(Clone, Debug)]
pub struct Display {
    pub ui: UiMode,
    /// The explicit color choice from the command line or configuration, if any.
    pub color: Option<ColorChoice>,
    pub theme: Theme,
    pub glyphs: Glyphs,
    pub animation: bool,
    pub quiet: bool,
    pub json: bool,
}

/// Fully resolved settings for one download.
#[derive(Clone, Debug)]
pub struct Settings {
    pub url: Url,
    pub output: Option<PathBuf>,
    /// Connections kept even when more do not raise throughput, once the transfer is segmented.
    pub min_connections: u16,
    pub max_connections: u16,
    pub proxy: ProxySetting,
    pub headers: Vec<UserHeader>,
    pub cookie_file: Option<PathBuf>,
    pub ca_certs: Vec<PathBuf>,
    pub checksum: Option<Checksum>,
    pub restart: bool,
    pub resume_from: Option<PathBuf>,
    /// Time between durable checkpoints while data is arriving.
    pub checkpoint_interval: Duration,
    pub retry: RetryMode,
    pub connect_timeout: Duration,
    pub stall_timeout: Duration,
    pub limit_rate: Option<u64>,
    pub http_version: HttpVersion,
    pub display: Display,
}

pub const DEFAULT_MIN_CONNECTIONS: u16 = 1;
pub const DEFAULT_MAX_CONNECTIONS: u16 = 15;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
/// Each checkpoint synchronizes the disk; shorter intervals would throttle the transfer.
const MIN_CHECKPOINT_INTERVAL: Duration = Duration::from_millis(10);

impl Settings {
    /// Apply precedence: command line, then the user configuration file, then defaults.
    pub fn resolve(args: Args) -> Result<Settings, Failure> {
        let config = config::load()?;

        let url = parse_download_url(&args.url)?;

        let proxy = if args.no_proxy {
            ProxySetting::Direct
        } else if let Some(proxy) = args.proxy.as_deref() {
            ProxySetting::Explicit(parse_proxy_url(proxy, "--proxy")?)
        } else if let Some(proxy) = config.proxy.as_deref() {
            match proxy {
                "none" | "direct" => ProxySetting::Direct,
                "system" => ProxySetting::System,
                _ => ProxySetting::Explicit(parse_proxy_url(proxy, "the configuration `proxy`")?),
            }
        } else {
            ProxySetting::System
        };

        let mut user_headers = Vec::new();
        for path in &args.header_file {
            user_headers.extend(headers::read_header_file(path)?);
        }
        for raw in &args.headers {
            user_headers.push(headers::parse_header(raw).map_err(Failure::usage)?);
        }

        let mut ca_certs = config.ca_cert.unwrap_or_default();
        ca_certs.extend(args.ca_cert);

        let (min_connections, max_connections) = connections(
            args.min_connections,
            config.min_connections,
            args.max_connections.or(config.max_connections),
        )?;

        let checkpoint_interval = config
            .checkpoint_interval
            .unwrap_or(DEFAULT_CHECKPOINT_INTERVAL);
        if checkpoint_interval < MIN_CHECKPOINT_INTERVAL {
            return Err(Failure::usage("checkpoint-interval must be at least 10ms"));
        }

        if let Some(output) = &args.output
            && output.as_os_str() == "-"
        {
            return Err(Failure::usage(
                "writing the download to stdout is not supported; choose a file with -o",
            ));
        }

        Ok(Settings {
            url,
            output: args.output,
            min_connections,
            max_connections,
            proxy,
            headers: user_headers,
            cookie_file: args.cookie_file,
            ca_certs,
            checksum: args.checksum,
            restart: args.restart,
            resume_from: args.resume_from,
            checkpoint_interval,
            retry: args.retry.or(config.retry).unwrap_or(RetryMode::Auto),
            connect_timeout: args
                .connect_timeout
                .or(config.connect_timeout)
                .unwrap_or(DEFAULT_CONNECT_TIMEOUT),
            stall_timeout: args
                .stall_timeout
                .or(config.stall_timeout)
                .unwrap_or(DEFAULT_STALL_TIMEOUT),
            limit_rate: args.limit_rate.or(config.limit_rate),
            http_version: args
                .http_version
                .or(config.http_version)
                .unwrap_or(HttpVersion::Auto),
            display: Display {
                ui: args.ui.or(config.ui).unwrap_or(UiMode::Auto),
                color: args.color.or(config.color),
                theme: args.theme.or(config.theme).unwrap_or(Theme::Auto),
                glyphs: args.glyphs.or(config.glyphs).unwrap_or(Glyphs::Auto),
                animation: !args.no_animation && config.animation.unwrap_or(true),
                quiet: args.quiet,
                json: args.json,
            },
        })
    }
}

/// Resolve the connection bounds from the command line minimum, the configured minimum, and
/// the maximum from either.
fn connections(
    cli_min: Option<u16>,
    config_min: Option<u16>,
    max: Option<u16>,
) -> Result<(u16, u16), Failure> {
    let min = cli_min.or(config_min).unwrap_or(DEFAULT_MIN_CONNECTIONS);
    if !(1..=64).contains(&min) {
        return Err(Failure::usage("min_connections must be between 1 and 64"));
    }
    // Without a maximum, a higher minimum raises it.
    let max = max.unwrap_or(DEFAULT_MAX_CONNECTIONS.max(min));
    if !(1..=64).contains(&max) {
        return Err(Failure::usage("max_connections must be between 1 and 64"));
    }
    // A configured minimum yields to a lower maximum, e.g. `--max-connections 1`.
    if cli_min.is_some() && min > max {
        return Err(Failure::usage(format!(
            "--min-connections {min} is above max_connections ({max})"
        ))
        .with_hint("raise --max-connections or lower --min-connections"));
    }
    Ok((min.min(max), max))
}

fn parse_download_url(input: &str) -> Result<Url, Failure> {
    let url = Url::parse(input.trim()).map_err(|err| {
        Failure::usage(format!("invalid URL: {err}"))
            .with_hint("quote the URL in your shell, e.g. vayu \"https://example.com/file.zip\"")
    })?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Failure::usage(format!(
                "unsupported URL scheme `{other}`; vayu downloads http:// and https:// URLs"
            )));
        }
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(Failure::usage("the URL has no host"));
    }
    Ok(url)
}

fn parse_proxy_url(input: &str, source: &str) -> Result<Url, Failure> {
    let url = Url::parse(input.trim())
        .map_err(|err| Failure::usage(format!("invalid proxy URL in {source}: {err}")))?;
    match url.scheme() {
        "http" | "https" | "socks5" | "socks5h" => Ok(url),
        other => Err(Failure::usage(format!(
            "unsupported proxy scheme `{other}` in {source}; use http, https, socks5, or socks5h"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_urls() {
        assert!(parse_download_url("https://example.com/file.zip").is_ok());
        assert!(parse_download_url("http://127.0.0.1:8080/x").is_ok());
        assert!(parse_download_url("ftp://example.com/file").is_err());
        assert!(parse_download_url("example.com/file").is_err());
    }

    #[test]
    fn connection_bounds() {
        let bounds = |cli_min, config_min, max| connections(cli_min, config_min, max).ok();
        assert_eq!(bounds(None, None, None), Some((1, 15)));
        assert_eq!(bounds(Some(5), None, None), Some((5, 15)));
        // Without a maximum, a minimum above the default raises it.
        assert_eq!(bounds(Some(20), None, None), Some((20, 20)));
        assert_eq!(bounds(None, Some(20), None), Some((20, 20)));
        assert_eq!(bounds(Some(20), None, Some(30)), Some((20, 30)));
        assert_eq!(bounds(Some(5), None, Some(2)), None);
        assert_eq!(bounds(None, Some(4), Some(1)), Some((1, 1)));
        assert_eq!(bounds(None, Some(0), None), None);
    }

    #[test]
    fn proxy_urls() {
        assert!(parse_proxy_url("socks5h://127.0.0.1:1080", "test").is_ok());
        assert!(parse_proxy_url("http://user:pw@proxy:3128", "test").is_ok());
        assert!(parse_proxy_url("socks4://127.0.0.1:1080", "test").is_err());
    }

    #[test]
    fn cli_parses_documented_examples() {
        let args = Args::try_parse_from([
            "vayu",
            "https://example.com/file.zip",
            "--min-connections",
            "2",
            "--max-connections",
            "8",
            "--proxy",
            "socks5h://127.0.0.1:1080",
            "--ui",
            "compact",
            "--color",
            "auto",
            "--http-version",
            "1.1",
            "--retry",
            "forever",
        ])
        .unwrap();
        assert_eq!(args.min_connections, Some(2));
        assert_eq!(args.max_connections, Some(8));
        assert_eq!(args.ui, Some(UiMode::Compact));
        assert_eq!(args.http_version, Some(HttpVersion::Http11));
        assert_eq!(args.retry, Some(RetryMode::Forever));
        assert!(Args::try_parse_from(["vayu", "u", "--max-connections", "0"]).is_err());
        assert!(Args::try_parse_from(["vayu", "u", "--min-connections", "65"]).is_err());
        assert!(Args::try_parse_from(["vayu", "u", "--proxy", "x", "--no-proxy"]).is_err());
        assert!(
            Args::try_parse_from(["vayu", "u", "--restart", "--resume-from", "f.vayu"]).is_err()
        );
    }
}
