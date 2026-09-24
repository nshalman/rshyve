#!/usr/bin/env bash
#
# Assert that no build script takes the source revision from git.
#
# The firehyve in a platform image named a commit that resolves in no
# clone, because its build script ran git on a host that holds no
# repository. The rule now lives in one file that both build scripts
# include, so all three are checked: a gate that reads the wrappers
# only is blind to the file that holds the rule. The match is textual,
# so a comment naming one of these also fails. The gate stays closed
# rather than learning to parse Rust.
#
# Exit status: 0 clean, 1 a file reads git again or cannot be read.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." ||
    { echo "no tree above ${BASH_SOURCE[0]}" >&2; exit 1; }

FILES="
crates/vmm-machine/src/build_rev.rs
bin/firehyve/build.rs
bin/rshyve/build.rs
"

status=0
for f in $FILES; do
    if [ ! -f "$f" ]; then
        echo "$f is gone: the stamp rule moved and this gate cannot see it" >&2
        status=1
        continue
    fi
    # grep exits 2 on a file it cannot read, and `if grep` reads that as
    # "no match", so the status is separated from the answer.
    hits=0
    out="$(grep -nE 'rev-parse|process::Command|Command::new' "$f")" || hits=$?
    case "$hits" in
        0)
            printf '%s\n' "$out" >&2
            echo "$f reads the revision from git again" >&2
            status=1
            ;;
        1) ;;
        *)
            echo "$f could not be read" >&2
            status=1
            ;;
    esac
done
exit "$status"
