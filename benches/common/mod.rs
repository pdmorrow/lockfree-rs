//! Scaffolding shared by the benchmark targets.
//!
//! Both targets need the same thread sweep and the same notion of
//! "non-critical work", and both would explain those choices in the
//! same comment. Sharing the code keeps the two from drifting into
//! measuring subtly different things -- the same reason the `Counter`
//! trait exists inside each target.

// Each bench target uses a subset of this module, and an unused item
// here is not a defect: `fairness` has no need to calibrate a pause,
// `spinlock` has no need for the fixed-window helpers. Without this,
// `clippy --all-targets -D warnings` fails on the target that happens
// to use less.
#![allow(dead_code)]

use std::hint::{black_box, spin_loop};
use std::time::Instant;

/// Thread counts to sweep: 2, 4, 8, ... up to the machine's
/// parallelism.
///
/// Starting at two, because one thread contending with nobody is not
/// a contended measurement: with no work outside the critical section
/// it measures an uncontended round trip wrapped in a thread spawn
/// and a barrier that contribute overhead and no information, and its
/// fairness is 1.00x by construction rather than by merit. The
/// uncontended case is not one this crate measures at all -- see the
/// header of `benches/spinlock.rs` for why.
///
/// Stopping at `available_parallelism` is deliberate. Oversubscribing
/// a spinlock is not a slow case but a pathological one -- a waiter
/// holds its core doing nothing while the thread it is waiting for
/// is descheduled -- so including it would drown the interesting
/// range in one enormous bar.
pub fn thread_counts() -> Vec<usize> {
    let max = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut counts: Vec<usize> = std::iter::successors(Some(2usize), |n| Some(n * 2))
        .take_while(|&n| n <= max)
        .collect();

    if counts.last() != Some(&max) {
        counts.push(max);
    }

    counts
}

/// Simulated work performed *outside* the critical section.
///
/// This is the parameter the contended benchmarks were missing. A
/// thread that does nothing between releasing the lock and asking for
/// it again is always either holding or waiting, which makes the
/// workload 100% serial: no lock can scale there, and the measurement
/// collapses onto the cost of a single handoff. Real callers do
/// something with the data they just read, and it is the ratio of
/// that something to the critical section that decides whether
/// contention exists at all.
///
/// `pause` rather than a computation because it is what the waiters
/// are already executing, so the units are commensurable, and because
/// it cannot be strength-reduced or vectorised into something
/// unrepresentative.
///
/// The loop body needs no `black_box`: `spin_loop` lowers to a real
/// instruction with side effects, so it cannot be deleted. The trip
/// count does, otherwise a caller passing a literal (as the sweep
/// does) hands the optimiser a fully known loop to unroll or fold.
#[inline]
pub fn spin_work(pauses: u32) {
    for _ in 0..black_box(pauses) {
        spin_loop();
    }
}

/// Measured cost of one `spin_loop()`, in nanoseconds.
///
/// The pause instruction's latency is not a constant: it is roughly
/// 65 cycles on AMD Zen, but ~140 on Intel since Skylake and a
/// handful before it. So `spin_work(64)` is not a portable duration,
/// and a sweep labelled in pause counts means nothing on its own.
/// Reporting the calibration alongside the sweep lets a reader
/// convert the axis into time on whatever machine produced the run.
///
/// Benchmark ids stay labelled by pause count regardless, because
/// they have to be stable for `--baseline` to match runs up.
pub fn pause_nanos() -> f64 {
    const N: u64 = 20_000_000;

    // A warm-up pass, so the measured pass is not paying for the
    // frequency ramp on a core that was idle a moment ago.
    for _ in 0..black_box(N / 10) {
        spin_loop();
    }

    let start = Instant::now();
    for _ in 0..black_box(N) {
        spin_loop();
    }

    start.elapsed().as_secs_f64() * 1e9 / N as f64
}
