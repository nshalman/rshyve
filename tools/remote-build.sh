#!/usr/bin/env bash
#
# Build and test vmmnew on the illumos build host.
#
# The workspace only compiles on illumos: vmm-tpm-sys drives an autotools
# build of vendored libtpms, bhyve-api binds SunOS ioctls, and viona-api
# needs libdladm. Developing on macOS therefore means every build is a
# remote build. This script makes that one command so it can be driven
# from an editor, a test loop, or an agent.
#
#   tools/remote-build.sh build              # cargo build --release
#   tools/remote-build.sh test               # cargo test --workspace
#   tools/remote-build.sh test -p vmm_tpm    # extra args pass through
#   tools/remote-build.sh clippy
#   tools/remote-build.sh check
#   tools/remote-build.sh sync               # push sources, build nothing
#   tools/remote-build.sh run -- -H -c 2 ... # run the built vmm
#   tools/remote-build.sh sh 'arbitrary command'
#
# Override the target with VMM_BUILD_HOST / VMM_BUILD_KEY / VMM_BUILD_DIR.

set -euo pipefail

HOST="${VMM_BUILD_HOST:-root@$VMM_BUILD_HOST}"
KEY="${VMM_BUILD_KEY:-$HOME/.ssh/$VMM_BUILD_KEY}"
REMOTE_DIR="${VMM_BUILD_DIR:-/opt/vmmnew}"
PROFILE="${VMM_BUILD_PROFILE:---release}"

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

SSH=(ssh -o StrictHostKeyChecking=no -o ConnectTimeout=15 -i "$KEY" "$HOST")

# /opt/tools/bin and /opt/local/bin hold the pkgsrc toolchain (cargo, gcc,
# autoconf, gmake, pkg-config); none of them are on root's default PATH.
REMOTE_ENV="export PATH=/opt/tools/bin:/opt/local/bin:/usr/bin:/usr/sbin:\$PATH; cd $REMOTE_DIR"

usage() { sed -n '3,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2; }

sync_sources() {
    # --checksum because rsync-over-ssh timestamps do not reliably invalidate
    # cargo's fingerprints; without it edited files silently fail to rebuild.
    rsync -az --checksum --delete \
        --exclude target --exclude .git --exclude 'kernel/linux-*' \
        -e "ssh -o StrictHostKeyChecking=no -i $KEY" \
        "$SRC_DIR/" "$HOST:$REMOTE_DIR/"
}

remote() { "${SSH[@]}" "$REMOTE_ENV; $*"; }

cmd="${1:-build}"
shift || true

case "$cmd" in
    sync)
        sync_sources
        echo "synced $SRC_DIR -> $HOST:$REMOTE_DIR"
        ;;
    build|check|clippy)
        sync_sources
        remote "cargo $cmd $PROFILE $*" 2>&1
        ;;
    test)
        sync_sources
        # Tests run in the dev profile: release strips the debug assertions
        # several device models rely on to catch ring-index mistakes.
        remote "cargo test --workspace $*" 2>&1
        ;;
    fmt)
        sync_sources
        remote "cargo fmt --all $*" 2>&1
        ;;
    run)
        sync_sources
        remote "cargo build $PROFILE" 2>&1
        remote "./target/release/vmm $*" 2>&1
        ;;
    sh)
        remote "$*" 2>&1
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        echo "unknown command: $cmd" >&2
        usage
        ;;
esac
