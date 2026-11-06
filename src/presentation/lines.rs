//! Line-oriented output for redirected streams, `TERM=dumb`, cron, and detached runs.
//!
//! No cursor movement or animation. Progress is reported at each 10% step or once a minute,
//! so output stays bounded for long transfers.

use std::io::Write;
use std::time::{Duration, Instant};

use super::Outcome;
use super::caps::Caps;
use super::dashboard::stats_text;
use super::style::{GlyphSet, Painter, Role};
use super::text;
use crate::metrics::{Event, Phase, Snapshot};

const PROGRESS_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    /// Events, progress, and the final result.
    Normal,
    /// Only the final result and failures.
    ResultOnly,
    /// Only failures.
    Quiet,
}

pub struct Lines {
    painter: Painter,
    glyphs: GlyphSet,
    verbosity: Verbosity,
    last_step: Option<u64>,
    last_line: Instant,
    dead: bool,
}

impl Lines {
    pub fn new(caps: &Caps, verbosity: Verbosity) -> Lines {
        Lines {
            painter: Painter::new(caps),
            glyphs: GlyphSet::for_caps(caps),
            verbosity,
            last_step: None,
            last_line: Instant::now(),
            dead: false,
        }
    }

    pub fn event(&mut self, event: &Event) {
        if self.verbosity != Verbosity::Normal {
            return;
        }
        let message = match event {
            Event::Started { url } => format!("downloading {}", text::sanitize(url)),
            Event::Redirected { to } => format!("redirected to {}", text::sanitize(to)),
            Event::Responded {
                output,
                total,
                http_version,
                ..
            } => format!(
                "saving to {} ({}, {http_version})",
                text::sanitize(&output.display().to_string()),
                total
                    .map(text::bytes)
                    .unwrap_or_else(|| "size unknown".into())
            ),
            Event::Retrying {
                attempt,
                delay,
                reason,
            } => format!(
                "{reason}; retrying in {}s (attempt {attempt})",
                delay.as_secs_f64().ceil()
            ),
            Event::Resumed { offset } => {
                format!("reconnected; continuing at {}", text::bytes(*offset))
            }
            Event::Recovered {
                output,
                saved,
                total,
                discarded,
            } => {
                let of = total
                    .map(|t| format!(" of {}", text::bytes(t)))
                    .unwrap_or_default();
                let mut line = format!(
                    "resuming {}: {}{of} saved",
                    text::sanitize(&output.display().to_string()),
                    text::bytes(*saved)
                );
                if *discarded > 0 {
                    line.push_str(&format!(
                        "; {} could not be verified and will be downloaded again",
                        text::bytes(*discarded)
                    ));
                }
                line
            }
            Event::Restarted { reason } => reason.clone(),
            Event::Verifying => "verifying SHA-256".into(),
            Event::Finalizing => return,
            Event::Notice { message } => message.clone(),
        };
        let role = match event {
            Event::Retrying { .. } | Event::Restarted { .. } | Event::Notice { .. } => {
                Some(Role::Warn)
            }
            _ => None,
        };
        self.emit(role, &message);
    }

    pub fn tick(&mut self, snap: &Snapshot) {
        if self.verbosity != Verbosity::Normal || snap.phase != Phase::Downloading {
            return;
        }
        let step = snap.fraction().map(|f| (f * 10.0).floor() as u64);
        let stepped = step.is_some() && step != self.last_step && step != Some(0);
        if !stepped && self.last_line.elapsed() < PROGRESS_EVERY {
            return;
        }
        self.last_step = step;
        let eta = snap
            .eta()
            .map(text::duration)
            .unwrap_or_else(|| self.glyphs.dash.into());
        let line = match (snap.fraction(), snap.total) {
            (Some(f), Some(total)) => format!(
                "{:.0}% {} / {} at {}, ETA {eta}",
                (f * 100.0).floor(),
                text::bytes(snap.received),
                text::bytes(total),
                text::rate(snap.speed)
            ),
            _ => format!(
                "{} received at {}",
                text::bytes(snap.received),
                text::rate(snap.speed)
            ),
        };
        self.emit(None, &line);
    }

    pub fn finish(&mut self, outcome: &Outcome, snap: &Snapshot) {
        match outcome {
            Outcome::Complete(done) => {
                if self.verbosity == Verbosity::Quiet {
                    return;
                }
                self.emit(
                    Some(Role::Success),
                    &format!(
                        "complete, saved to {} ({})",
                        text::sanitize(&done.path.display().to_string()),
                        stats_text(done, &self.glyphs)
                    ),
                );
                let digest = crate::integrity::hex(&done.sha256);
                let check = if done.verified {
                    "verified"
                } else {
                    "computed, no checksum supplied"
                };
                if self.verbosity == Verbosity::Normal || done.verified {
                    self.emit(None, &format!("sha256 {digest} ({check})"));
                }
            }
            Outcome::Failed { failure, partial } => {
                let interrupted =
                    matches!(failure.category, crate::error::Category::Interrupted(_));
                let (role, word) = if interrupted {
                    (Role::Warn, "stopped")
                } else {
                    (Role::Error, "error")
                };
                self.emit(
                    Some(role),
                    &format!("{word}: {}", text::sanitize(&failure.message)),
                );
                if let Some(hint) = &failure.hint {
                    self.emit(None, &text::sanitize(hint));
                }
                if let Some(part) = partial {
                    self.emit(
                        None,
                        &format!(
                            "partial data ({}) kept at {}",
                            text::bytes(snap.received),
                            text::sanitize(&part.display().to_string())
                        ),
                    );
                }
            }
        }
    }

    fn emit(&mut self, role: Option<Role>, message: &str) {
        if self.dead {
            return;
        }
        self.last_line = Instant::now();
        let line = format!(
            "{} {}\n",
            self.painter.paint(Some(Role::Brand), "vayu:"),
            self.painter.paint(role, message)
        );
        let mut stderr = std::io::stderr().lock();
        if stderr.write_all(line.as_bytes()).is_err() {
            self.dead = true;
        }
    }
}
