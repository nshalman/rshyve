#!/usr/bin/env bash
#
# Build and test rust-bhyve on the illumos build host.
#
# The rshyve binary only builds on illumos: vmm-tpm-sys drives an autotools
# build of vendored libtpms, bhyve-api binds SunOS ioctls, and viona-api
# needs libdladm. The library crates other than vmm-tpm-sys and vmm-tpm
# do build and test on macOS, which is enough for most unit work; the
# full workspace build and every platform test have to happen here.
# This script makes that one command so it can be driven from an editor,
# a test loop, or an agent.
#
#   tools/remote-build.sh build              # cargo build --release
#   tools/remote-build.sh test               # cargo test --workspace
#   tools/remote-build.sh test -p vmm_tpm    # extra args pass through
#   tools/remote-build.sh clippy
#   tools/remote-build.sh check
#   tools/remote-build.sh needed             # stock-PI dependency gate
#   tools/remote-build.sh sync               # push sources, build nothing
#   tools/remote-build.sh rev                # the revision a build stamps
#   tools/remote-build.sh run -- -H -c 2 ... # run the built rshyve
#   tools/remote-build.sh sh 'arbitrary command'
#
# Override the target with VMM_BUILD_HOST / VMM_BUILD_KEY / VMM_BUILD_DIR.
#
# A command that syncs holds a lock on the build directory, so two
# drivers cannot build each other's tree. VMM_BUILD_LOCK_WAIT sets how
# many seconds to wait for that lock.

set -euo pipefail

# No defaults: the build host is site-specific and must not be baked in.
HOST="${VMM_BUILD_HOST:?set VMM_BUILD_HOST to user@host of your illumos build host}"
KEY="${VMM_BUILD_KEY:?set VMM_BUILD_KEY to the SSH key for that host}"
# A trailing slash would put the lock below inside the synced tree,
# where --delete removes it.
REMOTE_DIR="${VMM_BUILD_DIR:-/opt/rust-bhyve}"
REMOTE_DIR="${REMOTE_DIR%/}"

# The value is spliced into a command that a remote root shell parses, so
# it is constrained to an absolute path made of path characters. A space,
# a quote or a `$(...)` would otherwise run as root on the node.
case "$REMOTE_DIR" in
    /*) ;;
    *) echo "BUILD-FAIL: VMM_BUILD_DIR must be an absolute path" >&2; exit 1 ;;
esac
if printf '%s' "$REMOTE_DIR" | grep -qv '^[A-Za-z0-9_./-]*$'; then
    echo "BUILD-FAIL: VMM_BUILD_DIR may hold only letters, digits, _ . / -" >&2
    exit 1
fi
PROFILE="${VMM_BUILD_PROFILE:---release}"

# git reports the real path, so the comparison in build_rev needs one.
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

# shellcheck source=tools/build-rev.sh
. "$SRC_DIR/tools/build-rev.sh"

# Host key verification stays on. Add the build host to known_hosts once,
# after checking its fingerprint out of band.
SSH=(ssh -o ConnectTimeout=15 -i "$KEY" "$HOST")

# /opt/tools/bin and /opt/local/bin hold the pkgsrc toolchain (cargo, gcc,
# autoconf, gmake, pkg-config); none of them are on root's default PATH.
REMOTE_BASE="export PATH=/opt/tools/bin:/opt/local/bin:/usr/bin:/usr/sbin:\$PATH; cd $REMOTE_DIR"

# The binary reports the revision it was built from, and that revision
# must be computed here: sync_sources excludes .git, so the build host
# holds no clone. A value that does not name an object is dropped
# rather than sent, so the binary says `unstamped` instead of naming a
# commit that resolves nowhere.
stamp_rev() {
    BUILD_REV="$(build_rev "$SRC_DIR")"
    if ! is_object_name "$BUILD_REV"; then
        echo "BUILD-WARN: no source revision here; the binary will report" \
             "unstamped" >&2
        BUILD_REV=""
    fi
    REMOTE_ENV="$REMOTE_BASE"
    # The value is spliced into a command a remote root shell parses. It
    # passed the object-name check above, so it carries no quote, and the
    # quotes here hold whatever a later caller sends to one word.
    if [ -n "$BUILD_REV" ]; then
        REMOTE_ENV="$REMOTE_ENV; export TRITON_BUILD_REV='$BUILD_REV'"
    fi
}

# The help text is the header comment. A line range drifts when the
# header changes, and it did: the old one printed `set -euo pipefail`.
usage() {
    sed -n -e '1,2d' -e '/^#/!q' -e 's/^# \{0,1\}//p' "${BASH_SOURCE[0]}"
    exit 2
}

# Two drivers that share $REMOTE_DIR interleave: one syncs, the other
# syncs over it, and the first then builds the second's tree and stamps
# its own revision on it. The binary would name a commit it was not
# built from, which is the failure the stamp exists to end. The lock
# holds from the sync to the end of the command.
LOCK_DIR="$REMOTE_DIR.lock"
LOCK_WAIT="${VMM_BUILD_LOCK_WAIT:-600}"
case "$LOCK_WAIT" in
    '' | *[!0-9]*)
        echo "BUILD-FAIL: VMM_BUILD_LOCK_WAIT must be seconds" >&2; exit 1 ;;
esac
lock_held=""

release_lock() {
    [ -n "$lock_held" ] || return 0
    lock_held=""
    "${SSH[@]}" "rmdir $LOCK_DIR" >/dev/null 2>&1 || true
}

take_lock() {
    local waited=0
    until "${SSH[@]}" "mkdir $LOCK_DIR" >/dev/null 2>&1; do
        if [ "$waited" -ge "$LOCK_WAIT" ]; then
            echo "BUILD-FAIL: $HOST:$LOCK_DIR is held. Another build has" \
                 "it, or one died; remove that directory to clear it." >&2
            exit 1
        fi
        if [ "$waited" -eq 0 ]; then
            echo "waiting for $HOST:$LOCK_DIR" >&2
        fi
        sleep 10
        waited=$((waited + 10))
    done
    lock_held=1
    trap release_lock EXIT
    trap 'release_lock; exit 130' INT TERM
}

sync_sources() {
    take_lock
    # Read under the lock, and again after the copy: a checkout that
    # moved while this driver waited or copied would otherwise put one
    # commit's name on another commit's tree.
    stamp_rev
    local synced_rev="$BUILD_REV"
    # --checksum because rsync-over-ssh timestamps do not reliably invalidate
    # cargo's fingerprints; without it edited files silently fail to rebuild.
    rsync -az --checksum --delete \
        --exclude target --exclude .git --exclude 'kernel/linux-*' \
        -e "ssh -i $KEY" \
        "$SRC_DIR/" "$HOST:$REMOTE_DIR/"
    stamp_rev
    if [ "$BUILD_REV" != "$synced_rev" ]; then
        echo "BUILD-FAIL: the checkout changed during the sync; run it again" >&2
        exit 1
    fi
}

remote() { "${SSH[@]}" "$REMOTE_ENV; $*"; }

cmd="${1:-build}"
shift || true

case "$cmd" in
    sync)
        sync_sources
        echo "synced $SRC_DIR -> $HOST:$REMOTE_DIR"
        ;;
    rev)
        # What the next build stamps into the binary, before it runs.
        stamp_rev
        echo "${BUILD_REV:-unstamped}"
        ;;
    build)
        sync_sources
        remote "cargo build $PROFILE $*" 2>&1
        # A binary that needs a library the platform image does not
        # ship links fine here and then fails to exec on a node, so
        # gate every build rather than only the packaging step.
        remote "set -- ; for b in target/release/rshyve target/release/firehyve; do \
                    if [ -f \$b ]; then set -- \"\$@\" \$b; fi; \
                done; \
                if [ \$# -gt 0 ]; then tools/check-needed.sh \"\$@\"; \
                else echo 'no release binary to check'; fi" 2>&1
        ;;
    check|clippy)
        sync_sources
        remote "cargo $cmd $PROFILE $*" 2>&1
        ;;
    needed)
        sync_sources
        remote "tools/check-needed.sh ${*:-target/release/rshyve target/release/firehyve}" 2>&1
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
        remote "./target/release/rshyve $*" 2>&1
        ;;
    sh)
        # No sync and no lock, so the tree on the host is whatever the
        # last caller left. A stamp from this driver's HEAD would name
        # code that may not be there; leave it unstamped instead.
        "${SSH[@]}" "$REMOTE_BASE; $*" 2>&1
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        echo "unknown command: $cmd" >&2
        usage
        ;;
esac
