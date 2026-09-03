#!/usr/bin/env bash
#
# Public-surface guard.
#
# Fails if a tracked file carries private infrastructure detail, a
# workstation-specific path, a personal identifier, or an unexpectedly
# large binary. This is a regression guard for the scrub done before
# publication, not a secret scanner: gitleaks covers credentials.
#
#   tools/public-surface.sh
#
# Every exception below is deliberate and carries a reason. Add one only
# when the occurrence is genuinely correct, and say why.

set -o errexit
set -o nounset
set -o pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

status=0

# Paths excluded from every rule: vendored upstream C, which is not ours
# to rewrite, and this script, which necessarily holds the patterns.
BASE_EXCLUDES=(
    ':!third_party/*'
    ':!tools/public-surface.sh'
)

# report <rule> <pattern> [extra-path-exception ...]
report() {
    local rule="$1" pattern="$2"
    shift 2
    local -a excludes=("${BASE_EXCLUDES[@]}")
    local f
    for f in "$@"; do excludes+=(":!$f"); done

    local hits
    hits="$(git grep -nIE "$pattern" -- . "${excludes[@]}" || true)"
    if [[ -n "$hits" ]]; then
        echo "FAIL: $rule" >&2
        echo "$hits" | sed 's/^/    /' >&2
        status=1
    fi
}

# The lab's own ranges, not private addresses in general: 10.0.0.x is a
# conventional placeholder in the CLI and metadata examples and carries
# no information about anyone's network. Anchoring on a full dotted quad
# keeps this from matching specification section numbers in comments.
report "lab network address" \
    '(^|[^0-9.])(10\.199\.199\.[0-9]{1,3}|192\.168\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3})([^0-9.]|$)'

report "named lab or build host" \
    '142\.147\.4\.194|us-central\.manta\.mnx\.io'

report "personal or workstation-specific path" \
    '/Users/[a-z]|~/workspace|sdc\.id_rsa|id_rsa-decrypt'

report "personal identifier" \
    '[A-Za-z0-9._%+-]+@(mnxsolutions|joyent)\.com'

# docs/testing.md: the test guest is rebuilt from a ZFS snapshot on every
#   run, so its host key legitimately changes each boot. Explained inline.
# vmm-migrate-agent: tracked as OSF-1b. Drop this exception when it closes.
# open-findings.md: describes the defect above; it does not practise it.
report "SSH host key verification disabled" \
    'StrictHostKeyChecking=no|UserKnownHostsFile=/dev/null' \
    'docs/testing.md' \
    'bin/vmm-migrate-agent/src/main.rs' \
    'docs/security/open-findings.md'

# A large tracked binary is almost always an accident, and one of them
# (a GPL-2.0 kernel image whose source is not tracked) is why this rule
# exists. Cargo.lock is text and legitimately large.
large="$(git ls-files -- . "${BASE_EXCLUDES[@]}" ':!Cargo.lock' | while read -r f; do
    [[ -f "$f" ]] || continue
    size=$(wc -c < "$f")
    (( size > 1048576 )) && printf '%s (%s bytes)\n' "$f" "$size"
done || true)"
if [[ -n "$large" ]]; then
    echo "FAIL: tracked file larger than 1 MiB" >&2
    echo "$large" | sed 's/^/    /' >&2
    status=1
fi

if (( status == 0 )); then
    echo "public-surface: clean"
fi
exit "$status"
