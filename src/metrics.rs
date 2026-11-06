//! Typed transfer statistics shared between the transfer engine and presentation.
//!
//! The engine records bytes in an atomic counter and reports phase changes and discrete events.
//! A sampler turns those into replaceable [`Snapshot`]s on a `watch` channel. Presentation only
//! ever reads snapshots and receives events through a bounded, non-blocking queue, so a slow
//! terminal cannot hold back the transfer.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// One-second throughput samples kept for the sparkline.
pub const HISTORY_LEN: usize = 24;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// Progress is published more often than speed is sampled, so a local dashboard can redraw
/// at up to 8 frames a second.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(125);
/// Window for the displayed speed.
const SPEED_WINDOW: Duration = Duration::from_secs(3);
/// Window for the ETA estimate, longer to keep it stable.
const ETA_WINDOW: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Checking saved progress from an earlier run.
    Recovering,
    Connecting,
    Downloading,
    /// Waiting before the next attempt.
    Retrying {
        reason: String,
        attempt: u32,
        until: Instant,
    },
    Verifying,
    Finalizing,
    Stopping,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub phase: Phase,
    /// Destination file name, once known.
    pub name: Option<String>,
    pub host: String,
    pub received: u64,
    /// Bytes covered by the latest durable checkpoint.
    pub saved: u64,
    pub total: Option<u64>,
    /// Smoothed bytes per second.
    pub speed: f64,
    /// Slower-moving rate used for ETA.
    pub eta_speed: f64,
    pub peak: f64,
    pub history: Vec<f64>,
    pub connections: u16,
    pub started: Instant,
    pub elapsed: Duration,
    pub http_version: Option<&'static str>,
    pub retries: u32,
    /// Byte ranges of the file still to be received, when it is fetched in segments.
    pub segments: Option<SegmentView>,
    pub route: Option<String>,
    /// Incremented whenever new bytes were seen; drives activity indicators.
    pub activity: u64,
}

impl Snapshot {
    fn new(host: String, route: Option<String>) -> Snapshot {
        Snapshot {
            phase: Phase::Connecting,
            name: None,
            host,
            received: 0,
            saved: 0,
            total: None,
            speed: 0.0,
            eta_speed: 0.0,
            peak: 0.0,
            history: Vec::new(),
            connections: 0,
            started: Instant::now(),
            elapsed: Duration::ZERO,
            http_version: None,
            retries: 0,
            segments: None,
            route,
            activity: 0,
        }
    }

    /// Remaining time, when it can be estimated honestly.
    pub fn eta(&self) -> Option<Duration> {
        let total = self.total?;
        if self.phase != Phase::Downloading || self.eta_speed < 1.0 {
            return None;
        }
        let remaining = total.saturating_sub(self.received) as f64;
        Some(Duration::from_secs_f64(remaining / self.eta_speed))
    }

    pub fn fraction(&self) -> Option<f64> {
        let total = self.total?;
        Some(if total == 0 {
            1.0
        } else {
            (self.received as f64 / total as f64).min(1.0)
        })
    }
}

/// Segment activity of a parallel transfer, for the detailed view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SegmentView {
    /// Segments being received.
    pub active: usize,
    /// Throughput of each active connection, in bytes per second.
    pub rates: Vec<f64>,
}

/// Discrete, ordered events for line-oriented and JSON output.
#[derive(Clone, Debug)]
pub enum Event {
    Started {
        url: String,
    },
    Redirected {
        to: String,
    },
    Responded {
        output: PathBuf,
        total: Option<u64>,
        http_version: &'static str,
        resumable: bool,
    },
    Retrying {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    Resumed {
        offset: u64,
    },
    /// Continuing saved progress from an earlier run.
    Recovered {
        output: PathBuf,
        saved: u64,
        total: Option<u64>,
        /// Bytes on disk that were not covered by a valid checkpoint or failed verification.
        discarded: u64,
    },
    Restarted {
        reason: String,
    },
    Verifying,
    Finalizing,
    Notice {
        message: String,
    },
}

#[derive(Clone)]
pub struct Reporter {
    inner: Arc<Inner>,
}

struct Inner {
    received: AtomicU64,
    /// Connections currently receiving data.
    streams: AtomicU64,
    /// The phase the engine last reported; see [`Reporter::phase`].
    requested: Mutex<Phase>,
    /// Bumped by `set_received`, so the sampler does not mistake a jump for throughput.
    rebases: AtomicU64,
    durable: Arc<Durable>,
    snapshot: watch::Sender<Snapshot>,
    events: SyncSender<Event>,
}

/// The latest durable checkpoint, recorded by the writer thread.
#[derive(Debug, Default)]
pub struct Durable {
    bytes: AtomicU64,
}

impl Durable {
    pub fn record(&self, bytes: u64) {
        self.bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

impl Reporter {
    pub fn new(
        host: String,
        route: Option<String>,
        events: SyncSender<Event>,
    ) -> (Reporter, watch::Receiver<Snapshot>) {
        let (tx, rx) = watch::channel(Snapshot::new(host, route));
        let reporter = Reporter {
            inner: Arc::new(Inner {
                received: AtomicU64::new(0),
                streams: AtomicU64::new(0),
                requested: Mutex::new(Phase::Connecting),
                rebases: AtomicU64::new(0),
                durable: Arc::default(),
                snapshot: tx,
                events,
            }),
        };
        (reporter, rx)
    }

    pub fn add_bytes(&self, n: u64) {
        self.inner.received.fetch_add(n, Ordering::Relaxed);
    }

    /// Set the received counter, e.g. after restarting from the beginning or recovering saved
    /// progress. Speed measurement starts again from this value.
    pub fn set_received(&self, n: u64) {
        self.inner.received.store(n, Ordering::Relaxed);
        self.inner.rebases.fetch_add(1, Ordering::Relaxed);
        self.inner.snapshot.send_modify(|s| s.received = n);
    }

    pub fn received(&self) -> u64 {
        self.inner.received.load(Ordering::Relaxed)
    }

    /// Shared with the writer thread, which records each published checkpoint.
    pub fn durable(&self) -> Arc<Durable> {
        self.inner.durable.clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        self.inner.snapshot.send_modify(f);
    }

    /// Report the engine's phase. While any connection is receiving data, connecting or
    /// waiting to retry on another one still shows as downloading.
    pub fn phase(&self, phase: Phase) {
        let mut requested = self.inner.requested.lock().expect("phase lock");
        *requested = phase;
        self.publish_phase(&requested);
    }

    /// Count a connection as receiving data until the guard is dropped.
    pub fn stream(&self) -> StreamGuard {
        self.inner.streams.fetch_add(1, Ordering::Relaxed);
        self.publish_phase(&self.inner.requested.lock().expect("phase lock"));
        StreamGuard {
            reporter: self.clone(),
        }
    }

    fn publish_phase(&self, requested: &Phase) {
        let streams = self.inner.streams.load(Ordering::Relaxed);
        let phase = match requested {
            Phase::Connecting | Phase::Retrying { .. } if streams > 0 => Phase::Downloading,
            other => other.clone(),
        };
        self.update(|s| {
            s.connections = u16::try_from(streams).unwrap_or(u16::MAX);
            s.phase = phase;
        });
    }

    /// Queue an event without blocking. Events are dropped if presentation falls far behind.
    pub fn event(&self, event: Event) {
        match self.inner.events.try_send(event) {
            Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Publish speed and progress snapshots until the reporter's last clone is dropped.
    pub fn spawn_sampler(&self) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut meter = SpeedMeter::new(Instant::now());
            let mut rebases = 0;
            let mut sampled: Option<Instant> = None;
            let mut ticker = tokio::time::interval(PUBLISH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else { break };
                let now = Instant::now();
                let received = inner.received.load(Ordering::Relaxed);
                let seen = inner.rebases.load(Ordering::Relaxed);
                if seen != rebases {
                    rebases = seen;
                    meter.rebase(now, received);
                }
                // Allow for timer jitter, so that every other tick samples.
                let sample = sampled.is_none_or(|at| now - at >= SAMPLE_INTERVAL * 9 / 10);
                if sample {
                    sampled = Some(now);
                    meter.record(now, received);
                }
                inner.snapshot.send_modify(|s| {
                    if received > s.received {
                        s.activity = s.activity.wrapping_add(1);
                    }
                    s.received = received;
                    s.saved = inner.durable.bytes();
                    s.elapsed = now.duration_since(s.started);
                    if sample {
                        s.speed = meter.rate(now, SPEED_WINDOW);
                        s.eta_speed = meter.rate(now, ETA_WINDOW);
                        s.peak = meter.peak;
                        s.history.clear();
                        s.history.extend(meter.history.iter().copied());
                    }
                });
            }
        })
    }
}

/// Keeps a connection counted as receiving data; see [`Reporter::stream`].
pub struct StreamGuard {
    reporter: Reporter,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let inner = &self.reporter.inner;
        let requested = inner.requested.lock().expect("phase lock");
        inner.streams.fetch_sub(1, Ordering::Relaxed);
        self.reporter.publish_phase(&requested);
    }
}

/// Sliding-window throughput measurement.
struct SpeedMeter {
    samples: VecDeque<(Instant, u64)>,
    history: VecDeque<f64>,
    bucket_start: (Instant, u64),
    peak: f64,
}

impl SpeedMeter {
    fn new(now: Instant) -> SpeedMeter {
        SpeedMeter {
            samples: VecDeque::from([(now, 0)]),
            history: VecDeque::with_capacity(HISTORY_LEN),
            bucket_start: (now, 0),
            peak: 0.0,
        }
    }

    /// Measure from `received` onwards, keeping the history and peak.
    fn rebase(&mut self, now: Instant, received: u64) {
        self.samples.clear();
        self.samples.push_back((now, received));
        self.bucket_start = (now, received);
    }

    fn record(&mut self, now: Instant, received: u64) {
        // A restart from zero invalidates the window.
        if self.samples.back().is_some_and(|&(_, b)| received < b) {
            self.samples.clear();
            self.bucket_start = (now, received);
        }
        self.samples.push_back((now, received));
        while self
            .samples
            .front()
            .is_some_and(|&(t, _)| now.duration_since(t) > ETA_WINDOW)
            && self.samples.len() > 2
        {
            self.samples.pop_front();
        }
        let (bucket_t, bucket_b) = self.bucket_start;
        let span = now.duration_since(bucket_t);
        if span >= Duration::from_secs(1) {
            let rate = received.saturating_sub(bucket_b) as f64 / span.as_secs_f64();
            if self.history.len() == HISTORY_LEN {
                self.history.pop_front();
            }
            self.history.push_back(rate);
            self.peak = self.peak.max(rate);
            self.bucket_start = (now, received);
        }
    }

    fn rate(&self, now: Instant, window: Duration) -> f64 {
        let Some(&(_, latest)) = self.samples.back() else {
            return 0.0;
        };
        let start = self
            .samples
            .iter()
            .find(|&&(t, _)| now.duration_since(t) <= window)
            .copied();
        match start {
            Some((t, b)) => {
                let span = now.duration_since(t).as_secs_f64();
                if span < 0.5 {
                    0.0
                } else {
                    latest.saturating_sub(b) as f64 / span
                }
            }
            None => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_measures_steady_rate() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::new(t0);
        for i in 1..=40u64 {
            let now = t0 + Duration::from_millis(250 * i);
            meter.record(now, i * 25_000); // 100 kB/s
        }
        let now = t0 + Duration::from_secs(10);
        let rate = meter.rate(now, SPEED_WINDOW);
        assert!((rate - 100_000.0).abs() < 1.0, "{rate}");
        assert_eq!(meter.history.len(), 10);
        assert!((meter.peak - 100_000.0).abs() < 1.0);
    }

    #[test]
    fn meter_handles_restart_and_stall() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::new(t0);
        meter.record(t0 + Duration::from_secs(1), 1_000_000);
        meter.record(t0 + Duration::from_secs(2), 10);
        assert_eq!(meter.rate(t0 + Duration::from_secs(2), SPEED_WINDOW), 0.0);
        for s in 3..10 {
            meter.record(t0 + Duration::from_secs(s), 10);
        }
        assert_eq!(meter.rate(t0 + Duration::from_secs(9), SPEED_WINDOW), 0.0);
    }

    #[test]
    fn rebase_ignores_recovered_bytes() {
        let t0 = Instant::now();
        let mut meter = SpeedMeter::new(t0);
        meter.rebase(t0, 50_000_000);
        for i in 1..=8u64 {
            meter.record(t0 + Duration::from_millis(250 * i), 50_000_000 + i * 2_500);
        }
        let rate = meter.rate(t0 + Duration::from_secs(2), SPEED_WINDOW);
        assert!((rate - 10_000.0).abs() < 1.0, "{rate}");
        assert!(meter.peak < 20_000.0);
    }

    #[test]
    fn active_streams_keep_the_phase_downloading() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(4);
        let (reporter, snapshots) = Reporter::new("h".into(), None, tx);
        let first = reporter.stream();
        let second = reporter.stream();
        reporter.phase(Phase::Retrying {
            reason: "x".into(),
            attempt: 1,
            until: Instant::now(),
        });
        assert_eq!(snapshots.borrow().phase, Phase::Downloading);
        assert_eq!(snapshots.borrow().connections, 2);
        drop(first);
        drop(second);
        assert!(matches!(snapshots.borrow().phase, Phase::Retrying { .. }));
        assert_eq!(snapshots.borrow().connections, 0);
        reporter.phase(Phase::Verifying);
        let _third = reporter.stream();
        assert_eq!(snapshots.borrow().phase, Phase::Verifying);
    }

    #[test]
    fn eta_is_absent_when_unknowable() {
        let mut snap = Snapshot::new("h".into(), None);
        snap.total = Some(1000);
        snap.eta_speed = 100.0;
        assert_eq!(snap.eta(), None, "not downloading yet");
        snap.phase = Phase::Downloading;
        snap.received = 500;
        assert_eq!(snap.eta(), Some(Duration::from_secs(5)));
        snap.total = None;
        assert_eq!(snap.eta(), None);
        assert_eq!(snap.fraction(), None);
    }
}
