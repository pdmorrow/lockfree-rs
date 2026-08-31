# spinlock-rs

Non-sleeping concurrency primitives, written from scratch. A thread
that cannot take one of these locks stays on its core and spins; it
never parks and never enters the kernel. That is the right trade only
when critical sections are short and threads do not outnumber cores.

| Type | Summary |
| --- | --- |
| [`Spinlock<T>`](src/spinlock/mod.rs) | Test-and-test-and-set on a cache-line-padded flag. RAII guard, no poisoning, supports unsized `T`. Barging. |
| [`McsSpinlock<T>`](src/mcs_spinlock/mod.rs) | Mellor-Crummey/Scott queue lock. Each waiter spins on its own node; strictly FIFO. |

Requires a stable toolchain, edition 2024 (Rust 1.85+).

## API docs

The crate is not published, so build the docs locally:

```sh
scripts/doc.sh --open      # build and open in a browser
scripts/doc.sh --public    # only the public API, as docs.rs would show it
scripts/doc.sh --strict    # fail on any rustdoc warning
```

The default includes private items, which is where most of the prose
is. The output path is printed on every run.

## Tests

```sh
cargo test                  # unit tests + doctests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Miri checks the aliasing and memory-ordering claims that no test on
x86 can:

```sh
rustup toolchain install nightly --component miri   # once
scripts/miri.sh                  # one pass
scripts/miri.sh --seeds 32       # re-run under 32 different schedules
scripts/miri.sh --tree           # Tree Borrows instead of Stacked
scripts/miri.sh concurrent       # filter, exactly like cargo test
```

Coverage:

```sh
scripts/coverage.sh              # report, and enforce the target
scripts/coverage.sh --html       # also write target/coverage/html
scripts/coverage.sh --min 95     # lower the bar
```

## Benchmarks

```sh
cargo bench                              # the whole suite
cargo bench -- contended                 # filter by name
cargo bench -- --save-baseline main      # record a baseline
cargo bench -- --baseline main           # compare against it
cargo bench --bench fairness             # just the fairness table
```

[`benches/spinlock.rs`](benches/spinlock.rs) is
[criterion](https://docs.rs/criterion)-based: `try_lock` (success and
failure paths separately), `contended` (throughput as threads are
added), and `work_ratio` (threads pinned, non-critical work swept).
Every case also runs `std::sync::Mutex` for scale.

[`benches/fairness.rs`](benches/fairness.rs) is not a criterion target.
It runs a fixed wall-clock window and prints acquisitions per handoff
(`batch`) and the busiest/idlest thread ratio (`spread`) — the cost a
throughput number cannot show.

## Hardware counters

`scripts/perf.sh` measures what the locks ask of the cache coherence
protocol, which is what separates them and what no timer can see. It
drives [`benches/perf.rs`](benches/perf.rs) — one lock, one thread
count, one process — not the criterion targets.

```sh
scripts/perf.sh              # thread sweep under perf stat
scripts/perf.sh padding      # what src/cache.rs buys, measured
scripts/perf.sh all          # both, into one report
scripts/perf.sh events       # which counters it picked for this CPU, and why

scripts/perf.sh record --locks mcs --threads 12   # where the cycles went
scripts/perf.sh c2c    --locks packed             # which line, which offset
```

Options: `--threads`, `--locks`, `--acquisitions`, `--work`, `--repeat`,
`--out`, `--no-report`; see `scripts/perf.sh --help`. Needs `perf`,
`jq`, `python3`, and `kernel.perf_event_paranoid` at 1 or lower — never
root. Each run writes `target/perf/counters.csv` and an HTML report.

### Reading the table

Every column is a total divided by the acquisition count, which the
harness fixes in advance; totals would only rank the locks by how slow
they are.

| Column | Meaning |
| --- | --- |
| `lines/acq` | Cache lines fetched from another core or the shared L3 — the coherence traffic, and the headline number. |
| `local/acq` | Fills served from this core's own L2: the cheap ones, where no line moved between cores. |
| `cycles/acq` | Core cycles per critical section summed over *every* thread — what the lock costs the machine, not the latency of one acquisition. |
| `nonspec/acq` | Locked read-modify-writes the core could not run speculatively. Not a census of atomics; read it as contention on the atomic itself. |
| `M acq/s` | Millions of critical sections per second, machine-wide. |

Two caveats. There is no portable event for "a load was satisfied from
another core's cache", so the script probes a per-vendor list and falls
back to `cache-misses` with a warning — under that fallback the
comparison holds but the absolute numbers mean less. And `c2c` needs
the sampled data source to carry coherence state, which AMD parts
before Zen 4 do not: it reports "0 shared cache lines", which reads
like a clean result and is not one. The script flags both cases.

## Cache line assumptions

The lock flag is padded so it cannot share a line with the data it
protects or with a neighbouring lock in an array. `repr(align(N))`
needs a literal, so the value is chosen per target architecture in
[`src/cache.rs`](src/cache.rs) and re-exported as
`spinlock::CACHE_LINE_ALIGN`. A test reads the real line size from the
OS and fails if the compiled-in constant is smaller.

## Everything at once

```sh
cargo fmt --check &&
cargo clippy --all-targets -- -D warnings &&
cargo test &&
scripts/doc.sh --strict &&
scripts/coverage.sh &&
scripts/miri.sh --seeds 8 &&
cargo bench &&
scripts/perf.sh all
```
