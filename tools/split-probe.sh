#!/bin/sh
# Validate the vmm-devices split before any real commit lands.
#
# Applies tools/split-rewrite.py to a throwaway worktree and requires a
# clean build of the six library crates the split leaves behind. Tasks
# that apply one stage each to the real tree need that to hold first, so
# the probe measures the same rewrite those commits apply.
#
# The split of vmm-devices clears its own blockers before this point, so
# the bar is zero errors, not a known residual set.
#
# Only pure library crates are compiled. bin/rshyve and bin/firehyve are
# illumos-only, so the probe asserts on their sources by grep and the
# real split commits build them on the illumos build host.
set -eu

REPO=$(git rev-parse --show-toplevel)
BASE=HEAD

while [ $# -gt 0 ]; do
    case "$1" in
        --base)
            [ $# -ge 2 ] || {
                echo "split-probe.sh: --base needs a ref" >&2
                exit 2
            }
            BASE="$2"
            shift 2
            ;;
        *) echo "usage: split-probe.sh [--base <ref>]" >&2; exit 2 ;;
    esac
done

git -C "$REPO" rev-parse --verify --quiet "$BASE^{commit}" >/dev/null || {
    echo "split-probe.sh: '$BASE' is not a commit" >&2
    exit 2
}

TMP=$(mktemp -d "${TMPDIR:-/tmp}/split-probe.XXXXXX")
WT="$TMP/tree"
trap 'git -C "$REPO" worktree remove --force "$WT" >/dev/null 2>&1 || true
      rm -rf "$TMP"' EXIT

git -C "$REPO" worktree add --detach --quiet "$WT" "$BASE"
python3 "$REPO/tools/split-rewrite.py" --root "$WT" --stage all

# --all-targets matters: the moved #[cfg(test)] modules carry most of
# the crate:: paths that cross the new crate boundaries.
rc=0
(
    cd "$WT" && cargo check --all-targets --message-format=short \
        -p vmm_devices -p vmm_metrics -p vmm_passthru \
        -p vmm_virtio -p vmm_storage -p vmm_hid
) >"$TMP/probe.out" 2>&1 || rc=$?

if [ "$rc" -ne 0 ]; then
    echo "split-probe: FAILED, the split does not build clean" >&2
    cat "$TMP/probe.out" >&2
    exit 1
fi

# A binary that still reaches a moved module through vmm_devices does
# not fail the check above, because the probe cannot compile either
# binary on this host.
if grep -rnE 'vmm_devices::(virtio|nvme|ahci|fbuf|xhci|ps2|passthru)::' \
    "$WT/bin" >"$TMP/stale" 2>/dev/null; then
    echo "split-probe: FAILED, a binary still reaches a moved module" >&2
    cat "$TMP/stale" >&2
    exit 1
fi

echo "split-probe: the split builds clean and no stale binary path remains"
