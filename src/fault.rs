//! Crash injection for recovery tests, compiled into debug builds only.
//!
//! `VAYU_FAULT=NAME` aborts the process the first time execution reaches the point `NAME`;
//! `VAYU_FAULT=NAME@N` aborts on the Nth time. `abort` skips destructors and buffered-writer
//! flushes, so the on-disk state is what a crash at that instant would leave behind.

#[cfg(debug_assertions)]
pub fn point(name: &str) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static HITS: AtomicU32 = AtomicU32::new(0);

    let Some(spec) = std::env::var_os("VAYU_FAULT") else {
        return;
    };
    let spec = spec.to_string_lossy();
    let (target, nth) = match spec.split_once('@') {
        Some((target, n)) => (target, n.parse().unwrap_or(1)),
        None => (&*spec, 1),
    };
    if target == name && HITS.fetch_add(1, Ordering::SeqCst) + 1 == nth {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn point(_name: &str) {}
