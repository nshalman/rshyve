#!/usr/bin/bash
#
# vmm-migrate.sh — Live migrate a bhyve VM between SmartOS hosts.
#
# Orchestrates ZFS disk replication + RAM/state migration:
#   1. Initial ZFS snapshot + full send
#   2. Iterative ZFS incremental sends (while VM runs)
#   3. Start destination VMM with --migrate-listen
#   4. Final ZFS incremental send (minimal delta)
#   5. Trigger live RAM migration (pause + dirty pages + device state)
#   6. VM resumes on destination
#
# Usage:
#   vmm-migrate.sh \
#     --source-host <ip> \
#     --dest-host <ip> \
#     --source-dataset <zfs-dataset> \
#     --control-socket <path> \
#     --vmm-args '<args for destination VMM>' \
#     [--ssh-key <path>] \
#     [--migrate-port <port>] \
#     [--max-syncs <N>]
#
# Example:
#   vmm-migrate.sh \
#     --source-host source.example \
#     --dest-host dest.example \
#     --source-dataset zones/vmm-test-disk \
#     --control-socket /tmp/vmm-testvm.sock \
#     --vmm-args '-H -c 2 -m 1G -s 0,hostbridge -s 4,virtio-blk,/dev/zvol/dsk/zones/vmm-test-disk -s 6,virtio-net-viona,vmm_test0 -s 31,lpc -l bootrom,/usr/share/bhyve/uefi-rom.bin' \
#     --ssh-key ~/.ssh/id_lab

set -o errexit
set -o pipefail

# Defaults
SSH_KEY="${VMM_LAB_KEY:-}"   # required; pass --ssh-key or set VMM_LAB_KEY
MIGRATE_PORT=4567
MAX_SYNCS=3
VMM_BIN_SRC="/opt/rust-bhyve/target/release/rshyve"
VMM_BIN_DST="/opt/vmm-new"
SNAP_PREFIX="vmm-mig"

# Parse args
while [[ $# -gt 0 ]]; do
    case $1 in
        --source-host)   SRC_HOST="$2"; shift 2 ;;
        --dest-host)     DST_HOST="$2"; shift 2 ;;
        --source-dataset) SRC_DATASET="$2"; shift 2 ;;
        --control-socket) CTL_SOCK="$2"; shift 2 ;;
        --vmm-args)      VMM_ARGS="$2"; shift 2 ;;
        --ssh-key)       SSH_KEY="$2"; shift 2 ;;
        --migrate-port)  MIGRATE_PORT="$2"; shift 2 ;;
        --max-syncs)     MAX_SYNCS="$2"; shift 2 ;;
        --vmm-bin-src)   VMM_BIN_SRC="$2"; shift 2 ;;
        --vmm-bin-dst)   VMM_BIN_DST="$2"; shift 2 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

# Validate required args
for var in SRC_HOST DST_HOST SRC_DATASET CTL_SOCK VMM_ARGS SSH_KEY; do
    if [[ -z "${!var}" ]]; then
        echo "Missing required argument: --$(echo $var | tr '_' '-' | tr 'A-Z' 'a-z')" >&2
        exit 1
    fi
done

# Host key verification stays on: this script pipes zfs send streams of
# guest disks to a remote root shell. Add both hosts to known_hosts first.
SSH="ssh -i ${SSH_KEY} -o ConnectTimeout=5"
DST_DATASET="${SRC_DATASET}"

log() { echo "[$(date +%T)] $*"; }
die() { log "FATAL: $*" >&2; exit 1; }

# ── Phase 1: Initial ZFS snapshot + full send ─────────────────────

log "Phase 1: Initial ZFS full send"

SNAP_BASE="${SNAP_PREFIX}-base"
log "  Creating snapshot ${SRC_DATASET}@${SNAP_BASE} on source..."
$SSH root@${SRC_HOST} "zfs snapshot ${SRC_DATASET}@${SNAP_BASE}" || \
    $SSH root@${SRC_HOST} "zfs destroy ${SRC_DATASET}@${SNAP_BASE} && zfs snapshot ${SRC_DATASET}@${SNAP_BASE}"

log "  Destroying destination dataset if exists..."
$SSH root@${DST_HOST} "zfs destroy -r ${DST_DATASET} 2>/dev/null; true"

log "  Sending ${SRC_DATASET}@${SNAP_BASE} → ${DST_HOST}:${DST_DATASET}..."
SEND_START=$(date +%s)
$SSH root@${SRC_HOST} "zfs send ${SRC_DATASET}@${SNAP_BASE}" | \
    $SSH root@${DST_HOST} "zfs recv -F ${DST_DATASET}"
SEND_END=$(date +%s)
SEND_SIZE=$($SSH root@${SRC_HOST} "zfs get -Hp -o value used ${SRC_DATASET}@${SNAP_BASE}")
log "  Full send complete: $(( SEND_SIZE / 1024 / 1024 )) MiB in $(( SEND_END - SEND_START ))s"

LAST_SNAP="${SNAP_BASE}"

# ── Phase 2: Iterative ZFS incremental sends ──────────────────────

for iter in $(seq 1 ${MAX_SYNCS}); do
    log "Phase 2: Incremental sync iteration ${iter}/${MAX_SYNCS}"

    SNAP_ITER="${SNAP_PREFIX}-iter-${iter}"
    $SSH root@${SRC_HOST} "zfs snapshot ${SRC_DATASET}@${SNAP_ITER}"

    INCR_START=$(date +%s)
    $SSH root@${SRC_HOST} \
        "zfs send -i ${SRC_DATASET}@${LAST_SNAP} ${SRC_DATASET}@${SNAP_ITER}" | \
        $SSH root@${DST_HOST} "zfs recv -F ${DST_DATASET}"
    INCR_END=$(date +%s)

    log "  Incremental send complete in $(( INCR_END - INCR_START ))s"

    # Destroy old snapshot on source (keep latest)
    $SSH root@${SRC_HOST} "zfs destroy ${SRC_DATASET}@${LAST_SNAP}" 2>/dev/null || true
    LAST_SNAP="${SNAP_ITER}"

    sleep 1
done

# ── Phase 3: Final ZFS incremental (before starting dest VMM) ─────
# Must happen before dest VMM opens the zvol.

log "Phase 3: Final ZFS incremental send"

SNAP_FINAL="${SNAP_PREFIX}-final"
$SSH root@${SRC_HOST} "zfs snapshot ${SRC_DATASET}@${SNAP_FINAL}"

FINAL_START=$(date +%s)
$SSH root@${SRC_HOST} \
    "zfs send -i ${SRC_DATASET}@${LAST_SNAP} ${SRC_DATASET}@${SNAP_FINAL}" | \
    $SSH root@${DST_HOST} "zfs recv -F ${DST_DATASET}"
FINAL_END=$(date +%s)
log "  Final incremental: $(( FINAL_END - FINAL_START ))s"

$SSH root@${SRC_HOST} "zfs destroy ${SRC_DATASET}@${LAST_SNAP}" 2>/dev/null || true

# ── Phase 4: Start destination VMM ────────────────────────────────

log "Phase 4: Starting destination VMM on ${DST_HOST}"

$SSH root@${DST_HOST} "pkill -9 rshyve 2>/dev/null; /usr/sbin/bhyvectl --vm=testvm --destroy 2>/dev/null; true"
sleep 2

$SSH root@${DST_HOST} "nohup ${VMM_BIN_DST} ${VMM_ARGS} \
    --migrate-listen 0.0.0.0:${MIGRATE_PORT} \
    testvm >/tmp/vmm-dest.log 2>&1 &"
sleep 2

DEST_READY=$($SSH root@${DST_HOST} "tail -1 /tmp/vmm-dest.log")
log "  Destination: ${DEST_READY}"
echo "${DEST_READY}" | grep -q "waiting for migration" || die "Destination not ready"

# ── Phase 5: Trigger live RAM migration ───────────────────────────

log "Phase 5: Triggering live RAM migration"
MIG_START=$(date +%s)

RESULT=$($SSH root@${SRC_HOST} \
    "echo '{\"command\":\"migrate-source\",\"target_addr\":\"${DST_HOST}:${MIGRATE_PORT}\"}' | \
     socat - UNIX-CONNECT:${CTL_SOCK}")

echo "${RESULT}" | grep -q '"migrating"' || die "Failed to trigger migration: ${RESULT}"
log "  Migration triggered: ${RESULT}"

# Monitor progress
PREV_PHASE=""
while true; do
    STAT=$($SSH root@${SRC_HOST} \
        "echo '{\"command\":\"migrate-status\"}' | \
         socat - UNIX-CONNECT:${CTL_SOCK} 2>/dev/null" 2>/dev/null || echo '{"migrate_phase":"done"}')

    PHASE=$(echo "${STAT}" | python3 -c "
import sys,json
d=json.load(sys.stdin)
p=d.get('migrate_phase','?')
b=d.get('migrate_bytes',0)
print(f'{p} ({b // 1048576} MiB)')
" 2>/dev/null || echo "done")

    if [[ "${PHASE}" != "${PREV_PHASE}" ]]; then
        log "  ${PHASE}"
        PREV_PHASE="${PHASE}"
    fi

    echo "${STAT}" | grep -qE '"finish"|"stopped"|"done"' && break
    sleep 1
done

MIG_END=$(date +%s)

# ── Phase 6: Cleanup ──────────────────────────────────────────────

log "Phase 6: Cleanup"

# Clean up migration snapshots on source
$SSH root@${SRC_HOST} "zfs destroy ${SRC_DATASET}@${SNAP_FINAL}" 2>/dev/null || true

log ""
log "=== MIGRATION COMPLETE ==="
log "  RAM migration: $(( MIG_END - MIG_START ))s"
log "  Destination VMM running on ${DST_HOST}"
log ""
log "  Verify: ssh -i ${SSH_KEY} root@${DST_HOST} 'tail -5 /tmp/vmm-dest.log'"
log "  Cleanup source: ssh -i ${SSH_KEY} root@${SRC_HOST} 'pkill -9 rshyve; /usr/sbin/bhyvectl --vm=testvm --destroy'"
