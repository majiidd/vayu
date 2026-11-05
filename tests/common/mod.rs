//! A small, fully controllable HTTP/1.1 server for end-to-end tests.
//!
//! Each test registers a handler that sees every request (with a per-path attempt counter) and
//! returns a scripted reply: status, headers, body, and optional misbehavior such as closing the
//! connection early or stalling.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub struct Request {
    pub path: String,
    pub headers: HashMap<String, String>,
    /// 1 for the first request to this path, 2 for the second, ...
    pub attempt: usize,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Start offset of a `Range: bytes=N-` request.
    pub fn range_start(&self) -> Option<usize> {
        self.header("range")?
            .strip_prefix("bytes=")?
            .strip_suffix('-')?
            .parse()
            .ok()
    }

    /// `Range: bytes=A-B` or `bytes=A-` as `[A, B + 1)`, with `None` for an open end.
    pub fn range(&self) -> Option<(usize, Option<usize>)> {
        let (a, b) = self
            .header("range")?
            .strip_prefix("bytes=")?
            .split_once('-')?;
        let start = a.parse().ok()?;
        let end = if b.is_empty() {
            None
        } else {
            Some(b.parse::<usize>().ok()? + 1)
        };
        Some((start, end))
    }
}

/// A bandwidth budget shared by every connection that uses it.
#[derive(Debug)]
pub struct SharedRate {
    rate: f64,
    window: Mutex<(Instant, u64)>,
}

impl SharedRate {
    pub fn new(bytes_per_second: u64) -> Arc<SharedRate> {
        Arc::new(SharedRate {
            rate: bytes_per_second as f64,
            window: Mutex::new((Instant::now(), 0)),
        })
    }

    fn delay(&self, n: usize) -> Duration {
        let mut window = self.window.lock().unwrap();
        window.1 += n as u64;
        let due = Duration::from_secs_f64(window.1 as f64 / self.rate);
        due.saturating_sub(window.0.elapsed())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Close the connection after this many body bytes.
    pub cut_after: Option<usize>,
    /// Pause before sending the body.
    pub stall: Option<Duration>,
    /// Send the body in small pieces with this delay between them.
    pub trickle: Option<Duration>,
    /// Send the body at about this many bytes per second on this connection.
    pub rate: Option<u64>,
    /// Draw the body's bandwidth from a budget shared with other connections.
    pub shared: Option<Arc<SharedRate>>,
}

impl Reply {
    pub fn new(status: u16) -> Reply {
        Reply {
            status,
            ..Reply::default()
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Reply {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Reply {
        self.body = body.into();
        self
    }

    /// 200 with Content-Length.
    pub fn full(body: &[u8]) -> Reply {
        Reply::new(200)
            .header("Content-Length", body.len().to_string())
            .body(body.to_vec())
    }

    /// 206 for `body[start..]` with a valid Content-Range.
    pub fn partial(body: &[u8], start: usize) -> Reply {
        Reply::new(206)
            .header("Content-Length", (body.len() - start).to_string())
            .header(
                "Content-Range",
                format!("bytes {start}-{}/{}", body.len() - 1, body.len()),
            )
            .body(body[start..].to_vec())
    }

    pub fn cut_after(mut self, n: usize) -> Reply {
        self.cut_after = Some(n);
        self
    }

    /// 206 for `body[start..end]` with a valid Content-Range.
    pub fn slice(body: &[u8], start: usize, end: usize) -> Reply {
        Reply::new(206)
            .header("Content-Length", (end - start).to_string())
            .header(
                "Content-Range",
                format!("bytes {start}-{}/{}", end - 1, body.len()),
            )
            .body(body[start..end].to_vec())
    }

    /// Answer `req` from `body` like a range-capable server: the requested range, or all of it.
    pub fn ranged(body: &[u8], req: &Request) -> Reply {
        match req.range() {
            Some((start, end)) if start < body.len() => {
                Reply::slice(body, start, end.unwrap_or(body.len()).min(body.len()))
            }
            Some(_) => Reply::new(416).header("Content-Length", "0"),
            None => Reply::full(body),
        }
    }

    pub fn rate(mut self, bytes_per_second: u64) -> Reply {
        self.rate = Some(bytes_per_second);
        self
    }

    pub fn shared(mut self, budget: &Arc<SharedRate>) -> Reply {
        self.shared = Some(budget.clone());
        self
    }
}

type Handler = dyn Fn(&Request) -> Reply + Send + Sync;

pub struct Server {
    pub base: String,
    /// Host as it appears in URLs: `127.0.0.1` or `[::1]`.
    pub host: String,
    requests: Arc<Mutex<Vec<Request>>>,
    gauge: Arc<Gauge>,
    _runtime: tokio::runtime::Runtime,
}

/// Responses being sent right now, and the most at once.
#[derive(Default)]
struct Gauge {
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl Server {
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Server {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(bind_loopback());
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let gauge: Arc<Gauge> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let log = requests.clone();
        let counter = gauge.clone();
        runtime.spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    continue;
                };
                let handler = handler.clone();
                let log = log.clone();
                let gauge = counter.clone();
                tokio::spawn(async move {
                    let Some(mut request) = read_head(&mut socket).await else {
                        return;
                    };
                    let reply = {
                        let mut log = log.lock().unwrap();
                        request.attempt = log.iter().filter(|r| r.path == request.path).count() + 1;
                        log.push(request.clone());
                        handler(&request)
                    };
                    let now = gauge.now.fetch_add(1, Ordering::SeqCst) + 1;
                    gauge.peak.fetch_max(now, Ordering::SeqCst);
                    let _ = write_reply(&mut socket, reply).await;
                    gauge.now.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Server {
            base: format!("http://{addr}"),
            host: match addr {
                std::net::SocketAddr::V4(a) => a.ip().to_string(),
                std::net::SocketAddr::V6(a) => format!("[{}]", a.ip()),
            },
            requests,
            gauge,
            _runtime: runtime,
        }
    }

    /// The most responses that were being sent at the same time.
    pub fn peak_concurrency(&self) -> usize {
        self.gauge.peak.load(Ordering::SeqCst)
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

/// Bind on IPv4 loopback, or on IPv6 loopback when IPv4 loopback connections are refused (some
/// VPN and network-filter software blocks `127.0.0.1` while leaving `::1` usable).
async fn bind_loopback() -> TcpListener {
    let v4 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let probe = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(v4.local_addr().unwrap()),
    )
    .await;
    if matches!(probe, Ok(Ok(_))) {
        return v4;
    }
    TcpListener::bind("[::1]:0").await.unwrap()
}

async fn read_head(socket: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 || buf.len() > 64 * 1024 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
    let headers = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(Request {
        path,
        headers,
        attempt: 0,
    })
}

async fn write_reply(socket: &mut tokio::net::TcpStream, reply: Reply) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} Test\r\nConnection: close\r\n", reply.status);
    for (k, v) in &reply.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await?;
    if let Some(stall) = reply.stall {
        tokio::time::sleep(stall).await;
    }
    let body = match reply.cut_after {
        Some(n) => &reply.body[..n.min(reply.body.len())],
        None => &reply.body[..],
    };
    if let Some(delay) = reply.trickle {
        for piece in body.chunks(1024) {
            socket.write_all(piece).await?;
            tokio::time::sleep(delay).await;
        }
    } else if reply.rate.is_some() || reply.shared.is_some() {
        let started = Instant::now();
        let mut sent = 0;
        for piece in body.chunks(16 * 1024) {
            let mut wait = Duration::ZERO;
            if let Some(rate) = reply.rate {
                sent += piece.len();
                let due = Duration::from_secs_f64(sent as f64 / rate as f64);
                wait = due.saturating_sub(started.elapsed());
            }
            if let Some(shared) = &reply.shared {
                wait = wait.max(shared.delay(piece.len()));
            }
            tokio::time::sleep(wait).await;
            socket.write_all(piece).await?;
        }
    } else {
        socket.write_all(body).await?;
    }
    socket.flush().await?;
    socket.shutdown().await
}

/// Deterministic, non-repeating test payload.
pub fn payload(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Run vayu with a clean, non-interactive environment in `dir`.
pub fn vayu(dir: &Path, args: &[&str]) -> Output {
    vayu_env(dir, args, &[])
}

/// Run to completion, failing the test instead of hanging if vayu does not exit in time
/// (for example when a sandbox blocks loopback connections and vayu keeps retrying).
pub fn vayu_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let child = command(dir, args, env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(output) => output.unwrap(),
        Err(_) => {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            panic!("vayu {args:?} did not finish within 60s; is loopback networking blocked?");
        }
    }
}

pub fn command(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vayu"));
    cmd.current_dir(dir)
        .args(args)
        .env_clear()
        .env("HOME", dir)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("VAYU_CONFIG", dir.join("no-config.toml"))
        .stdin(Stdio::null());
    // VAYU_CONFIG points at a missing file: an explicit missing config is an error, so
    // create an empty one.
    std::fs::write(dir.join("no-config.toml"), "").unwrap();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "no-config.toml")
        .collect();
    names.sort();
    names
}

pub fn path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}
