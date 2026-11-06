//! Orchestration of one download: settings, transport, recovery, transfer, verification,
//! publication.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::{Args, Settings};
use crate::error::{Category, Failure};
use crate::metrics::{Event, Phase, Reporter};
use crate::presentation::caps::{self, Caps};
use crate::presentation::style::{Painter, Role};
use crate::presentation::{Completion, Outcome, Presenter, text};
use crate::recovery;
use crate::scheduler::engine::{Engine, write_failure};
use crate::scheduler::{Stop, StopReason};
use crate::storage::Destination;
use crate::transport::Transport;

const EVENT_QUEUE: usize = 256;
/// How long a stopping download may spend saving its last checkpoint.
const SHUTDOWN_WINDOW: Duration = Duration::from_secs(10);

pub async fn run(args: Args) -> u8 {
    let settings = match Settings::resolve(args) {
        Ok(settings) => settings,
        Err(failure) => return report_early(&failure, None),
    };
    let caps = caps::detect(&settings.display);
    let transport = match Transport::new(&settings) {
        Ok(transport) => Arc::new(transport),
        Err(failure) => return report_early(&failure, Some(&caps)),
    };

    let (events_tx, events_rx) = std::sync::mpsc::sync_channel(EVENT_QUEUE);
    let host = settings.url.host_str().unwrap_or_default().to_string();
    let (reporter, snapshots) = Reporter::new(host, transport.route.clone(), events_tx);
    let stop = Stop::new();
    let presenter = Presenter::start(&settings.display, caps, snapshots, events_rx, stop.clone());
    let signals = tokio::spawn(watch_signals(stop.clone()));
    let sampler = reporter.spawn_sampler();
    if let Some(note) = transport.trust_note {
        reporter.event(Event::Notice {
            message: note.to_string(),
        });
    }

    let outcome = download(&settings, &transport, &reporter, &stop).await;

    // Let the sampler publish the final byte count before presentation reads it.
    reporter.set_received(reporter.received());
    drop(reporter);
    let _ = sampler.await;
    signals.abort();

    let code = match &outcome {
        Outcome::Complete(_) => 0,
        Outcome::Failed { failure, .. } => failure.category.exit_code(),
    };
    presenter.finish(outcome);
    code
}

async fn download(
    settings: &Settings,
    transport: &Arc<Transport>,
    reporter: &Reporter,
    stop: &Stop,
) -> Outcome {
    let started = Instant::now();
    reporter.event(Event::Started {
        url: text::redact_url(&settings.url),
    });
    let found = match tokio::task::block_in_place(|| recovery::locate(settings, reporter)) {
        Ok(found) => found,
        Err(failure) => {
            return Outcome::Failed {
                failure,
                partial: None,
            };
        }
    };
    let mut engine = Engine::new(settings, transport, reporter, stop);
    let result = engine.run(found).await;
    if let Some(mut done) = engine.published.take() {
        reporter.event(Event::Notice {
            message: "the file was already complete; removed the leftover recovery files".into(),
        });
        done.elapsed = started.elapsed();
        return Outcome::Complete(done);
    }
    let received = engine.progress();
    let checksum = engine.checksum.clone();
    let dest = engine.dest.take();
    let writer = engine.writer.take();

    let failed = |failure: Failure, dest: Option<&Destination>, kept: bool| Outcome::Failed {
        failure,
        partial: dest.filter(|_| kept).map(|d| d.part.clone()),
    };

    if let Err(failure) = result {
        reporter.phase(Phase::Stopping);
        // Checkpoint whatever reached the writer, then keep it unless nothing useful was
        // written. A stalled disk must not keep the process alive indefinitely.
        if let Some(writer) = writer {
            let _ = tokio::time::timeout(SHUTDOWN_WINDOW, writer.finish(false)).await;
        }
        if let Some(d) = &dest
            && received == 0
        {
            d.discard_if_empty();
        }
        return failed(failure, dest.as_ref(), received > 0);
    }

    let (Some(dest), Some(writer)) = (dest, writer) else {
        return failed(
            Failure::new(
                Category::Internal,
                "transfer finished without a destination",
            ),
            None,
            false,
        );
    };

    if checksum.is_some() {
        reporter.phase(Phase::Verifying);
        reporter.event(Event::Verifying);
    } else {
        reporter.phase(Phase::Finalizing);
    }
    let finished = match writer.finish(true).await {
        Ok(finished) => finished,
        Err(err) => return failed(write_failure(Some(&dest), &err), Some(&dest), true),
    };
    if finished.bytes != received {
        return failed(
            Failure::storage(format!(
                "wrote {} bytes but received {received}",
                finished.bytes
            )),
            Some(&dest),
            true,
        );
    }
    let verified = match &checksum {
        Some(expected) if !expected.matches(&finished.sha256) => {
            let failure = Failure::new(
                Category::Integrity,
                format!(
                    "SHA-256 mismatch: expected {}, got {}",
                    expected.hex(),
                    crate::integrity::hex(&finished.sha256)
                ),
            )
            .with_hint(format!(
                "{} was not created and the data was kept; if the expected checksum was wrong, \
                 run again with the correct --checksum to finish without downloading again; \
                 otherwise use --restart",
                dest.path.display()
            ));
            return failed(failure, Some(&dest), true);
        }
        Some(_) => true,
        None => false,
    };

    reporter.phase(Phase::Finalizing);
    reporter.event(Event::Finalizing);
    let bytes = finished.bytes;
    let sha256 = finished.sha256;
    if let Err(failure) = dest.publish(finished) {
        return failed(failure, Some(&dest), true);
    }
    Outcome::Complete(Completion {
        path: dest.path,
        bytes,
        elapsed: started.elapsed(),
        sha256,
        verified,
    })
}

/// First signal: stop safely. Second signal: exit immediately.
/// A hangup that was already ignored at startup (`nohup`) stays ignored.
async fn watch_signals(stop: Stop) {
    use tokio::signal::unix::{Signal, SignalKind, signal};

    async fn next(signal: &mut Option<Signal>) {
        match signal {
            Some(s) => {
                if s.recv().await.is_none() {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending().await,
        }
    }

    let mut int = signal(SignalKind::interrupt()).ok();
    let mut term = signal(SignalKind::terminate()).ok();
    let mut hup = if hangup_ignored() {
        None
    } else {
        signal(SignalKind::hangup()).ok()
    };
    let mut seen = false;
    loop {
        let sig = tokio::select! {
            _ = next(&mut int) => libc::SIGINT,
            _ = next(&mut term) => libc::SIGTERM,
            _ = next(&mut hup) => libc::SIGHUP,
        };
        if seen {
            std::process::exit(StopReason::Signal(sig).exit_code().into());
        }
        seen = true;
        stop.request(StopReason::Signal(sig));
    }
}

fn hangup_ignored() -> bool {
    // SAFETY: querying the current disposition with a null new action does not modify it.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGHUP, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}

/// Report a failure that happened before presentation started.
fn report_early(failure: &Failure, caps: Option<&Caps>) -> u8 {
    let painter = caps.map(Painter::new);
    let paint = |role, s: &str| match &painter {
        Some(p) => p.paint(Some(role), s),
        None => s.to_string(),
    };
    eprintln!(
        "{} {}",
        paint(Role::Error, "vayu: error:"),
        text::sanitize(&failure.message)
    );
    if let Some(hint) = &failure.hint {
        eprintln!("{}", text::sanitize(hint));
    }
    failure.category.exit_code()
}
