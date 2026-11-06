//! The real binary on a pseudo-terminal: dashboard versus line output, color and glyph
//! selection, responsive layouts and resizes, the SSH refresh rate, background jobs, and a
//! terminal that disappears (an SSH session that drops, with and without `nohup`).

mod common;

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::*;

const KIB: usize = 1 << 10;
const ETAG: &str = "\"v1\"";

/// A terminal-like environment with a UTF-8 locale.
const XTERM: &[(&str, &str)] = &[("TERM", "xterm-256color"), ("LANG", "en_US.UTF-8")];

struct Pty {
    master: OwnedFd,
    slave: OwnedFd,
}

impl Pty {
    fn open(cols: u16, rows: u16) -> Pty {
        let (mut master, mut slave) = (-1, -1);
        let mut size = winsize(cols, rows);
        let ok = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        };
        assert_eq!(ok, 0, "openpty: {}", std::io::Error::last_os_error());
        // Not inherited: a child holding the master would keep the terminal open after
        // `hang_up`. The child's stderr is a separate, inheritable copy of the slave.
        for fd in [master, slave] {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        unsafe {
            Pty {
                master: OwnedFd::from_raw_fd(master),
                slave: OwnedFd::from_raw_fd(slave),
            }
        }
    }

    /// Make background writes stop the writer with `SIGTTOU`, like `stty tostop`.
    fn set_tostop(&self) {
        unsafe {
            let mut attrs: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(self.slave.as_raw_fd(), &mut attrs), 0);
            attrs.c_lflag |= libc::TOSTOP;
            assert_eq!(
                libc::tcsetattr(self.slave.as_raw_fd(), libc::TCSANOW, &attrs),
                0
            );
        }
    }
}

fn set_size(master: RawFd, cols: u16, rows: u16) {
    let size = winsize(cols, rows);
    let ok = unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &size) };
    assert_eq!(ok, 0, "TIOCSWINSZ: {}", std::io::Error::last_os_error());
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// How the child is attached to the terminal.
#[derive(Clone, Copy, PartialEq)]
enum Session {
    /// stderr is the terminal, but it is not the controlling terminal.
    Plain,
    /// The terminal controls a new session, as for a login over SSH: closing it sends `SIGHUP`.
    Controlling,
    /// The same, with `SIGHUP` ignored as `nohup` leaves it.
    Nohup,
    /// Started as a background job (`vayu … &`) of a job-control shell on the terminal.
    Background,
}

struct Run {
    child: Child,
    /// The master, owned by the reader thread; valid until `hang_up`.
    master: RawFd,
    /// Asks the reader to close the master, which hangs up the terminal.
    hangup: Arc<AtomicBool>,
    output: Arc<Mutex<Vec<u8>>>,
    /// When each read arrived, and the output length after it.
    reads: Arc<Mutex<Vec<(Instant, usize)>>>,
    reader: Option<JoinHandle<()>>,
}

impl Run {
    fn start(dir: &Path, pty: Pty, session: Session, args: &[&str], env: &[(&str, &str)]) -> Run {
        let mut cmd = command(dir, args, env);
        if session == Session::Background {
            // Job control gives the background job its own process group; the shell keeps the
            // terminal and exits with the job's status.
            let mut shell = std::process::Command::new("/bin/sh");
            shell
                .args(["-c", "set -m; \"$0\" \"$@\" & wait $!"])
                .arg(cmd.get_program())
                .args(args)
                .current_dir(dir)
                .env_clear()
                .stdin(Stdio::null());
            for (key, value) in cmd.get_envs() {
                if let Some(value) = value {
                    shell.env(key, value);
                }
            }
            cmd = shell;
        }
        let Pty { master, slave } = pty;
        cmd.stdout(Stdio::null()).stderr(Stdio::from(slave));
        if session != Session::Plain {
            let ignore_hangup = session == Session::Nohup;
            unsafe {
                cmd.pre_exec(move || {
                    // stderr is the terminal by now.
                    if libc::setsid() < 0 || libc::ioctl(2, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if ignore_hangup {
                        libc::signal(libc::SIGHUP, libc::SIG_IGN);
                    }
                    Ok(())
                });
            }
        }
        // `cmd` owns the parent's slave descriptor; dropping it after the spawn lets the
        // master see EOF once the child is gone.
        let child = cmd.spawn().unwrap();
        drop(cmd);
        let raw = master.as_raw_fd();
        let hangup: Arc<AtomicBool> = Arc::default();
        let output: Arc<Mutex<Vec<u8>>> = Arc::default();
        let reads: Arc<Mutex<Vec<(Instant, usize)>>> = Arc::default();
        let (sink, log, closing) = (output.clone(), reads.clone(), hangup.clone());
        // Drain continuously (a full terminal buffer would block the presenter) until EOF, EIO
        // once every slave descriptor is closed (Linux), or a hang-up request. This thread
        // holds the only master descriptor, so dropping it really closes the terminal.
        let reader = std::thread::spawn(move || {
            let mut master = std::fs::File::from(master);
            let mut buf = [0u8; 16 * KIB];
            while !closing.load(Ordering::SeqCst) {
                let mut poll = libc::pollfd {
                    fd: master.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, 20) } == 0 {
                    continue;
                }
                match master.read(&mut buf) {
                    Ok(n @ 1..) => {
                        let mut out = sink.lock().unwrap();
                        out.extend_from_slice(&buf[..n]);
                        log.lock().unwrap().push((Instant::now(), out.len()));
                    }
                    _ => break,
                }
            }
        });
        Run {
            child,
            master: raw,
            hangup,
            output,
            reads,
            reader: Some(reader),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn len(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    /// Wait until the visible output contains `needle`.
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !visible(&self.text()).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "{needle:?} never appeared in {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        assert!(!self.hangup.load(Ordering::SeqCst));
        set_size(self.master, cols, rows);
    }

    /// Close the terminal, like an SSH connection that drops.
    fn hang_up(&mut self) {
        self.hangup.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }

    fn finish(mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let pid = self.child.id() as i32;
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                }
                panic!("vayu did not finish; output so far: {:?}", self.text());
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // The reader ends at EOF once it has read everything the child wrote.
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.reader.as_ref().is_some_and(|r| !r.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.hang_up();
        (status, self.text())
    }
}

/// A server that sends `body` at `rate` bytes per second, so the dashboard draws frames.
fn slow_server(body: &[u8], rate: u64) -> Server {
    let served = body.to_vec();
    Server::start(move |req| {
        Reply::ranged(&served, req)
            .header("ETag", ETAG)
            .header("Accept-Ranges", "bytes")
            .rate(rate)
    })
}

/// Download `len` bytes over about `secs` seconds on a terminal of the given size.
fn download(
    cols: u16,
    rows: u16,
    env: &[(&str, &str)],
    extra: &[&str],
    len: usize,
    secs: f64,
) -> (ExitStatus, String) {
    let body = payload(len);
    let server = slow_server(&body, (len as f64 / secs) as u64);
    let dir = tempfile::tempdir().unwrap();
    let url = server.url("/file.bin");
    let mut args = vec![url.as_str(), "--max-connections", "1"];
    args.extend_from_slice(extra);
    let run = Run::start(
        dir.path(),
        Pty::open(cols, rows),
        Session::Plain,
        &args,
        env,
    );
    let (status, out) = run.finish();
    assert!(status.success(), "{status:?}: {out:?}");
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
    (status, out)
}

/// Escape sequences of the form `ESC [ params final`.
fn sequences(out: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = out;
    while let Some(at) = rest.find("\x1b[") {
        let tail = &rest[at + 2..];
        let end = tail
            .find(|c: char| ('\x40'..='\x7e').contains(&c))
            .unwrap_or(tail.len());
        found.push(tail[..(end + 1).min(tail.len())].to_string());
        rest = &tail[(end + 1).min(tail.len())..];
    }
    found
}

/// Text without escape sequences.
fn visible(out: &str) -> String {
    let mut text = String::new();
    let mut rest = out;
    while let Some(at) = rest.find("\x1b[") {
        text.push_str(&rest[..at]);
        let tail = &rest[at + 2..];
        let end = tail
            .find(|c: char| ('\x40'..='\x7e').contains(&c))
            .map(|e| e + 1)
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    text.push_str(rest);
    text
}

/// Frames redrawn in place (each one starts by moving the cursor up).
fn redraws(out: &str) -> usize {
    sequences(out).iter().filter(|s| s.ends_with('A')).count()
}

fn colored(out: &str) -> bool {
    sequences(out).iter().any(|s| s.ends_with('m'))
}

#[test]
fn capable_terminal_gets_a_live_colored_dashboard() {
    let env = [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("COLORFGBG", "15;0"),
        ("LANG", "en_US.UTF-8"),
    ];
    let (_, out) = download(100, 30, &env, &[], 512 * KIB, 1.5);
    assert!(redraws(&out) >= 3, "frames are redrawn in place: {out:?}");
    assert!(out.contains("\x1b[38;2;"), "truecolor palette: {out:?}");
    let text = visible(&out);
    for label in ["Speed", "Connections", "ETA", "█", "Ctrl+C to stop"] {
        assert!(text.contains(label), "{label:?} missing from {text:?}");
    }
    assert!(text.contains("  vayu  ✓  Complete"), "{text:?}");
    assert!(!out.contains("\x1b[?1049h"), "no alternate screen");
    assert!(!out.contains("\x1b[?25l"), "the cursor is never hidden");
}

#[test]
fn no_color_keeps_the_dashboard_without_color() {
    let env = [XTERM, &[("NO_COLOR", "1")]].concat();
    let (_, out) = download(100, 30, &env, &[], 256 * KIB, 1.0);
    assert!(redraws(&out) >= 2, "still a live display: {out:?}");
    assert!(!colored(&out), "no SGR sequences: {:?}", sequences(&out));

    let (_, out) = download(100, 30, &env, &["--color", "always"], 64 * KIB, 0.3);
    assert!(colored(&out), "--color always overrides NO_COLOR");
}

#[test]
fn basic_terminal_uses_the_16_color_palette_and_ascii_without_utf8() {
    let env = [("TERM", "xterm"), ("LANG", "C")];
    let (_, out) = download(100, 30, &env, &[], 256 * KIB, 1.0);
    let sgr: Vec<String> = sequences(&out)
        .into_iter()
        .filter(|s| s.ends_with('m'))
        .collect();
    assert!(!sgr.is_empty());
    assert!(
        sgr.iter()
            .all(|s| !s.contains("38;5;") && !s.contains("38;2;")),
        "only ANSI colors: {sgr:?}"
    );
    let text = visible(&out);
    assert!(text.is_ascii(), "ASCII glyphs: {text:?}");
    assert!(text.contains("vayu  +  Complete"), "{text:?}");
}

#[test]
fn dumb_terminal_gets_plain_lines() {
    let env = [("TERM", "dumb"), ("LANG", "en_US.UTF-8")];
    let (_, out) = download(100, 30, &env, &[], 256 * KIB, 1.0);
    assert!(!out.contains('\x1b'), "no escape sequences at all: {out:?}");
    assert!(out.contains("vayu: downloading http://"), "{out:?}");
    assert!(out.contains("vayu: complete, saved to"), "{out:?}");
}

#[test]
fn narrow_and_tiny_terminals_use_smaller_layouts() {
    let (_, out) = download(40, 10, XTERM, &[], 256 * KIB, 1.0);
    let text = visible(&out);
    assert!(
        text.contains("ETA") && !text.contains("Connections"),
        "{text:?}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("vayu ✓ Complete")),
        "compact summary: {text:?}"
    );
    for line in text
        .lines()
        .filter(|l| !l.contains("Complete") && !l.contains("SHA-256"))
    {
        assert!(
            unicode_columns(line) < 40,
            "live lines never wrap: {line:?}"
        );
    }

    let (_, out) = download(20, 2, XTERM, &[], 256 * KIB, 1.0);
    let text = visible(&out);
    assert!(
        !text.contains("ETA") && !text.contains("Connections"),
        "{text:?}"
    );
    assert!(text.contains('%'), "{text:?}");
}

/// Display columns, counting every character as one (the dashboard's glyphs are all narrow).
fn unicode_columns(line: &str) -> usize {
    line.chars().count()
}

#[test]
fn resize_switches_layout_mid_download() {
    let body = payload(512 * KIB);
    let server = slow_server(&body, 256 * KIB as u64);
    let dir = tempfile::tempdir().unwrap();
    let url = server.url("/file.bin");
    let run = Run::start(
        dir.path(),
        Pty::open(100, 30),
        Session::Plain,
        &[&url, "--max-connections", "1"],
        XTERM,
    );
    run.wait_for("Connections");
    let mark = run.len();
    run.resize(40, 10);
    let (status, out) = run.finish();
    assert!(status.success(), "{out:?}");
    let after = visible(&String::from_utf8_lossy(&out.as_bytes()[mark..]));
    assert!(
        after.contains("vayu ↓ file.bin"),
        "compact frame: {after:?}"
    );
    assert!(
        after.lines().any(|l| l.starts_with("vayu ✓ Complete")),
        "compact summary after narrowing: {after:?}"
    );
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
}

/// The shortest time between two in-place redraws during a download over about `secs` seconds.
fn shortest_redraw_gap(env: &[(&str, &str)], len: usize, secs: f64) -> Duration {
    let body = payload(len);
    let server = slow_server(&body, (len as f64 / secs) as u64);
    let dir = tempfile::tempdir().unwrap();
    let url = server.url("/file.bin");
    let run = Run::start(
        dir.path(),
        Pty::open(100, 30),
        Session::Plain,
        &[&url, "--max-connections", "1"],
        env,
    );
    let reads = run.reads.clone();
    let (status, out) = run.finish();
    assert!(status.success(), "{status:?}: {out:?}");
    let reads = reads.lock().unwrap();
    // A redraw starts with a cursor-up sequence; time it by the read that delivered it.
    let bytes = out.as_bytes();
    let mut times = Vec::new();
    for at in 0..bytes.len() {
        if bytes[at] == 0x1b
            && out[at..].starts_with("\x1b[")
            && sequences(&out[at..])
                .first()
                .is_some_and(|s| s.ends_with('A'))
        {
            let read = reads.iter().find(|(_, end)| *end > at).unwrap();
            times.push(read.0);
        }
    }
    assert!(
        times.len() >= 4,
        "too few redraws to measure: {}",
        times.len()
    );
    times
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|gap| !gap.is_zero())
        .min()
        .unwrap()
}

#[test]
fn ssh_sessions_redraw_less_often() {
    // Timers can only delay frames, so the shortest gap shows the cap even on a slow machine.
    let local = shortest_redraw_gap(XTERM, 4096 * KIB, 3.0);
    let ssh_env = [XTERM, &[("SSH_CONNECTION", "10.0.0.2 52000 10.0.0.1 22")]].concat();
    let ssh = shortest_redraw_gap(&ssh_env, 4096 * KIB, 3.0);
    // At most 8 frames a second locally (125 ms apart) and 2 over SSH (500 ms apart).
    assert!(
        local < Duration::from_millis(300),
        "local frames {local:?} apart"
    );
    assert!(
        ssh >= Duration::from_millis(400),
        "ssh frames {ssh:?} apart"
    );
}

#[test]
fn background_job_does_not_draw_over_the_shell() {
    for tostop in [false, true] {
        let body = payload(256 * KIB);
        let server = slow_server(&body, 256 * KIB as u64);
        let dir = tempfile::tempdir().unwrap();
        let url = server.url("/file.bin");
        let pty = Pty::open(100, 30);
        if tostop {
            pty.set_tostop();
        }
        let run = Run::start(
            dir.path(),
            pty,
            Session::Background,
            &[&url, "--max-connections", "1"],
            XTERM,
        );
        // With `tostop`, a single frame would stop the job and this would time out.
        let (status, out) = run.finish();
        assert!(status.success(), "tostop={tostop} {status:?}: {out:?}");
        assert_eq!(redraws(&out), 0, "no live frames: {out:?}");
        assert!(!visible(&out).contains("Connections"), "{out:?}");
        assert_eq!(
            visible(&out).contains("Complete"),
            !tostop,
            "the summary is shown unless writing it would stop the job: {out:?}"
        );
        assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
    }
}

#[test]
fn vanished_terminal_does_not_stop_the_download() {
    let body = payload(512 * KIB);
    let server = slow_server(&body, 512 * KIB as u64);
    let dir = tempfile::tempdir().unwrap();
    let url = server.url("/file.bin");
    let mut run = Run::start(
        dir.path(),
        Pty::open(100, 30),
        Session::Plain,
        &[&url, "--max-connections", "1"],
        XTERM,
    );
    run.wait_for("Downloading");
    run.hang_up();
    let (status, out) = run.finish();
    assert!(status.success(), "{status:?}: {out:?}");
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
}

#[test]
fn dropped_ssh_session_stops_safely_and_resumes() {
    let body = payload(1024 * KIB);
    let server = slow_server(&body, 256 * KIB as u64);
    let dir = tempfile::tempdir().unwrap();
    let url = server.url("/file.bin");
    let mut run = Run::start(
        dir.path(),
        Pty::open(100, 30),
        Session::Controlling,
        &[&url, "--max-connections", "1"],
        XTERM,
    );
    run.wait_for("Downloading");
    run.hang_up();
    let (status, _) = run.finish();
    assert_eq!(status.code(), Some(129), "{status:?}");
    assert_eq!(files(dir.path()), ["file.bin.part", "file.bin.vayu"]);

    let out = vayu(dir.path(), &[&url, "--max-connections", "1"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
    let rerun = server.requests().last().unwrap().range_start().unwrap_or(0);
    assert!(rerun > 0, "the rerun continued from saved progress");
}

#[test]
fn nohup_survives_a_dropped_ssh_session() {
    let body = payload(512 * KIB);
    let server = slow_server(&body, 384 * KIB as u64);
    let dir = tempfile::tempdir().unwrap();
    let url = server.url("/file.bin");
    let mut run = Run::start(
        dir.path(),
        Pty::open(100, 30),
        Session::Nohup,
        &[&url, "--max-connections", "1"],
        XTERM,
    );
    run.wait_for("Downloading");
    run.hang_up();
    let (status, _) = run.finish();
    assert!(status.success(), "{status:?}");
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
}
