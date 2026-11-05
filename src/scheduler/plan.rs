//! Ownership of byte ranges during a segmented transfer.
//!
//! Every byte of the file is in exactly one place: received, owned by one worker's segment
//! (between its cursor and its end), or missing. A worker reserves bytes under the plan's lock
//! before it hands them to the writer, so a split or trim never gives away bytes already in
//! flight. Segment ids are never reused: a worker whose segment was released can no longer
//! reserve anything, so late data from an abandoned request cannot land in reassigned ranges.
//!
//! Split points are block boundaries. Each checkpoint block therefore has a single writer, which
//! fills it in order from its first byte, as the state format requires.

use std::time::{Duration, Instant};

pub type SegmentId = u64;

/// A worker's exclusive claim on `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub id: SegmentId,
    pub start: u64,
    pub end: u64,
}

/// A proposed split: `[start, end)` from the back of another worker's segment. It takes effect
/// only through [`Plan::commit`], after the response for it has been validated, so a server that
/// ignores ranges never costs the existing stream its work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Steal {
    victim: SegmentId,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug)]
struct Segment {
    id: SegmentId,
    /// Connection slot of the worker that owns it.
    owner: usize,
    cursor: u64,
    end: u64,
    /// The response delivers bytes up to here, so the segment may grow back to it.
    limit: u64,
    /// Being wound down; it does not take over neighbouring work.
    trimmed: bool,
    since: Instant,
    base: u64,
}

impl Segment {
    fn remaining(&self) -> u64 {
        self.end - self.cursor
    }

    /// Bytes per second since the segment started, once that is meaningful.
    fn rate(&self, now: Instant) -> Option<f64> {
        let elapsed = now.duration_since(self.since);
        (elapsed >= Duration::from_secs(1))
            .then(|| (self.cursor - self.base) as f64 / elapsed.as_secs_f64())
    }
}

#[derive(Debug)]
pub struct Plan {
    total: u64,
    block: u64,
    /// Unowned and not received: sorted, disjoint, non-empty ranges.
    missing: Vec<[u64; 2]>,
    segments: Vec<Segment>,
    next_id: SegmentId,
    /// Bytes received or saved earlier.
    done: u64,
}

impl Plan {
    /// A plan for a file of `total` bytes of which the sorted, disjoint `saved` ranges exist.
    pub fn new(total: u64, block: u64, saved: &[[u64; 2]]) -> Plan {
        let mut missing = Vec::new();
        let mut at = 0;
        let mut done = 0;
        for &[start, end] in saved {
            if start > at {
                missing.push([at, start]);
            }
            done += end - start;
            at = end;
        }
        if at < total {
            missing.push([at, total]);
        }
        Plan {
            total,
            block,
            missing,
            segments: Vec::new(),
            next_id: 1,
            done,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn done(&self) -> u64 {
        self.done
    }

    pub fn remaining(&self) -> u64 {
        self.total - self.done
    }

    pub fn complete(&self) -> bool {
        self.done == self.total
    }

    /// Take the lowest missing range.
    pub fn claim(&mut self, owner: usize, now: Instant) -> Option<Claim> {
        if self.missing.is_empty() {
            return None;
        }
        let [start, end] = self.missing.remove(0);
        Some(self.add(owner, start, end, end, now))
    }

    /// The back half of the segment with the most work left, if splitting it is worthwhile:
    /// both halves keep at least a block, and the owner would not finish within `min_secs` at
    /// its current rate anyway.
    pub fn propose(&self, now: Instant, min_secs: f64) -> Option<Steal> {
        self.segments
            .iter()
            .filter(|s| !s.trimmed && s.remaining() >= 2 * self.block)
            .filter(|s| {
                s.rate(now)
                    .is_none_or(|rate| rate <= 0.0 || s.remaining() as f64 / rate >= min_secs)
            })
            .max_by_key(|s| s.remaining())
            .and_then(|s| {
                let split = align_up(s.cursor + s.remaining() / 2, self.block);
                (split > s.cursor && split < s.end).then_some(Steal {
                    victim: s.id,
                    start: split,
                    end: s.end,
                })
            })
    }

    /// Carry out a proposed split. Fails when the victim is gone or has already passed the split
    /// point; the caller then drops its response and looks for other work.
    pub fn commit(&mut self, steal: Steal, owner: usize, now: Instant) -> Option<Claim> {
        let victim = self
            .segments
            .iter_mut()
            .find(|s| s.id == steal.victim && !s.trimmed)?;
        if victim.cursor > steal.start || steal.start >= victim.end {
            return None;
        }
        let old_end = victim.end;
        victim.end = steal.start;
        let end = old_end.min(steal.end);
        if old_end > end {
            // The victim grew after the proposal; the response does not cover the growth.
            self.insert_missing(end, old_end);
        }
        Some(self.add(owner, steal.start, end, steal.end, now))
    }

    /// Reserve up to `len` bytes at `offset` for segment `id`. Returns how many may be written:
    /// fewer than `len` at the segment's end, zero once it is complete, trimmed, or released.
    pub fn reserve(&mut self, id: SegmentId, offset: u64, len: u64) -> u64 {
        let Some(segment) = self.segments.iter_mut().find(|s| s.id == id) else {
            return 0;
        };
        if offset != segment.cursor {
            return 0;
        }
        let take = len.min(segment.remaining());
        segment.cursor += take;
        self.done += take;
        take
    }

    /// Whether segment `id` needs no more bytes.
    pub fn finished(&self, id: SegmentId) -> bool {
        self.segments
            .iter()
            .find(|s| s.id == id)
            .is_none_or(|s| s.remaining() == 0)
    }

    /// End a segment. Bytes it did not receive go to the segment just before them if that
    /// segment's response still covers them, or back to missing.
    pub fn release(&mut self, id: SegmentId) {
        let Some(i) = self.segments.iter().position(|s| s.id == id) else {
            return;
        };
        let segment = self.segments.remove(i);
        if segment.remaining() == 0 {
            return;
        }
        let (start, end) = (segment.cursor, segment.end);
        if let Some(before) = self
            .segments
            .iter_mut()
            .find(|s| s.end == start && s.limit >= end && !s.trimmed)
        {
            before.end = end;
            return;
        }
        self.insert_missing(start, end);
    }

    /// Wind down `owner`'s segment: it stops at the next block boundary and the rest returns to
    /// missing. Returns whether `owner` had a segment.
    pub fn trim(&mut self, owner: usize) -> bool {
        let block = self.block;
        let Some(segment) = self.segments.iter_mut().find(|s| s.owner == owner) else {
            return false;
        };
        segment.trimmed = true;
        let cut = align_up(segment.cursor, block).min(segment.end);
        let end = segment.end;
        segment.end = cut;
        if cut < end {
            self.insert_missing(cut, end);
        }
        true
    }

    /// Throughput of each segment that has been receiving for a while, in bytes per second.
    pub fn rates(&self, now: Instant) -> Vec<f64> {
        self.segments.iter().filter_map(|s| s.rate(now)).collect()
    }

    /// Whether another worker could find something to do.
    pub fn has_work(&self, now: Instant, min_secs: f64) -> bool {
        !self.missing.is_empty() || self.propose(now, min_secs).is_some()
    }

    fn add(&mut self, owner: usize, start: u64, end: u64, limit: u64, now: Instant) -> Claim {
        let id = self.next_id;
        self.next_id += 1;
        self.segments.push(Segment {
            id,
            owner,
            cursor: start,
            end,
            limit,
            trimmed: false,
            since: now,
            base: start,
        });
        Claim { id, start, end }
    }

    fn insert_missing(&mut self, start: u64, end: u64) {
        let at = self.missing.partition_point(|&[s, _]| s < start);
        self.missing.insert(at, [start, end]);
        // Merge with the neighbours.
        if at + 1 < self.missing.len() && self.missing[at][1] == self.missing[at + 1][0] {
            self.missing[at][1] = self.missing.remove(at + 1)[1];
        }
        if at > 0 && self.missing[at - 1][1] == self.missing[at][0] {
            self.missing[at - 1][1] = self.missing.remove(at)[1];
        }
    }

    /// Every byte is accounted for exactly once.
    #[cfg(test)]
    fn check(&self) {
        let mut pieces: Vec<[u64; 2]> = self.missing.clone();
        let mut owned = 0;
        for s in &self.segments {
            assert!(s.cursor <= s.end && s.end <= s.limit, "{s:?}");
            if s.remaining() > 0 {
                pieces.push([s.cursor, s.end]);
            }
            owned += s.remaining();
        }
        pieces.sort();
        for pair in pieces.windows(2) {
            assert!(pair[0][1] <= pair[1][0], "overlap: {pieces:?}");
        }
        let missing: u64 = self.missing.iter().map(|[s, e]| e - s).sum();
        assert_eq!(self.done + owned + missing, self.total, "{self:?}");
    }
}

pub fn align_up(offset: u64, block: u64) -> u64 {
    offset.div_ceil(block) * block
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: u64 = 1 << 20;

    #[test]
    fn starts_from_the_gaps_between_saved_ranges() {
        let plan = Plan::new(10 * B, B, &[[0, 2 * B], [5 * B, 6 * B + 7]]);
        assert_eq!(plan.missing, vec![[2 * B, 5 * B], [6 * B + 7, 10 * B]]);
        assert_eq!(plan.done(), 3 * B + 7);
        plan.check();
        assert!(Plan::new(B, B, &[[0, B]]).complete());
    }

    #[test]
    fn reserving_clamps_to_the_segment() {
        let now = Instant::now();
        let mut plan = Plan::new(3 * B, B, &[]);
        let c = plan.claim(0, now).unwrap();
        assert_eq!((c.start, c.end), (0, 3 * B));
        assert!(plan.claim(1, now).is_none(), "nothing left to claim");
        assert_eq!(plan.reserve(c.id, 0, 100), 100);
        assert_eq!(plan.reserve(c.id, 50, 10), 0, "only at the cursor");
        assert_eq!(plan.reserve(c.id, 100, 4 * B), 3 * B - 100);
        assert!(plan.finished(c.id) && plan.complete());
        plan.release(c.id);
        plan.check();
    }

    #[test]
    fn released_work_returns_to_missing() {
        let now = Instant::now();
        let mut plan = Plan::new(4 * B, B, &[]);
        let c = plan.claim(0, now).unwrap();
        plan.reserve(c.id, 0, B + 5);
        plan.release(c.id);
        assert_eq!(plan.missing, vec![[B + 5, 4 * B]]);
        assert_eq!(
            plan.reserve(c.id, B + 5, 1),
            0,
            "a released segment is dead"
        );
        let again = plan.claim(1, now).unwrap();
        assert_eq!(again.start, B + 5, "continues the partly written block");
        assert_ne!(again.id, c.id);
        plan.check();
    }

    #[test]
    fn steals_the_back_half_at_a_block_boundary() {
        let now = Instant::now();
        let mut plan = Plan::new(10 * B + 3, B, &[]);
        let lead = plan.claim(0, now).unwrap();
        plan.reserve(lead.id, 0, B / 2);
        let steal = plan.propose(now, 0.0).unwrap();
        assert_eq!(steal.start % B, 0);
        assert_eq!(steal.end, 10 * B + 3);
        assert!(steal.start > B / 2 && steal.start < steal.end);
        let thief = plan.commit(steal, 1, now).unwrap();
        assert_eq!((thief.start, thief.end), (steal.start, steal.end));
        assert_eq!(
            plan.reserve(lead.id, B / 2, 20 * B),
            steal.start - B / 2,
            "the victim stops at the split"
        );
        assert!(plan.finished(lead.id));
        plan.check();
    }

    #[test]
    fn a_split_the_victim_already_passed_is_refused() {
        let now = Instant::now();
        let mut plan = Plan::new(4 * B, B, &[]);
        let lead = plan.claim(0, now).unwrap();
        let steal = plan.propose(now, 0.0).unwrap();
        assert_eq!(steal.start, 2 * B);
        plan.reserve(lead.id, 0, 2 * B + 1);
        assert!(plan.commit(steal, 1, now).is_none());
        plan.check();
    }

    #[test]
    fn small_or_nearly_finished_segments_are_not_split() {
        let now = Instant::now();
        let mut plan = Plan::new(2 * B - 1, B, &[]);
        plan.claim(0, now).unwrap();
        assert!(plan.propose(now, 0.0).is_none(), "less than two blocks");

        let mut plan = Plan::new(8 * B, B, &[]);
        let lead = plan.claim(0, now).unwrap();
        let later = now + Duration::from_secs(2);
        plan.reserve(lead.id, 0, 4 * B); // 2 MiB/s, 2 s of work left
        assert!(plan.propose(later, 5.0).is_none());
        assert!(plan.propose(later, 1.0).is_some());
    }

    #[test]
    fn an_empty_thief_gives_its_range_back_to_the_victim() {
        let now = Instant::now();
        let mut plan = Plan::new(8 * B, B, &[]);
        let lead = plan.claim(0, now).unwrap();
        let thief = plan
            .commit(plan.propose(now, 0.0).unwrap(), 1, now)
            .unwrap();
        plan.release(thief.id);
        assert!(
            plan.missing.is_empty(),
            "the open-ended stream covers it again"
        );
        assert_eq!(plan.reserve(lead.id, 0, 8 * B), 8 * B);
        plan.check();
    }

    #[test]
    fn trimming_stops_at_the_next_block() {
        let now = Instant::now();
        let mut plan = Plan::new(8 * B, B, &[]);
        let c = plan.claim(3, now).unwrap();
        plan.reserve(c.id, 0, B + 10);
        assert!(plan.trim(3));
        assert_eq!(plan.missing, vec![[2 * B, 8 * B]]);
        assert_eq!(plan.reserve(c.id, B + 10, 8 * B), B - 10);
        assert!(plan.finished(c.id));
        assert!(!plan.trim(4));
        plan.check();
    }

    #[test]
    fn random_operations_keep_every_byte_accounted_for() {
        let now = Instant::now();
        for seed in 0..200 {
            let mut rng = fastrand::Rng::with_seed(seed);
            let total = rng.u64(1..40 * B);
            let mut plan = Plan::new(total, B, &[]);
            let mut live: Vec<(usize, Claim, u64)> = Vec::new();
            for step in 0..300 {
                match rng.u8(0..5) {
                    0 => {
                        if let Some(c) = plan.claim(step, now) {
                            live.push((step, c, c.start));
                        }
                    }
                    1 => {
                        if let Some(s) = plan.propose(now, 0.0)
                            && let Some(c) = plan.commit(s, step, now)
                        {
                            live.push((step, c, c.start));
                        }
                    }
                    2 if !live.is_empty() => {
                        let i = rng.usize(..live.len());
                        let (_, c, at) = &mut live[i];
                        *at += plan.reserve(c.id, *at, rng.u64(1..3 * B));
                    }
                    3 if !live.is_empty() => {
                        let (_, c, _) = live.swap_remove(rng.usize(..live.len()));
                        plan.release(c.id);
                    }
                    4 if !live.is_empty() => {
                        plan.trim(live[rng.usize(..live.len())].0);
                    }
                    _ => {}
                }
                plan.check();
            }
            // Drain: every byte can still be fetched exactly once.
            for (_, c, _) in live.drain(..) {
                plan.release(c.id);
            }
            while let Some(c) = plan.claim(0, now) {
                assert_eq!(
                    plan.reserve(c.id, c.start, c.end - c.start),
                    c.end - c.start
                );
                plan.release(c.id);
            }
            assert!(plan.complete(), "seed {seed}");
        }
    }
}
