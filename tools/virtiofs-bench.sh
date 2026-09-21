#!/usr/bin/env bash
#
# Compare guest kernels on boot time and virtio-fs I/O time.
#
#   virtiofs-bench.sh <runs> <kernel> [kernel...]
#
# Runs the read-write check set once per iteration per kernel and reads
# two numbers out of the guest's own printk timestamps:
#
#   boot_ms  kernel entry to "Run /init as init process"
#   io_ms    that point to power-off, which is the 19 checks, and is
#            dominated by the 4 MiB read and the 1 MiB write
#
# Guest timestamps are used rather than host ones so the figure excludes
# VMM start-up and the host's own scheduling noise.

set -u
set -o pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
RUNS="${1:-5}"
shift || true
KERNELS=("$@")
if [ "${#KERNELS[@]}" -eq 0 ]; then
    echo "usage: $0 <runs> <kernel> [kernel...]"
    exit 2
fi

# printk timestamps look like "[    0.255689] ". Parsed with awk, not
# sed: the patterns searched for contain slashes.
stamp_ms() {
    awk -v pat="$2" '
        index($0, pat) > 0 && match($0, /^\[ *[0-9]+\.[0-9]+\]/) {
            t = substr($0, RSTART + 1, RLENGTH - 2)
            gsub(/ /, "", t)
            printf "%.1f", t * 1000
            exit
        }' "$1"
}

for kernel in "${KERNELS[@]}"; do
    if [ ! -f "$kernel" ]; then
        echo "BENCH-FAIL: no kernel at $kernel"
        exit 1
    fi
    echo "=== $(basename "$kernel") ==="
    boots=""
    ios=""
    for i in $(seq 1 "$RUNS"); do
        VMM_FIREHYVE_KERNEL="$kernel" "$DIR/virtiofs-run.sh" rw > /dev/null 2>&1
        log=$DIR/run-rw.log
        boot=$(stamp_ms "$log" "Run /init as init process")
        if [ -z "$boot" ]; then
            echo "  run $i: did not reach init, see $log"
            continue
        fi
        boots="$boots $boot"
        # A kernel without FUSE still boots, so its boot time is
        # comparable even though it cannot run the checks.
        if grep -q "result=OK" "$log" 2>/dev/null; then
            end=$(stamp_ms "$log" "reboot: Power down")
            io=$(awk -v a="$end" -v b="$boot" 'BEGIN { printf "%.1f", a - b }')
            ios="$ios $io"
            echo "  run $i: boot=${boot}ms io=${io}ms"
        else
            echo "  run $i: boot=${boot}ms io=n/a (checks did not pass)"
        fi
    done
    summarize() {
        echo "$2" | awk -v label="$1" '
            NF == 0 { printf "  %s: no samples\n", label; exit }
            { m = $1; for (i = 1; i <= NF; i++) { s += $i; if ($i < m) m = $i }
              printf "  %s: min=%.1fms mean=%.1fms over %d runs\n",
                     label, m, s / NF, NF }'
    }
    summarize "boot" "$boots"
    summarize "io  " "$ios"
done
