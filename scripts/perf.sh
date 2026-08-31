#!/usr/bin/env bash
#
# Hardware performance counters for the locks, collected automatically.
#
# The benchmarks in `benches/spinlock.rs` say the queued lock overtakes
# the barging one at around four threads. They do not say why, because
# a stopwatch cannot: the two locks execute a similar number of
# instructions and differ almost entirely in what they ask the cache
# coherence protocol to do, and coherence traffic is invisible to
# every timer. It is not invisible to the CPU's performance monitoring
# unit, which counts cache lines fetched from another core's cache
# directly. That counter is the mechanism behind the crossover, and
# this script is how it gets read.
#
# What it drives is `benches/perf.rs`, not the criterion targets --
# see that file's header for why a profiler pointed at `cargo bench`
# measures mostly criterion. One invocation of that harness is one
# lock at one thread count, and it brackets its own measured region
# using perf's control FIFO, so the counters never see the thread
# spawn or the barrier.
#
# Usage:
#   scripts/perf.sh                    # the thread sweep, table + report
#   scripts/perf.sh stat               # ... the same thing, named
#   scripts/perf.sh padding            # what cache::Aligned buys, measured
#   scripts/perf.sh all                # both sweeps into one report
#   scripts/perf.sh record             # profile one run, attribute samples
#   scripts/perf.sh c2c                # which cache line, and which offset
#   scripts/perf.sh events             # the counters chosen for this CPU
#
# Options:
#   --threads LIST     comma-separated sweep    (default: 2,4,...,nproc)
#   --locks LIST       which locks              (default: spinlock,mcs,mutex)
#   --acquisitions N   critical sections per thread per run (default 300000)
#   --work N           spin_loop()s outside the lock        (default 0)
#   --repeat N         runs per point, best-of is not taken; all are kept
#   --out DIR          output directory         (default target/perf)
#   --no-report        skip the HTML report
#
# `record` and `c2c` take a single point rather than a sweep, so give
# them one lock and one thread count:
#   scripts/perf.sh record --locks mcs --threads 12
#
# Requirements: `perf` (linux-perf / linux-tools-$(uname -r)), `jq`,
# and python3 for the report. Reading counters from an unprivileged
# process needs
#
#   sudo sysctl kernel.perf_event_paranoid=1
#
# which this script checks for and explains if it is not set. It never
# needs root itself: everything here is per-process counting on a
# process perf started, which paranoid level 1 allows.

set -euo pipefail

cd "$(dirname "$0")/.."

CMD=stat
THREADS=""
LOCKS=""
ACQUISITIONS=300000
ACQ_EXPLICIT=0
WORK=0
REPEAT=1
OUT=target/perf
REPORT=1

while [[ $# -gt 0 ]]; do
    case "$1" in
        stat|padding|all|record|c2c|events) CMD=$1; shift ;;
        --threads) THREADS=$2; shift 2 ;;
        --locks) LOCKS=$2; shift 2 ;;
        --acquisitions) ACQUISITIONS=$2; ACQ_EXPLICIT=1; shift 2 ;;
        --work) WORK=$2; shift 2 ;;
        --repeat) REPEAT=$2; shift 2 ;;
        --out) OUT=$2; shift 2 ;;
        --no-report) REPORT=0; shift ;;
        -h|--help) sed -n '2,55p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    esac
done

# --- preflight -------------------------------------------------------

if ! command -v perf >/dev/null 2>&1; then
    cat >&2 <<'MSG'
error: `perf` is not on PATH.

  debian/ubuntu:  apt install linux-perf        (or linux-tools-$(uname -r))
  fedora:         dnf install perf
  arch:           pacman -S perf

A distro sometimes ships a `perf` that refuses to run against a
different kernel version than it was built for; it will say so.
MSG
    exit 1
fi

PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 3)
if [[ $PARANOID -gt 1 ]]; then
    cat >&2 <<MSG
error: kernel.perf_event_paranoid is $PARANOID, which blocks the
       hardware counters this script reads.

  sudo sysctl kernel.perf_event_paranoid=1        # this session
  echo 'kernel.perf_event_paranoid=1' | sudo tee /etc/sysctl.d/99-perf.conf

Level 1 allows an unprivileged process to count events on processes it
owns, which is all of what happens here -- no system-wide collection,
no raw tracepoints, no root.
MSG
    exit 1
fi

# --- which counters this CPU has -------------------------------------
#
# The event that matters -- "a load was satisfied by a cache line held
# by another core" -- has no portable name. It is a vendor-specific
# PMU event with a different name per microarchitecture, and the
# generic `cache-misses` is no substitute: a line arriving from DRAM
# and a line arriving from the core next door are both misses, and
# only the second one is what a contended lock does.
#
# So: name the candidates per vendor, ask perf to open each one
# against a trivial process, and keep the ones that work. Probing
# rather than a `case $VENDOR` table because the names move between
# microarchitectures within a vendor, and a wrong guess should degrade
# to a smaller event set rather than fail the run.
probe() {
    perf stat -e "$1" -- true >/dev/null 2>&1
}

# Each entry is `role:candidate,candidate,...`; the first candidate
# that opens wins the role. Roles, not raw names, are what the report
# is written against.
declare -A EVENT_OF=()
declare -a ROLES=()

pick() {
    local role=$1 candidate
    shift
    for candidate in "$@"; do
        if probe "$candidate"; then
            EVENT_OF[$role]=$candidate
            ROLES+=("$role")
            return 0
        fi
    done
    return 0
}

# cycles and instructions come off fixed-function counters on both
# vendors, so they are free and never displace anything below.
pick cycles cycles
pick instructions instructions

# The headline. AMD: a demand fill sourced from L3 or from another
# L2 in the same core complex -- on a single-CCX part that is exactly
# "another core had this line". Intel: an L3 hit where the line was
# snooped out of another core, forwarded (XSNP_FWD on Ice Lake and
# later) or taken while modified (XSNP_HITM before it).
pick transfer \
    ls_dmnd_fills_from_sys.int_cache \
    mem_load_l3_hit_retired.xsnp_fwd \
    mem_load_l3_hit_retired.xsnp_hitm \
    mem_load_uops_l3_hit_retired.xsnp_hitm

# The contrast to it: fills that stayed local, i.e. the ones that cost
# nothing in coherence terms. A lock that spins on a line nobody else
# writes generates these and not the ones above.
pick local_fill \
    ls_dmnd_fills_from_sys.lcl_l2 \
    mem_load_retired.l2_hit

# Locked read-modify-writes that the core could not execute
# speculatively. Not a cache event, and NOT a count of every atomic:
# on Zen the speculated ones land in `ls_locks.spec_lock_*` instead, so
# a lock whose compare-exchange usually succeeds first time can report
# close to zero here. Read it as "contention on the atomic itself" --
# the barging lock's figure climbs with the thread count because that
# is its failed compare-exchanges being retried, while the queued
# lock's single unconditional swap almost never shows up.
pick atomics \
    ls_locks.non_spec_lock \
    mem_inst_retired.lock_loads

# Software events; they use no PMU counter at all, so they cannot
# cause multiplexing. `context-switches` is what separates a spinlock
# from a Mutex more sharply than any cache event does.
pick ctxsw context-switches

if [[ -z ${EVENT_OF[transfer]:-} ]]; then
    echo >&2 "note: no cache-to-cache transfer event for this CPU; falling back"
    echo >&2 "      to cache-misses, which cannot tell a DRAM miss from a line"
    echo >&2 "      taken off another core. The comparison still works, but the"
    echo >&2 "      absolute numbers mean less."
    pick transfer cache-misses
fi

EVENT_LIST=$(for r in "${ROLES[@]}"; do printf '%s,' "${EVENT_OF[$r]}"; done)
EVENT_LIST=${EVENT_LIST%,}

if [[ $CMD == events ]]; then
    echo "counters selected for this CPU:"
    printf '  %-13s %s\n' "role" "event"
    for r in "${ROLES[@]}"; do printf '  %-13s %s\n' "$r" "${EVENT_OF[$r]}"; done
    echo
    echo "perf stat -e $EVENT_LIST"
    exit 0
fi

# --- defaults that depend on the machine -----------------------------

NPROC=$(nproc)

if [[ -z $THREADS ]]; then
    # The same sweep the criterion benchmarks use: powers of two up to
    # the machine's parallelism, plus that parallelism itself. Starting
    # at two, since one thread contends with nobody and there is no
    # coherence traffic to count.
    t=2
    while [[ $t -le $NPROC ]]; do THREADS="${THREADS:+$THREADS,}$t"; t=$((t * 2)); done
    [[ ${THREADS##*,} -ne $NPROC ]] && THREADS="$THREADS,$NPROC"
fi

if [[ -z $LOCKS ]]; then
    case "$CMD" in
        padding) LOCKS=packed,pad64,pad128 ;;
        *)       LOCKS=spinlock,mcs,mutex ;;
    esac
fi

mkdir -p "$OUT"

# --- the built binary ------------------------------------------------
#
# perf has to wrap the harness itself, not `cargo bench`: cargo forks,
# and while perf follows children it would also be counting cargo's own
# work and attributing the samples to it. `--no-run` builds and prints
# where the binary landed.
echo "==> building the harness"
BIN=$(cargo bench --bench perf --no-run --message-format=json 2>/dev/null |
          jq -r 'select(.target.name == "perf" and .executable != null) | .executable' |
          tail -1)

if [[ -z ${BIN:-} || ! -x $BIN ]]; then
    echo "error: could not find the built `perf` bench binary" >&2
    exit 1
fi

# --- one measured run ------------------------------------------------
#
# perf is started with its counters off (`--delay=-1`) and handed a
# pair of FIFOs; the harness turns them on once its threads are all
# past the barrier and off again when the last one joins. Without this
# the counts would include process startup, three page-faulting thread
# spawns and the barrier -- which for a run measured in tenths of a
# second is a larger correction than several of the effects being
# measured.
FIFO_DIR=$(mktemp -d)
trap 'rm -rf "$FIFO_DIR"' EXIT

run_one() {
    local scenario=$1 lock=$2 threads=$3 acquisitions=$4 out=$5

    rm -f "$FIFO_DIR/ctl" "$FIFO_DIR/ack"
    mkfifo "$FIFO_DIR/ctl" "$FIFO_DIR/ack"

    perf stat -x, --delay=-1 \
        --control="fifo:$FIFO_DIR/ctl,$FIFO_DIR/ack" \
        -e "$EVENT_LIST" \
        -- "$BIN" \
        --scenario "$scenario" --lock "$lock" \
        --threads "$threads" --acquisitions "$acquisitions" --work "$WORK" \
        --ctl-fifo "$FIFO_DIR/ctl" --ack-fifo "$FIFO_DIR/ack" \
        >"$out.stdout" 2>"$out.stderr"
}

CSV="$OUT/counters.csv"

sweep() {
    local scenario=$1

    # An uncontended acquisition is roughly two orders of magnitude
    # cheaper than a contended one -- a dozen cycles against several
    # thousand -- so the same acquisition count that gives the
    # contended sweep a comfortable tenth of a second gives the
    # padding sweep a run shorter than the FIFO handshake that arms
    # the counters. Scale it up, unless the caller said otherwise.
    local acquisitions=$ACQUISITIONS
    if [[ $scenario == disjoint && $ACQ_EXPLICIT -eq 0 ]]; then
        acquisitions=$((ACQUISITIONS * 20))
    fi

    echo "==> $scenario: ${LOCKS//,/ } over ${THREADS//,/ } threads,"
    echo "    $acquisitions acquisitions per thread, $WORK pauses outside the lock"

    local threads lock rep
    for threads in ${THREADS//,/ }; do
        for lock in ${LOCKS//,/ }; do
            for rep in $(seq 1 "$REPEAT"); do
                printf '    %-9s %3s threads ... ' "$lock" "$threads"

                local stem="$FIFO_DIR/run"
                if ! run_one "$scenario" "$lock" "$threads" "$acquisitions" "$stem"; then
                    echo "FAILED"
                    sed 's/^/      /' "$stem.stderr" >&2
                    exit 1
                fi

                # The harness prints its own line on stdout; perf's
                # CSV goes to stderr. Both are needed: perf supplies
                # the counters, the harness supplies the exact number
                # of acquisitions they were counted over.
                local acq seconds
                acq=$(sed -n 's/.*acquisitions=\([0-9]*\).*/\1/p' "$stem.stdout")
                seconds=$(sed -n 's/.*seconds=\([0-9.]*\).*/\1/p' "$stem.stdout")

                # perf's -x, columns are: count, unit, event, run time,
                # run percentage, metric, metric unit. A count of
                # `<not counted>` means the event never got a counter,
                # and a run percentage below 100 means it was
                # multiplexed and the count is an extrapolation -- both
                # are carried through to the report rather than hidden.
                awk -F, -v s="$scenario" -v l="$lock" -v t="$threads" \
                    -v w="$WORK" -v a="$acq" -v sec="$seconds" -v r="$rep" \
                    '$3 != "" && $1 !~ /not (counted|supported)/ {
                         printf "%s,%s,%s,%s,%s,%s,%s,%s,%s\n",
                                s, l, t, w, a, sec, r, $3, $1
                         if ($5 != "" && $5 + 0 < 99.5) mux = $3 " " $5 "%"
                     }
                     END { if (mux != "") print "    (multiplexed: " mux ")" > "/dev/stderr" }' \
                    "$stem.stderr" >>"$CSV"

                printf '%8.1f ms\n' "$(awk -v x="$seconds" 'BEGIN { print x * 1000 }')"
            done
        done
    done
}

case "$CMD" in
    stat|padding|all)
        echo "scenario,lock,threads,work,acquisitions,seconds,rep,event,count" >"$CSV"
        ;;
esac

case "$CMD" in
    stat)    sweep contended ;;
    padding) sweep disjoint ;;
    all)
        SAVED_LOCKS=$LOCKS
        LOCKS=spinlock,mcs,mutex; sweep contended
        LOCKS=packed,pad64,pad128; sweep disjoint
        LOCKS=$SAVED_LOCKS
        ;;

    record)
        # A profile rather than a count: where in the code the cycles
        # went.
        #
        # `--call-graph dwarf` is not optional here, and not for the
        # usual reason. At this optimisation level the entire critical
        # section -- `bump`, `lock`, the atomic operations inside it --
        # inlines into the thread closure, so a flat profile is a
        # single symbol at 99% and says nothing at all. The unwind
        # information is what still carries the inlined frames, and the
        # call graph is where the split between "waiting on the shared
        # flag" and "waiting on my own node" actually shows up.
        LOCK=${LOCKS%%,*}
        T=${THREADS##*,}
        DATA="$OUT/perf.data"

        echo "==> recording $LOCK at $T threads"
        perf record -o "$DATA" --call-graph dwarf -F 999 \
            -e cycles \
            -- "$BIN" --scenario contended --lock "$LOCK" \
               --threads "$T" --acquisitions "$ACQUISITIONS" --work "$WORK"
        echo

        # The flat profile, which is one line and exists to show why
        # it is not the answer: at this optimisation level `bump`,
        # `lock` and the atomics all inline into the thread closure,
        # so symbol-level attribution has exactly one symbol to offer.
        echo "==> flat profile"
        perf report -i "$DATA" --stdio --no-children -g none \
            --percent-limit 5 2>/dev/null | grep -E '^\s+[0-9]+\.[0-9]+%' | head -3

        # Which is why this is the real output. Every instruction the
        # lock executes is in one basic block, and `perf annotate`
        # attributes cycles to each one -- so the difference between
        # the two algorithms is legible as a difference between two
        # short lists of instructions.
        #
        # Sampling skid means a cycle count lands a little after the
        # instruction that earned it, which is why the weight sits on
        # the `test` following a load rather than on the load: the
        # `test` is where the pipeline waits for the line to arrive.
        echo
        echo "==> hottest instructions (>= 1% of cycles, plus every locked one)"
        if ANNOTATED=$(perf annotate -i "$DATA" --stdio --percent-limit 5 2>/dev/null); then
            echo "$ANNOTATED" |
                awk '$1 ~ /^[0-9]+\.[0-9]+$/ && ($1 + 0 >= 1.0 || /lock /) \
                     { print "   ", $0 }' |
                sed 's/ <.*>$//'
        else
            cat <<'MSG'
    perf annotate failed on this build. It segfaults on some distro
    perf packages (observed on Debian's 6.12); a perf matching the
    running kernel generally fixes it. The counters in `stat` and
    `padding` do not depend on it.
MSG
        fi

        # Only worth mentioning if the names actually came out
        # mangled: perf demangles Rust's v0 scheme itself when it was
        # built with support for it, and recent builds are.
        if perf report -i "$DATA" --stdio -g none 2>/dev/null | grep -q '_R[NIN][a-zA-Z]'; then
            echo
            echo "note: \`cargo install rustfilt\` will demangle those symbol names;"
            echo "      a perf built with Rust demangling support does it itself."
        fi

        cat <<MSG

==> $DATA
    perf annotate -i $DATA --stdio     # the above, with source interleaved
    perf report -i $DATA -g graph,0.5  # interactive, inlined frames

    What to look for: \`Spinlock\` splits its cycles between the load of
    the one shared flag and the branch retrying a failed
    \`lock cmpxchg\` -- waiting, and losing races. \`McsSpinlock\` has no
    retry branch in its profile at all, because there is no race to
    lose: nearly all of its cycles sit on a single load of the flag in
    the thread's own queue node.
MSG
        ;;

    c2c)
        # `perf c2c` is the tool built for exactly this question: it
        # samples memory accesses with their data addresses, groups
        # them by cache line, and reports which lines were bounced
        # between cores and which *offsets within the line* did it.
        # That last column is what turns "false sharing" from a theory
        # into an address.
        LOCK=${LOCKS%%,*}
        SCENARIO=contended
        case "$LOCK" in packed|pad64|pad128) SCENARIO=disjoint ;; esac
        T=${THREADS##*,}
        DATA="$OUT/c2c.data"

        echo "==> c2c record: $SCENARIO / $LOCK at $T threads"
        if ! perf c2c record -o "$DATA" \
             -- "$BIN" --scenario "$SCENARIO" --lock "$LOCK" \
                --threads "$T" --acquisitions "$ACQUISITIONS" --work "$WORK"; then
            cat >&2 <<'MSG'

error: `perf c2c record` failed.

It needs precise memory sampling: IBS on AMD (kernel 5.19+, an
`ibs_op` PMU under /sys/bus/event_source/devices) or PEBS with
load-latency on Intel. A VM usually has neither.
MSG
            exit 1
        fi
        echo
        REPORT_TXT=$(perf c2c report -i "$DATA" --stdio)
        echo "$REPORT_TXT" | head -80

        # c2c depends on the memory-sampling hardware tagging each
        # sample with where the line came from, and that tagging is the
        # part that varies. It is worth checking rather than trusting,
        # because the failure mode is silent: unrecognised data sources
        # are dropped, every line ends up in no category, and the
        # report says "0 shared cache lines" -- which reads exactly
        # like a clean result.
        if echo "$REPORT_TXT" | grep -q 'Total Shared Cache Lines *: *0'; then
            cat >&2 <<'MSG'

note: c2c found no shared cache lines, which on this machine means it
      could not classify the samples rather than that there were none
      -- `scripts/perf.sh padding` counts real cache-to-cache transfers
      in the same workload. Look for a large "Unable to parse data
      source" count in the trace summary above: that is the tell.

      The per-line attribution needs the sampled data source to carry
      coherence state. Intel's PEBS load-latency does; AMD's IBS
      reports it only on newer parts, and Zen 3 does not, so c2c is a
      no-op there. `scripts/perf.sh stat` and `padding` do not depend
      on any of this and work on both vendors.
MSG
        else
            cat <<MSG

==> $DATA
    perf c2c report -i $DATA          # interactive; 'd' opens a line

    Read the "Shared Data Cache Line Table" first: each row is one
    64-byte line, sorted by how much of the machine's cache-to-cache
    traffic it caused. Then open a line to get the per-offset
    breakdown -- two different offsets in one row is false sharing,
    one offset is genuine contention on the same word.
MSG
        fi
        ;;
esac

if [[ $CMD == stat || $CMD == padding || $CMD == all ]]; then
    echo
    if [[ $REPORT -eq 1 ]]; then
        python3 scripts/perf_report.py "$CSV" --out "$OUT/report.html" \
                --events "$(for r in "${ROLES[@]}"; do
                                printf '%s=%s;' "$r" "${EVENT_OF[$r]}"
                            done)"
        echo
        echo "==> $OUT/report.html"
    else
        echo "==> $CSV"
    fi
fi
