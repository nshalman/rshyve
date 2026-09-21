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

set -euo pipefail

# No defaults: the build host is site-specific and must not be baked in.
HOST="${VMM_BUILD_HOST:?set VMM_BUILD_HOST to user@host of your illumos build host}"
KEY="${VMM_BUILD_KEY:?set VMM_BUILD_KEY to the SSH key for that host}"
REMOTE_DIR="${VMM_BUILD_DIR:-/opt/rust-bhyve}"

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

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The binary reports the revision it was built from, and that revision
# must be computed here: sync_sources excludes .git, so the build host
# holds no clone and git there answers for an unrelated repository or
# not at all. A value that does not name an object is dropped rather
# than sent, so the binary says `unstamped` instead of naming a commit
# that resolves nowhere.
build_rev() {
    local rev changes
    rev="$(git -C "$SRC_DIR" rev-parse --short HEAD 2>/dev/null)" || return 0
    # Untracked files reach the build host too, because rsync sends the
    # working tree and not a commit. A status that cannot be read counts
    # as changed: a bare commit name on a tree that may not match it is
    # the failure this path exists to prevent.
    if ! changes="$(git -C "$SRC_DIR" status --porcelain 2>/dev/null)" ||
        [ -n "$changes" ]; then
        rev="$rev-dirty"
    fi
    printf '%s' "$rev"
}

BUILD_REV="$(build_rev)"
if ! printf '%s' "$BUILD_REV" | grep -Eq '^[0-9a-f]{7,40}(-dirty)?$'; then
    echo "BUILD-WARN: no source revision here; the binary will report" \
         "unstamped" >&2
    BUILD_REV=""
fi

# Host key verification stays on. Add the build host to known_hosts once,
# after checking its fingerprint out of band.
SSH=(ssh -o ConnectTimeout=15 -i "$KEY" "$HOST")

# /opt/tools/bin and /opt/local/bin hold the pkgsrc toolchain (cargo, gcc,
# autoconf, gmake, pkg-config); none of them are on root's default PATH.
REMOTE_ENV="export PATH=/opt/tools/bin:/opt/local/bin:/usr/bin:/usr/sbin:\$PATH; cd $REMOTE_DIR"
# The value is spliced into a command a remote root shell parses, and it
# passed the object-name check above, so it holds no shell metacharacter.
if [ -n "$BUILD_REV" ]; then
    REMOTE_ENV="$REMOTE_ENV; export TRITON_BUILD_REV=$BUILD_REV"
fi

usage() { sed -n '3,27p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2; }

sync_sources() {
    # --checksum because rsync-over-ssh timestamps do not reliably invalidate
    # cargo's fingerprints; without it edited files silently fail to rebuild.
    rsync -az --checksum --delete \
        --exclude target --exclude .git --exclude 'kernel/linux-*' \
        -e "ssh -i $KEY" \
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
    rev)
        # What the next build stamps into the binary, before it runs.
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
