#!/usr/bin/env bash
#
# Test the revision rule in tools/build-rev.sh.
#
# The rule decides what a shipped binary says it was built from, and it
# has failed twice: a git that answered for the repository above the
# tree, and a status that local configuration made blind. Both are
# asserted here.

set -euo pipefail

TOOLS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=tools/build-rev.sh
. "$TOOLS/build-rev.sh"

# mktemp answers a symlinked path on macOS, and git reports the real
# one, so the comparison in build_rev needs the resolved path.
tmp="$(cd "$(mktemp -d)" && pwd -P)"
trap 'rm -rf "$tmp"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

outer="$tmp/outer"
mkdir -p "$outer"
git init -q "$outer"
git -C "$outer" config user.email ci@example.com
git -C "$outer" config user.name ci
: > "$outer/tracked"
git -C "$outer" add tracked
git -C "$outer" commit -q -m one
head="$(git -C "$outer" rev-parse --short HEAD)"

got="$(build_rev "$outer")"
[ "$got" = "$head" ] || fail "a clean tree: wanted $head, got '$got'"

# A tree the build host will see with changes must say so, whatever the
# local configuration chooses to report.
printf 'x\n' > "$outer/untracked"
git -C "$outer" config status.showUntrackedFiles no
got="$(build_rev "$outer")"
[ "$got" = "$head$DIRTY_SUFFIX" ] ||
    fail "a hidden untracked file: wanted $head$DIRTY_SUFFIX, got '$got'"
rm "$outer/untracked"
git -C "$outer" config --unset status.showUntrackedFiles

printf 'x\n' > "$outer/tracked"
got="$(build_rev "$outer")"
[ "$got" = "$head$DIRTY_SUFFIX" ] ||
    fail "a changed file: wanted $head$DIRTY_SUFFIX, got '$got'"
git -C "$outer" checkout -q -- tracked

# A directory that is not a repository has no revision. git answers for
# the repository above it, which names code that was never built.
inner="$outer/inner"
mkdir -p "$inner"
got="$(build_rev "$inner")"
[ -z "$got" ] ||
    fail "a directory that is not a repository reported '$got'"

for good in \
    3a40bd9 \
    3a40bd9f \
    "3a40bd9$DIRTY_SUFFIX" \
    0123456789abcdef0123456789abcdef01234567; do
    is_object_name "$good" || fail "rejected the object name '$good'"
done

for bad in \
    '' \
    3a40bd \
    3a40bd9g \
    main \
    HEAD \
    "3a40bd9$DIRTY_SUFFIX$DIRTY_SUFFIX" \
    '3a40bd9 ' \
    '3a40bd9;rm -rf /' \
    '3a40bd9
rm -rf /' \
    0123456789abcdef0123456789abcdef012345678; do
    ! is_object_name "$bad" || fail "accepted '$bad' as an object name"
done

echo "build-rev: PASSED"
