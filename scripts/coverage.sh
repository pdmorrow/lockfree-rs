#!/usr/bin/env bash
#
# Source-based code coverage, gated on a minimum line coverage of the
# library itself.
#
# Uses rustc's own `-C instrument-coverage` plus the llvm-tools that
# ship with the toolchain, so there is nothing to install if you have
# the `llvm-tools` component (cargo-llvm-cov is nicer, but this works
# on a toolchain that has no rustup).
#
# The number the gate uses is deliberately not the one llvm-cov puts
# at the bottom of its table. This crate keeps its unit tests inside
# `src/**` in a `#[cfg(test)] mod test`, and test code is by
# definition almost entirely executed, so including it would pad the
# figure with several hundred lines that are guaranteed to be green.
# The gate therefore counts only the lines above the `#[cfg(test)]`
# marker in each file -- the library. The full table is printed too,
# for reference.
#
# Usage:
#   scripts/coverage.sh                 # report + enforce the target
#   scripts/coverage.sh --min 95        # override the target (default 100)
#   scripts/coverage.sh --html          # also write an HTML report
#   scripts/coverage.sh --open          # ... and open it
#   COVERAGE_MIN=100 scripts/coverage.sh
#
# Anything after `--` is passed on to `cargo test`, e.g.
#   scripts/coverage.sh -- --test-threads=1

set -euo pipefail

MIN=${COVERAGE_MIN:-100}
HTML=0
OPEN=0
CARGO_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --min) MIN=$2; shift 2 ;;
        --html) HTML=1; shift ;;
        --open) HTML=1; OPEN=1; shift ;;
        --) shift; CARGO_ARGS=("$@"); break ;;
        -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    esac
done

cd "$(dirname "$0")/.."

# --- locate the llvm tools ------------------------------------------
#
# Version matters: a .profraw written by this rustc's LLVM cannot
# always be read by a different LLVM major version, which shows up as
# "unsupported instrumentation profile format version". The copies
# inside the toolchain's sysroot are guaranteed to match, so they are
# tried first and the ones on PATH are only a fallback.
SYSROOT=$(rustc --print sysroot)
HOST=$(rustc -vV | sed -n 's/^host: //p')
TOOLS="$SYSROOT/lib/rustlib/$HOST/bin"

find_tool() {
    if [[ -x "$TOOLS/$1" ]]; then
        echo "$TOOLS/$1"
    elif command -v "$1" >/dev/null 2>&1; then
        echo >&2 "note: using $1 from PATH; if it reports an unsupported"
        echo >&2 "      profile format, install the matching llvm-tools:"
        echo >&2 "      rustup component add llvm-tools-preview"
        command -v "$1"
    else
        echo >&2 "error: $1 not found."
        echo >&2 "  rustup:  rustup component add llvm-tools-preview"
        echo >&2 "  debian:  apt install llvm  (version must match rustc's LLVM)"
        exit 1
    fi
}

PROFDATA=$(find_tool llvm-profdata)
LLVMCOV=$(find_tool llvm-cov)

OUT=target/coverage
RAW=$OUT/raw
rm -rf "$OUT"
mkdir -p "$RAW"

export RUSTFLAGS="${RUSTFLAGS:-} -C instrument-coverage"
export LLVM_PROFILE_FILE="$PWD/$RAW/spinlock-rs-%p-%m.profraw"

# Coverage needs its own build (the instrumentation changes every
# object file), and a separate target dir keeps it from thrashing the
# cache of ordinary `cargo build`/`cargo test` runs.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/coverage-build}"

echo "==> running the test suite under instrumentation"
cargo test --tests "${CARGO_ARGS[@]}"

# The doctests are part of the public API's coverage, but instrumented
# doctests need an unstable flag (-Z doctest-in-workspace and friends)
# and would need every rustdoc invocation instrumented too, so they are
# measured by the ordinary `cargo test` run and left out here.

# --- object files ----------------------------------------------------
#
# llvm-cov needs the binaries that produced the counters, not the
# source; cargo prints their paths in its JSON output.
mapfile -t OBJECTS < <(
    cargo test --tests --no-run --message-format=json 2>/dev/null |
        jq -r 'select(.profile.test == true) | .filenames[]' |
        grep -v '\.dSYM'
)

if [[ ${#OBJECTS[@]} -eq 0 ]]; then
    echo "error: no test binaries found" >&2
    exit 1
fi

# llvm-cov's command line takes the first binary as a positional
# argument and the rest via -object; positional arguments after that
# are read as source-file filters, so the split matters.
MAIN_OBJECT=${OBJECTS[0]}
OBJ_ARGS=()
for o in "${OBJECTS[@]:1}"; do OBJ_ARGS+=(-object "$o"); done

echo "==> merging profiles"
"$PROFDATA" merge -sparse "$RAW"/*.profraw -o "$OUT/coverage.profdata"

# Only this crate's own sources; everything under ~/.cargo, the
# standard library and the benchmarks are noise here.
mapfile -t SOURCES < <(find src -name '*.rs' | sort)

COMMON=("$MAIN_OBJECT" "${OBJ_ARGS[@]}" -instr-profile="$OUT/coverage.profdata"
        --ignore-filename-regex='(/\.cargo/|/rustc/|^/usr/|benches/)')

echo
echo "==> full table (library + in-file unit tests)"
"$LLVMCOV" report "${COMMON[@]}" "${SOURCES[@]}"

"$LLVMCOV" export "${COMMON[@]}" --format=text "${SOURCES[@]}" > "$OUT/coverage.json"

if [[ $HTML -eq 1 ]]; then
    "$LLVMCOV" show "${COMMON[@]}" --format=html --show-line-counts-or-regions \
        --output-dir="$OUT/html" "${SOURCES[@]}"
    echo "==> HTML report: $OUT/html/index.html"
    [[ $OPEN -eq 1 ]] && { command -v xdg-open >/dev/null && xdg-open "$OUT/html/index.html"; }
fi

# --- the gate --------------------------------------------------------
echo
python3 - "$OUT/coverage.json" "$MIN" <<'PY'
import json
import sys

report_path, minimum = sys.argv[1], float(sys.argv[2])
data = json.load(open(report_path))["data"][0]

def test_mod_line(path):
    """First line of the in-file `#[cfg(test)]` module, or infinity."""
    try:
        with open(path) as f:
            for n, line in enumerate(f, 1):
                if line.strip().startswith("#[cfg(test)]"):
                    return n
    except OSError:
        pass
    return float("inf")

total_covered = total_lines = 0
rows, uncovered = [], []

for f in data["files"]:
    path = f["filename"]
    cutoff = test_mod_line(path)

    # llvm-cov's segments are (line, col, count, has_count, ...). A
    # line counts as executable if some segment on it carries a count,
    # and as covered if the largest such count is non-zero -- which is
    # how llvm-cov derives its own line coverage.
    per_line = {}
    for seg in f["segments"]:
        line, _col, count, has_count = seg[0], seg[1], seg[2], seg[3]
        if has_count and line < cutoff:
            per_line[line] = max(per_line.get(line, 0), count)

    if not per_line:
        continue

    covered = sum(1 for c in per_line.values() if c > 0)
    total_covered += covered
    total_lines += len(per_line)
    rows.append((path, covered, len(per_line)))
    uncovered += [(path, ln) for ln, c in sorted(per_line.items()) if c == 0]

pct = 100.0 * total_covered / total_lines if total_lines else 100.0

print("==> library coverage (excluding the #[cfg(test)] modules)")
for path, covered, lines in rows:
    print(f"    {path:<40} {covered:>4}/{lines:<4} {100.0 * covered / lines:6.2f}%")
print(f"    {'TOTAL':<40} {total_covered:>4}/{total_lines:<4} {pct:6.2f}%")

if uncovered:
    print("\n    never executed:")
    for path, line in uncovered:
        print(f"      {path}:{line}")

print()
if pct + 1e-9 < minimum:
    print(f"FAIL: {pct:.2f}% is below the {minimum:.2f}% target")
    sys.exit(1)
print(f"PASS: {pct:.2f}% meets the {minimum:.2f}% target")
PY
