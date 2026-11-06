//! Adaptive concurrency: how many connections a segmented transfer should use.
//!
//! Hill climbing on measured useful throughput (bytes accepted for the file, so retransmitted or
//! discarded bytes never count). The transfer starts with the minimum number of connections, one
//! unless configured otherwise. After a warm-up, and only while enough work remains to repay a
//! connection's setup, the controller probes: it adds connections and waits for them to ramp up.
//! The wait scales with the observed time to first byte, so high-latency links get longer windows.
//!
//! Probing starts fast, like TCP slow start: each probe doubles the connection count, so a link
//! that rewards many connections reaches them in a few probes, and each probe's effect is large
//! enough to stand out from noise. Doubling continues while the added connections carry nearly as
//! much as the existing ones ([`FAST_KEEP`]). When a doubling helps less than that, throughput is
//! no longer rising in step with connections, and the two measurements are fitted to a link that
//! gives each connection the same rate up to a shared capacity: the count drops to the smallest
//! that fills the capacity (but never below where the probe started), and probing continues one
//! connection at a time.
//!
//! A single added connection stays when aggregate throughput improved by enough: [`GAIN`] of the
//! total, or half an average connection's share ([`SHARE`]) when that is smaller, but never less
//! than [`MIN_GAIN`]. A fixed fraction of the total would stop growth near `1 / GAIN` connections
//! even when each one adds as much as the others. A connection that does not help is retired and
//! the next probe waits for a cooldown that doubles after every fruitless probe.
//!
//! Throttling responses (429, 503) halve the connection count, other failures on extra
//! connections remove one (or abandon a probe), and a server that does not serve ranges to extra
//! connections ends segmentation. Throttling and failures end fast probing. A disk that cannot
//! keep up stops growth, since more connections cannot help. A level whose throughput falls well
//! below what a smaller measured level achieved is abandoned for that level.
//!
//! Decisions based on throughput never go below the minimum. Throttling and failures may, since
//! the server asked for fewer connections; after [`COOLDOWN`], the count returns to the minimum
//! without a probe.
//!
//! The controller is a pure state machine over time and byte counts; the supervisor feeds it and
//! applies its target.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Relative throughput improvement that always justifies keeping one more connection.
pub const GAIN: f64 = 0.10;
/// Fraction of an average connection's throughput that also justifies keeping one more.
pub const SHARE: f64 = 0.5;
/// Below this relative improvement a connection is never kept: smaller gains are noise.
pub const MIN_GAIN: f64 = 0.05;
/// Fast probing continues while added connections carry at least this fraction of an average
/// existing connection's throughput.
pub const FAST_KEEP: f64 = 0.6;
/// The first connection's ramp-up before it is measured.
const WARMUP: Duration = Duration::from_secs(1);
/// Throughput comparison window, at minimum.
const MEASURE: Duration = Duration::from_millis(1500);
/// A new connection's ramp-up after its first byte, at minimum.
const RAMP: Duration = Duration::from_millis(500);
/// Growth needs at least this much remaining transfer time at the current rate (and eight
/// times the time to first byte): a connection that does not help is retired cheaply, but one
/// that arrives as the transfer ends cannot help.
const MIN_REMAINING: Duration = Duration::from_secs(2);
const COOLDOWN: Duration = Duration::from_secs(15);
const MAX_COOLDOWN: Duration = Duration::from_secs(120);
/// A probe that has not delivered a byte by then is abandoned, at minimum.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Window and factor for detecting that the current level does worse than a smaller one did.
const WORSE_WINDOW: Duration = Duration::from_secs(8);
const WORSE_FACTOR: f64 = 0.8;
const HISTORY: Duration = Duration::from_secs(30);

/// What the transfer looks like at one tick.
#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub now: Instant,
    /// Useful bytes received so far, from all connections.
    pub bytes: u64,
    pub remaining: u64,
    /// Another connection would find work.
    pub has_work: bool,
    /// The writer's queue is nearly full: the disk is the bottleneck.
    pub disk_busy: bool,
}

/// Outcomes reported by the connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// A probing connection received its first byte after this time to first byte.
    Connected(Duration),
    /// A time to first byte measured on another request.
    Latency(Duration),
    /// A probing connection found no work before its first byte and ended.
    Unused,
    /// The server asked to slow down (429, 503).
    Throttled,
    /// A connection failed, stalled, or closed early.
    Failed,
    /// The server does not serve ranges to additional connections.
    Unsupported,
}

#[derive(Clone, Copy, Debug)]
enum State {
    /// Waiting for throughput to settle at the current level.
    Settling { until: Instant },
    /// Measured at the current level since `since`.
    Steady { since: Instant },
    /// Connections were added to `base`; they are judged once all have ramped up.
    Probing {
        baseline: f64,
        base: u16,
        launched: Instant,
        /// Added connections that have not delivered a byte yet.
        waiting: u16,
        /// When the last added connection delivered its first byte.
        connected: Option<Instant>,
        /// The first measurement window passed; a second, longer one decides.
        passed: bool,
    },
}

pub struct Controller {
    min: u16,
    max: u16,
    target: u16,
    state: State,
    /// Probes double the count; cleared once doubling stops paying off.
    fast: bool,
    /// No growth before this time.
    hold: Option<Instant>,
    /// No return to the minimum before this time.
    restore: Option<Instant>,
    cooldown: Duration,
    /// Smoothed time to first byte.
    setup: Option<Duration>,
    samples: VecDeque<(Instant, u64)>,
    /// Throughput measured at each level (index = connections).
    levels: Vec<Option<f64>>,
}

impl Controller {
    /// `min` is clamped to `1..=max`.
    pub fn new(min: u16, max: u16, now: Instant) -> Controller {
        let max = max.max(1);
        let min = min.clamp(1, max);
        Controller {
            min,
            max,
            target: min,
            state: State::Settling {
                until: now + WARMUP,
            },
            fast: true,
            hold: None,
            restore: None,
            cooldown: COOLDOWN,
            setup: None,
            samples: VecDeque::new(),
            levels: vec![None; usize::from(max) + 1],
        }
    }

    #[cfg(test)]
    pub fn target(&self) -> u16 {
        self.target
    }

    /// Whether a probe is being evaluated.
    pub fn probing(&self) -> bool {
        matches!(self.state, State::Probing { .. })
    }

    pub fn signal(&mut self, now: Instant, signal: Signal) {
        match signal {
            Signal::Connected(ttfb) => {
                self.note_latency(ttfb);
                if let State::Probing {
                    waiting,
                    connected: connected @ None,
                    ..
                } = &mut self.state
                {
                    *waiting = waiting.saturating_sub(1);
                    if *waiting == 0 {
                        *connected = Some(now);
                    }
                }
            }
            Signal::Latency(ttfb) => self.note_latency(ttfb),
            Signal::Unused => {
                if let State::Probing {
                    base,
                    waiting,
                    connected: connected @ None,
                    ..
                } = &mut self.state
                    && *waiting > 0
                {
                    // Judge the probe on the connections that started; it was not a failure.
                    *waiting -= 1;
                    if *waiting == 0 {
                        *connected = Some(now);
                    }
                    let base = *base;
                    self.target -= 1;
                    if self.target == base {
                        self.state = State::Settling {
                            until: now + self.ramp(),
                        };
                    }
                }
            }
            Signal::Throttled => {
                self.fast = false;
                let cut = (self.target / 2).max(1);
                self.back_off(now, cut, 1);
            }
            Signal::Failed => {
                self.fast = false;
                let by = match self.state {
                    State::Probing { base, .. } => self.target - base,
                    _ => 1,
                };
                self.back_off(now, by, 1);
            }
            Signal::Unsupported => {
                self.min = 1;
                self.max = 1;
                self.target = 1;
                self.state = State::Steady { since: now };
            }
        }
    }

    pub fn tick(&mut self, obs: Observation) -> u16 {
        let now = obs.now;
        self.samples.push_back((now, obs.bytes));
        while self
            .samples
            .front()
            .is_some_and(|&(t, _)| now.duration_since(t) > HISTORY)
        {
            self.samples.pop_front();
        }
        match self.state {
            State::Settling { until } => {
                if now >= until {
                    self.state = State::Steady { since: now };
                }
            }
            State::Steady { since } => {
                if self.target < self.min && self.restore.is_none_or(|r| now >= r) {
                    self.target = self.min;
                    self.state = State::Settling {
                        until: now + self.ramp(),
                    };
                } else if let Some(fewer) = self.worse_than_fewer(now, since) {
                    self.back_off(now, self.target - fewer, self.min);
                } else if self.may_grow(&obs, since)
                    // Up to two windows: the longer the baseline, the less noise in the decision,
                    // but the first probe should not wait for it.
                    && let Some(baseline) =
                        self.rate(now, now.duration_since(since).min(self.measure() * 2))
                {
                    self.levels[usize::from(self.target)] = Some(baseline);
                    let step = if self.fast { self.target } else { 1 };
                    let step = step.min(self.max - self.target);
                    let base = self.target;
                    self.target += step;
                    self.state = State::Probing {
                        baseline,
                        base,
                        launched: now,
                        waiting: step,
                        connected: None,
                        passed: false,
                    };
                }
            }
            State::Probing {
                baseline,
                base,
                launched,
                connected,
                passed,
                ..
            } => match connected {
                None if now.duration_since(launched) > self.probe_timeout() => {
                    self.fast = false;
                    self.back_off(now, self.target - base, self.min);
                }
                None => {}
                Some(at) if !passed && now >= at + self.ramp() + self.measure() => {
                    // A probe that fails one window is judged at once; one that passes must
                    // pass again over a window twice as long before anything is kept.
                    let rate = self.rate(now, self.measure()).unwrap_or(0.0);
                    if self.gain(baseline, rate) >= self.required(base) {
                        if let State::Probing { passed, .. } = &mut self.state {
                            *passed = true;
                        }
                    } else {
                        self.judge(now, base, baseline, rate);
                    }
                }
                Some(at) if passed && now >= at + self.ramp() + self.measure() * 2 => {
                    let rate = self.rate(now, self.measure() * 2).unwrap_or(0.0);
                    self.judge(now, base, baseline, rate);
                }
                Some(_) => {}
            },
        }
        self.target
    }

    /// Decide on a probe from `base` connections at `baseline` to `self.target` at `rate`.
    fn judge(&mut self, now: Instant, base: u16, baseline: f64, rate: f64) {
        let level = self.target;
        let added = f64::from(level - base);
        let gain = self.gain(baseline, rate);
        // The gain if every added connection carried as much as an average existing one.
        let linear = added / f64::from(base);
        let in_step = gain >= FAST_KEEP * linear;
        if level - base == 1 {
            if in_step {
                self.keep(now, level, rate);
            } else if gain >= self.required(base) {
                self.fast = false;
                self.keep(now, level, rate);
            } else {
                self.fast = false;
                self.back_off(now, 1, self.min);
            }
            return;
        }
        // Fit equal per-connection rates up to a shared capacity, and keep the fewest
        // connections that reach it.
        let per = baseline / f64::from(base);
        let fit = if per > 0.0 { rate / per } else { 0.0 };
        // A fraction of a connection is rounded up only when that last connection would bring
        // the gain a single added connection must, so noise does not keep one that carries
        // nothing.
        let whole = (fit.floor() as u16).max(1);
        let fit = (fit - fit * self.required(whole)).ceil().max(0.0);
        let fit = (fit as u16).clamp(base, level);
        if fit == base || gain < self.required(base) {
            self.fast = false;
            self.back_off(now, level - base, self.min);
            return;
        }
        if fit == level {
            // Doubling continues only while every added connection pulls its weight.
            self.fast &= in_step;
            self.keep(now, level, rate);
            return;
        }
        // Throughput did not rise in step.
        self.fast = false;
        self.levels[usize::from(level)] = Some(rate);
        self.levels[usize::from(fit)] = Some(rate);
        self.target = fit;
        self.cooldown = COOLDOWN;
        // The fit says one more would not help; wait before testing that.
        self.hold = Some(now + COOLDOWN);
        self.state = State::Settling {
            until: now + self.ramp(),
        };
    }

    /// The probe paid off: stay at `level`, measured at `rate`.
    fn keep(&mut self, now: Instant, level: u16, rate: f64) {
        self.levels[usize::from(level)] = Some(rate);
        self.cooldown = COOLDOWN;
        // The window just measured is this level's baseline for the next probe.
        self.state = State::Steady {
            since: now - self.measure() * 2,
        };
    }

    fn gain(&self, baseline: f64, rate: f64) -> f64 {
        if baseline > 0.0 {
            rate / baseline - 1.0
        } else {
            0.0
        }
    }

    /// Relative improvement one more connection must bring at `base` connections.
    fn required(&self, base: u16) -> f64 {
        GAIN.min(SHARE / f64::from(base)).max(MIN_GAIN)
    }

    fn may_grow(&self, obs: &Observation, since: Instant) -> bool {
        let now = obs.now;
        if self.target < self.min
            || self.target >= self.max
            || !obs.has_work
            || obs.disk_busy
            || self.hold.is_some_and(|h| now < h)
            || now.duration_since(since) < self.measure()
        {
            return false;
        }
        let Some(rate) = self.rate(now, self.measure()).filter(|&r| r > 0.0) else {
            return false;
        };
        let worthwhile = MIN_REMAINING.max(self.setup.unwrap_or_default() * 8);
        obs.remaining as f64 / rate >= worthwhile.as_secs_f64()
    }

    /// The largest smaller level whose measured throughput the current level has fallen well
    /// below.
    fn worse_than_fewer(&self, now: Instant, since: Instant) -> Option<u16> {
        if self.target <= self.min || now.duration_since(since) < WORSE_WINDOW {
            return None;
        }
        let (fewer, rate) = (self.min..self.target)
            .rev()
            .find_map(|l| self.levels[usize::from(l)].map(|r| (l, r)))?;
        self.rate(now, WORSE_WINDOW)
            .is_some_and(|now_rate| now_rate < rate * WORSE_FACTOR)
            .then_some(fewer)
    }

    /// Remove `by` connections, keeping at least `floor`, and wait before trying more.
    fn back_off(&mut self, now: Instant, by: u16, floor: u16) {
        self.target = self.target.saturating_sub(by).max(floor).max(1);
        self.hold = Some(now + self.cooldown);
        // Fixed, unlike the growth cooldown: probes that did not help say nothing about how soon
        // the server takes the minimum again.
        self.restore = Some(now + COOLDOWN);
        self.cooldown = (self.cooldown * 2).min(MAX_COOLDOWN);
        self.state = State::Settling {
            until: now + self.ramp(),
        };
    }

    fn note_latency(&mut self, ttfb: Duration) {
        self.setup = Some(match self.setup {
            Some(old) => old.mul_f64(0.7) + ttfb.mul_f64(0.3),
            None => ttfb,
        });
    }

    fn setup(&self) -> Duration {
        self.setup.unwrap_or_default()
    }

    fn measure(&self) -> Duration {
        MEASURE.max(self.setup() * 6)
    }

    fn ramp(&self) -> Duration {
        RAMP.max(self.setup() * 3)
    }

    fn probe_timeout(&self) -> Duration {
        PROBE_TIMEOUT.max(self.setup() * 20)
    }

    /// Bytes per second over the last `window`, if the samples span most of it.
    fn rate(&self, now: Instant, window: Duration) -> Option<f64> {
        let &(last_t, last_b) = self.samples.back()?;
        let &(first_t, first_b) = self
            .samples
            .iter()
            .find(|&&(t, _)| now.duration_since(t) <= window)?;
        let span = last_t.duration_since(first_t);
        (span >= window.mul_f64(0.8))
            .then(|| last_b.saturating_sub(first_b) as f64 / span.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_millis(250);

    /// A simulated link: throughput as a function of the number of connections, a time to first
    /// byte for new connections, and a file size.
    struct World {
        rate: fn(u16) -> f64,
        ttfb: Duration,
        size: u64,
    }

    /// Run the controller against `world` for `secs`; returns the connection count over time.
    fn simulate(world: &World, min: u16, max: u16, secs: u64) -> Vec<u16> {
        simulate_with(world, min, max, secs, u16::MAX, None)
    }

    /// Like [`simulate`], but connections beyond `usable` find no work and end before their
    /// first byte, and with `noise` = (seed, amplitude) throughput varies by up to that fraction,
    /// in steps that last one second.
    fn simulate_with(
        world: &World,
        min: u16,
        max: u16,
        secs: u64,
        usable: u16,
        noise: Option<(u64, f64)>,
    ) -> Vec<u16> {
        let t0 = Instant::now();
        let mut c = Controller::new(min, max, t0);
        let mut active = 1u16;
        // Arrival times of connections that are still being set up.
        let mut pending: Vec<Instant> = Vec::new();
        let mut bytes = 0.0;
        let mut trace = Vec::new();
        let mut now = t0;
        let mut rng = noise.map_or(0, |(seed, _)| seed);
        let mut factor = 1.0;
        let mut step = 0u64;
        while now < t0 + Duration::from_secs(secs) {
            now += TICK;
            step += 1;
            if let Some((_, amp)) = noise
                && step % 4 == 1
            {
                // A small linear congruential generator: deterministic across runs.
                rng = rng
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let unit = (rng >> 11) as f64 / (1u64 << 53) as f64;
                factor = 1.0 + amp * (2.0 * unit - 1.0);
            }
            bytes += (world.rate)(active) * factor * TICK.as_secs_f64();
            let before = pending.len();
            pending.retain(|&at| at > now);
            for _ in pending.len()..before {
                if active < usable {
                    active += 1;
                    c.signal(now, Signal::Connected(world.ttfb));
                } else {
                    c.signal(now, Signal::Unused);
                }
            }
            let target = c.tick(Observation {
                now,
                bytes: bytes as u64,
                remaining: world.size.saturating_sub(bytes as u64),
                has_work: true,
                disk_busy: false,
            });
            let planned = active + pending.len() as u16;
            if target > planned {
                pending.extend((planned..target).map(|_| now + world.ttfb));
            } else if target < planned {
                pending.clear();
                active = active.min(target);
            }
            trace.push(active);
        }
        trace
    }

    /// Seconds until `trace` first reaches `level`.
    fn reached(trace: &[u16], level: u16) -> Option<f64> {
        let i = trace.iter().position(|&k| k >= level)?;
        Some((i + 1) as f64 * TICK.as_secs_f64())
    }

    const BIG: u64 = 1 << 40;

    #[test]
    fn per_connection_limits_ramp_up_to_the_maximum() {
        let world = World {
            rate: |k| f64::from(k) * 100_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate(&world, 1, 8, 60);
        assert_eq!(*trace.last().unwrap(), 8, "{trace:?}");
        // Doubling: 1, 2, 4, 8 in three probes rather than seven.
        assert!(trace.contains(&4), "{trace:?}");
        assert!(!trace.contains(&3), "doubles: {trace:?}");
        let at = reached(&trace, 8).unwrap();
        assert!(at <= 14.0, "reached 8 after {at}s: {trace:?}");
    }

    #[test]
    fn equal_connections_reach_a_high_maximum() {
        // A server that limits every connection to the same rate: each one adds as much as the
        // others, so growth must not stop near 1 / GAIN connections.
        let world = World {
            rate: |k| f64::from(k) * 50_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate(&world, 1, 15, 120);
        assert_eq!(*trace.last().unwrap(), 15, "{trace:?}");
        let at = reached(&trace, 15).unwrap();
        assert!(at <= 20.0, "reached 15 after {at}s: {trace:?}");
    }

    #[test]
    fn a_doubling_past_the_capacity_settles_at_the_fit() {
        // Six connections fill the link; the probe from 4 to 8 helps by only half as much as
        // four more equal connections would, so the fit lands on six, not eight.
        let world = World {
            rate: |k| (f64::from(k) * 100_000.0).min(600_000.0),
            ttfb: Duration::from_millis(50),
            size: BIG,
        };
        let trace = simulate(&world, 1, 15, 120);
        assert!(trace.iter().all(|&k| k <= 8), "{trace:?}");
        let settled = &trace[trace.len() / 2..];
        assert!(settled.iter().all(|&k| (6..=7).contains(&k)), "{trace:?}");
        assert!(
            settled.iter().filter(|&&k| k == 6).count() * 10 > settled.len() * 8,
            "mostly six: {trace:?}"
        );
    }

    #[test]
    fn diminishing_returns_stop_growth() {
        // Each connection adds less than the one before, as with loss-limited flows sharing a
        // link: 1 -> 100, 2 -> 167, 4 -> 250, 8 -> 333, 15 -> 375 (kB/s).
        let world = World {
            rate: |k| 500_000.0 * f64::from(k) / (f64::from(k) + 4.0),
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate(&world, 1, 15, 180);
        let last = *trace.last().unwrap();
        assert!((3..=9).contains(&last), "{trace:?}");
    }

    #[test]
    fn a_shared_limit_settles_at_one_or_two() {
        let world = World {
            rate: |_| 500_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate(&world, 1, 8, 120);
        assert!(trace.iter().all(|&k| k <= 2), "{trace:?}");
        let ones = trace.iter().filter(|&&k| k == 1).count();
        assert!(
            ones * 10 > trace.len() * 8,
            "mostly one connection: {trace:?}"
        );
    }

    #[test]
    fn stops_where_the_link_saturates() {
        let world = World {
            rate: |k| (f64::from(k) * 100_000.0).min(300_000.0),
            ttfb: Duration::from_millis(50),
            size: BIG,
        };
        let trace = simulate(&world, 1, 8, 120);
        assert!(trace.iter().all(|&k| k <= 4), "{trace:?}");
        assert!(
            trace[trace.len() / 2..].iter().all(|&k| k >= 3),
            "{trace:?}"
        );
    }

    #[test]
    fn high_latency_slows_probing_but_still_converges() {
        let world = World {
            rate: |k| f64::from(k) * 50_000.0,
            ttfb: Duration::from_millis(1500),
            size: BIG,
        };
        let trace = simulate(&world, 1, 4, 180);
        assert_eq!(*trace.last().unwrap(), 4, "{trace:?}");
        let first_growth = trace.iter().position(|&k| k > 1).unwrap();
        assert!(first_growth as u32 * TICK.as_millis() as u32 >= 1000);
    }

    #[test]
    fn small_remaining_work_never_grows() {
        let world = World {
            rate: |k| f64::from(k) * 1_000_000.0,
            ttfb: Duration::from_millis(10),
            size: 1_500_000,
        };
        assert!(simulate(&world, 1, 8, 10).iter().all(|&k| k == 1));
    }

    #[test]
    fn probe_connections_without_work_do_not_stall_the_probe() {
        // Only three connections find work: the doubling to four is judged on three, promptly,
        // and does not count as a failure.
        let world = World {
            rate: |k| f64::from(k) * 100_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate_with(&world, 1, 8, 30, 3, None);
        let at = reached(&trace, 3).unwrap();
        assert!(at <= 10.0, "reached 3 after {at}s: {trace:?}");
        assert!(
            trace[trace.len() / 2..].iter().all(|&k| k == 3),
            "{trace:?}"
        );
    }

    /// The average connection count over the second half of `trace`.
    fn late_mean(trace: &[u16]) -> f64 {
        let late = &trace[trace.len() / 2..];
        late.iter().map(|&k| f64::from(k)).sum::<f64>() / late.len() as f64
    }

    #[test]
    fn noise_does_not_make_useless_connections_creep_up() {
        type Rate = fn(u16) -> f64;
        let worlds: [(Rate, f64); 3] = [
            (|_| 500_000.0, 1.0),
            (|k| (f64::from(k) * 100_000.0).min(300_000.0), 3.0),
            (|k| (f64::from(k) * 100_000.0).min(600_000.0), 6.0),
        ];
        for (rate, needed) in worlds {
            let world = World {
                rate,
                ttfb: Duration::from_millis(100),
                size: BIG,
            };
            // Noise can let a useless connection through now and then, but not as a rule.
            let mut means: Vec<f64> = (1..=20)
                .map(|seed| {
                    late_mean(&simulate_with(
                        &world,
                        1,
                        15,
                        300,
                        u16::MAX,
                        Some((seed, 0.15)),
                    ))
                })
                .collect();
            means.sort_by(f64::total_cmp);
            assert!(means[10] <= needed + 0.5, "needs {needed}: {means:.2?}");
            assert!(means[19] <= needed + 2.5, "needs {needed}: {means:.2?}");
        }
    }

    #[test]
    fn noise_does_not_stop_equal_connections_early() {
        let world = World {
            rate: |k| f64::from(k) * 50_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        for seed in 1..=20 {
            let trace = simulate_with(&world, 1, 15, 300, u16::MAX, Some((seed, 0.15)));
            let mean = late_mean(&trace);
            assert!(mean >= 11.0, "averaged {mean:.2} (seed {seed}): {trace:?}");
        }
    }

    fn steady(max: u16) -> (Controller, Instant, u64) {
        let t0 = Instant::now();
        let mut c = Controller::new(1, max, t0);
        let mut now = t0;
        let mut bytes = 0;
        while !c.probing() {
            now += TICK;
            bytes += 100_000;
            c.tick(Observation {
                now,
                bytes,
                remaining: BIG,
                has_work: true,
                disk_busy: false,
            });
            assert!(now < t0 + Duration::from_secs(10));
        }
        (c, now, bytes)
    }

    #[test]
    fn throttling_halves_and_holds() {
        let (mut c, now, _) = steady(8);
        c.target = 6;
        c.signal(now, Signal::Throttled);
        assert_eq!(c.target(), 3);
        c.signal(now, Signal::Failed);
        assert_eq!(c.target(), 2);
        assert!(c.hold.unwrap() > now + COOLDOWN, "cooldown doubles");
        c.signal(now, Signal::Unsupported);
        assert_eq!((c.target(), c.max), (1, 1));
    }

    #[test]
    fn a_probe_that_never_connects_is_abandoned() {
        let (mut c, mut now, bytes) = steady(8);
        assert_eq!(c.target(), 2);
        for _ in 0..60 {
            now += TICK;
            c.tick(Observation {
                now,
                bytes,
                remaining: BIG,
                has_work: true,
                disk_busy: false,
            });
        }
        assert_eq!(c.target(), 1);
    }

    #[test]
    fn a_busy_disk_or_no_work_prevents_growth() {
        for (has_work, disk_busy) in [(false, false), (true, true)] {
            let t0 = Instant::now();
            let mut c = Controller::new(1, 8, t0);
            for i in 1..100u64 {
                c.tick(Observation {
                    now: t0 + TICK * i as u32,
                    bytes: i * 100_000,
                    remaining: BIG,
                    has_work,
                    disk_busy,
                });
            }
            assert_eq!(c.target(), 1);
        }
    }

    #[test]
    fn the_minimum_holds_when_more_connections_do_not_help() {
        let world = World {
            rate: |_| 500_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate(&world, 3, 8, 120);
        let reached = trace.iter().position(|&k| k == 3).unwrap();
        assert!(reached <= 8, "starts at the minimum: {trace:?}");
        assert!(trace[reached..].iter().all(|&k| k >= 3), "{trace:?}");
        // The first probe doubles the minimum; later ones add a single connection.
        assert!(trace.iter().all(|&k| k <= 6), "{trace:?}");
        let late = &trace[trace.len() / 2..];
        assert!(late.iter().all(|&k| k <= 4), "{trace:?}");
    }

    #[test]
    fn the_minimum_still_grows_when_connections_help() {
        let world = World {
            rate: |k| f64::from(k) * 100_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate(&world, 3, 8, 60);
        assert_eq!(*trace.last().unwrap(), 8, "{trace:?}");
    }

    #[test]
    fn the_minimum_is_clamped_to_the_maximum() {
        let c = Controller::new(10, 4, Instant::now());
        assert_eq!((c.min, c.target()), (4, 4));
        let c = Controller::new(0, 4, Instant::now());
        assert_eq!((c.min, c.target()), (1, 1));
    }

    #[test]
    fn throttling_goes_below_the_minimum_until_the_cooldown_passes() {
        let t0 = Instant::now();
        let mut c = Controller::new(4, 8, t0);
        let tick = |c: &mut Controller, now: Instant| {
            c.tick(Observation {
                now,
                bytes: 0,
                remaining: BIG,
                has_work: true,
                disk_busy: false,
            })
        };
        c.signal(t0, Signal::Throttled);
        assert_eq!(c.target(), 2);
        c.signal(t0, Signal::Throttled);
        assert_eq!(c.target(), 1);
        let restore = c.restore.unwrap();
        let mut now = t0;
        while now + TICK < restore {
            now += TICK;
            assert!(tick(&mut c, now) < 4, "held below the minimum");
        }
        now = restore + TICK;
        tick(&mut c, now);
        assert_eq!(tick(&mut c, now + TICK), 4, "restored without a probe");
        assert!(!c.probing());
    }

    #[test]
    fn a_long_growth_cooldown_does_not_delay_the_minimum() {
        let t0 = Instant::now();
        let mut c = Controller::new(3, 8, t0);
        let mut now = t0;
        let mut bytes = 0;
        // A shared limit: every probe above the minimum fails and doubles the cooldown.
        while c.cooldown < MAX_COOLDOWN {
            now += TICK;
            bytes += 125_000;
            if c.probing() {
                c.signal(now, Signal::Connected(Duration::from_millis(50)));
            }
            c.tick(Observation {
                now,
                bytes,
                remaining: BIG,
                has_work: true,
                disk_busy: false,
            });
            assert!(now < t0 + Duration::from_secs(600), "probes keep failing");
        }
        assert_eq!(c.target(), 3);
        c.signal(now, Signal::Failed);
        assert_eq!(c.target(), 2);
        let failed = now;
        while c.target() < 3 {
            now += TICK;
            bytes += 125_000;
            c.tick(Observation {
                now,
                bytes,
                remaining: BIG,
                has_work: true,
                disk_busy: false,
            });
        }
        assert!(now - failed <= COOLDOWN + TICK * 2, "{:?}", now - failed);
        assert!(c.hold.unwrap() > now, "growth stays on hold");
    }

    #[test]
    fn unsupported_overrides_the_minimum() {
        let t0 = Instant::now();
        let mut c = Controller::new(4, 8, t0);
        c.signal(t0, Signal::Unsupported);
        assert_eq!((c.target(), c.min, c.max), (1, 1, 1));
        for i in 1..100u32 {
            c.tick(Observation {
                now: t0 + TICK * i,
                bytes: u64::from(i) * 100_000,
                remaining: BIG,
                has_work: true,
                disk_busy: false,
            });
        }
        assert_eq!(c.target(), 1);
    }
}
