//! Work assignment, retries, adaptation, and cancellation.
//!
//! [`engine`] runs the lead stream and decides whether to segment; [`plan`] owns the byte
//! ranges; [`crew`] runs helper connections under the adaptive [`controller`]; [`retry`] holds
//! the retry budget.

pub mod controller;
pub mod crew;
pub mod engine;
pub mod plan;
pub mod retry;

use std::sync::Arc;

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// A termination signal (its number).
    Signal(i32),
    /// The JSON event stream was closed by its reader.
    OutputClosed,
}

impl StopReason {
    pub fn exit_code(self) -> u8 {
        match self {
            StopReason::Signal(sig) => (128 + sig).clamp(0, 255) as u8,
            StopReason::OutputClosed => 141,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            StopReason::Signal(libc::SIGINT) => "interrupted",
            StopReason::Signal(libc::SIGTERM) => "terminated",
            StopReason::Signal(libc::SIGHUP) => "hangup received",
            StopReason::Signal(_) => "stopped by a signal",
            StopReason::OutputClosed => "the JSON event stream was closed",
        }
    }
}

/// A cooperative, first-reason-wins stop request.
#[derive(Clone)]
pub struct Stop {
    tx: Arc<watch::Sender<Option<StopReason>>>,
}

impl Default for Stop {
    fn default() -> Self {
        Stop::new()
    }
}

impl Stop {
    pub fn new() -> Stop {
        Stop {
            tx: Arc::new(watch::channel(None).0),
        }
    }

    pub fn request(&self, reason: StopReason) {
        self.tx.send_if_modified(|current| {
            if current.is_none() {
                *current = Some(reason);
                true
            } else {
                false
            }
        });
    }

    pub fn reason(&self) -> Option<StopReason> {
        *self.tx.borrow()
    }

    /// Resolves once a stop has been requested.
    pub async fn wait(&self) -> StopReason {
        let mut rx = self.tx.subscribe();
        // Copy the reason out so no lock guard is held across the pending await (which would
        // make the future unsendable).
        let reason = rx.wait_for(Option::is_some).await.map(|reason| *reason);
        match reason {
            Ok(reason) => reason.expect("checked by wait_for"),
            Err(_) => std::future::pending().await,
        }
    }
}
