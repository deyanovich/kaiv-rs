#!/usr/bin/env bash
# Refresh the vendored conformance tree from the conformance repo.
#
# The vectors are the executable definition of correct, and they
# live in their own public repository:
#
#   https://gitlab.com/kaiv-format/conformance
#
# Vendoring them here is what lets `cargo test` work on a clone
# with no sibling checkout, and what pins a run to an identifiable
# vector set instead of "whatever the neighbouring directory
# happened to be at". That repo stays authoritative: this copies
# one way and records the source commit in VECTORS. Never edit
# kaiv/tests/conformance-vectors/ by hand — the next refresh
# overwrites it.
#
#   scripts/refresh-vectors.sh [<conformance-repo>]
#
# Default source is ../conformance, matching the sibling layout.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
src_repo="${1:-$repo_root/../conformance}"
src="$src_repo"
dest="$repo_root/kaiv/tests/conformance-vectors"

if [ ! -d "$src" ]; then
    echo "refresh-vectors: no conformance tree at $src" >&2
    echo "  clone it beside this repo, or pass its path:" >&2
    echo "  git clone https://gitlab.com/kaiv-format/conformance" >&2
    echo "  scripts/refresh-vectors.sh <path>" >&2
    exit 1
fi

# Provenance is the point of the exercise: a vendored tree that
# cannot name its source commit is no more pinnable than a path.
# Refuse to record a commit that does not describe the tree we are
# about to copy.
if ! git -C "$src_repo" rev-parse --git-dir >/dev/null 2>&1; then
    echo "refresh-vectors: $src_repo is not a git repo" >&2
    exit 1
fi
if ! git -C "$src_repo" diff --quiet . ||
   ! git -C "$src_repo" diff --cached --quiet .; then
    echo "refresh-vectors: uncommitted changes under $src" >&2
    echo "  commit them upstream first, so the vendored" >&2
    echo "  tree can name the commit it came from." >&2
    exit 1
fi

commit=$(git -C "$src_repo" rev-parse HEAD)
described=$(git -C "$src_repo" describe --tags --always --dirty 2>/dev/null || echo "$commit")

rm -rf "$dest"
mkdir -p "$dest"
# Content only: no ownership/times (-a), and emphatically not the
# source repo's own .git — a nested repository inside the test
# tree confuses tooling and bloats the checkout.
( cd "$src" && git archive --format=tar HEAD ) | tar -x -C "$dest"

cat > "$dest/VECTORS" <<EOF
# Vendored conformance tree — DO NOT EDIT.
#
# Copied by scripts/refresh-vectors.sh from the conformance repo,
# which is authoritative: edit vectors there and re-run the
# script. This file is what pins a test run to a vector set.
source: gitlab.com/kaiv-format/conformance
commit: $commit
describe: $described
EOF

count=$(find "$dest" -mindepth 1 -maxdepth 2 -type d | wc -l | tr -d ' ')
echo "refresh-vectors: vendored $count vector directories from $described"
echo "  $dest"
