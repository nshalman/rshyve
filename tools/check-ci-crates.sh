#!/bin/sh
# Assert that CI still lints, tests and MSRV-checks every portable crate.
#
# The vmm-devices split moved roughly 19k lines into vmm-virtio,
# vmm-storage, vmm-hid, vmm-passthru and vmm-metrics. A crate that no
# `-p` list names does not fail the build. It becomes unlinted and
# untested code that CI reports green, so this is asserted instead of
# trusted.
#
# The crate list is derived from the workspace members, not kept by
# hand. A new library crate therefore fails this check until either CI
# names it or it is declared not portable below.
#
# Four jobs are checked, not three. The illumos cross-check is the only
# job that compiles a `#[cfg(test)]` body behind
# `cfg(target_os = "illumos")`, so a crate missing from ITS list is
# unchecked against the target with every other job green.
set -eu

CI=".github/workflows/ci.yml"
ROOT="Cargo.toml"

# Crates the portable jobs cannot cover. The bhyve, bhyve-sys and viona
# bindings are ioctl and struct-layout code whose only meaningful check
# is against the illumos target, which the cross-check job does. The two
# TPM crates run an autotools build of vendored libtpms and need an
# illumos host.
NOT_PORTABLE="bhyve_api bhyve_api_sys viona_api vmm_tpm vmm_tpm_sys"

[ -f "$CI" ] || { echo "run from the repository root: no $CI" >&2; exit 1; }

members=$(awk '
    /^members = \[/ { grab = 1; next }
    grab && /^\]/ { exit }
    grab {
        sub(/#.*/, "")
        gsub(/[",]/, "")
        gsub(/^[ \t]+|[ \t]+$/, "")
        if ($0 != "") print
    }
' "$ROOT")

[ -n "$members" ] || { echo "no [workspace] members found in $ROOT" >&2; exit 1; }

crates=""
for dir in $members; do
    # Binaries are illumos-only and are built on the build host.
    case "$dir" in
        bin/*) continue ;;
    esac
    manifest="$dir/Cargo.toml"
    if [ ! -f "$manifest" ]; then
        echo "$ROOT names $dir, which has no Cargo.toml" >&2
        exit 1
    fi
    name=$(sed -n 's/^name = "\(.*\)"/\1/p' "$manifest" | head -1)
    if [ -z "$name" ]; then
        echo "$manifest has no package name" >&2
        exit 1
    fi
    portable=yes
    for skip in $NOT_PORTABLE; do
        if [ "$name" = "$skip" ]; then
            portable=no
        fi
    done
    if [ "$portable" = yes ]; then
        crates="$crates $name"
    fi
done

status=0
for job in clippy test msrv cross; do
    # $MSRV is literal text in the workflow, not a shell expansion.
    # shellcheck disable=SC2016
    case "$job" in
        clippy) marker="cargo clippy --locked --all-targets" ;;
        test) marker="cargo test --locked" ;;
        msrv) marker='cargo "+$MSRV" check --locked' ;;
        cross) marker="cargo check --locked --all-targets" ;;
    esac
    # Crates this one job may leave out, with the reason recorded at
    # the job itself. Everything else must be named.
    case "$job" in
        # vmm_migrate pulls zstd-sys, which needs an illumos cross-cc
        # the runner does not have. The host clippy and test jobs cover
        # it, and its illumos paths are covered on the build host.
        cross) allowed="vmm_migrate" ;;
        *) allowed="" ;;
    esac
    # The command is a folded YAML scalar: the marker line, then the
    # continuation lines that carry the arguments. A --target line is
    # one of those; stopping at it would read an empty crate list and
    # report every crate missing.
    block=$(awk -v m="$marker" '
        index($0, m) { seen = 1; grab = 1; print; next }
        grab && $0 ~ /^[ \t]*(-p |--target )/ { print; next }
        grab { exit }
        END { if (!seen) exit 1 }
    ' "$CI") || {
        echo "$CI: no $job job line matching: $marker" >&2
        status=1
        continue
    }
    args=" $(echo "$block" | tr '\n' ' ') "
    case "$args" in
        *" -p "*) ;;
        *)
            echo "$CI: the $job job block carries no -p arguments" >&2
            status=1
            continue
            ;;
    esac
    for crate in $crates; do
        skip=no
        for ok in $allowed; do
            if [ "$crate" = "$ok" ]; then
                skip=yes
            fi
        done
        [ "$skip" = yes ] && continue
        case "$args" in
            *" -p $crate "*) ;;
            *)
                echo "$CI: the $job job does not name $crate" >&2
                status=1
                ;;
        esac
    done
done

if [ "$status" -eq 0 ]; then
    echo "check-ci-crates: all portable crates covered"
fi
exit "$status"
