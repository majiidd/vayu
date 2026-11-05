//! HTTP client construction, request policy, and failure classification.

pub mod cookies;
pub mod headers;
pub mod http;

use std::error::Error as StdError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use reqwest::cookie::Jar;
use reqwest::header::{self, HeaderValue};
use reqwest::{Client, Response, StatusCode};
use url::Url;

use crate::cli::{HttpVersion, ProxySetting, Settings};
use crate::error::{Category, Failure, describe_io};
use crate::presentation::text::redact_url;
use headers::UserHeader;

const MAX_REDIRECTS: usize = 10;

/// A request for the bytes `[start, end)` of a representation, or from `start` to its end.
#[derive(Clone, Copy, Debug)]
pub struct RangeRequest<'a> {
    pub start: u64,
    /// Exclusive end; `None` asks for everything from `start`.
    pub end: Option<u64>,
    /// `If-Range` validator. Without it, the caller must have another way to verify the result.
    pub if_range: Option<&'a str>,
}

impl RangeRequest<'_> {
    fn header(&self) -> String {
        match self.end {
            Some(end) => format!("bytes={}-{}", self.start, end - 1),
            None => format!("bytes={}-", self.start),
        }
    }
}

/// Why one attempt failed.
#[derive(Debug)]
pub enum AttemptError {
    /// Transient; try again after a delay.
    Retryable {
        reason: String,
        retry_after: Option<Duration>,
    },
    /// Retrying the same request cannot succeed.
    Fatal(Failure),
}

type BuildClient = dyn Fn() -> reqwest::Result<Client> + Send + Sync;

/// HTTP access for one download.
///
/// Each connection slot (lane) has its own client and therefore its own connection pool: lane
/// `n` keeps reusing its own connection, and parallel range requests use separate transport
/// connections even over HTTP/2, which would otherwise multiplex them onto one connection that
/// shares a single congestion window and any per-connection limit.
pub struct Transport {
    /// Lane 0, built up front so configuration errors surface before any request.
    client: Client,
    lanes: Mutex<Vec<Client>>,
    build: Box<BuildClient>,
    /// Origin of the requested URL; custom headers are sent only here.
    origin: url::Origin,
    headers: Vec<UserHeader>,
    /// Human-readable route, e.g. `via socks5h://127.0.0.1:1080`.
    pub route: Option<String>,
    /// Set when the bundled root store replaced a missing system store.
    pub trust_note: Option<&'static str>,
}

impl Transport {
    pub fn new(settings: &Settings) -> Result<Transport, Failure> {
        let proxy = match &settings.proxy {
            ProxySetting::Direct | ProxySetting::System => None,
            ProxySetting::Explicit(url) => Some(
                reqwest::Proxy::all(url.as_str())
                    .map_err(|err| Failure::usage(format!("invalid proxy URL: {err}")))?
                    .no_proxy(reqwest::NoProxy::from_env()),
            ),
        };
        let route = match &settings.proxy {
            ProxySetting::Direct => Some("direct".to_string()),
            ProxySetting::System => system_proxy_label(),
            ProxySetting::Explicit(url) => Some(format!("via {}", redact_url(url))),
        };

        let jar = Arc::new(Jar::default());
        if let Some(path) = &settings.cookie_file {
            cookies::load(path, &jar)?;
        }
        let (roots, trust_note) = trust_roots(&settings.ca_certs)?;

        let direct = settings.proxy == ProxySetting::Direct;
        let connect_timeout = settings.connect_timeout;
        let http_version = settings.http_version;
        let build = move || {
            client_builder(connect_timeout, http_version, direct, proxy.clone())
                .cookie_provider(jar.clone())
                .tls_certs_merge(roots.clone())
                .build()
        };
        let client = build().map_err(|err| {
            Failure::new(
                Category::Internal,
                format!("cannot initialize HTTP client: {}", chain(&err)),
            )
        })?;

        Ok(Transport {
            client,
            lanes: Mutex::default(),
            build: Box::new(build),
            origin: settings.url.origin(),
            headers: settings.headers.clone(),
            route,
            trust_note,
        })
    }

    /// The client for connection slot `lane`.
    fn lane(&self, lane: usize) -> Result<Client, AttemptError> {
        if lane == 0 {
            return Ok(self.client.clone());
        }
        let mut lanes = self.lanes.lock().expect("lanes lock");
        while lanes.len() < lane {
            let client = (self.build)().map_err(|err| {
                AttemptError::Fatal(Failure::new(
                    Category::Internal,
                    format!("cannot initialize HTTP client: {}", chain(&err)),
                ))
            })?;
            lanes.push(client);
        }
        Ok(lanes[lane - 1].clone())
    }

    /// Send a GET, following redirects under vayu's own policy: at most ten hops, HTTP(S) only,
    /// custom headers only to the original origin. Cookies follow the jar's domain rules.
    /// `lane` selects the connection slot.
    pub async fn get(
        &self,
        lane: usize,
        url: &Url,
        range: Option<RangeRequest<'_>>,
        mut on_redirect: impl FnMut(&Url),
    ) -> Result<(Url, Response), AttemptError> {
        let client = self.lane(lane)?;
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            let mut request = client.get(current.clone()).header(
                header::ACCEPT_ENCODING,
                HeaderValue::from_static("identity"),
            );
            if let Some(range) = range {
                request = request.header(header::RANGE, range.header());
                if let Some(validator) = range.if_range {
                    request = request.header(header::IF_RANGE, validator);
                }
            }
            if current.origin() == self.origin {
                for h in &self.headers {
                    request = request.header(h.name.clone(), h.value.clone());
                }
            }

            let response = request
                .send()
                .await
                .map_err(|err| classify_error(&err, &current))?;
            if !response.status().is_redirection() || response.status() == StatusCode::NOT_MODIFIED
            {
                return Ok((current, response));
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    AttemptError::Fatal(Failure::remote(format!(
                        "server sent HTTP {} without a redirect location",
                        response.status().as_u16()
                    )))
                })?;
            let next = current.join(location).map_err(|_| {
                AttemptError::Fatal(Failure::remote("server redirected to an invalid URL"))
            })?;
            if !matches!(next.scheme(), "http" | "https") {
                return Err(AttemptError::Fatal(Failure::remote(format!(
                    "server redirected to an unsupported `{}` URL",
                    next.scheme()
                ))));
            }
            on_redirect(&next);
            current = next;
        }
        Err(AttemptError::Fatal(Failure::remote(format!(
            "too many redirects (more than {MAX_REDIRECTS})"
        ))))
    }
}

/// Client settings shared by every lane. Each lane keeps a single connection per host.
fn client_builder(
    connect_timeout: Duration,
    http_version: HttpVersion,
    direct: bool,
    proxy: Option<reqwest::Proxy>,
) -> reqwest::ClientBuilder {
    let mut builder = Client::builder()
        .user_agent(concat!("vayu/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .tcp_keepalive(Duration::from_secs(30))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .http2_keep_alive_timeout(Duration::from_secs(20))
        .pool_idle_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(1);
    builder = match http_version {
        HttpVersion::Auto => builder,
        HttpVersion::Http11 => builder.http1_only(),
        HttpVersion::Http2 => builder.http2_prior_knowledge(),
    };
    if direct {
        builder = builder.no_proxy();
    }
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    }
    builder
}

fn system_proxy_label() -> Option<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .find(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
    .map(|name| format!("proxy from ${name}"))
}

/// Trust roots in addition to the platform verifier's system store. On Linux and other
/// non-Apple Unix systems without a usable system store, fall back to the bundled Mozilla roots.
fn trust_roots(
    ca_certs: &[std::path::PathBuf],
) -> Result<(Vec<reqwest::Certificate>, Option<&'static str>), Failure> {
    let mut roots = Vec::new();
    for path in ca_certs {
        let pem = std::fs::read(path).map_err(|err| {
            Failure::usage(format!(
                "cannot read certificate file {}: {}",
                path.display(),
                describe_io(&err)
            ))
        })?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .ok()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                Failure::usage(format!(
                    "{} does not contain PEM certificates",
                    path.display()
                ))
            })?;
        roots.extend(certs);
    }

    #[cfg(all(unix, not(target_vendor = "apple")))]
    if rustls_native_certs::load_native_certs().certs.is_empty() {
        for der in webpki_root_certs::TLS_SERVER_ROOT_CERTS {
            if let Ok(cert) = reqwest::Certificate::from_der(der.as_ref()) {
                roots.push(cert);
            }
        }
        return Ok((
            roots,
            Some("system certificate store not found; using bundled roots"),
        ));
    }

    Ok((roots, None))
}

/// Classify a transport error as transient or permanent.
pub fn classify_error(err: &reqwest::Error, url: &Url) -> AttemptError {
    let host = url.host_str().unwrap_or("the server");
    if let Some(tls) = find::<rustls::Error>(err) {
        return classify_tls(tls, host);
    }
    // Skip reqwest's top-level message: it embeds the URL, which may carry signed secrets.
    let text = err.source().map(chain).unwrap_or_default();
    let lower = text.to_ascii_lowercase();
    if is_proxy_auth_failure(&lower) {
        return AttemptError::Fatal(proxy_auth_failure());
    }
    if err.is_builder() {
        return AttemptError::Fatal(Failure::usage(format!("cannot build request: {text}")));
    }
    let reason = if err.is_timeout() {
        "connection timed out".to_string()
    } else if err.is_dns() {
        format!("cannot resolve {host}")
    } else if let Some(io) = find::<std::io::Error>(err) {
        use std::io::ErrorKind::*;
        match io.kind() {
            ConnectionRefused => "connection refused".into(),
            ConnectionReset | ConnectionAborted => "connection reset".into(),
            UnexpectedEof | BrokenPipe => "connection closed early".into(),
            TimedOut => "connection timed out".into(),
            NetworkUnreachable | HostUnreachable | NetworkDown => "network unreachable".into(),
            _ if err.is_connect() => format!("cannot connect to {host}"),
            _ => "connection lost".into(),
        }
    } else if err.is_connect() {
        format!("cannot connect to {host}")
    } else {
        "connection lost".into()
    };
    AttemptError::Retryable {
        reason,
        retry_after: None,
    }
}

/// Classify a non-success HTTP status.
pub fn classify_status(response: &Response) -> AttemptError {
    let status = response.status();
    let code = status.as_u16();
    let label = format!(
        "HTTP {code}{}",
        status
            .canonical_reason()
            .map(|r| format!(" {r}"))
            .unwrap_or_default()
    );
    match code {
        408 | 425 | 429 | 500 | 502 | 503 | 504 => AttemptError::Retryable {
            reason: if code == 429 || code == 503 {
                format!("server is busy ({label})")
            } else {
                format!("server error ({label})")
            },
            retry_after: http::retry_after(response.headers(), SystemTime::now()),
        },
        401 => AttemptError::Fatal(
            Failure::remote(format!("authentication required ({label})"))
                .with_hint("supply credentials with --header-file or --cookie-file"),
        ),
        403 => AttemptError::Fatal(
            Failure::remote(format!("access denied ({label})"))
                .with_hint("the link may have expired or need credentials; get a fresh link"),
        ),
        404 | 410 => AttemptError::Fatal(
            Failure::remote(format!("file not found ({label})")).with_hint("check the URL"),
        ),
        407 => AttemptError::Fatal(proxy_auth_failure()),
        _ => AttemptError::Fatal(Failure::remote(format!("server responded with {label}"))),
    }
}

/// Certificate problems are permanent until the user acts. Other TLS failures (corrupted
/// records, unexpected alerts, resets mid-handshake) are typical of interfering middleboxes
/// and lossy links, so they are retried.
fn classify_tls(err: &rustls::Error, host: &str) -> AttemptError {
    let detail = match err {
        rustls::Error::InvalidCertificate(reason) => describe_certificate_error(reason),
        rustls::Error::NoCertificatesPresented => "the server presented no certificate".into(),
        _ => {
            return AttemptError::Retryable {
                reason: "TLS handshake failed".into(),
                retry_after: None,
            };
        }
    };
    AttemptError::Fatal(
        Failure::remote(format!(
            "TLS certificate for {host} could not be verified ({detail})"
        ))
        .with_hint(
            "if your network uses a private certificate authority, trust it with \
             --ca-cert PATH; vayu never disables certificate verification",
        ),
    )
}

fn describe_certificate_error(err: &rustls::CertificateError) -> String {
    use rustls::CertificateError::*;
    match err {
        Expired | ExpiredContext { .. } => "the certificate has expired".into(),
        NotValidYet | NotValidYetContext { .. } => "the certificate is not valid yet".into(),
        NotValidForName | NotValidForNameContext { .. } => {
            "the certificate is not valid for this host".into()
        }
        UnknownIssuer => "issued by an untrusted certificate authority".into(),
        Revoked => "the certificate has been revoked".into(),
        BadSignature | BadEncoding => "the certificate is malformed".into(),
        Other(other) => other.to_string(),
        other => format!("{other:?}"),
    }
}

/// Proxy authentication failures surface only as error text from hyper-util's HTTP tunnel and
/// SOCKS5 handshake; match the messages those versions produce.
fn is_proxy_auth_failure(lower: &str) -> bool {
    [
        "proxy authorization required",
        "proxy authentication required",
        "credentials not accepted",
        "does not support user/pass authentication",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn proxy_auth_failure() -> Failure {
    Failure::remote("the proxy rejected the credentials or requires authentication")
        .with_hint("include credentials in the proxy URL, e.g. http://user:password@host:port")
}

/// Find an error of type `T` in a source chain, looking inside `io::Error` wrappers.
fn find<'a, T: StdError + 'static>(err: &'a (dyn StdError + 'static)) -> Option<&'a T> {
    let mut current: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = current {
        if let Some(found) = e.downcast_ref::<T>() {
            return Some(found);
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && let Some(inner) = io.get_ref()
            && let Some(found) = find::<T>(inner)
        {
            return Some(found);
        }
        current = e.source();
    }
    None
}

/// Join an error and its sources into one line.
pub fn chain(err: &(dyn StdError + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(e) = source {
        let text = e.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        source = e.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_headers() {
        let open = RangeRequest {
            start: 5,
            end: None,
            if_range: None,
        };
        assert_eq!(open.header(), "bytes=5-");
        let closed = RangeRequest {
            end: Some(10),
            ..open
        };
        assert_eq!(closed.header(), "bytes=5-9");
    }

    #[test]
    fn proxy_auth_messages() {
        assert!(is_proxy_auth_failure(
            "client error (connect): proxy authorization required"
        ));
        assert!(is_proxy_auth_failure(
            "socks error: credentials not accepted"
        ));
        assert!(!is_proxy_auth_failure("connection refused"));
    }

    #[test]
    fn tls_classification() {
        let cert = rustls::Error::InvalidCertificate(rustls::CertificateError::Expired);
        match classify_tls(&cert, "example.com") {
            AttemptError::Fatal(f) => {
                assert!(f.message.contains("example.com") && f.message.contains("expired"));
                assert!(f.hint.unwrap().contains("--ca-cert"));
            }
            other => panic!("certificate errors are fatal: {other:?}"),
        }
        assert!(matches!(
            classify_tls(&rustls::Error::NoCertificatesPresented, "h"),
            AttemptError::Fatal(_)
        ));
        for transient in [
            rustls::Error::DecryptError,
            rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure),
            rustls::Error::InvalidMessage(rustls::InvalidMessage::MessageTooShort),
        ] {
            assert!(
                matches!(
                    classify_tls(&transient, "h"),
                    AttemptError::Retryable { .. }
                ),
                "{transient:?} is retried"
            );
        }
    }
}
