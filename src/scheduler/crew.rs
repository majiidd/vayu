//! Additional connections for a segmented transfer.
//!
//! The engine's own stream, the lead, is one worker. A supervisor task runs the concurrency
//! [`Controller`] and starts helper connections when it asks for more. A helper claims work from
//! the shared [`Plan`]: a missing range, or the back half of the busiest segment. It requests
//! exactly that range (`Range: bytes=a-b`, with `If-Range`), validates the partial response the
//! way the lead does, and only then takes over a split, so a server that ignores ranges never
//! costs the existing stream its work. Each helper keeps its own connection slot, and reuses its
//! connection for the next range when the previous response ended cleanly.
//!
//! Helpers never retry on their own. A failed helper returns its range to the plan and the
//! controller lowers the connection count; the lead's retry policy covers the transfer as a
//! whole. Outcomes on a helper:
//!
//! * a changed source (validators, size) stops the transfer with a conflict, keeping the data;
//! * a full response to a range request, a 416, or a refusal before ranges were ever served
//!   means the server does not offer this: segmentation ends and the lead continues alone;
//! * 429 and 503 halve the connection count; network failures, stalls, early closes, and later
//!   refusals remove one connection.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::Response;
use reqwest::header::{self, HeaderMap};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use super::Stop;
use super::controller::{Controller, Observation, Signal};
use super::engine::Limiter;
use super::plan::{Claim, Plan, Steal};
use super::retry::PROGRESS_BYTES;
use crate::cli::Settings;
use crate::error::Failure;
use crate::metrics::{Event, Reporter, SegmentView};
use crate::storage::WriteHandle;
use crate::transport::http::{Validators, parse_content_range};
use crate::transport::{AttemptError, RangeRequest, Transport, classify_status};

const TICK: Duration = Duration::from_millis(250);
/// Upper bound for winding down helpers when the transfer ends.
const SHUTDOWN: Duration = Duration::from_secs(5);
/// Proposed splits that lost a race before a helper gives up looking for work.
const MAX_LOST_SPLITS: u32 = 3;

/// Everything a helper needs, shared by all of them.
pub struct Context {
    pub settings: Arc<Settings>,
    pub transport: Arc<Transport>,
    pub reporter: Reporter,
    pub stop: Stop,
    pub writes: WriteHandle,
    /// The representation the transfer is bound to.
    pub validators: Validators,
    pub limiter: Option<Arc<Limiter>>,
    /// Connections kept in total, the lead included.
    pub min_connections: u16,
    /// Connections in total, the lead included.
    pub max_connections: u16,
}

/// Why the crew ended the transfer.
pub enum CrewFailure {
    Fatal(Failure),
    /// The writer stopped; its own error explains why.
    Write,
}

/// Messages from workers to the supervisor.
enum Note {
    Signal(Signal),
}

pub struct Shared {
    plan: Mutex<Plan>,
    /// Bumped whenever segments end or work returns, so an idle lead can look again.
    changed: watch::Sender<u64>,
    failure: Mutex<Option<CrewFailure>>,
    failed: watch::Sender<bool>,
    notes: mpsc::UnboundedSender<Note>,
    /// A helper received a valid partial response: the server serves ranges.
    proven: AtomicBool,
}

impl Shared {
    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn fail(&self, failure: CrewFailure) {
        let mut slot = self.failure.lock().expect("failure lock");
        if slot.is_none() {
            *slot = Some(failure);
            self.failed.send_replace(true);
        }
    }
}

/// The plan of a segmented transfer and, when more than one connection is allowed, the
/// supervisor of its helper connections.
pub struct Crew {
    shared: Arc<Shared>,
    cancel: watch::Sender<bool>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
}

impl Crew {
    /// Start with `plan`. Without a context, the lead works through the plan alone.
    pub fn start(plan: Plan, context: Option<Context>) -> Crew {
        let (notes, inbox) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            plan: Mutex::new(plan),
            changed: watch::channel(0).0,
            failure: Mutex::new(None),
            failed: watch::channel(false).0,
            notes,
            proven: AtomicBool::new(false),
        });
        let (cancel, cancelled) = watch::channel(false);
        let supervisor = context.filter(|c| c.max_connections > 1).map(|context| {
            tokio::spawn(supervise(
                shared.clone(),
                Arc::new(context),
                inbox,
                cancelled,
            ))
        });
        Crew {
            shared,
            cancel,
            supervisor,
        }
    }

    pub fn plan(&self) -> MutexGuard<'_, Plan> {
        self.shared.plan.lock().expect("plan lock")
    }

    /// Subscribe before looking for work; [`watch::Receiver::changed`] then resolves on the next
    /// change.
    pub fn watch(&self) -> watch::Receiver<u64> {
        self.shared.changed.subscribe()
    }

    pub fn notify(&self) {
        self.shared.notify();
    }

    /// Report the lead's time to first byte or failures to the controller.
    pub fn signal(&self, signal: Signal) {
        let _ = self.shared.notes.send(Note::Signal(signal));
    }

    /// Resolves once a helper has ended the transfer.
    pub async fn failed(&self) {
        let mut rx = self.shared.failed.subscribe();
        if rx.wait_for(|failed| *failed).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    pub fn take_failure(&self) -> Option<CrewFailure> {
        self.shared.failure.lock().expect("failure lock").take()
    }

    /// Stop all helpers and wait until their last writes are queued. A stalled disk cannot hold
    /// this up for more than a few seconds; helpers still waiting then are abandoned.
    pub async fn shutdown(&mut self) {
        self.cancel.send_replace(true);
        if let Some(mut supervisor) = self.supervisor.take()
            && tokio::time::timeout(SHUTDOWN, &mut supervisor)
                .await
                .is_err()
        {
            supervisor.abort();
        }
    }
}

/// How a helper ended.
enum HelperEnd {
    /// No work left for another connection.
    Idle,
    Retired,
    Stopped,
    Failed {
        throttled: bool,
    },
    /// The server does not serve ranges this way.
    Unsupported(String),
    Fatal(Failure),
    WriteFailed,
}

struct Lane {
    retire: Arc<AtomicBool>,
}

async fn supervise(
    shared: Arc<Shared>,
    ctx: Arc<Context>,
    mut inbox: mpsc::UnboundedReceiver<Note>,
    mut cancelled: watch::Receiver<bool>,
) {
    let max = ctx.max_connections;
    let mut controller = Controller::new(ctx.min_connections, max, Instant::now());
    // Index `i` is connection slot `i + 1`; slot 0 is the lead's.
    let mut lanes: Vec<Option<Lane>> = (1..max).map(|_| None).collect();
    let mut helpers: JoinSet<(usize, HelperEnd)> = JoinSet::new();
    let mut setup = Duration::ZERO;
    let mut segmenting = true;
    let for_helpers = cancelled.clone();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancelled.wait_for(|c| *c) => break,
            _ = ctx.stop.wait() => break,
            Some(Note::Signal(signal)) = inbox.recv() => {
                if let Signal::Connected(d) | Signal::Latency(d) = signal {
                    setup = if setup.is_zero() { d } else { setup.mul_f64(0.7) + d.mul_f64(0.3) };
                }
                controller.signal(Instant::now(), signal);
            }
            Some(joined) = helpers.join_next(), if !helpers.is_empty() => {
                let Ok((lane, end)) = joined else { continue };
                lanes[lane - 1] = None;
                let now = Instant::now();
                match end {
                    HelperEnd::Idle | HelperEnd::Retired | HelperEnd::Stopped => {}
                    HelperEnd::Failed { throttled } => controller.signal(
                        now,
                        if throttled { Signal::Throttled } else { Signal::Failed },
                    ),
                    HelperEnd::Unsupported(reason) => {
                        controller.signal(now, Signal::Unsupported);
                        if std::mem::take(&mut segmenting) {
                            ctx.reporter.event(Event::Notice {
                                message: format!(
                                    "{reason}; continuing with one connection"
                                ),
                            });
                        }
                    }
                    HelperEnd::Fatal(failure) => shared.fail(CrewFailure::Fatal(failure)),
                    HelperEnd::WriteFailed => shared.fail(CrewFailure::Write),
                }
                shared.notify();
            }
            _ = ticker.tick() => {
                let now = Instant::now();
                let min_secs = steal_horizon(setup);
                let (bytes, remaining, has_work, rates) = {
                    let plan = shared.plan.lock().expect("plan lock");
                    (plan.done(), plan.remaining(), plan.has_work(now, min_secs), plan.rates(now))
                };
                let target = controller.tick(Observation {
                    now,
                    bytes,
                    remaining,
                    has_work,
                    disk_busy: ctx.writes.congested(),
                });
                let running = lanes.iter().filter(|l| l.is_some()).count();
                let active = 1 + running;
                if usize::from(target) > active && has_work {
                    if let Some(slot) = lanes.iter().position(Option::is_none) {
                        let retire = Arc::new(AtomicBool::new(false));
                        lanes[slot] = Some(Lane { retire: retire.clone() });
                        let lane = slot + 1;
                        let probe = controller.probing();
                        let task = helper(
                            ctx.clone(),
                            shared.clone(),
                            lane,
                            retire,
                            probe,
                            for_helpers.clone(),
                            setup,
                        );
                        helpers.spawn(async move { (lane, task.await) });
                    }
                } else if usize::from(target) < active {
                    // Retire the most recently added helpers at their next block boundary.
                    let mut excess = active - usize::from(target);
                    for (slot, lane) in lanes.iter().enumerate().rev() {
                        if excess == 0 {
                            break;
                        }
                        if let Some(lane) = lane
                            && !lane.retire.swap(true, Ordering::Relaxed)
                        {
                            shared.plan.lock().expect("plan lock").trim(slot + 1);
                            excess -= 1;
                        }
                    }
                }
                ctx.reporter.update(|s| {
                    s.segments = Some(SegmentView {
                        active: rates.len(),
                        rates,
                    });
                });
            }
        }
    }
    cancel_helpers(&mut helpers).await;
    ctx.reporter.update(|s| s.segments = None);
}

async fn cancel_helpers(helpers: &mut JoinSet<(usize, HelperEnd)>) {
    // Helpers watch the same cancellation flag; wait for them to release their segments.
    while helpers.join_next().await.is_some() {}
}

/// A split is worthwhile only when its owner would not finish within this many seconds anyway.
fn steal_horizon(setup: Duration) -> f64 {
    2.0 + 4.0 * setup.as_secs_f64()
}

enum Work {
    Owned(Claim),
    Steal(Steal),
}

impl Work {
    fn range(&self) -> (u64, u64) {
        match self {
            Work::Owned(c) => (c.start, c.end),
            Work::Steal(s) => (s.start, s.end),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn helper(
    ctx: Arc<Context>,
    shared: Arc<Shared>,
    lane: usize,
    retire: Arc<AtomicBool>,
    probe: bool,
    mut cancelled: watch::Receiver<bool>,
    setup: Duration,
) -> HelperEnd {
    let mut connected = false;
    let mut lost_splits = 0;
    let stall = ctx.settings.stall_timeout;
    loop {
        if retire.load(Ordering::Relaxed) {
            return HelperEnd::Retired;
        }
        let now = Instant::now();
        let (work, total) = {
            let mut plan = shared.plan.lock().expect("plan lock");
            let total = plan.total();
            match plan.claim(lane, now) {
                Some(claim) => (Work::Owned(claim), total),
                None => match plan.propose(now, steal_horizon(setup)) {
                    Some(steal) => (Work::Steal(steal), total),
                    None => return HelperEnd::Idle,
                },
            }
        };
        let give_back = |work: &Work| {
            if let Work::Owned(claim) = work {
                shared.plan.lock().expect("plan lock").release(claim.id);
                shared.notify();
            }
        };
        let (start, end) = work.range();
        let request = RangeRequest {
            start,
            end: Some(end),
            if_range: ctx.validators.if_range(),
        };
        let sent = Instant::now();
        let send = ctx
            .transport
            .get(lane, &ctx.settings.url, Some(request), |_| {});
        let result = tokio::select! {
            _ = ctx.stop.wait() => None,
            _ = cancelled.wait_for(|c| *c) => None,
            result = tokio::time::timeout(stall, send) => Some(result),
        };
        let response = match result {
            None => {
                give_back(&work);
                return HelperEnd::Stopped;
            }
            Some(Err(_)) => {
                give_back(&work);
                return HelperEnd::Failed { throttled: false };
            }
            Some(Ok(Err(err))) => {
                give_back(&work);
                return match err {
                    AttemptError::Retryable { .. } => HelperEnd::Failed { throttled: false },
                    AttemptError::Fatal(failure) => HelperEnd::Unsupported(format!(
                        "an additional connection failed: {failure}"
                    )),
                };
            }
            Some(Ok(Ok((_, response)))) => response,
        };
        let served = shared.proven.load(Ordering::Relaxed);
        if let Err(end) = check(&ctx, &response, start, total, served) {
            give_back(&work);
            return end;
        }
        let claim = match work {
            Work::Owned(claim) => claim,
            Work::Steal(steal) => {
                match shared
                    .plan
                    .lock()
                    .expect("plan lock")
                    .commit(steal, lane, Instant::now())
                {
                    Some(claim) => claim,
                    None => {
                        // The owner got there first; this response is not needed.
                        lost_splits += 1;
                        if lost_splits >= MAX_LOST_SPLITS {
                            return HelperEnd::Idle;
                        }
                        continue;
                    }
                }
            }
        };
        shared.proven.store(true, Ordering::Relaxed);
        let ttfb = sent.elapsed();
        let _ = shared.notes.send(Note::Signal(if probe && !connected {
            Signal::Connected(ttfb)
        } else {
            Signal::Latency(ttfb)
        }));
        connected = true;
        match receive(&ctx, &shared, &mut cancelled, response, claim, end).await {
            Received::Done => {}
            Received::Short(got) if got >= PROGRESS_BYTES => {}
            Received::Short(_) => return HelperEnd::Failed { throttled: false },
            Received::Ended(end) => return end,
        }
    }
}

/// Validate a response to a helper's range request for `[start, ..)` of `total` bytes.
fn check(
    ctx: &Context,
    response: &Response,
    start: u64,
    total: u64,
    served_before: bool,
) -> Result<(), HelperEnd> {
    let headers = response.headers();
    let validators = Validators::from_headers(headers);
    let changed = || {
        HelperEnd::Fatal(
            Failure::conflict("the file changed on the server during the download").with_hint(
                "the partial data was kept; run again with --restart to fetch the current \
                 version",
            ),
        )
    };
    match response.status().as_u16() {
        206 => {
            let Some(range) =
                header_str(headers, header::CONTENT_RANGE).and_then(parse_content_range)
            else {
                return Err(HelperEnd::Unsupported(
                    "the server sent a partial response without a valid Content-Range".into(),
                ));
            };
            if ctx.validators.conflicts_with(&validators) || range.total != Some(total) {
                return Err(changed());
            }
            if range.first != start {
                return Err(HelperEnd::Unsupported(
                    "the server answered a range request at the wrong position".into(),
                ));
            }
            if !identity_encoding(headers) {
                return Err(HelperEnd::Unsupported(
                    "the server compresses partial responses".into(),
                ));
            }
            Ok(())
        }
        200 => {
            let length_changed = response.content_length().is_some_and(|len| len != total);
            if ctx.validators.conflicts_with(&validators) || length_changed {
                return Err(changed());
            }
            Err(HelperEnd::Unsupported(
                "the server ignores range requests from additional connections".into(),
            ))
        }
        416 => Err(HelperEnd::Unsupported(
            "the server rejected a range request".into(),
        )),
        code if (200..300).contains(&code) => Err(HelperEnd::Unsupported(format!(
            "the server sent an unexpected HTTP {code} response to a range request"
        ))),
        code => match classify_status(response) {
            AttemptError::Retryable { .. } => Err(HelperEnd::Failed {
                throttled: code == 429 || code == 503,
            }),
            // A host that served ranges before but refuses another connection limits
            // connections; one that never served them does not offer this at all.
            AttemptError::Fatal(_) if served_before => Err(HelperEnd::Failed { throttled: true }),
            AttemptError::Fatal(failure) => Err(HelperEnd::Unsupported(format!(
                "an additional connection was refused: {failure}"
            ))),
        },
    }
}

enum Received {
    /// The segment is complete or was handed over.
    Done,
    /// The response ended early after this many bytes; the rest went back to the plan.
    Short(u64),
    Ended(HelperEnd),
}

/// Stream `response` into the segment `claim`; the response covers `[claim.start, response_end)`.
async fn receive(
    ctx: &Context,
    shared: &Shared,
    cancelled: &mut watch::Receiver<bool>,
    response: Response,
    claim: Claim,
    response_end: u64,
) -> Received {
    let _streaming = ctx.reporter.stream();
    let stall = ctx.settings.stall_timeout;
    let mut body = response.bytes_stream();
    let mut offset = claim.start;
    let release = |shared: &Shared| {
        shared.plan.lock().expect("plan lock").release(claim.id);
        shared.notify();
    };
    loop {
        let deadline = tokio::time::Instant::now() + stall;
        let next = tokio::select! {
            _ = ctx.stop.wait() => None,
            _ = cancelled.wait_for(|c| *c) => None,
            next = tokio::time::timeout_at(deadline, body.next()) => Some(next),
        };
        let chunk = match next {
            None => {
                release(shared);
                return Received::Ended(HelperEnd::Stopped);
            }
            Some(Err(_)) => {
                release(shared);
                return Received::Ended(HelperEnd::Failed { throttled: false });
            }
            Some(Ok(None)) => {
                release(shared);
                return Received::Short(offset - claim.start);
            }
            Some(Ok(Some(Err(_)))) => {
                release(shared);
                return Received::Ended(HelperEnd::Failed { throttled: false });
            }
            Some(Ok(Some(Ok(chunk)))) => chunk,
        };
        let len = chunk.len() as u64;
        let take = shared
            .plan
            .lock()
            .expect("plan lock")
            .reserve(claim.id, offset, len);
        if take > 0 {
            if ctx
                .writes
                .write(offset, chunk.slice(..take as usize))
                .await
                .is_err()
            {
                release(shared);
                return Received::Ended(HelperEnd::WriteFailed);
            }
            offset += take;
            ctx.reporter.add_bytes(take);
            if let Some(limiter) = &ctx.limiter {
                tokio::select! {
                    _ = ctx.stop.wait() => {}
                    _ = limiter.consume(take) => {}
                }
            }
        }
        if take < len || shared.plan.lock().expect("plan lock").finished(claim.id) {
            release(shared);
            if offset == response_end {
                // Read the end of the body so the connection can be reused.
                let _ = tokio::time::timeout(Duration::from_secs(1), body.next()).await;
            }
            return Received::Done;
        }
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The response carries the bytes of the representation itself.
pub fn identity_encoding(headers: &HeaderMap) -> bool {
    header_str(headers, header::CONTENT_ENCODING)
        .is_none_or(|e| e.trim().is_empty() || e.trim().eq_ignore_ascii_case("identity"))
}
