//! Benchmarks for [`Spinlock`] and [`McsSpinlock`], measured side by
//! side with [`std::sync::Mutex`] so the numbers have a reference
//! point.
//!
//! The two spinlocks differ only in how they make waiters wait, so
//! every case here runs both: the pair is the comparison, and the
//! Mutex column is the scale it is read against.
//!
//! Three things are worth measuring about a lock, and they pull in
//! different directions:
//!
//!   * `try_lock`, both when it succeeds and when it fails, since the
//!     failing path is what a caller polls in a loop;
//!   * throughput under real contention, where a spinlock burns CPU
//!     to avoid a context switch and a Mutex does the opposite;
//!   * and how much non-critical work it takes for that contention to
//!     stop mattering, which is the axis the first two both hold
//!     fixed at zero.
//!
//! What is not measured is the uncontended round trip. A lock that is
//! not excluding anybody is a lock this crate has no use for: the
//! whole premise of spinning is that a contending thread is coming,
//! and soon enough that waiting for it beats sleeping.
//!
//! What is deliberately NOT here is fairness, which the throughput
//! numbers below cannot express and are actively misleading about --
//! see `benches/fairness.rs`, and read the two together.
//!
//! Run with `cargo bench`. See README.md for how to read the output.

use std::hint::black_box;
use std::sync::{Barrier, Mutex};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use spinlock_rs::mcs_spinlock::McsSpinlock;
use spinlock_rs::spinlock::Spinlock;

mod common;
use common::{pause_nanos, spin_work, thread_counts};

/// The one operation every lock is asked to perform: take the lock,
/// mutate the payload, release it.
///
/// A trait rather than a copy-pasted closure so the contended harness
/// below is written once and the three implementations cannot silently
/// drift into measuring different amounts of work.
trait Counter: Sync {
    fn new() -> Self;
    fn bump(&self);
    fn get(&self) -> u64;
}

impl Counter for Spinlock<u64> {
    fn new() -> Self {
        Spinlock::new(0)
    }

    fn bump(&self) {
        *self.lock() += 1;
    }

    fn get(&self) -> u64 {
        *self.lock()
    }
}

impl Counter for McsSpinlock<u64> {
    fn new() -> Self {
        McsSpinlock::new(0)
    }

    fn bump(&self) {
        *self.lock() += 1;
    }

    fn get(&self) -> u64 {
        *self.lock()
    }
}

impl Counter for Mutex<u64> {
    fn new() -> Self {
        Mutex::new(0)
    }

    fn bump(&self) {
        *self.lock().unwrap() += 1;
    }

    fn get(&self) -> u64 {
        *self.lock().unwrap()
    }
}

/// Times `threads` threads each performing `iters` critical sections,
/// with `outside` pauses of non-critical work between them.
///
/// The threads are spawned, then held at a barrier; the clock starts
/// only once every one of them is runnable, so thread creation is
/// outside the measurement and what is left is the lock handoff. The
/// returned duration is wall time for the whole burst, which is what
/// `iter_custom` wants.
///
/// `outside` is what decides whether this measures a lock or measures
/// a queue. At zero every thread re-enters the critical section the
/// instant it leaves, so the offered load is unbounded and the lock is
/// saturated by construction: throughput is then `1 / handoff_cost`
/// and nothing else, for any lock. Contention disappears once the
/// non-critical work exceeds roughly `threads * handoff_cost`, since
/// that is the point at which the queue drains faster than it fills.
fn contended_run<L: Counter>(threads: usize, iters: u64, outside: u32) -> Duration {
    let lock = L::new();
    let barrier = Barrier::new(threads + 1);

    let elapsed = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    barrier.wait();
                    for _ in 0..iters {
                        lock.bump();
                        spin_work(outside);
                    }
                })
            })
            .collect();

        barrier.wait();
        let start = Instant::now();
        for h in handles {
            h.join().unwrap();
        }
        start.elapsed()
    });

    // Cheap correctness check: a lock that loses updates would
    // otherwise benchmark beautifully.
    assert_eq!(lock.get(), threads as u64 * iters, "lost an update");

    elapsed
}

/// The two `try_lock` outcomes, measured apart.
///
/// They are genuinely different code paths: the success case pays for
/// a read-modify-write that must own the cache line, while the
/// failure case is rejected by the strong compare-exchange without
/// ever taking the lock. Averaging them together would hide both.
fn try_lock(c: &mut Criterion) {
    let mut group = c.benchmark_group("try_lock");

    let spin = Spinlock::new(0u64);
    group.bench_function("spinlock/free", |b| {
        b.iter(|| {
            let mut g = spin.try_lock().unwrap();
            *g += black_box(1);
        })
    });

    // Held for the whole run, so every attempt inside the timing loop
    // takes the failure path.
    let held = Spinlock::new(0u64);
    let _guard = held.lock();
    group.bench_function("spinlock/held", |b| {
        b.iter(|| black_box(held.try_lock()).is_none())
    });
    drop(_guard);

    // `McsSpinlock::try_lock` makes the same single strong CAS as
    // `Spinlock::try_lock` -- it never queues, since queueing is the
    // one thing a caller polling in a loop does not want -- so the
    // difference between these two rows is the node bookkeeping and
    // nothing else.
    let mcs = McsSpinlock::new(0u64);
    group.bench_function("mcs/free", |b| {
        b.iter(|| {
            let mut g = mcs.try_lock().unwrap();
            *g += black_box(1);
        })
    });

    let mcs_held = McsSpinlock::new(0u64);
    let _mcs_guard = mcs_held.lock();
    group.bench_function("mcs/held", |b| {
        b.iter(|| black_box(mcs_held.try_lock()).is_none())
    });
    drop(_mcs_guard);

    let mutex = Mutex::new(0u64);
    group.bench_function("mutex/free", |b| {
        b.iter(|| {
            let mut g = mutex.try_lock().unwrap();
            *g += black_box(1);
        })
    });

    group.finish();
}

/// Throughput as threads are added, with no work outside the lock.
///
/// Starts at two threads: see `common::thread_counts` for why one is
/// not a contended measurement.
///
/// This is the saturated case, and it is worth being explicit about
/// what that means: every thread is at all times either holding the
/// lock or waiting for it, so the workload is 100% serial and no lock
/// can scale. The numbers are therefore a measurement of handoff cost
/// under an N-deep queue, not of how the locks behave in a program.
/// `work_ratio` below is the one that answers that.
///
/// Throughput is set to the thread count because one `iter_custom`
/// iteration is one critical section *per thread*, so criterion
/// reports critical sections per second across the whole machine
/// rather than per thread.
fn contended(c: &mut Criterion) {
    let mut group = c.benchmark_group("contended");

    for threads in thread_counts() {
        group.throughput(Throughput::Elements(threads as u64));

        group.bench_with_input(
            BenchmarkId::new("spinlock", threads),
            &threads,
            |b, &threads| b.iter_custom(|iters| contended_run::<Spinlock<u64>>(threads, iters, 0)),
        );

        group.bench_with_input(BenchmarkId::new("mcs", threads), &threads, |b, &threads| {
            b.iter_custom(|iters| contended_run::<McsSpinlock<u64>>(threads, iters, 0))
        });

        group.bench_with_input(
            BenchmarkId::new("mutex", threads),
            &threads,
            |b, &threads| b.iter_custom(|iters| contended_run::<Mutex<u64>>(threads, iters, 0)),
        );
    }

    group.finish();
}

/// Throughput at full thread count as non-critical work is added.
///
/// The thread sweep varies the wrong axis. Contention is not caused by
/// having many threads, it is caused by threads asking for the lock
/// faster than it can be handed over, and the quantity that decides
/// that is the ratio of non-critical work to critical section. Holding
/// it pinned at zero -- which every benchmark above does -- measures
/// the one point on the curve where the answer is fixed in advance.
///
/// So: pin the threads at the machine's parallelism, the worst case
/// the sweep above reaches, and walk the ratio instead. The interesting
/// result is where the curves meet, because to the right of that point
/// the choice of lock stops being a decision.
///
/// Ids are labelled by pause count rather than by nanoseconds so that
/// `--baseline` can match runs up; the calibration printed at the top
/// converts the axis to time for this machine.
fn work_ratio(c: &mut Criterion) {
    let threads = thread_counts().last().copied().unwrap_or(4);

    println!(
        "\nwork_ratio: {threads} threads, one spin_loop() ~= {:.1} ns here",
        pause_nanos()
    );

    let mut group = c.benchmark_group("work_ratio");

    // Fewer, shorter samples than the default: each iteration of the
    // right-hand end of this sweep costs microseconds rather than
    // nanoseconds, and criterion sizes its batches accordingly.
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(6));

    // Stopping at 256. Past that the non-critical work dominates so
    // completely that both locks report `threads / spin_work`, which
    // is a measurement of the pause instruction rather than of a lock
    // -- and 256 has already shown the convergence that is the point
    // of the sweep.
    for outside in [0u32, 4, 16, 64, 256] {
        group.throughput(Throughput::Elements(threads as u64));

        group.bench_with_input(
            BenchmarkId::new("spinlock", outside),
            &outside,
            |b, &outside| {
                b.iter_custom(|iters| contended_run::<Spinlock<u64>>(threads, iters, outside))
            },
        );

        group.bench_with_input(BenchmarkId::new("mcs", outside), &outside, |b, &outside| {
            b.iter_custom(|iters| contended_run::<McsSpinlock<u64>>(threads, iters, outside))
        });

        group.bench_with_input(
            BenchmarkId::new("mutex", outside),
            &outside,
            |b, &outside| {
                b.iter_custom(|iters| contended_run::<Mutex<u64>>(threads, iters, outside))
            },
        );
    }

    group.finish();
}

criterion_group!(benches, try_lock, contended, work_ratio);
criterion_main!(benches);
