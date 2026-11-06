//! The transfer engine: the lead stream, and segmentation when it pays off.
//!
//! The lead is a single validated stream, continued with range requests after interruptions.
//! Continuation requires proof that the new bytes belong to the same representation: a strong
//! validator sent as `If-Range`, or a user-supplied checksum that gates publication. Without
//! either, the transfer restarts from the beginning rather than risk splicing two versions. A
//! detected source change stops the transfer and keeps the data.
//!
//! When the size is known, continuity can be proven, the bytes are not re-encoded, and enough
//! work remains (or saved progress has gaps), the transfer is segmented: a [`Plan`] tracks which
//! ranges are missing, the lead works through them, and the [`Crew`] keeps `--min-connections`
//! and adds more while the controller finds them useful. Otherwise, and always with `--max-connections 1` unless saved
//! progress has gaps, the lead streams the file alone from start to end.
//!
//! Progress saved by an earlier run is never discarded automatically: when it cannot be
//! continued safely, the transfer stops with a conflict and leaves the decision to the user.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{self, HeaderMap};
use reqwest::{Response, Version};
use tokio::sync::watch;
use url::Url;

use super::controller::Signal;
use super::crew::{Context, Crew, CrewFailure, identity_encoding};
use super::plan::{Claim, Plan, Steal};
use super::retry::{PROGRESS_BYTES, RetryState};
use super::{Stop, StopReason};
use crate::cli::Settings;
use crate::error::{Category, Failure, describe_io};
use crate::integrity::Checksum;
use crate::metrics::{Event, Phase, Reporter};
use crate::presentation::Completion;
use crate::presentation::text::redact_url;
use crate::recovery::{self, Found, Recovered};
use crate::state::State;
use crate::storage::{self, Checkpoints, Destination, Ledger, PartWriter};
use crate::transport::http::{Validators, content_disposition_filename, parse_content_range};
use crate::transport::{AttemptError, RangeRequest, Transport, classify_error, classify_status};

/// Segment only files of at least this many checkpoint blocks: smaller ones finish before an
/// extra connection could pay for its setup.
const MIN_SEGMENTED_BLOCKS: u64 = 4;
/// The lead splits another segment only when its owner has at least this much work left.
const LEAD_STEAL_SECS: f64 = 2.0;

/// What the first response revealed about the remote representation.
struct Resource {
    total: Option<u64>,
    validators: Validators,
    /// Cleared once the server shows it does not honor range requests.
    ranges: bool,
    /// The server advertised `Accept-Ranges: bytes` or served a valid partial response.
    ranges_seen: bool,
    /// The bytes are the representation itself, not a compressed encoding of it.
    identity: bool,
}

enum AttemptEnd {
    /// The file, or in a segmented transfer the lead's segment, is complete.
    Complete,
    /// Nothing failed; look for the next request (saved progress was found for the destination,
    /// a split was lost to its owner, or a helper ended the transfer).
    Again,
    Failed(AttemptError),
    Stopped(StopReason),
}

/// The next request of the lead: everything (`None`) or a range.
struct Span {
    start: u64,
    /// Exclusive; `None` runs to the end of the file.
    end: Option<u64>,
    if_range: Option<String>,
    /// The range belongs to another segment until the response is validated.
    steal: Option<Steal>,
}

enum Next {
    Request(Option<Span>),
    /// Every remaining range is with a helper; wait for a change.
    Wait(watch::Receiver<u64>),
    Complete,
}

/// What to do with an accepted response.
enum Accepted {
    Stream,
    Recovered,
    Published,
}

pub struct Engine<'a> {
    settings: &'a Settings,
    transport: &'a Arc<Transport>,
    reporter: &'a Reporter,
    stop: &'a Stop,
    resource: Option<Resource>,
    limiter: Option<Arc<Limiter>>,
    /// The expected checksum: from the command line, or saved with recovered progress.
    pub checksum: Option<Checksum>,
    /// The destination, once chosen.
    pub dest: Option<Destination>,
    /// The part-file writer, once the first response arrived.
    pub writer: Option<PartWriter>,
    /// Unsegmented: bytes handed to the writer, in order, from offset zero.
    received: u64,
    /// Bytes recovered from an earlier run. While nonzero, the transfer never starts over.
    prior: u64,
    /// A destination whose recovery files hold no data and may be taken over.
    reusable: Option<Destination>,
    /// The destination selected with `--resume-from`; the server's file name does not apply.
    pinned: Option<Destination>,
    /// An earlier run's complete file, found in place.
    pub published: Option<Completion>,
    /// The first partial response after recovery continues quietly.
    quiet_resume: bool,
    /// Segmented: the plan and the helper connections.
    crew: Option<Crew>,
    /// Segmented: the range the lead is receiving.
    segment: Option<Claim>,
    /// Segmented: the next partial response follows a failure and is announced as a resume.
    after_failure: bool,
    /// Time to first byte of the latest response.
    latency: Option<Duration>,
}

impl<'a> Engine<'a> {
    pub fn new(
        settings: &'a Settings,
        transport: &'a Arc<Transport>,
        reporter: &'a Reporter,
        stop: &'a Stop,
    ) -> Engine<'a> {
        Engine {
            settings,
            transport,
            reporter,
            stop,
            resource: None,
            limiter: settings.limit_rate.map(|rate| Arc::new(Limiter::new(rate))),
            checksum: settings.checksum.clone(),
            dest: None,
            writer: None,
            received: 0,
            prior: 0,
            reusable: None,
            pinned: None,
            published: None,
            quiet_resume: false,
            crew: None,
            segment: None,
            after_failure: false,
            latency: None,
        }
    }

    /// Bytes received for the file so far, including saved progress.
    pub fn progress(&self) -> u64 {
        match &self.crew {
            Some(crew) => crew.plan().done(),
            None => self.received,
        }
    }

    /// Transfer the whole representation into the part file, continuing what `found` saved.
    /// Helper connections have stopped and queued their last writes when this returns.
    pub async fn run(&mut self, found: Found) -> Result<(), Failure> {
        let result = self.transfer(found).await;
        if let Some(crew) = self.crew.as_mut() {
            crew.shutdown().await;
        }
        result
    }

    async fn transfer(&mut self, found: Found) -> Result<(), Failure> {
        match found {
            Found::Nothing => {
                if let Some(dest) = Destination::explicit(self.settings.output.as_deref()) {
                    dest.precheck(self.settings.restart)?;
                }
            }
            Found::Empty { dest, discarded } => {
                self.discarded_notice(&dest, discarded);
                if self.settings.resume_from.is_some() {
                    self.pinned = Some(dest.clone());
                }
                self.reusable = Some(dest);
            }
            Found::Resume(recovered) => self.adopt(*recovered)?,
            Found::Published(done) => {
                self.published = Some(done);
                return Ok(());
            }
            Found::Foreign(failure) => return Err(failure),
        }
        let mut retry = RetryState::new(self.settings.retry, Instant::now());
        loop {
            if let Some(reason) = self.stop.reason() {
                return Err(self.interrupted(reason));
            }
            if let Some(failure) = self.crew_failure().await {
                return Err(failure);
            }
            if self.crew.is_none()
                && self.writer.is_some()
                && self
                    .resource
                    .as_ref()
                    .is_some_and(|r| r.total == Some(self.received))
            {
                // Everything is already saved; nothing to request.
                return Ok(());
            }
            let before = self.progress();
            self.reporter.phase(Phase::Connecting);
            let span = match self.next().await? {
                Next::Request(span) => span,
                Next::Complete => return Ok(()),
                Next::Wait(mut changed) => {
                    tokio::select! {
                        reason = self.stop.wait() => return Err(self.interrupted(reason)),
                        _ = crew_failed(self.crew.as_ref()) => {}
                        _ = changed.changed() => {}
                    }
                    continue;
                }
            };
            let failure = match self.attempt(span).await {
                AttemptEnd::Complete if self.crew.is_some() => continue,
                AttemptEnd::Complete => return Ok(()),
                AttemptEnd::Again => continue,
                AttemptEnd::Stopped(reason) => return Err(self.interrupted(reason)),
                AttemptEnd::Failed(AttemptError::Fatal(failure)) => {
                    return Err(self.with_resume_hint(failure));
                }
                AttemptEnd::Failed(AttemptError::Retryable {
                    reason,
                    retry_after,
                }) => (reason, retry_after),
            };
            let (reason, retry_after) = failure;
            self.release_segment();
            if let Some(crew) = &self.crew {
                self.after_failure = true;
                crew.signal(
                    if retry_after.is_some() || reason.starts_with("server is busy") {
                        Signal::Throttled
                    } else {
                        Signal::Failed
                    },
                );
            }
            // Persist what arrived before waiting; the process may not survive the wait.
            self.checkpoint().await?;
            let progressed = self.progress().saturating_sub(before) >= PROGRESS_BYTES;
            let Some(delay) = retry.on_failure(progressed, retry_after, Instant::now()) else {
                let hint = match self.saved_hint() {
                    Some(saved) => format!("check the connection; {saved}; or use --retry forever"),
                    None => "check the connection and run again, or use --retry forever".into(),
                };
                return Err(Failure::new(
                    Category::Network,
                    format!("{reason}; {}", retry.describe_exhausted()),
                )
                .with_hint(hint));
            };
            let attempts = retry.attempts;
            self.reporter.update(|s| s.retries = attempts);
            self.reporter.event(Event::Retrying {
                attempt: attempts + 1,
                delay,
                reason: reason.clone(),
            });
            self.reporter.phase(Phase::Retrying {
                reason,
                attempt: attempts + 1,
                until: Instant::now() + delay,
            });
            tokio::select! {
                reason = self.stop.wait() => return Err(self.interrupted(reason)),
                _ = crew_failed(self.crew.as_ref()) => {}
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// The failure a helper connection reported, if any.
    async fn crew_failure(&mut self) -> Option<Failure> {
        let failure = self.crew.as_ref()?.take_failure()?;
        Some(match failure {
            CrewFailure::Fatal(failure) => self.with_resume_hint(failure),
            CrewFailure::Write => {
                let writer = self
                    .writer
                    .as_mut()
                    .expect("helpers write through the writer");
                let err = writer.error().await;
                write_failure(self.dest.as_ref(), &err)
            }
        })
    }

    /// Whether the transfer may proceed in segments: the size is known, every segment can be
    /// bound to this representation, and the bytes are not re-encoded. Returns the size.
    fn segmentable(&self) -> Option<u64> {
        let resource = self.resource.as_ref()?;
        let total = resource.total?;
        let bound = resource.validators.if_range().is_some() || self.checksum.is_some();
        (resource.ranges && resource.identity && bound).then_some(total)
    }

    /// Worth segmenting a transfer with `remaining` bytes left in blocks of `block_size`.
    fn worth_segmenting(&self, remaining: u64, block_size: u64) -> bool {
        self.settings.max_connections > 1 && remaining >= MIN_SEGMENTED_BLOCKS * block_size
    }

    fn start_crew(&mut self, plan: Plan) {
        let writer = self
            .writer
            .as_ref()
            .expect("segments are written by the writer");
        let resource = self.resource.as_ref().expect("segments need a resource");
        let context = (self.settings.max_connections > 1).then(|| Context {
            settings: Arc::new(self.settings.clone()),
            transport: self.transport.clone(),
            reporter: self.reporter.clone(),
            stop: self.stop.clone(),
            writes: writer.handle(),
            validators: resource.validators.clone(),
            limiter: self.limiter.clone(),
            min_connections: self.settings.min_connections,
            max_connections: self.settings.max_connections,
        });
        let crew = Crew::start(plan, context);
        if let Some(latency) = self.latency {
            crew.signal(Signal::Latency(latency));
        }
        self.crew = Some(crew);
    }

    /// Return the lead's unreceived range to the plan.
    fn release_segment(&mut self) {
        if let (Some(crew), Some(segment)) = (&self.crew, self.segment.take()) {
            crew.plan().release(segment.id);
            crew.notify();
        }
    }

    /// Continue from verified saved progress.
    fn adopt(&mut self, recovered: Recovered) -> Result<(), Failure> {
        let Recovered {
            dest,
            file,
            ledger,
            state,
            checksum,
            discarded,
        } = recovered;
        let saved = ledger.bytes();
        let total = state.total;
        let ranges = ledger.ranges();
        let gaps = ranges.len() > 1 || ledger.prefix() != saved;
        self.resource = Some(Resource {
            total,
            validators: state.validators.clone(),
            ranges: true,
            ranges_seen: false,
            identity: true,
        });
        self.checksum = checksum;
        self.received = ledger.prefix();
        self.prior = saved;
        self.quiet_resume = true;
        self.reporter.set_received(saved);
        self.reporter.update(|s| {
            s.name = Some(dest.file_name());
            s.total = total;
        });
        self.reporter.event(Event::Recovered {
            output: dest.path.clone(),
            saved,
            total,
            discarded,
        });
        let block_size = ledger.block_size();
        self.writer = Some(self.spawn_writer(file, ledger, state, &dest)?);
        self.dest = Some(dest);
        match self.segmentable() {
            Some(total) if gaps || self.worth_segmenting(total - saved, block_size) => {
                self.start_crew(Plan::new(total, block_size, &ranges));
            }
            Some(_) => {}
            None if gaps => {
                return Err(Failure::conflict(
                    "the saved progress has gaps that cannot be filled safely",
                )
                .with_hint("run again with --restart to download from the beginning"));
            }
            None => {}
        }
        Ok(())
    }

    fn spawn_writer(
        &self,
        file: std::fs::File,
        ledger: Ledger,
        state: State,
        dest: &Destination,
    ) -> Result<PartWriter, Failure> {
        let checkpoints = Checkpoints {
            state,
            path: dest.state.clone(),
            tmp: dest.state_tmp.clone(),
            interval: self.settings.checkpoint_interval,
            durable: self.reporter.durable(),
        };
        PartWriter::spawn(file, ledger, checkpoints).map_err(|err| write_failure(Some(dest), &err))
    }

    async fn checkpoint(&mut self) -> Result<(), Failure> {
        if let Some(writer) = self.writer.as_mut()
            && let Err(err) = writer.checkpoint().await
        {
            return Err(write_failure(self.dest.as_ref(), &err));
        }
        Ok(())
    }

    /// Saved data that failed verification is about to be replaced; say so.
    fn discarded_notice(&self, dest: &Destination, discarded: u64) {
        if discarded > 0 {
            self.reporter.event(Event::Notice {
                message: format!(
                    "the saved data in {} ({}) could not be verified; downloading it again",
                    dest.part.display(),
                    crate::presentation::text::bytes(discarded)
                ),
            });
        }
    }

    /// Whether a later run can continue the saved progress automatically.
    fn resumable(&self) -> bool {
        self.progress() > 0
            && self.dest.is_some()
            && (self.checksum.is_some()
                || self
                    .resource
                    .as_ref()
                    .is_some_and(|r| r.validators.if_range().is_some()))
    }

    /// What to tell the user about saved progress, promising a resume only when the server
    /// has shown that it honors range requests.
    fn saved_hint(&self) -> Option<&'static str> {
        if !self.resumable() {
            return None;
        }
        Some(if self.resource.as_ref().is_some_and(|r| r.ranges_seen) {
            "progress is saved; run the same command again to resume"
        } else {
            "progress is saved; running the same command again resumes it if the server \
             supports range requests"
        })
    }

    fn interrupted(&self, reason: StopReason) -> Failure {
        let failure = interrupted(reason);
        match self.saved_hint() {
            Some(hint) => failure.with_hint(hint),
            None => failure,
        }
    }

    /// Point to `--resume-from` when the server refuses the request but progress is saved,
    /// typically because a signed link expired.
    fn with_resume_hint(&self, mut failure: Failure) -> Failure {
        if failure.category != Category::Remote || !self.resumable() {
            return failure;
        }
        let Some(dest) = &self.dest else {
            return failure;
        };
        let resume = format!(
            "progress is saved; with a new link, run: vayu \"NEW_URL\" --resume-from {}",
            dest.state.display()
        );
        failure.hint = Some(match failure.hint.take() {
            Some(hint) => format!("{hint}; {resume}"),
            None => resume,
        });
        failure
    }

    /// Decide what the lead requests next.
    async fn next(&mut self) -> Result<Next, Failure> {
        let Some(crew) = &self.crew else {
            return Ok(Next::Request(self.plan_range().await?));
        };
        let resource = self.resource.as_ref().expect("segments need a resource");
        if !resource.ranges {
            self.restart_from_zero("the server cannot resume this download")
                .await?;
            return Ok(Next::Request(None));
        }
        let total = resource.total.expect("segments need a size");
        let if_range = resource.validators.if_range().map(str::to_string);
        let changed = crew.watch();
        let now = Instant::now();
        let mut plan = crew.plan();
        if plan.complete() {
            return Ok(Next::Complete);
        }
        let end = |e: u64| (e < total).then_some(e);
        if let Some(claim) = plan.claim(0, now) {
            drop(plan);
            self.segment = Some(claim);
            return Ok(Next::Request(Some(Span {
                start: claim.start,
                end: end(claim.end),
                if_range,
                steal: None,
            })));
        }
        Ok(match plan.propose(now, LEAD_STEAL_SECS) {
            Some(steal) => Next::Request(Some(Span {
                start: steal.start,
                end: end(steal.end),
                if_range,
                steal: Some(steal),
            })),
            None => Next::Wait(changed),
        })
    }

    /// Unsegmented: decide whether the next request continues from the current offset.
    async fn plan_range(&mut self) -> Result<Option<Span>, Failure> {
        if self.received == 0 {
            return Ok(None);
        }
        let resource = self.resource.as_ref().expect("bytes imply a response");
        let if_range = resource.validators.if_range().map(str::to_string);
        if resource.ranges && (if_range.is_some() || self.checksum.is_some()) {
            return Ok(Some(Span {
                start: self.received,
                end: None,
                if_range,
                steal: None,
            }));
        }
        let cause = if resource.ranges {
            "the server offers no way to verify resumed data"
        } else {
            "the server cannot resume this download"
        };
        self.restart_from_zero(cause).await?;
        Ok(None)
    }

    /// Start over within this run. Progress saved by an earlier run is kept instead: the
    /// transfer stops so the user can decide.
    async fn restart_from_zero(&mut self, cause: &str) -> Result<(), Failure> {
        if self.prior > 0 {
            let part = self
                .dest
                .as_ref()
                .map(|d| d.part.display().to_string())
                .unwrap_or_default();
            return Err(Failure::conflict(format!(
                "{cause}; the saved progress cannot be continued safely"
            ))
            .with_hint(format!(
                "the partial data was kept in {part}; run again with --restart to download from \
                 the beginning"
            )));
        }
        // Helpers stop first, so none of their writes arrive after the reset.
        if let Some(mut crew) = self.crew.take() {
            crew.shutdown().await;
        }
        self.segment = None;
        if let Some(writer) = self.writer.as_mut()
            && let Err(err) = writer.reset().await
        {
            return Err(write_failure(self.dest.as_ref(), &err));
        }
        self.received = 0;
        self.reporter.set_received(0);
        self.reporter.event(Event::Restarted {
            reason: format!("{cause}; restarting from the beginning"),
        });
        Ok(())
    }

    async fn attempt(&mut self, span: Option<Span>) -> AttemptEnd {
        let request = span.as_ref().map(|s| RangeRequest {
            start: s.start,
            end: s.end,
            if_range: s.if_range.as_deref(),
        });
        let reporter = self.reporter;
        let stop = self.stop;
        let stall = self.settings.stall_timeout;
        let sent = Instant::now();
        let send = self.transport.get(0, &self.settings.url, request, |url| {
            reporter.event(Event::Redirected {
                to: redact_url(url),
            })
        });
        let (final_url, response) = tokio::select! {
            reason = stop.wait() => return AttemptEnd::Stopped(reason),
            _ = crew_failed(self.crew.as_ref()) => return AttemptEnd::Again,
            result = tokio::time::timeout(stall, send) => match result {
                Err(_) => {
                    return AttemptEnd::Failed(AttemptError::Retryable {
                        reason: format!("no response for {}", seconds(stall)),
                        retry_after: None,
                    })
                }
                Ok(Err(err)) => return AttemptEnd::Failed(err),
                Ok(Ok(pair)) => pair,
            },
        };
        let latency = sent.elapsed();
        self.latency = Some(latency);
        if let Some(crew) = &self.crew {
            crew.signal(Signal::Latency(latency));
        }
        let requested = span.as_ref().map(|s| s.start);
        match self.accept(&final_url, &response, requested).await {
            Err(err) => AttemptEnd::Failed(err),
            Ok(Accepted::Stream) => {
                // A full response (after a restart) starts at zero; a partial one where asked.
                let start = match requested {
                    Some(start) if response.status().as_u16() == 206 => start,
                    _ => 0,
                };
                if let (Some(crew), Some(steal)) = (&self.crew, span.and_then(|s| s.steal)) {
                    match crew.plan().commit(steal, 0, Instant::now()) {
                        Some(claim) => self.segment = Some(claim),
                        // The owner got there first; this response is not needed.
                        None => return AttemptEnd::Again,
                    }
                }
                self.stream(response, start).await
            }
            Ok(Accepted::Recovered) => AttemptEnd::Again,
            Ok(Accepted::Published) => AttemptEnd::Complete,
        }
    }

    /// Check a response against what is already on disk.
    async fn accept(
        &mut self,
        final_url: &Url,
        response: &Response,
        range_start: Option<u64>,
    ) -> Result<Accepted, AttemptError> {
        let headers = response.headers();
        let validators = Validators::from_headers(headers);
        let fatal = |f: Failure| AttemptError::Fatal(f);
        if response.status().is_success() {
            let http_version = version_label(response.version());
            self.reporter
                .update(|s| s.http_version = Some(http_version));
        }
        match response.status().as_u16() {
            206 => {
                let Some(range) =
                    header_str(headers, header::CONTENT_RANGE).and_then(parse_content_range)
                else {
                    return Err(fatal(Failure::remote(
                        "the server sent a partial response without a valid Content-Range",
                    )));
                };
                let Some(start) = range_start else {
                    // Partial content for a plain request is usable only if it is everything.
                    if range.first != 0 || range.total != Some(range.last + 1) {
                        return Err(fatal(Failure::remote(
                            "the server sent an unexpected partial response",
                        )));
                    }
                    let Some(resource) = self.resource.as_ref() else {
                        return self.begin(final_url, response, range.total, validators);
                    };
                    let total_changed = matches!(resource.total, Some(t) if range.total != Some(t));
                    if resource.validators.conflicts_with(&validators) || total_changed {
                        return Err(fatal(source_changed()));
                    }
                    return Ok(Accepted::Stream);
                };
                let resource = self.resource.as_mut().expect("range implies a response");
                let total_changed =
                    matches!((resource.total, range.total), (Some(a), Some(b)) if a != b);
                if resource.validators.conflicts_with(&validators) || total_changed {
                    return Err(fatal(source_changed()));
                }
                if range.first != start {
                    resource.ranges = false;
                    return Err(AttemptError::Retryable {
                        reason: "the server resumed at the wrong position".into(),
                        retry_after: None,
                    });
                }
                resource.ranges_seen = true;
                if resource.total.is_none() && range.total.is_some() {
                    resource.total = range.total;
                    self.reporter.update(|s| s.total = range.total);
                }
                if std::mem::take(&mut self.quiet_resume) {
                    // Already announced as recovered progress.
                } else if self.crew.is_none() || std::mem::take(&mut self.after_failure) {
                    // Segmented transfers request ranges routinely; announce only reconnections.
                    self.reporter.event(Event::Resumed { offset: start });
                }
                Ok(Accepted::Stream)
            }
            200 => {
                let total = response.content_length();
                let Some(resource) = self.resource.as_mut() else {
                    return self.begin(final_url, response, total, validators);
                };
                let total_changed = matches!((resource.total, total), (Some(a), Some(b)) if a != b);
                if resource.validators.conflicts_with(&validators) || total_changed {
                    return Err(fatal(source_changed()));
                }
                if range_start.is_some() {
                    // Same representation, but the server ignored the range: start over.
                    resource.ranges = false;
                    self.restart_from_zero("the server did not resume")
                        .await
                        .map_err(fatal)?;
                }
                Ok(Accepted::Stream)
            }
            416 if range_start.is_some() => {
                self.restart_from_zero("the server rejected the resume position")
                    .await
                    .map_err(fatal)?;
                Err(AttemptError::Retryable {
                    reason: "the server rejected the resume position".into(),
                    retry_after: None,
                })
            }
            code if (200..300).contains(&code) => Err(fatal(Failure::remote(format!(
                "the server sent an unexpected HTTP {code} response"
            )))),
            _ => Err(classify_status(response)),
        }
    }

    /// First usable response: choose the destination, then continue its saved progress or
    /// open a new part file.
    fn begin(
        &mut self,
        final_url: &Url,
        response: &Response,
        total: Option<u64>,
        validators: Validators,
    ) -> Result<Accepted, AttemptError> {
        let fatal = AttemptError::Fatal;
        let headers = response.headers();
        let suggested = header_str(headers, header::CONTENT_DISPOSITION)
            .and_then(content_disposition_filename)
            .and_then(|name| storage::sanitize_filename(&name))
            .or_else(|| storage::filename_from_url(&self.settings.url))
            .or_else(|| storage::filename_from_url(final_url))
            .unwrap_or_else(|| storage::FALLBACK_NAME.to_string());
        let dest = match &self.pinned {
            Some(pinned) => pinned.clone(),
            None => Destination::resolve(self.settings.output.as_deref(), &suggested),
        };

        let mut take_over = self.settings.restart || self.reusable.as_ref() == Some(&dest);
        if !take_over {
            let found = tokio::task::block_in_place(|| {
                recovery::examine(&dest, self.settings, false, self.reporter)
            })
            .map_err(fatal)?;
            match found {
                Found::Nothing => {}
                Found::Empty { discarded, .. } => {
                    self.discarded_notice(&dest, discarded);
                    take_over = true;
                }
                Found::Resume(recovered) => {
                    let saved = &recovered.state;
                    let total_changed =
                        matches!((saved.total, total), (Some(a), Some(b)) if a != b);
                    if saved.validators.conflicts_with(&validators) || total_changed {
                        return Err(fatal(source_changed()));
                    }
                    self.adopt(*recovered).map_err(fatal)?;
                    return Ok(Accepted::Recovered);
                }
                Found::Published(done) => {
                    self.published = Some(done);
                    return Ok(Accepted::Published);
                }
                Found::Foreign(failure) => return Err(fatal(failure)),
            }
        }
        let file = dest.open_part(take_over).map_err(fatal)?;
        let state = State::new(
            &self.settings.url,
            dest.file_name(),
            total,
            validators.clone(),
            self.checksum.as_ref(),
        );
        let block_size = state.block_size;
        let ledger = Ledger::new(block_size);
        let writer = self
            .spawn_writer(file, ledger, state, &dest)
            .map_err(fatal)?;

        let accept_ranges = header_str(headers, header::ACCEPT_RANGES).map(str::trim);
        let ranges = accept_ranges.is_none_or(|v| !v.eq_ignore_ascii_case("none"));
        let ranges_seen = accept_ranges.is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
        let resumable = ranges && (validators.if_range().is_some() || self.checksum.is_some());
        let http_version = version_label(response.version());
        let name = dest.file_name();
        self.reporter.update(|s| {
            s.name = Some(name);
            s.total = total;
        });
        self.reporter.event(Event::Responded {
            output: dest.path.clone(),
            total,
            http_version,
            resumable,
        });
        self.resource = Some(Resource {
            total,
            validators,
            ranges,
            ranges_seen,
            identity: identity_encoding(headers),
        });
        self.dest = Some(dest);
        self.writer = Some(writer);
        if let Some(total) = self.segmentable()
            && self.worth_segmenting(total, block_size)
        {
            self.start_crew(Plan::new(total, block_size, &[]));
            let crew = self.crew.as_ref().expect("just started");
            self.segment = crew.plan().claim(0, Instant::now());
        }
        Ok(Accepted::Stream)
    }

    /// Receive `response`, whose body belongs at `start`, until it ends or, in a segmented
    /// transfer, until the lead's segment is complete.
    async fn stream(&mut self, response: Response, start: u64) -> AttemptEnd {
        let total = self.resource.as_ref().and_then(|r| r.total);
        let response_end = response.content_length().map(|len| start + len);
        let stop = self.stop;
        let stall = self.settings.stall_timeout;
        let mut body = response.bytes_stream();
        self.reporter.phase(Phase::Downloading);
        let _streaming = self.reporter.stream();
        let mut offset = start;
        loop {
            let next = tokio::select! {
                reason = stop.wait() => return AttemptEnd::Stopped(reason),
                _ = crew_failed(self.crew.as_ref()) => return AttemptEnd::Again,
                next = tokio::time::timeout(stall, body.next()) => next,
            };
            let chunk = match next {
                Err(_) => {
                    return AttemptEnd::Failed(AttemptError::Retryable {
                        reason: format!("connection stalled (no data for {})", seconds(stall)),
                        retry_after: None,
                    });
                }
                Ok(None) => break,
                Ok(Some(Err(err))) => {
                    return AttemptEnd::Failed(classify_error(&err, &self.settings.url));
                }
                Ok(Some(Ok(chunk))) => chunk,
            };
            let len = chunk.len() as u64;
            let take = match (&self.crew, self.segment) {
                (Some(crew), Some(segment)) => crew.plan().reserve(segment.id, offset, len),
                _ if total.is_some_and(|t| offset + len > t) => {
                    return AttemptEnd::Failed(AttemptError::Fatal(Failure::remote(
                        "the server sent more data than it announced",
                    )));
                }
                _ => len,
            };
            if take > 0 {
                let writer = self
                    .writer
                    .as_mut()
                    .expect("writer exists after a response");
                if let Err(err) = writer.write(offset, chunk.slice(..take as usize)).await {
                    return AttemptEnd::Failed(AttemptError::Fatal(write_failure(
                        self.dest.as_ref(),
                        &err,
                    )));
                }
                offset += take;
                if self.crew.is_none() {
                    self.received = offset;
                }
                self.reporter.add_bytes(take);
                if let Some(limiter) = &self.limiter {
                    tokio::select! {
                        reason = stop.wait() => return AttemptEnd::Stopped(reason),
                        _ = limiter.consume(take) => {}
                    }
                }
            }
            if let (Some(crew), Some(segment)) = (&self.crew, self.segment)
                && (take < len || crew.plan().finished(segment.id))
            {
                self.release_segment();
                if response_end == Some(offset) {
                    // Read the end of the body so the connection can be reused.
                    let _ = tokio::time::timeout(Duration::from_secs(1), body.next()).await;
                }
                return AttemptEnd::Complete;
            }
        }
        if let (Some(crew), Some(segment)) = (&self.crew, self.segment) {
            if crew.plan().finished(segment.id) {
                self.release_segment();
                return AttemptEnd::Complete;
            }
            return AttemptEnd::Failed(AttemptError::Retryable {
                reason: "connection closed early".into(),
                retry_after: None,
            });
        }
        match total {
            Some(total) if self.received < total => AttemptEnd::Failed(AttemptError::Retryable {
                reason: "connection closed early".into(),
                retry_after: None,
            }),
            _ => AttemptEnd::Complete,
        }
    }
}

/// Resolves once a helper connection has ended the transfer; never without helpers.
async fn crew_failed(crew: Option<&Crew>) {
    match crew {
        Some(crew) => crew.failed().await,
        None => std::future::pending().await,
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn version_label(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "HTTP",
    }
}

fn seconds(d: Duration) -> String {
    format!("{}s", d.as_secs_f64().round())
}

pub fn interrupted(reason: StopReason) -> Failure {
    Failure::new(Category::Interrupted(reason.exit_code()), reason.describe())
}

fn source_changed() -> Failure {
    Failure::conflict("the file changed on the server during the download").with_hint(
        "the partial data was kept; run again with --restart to fetch the current version",
    )
}

pub fn write_failure(dest: Option<&Destination>, err: &std::io::Error) -> Failure {
    let target = dest
        .map(|d| d.part.display().to_string())
        .unwrap_or_else(|| "the partial file".into());
    let failure = Failure::storage(format!("cannot write {target}: {}", describe_io(err)));
    match err.kind() {
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => failure.with_hint(
            "free up disk space, then run the same command again to resume from the last saved \
             point",
        ),
        _ => failure,
    }
}

/// Average-rate limiter with a one-second burst allowance, shared by all connections.
pub struct Limiter {
    rate: f64,
    /// When measurement started, and the bytes counted since.
    window: Mutex<(Instant, u64)>,
}

impl Limiter {
    fn new(rate: u64) -> Limiter {
        Limiter {
            rate: rate as f64,
            window: Mutex::new((Instant::now(), 0)),
        }
    }

    pub async fn consume(&self, n: u64) {
        let wait = {
            let mut window = self.window.lock().expect("limiter lock");
            let (start, sent) = &mut *window;
            *sent += n;
            let due = Duration::from_secs_f64(*sent as f64 / self.rate);
            let elapsed = start.elapsed();
            if elapsed > due + Duration::from_secs(1) {
                // After an idle period (e.g. a retry), do not allow a catch-up burst.
                *window = (Instant::now(), 0);
            }
            due.checked_sub(elapsed)
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }
}
