//! Who actually gets the lock, and how often does it change hands.
//!
//! [`benches/spinlock.rs`] reports throughput, and under contention
//! throughput is not a measure of how good a lock is -- it is very
//! largely a measure of how *unfair* the lock is. Every acquisition
//! granted to the thread that just released is one that costs no
//! cache-line transfer at all, so a lock that lets one thread run
//! away with a burst of a hundred posts a spectacular elements/second
//! figure while the other eleven threads make no progress whatsoever.
//! Nothing in a throughput number distinguishes that from a lock that
//! is genuinely fast.
//!
//! This is also the table [`McsSpinlock`] exists for. Its whole
//! claim is that arrival order is service order, which is a claim
//! about the batch and spread columns and about nothing else: a FIFO
//! lock cannot barge, so it cannot buy throughput the way the other
//! two do, and a throughput-only comparison would score it purely on
//! the price it pays and never on what it buys.
//!
//! So this target measures the thing that distinguishes them. The
//! payload under the lock records which thread touched it last, which
//! makes the count of *handoffs* -- acquisitions that changed hands --
//! observable from inside the critical section, where it cannot be
//! raced. The ratio of acquisitions to handoffs is the batch factor:
//! 1.0 is a lock that strictly alternates, and large values mean the
//! throughput column is being paid for out of somebody's latency.
//!
//! Not a criterion benchmark, because there is nothing here to
//! optimise for and no confidence interval to report: it is a fixed
//! wall-clock window and a set of counters. Run it with
//! `cargo bench --bench fairness`.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Barrier, Mutex};
use std::time::{Duration, Instant};

use spinlock_rs::mcs_spinlock::McsSpinlock;
use spinlock_rs::spinlock::Spinlock;

mod common;
use common::thread_counts;

/// How long each lock is hammered for, per thread count.
const WINDOW: Duration = Duration::from_millis(500);

/// The value under the lock.
///
/// Deliberately non-atomic, like the payload in the concurrency
/// tests: every field here is written under the lock, so a lock that
/// failed to exclude would corrupt the counts rather than merely
/// slow them down.
struct Payload {
    /// Id of the thread that most recently held the lock, or
    /// `usize::MAX` before anyone has.
    last: usize,
    /// Total critical sections entered.
    acquisitions: u64,
    /// Of those, the ones that changed hands.
    handoffs: u64,
}

impl Payload {
    fn new() -> Self {
        Payload {
            last: usize::MAX,
            acquisitions: 0,
            handoffs: 0,
        }
    }

    #[inline]
    fn touch(&mut self, id: usize) {
        self.acquisitions += 1;

        if self.last != id {
            self.handoffs += 1;
            self.last = id;
        }
    }
}

/// The operation every lock is asked to perform.
///
/// The same trait-not-closure reasoning as in the throughput target:
/// written once, so the three implementations cannot drift into doing
/// different amounts of work inside the critical section.
trait Tracked: Sync {
    fn new() -> Self;
    fn bump(&self, id: usize);
    fn read(&self) -> (u64, u64);
}

impl Tracked for Spinlock<Payload> {
    fn new() -> Self {
        Spinlock::new(Payload::new())
    }

    fn bump(&self, id: usize) {
        self.lock().touch(id);
    }

    fn read(&self) -> (u64, u64) {
        let p = self.lock();
        (p.acquisitions, p.handoffs)
    }
}

impl Tracked for McsSpinlock<Payload> {
    fn new() -> Self {
        McsSpinlock::new(Payload::new())
    }

    fn bump(&self, id: usize) {
        self.lock().touch(id);
    }

    fn read(&self) -> (u64, u64) {
        let p = self.lock();
        (p.acquisitions, p.handoffs)
    }
}

impl Tracked for Mutex<Payload> {
    fn new() -> Self {
        Mutex::new(Payload::new())
    }

    fn bump(&self, id: usize) {
        self.lock().unwrap().touch(id);
    }

    fn read(&self) -> (u64, u64) {
        let p = self.lock().unwrap();
        (p.acquisitions, p.handoffs)
    }
}

/// What one `(lock, threads)` cell of the table came out as.
struct Row {
    /// Critical sections per second across the whole machine.
    throughput: f64,
    /// Acquisitions per handoff: 1.0 strictly alternates.
    batch: f64,
    /// Acquisitions by the least and most successful thread.
    min: u64,
    max: u64,
}

/// Runs `threads` threads against one lock for [`WINDOW`], and
/// reports both what they achieved and how it was distributed.
///
/// A fixed window rather than a fixed iteration count, because the
/// question is how a lock divides a fixed amount of *opportunity*.
/// Giving every thread the same number of iterations and timing them
/// would hide exactly the effect being looked for: the starved thread
/// would simply hold the measurement open until it caught up, and the
/// result would be the fair number for an unfair lock.
fn measure<L: Tracked>(threads: usize) -> Row {
    let lock = L::new();
    let stop = AtomicBool::new(false);
    let barrier = Barrier::new(threads + 1);
    let counts: Vec<AtomicU64> = (0..threads).map(|_| AtomicU64::new(0)).collect();

    std::thread::scope(|s| {
        for id in 0..threads {
            let (lock, stop, barrier, counts) = (&lock, &stop, &barrier, &counts);

            s.spawn(move || {
                barrier.wait();

                // The stop flag is checked once per chunk rather than
                // once per acquisition: a relaxed load of a line that
                // eleven other threads are also reading is cheap, but
                // it is not free, and at these rates it would be a
                // visible fraction of the critical section.
                const CHUNK: u64 = 64;
                let mut n = 0;

                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..CHUNK {
                        lock.bump(id);
                    }

                    n += CHUNK;
                }

                counts[id].store(n, Ordering::Relaxed);
            });
        }

        barrier.wait();
        let start = Instant::now();

        // Sleeping rather than spinning: a twelfth spinning thread in
        // the harness would be one more competitor for the cores the
        // measurement is about.
        std::thread::sleep(WINDOW);
        stop.store(true, Ordering::Relaxed);
        black_box(start.elapsed());
    });

    let (acquisitions, handoffs) = lock.read();
    let counts: Vec<u64> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();

    Row {
        throughput: counts.iter().sum::<u64>() as f64 / WINDOW.as_secs_f64(),
        batch: acquisitions as f64 / handoffs.max(1) as f64,
        min: counts.iter().copied().min().unwrap_or(0),
        max: counts.iter().copied().max().unwrap_or(0),
    }
}

fn print_row(name: &str, threads: usize, row: Row) {
    // Spread is the ratio of the busiest thread to the idlest, which
    // is the blunt version of the batch factor: batch says how long a
    // burst is, spread says whether the bursts evened out over the
    // window. A lock can batch heavily and still be even-handed over
    // half a second, and that is a different complaint from starvation.
    let spread = row.max as f64 / row.min.max(1) as f64;

    println!(
        "{name:<10} {threads:>7} {:>11.2} {:>8.1} {:>11} {:>11} {spread:>8.2}x",
        row.throughput / 1e6,
        row.batch,
        row.min,
        row.max,
    );
}

fn main() {
    println!(
        "\nFairness under contention, {}ms window per cell, no work \
         outside the critical section.\n",
        WINDOW.as_millis()
    );
    println!(
        "{:<10} {:>7} {:>11} {:>8} {:>11} {:>11} {:>9}",
        "lock", "threads", "Melem/s", "batch", "min/thread", "max/thread", "spread"
    );

    for threads in thread_counts() {
        print_row("spinlock", threads, measure::<Spinlock<Payload>>(threads));
        print_row("mcs", threads, measure::<McsSpinlock<Payload>>(threads));
        print_row("mutex", threads, measure::<Mutex<Payload>>(threads));
    }

    println!(
        "\nbatch  = acquisitions per handoff; 1.0 strictly alternates, higher means\n\
         \x20        the throughput column is bought with somebody's acquisition latency.\n\
         spread = busiest thread / idlest thread over the window."
    );
}
