//! Benchmarks for [`Spinlock`], measured side by side with
//! [`std::sync::Mutex`] so the numbers have a reference point.
//!
//! Three things are worth measuring about a lock, and they pull in
//! different directions:
//!
//!   * the uncontended round trip (one CAS, one store), which is what
//!     a lock costs when it is not actually excluding anybody;
//!   * `try_lock`, both when it succeeds and when it fails, since the
//!     failing path is what a caller polls in a loop;
//!   * throughput under real contention, where a spinlock burns CPU
//!     to avoid a context switch and a Mutex does the opposite.
//!
//! Run with `cargo bench`. See README.md for how to read the output.

use std::hint::black_box;
use std::sync::{Barrier, Mutex};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use lockfree_rs::spinlock::Spinlock;

/// The one operation both locks are asked to perform: take the lock,
/// mutate the payload, release it.
///
/// A trait rather than a copy-pasted closure so the contended harness
/// below is written once and the two implementations cannot silently
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

/// Thread counts to sweep: 1, 2, 4, ... up to the machine's
/// parallelism.
///
/// Stopping at `available_parallelism` is deliberate. Oversubscribing
/// a spinlock is not a slow case but a pathological one -- a waiter
/// holds its core doing nothing while the thread it is waiting for
/// is descheduled -- so including it would drown the interesting
/// range in one enormous bar.
fn thread_counts() -> Vec<usize> {
    let max = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut counts: Vec<usize> = std::iter::successors(Some(1usize), |n| Some(n * 2))
        .take_while(|&n| n <= max)
        .collect();

    if counts.last() != Some(&max) {
        counts.push(max);
    }

    counts
}

/// Times `threads` threads each performing `iters` critical sections.
///
/// The threads are spawned, then held at a barrier; the clock starts
/// only once every one of them is runnable, so thread creation is
/// outside the measurement and what is left is the lock handoff. The
/// returned duration is wall time for the whole burst, which is what
/// `iter_custom` wants.
fn contended_run<L: Counter>(threads: usize, iters: u64) -> Duration {
    let lock = L::new();
    let barrier = Barrier::new(threads + 1);

    let elapsed = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    barrier.wait();
                    for _ in 0..iters {
                        lock.bump();
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

/// Lock, mutate, unlock on a single thread with nobody else in sight.
///
/// This is the floor: an uncontended acquire is one successful
/// compare-exchange and one release store, on a cache line that is
/// already Exclusive to this core.
fn uncontended(c: &mut Criterion) {
    let mut group = c.benchmark_group("uncontended");

    let spin = Spinlock::new(0u64);
    group.bench_function("spinlock/lock", |b| {
        b.iter(|| {
            *spin.lock() += black_box(1);
        })
    });

    let mutex = Mutex::new(0u64);
    group.bench_function("mutex/lock", |b| {
        b.iter(|| {
            *mutex.lock().unwrap() += black_box(1);
        })
    });

    group.finish();
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

    let mutex = Mutex::new(0u64);
    group.bench_function("mutex/free", |b| {
        b.iter(|| {
            let mut g = mutex.try_lock().unwrap();
            *g += black_box(1);
        })
    });

    group.finish();
}

/// Throughput as threads are added.
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
            |b, &threads| b.iter_custom(|iters| contended_run::<Spinlock<u64>>(threads, iters)),
        );

        group.bench_with_input(
            BenchmarkId::new("mutex", threads),
            &threads,
            |b, &threads| b.iter_custom(|iters| contended_run::<Mutex<u64>>(threads, iters)),
        );
    }

    group.finish();
}

criterion_group!(benches, uncontended, try_lock, contended);
criterion_main!(benches);
