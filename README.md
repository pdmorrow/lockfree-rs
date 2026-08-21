# lockfree-rs

Concurrency primitives written from scratch, for study rather than for
production: the point is that every line of the implementation is
explained, tested, and measured.

Currently implemented:

| Module | Type | Summary |
| --- | --- | --- |
| [`spinlock`](src/spinlock/mod.rs) | `Spinlock<T>` | Mutual exclusion by busy-waiting. Test-and-test-and-set on a cache-line-padded flag, RAII guard, no poisoning, supports unsized `T`. |

## Requirements

* A stable Rust toolchain, edition 2024 (Rust 1.85 or newer).
* For `scripts/miri.sh`: a nightly toolchain with the `miri`
  component, installed through [rustup](https://rustup.rs).
* For `scripts/coverage.sh`: `llvm-profdata` and `llvm-cov` (the
  `llvm-tools` rustup component, or a system LLVM matching rustc's),
  plus `jq` and `python3`.

## Build

```sh
cargo build                 # debug
cargo build --release       # optimised
cargo doc --open            # the commentary reads better rendered
```

Warnings are worth keeping at zero here, since most of what this crate
does is `unsafe`:

```sh
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Test

```sh
cargo test                  # unit tests + doctests
cargo test --lib            # unit tests only
cargo test -- --nocapture   # show output from the tests that print
```

The suite in [`src/spinlock/mod.rs`](src/spinlock/mod.rs) covers four
kinds of claim: trait bounds (compile-time assertions that
`Spinlock<Cell<u32>>` is `Sync` and friends), memory layout,
single-threaded behaviour including drop counts and unwinding, and
concurrency. The concurrency tests are the interesting ones — they use
a non-atomic `u64` payload deliberately, so any failure to serialise
shows up as a lost update rather than as something the hardware papers
over.

### Miri

x86 is too strongly ordered to punish a missing `Acquire` or
`Release`, and no native test can detect an aliasing violation in the
`UnsafeCell` -> `&mut T` conversion at the heart of the guard.
[Miri](https://github.com/rust-lang/miri) interprets MIR and checks
both: it enforces the Stacked/Tree Borrows aliasing model, and it
emulates the weak memory model that Rust's atomics are specified
against, so an ordering bug that only manifests on ARM can fail on any
machine.

```sh
rustup toolchain install nightly --component miri   # once
scripts/miri.sh                  # one pass
scripts/miri.sh --seeds 32       # re-run under 32 different schedules
scripts/miri.sh --tree           # Tree Borrows instead of Stacked
scripts/miri.sh concurrent       # filter, exactly like cargo test
```

`--seeds` is where the bug-finding power is: a race that needs one
specific interleaving will not appear in a single run, and each seed
is a different set of scheduling decisions.

Two accommodations are made for Miri in the source, both `cfg!(miri)`
rather than `#[cfg]` so that every branch is still compiled and
type-checked on a normal build:

* **`spin_hint()`** yields to the interpreter's scheduler instead of
  issuing a `pause`. A spin hint that does nothing is fine on hardware
  with real parallelism, but under an interpreter that interleaves
  threads it just burns interpreted instructions waiting for a
  preemption.
* **Iteration counts are divided by `SCALE`** (50 under Miri). Miri
  runs roughly two orders of magnitude slower than native, and it
  explores interleavings per scheduling decision rather than per
  iteration, so shorter runs cost much less coverage than the ratio
  suggests — and `--seeds` buys it back. 50 puts a full pass at about
  7 seconds; the cost is very sublinear in `SCALE`, so dividing
  harder saves little and gives up interleavings.

The thread count also drops to 3 under Miri, since extra threads buy
interleavings rather than parallelism there and each one costs real
time.

To check that this arrangement has teeth, weaken the guard's
`Ordering::Release` store to `Ordering::Relaxed` and run both:
`cargo test` passes all 17 tests on x86 in a tenth of a second, while
Miri fails `writes_are_published_to_the_next_holder` with a data race
between the retag in one thread's critical section and the `Deref` in
the next holder's. That gap is the entire argument for running this
crate under Miri.

### Coverage

```sh
scripts/coverage.sh              # report, and enforce the target
scripts/coverage.sh --html       # also write target/coverage/html
scripts/coverage.sh --min 95     # lower the bar for a work in progress
```

The script uses rustc's own `-C instrument-coverage` and the
`llvm-profdata`/`llvm-cov` pair from the toolchain's sysroot, so it
works without `cargo-llvm-cov` (which is nicer, and which you should
prefer if you have rustup: `cargo llvm-cov --html`).

**The target is 100% of the library, and the build fails below it.**
That is a stricter number than it sounds, because the figure being
gated is not the one llvm-cov prints at the bottom of its table. The
unit tests live inside `src/**` in a `#[cfg(test)] mod test`, and test
code is by definition almost all executed, so including it would pad
the total with several hundred guaranteed-green lines. The gate counts
only the lines above the `#[cfg(test)]` marker in each file — the
library itself — and lists every line that was never executed.

Doctests are not instrumented (that needs unstable rustdoc flags);
they are still executed by `cargo test`.

## Benchmark

```sh
cargo bench                              # the whole suite
cargo bench -- uncontended               # filter by name
cargo bench -- --save-baseline main      # record a baseline
cargo bench -- --baseline main           # compare against it
```

Benchmarks use [criterion](https://docs.rs/criterion), which runs each
case until the measurement is statistically stable and reports a
confidence interval rather than a single number. With a baseline saved
it also reports the change and whether it is significant, which is the
only trustworthy way to evaluate a change to a lock — the run-to-run
noise on a contended benchmark easily exceeds the effect you are
looking for.

[`benches/spinlock.rs`](benches/spinlock.rs) measures three things,
each against `std::sync::Mutex` for scale:

* **`uncontended`** — lock, mutate, unlock on one thread with nobody
  else in sight. This is the floor: one successful compare-exchange
  and one release store, on a cache line already held Exclusive.
  Expect single-digit nanoseconds, and expect `Mutex` to be close,
  since an uncontended `Mutex` never enters the kernel either.
* **`try_lock`** — the succeeding and failing paths measured
  separately, because they are genuinely different (the failure path
  is rejected by the compare-exchange without acquiring anything) and
  averaging them would hide both.
* **`contended`** — throughput as threads are added, 1, 2, 4, … up to
  `available_parallelism`. Threads are spawned and parked on a barrier
  before the clock starts, so thread creation stays out of the
  measurement and what is left is the handoff. Reported as elements
  per second, where an element is one critical section.

Illustrative output, from an AMD Ryzen 5 5600U (12 hardware
threads); treat the shape as the result, not the digits, since they
move with the machine:

| Case | `Spinlock` | `std::sync::Mutex` |
| --- | --- | --- |
| uncontended lock/unlock | 2.61 ns | 4.14 ns |
| `try_lock`, free | 3.28 ns | 4.19 ns |
| `try_lock`, held | 2.40 ns | — |
| contended, 2 threads | 36.0 Melem/s | 44.6 Melem/s |
| contended, 4 threads | 12.6 Melem/s | 32.1 Melem/s |
| contended, 12 threads | 4.88 Melem/s | 20.8 Melem/s |

That is the whole trade in one table. Uncontended, the spinlock is
about 1.6x faster than `Mutex`, because it is two atomic operations
and nothing else. From two threads onward it loses, and the gap widens
with every thread added: waiters keep the flag's cache line moving
between cores and burn their timeslices doing it, while `Mutex` parks
its waiters and gets out of the way. Note that `Mutex` throughput
barely degrades past four threads, and the spinlock's keeps falling.

The contended sweep stops at the machine's parallelism on purpose.
Oversubscribing a spinlock is not merely a slow case, it is a
pathological one: a waiter holds a core doing nothing while the thread
it is waiting for is descheduled, and the resulting numbers would
drown out the range that matters. That cliff is the entire reason
`Spinlock` is documented as suitable only for critical sections short
enough that spinning beats a context switch.

`[profile.bench]` keeps debug symbols on, so `perf record --call-graph
dwarf -- cargo bench -- --profile-time 5` will attribute samples
properly.

## Cache line assumptions

The lock flag is padded so it cannot share a cache line with the data
it protects, or with a neighbouring `Spinlock` in an array — otherwise
a CAS on one lock invalidates the line holding the other, and two
unrelated locks contend for no reason ("false sharing").

The padding cannot be queried at runtime, because `repr(align(N))`
needs `N` as a literal at compile time: by the time a process can call
`sysconf(_SC_LEVEL1_DCACHE_LINESIZE)`, every offset in the struct is
already fixed. A build script could emit the value as a cfg, but it
would bake the *build* machine's geometry into the artifact, which is
wrong as soon as you cross-compile.

So the value is chosen per target architecture (128 bytes on x86_64
and aarch64, 256 on s390x, 32 on arm/mips, 64 elsewhere) and exposed
as `spinlock::CACHE_LINE_ALIGN`. x86_64 uses 128 despite having
64-byte lines because Intel's L2 prefetcher fetches them in aligned
128-byte pairs. The runtime value is then used to *check* that choice:
`alignment_covers_the_platform_cache_line` reads
`/sys/devices/system/cpu/cpu0/cache/index0/coherency_line_size` on
Linux (or `sysctl hw.cachelinesize` on macOS) and fails if the
compiled-in constant is smaller than what the CPU reports.

## Everything at once

```sh
cargo fmt --check &&
cargo clippy --all-targets -- -D warnings &&
cargo test &&
scripts/coverage.sh &&
scripts/miri.sh --seeds 8 &&
cargo bench
```
