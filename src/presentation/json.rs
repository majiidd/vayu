//! Versioned JSON Lines events on stdout for scripts.
//!
//! Every line is one object with `"v": 1`, an `"event"` name, and `"time_ms"` (Unix epoch
//! milliseconds). URLs are redacted. Progress is emitted at most once per second while bytes
//! are flowing. If stdout is closed, the download stops (exit code 141) and partial data is kept.

use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::Outcome;
use crate::metrics::{Event, Phase, Snapshot};
use crate::scheduler::{Stop, StopReason};

pub const VERSION: u32 = 1;
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

pub struct Json {
    stop: Stop,
    last_progress: Option<Instant>,
    last_received: Option<u64>,
    dead: bool,
}

impl Json {
    pub fn new(stop: Stop) -> Json {
        Json {
            stop,
            last_progress: None,
            last_received: None,
            dead: false,
        }
    }

    pub fn event(&mut self, event: &Event) {
        let value = match event {
            Event::Started { url } => json!({"event": "start", "url": url}),
            Event::Redirected { to } => json!({"event": "redirect", "url": to}),
            Event::Responded {
                output,
                total,
                http_version,
                resumable,
            } => json!({
                "event": "response",
                "output": output.display().to_string(),
                "total_bytes": total,
                "http_version": http_version,
                "resumable": resumable,
            }),
            Event::Retrying {
                attempt,
                delay,
                reason,
            } => json!({
                "event": "retry",
                "attempt": attempt,
                "delay_ms": delay.as_millis() as u64,
                "reason": reason,
            }),
            Event::Resumed { offset } => json!({"event": "resume", "offset": offset}),
            Event::Recovered {
                output,
                saved,
                total,
                discarded,
            } => json!({
                "event": "recover",
                "output": output.display().to_string(),
                "saved_bytes": saved,
                "total_bytes": total,
                "discarded_bytes": discarded,
            }),
            Event::Restarted { reason } => json!({"event": "restart", "reason": reason}),
            Event::Verifying => json!({"event": "verify", "algorithm": "sha256"}),
            Event::Finalizing => json!({"event": "finalize"}),
            Event::Notice { message } => json!({"event": "notice", "message": message}),
        };
        self.emit(value);
    }

    pub fn tick(&mut self, snap: &Snapshot) {
        if snap.phase != Phase::Downloading
            || self.last_received == Some(snap.received)
            || self
                .last_progress
                .is_some_and(|t| t.elapsed() < PROGRESS_EVERY)
        {
            return;
        }
        self.last_progress = Some(Instant::now());
        self.last_received = Some(snap.received);
        self.emit(json!({
            "event": "progress",
            "received_bytes": snap.received,
            "saved_bytes": snap.saved,
            "total_bytes": snap.total,
            "speed_bps": snap.speed.round() as u64,
            "eta_ms": snap.eta().map(|d| d.as_millis() as u64),
            "elapsed_ms": snap.elapsed.as_millis() as u64,
            "connections": snap.connections,
        }));
    }

    pub fn finish(&mut self, outcome: &Outcome, snap: &Snapshot) {
        let value = match outcome {
            Outcome::Complete(done) => json!({
                "event": "complete",
                "output": done.path.display().to_string(),
                "bytes": done.bytes,
                "elapsed_ms": done.elapsed.as_millis() as u64,
                "sha256": crate::integrity::hex(&done.sha256),
                "checksum_verified": done.verified,
            }),
            Outcome::Failed { failure, partial } => json!({
                "event": "error",
                "category": failure.category.name(),
                "exit_code": failure.category.exit_code(),
                "message": failure.message,
                "hint": failure.hint,
                "partial": partial.as_ref().map(|p| p.display().to_string()),
                "received_bytes": snap.received,
            }),
        };
        self.emit(value);
    }

    fn emit(&mut self, mut value: Value) {
        if self.dead {
            return;
        }
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if let Value::Object(map) = &mut value {
            map.insert("v".into(), VERSION.into());
            map.insert("time_ms".into(), time_ms.into());
        }
        let mut line = value.to_string();
        line.push('\n');
        let mut stdout = std::io::stdout().lock();
        if stdout
            .write_all(line.as_bytes())
            .and_then(|_| stdout.flush())
            .is_err()
        {
            self.dead = true;
            self.stop.request(StopReason::OutputClosed);
        }
    }
}
