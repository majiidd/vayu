//! Terminal presentation: capability detection, live dashboard, line output, and JSON events.
//!
//! Presentation runs on its own thread. It reads replaceable snapshots and a bounded event
//! queue and never holds transfer state, so blocked or slow output cannot stall the download.

pub mod caps;
pub mod dashboard;
mod json;
mod lines;
pub mod style;
pub mod text;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::cli::{Display, UiMode};
use crate::error::Failure;
use crate::metrics::{Event, Snapshot};
use crate::scheduler::Stop;
use caps::Caps;
use dashboard::Dashboard;
use json::Json;
use lines::{Lines, Verbosity};

/// A finished download.
#[derive(Clone, Debug)]
pub struct Completion {
    pub path: PathBuf,
    pub bytes: u64,
    pub elapsed: Duration,
    pub sha256: [u8; 32],
    /// Matched a checksum supplied by the user.
    pub verified: bool,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Complete(Completion),
    Failed {
        failure: Failure,
        /// Partial data left on disk for a later decision.
        partial: Option<PathBuf>,
    },
}

enum Sink {
    Dashboard(Dashboard),
    Lines(Lines),
    Json(Json),
}

impl Sink {
    fn select(display: &Display, caps: Caps, stop: Stop) -> Sink {
        if display.json {
            return Sink::Json(Json::new(stop));
        }
        if display.quiet {
            return Sink::Lines(Lines::new(&caps, Verbosity::Quiet));
        }
        match display.ui {
            UiMode::Off => Sink::Lines(Lines::new(&caps, Verbosity::ResultOnly)),
            mode if caps.cursor => Sink::Dashboard(Dashboard::new(caps, mode)),
            _ => Sink::Lines(Lines::new(&caps, Verbosity::Normal)),
        }
    }

    fn interval(&self) -> Duration {
        match self {
            Sink::Dashboard(d) => d.interval(),
            _ => Duration::from_millis(250),
        }
    }

    fn event(&mut self, event: &Event) {
        match self {
            Sink::Dashboard(d) => d.event(event),
            Sink::Lines(l) => l.event(event),
            Sink::Json(j) => j.event(event),
        }
    }

    fn tick(&mut self, snap: &Snapshot) {
        match self {
            Sink::Dashboard(d) => d.draw(snap),
            Sink::Lines(l) => l.tick(snap),
            Sink::Json(j) => j.tick(snap),
        }
    }

    fn finish(&mut self, outcome: &Outcome, snap: &Snapshot) {
        match self {
            Sink::Dashboard(d) => d.finish(outcome, snap),
            Sink::Lines(l) => l.finish(outcome, snap),
            Sink::Json(j) => j.finish(outcome, snap),
        }
    }
}

pub struct Presenter {
    outcome: mpsc::Sender<Outcome>,
    done: Receiver<()>,
}

impl Presenter {
    pub fn start(
        display: &Display,
        caps: Caps,
        snapshots: watch::Receiver<Snapshot>,
        events: Receiver<Event>,
        stop: Stop,
    ) -> Presenter {
        let (outcome_tx, outcome_rx) = mpsc::channel::<Outcome>();
        let (done_tx, done_rx) = mpsc::channel();
        let mut sink = Sink::select(display, caps, stop);
        let spawned = std::thread::Builder::new()
            .name("vayu-presenter".into())
            .spawn(move || {
                let interval = sink.interval();
                let mut next_frame = Instant::now();
                loop {
                    let wait = next_frame.saturating_duration_since(Instant::now());
                    match events.recv_timeout(wait) {
                        Ok(event) => sink.event(&event),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => std::thread::sleep(wait),
                    }
                    if let Ok(outcome) = outcome_rx.try_recv() {
                        while let Ok(event) = events.try_recv() {
                            sink.event(&event);
                        }
                        let snap = snapshots.borrow().clone();
                        sink.finish(&outcome, &snap);
                        let _ = done_tx.send(());
                        return;
                    }
                    if Instant::now() >= next_frame {
                        let snap = snapshots.borrow().clone();
                        sink.tick(&snap);
                        next_frame = Instant::now() + interval;
                    }
                }
            });
        // Without a presentation thread the download still works; it is just silent.
        drop(spawned);
        Presenter {
            outcome: outcome_tx,
            done: done_rx,
        }
    }

    /// Show the outcome and wait briefly for it to be written. A blocked terminal must not keep
    /// the process alive, so the wait is bounded.
    pub fn finish(self, outcome: Outcome) {
        if self.outcome.send(outcome).is_ok() {
            let _ = self.done.recv_timeout(Duration::from_secs(2));
        }
    }
}
