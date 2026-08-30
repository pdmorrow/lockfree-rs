# spinlock-rs

Concurrency primitives written from scratch, for study mainly but more
than suitable for production: from a learning perspective every line of
the implementation is explained, tested, and measured.

Currently implemented:

| Module | Type | Summary |
| --- | --- | --- |
| [`spinlock`](src/spinlock/mod.rs) | `Spinlock<T>` | Mutual exclusion by busy-waiting. Test-and-test-and-set on a cache-line-padded flag, RAII guard, no poisoning, supports unsized `T`. |
| [`mcs_spinlock`](src/mcs_spinlock/mod.rs) | `McsSpinlock<T>` | The same, queued. Mellor-Crummey/Scott: waiters spin on their own node instead of a shared flag, so a handoff costs one cache line transfer no matter how many are waiting, and the lock is strictly FIFO. |

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
```

Warnings are worth keeping at zero here, since most of what this crate
does is `unsafe`:

```sh
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## Documentation

Most of this crate is commentary — the notes at the top of
[`src/mcs_spinlock/mod.rs`](src/mcs_spinlock/mod.rs) run to a hundred
lines before the first `use` — and it reads better rendered than it
does in a source file.

```sh
scripts/doc.sh                   # build
scripts/doc.sh --open            # ... and open it in a browser
scripts/doc.sh --public          # only the public API, as docs.rs would show it
scripts/doc.sh --strict          # fail on any rustdoc warning
```

The script passes `--document-private-items` by default, which is the
part worth knowing about. `cache::Aligned`, `spin::spin_hint` and the
MCS node pool are all private, and they are precisely what the prose
discusses: without the flag rustdoc drops their pages entirely, and
the links to them from the `mcs_spinlock` header render as plain text
rather than as hyperlinks. Use `--public` to check what a consumer of
the crate would actually see.

One wrinkle, in case the output looks contradictory: rustdoc's
`private_intra_doc_links` lint fires on a public module that links to
a private item and keeps firing under `--document-private-items`, even
though the link has by then resolved to a real page. The script
therefore silences that one lint in its default mode — including under
`--strict` — and leaves it on under `--public`, where it is telling
the truth.

### If `--open` does nothing

`--open` defers to `cargo doc --open`, which consults `$BROWSER`
before it falls back to `xdg-open`. So a `$BROWSER` left pointing at a
browser that is no longer installed is a *quiet* failure: cargo prints

```
warning: Couldn't open docs with firefox: No such file or directory (os error 2)
```

and then exits 0. No window appears, nothing downstream notices, and
the warning is easy to scroll past at the end of a build log. The
script now checks for this and says so before handing over, but the
fix is in the environment rather than here — either point the variable
at something real or let xdg decide:

```sh
export BROWSER=brave-browser   # whatever you actually run
unset BROWSER                  # ... or defer to the xdg default
```

None of which is needed to read the docs. The path is printed on every
run, so the browser can be skipped:

```sh
xdg-open target/doc/spinlock_rs/index.html
python3 -m http.server -d target/doc 8000   # http://localhost:8000/spinlock_rs/
```

Prefer the second over SSH: a `file://` URL does not survive a port
forward, and a served directory does.

## Test

```sh
cargo test                  # unit tests + doctests
cargo test --lib            # unit tests only
cargo test -- --nocapture   # show output from the tests that print
```

Each lock's suite lives beside it and covers four kinds of claim:
trait bounds (compile-time assertions that `Spinlock<Cell<u32>>` is
`Sync` and friends), memory layout, single-threaded behaviour
including drop counts and unwinding, and concurrency. The concurrency
tests are the interesting ones — they use a non-atomic `u64` payload
deliberately, so any failure to serialise shows up as a lost update
rather than as something the hardware papers over.

[`src/mcs_spinlock/mod.rs`](src/mcs_spinlock/mod.rs) adds two
categories of its own: the node pool (that guards may be dropped out
of order, that acquisitions recycle nodes instead of allocating, and
that a lock taken from inside a thread-local destructor still works
after the pool has been destroyed), and the layout of the queue node
itself, whose flag and link must not share a line.

Its concurrency tests use a fixed four threads where the `spinlock`
ones scale with `available_parallelism`, and the difference is the
algorithm rather than an arbitrary choice. `cargo test` runs tests in
parallel, so the machine is oversubscribed — which a barging lock
shrugs off and a strictly FIFO one cannot. On a 12-core box, letting
the MCS tests ask for 12 threads each took the suite from 0.09 seconds
to 64; the same tests with the machine to themselves run in 0.03
either way. That is the convoy cost of no barging, reproduced
accidentally, and it is worth keeping in mind before reading the
benchmark section below.

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
`cargo test` passes all 38 tests on x86 in a tenth of a second, while
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

Doctests are not instrumented (that needs unstable rustdoc flags);
they are still executed by `cargo test`.

## Benchmark

```sh
cargo bench                              # the whole suite
cargo bench -- contended                 # filter by name
cargo bench -- --save-baseline main      # record a baseline
cargo bench -- --baseline main           # compare against it
cargo bench --bench fairness             # just the fairness table
```

Benchmarks use [criterion](https://docs.rs/criterion), which runs each
case until the measurement is statistically stable and reports a
confidence interval rather than a single number. With a baseline saved
it also reports the change and whether it is significant, which is the
only trustworthy way to evaluate a change to a lock — the run-to-run
noise on a contended benchmark easily exceeds the effect you are
looking for.

[`benches/spinlock.rs`](benches/spinlock.rs) measures three things,
each for `Spinlock` and `McsSpinlock`, and each against
`std::sync::Mutex` for scale:

* **`try_lock`** — the succeeding and failing paths measured
  separately, because they are genuinely different (the failure path
  is rejected by the compare-exchange without acquiring anything) and
  averaging them would hide both.
* **`contended`** — throughput as threads are added, 2, 4, … up to
  `available_parallelism`, with nothing between one release and the
  next acquire. Threads are spawned and parked on a barrier before the
  clock starts, so thread creation stays out of the measurement. It
  starts at two because one thread contending with nobody is not a
  contended measurement — it is an uncontended round trip wrapped in a
  barrier that adds overhead and no information.
* **`work_ratio`** — threads pinned at `available_parallelism`, and
  the *non-critical* work swept instead. This is the axis the other
  two hold fixed at zero, and it turns out to be the one that decides
  the answer.

What is deliberately absent is the uncontended round trip. A lock that
is not excluding anybody is not a case this crate has any use for —
the whole premise of spinning is that a contending thread is coming,
and soon enough that waiting for it beats sleeping — so measuring the
one-thread floor would only invite a comparison nothing here is
optimised to win.

[`benches/fairness.rs`](benches/fairness.rs) is a fourth measurement
and not a criterion benchmark, because it has no time to report: it
runs a fixed wall-clock window and counts. The payload under the lock
records which thread held it last, so the number of *handoffs* —
acquisitions that actually changed hands — is observable from inside
the critical section where it cannot be raced.

Illustrative output, from an AMD Ryzen 5 5600U (12 hardware
threads); treat the shape as the result, not the digits, since they
move with the machine:

| Case | `Spinlock` | `McsSpinlock` | `std::sync::Mutex` |
| --- | --- | --- | --- |
| `try_lock`, free | 3.09 ns | 4.43 ns | 3.80 ns |
| `try_lock`, held | 2.14 ns | 2.57 ns | — |
| contended, 2 threads | 38.3 Melem/s | 18.5 Melem/s | 51.6 Melem/s |
| contended, 4 threads | 13.7 Melem/s | 15.3 Melem/s | 34.3 Melem/s |
| contended, 8 threads | 8.89 Melem/s | 13.3 Melem/s | 25.1 Melem/s |
| contended, 12 threads | 5.23 Melem/s | 6.65 Melem/s | 21.8 Melem/s |

`Spinlock` loses to `Mutex` from two threads onward, and the gap
widens with every thread added.

`McsSpinlock` pays about 1.3 ns for its node, which makes it the
slowest of the three at `try_lock` — the same CAS as `Spinlock`'s,
wrapped in bookkeeping for a queue `try_lock` never joins. It pays
that price at two threads as well, where it is half `Spinlock`'s
throughput: a two-deep queue is the case a queue cannot help with, and
a strictly FIFO lock has given up the barging that makes the
alternative fast there.

From four threads it is ahead, and the ordering does not reverse
again: 1.5x `Spinlock` at eight threads, and by then the flag every
TTAS waiter shares is being invalidated on every release while each
MCS waiter is spinning on a line nobody else touches. That is the
algorithm doing what it was designed to do.

The obvious reading of the `Mutex` column is that it parks its waiters
and gets out of the way while both spinlocks keep a cache line moving
between cores. That is true as far as it goes, and it is not the main
effect. The fairness table says what is:

| Threads | | Melem/s | batch | spread |
| --- | --- | --- | --- | --- |
| 2 | `Spinlock` | 35.8 | 3.4 | 1.05x |
| 2 | `McsSpinlock` | 17.7 | 1.0 | 1.00x |
| 2 | `Mutex` | 45.9 | 5.5 | 1.07x |
| 4 | `Spinlock` | 14.2 | 3.0 | 1.46x |
| 4 | `McsSpinlock` | 16.5 | 1.0 | 1.00x |
| 4 | `Mutex` | 37.0 | 5.1 | 1.18x |
| 8 | `Spinlock` | 8.29 | 2.2 | 4.86x |
| 8 | `McsSpinlock` | 15.1 | 1.0 | 1.00x |
| 8 | `Mutex` | 26.8 | 4.8 | 1.28x |
| 12 | `Spinlock` | 4.63 | 1.2 | 2.01x |
| 12 | `McsSpinlock` | 9.63 | 1.0 | 1.00x |
| 12 | `Mutex` | 24.7 | 5.0 | 1.04x |

`batch` is acquisitions per handoff. At 12 threads `Mutex` gives the
lock straight back to the thread that just released it four times out
of five: it is *barging*, and each acquisition it grants that way is
one that costs no cache-line transfer at all, because the line is
already Exclusive on that core. `Spinlock` at 1.2 pays a genuine
cross-core transfer nearly every time. `Mutex` is not completing five
times as many handoffs per second — it is completing about the same
number of handoffs and rather more acquisitions per handoff.

That is a real advantage and worth having. It is just not the
advantage the throughput column appears to describe, and it has a
price the throughput column cannot show, which is why `spread` is
there: the throughput number and the batch factor rise together, and
what is being spent to buy them is somebody's acquisition latency.
Read the two tables together or neither.

`McsSpinlock` is the column that makes the point, because it does not
approximate fairness — it is 1.0 batch and 1.00x spread at every
thread count, which is not a good result so much as the algorithm's
specification showing up in a measurement. Every acquisition changes
hands, and over half a second no thread got more than 0.2% more of
the lock than any other. Compare the 8-thread `Spinlock` row, where
the busiest thread got 4.9x the acquisitions of the idlest: that is
what "the hardware picks the winner" looks like when the winner has a
topological advantage, and it is invisible in a throughput number.

What is worth dwelling on is that the fairness is not being bought
here. `McsSpinlock` is *also* the faster of the two spinlocks from
four threads up, and at 12 threads it is a little over twice
`Spinlock`'s throughput. The usual expectation with a FIFO lock is a
trade — pay some throughput, get determinism — and at two threads that
is exactly what happens. Past four it does not, because the cost the
queue removes (N-1 wasted CAS attempts and an N-way invalidation per
release) grows with N while the cost it adds does not.

The `contended` sweep also flatters the effect, because it is
saturated by construction. With no work between releasing the lock and
asking for it again, every thread is at all times either holding or
waiting, the workload is 100% serial, and no lock can scale: what is
left to measure is the cost of one handoff under a 12-deep queue.
`work_ratio` adds the missing axis, at 12 threads, one `spin_loop()`
costing about 15.4 ns on this machine:

| Non-critical work | `Spinlock` | `McsSpinlock` | `std::sync::Mutex` |
| --- | --- | --- | --- |
| none | 4.75 Melem/s | 10.4 Melem/s | 21.3 Melem/s |
| 4 pauses (~62 ns) | 4.65 Melem/s | 11.4 Melem/s | 8.26 Melem/s |
| 16 pauses (~250 ns) | 4.33 Melem/s | 11.6 Melem/s | 8.57 Melem/s |
| 64 pauses (~990 ns) | 8.91 Melem/s | 8.58 Melem/s | 9.26 Melem/s |
| 256 pauses (~3.9 µs) | 2.28 Melem/s | 2.02 Melem/s | 2.30 Melem/s |

Most of `Mutex`'s 4.5x over `Spinlock` is gone by the time each thread
does 62 ns of work outside the lock, and by ~4 µs all three are within
noise of each other and all three are falling — falling because the
lock has stopped being the constraint and the work has become it. The
sweep stops there for that reason: further right every column is just
`threads / non-critical work`, which measures the pause instruction
and not a lock. The threshold is roughly `threads × handoff_cost`,
which is where the queue starts draining faster than it fills; on this
machine that is around 12 × 200 ns.

The `McsSpinlock` column is the one that changes the conclusion. Its
worst showing is at zero non-critical work — the saturated case, which
is the only case the other tables measure — and the first step off
that point, 60 ns of real work per thread, is already enough to put it
ahead of `Mutex` and keep it there until the lock stops mattering at
all. At 16 pauses it is 1.35x
`Mutex` and 2.7x `Spinlock`, and it is doing that while remaining
strictly FIFO. For a 12-thread workload that does any work outside its
critical sections, this is the row to read.

So the honest summary is narrower than the headline. `Spinlock`'s win
is the shallow queue — two threads, where barging beats ordering — and
it holds it. Its loss is confined to the saturated case, and about
that case the interesting fact is that no lock here scales — some are
merely less fair about not scaling. What `Spinlock` genuinely lacks is
any mechanism to degrade gracefully once saturated, and the parking,
the barging and the queue are each such a mechanism.

`McsSpinlock` gives up the thing `Spinlock` is actually good at — it
is slower at `try_lock` and slower at two threads — to buy determinism
it then turns out not to have to pay for, since from four threads up
it is faster as well as fairer. The case against it is the one the
tests already ran into: strict FIFO has no answer to a
descheduled queue head, so it needs the threads to fit on the cores.
Within that constraint it is the better spinlock; outside it, it is
the worse one, and the boundary is sharp rather than gradual.

The contended sweeps stop at the machine's parallelism on purpose.
Oversubscribing a spinlock is not merely a slow case, it is a
pathological one: a waiter holds a core doing nothing while the thread
it is waiting for is descheduled, and the resulting numbers would
drown out the range that matters. That cliff is the entire reason
`Spinlock` is documented as suitable only for critical sections short
enough that spinning beats a context switch.

Note that the fairness target's throughput runs slightly below the
criterion `contended` figures for the same thread count. Its critical
section is doing a little more — a comparison and two increments of
the bookkeeping — and that is the price of being able to see the
handoffs at all.

`McsSpinlock` at 12 threads is the exception, and it reports *higher*
there (9.63 against 6.65). Neither number is wrong; the criterion one
is unstable. That cell's samples have a standard deviation larger than
its own estimate, and its median works out to 10.9 Melem/s against a
mean of 4.33 — a heavy right tail, which is exactly the shape a convoy
makes, one descheduled queue head stalling every thread behind it for
a scheduler quantum. The fairness target averages over a fixed 500 ms
window and absorbs those stalls instead of sampling around them, so it
is the more trustworthy figure for this lock at full occupancy. Treat
the 12-thread MCS throughput as approximate and the shape of the
fairness columns as the result.

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
and aarch64, 256 on s390x, 32 on arm/mips, 64 elsewhere). It lives in
[`src/cache.rs`](src/cache.rs), shared by every lock in the crate, and
is re-exported as `spinlock::CACHE_LINE_ALIGN`. x86_64 uses 128 despite having
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
scripts/doc.sh --strict &&
scripts/coverage.sh &&
scripts/miri.sh --seeds 8 &&
cargo bench
```
