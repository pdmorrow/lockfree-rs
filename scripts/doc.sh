#!/usr/bin/env bash
#
# Render the crate documentation.
#
# Most of this crate is commentary, and it reads better rendered than
# it does in a source file, so this is the intended way to read it.
#
# The default includes private items, which is the non-obvious part.
# `cache::Aligned`, `spin::spin_hint` and the MCS node pool are all
# private, and they are exactly what the module prose is about: with
# only the public API rustdoc drops their pages entirely, and the
# intra-doc links pointing at them come out as plain text instead of
# hyperlinks. Pass --public for the view a consumer of the crate gets.
#
# Usage:
#   scripts/doc.sh                  # build the docs
#   scripts/doc.sh --open           # ... and open them in a browser
#   scripts/doc.sh --public         # only the public API, as docs.rs shows it
#   scripts/doc.sh --deps           # document the dependencies too
#   scripts/doc.sh --strict         # fail on any rustdoc warning
#
# Anything after `--` is passed on to `cargo doc`.

set -euo pipefail

PRIVATE=1
OPEN=0
NO_DEPS=1
STRICT=0
CARGO_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --open) OPEN=1; shift ;;
        --public) PRIVATE=0; shift ;;
        --deps) NO_DEPS=0; shift ;;
        --strict) STRICT=1; shift ;;
        --) shift; CARGO_ARGS=("$@"); break ;;
        -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    esac
done

cd "$(dirname "$0")/.."

ARGS=()
[[ $NO_DEPS -eq 1 ]] && ARGS+=(--no-deps)

FLAGS="${RUSTDOCFLAGS:-}"
[[ $STRICT -eq 1 ]] && FLAGS="$FLAGS -D warnings"

if [[ $PRIVATE -eq 1 ]]; then
    ARGS+=(--document-private-items)
    # The `private_intra_doc_links` lint fires on a public module whose
    # docs link to a private item, and keeps firing even when
    # --document-private-items has rendered that item and resolved the
    # link. Under this mode the warning is simply wrong, so drop it.
    # Listed after -D warnings so it survives --strict.
    FLAGS="$FLAGS -A rustdoc::private_intra_doc_links"
fi

export RUSTDOCFLAGS="$FLAGS"

echo "==> cargo doc ${ARGS[*]}"
cargo doc "${ARGS[@]}" "${CARGO_ARGS[@]}"

INDEX="target/doc/lockfree_rs/index.html"
echo "==> $INDEX"

if [[ $OPEN -eq 1 ]]; then
    # `cargo doc --open` rather than xdg-open: it already knows how to
    # find a browser on each platform, and re-running it here is free
    # because the build above is up to date.
    cargo doc "${ARGS[@]}" "${CARGO_ARGS[@]}" --open
fi
