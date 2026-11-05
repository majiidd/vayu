//! Adaptive concurrency: how many connections a segmented transfer should use.
//!
//! Hill climbing on measured useful throughput (bytes accepted for the file, so retransmitted or
//! discarded bytes never count). The transfer starts with one connection. After a warm-up, and
//! only while enough work remains to repay a connection's setup, the controller adds one
//! connection and waits for it to ramp up: the wait scales with the observed time to first byte,
//! so high-latency links get longer windows. The connection stays only when aggregate throughput
//! improved by [`GAIN`]; otherwise it is retired and the next probe waits for a cooldown that
//! doubles after every fruitless probe.
//!
//! Throttling responses (429, 503) halve the connection count, other failures on extra
//! connections remove one, and a server that does not serve ranges to extra connections ends
//! segmentation. A disk that cannot keep up stops growth, since more connections cannot help. A
//! level whose throughput falls well below what one connection fewer achieved is abandoned.
//!
//! The controller is a pure state machine over time and byte counts; the supervisor feeds it and
//! applies its target.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Minimum relative throughput improvement that justifies keeping a connection.
pub const GAIN: f64 = 0.10;
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
/// Window and factor for detecting that the current level does worse than one fewer did.
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
    /// The probing connection received its first byte after this time to first byte.
    Connected(Duration),
    /// A time to first byte measured on another request.
    Latency(Duration),
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
    /// One connection was added; it is judged once it has ramped up.
    Probing {
        baseline: f64,
        launched: Instant,
        connected: Option<Instant>,
    },
}

pub struct Controller {
    max: u16,
    target: u16,
    state: State,
    /// No growth before this time.
    hold: Option<Instant>,
    cooldown: Duration,
    /// Smoothed time to first byte.
    setup: Option<Duration>,
    samples: VecDeque<(Instant, u64)>,
    /// Throughput measured at each level (index = connections).
    levels: Vec<Option<f64>>,
}

impl Controller {
    pub fn new(max: u16, now: Instant) -> Controller {
        Controller {
            max: max.max(1),
            target: 1,
            state: State::Settling {
                until: now + WARMUP,
            },
            hold: None,
            cooldown: COOLDOWN,
            setup: None,
            samples: VecDeque::new(),
            levels: vec![None; usize::from(max.max(1)) + 1],
        }
    }

    #[cfg(test)]
    pub fn target(&self) -> u16 {
        self.target
    }

    /// Whether a probe connection is being evaluated.
    pub fn probing(&self) -> bool {
        matches!(self.state, State::Probing { .. })
    }

    pub fn signal(&mut self, now: Instant, signal: Signal) {
        match signal {
            Signal::Connected(ttfb) => {
                self.note_latency(ttfb);
                if let State::Probing {
                    connected: connected @ None,
                    ..
                } = &mut self.state
                {
                    *connected = Some(now);
                }
            }
            Signal::Latency(ttfb) => self.note_latency(ttfb),
            Signal::Throttled => {
                let cut = (self.target / 2).max(1);
                self.back_off(now, cut);
            }
            Signal::Failed => self.back_off(now, 1),
            Signal::Unsupported => {
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
                if self.worse_than_one_fewer(now, since) {
                    self.back_off(now, 1);
                } else if self.may_grow(&obs, since)
                    && let Some(baseline) = self.rate(now, self.measure())
                {
                    self.levels[usize::from(self.target)] = Some(baseline);
                    self.target += 1;
                    self.state = State::Probing {
                        baseline,
                        launched: now,
                        connected: None,
                    };
                }
            }
            State::Probing {
                baseline,
                launched,
                connected,
            } => match connected {
                None if now.duration_since(launched) > self.probe_timeout() => {
                    self.back_off(now, 1);
                }
                None => {}
                Some(at) if now >= at + self.ramp() + self.measure() => {
                    let rate = self.rate(now, self.measure()).unwrap_or(0.0);
                    if rate >= baseline * (1.0 + GAIN) {
                        self.levels[usize::from(self.target)] = Some(rate);
                        self.cooldown = COOLDOWN;
                        // The window just measured is this level's baseline for the next probe.
                        self.state = State::Steady {
                            since: now - self.measure(),
                        };
                    } else {
                        self.back_off(now, 1);
                    }
                }
                Some(_) => {}
            },
        }
        self.target
    }

    fn may_grow(&self, obs: &Observation, since: Instant) -> bool {
        let now = obs.now;
        if self.target >= self.max
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

    /// Throughput at this level has fallen well below what one connection fewer achieved.
    fn worse_than_one_fewer(&self, now: Instant, since: Instant) -> bool {
        if self.target < 2 || now.duration_since(since) < WORSE_WINDOW {
            return false;
        }
        let Some(fewer) = self.levels[usize::from(self.target - 1)] else {
            return false;
        };
        self.rate(now, WORSE_WINDOW)
            .is_some_and(|rate| rate < fewer * WORSE_FACTOR)
    }

    /// Remove `by` connections and wait before trying more.
    fn back_off(&mut self, now: Instant, by: u16) {
        self.target = self.target.saturating_sub(by).max(1);
        self.hold = Some(now + self.cooldown);
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
    fn simulate(world: &World, max: u16, secs: u64) -> Vec<u16> {
        let t0 = Instant::now();
        let mut c = Controller::new(max, t0);
        let mut active = 1u16;
        let mut pending: Option<Instant> = None;
        let mut bytes = 0.0;
        let mut trace = Vec::new();
        let mut now = t0;
        while now < t0 + Duration::from_secs(secs) {
            now += TICK;
            bytes += (world.rate)(active) * TICK.as_secs_f64();
            if let Some(at) = pending
                && now >= at
            {
                active += 1;
                pending = None;
                c.signal(now, Signal::Connected(world.ttfb));
            }
            let target = c.tick(Observation {
                now,
                bytes: bytes as u64,
                remaining: world.size.saturating_sub(bytes as u64),
                has_work: true,
                disk_busy: false,
            });
            if target > active + u16::from(pending.is_some()) {
                pending = Some(now + world.ttfb);
            } else if target < active {
                active = target;
                pending = None;
            }
            trace.push(active);
        }
        trace
    }

    const BIG: u64 = 1 << 40;

    #[test]
    fn per_connection_limits_ramp_up_to_the_maximum() {
        let world = World {
            rate: |k| f64::from(k) * 100_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate(&world, 8, 60);
        assert_eq!(*trace.last().unwrap(), 8, "{trace:?}");
        assert!(trace.windows(2).all(|w| w[1] <= w[0] + 1), "one at a time");
    }

    #[test]
    fn a_shared_limit_settles_at_one_or_two() {
        let world = World {
            rate: |_| 500_000.0,
            ttfb: Duration::from_millis(100),
            size: BIG,
        };
        let trace = simulate(&world, 8, 120);
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
        let trace = simulate(&world, 8, 120);
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
        let trace = simulate(&world, 4, 180);
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
        assert!(simulate(&world, 8, 10).iter().all(|&k| k == 1));
    }

    fn steady(max: u16) -> (Controller, Instant, u64) {
        let t0 = Instant::now();
        let mut c = Controller::new(max, t0);
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
            let mut c = Controller::new(8, t0);
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
}
