//! A single lock, a single configuration, one process: the shape
//! `perf` needs.
//!
//! The criterion targets are the wrong thing to point a profiler at.
//! `cargo bench` runs warm-up passes, a calibration loop, three lock
//! implementations and a whole thread sweep inside one process, and a
//! counter that ran for all of it answers a question nobody asked.
//! Worse, criterion's own sampling machinery -- timers, statistics,
//! allocation for the sample vectors -- lands in the same profile as
//! the critical section, and there is no way to tell them apart after
//! the fact.
//!
//! So this target does one thing per invocation. Pick a scenario, a
//! lock and a thread count; it runs exactly that, for a fixed number
//! of acquisitions, and exits. Everything the process does that is
//! not the measured loop -- argument parsing, spawning, the barrier --
//! happens with the counters switched off, because the harness drives
//! `perf`'s control FIFO itself (see [`PerfControl`]). What the
//! counters see is the critical-section loop and nothing else.
//!
//! Fixed acquisitions rather than a fixed window, because the numbers
//! that matter are per acquisition: "cache lines pulled from another
//! core per handoff" is a property of the lock, while "cache lines
//! per second" is mostly a property of how fast the machine happened
//! to be clocking. A fixed count makes the denominator exact and
//! known in advance, so the driver script can normalise without
//! having to trust a number the harness reports about itself.
//!
//! # Scenarios
//!
//! `contended` is the one the crate's argument is about: N threads,
//! one lock, and the question is what the coherence protocol has to
//! do per handoff. A test-and-test-and-set lock has every waiter
//! spinning on the same line, so the release invalidates all of them
//! at once and each waiter refills from the new owner's cache; an MCS
//! lock gives every waiter a line of its own, which the predecessor
//! writes exactly once. Both cost roughly the same instruction count.
//! Only the coherence traffic distinguishes them, and only a hardware
//! counter can see it.
//!
//! `disjoint` is the control experiment for `cache::Aligned`. Every
//! thread gets its *own* lock, so by the logic of the program there is
//! no contention at all and the counters should be flat. Whether they
//! actually are depends entirely on how far apart the flags landed --
//! which is what the padding decides. Running it against `packed`,
//! `pad64` and `pad128` measures the cost of getting that wrong, and
//! measures whether this machine's line is really 64 or effectively
//! 128.
//!
//! # Running it
//!
//! Under the driver, which knows which counters this CPU has:
//!
//! ```sh
//! scripts/perf.sh stat
//! ```
//!
//! Or directly, without a profiler attached, which is a perfectly
//! good smoke test:
//!
//! ```sh
//! cargo bench --bench perf -- --scenario contended --lock mcs --threads 8
//! ```

use std::cell::UnsafeCell;
use std::fs::OpenOptions;
use std::hint::black_box;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Barrier, Mutex};
use std::time::Instant;

use spinlock_rs::mcs_spinlock::McsSpinlock;
use spinlock_rs::spinlock::Spinlock;

mod common;
use common::spin_work;

/// Talks to the `perf stat --control=fifo:...` on the other end.
///
/// `perf` can be started with its counters disabled (`--delay=-1`)
/// and told to enable and disable them by writing `enable` / `disable`
/// to a FIFO. That is the whole reason this harness can report clean
/// numbers: the alternative is to subtract an estimate of the setup
/// cost afterwards, which for a run this short is a bigger correction
/// than several of the effects being measured.
///
/// The FIFOs are opened read-write even though each is used in one
/// direction. Opening a FIFO read-only blocks until a writer appears
/// and vice versa, so the obvious code deadlocks against a `perf`
/// that is waiting for us at the same moment; `O_RDWR` on a FIFO
/// never blocks, which is the standard way out and the one perf's own
/// manual page uses from bash.
struct PerfControl {
    ctl: std::fs::File,
    ack: BufReader<std::fs::File>,
}

impl PerfControl {
    fn open(ctl_path: &str, ack_path: &str) -> std::io::Result<Self> {
        let opts = {
            let mut o = OpenOptions::new();
            o.read(true).write(true);
            o
        };

        Ok(PerfControl {
            ctl: opts.open(ctl_path)?,
            ack: BufReader::new(opts.open(ack_path)?),
        })
    }

    /// Sends a command and waits for perf to acknowledge it.
    ///
    /// The wait is the point. Without reading the `ack` back, the
    /// harness would race ahead into the measured loop while perf was
    /// still arming the counters, and the first slice of the workload
    /// would go uncounted -- exactly the error this is here to avoid.
    fn send(&mut self, cmd: &str) {
        writeln!(self.ctl, "{cmd}").expect("write to perf control fifo");
        self.ctl.flush().expect("flush perf control fifo");

        let mut ack = String::new();
        self.ack
            .read_line(&mut ack)
            .expect("read ack from perf control fifo");
    }
}

/// Holds every worker at the start line until the counters are armed.
///
/// Two barriers rather than one, and the reason is a race that a
/// single barrier cannot avoid. Arming the counters means a write to
/// perf's control FIFO and a read of the acknowledgement back, which
/// is a round trip through another process and costs on the order of
/// a hundred microseconds. Release the workers first and that round
/// trip runs *concurrently with the workload*: for the contended
/// sweeps it is lost in the noise, but the uncontended `disjoint` runs
/// finish in well under a millisecond, and one of them was observed
/// completing before the counters were switched on at all -- reported,
/// hilariously, as seventy billion acquisitions per second.
///
/// So the workers arrive at `ready` and block on `go`. Only when every
/// one of them is parked does the main thread arm the counters, start
/// the clock and open `go`. What that leaves inside the measured
/// region is the barrier's own wake-up, which is a few microseconds,
/// identical for every lock, and unavoidable in any case: some
/// synchronisation has to release the threads.
struct Gate {
    ready: Barrier,
    go: Barrier,
}

impl Gate {
    fn new(threads: usize) -> Self {
        Gate {
            ready: Barrier::new(threads + 1),
            go: Barrier::new(threads + 1),
        }
    }

    /// Called by a worker: announce it is up, then wait to be let go.
    fn arrive(&self) {
        self.ready.wait();
        self.go.wait();
    }

    /// Called by the main thread once the workers are spawned: waits
    /// for all of them, arms the counters, and releases them.
    ///
    /// Returns the instant the workload started, so the caller's wall
    /// clock and the counters cover the same region.
    fn start(&self, ctl: &mut Option<PerfControl>) -> Instant {
        self.ready.wait();

        if let Some(c) = ctl.as_mut() {
            c.send("enable");
        }

        let start = Instant::now();
        self.go.wait();
        start
    }

    /// Called by the main thread once every worker has joined.
    fn stop(ctl: &mut Option<PerfControl>) {
        if let Some(c) = ctl.as_mut() {
            c.send("disable");
        }
    }
}

/// The one operation every lock is asked to perform.
///
/// Same trait-not-closure reasoning as the criterion target: the
/// measured loop is written once, so no implementation can drift into
/// doing a different amount of work inside the critical section. That
/// matters more here than there, because the entire result is a
/// comparison of counter values between implementations that are
/// supposed to differ in exactly one respect.
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

/// Defines a test-and-test-and-set lock whose only variable is how
/// far apart two of them sit in memory.
///
/// The algorithm is `Spinlock`'s, reduced to the part that touches
/// memory: a relaxed load until the flag looks free, then one
/// acquiring compare-exchange to claim it, then a releasing store to
/// drop it. It is duplicated here rather than reused because the
/// experiment needs the *same* code at three different alignments,
/// and `cache::Aligned` is not a knob the public API exposes -- nor
/// should it be, since there is exactly one right answer for a lock
/// somebody else is going to use.
///
/// The payload lives in the same struct as the flag on purpose. That
/// is how a real lock is laid out, and it means the alignment
/// attribute is deciding the placement of everything a critical
/// section touches, not just of the atomic.
macro_rules! ttas_lock {
    ($(#[$attr:meta])* $name:ident, $doc:expr) => {
        #[doc = $doc]
        $(#[$attr])*
        struct $name {
            locked: AtomicBool,
            value: UnsafeCell<u64>,
        }

        // SAFETY: `value` is only ever reached with `locked` held, so
        // at most one thread has a reference to it at a time. Same
        // argument as the crate's `Spinlock`, which has it written
        // out at length.
        unsafe impl Sync for $name {}

        impl Counter for $name {
            fn new() -> Self {
                $name {
                    locked: AtomicBool::new(false),
                    value: UnsafeCell::new(0),
                }
            }

            fn bump(&self) {
                loop {
                    // The "test" half: a plain load, which can be
                    // served from a shared copy in this core's L1
                    // and generates no coherence traffic while the
                    // lock stays held.
                    while self.locked.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }

                    // The "and-set" half: this one has to own the
                    // line, so it invalidates every other copy.
                    if self
                        .locked
                        .compare_exchange_weak(
                            false,
                            true,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }

                // SAFETY: the flag is ours until the store below.
                unsafe { *self.value.get() += 1 };
                self.locked.store(false, Ordering::Release);
            }

            fn get(&self) -> u64 {
                // Called after every worker has joined, so no lock is
                // needed and none is taken.
                //
                // SAFETY: sole reference; all writers have exited.
                unsafe { *self.value.get() }
            }
        }
    };
}

ttas_lock!(
    #[repr(align(128))]
    Pad128,
    "Padded the way `cache::Aligned` pads on x86_64."
);
ttas_lock!(
    #[repr(align(64))]
    Pad64,
    "Padded to one coherence granule, the naive answer."
);
ttas_lock!(
    Packed,
    "Not padded at all: natural alignment, several per line."
);

/// N threads, one lock, `acquisitions` critical sections each.
///
/// The measured region starts once every thread is past the barrier
/// and stops when the last one has finished its count -- but note
/// that it stops on the *slowest* thread, so the tail of the region
/// has fewer threads contending than the head. That skew is inherent
/// to a fixed-count run and is the price of an exact denominator; it
/// is small as long as the lock is not wildly unfair, and how unfair
/// each lock is has its own target in `benches/fairness.rs`.
fn contended<L: Counter>(
    threads: usize,
    acquisitions: u64,
    work: u32,
    ctl: &mut Option<PerfControl>,
) -> f64 {
    let lock = L::new();
    let gate = Gate::new(threads);

    let elapsed = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    gate.arrive();
                    for _ in 0..acquisitions {
                        lock.bump();
                        spin_work(work);
                    }
                })
            })
            .collect();

        let start = gate.start(ctl);
        for h in handles {
            h.join().unwrap();
        }
        let elapsed = start.elapsed();

        Gate::stop(ctl);
        elapsed
    });

    assert_eq!(
        lock.get(),
        threads as u64 * acquisitions,
        "lost an update: the lock does not exclude"
    );

    elapsed.as_secs_f64()
}

/// N threads, N locks, one each: contention that exists only in the
/// cache.
///
/// No thread ever waits for another here -- each one owns its lock
/// outright, every acquisition succeeds on the first compare-exchange,
/// and a machine with no caches would show a flat line as threads are
/// added. Any slope is the coherence protocol moving lines between
/// cores because two logically unrelated flags landed in one of them.
///
/// The locks are allocated as a single `Vec` rather than one per
/// thread, so their spacing is decided by the type's alignment and
/// nothing else. That is the entire independent variable.
fn disjoint<L: Counter>(
    threads: usize,
    acquisitions: u64,
    work: u32,
    ctl: &mut Option<PerfControl>,
) -> f64 {
    let locks: Vec<L> = (0..threads).map(|_| L::new()).collect();
    let gate = Gate::new(threads);

    let elapsed = std::thread::scope(|s| {
        let handles: Vec<_> = locks
            .iter()
            .map(|lock| {
                s.spawn(|| {
                    gate.arrive();
                    for _ in 0..acquisitions {
                        lock.bump();
                        spin_work(work);
                    }
                })
            })
            .collect();

        let start = gate.start(ctl);
        for h in handles {
            h.join().unwrap();
        }
        let elapsed = start.elapsed();

        Gate::stop(ctl);
        elapsed
    });

    for lock in &locks {
        assert_eq!(lock.get(), acquisitions, "lost an update");
    }

    elapsed.as_secs_f64()
}

const USAGE: &str = "\
usage: perf [options]

  --scenario contended|disjoint   what to run        (default contended)
  --lock NAME                     which lock         (default spinlock)
  --threads N                     worker threads     (default 4)
  --acquisitions N                per thread         (default 1000000)
  --work N                        spin_loop()s outside the lock (default 0)
  --ctl-fifo PATH --ack-fifo PATH drive perf's counters over its control FIFO

  contended locks: spinlock, mcs, mutex
  disjoint  locks: pad128, pad64, packed

Normally invoked through scripts/perf.sh, which picks the counters.
";

fn main() {
    let mut scenario = String::from("contended");
    let mut lock = String::from("spinlock");
    let mut threads = 4usize;
    let mut acquisitions = 1_000_000u64;
    let mut work = 0u32;
    let mut ctl_fifo: Option<String> = None;
    let mut ack_fifo: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // Every option takes a value, so the closure is shared.
        let mut value = |name: &str| {
            args.next()
                .unwrap_or_else(|| panic!("{name} needs a value\n\n{USAGE}"))
        };

        match arg.as_str() {
            "--scenario" => scenario = value("--scenario"),
            "--lock" => lock = value("--lock"),
            "--threads" => threads = value("--threads").parse().expect("--threads"),
            "--acquisitions" => {
                acquisitions = value("--acquisitions").parse().expect("--acquisitions")
            }
            "--work" => work = value("--work").parse().expect("--work"),
            "--ctl-fifo" => ctl_fifo = Some(value("--ctl-fifo")),
            "--ack-fifo" => ack_fifo = Some(value("--ack-fifo")),
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            // `cargo bench` inserts these when it runs a
            // `harness = false` target; ignoring them means the same
            // command line works with and without cargo in front.
            "--bench" | "--test" => {}
            other => panic!("unknown argument: {other}\n\n{USAGE}"),
        }
    }

    let mut ctl = match (ctl_fifo, ack_fifo) {
        (Some(c), Some(a)) => Some(PerfControl::open(&c, &a).expect("open perf control fifos")),
        (None, None) => None,
        _ => panic!("--ctl-fifo and --ack-fifo go together\n\n{USAGE}"),
    };

    // black_box on the parameters: they arrive from argv so the
    // optimiser cannot fold them anyway, but saying so keeps that
    // true if this ever grows a compile-time default path.
    let (threads, acquisitions, work) =
        (black_box(threads), black_box(acquisitions), black_box(work));

    let seconds = match (scenario.as_str(), lock.as_str()) {
        ("contended", "spinlock") => {
            contended::<Spinlock<u64>>(threads, acquisitions, work, &mut ctl)
        }
        ("contended", "mcs") => {
            contended::<McsSpinlock<u64>>(threads, acquisitions, work, &mut ctl)
        }
        ("contended", "mutex") => contended::<Mutex<u64>>(threads, acquisitions, work, &mut ctl),

        ("disjoint", "pad128") => disjoint::<Pad128>(threads, acquisitions, work, &mut ctl),
        ("disjoint", "pad64") => disjoint::<Pad64>(threads, acquisitions, work, &mut ctl),
        ("disjoint", "packed") => disjoint::<Packed>(threads, acquisitions, work, &mut ctl),

        (s, l) => panic!("no such scenario/lock combination: {s}/{l}\n\n{USAGE}"),
    };

    // One machine-readable line, so the driver never has to guess at
    // what the run actually did. The counters come from perf; this
    // supplies the denominator and the wall clock to divide by.
    println!(
        "harness scenario={scenario} lock={lock} threads={threads} work={work} \
         acquisitions={} seconds={seconds:.6}",
        threads as u64 * acquisitions
    );
}
