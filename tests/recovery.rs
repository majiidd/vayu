//! Durable recovery: interrupted runs, crashes at each checkpoint and finalization step,
//! replacement URLs, source changes, and damaged saved data.

mod common;

use std::os::unix::fs::PermissionsExt;
#[cfg(debug_assertions)]
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use common::*;

const SIZE: usize = 256 * 1024;
const ETAG: &str = "\"v1\"";

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Run with frequent checkpoints so short tests have several of them.
fn run(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let config = dir.join("fast-checkpoints.toml");
    std::fs::write(&config, "checkpoint-interval = \"50ms\"\n").unwrap();
    let config = config.to_str().unwrap().to_string();
    let mut env = env.to_vec();
    env.push(("VAYU_CONFIG", &config));
    vayu_env(dir, args, &env)
}

/// Bytes claimed by the state file's saved prefix.
fn saved_prefix(state: &Path) -> u64 {
    try_saved_prefix(state).unwrap_or_else(|| panic!("no valid state at {}", state.display()))
}

fn try_saved_prefix(state: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(state).ok()?;
    let (header, body) = text.split_once('\n')?;
    assert!(header.starts_with("vayu-state 1 "), "{header}");
    let body: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(
        body["ranges"]
            .get(0)
            .map(|r| r[1].as_u64().unwrap())
            .unwrap_or(0),
    )
}

/// Wait until a running download has checkpointed some data, instead of guessing a delay.
fn wait_for_progress(state: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while try_saved_prefix(state).unwrap_or(0) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no progress was saved at {}",
            state.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn files_of(dir: &Path) -> Vec<String> {
    files(dir)
        .into_iter()
        .filter(|n| !n.ends_with(".toml"))
        .collect()
}

/// The first request is cut short; range requests that carry the right validator succeed.
fn flaky_server(body: &[u8], cut: usize) -> Server {
    let served = body.to_vec();
    Server::start(move |req| match req.range_start() {
        None if req.attempt == 1 => Reply::full(&served).header("ETag", ETAG).cut_after(cut),
        None => Reply::full(&served).header("ETag", ETAG),
        Some(start) if req.header("if-range") == Some(ETAG) => {
            Reply::partial(&served, start).header("ETag", ETAG)
        }
        Some(_) => Reply::full(&served).header("ETag", ETAG),
    })
}

/// A first run that loses its connection halfway and gives up, leaving saved progress.
fn interrupted_run(dir: &Path, url: &str) -> u64 {
    let out = run(dir, &[url, "--retry", "0"], &[]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("progress is saved"), "{err}");
    let name = url.rsplit('/').next().unwrap().split('?').next().unwrap();
    saved_prefix(&dir.join(format!("{name}.vayu")))
}

fn trickling(body: &[u8], delay: Duration) -> Server {
    let served = body.to_vec();
    Server::start(move |req| match req.range_start() {
        None => {
            let mut reply = Reply::full(&served)
                .header("ETag", ETAG)
                .header("Accept-Ranges", "bytes");
            reply.trickle = Some(delay);
            reply
        }
        Some(start) => Reply::partial(&served, start).header("ETag", ETAG),
    })
}

fn spawn(dir: &Path, args: &[&str]) -> std::process::Child {
    let config = dir.join("fast-checkpoints.toml");
    std::fs::write(&config, "checkpoint-interval = \"50ms\"\n").unwrap();
    command(dir, args, &[("VAYU_CONFIG", config.to_str().unwrap())])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn sigint_then_rerun_resumes_at_the_saved_offset() {
    let body = payload(SIZE);
    let server = trickling(&body, Duration::from_millis(20));
    let dir = tempdir();
    let url = server.url("/slow.bin?token=secret");

    let child = spawn(dir.path(), &[&url]);
    wait_for_progress(&dir.path().join("slow.bin.vayu"));
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("run the same command again to resume"),
        "{}",
        stderr(&out)
    );

    let state = dir.path().join("slow.bin.vayu");
    let part = dir.path().join("slow.bin.part");
    let saved = saved_prefix(&state);
    assert!(saved > 0 && saved < SIZE as u64);
    assert_eq!(
        saved,
        std::fs::metadata(&part).unwrap().len(),
        "a graceful stop checkpoints everything it wrote"
    );
    let mode = std::fs::metadata(&state).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "state files are private");
    let text = std::fs::read_to_string(&state).unwrap();
    assert!(
        !text.contains("secret"),
        "signed query values are not stored"
    );

    let out = run(dir.path(), &[&url], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("slow.bin")).unwrap(), body);
    assert_eq!(files_of(dir.path()), ["slow.bin"]);
    let resumed = server.requests().last().unwrap().clone();
    assert_eq!(resumed.range_start(), Some(saved as usize));
    assert_eq!(resumed.header("if-range"), Some(ETAG));
    assert!(stderr(&out).contains("resuming"), "{}", stderr(&out));
}

#[test]
fn kill_9_then_rerun_resumes_from_the_last_checkpoint() {
    let body = payload(SIZE);
    let server = trickling(&body, Duration::from_millis(20));
    let dir = tempdir();
    let url = server.url("/k.bin");

    let mut child = spawn(dir.path(), &[&url]);
    wait_for_progress(&dir.path().join("k.bin.vayu"));
    child.kill().unwrap();
    child.wait().unwrap();

    let saved = saved_prefix(&dir.path().join("k.bin.vayu"));
    let on_disk = std::fs::metadata(dir.path().join("k.bin.part"))
        .unwrap()
        .len();
    assert!(saved > 0, "checkpoints were published while downloading");
    assert!(saved <= on_disk, "no checkpoint claims unwritten data");

    let out = run(dir.path(), &[&url, "--json"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("k.bin")).unwrap(), body);
    assert_eq!(files_of(dir.path()), ["k.bin"]);
    assert_eq!(
        server.requests().last().unwrap().range_start(),
        Some(saved as usize)
    );

    let events: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let recover = events.iter().find(|e| e["event"] == "recover").unwrap();
    assert_eq!(recover["saved_bytes"], saved);
    assert_eq!(recover["discarded_bytes"], on_disk - saved);
    assert_eq!(events.last().unwrap()["event"], "complete");
}

#[cfg(debug_assertions)]
#[test]
fn crash_at_each_checkpoint_step_recovers() {
    for point in [
        "checkpoint-synced@4",
        "checkpoint-tmp-written@4",
        "checkpoint-published@4",
    ] {
        let body = payload(SIZE);
        let server = trickling(&body, Duration::from_millis(5));
        let dir = tempdir();
        let url = server.url("/c.bin");

        let out = run(dir.path(), &[&url], &[("VAYU_FAULT", point)]);
        assert_eq!(
            out.status.signal(),
            Some(libc::SIGABRT),
            "{point}: {}",
            stderr(&out)
        );
        let saved = saved_prefix(&dir.path().join("c.bin.vayu"));
        let on_disk = std::fs::metadata(dir.path().join("c.bin.part"))
            .unwrap()
            .len();
        assert!(saved <= on_disk, "{point}: claims {saved} of {on_disk}");
        assert!(saved > 0, "{point}: earlier checkpoints claimed data");

        let out = run(dir.path(), &[&url], &[]);
        assert!(out.status.success(), "{point}: {}", stderr(&out));
        assert_eq!(std::fs::read(dir.path().join("c.bin")).unwrap(), body);
        assert_eq!(files_of(dir.path()), ["c.bin"], "{point}");
        let last = server.requests().last().unwrap().clone();
        assert_eq!(last.range_start().unwrap_or(0), saved as usize, "{point}");
    }
}

#[cfg(debug_assertions)]
#[test]
fn crash_during_finalization_finishes_without_downloading_again() {
    for point in [
        "finalize-before-link",
        "finalize-linked",
        "finalize-part-removed",
    ] {
        let body = payload(SIZE);
        let served = body.clone();
        let server = Server::start(move |_| Reply::full(&served).header("ETag", ETAG));
        let dir = tempdir();
        let url = server.url("/f.bin");
        let checksum = format!("sha256:{}", sha256_hex(&body));

        let out = run(
            dir.path(),
            &[&url, "--checksum", &checksum],
            &[("VAYU_FAULT", point)],
        );
        assert_eq!(out.status.signal(), Some(libc::SIGABRT), "{point}");
        assert!(dir.path().join("f.bin.vayu").exists(), "{point}");

        let requests = server.requests().len();
        let out = run(dir.path(), &[&url], &[]);
        assert!(out.status.success(), "{point}: {}", stderr(&out));
        assert_eq!(server.requests().len(), requests, "{point}: no new request");
        assert_eq!(std::fs::read(dir.path().join("f.bin")).unwrap(), body);
        assert_eq!(files_of(dir.path()), ["f.bin"], "{point}");
        let err = stderr(&out);
        assert!(
            err.contains("(verified)"),
            "{point}: the saved checksum still gates publication: {err}"
        );
        assert_eq!(
            err.contains("already complete"),
            point != "finalize-before-link",
            "{point}: once linked, only cleanup remains: {err}"
        );
    }
}

#[test]
fn replacement_url_continues_with_resume_from() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| {
        let fresh = req.path.ends_with("sig=new");
        match req.range_start() {
            None if !fresh && req.attempt == 1 => Reply::full(&served)
                .header("ETag", ETAG)
                .cut_after(SIZE / 2),
            Some(start) if fresh && req.header("if-range") == Some(ETAG) => {
                Reply::partial(&served, start).header("ETag", ETAG)
            }
            // The old signed link has expired.
            _ if !fresh => Reply::new(403).header("Content-Length", "0"),
            _ => Reply::full(&served).header("ETag", ETAG),
        }
    });
    let dir = tempdir();
    let old = server.url("/file.bin?sig=old");
    let new = server.url("/file.bin?sig=new");

    let saved = interrupted_run(dir.path(), &old);
    assert_eq!(saved, SIZE as u64 / 2);

    let out = run(dir.path(), &[&old], &[]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("--resume-from"), "{err}");
    assert!(err.contains("file.bin.vayu"), "{err}");

    let out = run(dir.path(), &[&new], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(stderr(&out).contains("different URL"), "{}", stderr(&out));
    assert_eq!(saved_prefix(&dir.path().join("file.bin.vayu")), saved);

    let out = run(dir.path(), &[&new, "--resume-from", "file.bin.vayu"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
    assert_eq!(files_of(dir.path()), ["file.bin"]);
    let last = server.requests().last().unwrap().clone();
    assert!(last.path.ends_with("sig=new"));
    assert_eq!(last.range_start(), Some(SIZE / 2));
}

#[test]
fn resume_from_keeps_the_saved_destination() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| match req.range_start() {
        None if req.path.starts_with("/file.bin") && req.attempt == 1 => Reply::full(&served)
            .header("ETag", ETAG)
            .cut_after(SIZE / 2),
        None => Reply::full(&served).header("ETag", ETAG),
        Some(start) if req.header("if-range") == Some(ETAG) => {
            Reply::partial(&served, start).header("ETag", ETAG)
        }
        Some(_) => Reply::full(&served).header("ETag", ETAG),
    });
    let fresh = server.url("/fresh-link?sig=new");

    // Verified progress continues into the saved destination.
    let dir = tempdir();
    interrupted_run(dir.path(), &server.url("/file.bin?sig=a"));
    let out = run(dir.path(), &[&fresh, "--resume-from", "file.bin.vayu"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(files_of(dir.path()), ["file.bin"]);
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
    assert_eq!(
        server.requests().last().unwrap().range_start(),
        Some(SIZE / 2)
    );

    // Saved data that fails verification is downloaded again, still into that destination,
    // and the user is told.
    let dir = tempdir();
    interrupted_run(dir.path(), &server.url("/file.bin?sig=b"));
    let part = dir.path().join("file.bin.part");
    let mut data = std::fs::read(&part).unwrap();
    data[0] ^= 0xff;
    std::fs::write(&part, &data).unwrap();
    let out = run(dir.path(), &[&fresh, "--resume-from", "file.bin.vayu"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("could not be verified"),
        "{}",
        stderr(&out)
    );
    assert_eq!(files_of(dir.path()), ["file.bin"]);
    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), body);
    assert_eq!(server.requests().last().unwrap().header("range"), None);
}

#[test]
fn checkpoint_interval_has_a_floor() {
    let dir = tempdir();
    let config = dir.path().join("tiny.toml");
    for (value, expect) in [("1ms", "at least 10ms"), ("0s", "invalid duration")] {
        std::fs::write(&config, format!("checkpoint-interval = \"{value}\"\n")).unwrap();
        let out = vayu_env(
            dir.path(),
            &["http://127.0.0.1:9/x"],
            &[("VAYU_CONFIG", config.to_str().unwrap())],
        );
        assert_eq!(out.status.code(), Some(2), "{value}: {}", stderr(&out));
        assert!(stderr(&out).contains(expect), "{value}: {}", stderr(&out));
    }
}

#[test]
fn resume_from_validates_its_arguments() {
    let dir = tempdir();
    let url = "http://127.0.0.1:9/x.bin";
    let out = run(dir.path(), &[url, "--resume-from", "missing.vayu"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("no saved progress"));

    let body = payload(SIZE);
    let server = flaky_server(&body, SIZE / 2);
    interrupted_run(dir.path(), &server.url("/x.bin"));
    let out = run(
        dir.path(),
        &[url, "--resume-from", "x.bin", "-o", "other.bin"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("does not match"));
}

#[test]
fn source_change_between_runs_is_a_conflict_that_keeps_data() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| match req.attempt {
        1 => Reply::full(&served)
            .header("ETag", ETAG)
            .cut_after(SIZE / 2),
        // A new version: If-Range does not match, so the server sends all of it.
        _ => Reply::full(&payload(SIZE + 7)).header("ETag", "\"v2\""),
    });
    let dir = tempdir();
    let url = server.url("/d.bin");
    let saved = interrupted_run(dir.path(), &url);

    let out = run(dir.path(), &[&url], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(stderr(&out).contains("changed on the server"));
    assert_eq!(server.requests()[1].header("if-range"), Some(ETAG));
    let part = std::fs::read(dir.path().join("d.bin.part")).unwrap();
    assert_eq!(part, &body[..saved as usize]);
    assert_eq!(saved_prefix(&dir.path().join("d.bin.vayu")), saved);
}

/// A first run that saves `saved` bytes of `body` under [`ETAG`], then a server that sends `copy`
/// under another ETag, as a mirror with its own copy does.
fn another_copy(body: &[u8], saved: usize, copy: Vec<u8>) -> Server {
    let served = body.to_vec();
    Server::start(move |req| match req.attempt {
        1 => Reply::full(&served).header("ETag", ETAG).cut_after(saved),
        _ => Reply::ranged(&copy, req).header("ETag", "\"mirror\""),
    })
}

#[test]
fn another_copy_of_the_same_file_is_verified_and_resumed() {
    const MIB: usize = 1 << 20;
    let body = payload(6 * MIB);
    let server = another_copy(&body, 4 * MIB + 100, body.clone());
    let dir = tempdir();
    let url = server.url("/m.bin");
    let saved = interrupted_run(dir.path(), &url);

    let out = run(dir.path(), &[&url, "--max-connections", "1"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("another copy of the file"),
        "{}",
        stderr(&out)
    );
    assert_eq!(std::fs::read(dir.path().join("m.bin")).unwrap(), body);
    let requests = server.requests();
    // The If-Range miss, four full blocks and the partial prefix checked, then the rest.
    let last = requests.last().unwrap();
    assert_eq!(last.range_start(), Some(saved as usize), "{requests:?}");
    assert_eq!(last.header("if-range"), Some("\"mirror\""));
    assert_eq!(requests.len(), 8, "{requests:?}");
}

#[test]
fn another_copy_with_other_bytes_at_the_same_size_is_a_change() {
    const MIB: usize = 1 << 20;
    let body = payload(6 * MIB);
    let mut copy = body.clone();
    // Inside the last saved block, which is always checked.
    copy[3 * MIB + 5] ^= 1;
    let server = another_copy(&body, 4 * MIB + 100, copy);
    let dir = tempdir();
    let url = server.url("/n.bin");
    let saved = interrupted_run(dir.path(), &url);

    let out = run(dir.path(), &[&url, "--max-connections", "1"], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(stderr(&out).contains("changed on the server"));
    assert_eq!(saved_prefix(&dir.path().join("n.bin.vayu")), saved);
}

#[test]
fn another_copy_cannot_change_a_saved_partial_block() {
    const MIB: usize = 1 << 20;
    let body = payload(6 * MIB);
    let mut copy = body.clone();
    copy[MIB + 5] ^= 1;
    copy[5 * MIB + 5] ^= 1;
    let server = another_copy(&body, MIB + 100, copy);
    let dir = tempdir();
    let url = server.url("/partial-change.bin");
    let saved = interrupted_run(dir.path(), &url);
    assert_eq!(saved, (MIB + 100) as u64);
    let out = run(dir.path(), &[&url, "--max-connections", "1"], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(!dir.path().join("partial-change.bin").exists());
    assert_eq!(
        saved_prefix(&dir.path().join("partial-change.bin.vayu")),
        saved
    );
}

#[test]
fn another_copy_cannot_skip_a_saved_complete_block() {
    const MIB: usize = 1 << 20;
    let body = payload(20 * MIB);
    let mut copy = body.clone();
    copy[MIB + 5] ^= 1;
    let server = another_copy(&body, 12 * MIB + 100, copy);
    let dir = tempdir();
    let url = server.url("/omitted-change.bin");
    interrupted_run(dir.path(), &url);
    let out = run(dir.path(), &[&url, "--max-connections", "1"], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(!dir.path().join("omitted-change.bin").exists());
}

#[test]
fn another_copy_with_only_partial_progress_stops_safely() {
    let body = payload(SIZE);
    let server = another_copy(&body, SIZE / 2, body.clone());
    let dir = tempdir();
    let url = server.url("/small-copy.bin");
    interrupted_run(dir.path(), &url);
    let out = run(dir.path(), &[&url, "--max-connections", "1"], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(!dir.path().join("small-copy.bin").exists());
    assert_eq!(
        std::fs::read(dir.path().join("small-copy.bin.part")).unwrap(),
        &body[..SIZE / 2]
    );
}

#[test]
fn another_copy_rate_limiting_does_not_count_as_a_network_stall() {
    const MIB: usize = 1 << 20;
    let body = payload(3 * MIB);
    let server = another_copy(&body, MIB + 100, body.clone());
    let dir = tempdir();
    let url = server.url("/limited-copy.bin");
    interrupted_run(dir.path(), &url);
    let out = run(
        dir.path(),
        &[
            &url,
            "--max-connections",
            "1",
            "--limit-rate",
            "2M",
            "--stall-timeout",
            "200ms",
            "--retry",
            "0",
        ],
        &[],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(dir.path().join("limited-copy.bin")).unwrap(),
        body
    );
}

#[test]
fn another_copy_can_resume_with_parallel_ranges() {
    const MIB: usize = 1 << 20;
    let body = payload(20 * MIB);
    let server = another_copy(&body, 4 * MIB + 100, body.clone());
    let dir = tempdir();
    let url = server.url("/parallel-copy.bin");
    interrupted_run(dir.path(), &url);
    let out = run(
        dir.path(),
        &[&url, "--min-connections", "3", "--max-connections", "3"],
        &[],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(dir.path().join("parallel-copy.bin")).unwrap(),
        body
    );
}

#[test]
fn verification_rejects_excess_body_with_and_without_content_length() {
    const MIB: usize = 1 << 20;
    for declared in [false, true] {
        let body = payload(6 * MIB);
        let served = body.clone();
        let server = Server::start(move |req| {
            if req.attempt == 1 {
                return Reply::full(&served)
                    .header("ETag", ETAG)
                    .cut_after(MIB + 100);
            }
            if let Some((start, Some(end))) = req.range() {
                let mut reply = Reply::new(206)
                    .header("ETag", "\"mirror\"")
                    .header(
                        "Content-Range",
                        format!("bytes {start}-{}/{}", end - 1, served.len()),
                    )
                    .body(vec![0; end - start + 1]);
                if declared {
                    reply = reply.header("Content-Length", (end - start + 1).to_string());
                }
                return reply;
            }
            Reply::ranged(&served, req).header("ETag", "\"mirror\"")
        });
        let dir = tempdir();
        let url = server.url("/excess-copy.bin");
        interrupted_run(dir.path(), &url);
        let out = run(
            dir.path(),
            &[&url, "--max-connections", "1", "--retry", "0"],
            &[],
        );
        assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
        assert!(stderr(&out).contains("verification"), "{}", stderr(&out));
        assert!(!dir.path().join("excess-copy.bin").exists());
    }
}

#[test]
fn verification_requires_the_new_binding_size_and_identity_encoding() {
    const MIB: usize = 1 << 20;
    for invalid in ["binding", "total", "encoding"] {
        let body = payload(6 * MIB);
        let server = Server::start(move |req| {
            if req.attempt == 1 {
                return Reply::full(&body).header("ETag", ETAG).cut_after(MIB + 100);
            }
            if let Some((start, Some(end))) = req.range() {
                let mut reply = Reply::slice(&body, start, end);
                if invalid == "binding" {
                    return reply
                        .header("Last-Modified", "Mon, 01 Jan 2024 00:00:00 GMT")
                        .header("Date", "Tue, 02 Jan 2024 00:00:00 GMT");
                }
                if invalid == "total" {
                    reply.headers.retain(|(name, _)| name != "Content-Range");
                    reply = reply.header(
                        "Content-Range",
                        format!("bytes {start}-{}/{}", end - 1, body.len() + 1),
                    );
                }
                if invalid == "encoding" {
                    reply = reply.header("Content-Encoding", "gzip");
                }
                return reply.header("ETag", "\"mirror\"");
            }
            Reply::ranged(&body, req).header("ETag", "\"mirror\"")
        });
        let dir = tempdir();
        let url = server.url("/invalid-copy.bin");
        interrupted_run(dir.path(), &url);
        let out = run(
            dir.path(),
            &[&url, "--max-connections", "1", "--retry", "0"],
            &[],
        );
        assert_eq!(out.status.code(), Some(3), "{invalid}: {}", stderr(&out));
        assert!(!dir.path().join("invalid-copy.bin").exists());
    }
}

#[test]
fn verification_still_times_out_an_idle_network_read() {
    const MIB: usize = 1 << 20;
    let body = payload(3 * MIB);
    let server = Server::start(move |req| {
        if req.attempt == 1 {
            return Reply::full(&body).header("ETag", ETAG).cut_after(MIB + 100);
        }
        let mut reply = Reply::ranged(&body, req).header("ETag", "\"mirror\"");
        if matches!(req.range(), Some((_, Some(_)))) {
            reply.stall = Some(Duration::from_millis(500));
        }
        reply
    });
    let dir = tempdir();
    let url = server.url("/stalled-copy.bin");
    let saved = interrupted_run(dir.path(), &url);
    let out = run(
        dir.path(),
        &[
            &url,
            "--max-connections",
            "1",
            "--stall-timeout",
            "100ms",
            "--retry",
            "0",
        ],
        &[],
    );
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(!dir.path().join("stalled-copy.bin").exists());
    assert_eq!(
        saved_prefix(&dir.path().join("stalled-copy.bin.vayu")),
        saved
    );
}

#[test]
fn an_error_page_sent_as_ok_is_retried_not_taken_for_a_new_version() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| match req.attempt {
        1 => Reply::full(&served)
            .header("ETag", ETAG)
            .cut_after(SIZE / 2),
        // Some hosts refuse a request with `200 OK` and a short HTML page.
        2 => Reply::full(b"Bad Request").header("Content-Type", "text/html"),
        _ => Reply::ranged(&served, req).header("ETag", ETAG),
    });
    let dir = tempdir();
    let url = server.url("/e.bin");
    let saved = interrupted_run(dir.path(), &url);

    let out = run(dir.path(), &[&url], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("e.bin")).unwrap(), body);
    let resumed = server.requests()[2].range_start();
    assert_eq!(resumed, Some(saved as usize), "{:?}", server.requests());
}

#[test]
fn an_expired_link_answering_with_error_pages_gives_up_and_keeps_data() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| match req.attempt {
        1 => Reply::full(&served)
            .header("ETag", ETAG)
            .cut_after(SIZE / 2),
        _ => Reply::full(b"Bad Request").header("Content-Type", "text/html"),
    });
    let dir = tempdir();
    let url = server.url("/x.bin");
    let saved = interrupted_run(dir.path(), &url);

    let out = run(dir.path(), &[&url, "--retry", "2"], &[]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("not the file"), "{err}");
    assert!(!err.contains("changed on the server"), "{err}");
    assert_eq!(saved_prefix(&dir.path().join("x.bin.vayu")), saved);
}

#[test]
fn server_ignoring_ranges_on_rerun_keeps_saved_progress() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| {
        let reply = Reply::full(&served).header("ETag", ETAG);
        if req.attempt == 1 {
            reply.cut_after(SIZE / 2)
        } else {
            reply
        }
    });
    let dir = tempdir();
    let url = server.url("/r.bin");
    let saved = interrupted_run(dir.path(), &url);

    let out = run(dir.path(), &[&url], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("cannot be continued safely"), "{err}");
    assert!(err.contains("--restart"), "{err}");
    assert_eq!(saved_prefix(&dir.path().join("r.bin.vayu")), saved);

    let out = run(dir.path(), &[&url, "--restart"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("r.bin")).unwrap(), body);
    assert_eq!(server.requests().last().unwrap().header("range"), None);
}

#[test]
fn damaged_state_is_refused_and_data_left_untouched() {
    let body = payload(SIZE);
    let server = flaky_server(&body, SIZE / 2);
    let dir = tempdir();
    let url = server.url("/s.bin");
    interrupted_run(dir.path(), &url);
    let state = dir.path().join("s.bin.vayu");
    let part = std::fs::read(dir.path().join("s.bin.part")).unwrap();
    let original = std::fs::read(&state).unwrap();
    let requests = server.requests().len();

    let mut flipped = original.clone();
    let at = flipped.len() - 20;
    flipped[at] ^= 0x01;
    let newer =
        String::from_utf8(original.clone())
            .unwrap()
            .replacen("vayu-state 1 ", "vayu-state 9 ", 1);
    for (bytes, expect) in [
        (flipped, "damaged"),
        (newer.into_bytes(), "unsupported state format version 9"),
        (vec![b'x'; 9 << 20], "too large"),
    ] {
        std::fs::write(&state, &bytes).unwrap();
        let out = run(dir.path(), &[&url], &[]);
        assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
        assert!(stderr(&out).contains(expect), "{expect}: {}", stderr(&out));
        assert_eq!(std::fs::read(dir.path().join("s.bin.part")).unwrap(), part);
        assert_eq!(std::fs::read(&state).unwrap(), bytes, "state left as found");
    }
    assert_eq!(server.requests().len(), requests, "nothing was requested");
}

#[test]
fn damaged_block_is_downloaded_again() {
    const MIB: usize = 1 << 20;
    let body = payload(3 * MIB + 4321);
    let server = flaky_server(&body, 2 * MIB + MIB / 2);
    let dir = tempdir();
    let url = server.url("/b.bin");
    let saved = interrupted_run(dir.path(), &url);
    assert_eq!(saved, (2 * MIB + MIB / 2) as u64);

    let part = dir.path().join("b.bin.part");
    let mut data = std::fs::read(&part).unwrap();
    data[MIB + MIB / 2] ^= 0xff;
    std::fs::write(&part, &data).unwrap();

    let out = run(dir.path(), &[&url], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("b.bin")).unwrap(), body);
    let err = stderr(&out);
    assert!(err.contains("could not be verified"), "{err}");
    assert_eq!(server.requests().last().unwrap().range_start(), Some(MIB));
}

#[test]
fn progress_without_a_validator_needs_a_checksum() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| match req.range_start() {
        None if req.attempt == 1 => Reply::full(&served).cut_after(SIZE / 2),
        None => Reply::full(&served),
        Some(start) => Reply::partial(&served, start),
    });
    let dir = tempdir();
    let url = server.url("/n.bin");
    let out = run(dir.path(), &[&url, "--retry", "0"], &[]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));

    let out = run(dir.path(), &[&url], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(stderr(&out).contains("--checksum"), "{}", stderr(&out));
    assert_eq!(server.requests().len(), 1, "refused before any request");

    let checksum = format!("sha256:{}", sha256_hex(&body));
    let out = run(dir.path(), &[&url, "--checksum", &checksum], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("n.bin")).unwrap(), body);
    let last = server.requests().last().unwrap().clone();
    assert_eq!(last.range_start(), Some(SIZE / 2));
    assert!(stderr(&out).contains("(verified)"));
}

#[test]
fn server_named_files_are_found_after_the_first_response() {
    let body = payload(SIZE);
    let served = body.clone();
    let server = Server::start(move |req| {
        let named = |r: Reply| {
            r.header("ETag", ETAG)
                .header("Content-Disposition", "attachment; filename=\"named.iso\"")
        };
        match req.range_start() {
            None if req.attempt == 1 => named(Reply::full(&served)).cut_after(SIZE / 4),
            None => named(Reply::full(&served)),
            Some(start) => named(Reply::partial(&served, start)),
        }
    });
    let dir = tempdir();
    let url = server.url("/get?id=7");
    let out = run(dir.path(), &[&url, "--retry", "0"], &[]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert_eq!(files_of(dir.path()), ["named.iso.part", "named.iso.vayu"]);

    let out = run(dir.path(), &[&url], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("named.iso")).unwrap(), body);
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "probe, then one range request");
    assert_eq!(requests[2].range_start(), Some(SIZE / 4));
}

#[test]
fn a_second_process_cannot_take_over_a_running_download() {
    let body = payload(SIZE);
    let server = trickling(&body, Duration::from_millis(20));
    let dir = tempdir();
    let url = server.url("/busy.bin");

    let mut first = spawn(dir.path(), &[&url]);
    wait_for_progress(&dir.path().join("busy.bin.vayu"));
    for extra in [
        &[][..],
        &["--restart"][..],
        &["--resume-from", "busy.bin.vayu"],
    ] {
        let mut args = vec![url.as_str()];
        args.extend_from_slice(extra);
        let out = run(dir.path(), &args, &[]);
        assert_eq!(out.status.code(), Some(7), "{extra:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("another vayu process"),
            "{extra:?}: {}",
            stderr(&out)
        );
    }
    first.kill().unwrap();
    first.wait().unwrap();
    let out = run(dir.path(), &[&url], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("busy.bin")).unwrap(), body);
}

#[test]
fn leftover_part_without_state_still_requires_restart() {
    let server = Server::start(|_| Reply::full(b"fresh"));
    let dir = tempdir();
    std::fs::write(dir.path().join("old.bin.part"), b"from somewhere else").unwrap();
    let out = run(dir.path(), &[&server.url("/old.bin")], &[]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(stderr(&out).contains("without saved progress"));
    assert_eq!(
        std::fs::read(dir.path().join("old.bin.part")).unwrap(),
        b"from somewhere else"
    );
}
