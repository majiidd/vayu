//! Segmented transfers: adaptive extra connections, validation of every partial response, safe
//! fallback to one stream, and recovery of saved progress with gaps.

mod common;

use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;

const MIB: usize = 1 << 20;
const ETAG: &str = "\"v1\"";

/// One test at a time: these tests check that measured gains add connections, and concurrent
/// throttled downloads on a small CI runner leave no CPU to measure them with.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Run with frequent checkpoints.
fn run(dir: &Path, args: &[&str]) -> Output {
    let config = dir.join("fast-checkpoints.toml");
    std::fs::write(&config, "checkpoint-interval = \"50ms\"\n").unwrap();
    vayu_env(dir, args, &[("VAYU_CONFIG", config.to_str().unwrap())])
}

/// A range-capable server with a strong validator that sends each response at `rate` bytes per
/// second, like a host that limits every connection.
fn per_connection(body: &[u8], rate: u64) -> Server {
    let served = body.to_vec();
    Server::start(move |req| {
        Reply::ranged(&served, req)
            .header("ETag", ETAG)
            .header("Accept-Ranges", "bytes")
            .rate(rate)
    })
}

fn json_events(out: &Output) -> Vec<serde_json::Value> {
    String::from_utf8(out.stdout.clone())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn throttled_connections_are_added_and_the_file_matches() {
    let _serial = serial();
    let body = payload(16 * MIB);
    let server = per_connection(&body, 2 * MIB as u64);
    let dir = tempdir();
    let started = Instant::now();

    let out = run(dir.path(), &[&server.url("/big.bin"), "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("big.bin")).unwrap(), body);
    assert_eq!(files(dir.path()), ["big.bin", "fast-checkpoints.toml"]);

    let requests = server.requests();
    assert_eq!(
        requests[0].header("range"),
        None,
        "the first request is plain"
    );
    let ranged: Vec<_> = requests.iter().filter_map(|r| r.range()).collect();
    assert!(!ranged.is_empty(), "{requests:?}");
    for req in &requests[1..] {
        assert_eq!(req.header("if-range"), Some(ETAG), "{req:?}");
        let (start, end) = req.range().unwrap();
        assert_eq!(start % MIB, 0, "splits fall on block boundaries: {req:?}");
        assert!(
            end.is_some(),
            "extra connections ask for closed ranges: {req:?}"
        );
    }
    assert!(
        server.peak_concurrency() >= 2,
        "{}",
        server.peak_concurrency()
    );
    assert!(server.peak_concurrency() <= 8);

    let events = json_events(&out);
    let most = events
        .iter()
        .filter(|e| e["event"] == "progress")
        .filter_map(|e| e["connections"].as_u64())
        .max()
        .unwrap();
    assert!(most >= 2, "progress reports the connections: {most}");
    assert_eq!(events.last().unwrap()["event"], "complete");
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "faster than one connection alone (8 s): {:?}",
        started.elapsed()
    );
}

#[test]
fn max_connections_one_keeps_a_single_stream() {
    let _serial = serial();
    let body = payload(6 * MIB);
    let server = per_connection(&body, 4 * MIB as u64);
    let dir = tempdir();

    let out = run(
        dir.path(),
        &[&server.url("/one.bin"), "--max-connections", "1"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("one.bin")).unwrap(), body);
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].header("range"), None);
}

#[test]
fn max_connections_one_overrides_a_configured_minimum() {
    let _serial = serial();
    let body = payload(6 * MIB);
    let server = per_connection(&body, 4 * MIB as u64);
    let dir = tempdir();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "min-connections = 4\n").unwrap();

    let out = vayu_env(
        dir.path(),
        &[&server.url("/one.bin"), "--max-connections", "1"],
        &[("VAYU_CONFIG", config.to_str().unwrap())],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("one.bin")).unwrap(), body);
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
}

#[test]
fn min_connections_are_kept_when_more_do_not_help() {
    let _serial = serial();
    let body = payload(12 * MIB);
    let served = body.clone();
    // One budget for all connections: extra connections never raise throughput.
    let budget = SharedRate::new(4 * MIB as u64);
    let server = Server::start(move |req| {
        Reply::ranged(&served, req)
            .header("ETag", ETAG)
            .header("Accept-Ranges", "bytes")
            .shared(&budget)
    });
    let dir = tempdir();

    let out = run(
        dir.path(),
        &[&server.url("/min.bin"), "--min-connections", "3"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("min.bin")).unwrap(), body);
    assert!(
        server.peak_concurrency() >= 3,
        "{}",
        server.peak_concurrency()
    );
}

#[test]
fn a_server_that_ignores_ranges_is_served_by_one_stream() {
    let _serial = serial();
    let body = payload(12 * MIB);
    let served = body.clone();
    // Claims range support but always sends everything.
    let server = Server::start(move |_| {
        Reply::full(&served)
            .header("ETag", ETAG)
            .header("Accept-Ranges", "bytes")
            .rate(2 * MIB as u64)
    });
    let dir = tempdir();

    let out = run(dir.path(), &[&server.url("/plain.bin")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("plain.bin")).unwrap(), body);
    let err = stderr(&out);
    assert!(err.contains("continuing with one connection"), "{err}");
    assert!(
        !err.contains("restart"),
        "the first stream kept its work: {err}"
    );
    let requests = server.requests();
    assert!(
        requests.iter().filter(|r| r.range().is_some()).count() <= 1,
        "segmentation stops after the first ignored range: {requests:?}"
    );
}

#[test]
fn without_a_validator_no_range_is_requested() {
    let _serial = serial();
    let body = payload(8 * MIB);
    let served = body.clone();
    let server = Server::start(move |req| Reply::ranged(&served, req).rate(4 * MIB as u64));
    let dir = tempdir();

    let out = run(dir.path(), &[&server.url("/nv.bin")]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("nv.bin")).unwrap(), body);
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].header("range"), None);
}

#[test]
fn a_checksum_allows_segments_without_a_validator() {
    let _serial = serial();
    let body = payload(10 * MIB);
    let served = body.clone();
    let server = Server::start(move |req| Reply::ranged(&served, req).rate(2 * MIB as u64));
    let dir = tempdir();
    let checksum = format!("sha256:{}", sha256_hex(&body));

    let out = run(
        dir.path(),
        &[&server.url("/ck.bin"), "--checksum", &checksum],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("ck.bin")).unwrap(), body);
    assert!(stderr(&out).contains("(verified)"));
    let requests = server.requests();
    assert!(requests.iter().any(|r| r.range().is_some()), "{requests:?}");
    assert!(requests.iter().all(|r| r.header("if-range").is_none()));
}

#[test]
fn a_changed_source_seen_by_a_helper_is_a_conflict_that_keeps_data() {
    let _serial = serial();
    let body = payload(12 * MIB);
    let served = body.clone();
    let server = Server::start(move |req| match req.range() {
        None => Reply::full(&served)
            .header("ETag", ETAG)
            .rate(2 * MIB as u64),
        // A broken server ignores If-Range and serves another version.
        Some(_) => Reply::ranged(&served, req).header("ETag", "\"v2\""),
    });
    let dir = tempdir();

    let out = run(dir.path(), &[&server.url("/chg.bin")]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("changed on the server"),
        "{}",
        stderr(&out)
    );
    let names = files(dir.path());
    assert!(names.contains(&"chg.bin.part".to_string()), "{names:?}");
    assert!(names.contains(&"chg.bin.vayu".to_string()), "{names:?}");
    let part = std::fs::read(dir.path().join("chg.bin.part")).unwrap();
    assert!(!part.is_empty());
    assert_eq!(part, body[..part.len()], "only data from the first version");
}

#[test]
fn busy_or_refusing_helpers_leave_the_lead_to_finish() {
    let _serial = serial();
    for (status, extra) in [(503, Some(("Retry-After", "1"))), (403, None)] {
        let body = payload(12 * MIB);
        let served = body.clone();
        let server = Server::start(move |req| match req.range() {
            None => Reply::full(&served)
                .header("ETag", ETAG)
                .header("Accept-Ranges", "bytes")
                .rate(2 * MIB as u64),
            Some(_) => {
                let reply = Reply::new(status).header("Content-Length", "0");
                match extra {
                    Some((name, value)) => reply.header(name, value),
                    None => reply,
                }
            }
        });
        let dir = tempdir();

        let out = run(dir.path(), &[&server.url("/busy.bin")]);
        assert!(out.status.success(), "HTTP {status}: {}", stderr(&out));
        assert_eq!(std::fs::read(dir.path().join("busy.bin")).unwrap(), body);
        let range_requests = server
            .requests()
            .iter()
            .filter(|r| r.range().is_some())
            .count();
        assert!(range_requests >= 1, "HTTP {status}: a helper was tried");
        assert!(
            range_requests <= 3,
            "HTTP {status}: backs off: {range_requests}"
        );
    }
}

#[test]
fn the_rate_limit_applies_to_all_connections_together() {
    let _serial = serial();
    let body = payload(6 * MIB);
    let server = per_connection(&body, MIB as u64);
    let dir = tempdir();
    let started = Instant::now();

    let out = run(dir.path(), &[&server.url("/lim.bin"), "--limit-rate", "2M"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("lim.bin")).unwrap(), body);
    // 6 MiB at 2 MiB/s, less the one-second burst allowance.
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// A range-capable server limited to 1 MiB/s per connection until the flag is cleared.
fn throttled_until_released(body: &[u8]) -> (Server, Arc<AtomicBool>) {
    let served = body.to_vec();
    let slow = Arc::new(AtomicBool::new(true));
    let throttle = slow.clone();
    let server = Server::start(move |req| {
        let reply = Reply::ranged(&served, req).header("ETag", ETAG);
        if throttle.load(Ordering::SeqCst) {
            reply.rate(MIB as u64)
        } else {
            reply
        }
    });
    (server, slow)
}

/// Saved ranges in a state file.
fn saved_ranges(state: &Path) -> Option<Vec<(u64, u64)>> {
    let text = std::fs::read_to_string(state).ok()?;
    let (_, body) = text.split_once('\n')?;
    let body: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(
        body["ranges"]
            .as_array()?
            .iter()
            .map(|r| (r[0].as_u64().unwrap(), r[1].as_u64().unwrap()))
            .collect(),
    )
}

#[test]
fn kill_9_during_a_segmented_download_resumes_every_gap() {
    let _serial = serial();
    let body = payload(24 * MIB);
    let (server, slow) = throttled_until_released(&body);
    let dir = tempdir();
    let url = server.url("/gaps.bin");
    let state = dir.path().join("gaps.bin.vayu");

    let config = dir.path().join("fast-checkpoints.toml");
    std::fs::write(&config, "checkpoint-interval = \"50ms\"\n").unwrap();
    let mut child = command(
        dir.path(),
        &[&url],
        &[("VAYU_CONFIG", config.to_str().unwrap())],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ranges = saved_ranges(&state).unwrap_or_default();
        if ranges.len() >= 2 && ranges.iter().all(|r| r.1 > r.0) {
            break;
        }
        assert!(Instant::now() < deadline, "no gaps were saved: {ranges:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();

    let saved = saved_ranges(&state).unwrap();
    assert!(saved.len() >= 2, "{saved:?}");
    assert!(saved.iter().all(|&(s, _)| s % MIB as u64 == 0), "{saved:?}");
    let before = server.requests().len();

    slow.store(false, Ordering::SeqCst);
    let out = run(dir.path(), &[&url, "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("gaps.bin")).unwrap(), body);
    assert_eq!(files(dir.path()), ["fast-checkpoints.toml", "gaps.bin"]);

    let events = json_events(&out);
    let recover = events.iter().find(|e| e["event"] == "recover").unwrap();
    let saved_bytes: u64 = saved.iter().map(|(s, e)| e - s).sum();
    assert_eq!(recover["saved_bytes"], saved_bytes);
    for req in &server.requests()[before..] {
        let (start, _) = req.range().expect("the rerun only asks for ranges");
        let start = start as u64;
        assert!(
            !saved.iter().any(|&(s, e)| s <= start && start < e),
            "{start} was already saved in {saved:?}"
        );
    }
}

#[test]
fn sigint_during_a_segmented_download_saves_every_connection() {
    let _serial = serial();
    let body = payload(24 * MIB);
    let (server, slow) = throttled_until_released(&body);
    let dir = tempdir();
    let url = server.url("/int.bin");
    let state = dir.path().join("int.bin.vayu");

    let config = dir.path().join("fast-checkpoints.toml");
    std::fs::write(&config, "checkpoint-interval = \"5s\"\n").unwrap();
    let child = command(
        dir.path(),
        &[&url],
        &[("VAYU_CONFIG", config.to_str().unwrap())],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    // Wait until a helper is receiving, then stop well before the next periodic checkpoint.
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.peak_concurrency() < 2 {
        assert!(Instant::now() < deadline, "no helper connection was added");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(500));
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130), "{}", stderr(&out));

    let saved = saved_ranges(&state).unwrap();
    assert!(
        saved.len() >= 2,
        "every connection's data was checkpointed: {saved:?}"
    );
    let len = std::fs::metadata(dir.path().join("int.bin.part"))
        .unwrap()
        .len();
    assert_eq!(
        saved.last().unwrap().1,
        len,
        "nothing written after the last checkpoint"
    );

    slow.store(false, Ordering::SeqCst);
    let out = run(dir.path(), &[&url, "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read(dir.path().join("int.bin")).unwrap(), body);
    let events = json_events(&out);
    let recover = events.iter().find(|e| e["event"] == "recover").unwrap();
    assert_eq!(recover["discarded_bytes"], 0);
}
