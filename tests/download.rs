//! End-to-end behavior of the `vayu` binary against a scripted local server.

mod common;

use std::time::Duration;

use common::*;

const SIZE: usize = 256 * 1024;
const ETAG: &str = "\"v1\"";

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn downloads_and_publishes_exact_bytes() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |_| Reply::full(&served).header("ETag", ETAG));
    let dir = tempdir();

    let out = vayu(dir.path(), &[&server.url("/files/data.bin")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("data.bin")).unwrap(), body);
    assert_eq!(files(dir.path()), ["data.bin"], "no leftover part file");

    let err = stderr(&out);
    assert!(err.contains("complete, saved to"), "{err}");
    assert!(
        err.contains(&sha256_hex(&body)),
        "summary reports the digest: {err}"
    );
    assert!(
        !err.contains('\x1b'),
        "no escape sequences without a terminal: {err:?}"
    );

    let request = &server.requests()[0];
    assert_eq!(request.header("accept-encoding"), Some("identity"));
    assert!(request.header("user-agent").unwrap().starts_with("vayu/"));
}

#[test]
fn resumes_with_if_range_after_a_dropped_connection() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| match req.range_start() {
        None if req.attempt == 1 => Reply::full(&served)
            .header("ETag", ETAG)
            .cut_after(SIZE / 2),
        Some(start) if req.header("if-range") == Some(ETAG) => {
            Reply::partial(&served, start).header("ETag", ETAG)
        }
        _ => Reply::new(500),
    });
    let dir = tempdir();

    let out = vayu(dir.path(), &[&server.url("/data.bin"), "-o", "out.bin"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("out.bin")).unwrap(), body);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].range_start(), Some(SIZE / 2));
    let err = stderr(&out);
    assert!(err.contains("connection closed early; retrying"), "{err}");
    assert!(err.contains("reconnected; continuing at 128 KiB"), "{err}");
}

#[test]
fn restarts_from_zero_without_a_validator() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| {
        if req.attempt == 1 {
            Reply::full(&served).cut_after(SIZE / 3)
        } else {
            Reply::full(&served)
        }
    });
    let dir = tempdir();

    let out = vayu(dir.path(), &[&server.url("/data.bin")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("data.bin")).unwrap(), body);
    let requests = server.requests();
    assert_eq!(
        requests[1].header("range"),
        None,
        "unverifiable data is not resumed"
    );
    assert!(stderr(&out).contains("no way to verify resumed data"));
}

#[test]
fn checksum_allows_resuming_without_a_validator() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| match req.range_start() {
        None => Reply::full(&served).cut_after(SIZE / 2),
        Some(start) => Reply::partial(&served, start),
    });
    let dir = tempdir();
    let checksum = format!("sha256:{}", sha256_hex(&body));

    let out = vayu(
        dir.path(),
        &[&server.url("/d.bin"), "--checksum", &checksum],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let requests = server.requests();
    assert_eq!(requests[1].range_start(), Some(SIZE / 2));
    assert_eq!(requests[1].header("if-range"), None);
    assert!(stderr(&out).contains("(verified)"));
}

#[test]
fn source_change_is_a_conflict_that_keeps_data() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| match req.range_start() {
        None => Reply::full(&served)
            .header("ETag", ETAG)
            .cut_after(SIZE / 2),
        // A broken server ignores If-Range and serves a different version.
        Some(start) => Reply::partial(&served, start).header("ETag", "\"v2\""),
    });
    let dir = tempdir();

    let out = vayu(dir.path(), &[&server.url("/d.bin")]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(stderr(&out).contains("changed on the server"));
    assert_eq!(files(dir.path()), ["d.bin.part", "d.bin.vayu"]);
    assert_eq!(
        std::fs::read(dir.path().join("d.bin.part")).unwrap(),
        &body[..SIZE / 2]
    );
}

#[test]
fn ignored_range_restarts_cleanly() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| {
        let reply = Reply::full(&served).header("ETag", ETAG);
        if req.attempt == 1 {
            reply.cut_after(SIZE / 2)
        } else {
            reply // 200 even though a range was requested
        }
    });
    let dir = tempdir();

    let out = vayu(dir.path(), &[&server.url("/d.bin")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("d.bin")).unwrap(), body);
    assert!(stderr(&out).contains("did not resume"));
}

#[test]
fn retries_server_errors_honoring_retry_after() {
    let body = payload(1000);
    let served = body.clone();
    let server = Server::start(move |req| {
        if req.attempt == 1 {
            Reply::new(503)
                .header("Retry-After", "1")
                .header("Content-Length", "0")
        } else {
            Reply::full(&served)
        }
    });
    let dir = tempdir();
    let started = std::time::Instant::now();

    let out = vayu(dir.path(), &[&server.url("/x.bin")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert!(stderr(&out).contains("server is busy (HTTP 503"));
}

#[test]
fn not_found_is_a_remote_failure_without_leftovers() {
    let server = Server::start(|_| Reply::new(404).header("Content-Length", "0"));
    let dir = tempdir();

    let out = vayu(dir.path(), &[&server.url("/missing.bin")]);
    assert_eq!(out.status.code(), Some(4));
    assert!(stderr(&out).contains("file not found (HTTP 404"));
    assert!(files(dir.path()).is_empty());
    assert_eq!(server.requests().len(), 1, "404 is not retried");
}

#[test]
fn retry_budget_exhaustion_is_a_network_failure() {
    let body = payload(SIZE);
    let server = Server::start(move |_| Reply::full(&body).cut_after(SIZE / 4));
    let dir = tempdir();

    let out = vayu(dir.path(), &[&server.url("/d.bin"), "--retry", "0"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("retries are disabled"), "{err}");
    assert!(err.contains("partial data (64.0 KiB) kept at"), "{err}");
    assert_eq!(files(dir.path()), ["d.bin.part", "d.bin.vayu"]);
}

#[test]
fn stalled_transfer_times_out() {
    let body = payload(SIZE);
    let server = Server::start(move |_| {
        let mut reply = Reply::full(&body);
        reply.stall = Some(Duration::from_secs(5));
        reply
    });
    let dir = tempdir();

    let out = vayu(
        dir.path(),
        &[
            &server.url("/d.bin"),
            "--stall-timeout",
            "1s",
            "--retry",
            "0",
        ],
    );
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("connection stalled (no data for 1s)"));
}

#[test]
fn checksum_mismatch_keeps_the_result_unpublished() {
    let body = payload(5000);
    let server = Server::start(move |_| Reply::full(&body));
    let dir = tempdir();
    let wrong = format!("sha256:{}", "0".repeat(64));

    let out = vayu(dir.path(), &[&server.url("/f.iso"), "--checksum", &wrong]);
    assert_eq!(out.status.code(), Some(6), "{}", stderr(&out));
    assert!(stderr(&out).contains("SHA-256 mismatch"));
    assert_eq!(files(dir.path()), ["f.iso.part", "f.iso.vayu"]);
}

#[test]
fn existing_destination_is_never_replaced() {
    let server = Server::start(|_| Reply::full(b"new"));
    let dir = tempdir();
    std::fs::write(dir.path().join("f.txt"), b"precious").unwrap();

    for extra in [&[][..], &["--restart"][..]] {
        let mut args = vec![server.url("/f.txt")];
        args.extend(extra.iter().map(|s| s.to_string()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = vayu(dir.path(), &args);
        assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
        assert!(stderr(&out).contains("already exists"));
    }
    assert_eq!(
        std::fs::read(dir.path().join("f.txt")).unwrap(),
        b"precious"
    );

    // With -o the conflict is found before any request is sent.
    let before = server.requests().len();
    let out = vayu(dir.path(), &[&server.url("/f.txt"), "-o", "f.txt"]);
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(server.requests().len(), before);
}

#[test]
fn incomplete_part_requires_restart() {
    let body = payload(3000);
    let served = body.clone();
    let server = Server::start(move |_| Reply::full(&served));
    let dir = tempdir();
    std::fs::write(dir.path().join("f.bin.part"), b"old partial").unwrap();

    let out = vayu(dir.path(), &[&server.url("/f.bin"), "-o", "f.bin"]);
    assert_eq!(out.status.code(), Some(7));
    assert!(stderr(&out).contains("--restart"));
    assert!(server.requests().is_empty());

    let out = vayu(
        dir.path(),
        &[&server.url("/f.bin"), "-o", "f.bin", "--restart"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), body);
}

#[test]
fn content_disposition_names_are_sanitized() {
    let server = Server::start(|_| {
        Reply::full(b"hello").header(
            "Content-Disposition",
            "attachment; filename*=UTF-8''..%2F..%2Fevil%1B%5B31m.txt",
        )
    });
    let dir = tempdir();
    let sub = dir.path().join("downloads");
    std::fs::create_dir(&sub).unwrap();

    let out = vayu(dir.path(), &[&server.url("/get?id=1"), "-o", "downloads/"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(files(&sub), ["evil[31m.txt"]);
    assert!(!stderr(&out).contains('\x1b'));
}

#[test]
fn custom_headers_stay_on_the_original_origin() {
    let other = Server::start(|_| Reply::full(b"from the other origin"));
    let target = other.url("/final.bin");
    let server = Server::start(move |req| match req.path.as_str() {
        "/start" => Reply::new(302)
            .header("Location", "/same-origin")
            .header("Content-Length", "0"),
        _ => Reply::new(302)
            .header("Location", target.clone())
            .header("Content-Length", "0"),
    });
    let dir = tempdir();

    let out = vayu(
        dir.path(),
        &[
            &server.url("/start"),
            "-H",
            "X-Secret-Token: abc",
            "-o",
            "f.bin",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let first = server.requests();
    assert!(
        first
            .iter()
            .all(|r| r.header("x-secret-token") == Some("abc"))
    );
    assert_eq!(first.len(), 2);
    let elsewhere = &other.requests()[0];
    assert_eq!(
        elsewhere.header("x-secret-token"),
        None,
        "not sent cross-origin"
    );
    assert_eq!(
        std::fs::read(dir.path().join("f.bin")).unwrap(),
        b"from the other origin"
    );
}

#[test]
fn cookies_follow_domain_rules() {
    let server = Server::start(|_| Reply::full(b"ok"));
    let dir = tempdir();
    std::fs::write(
        dir.path().join("cookies.txt"),
        format!(
            "# Netscape HTTP Cookie File\n\
             {}\tFALSE\t/\tFALSE\t0\tsession\tabc123\n\
             example.com\tTRUE\t/\tFALSE\t0\tother\tnope\n",
            server.host
        ),
    )
    .unwrap();

    let out = vayu(
        dir.path(),
        &[&server.url("/f"), "--cookie-file", "cookies.txt", "-o", "f"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        server.requests()[0].header("cookie"),
        Some("session=abc123")
    );
}

#[test]
fn reserved_headers_are_rejected() {
    let dir = tempdir();
    std::fs::write(dir.path().join("h.txt"), "X-Ok: 1\nRange: bytes=5-\n").unwrap();
    let out = vayu(
        dir.path(),
        &["http://127.0.0.1:9/f", "--header-file", "h.txt"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("h.txt:2"), "{}", stderr(&out));
}

#[test]
fn invalid_arguments_exit_2() {
    let dir = tempdir();
    for args in [
        &["ftp://example.com/f"][..],
        &["https://example.com/f", "--checksum", "md5:abc"],
        &["https://example.com/f", "--retry", "often"],
        &["https://example.com/f", "--max-connections", "0"],
        &[
            "https://example.com/f",
            "--proxy",
            "socks4://127.0.0.1:1080",
        ],
    ] {
        let out = vayu(dir.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn json_events_are_versioned_and_unstyled() {
    let body = payload(SIZE);
    let server = Server::start(move |_| Reply::full(&body).header("ETag", ETAG));
    let dir = tempdir();

    let out = vayu_env(
        dir.path(),
        &[
            &format!("{}?token=secret", server.url("/d.bin")),
            "--json",
            "--color",
            "always",
        ],
        &[("TERM", "xterm-256color")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains('\x1b'));
    assert!(
        !stdout.contains("secret"),
        "signed query values are redacted"
    );
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(events.iter().all(|e| e["v"] == 1 && e["time_ms"].is_u64()));
    let names: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(names.first(), Some(&"start"));
    assert!(names.contains(&"response"));
    let last = events.last().unwrap();
    assert_eq!(last["event"], "complete");
    assert_eq!(last["bytes"], SIZE as u64);
    assert_eq!(last["checksum_verified"], false);
    assert!(out.stderr.is_empty(), "JSON mode keeps stderr quiet");
}

#[test]
fn json_errors_carry_category_and_exit_code() {
    let server = Server::start(|_| Reply::new(403).header("Content-Length", "0"));
    let dir = tempdir();
    let out = vayu(dir.path(), &[&server.url("/f"), "--json"]);
    assert_eq!(out.status.code(), Some(4));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let last: serde_json::Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["event"], "error");
    assert_eq!(last["category"], "remote");
    assert_eq!(last["exit_code"], 4);
}

#[test]
fn color_and_quiet_modes() {
    let server = Server::start(|_| Reply::full(b"data"));
    let dir = tempdir();

    let out = vayu_env(
        dir.path(),
        &[&server.url("/a"), "--color", "always"],
        &[("TERM", "xterm"), ("NO_COLOR", "1")],
    );
    assert!(out.status.success());
    assert!(
        stderr(&out).contains("\x1b["),
        "--color always overrides NO_COLOR"
    );

    let out = vayu(dir.path(), &[&server.url("/b"), "--quiet"]);
    assert!(out.status.success());
    assert!(out.stderr.is_empty(), "{}", stderr(&out));

    let out = vayu(dir.path(), &[&server.url("/c"), "--ui", "off"]);
    assert!(out.status.success());
    assert_eq!(stderr(&out).lines().count(), 1, "{}", stderr(&out));
}

#[test]
fn rate_limit_is_respected() {
    let body = payload(64 * 1024);
    let server = Server::start(move |_| Reply::full(&body));
    let dir = tempdir();
    let started = std::time::Instant::now();
    let out = vayu(dir.path(), &[&server.url("/r.bin"), "--limit-rate", "32K"]);
    assert!(out.status.success(), "{}", stderr(&out));
    // 64 KiB at 32 KiB/s with a one-second burst allowance: at least ~1 s.
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn sigint_stops_safely_and_keeps_partial_data() {
    let body = payload(SIZE);
    let server = Server::start(move |_| {
        let mut reply = Reply::full(&body);
        reply.trickle = Some(Duration::from_millis(20));
        reply
    });
    let dir = tempdir();
    let child = command(dir.path(), &[&server.url("/slow.bin")], &[])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("stopped: interrupted"), "{err}");
    let part = dir.path().join("slow.bin.part");
    let saved = std::fs::read(&part).unwrap();
    assert!(!saved.is_empty() && saved.len() < SIZE);
    assert_eq!(
        &saved[..],
        &payload(SIZE)[..saved.len()],
        "saved bytes are a valid prefix"
    );
}

#[test]
fn unknown_length_and_empty_files() {
    let server = Server::start(|req| match req.path.as_str() {
        "/empty" => Reply::full(b""),
        _ => Reply::new(200).body(b"streamed without a length".to_vec()),
    });
    let dir = tempdir();
    let out = vayu(dir.path(), &[&server.url("/empty")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("empty")).unwrap(), b"");

    let out = vayu(dir.path(), &[&server.url("/stream")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("size unknown"));
    assert_eq!(
        std::fs::read(dir.path().join("stream")).unwrap(),
        b"streamed without a length"
    );
}
