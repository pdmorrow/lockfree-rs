#!/usr/bin/env bash
#
# Run the test suite under Miri.
#
# Miri interprets MIR instead of running machine code, which buys two
# things a spinlock cares about a great deal:
#
#   * Undefined behaviour detection. This crate hands out `&mut T`
#     from an `UnsafeCell` behind a raw pointer and asserts `Sync` by
#     hand; Miri's aliasing model (Stacked/Tree Borrows) checks that
#     the references derived that way never overlap illegally, which
#     no amount of native testing can do.
#
#   * A weak memory model. Real x86 hardware is too strongly ordered
#     to punish a missing Acquire or Release -- the bug only appears
#     on ARM, and then only sometimes. Miri emulates the C++20 model
#     the Rust atomics are specified against, so an ordering that is
#     wrong on paper can fail here on any machine.
#
# Usage:
#   scripts/miri.sh                  # one pass, default schedule
#   scripts/miri.sh --seeds 32       # re-run under 32 schedules
#   scripts/miri.sh --tree           # use Tree Borrows, not Stacked
#   scripts/miri.sh spinlock::test::concurrent   # filter, as cargo test
#
# Requires a nightly toolchain with Miri:
#   rustup toolchain install nightly --component miri
#   cargo +nightly miri setup

set -euo pipefail

TOOLCHAIN=${MIRI_TOOLCHAIN:-nightly}
SEEDS=0
FLAGS=(
    # Reject casting an integer back to a usable pointer. Nothing here
    # does that, and the strict model gives Miri sharper provenance
    # information for everything that follows.
    -Zmiri-strict-provenance
)

while [[ $# -gt 0 ]]; do
    case "$1" in
        --seeds) SEEDS=$2; shift 2 ;;
        --tree) FLAGS+=(-Zmiri-tree-borrows); shift ;;
        -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) break ;;
    esac
done

if ! cargo "+$TOOLCHAIN" miri --version >/dev/null 2>&1; then
    cat >&2 <<'MSG'
error: Miri is not available.

  rustup toolchain install nightly --component miri
  cargo +nightly miri setup

(Miri only exists on nightly: it is built against the compiler's
internals, which have no stable interface. If `rustup` itself is
missing -- a distro-packaged rustc has no nightly channel -- install
it from https://rustup.rs and re-run.)
MSG
    exit 1
fi

if [[ $SEEDS -gt 0 ]]; then
    # Each seed is a different set of scheduling decisions. A data
    # race that needs one specific interleaving will not show up in a
    # single run, so this is where the real bug-finding happens; it is
    # off by default because it costs N times as long.
    FLAGS+=("-Zmiri-many-seeds=0..$SEEDS")
fi

cd "$(dirname "$0")/.."

echo "==> MIRIFLAGS=${FLAGS[*]}"
MIRIFLAGS="${MIRIFLAGS:-} ${FLAGS[*]}" exec cargo "+$TOOLCHAIN" miri test "$@"
