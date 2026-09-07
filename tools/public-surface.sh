#!/usr/bin/env bash
#
# Public-surface guard.
#
# Fails if a file in this tree carries private infrastructure detail, a
# workstation-specific path, a personal identifier, or an unexpectedly
# large binary. This is a regression guard for the scrub done before
# publication, not a secret scanner: gitleaks covers credentials.
#
#   tools/public-surface.sh
#
# Exit status: 0 clean, 1 a rule found something, 2 the check could not
# run.
#
# WHY the third status, and why every command is checked: a scan that
# cannot list or read the files must never report clean. This script
# read a failure of `git grep` as "no hits", so outside a checkout it
# printed "fatal: not a git repository" and then said clean. That is
# what happened on the illumos build host, where the tree is an
# extracted tarball. A guard that stops private data reaching a public
# repository has no worse failure mode than a silent pass.
#
# So the file list is built once, from the git index in a checkout and
# from the filesystem otherwise, and the tarball on the build host is
# scanned like any other tree. One scanner reads that list. The mode and
# the file count are printed, and the list must hold the sentinel files
# below, so a run that read the wrong directory cannot look like a clean
# one.
#
# Every exception below is deliberate and carries a reason. Add one only
# when the occurrence is genuinely correct, and say why.

set -o errexit
set -o nounset
set -o pipefail

die() {
    echo "public-surface: cannot run: $*" >&2
    exit 2
}

cd "$(dirname "${BASH_SOURCE[0]}")/.." || die "no tree above ${BASH_SOURCE[0]}"

status=0

# Files that every copy of this tree holds. If the listing misses one,
# it read the wrong directory or read nothing.
SENTINELS=(Cargo.toml tools/public-surface.sh)

# Directories the filesystem walk skips. Build output is not part of the
# published tree and dwarfs it. A prune is a blind spot, so the list is
# short and the run prints it.
PRUNE_DIRS=(.git target node_modules)

# A large tracked binary is almost always an accident, and one of them
# (a GPL-2.0 kernel image whose source is not tracked) is why the rule
# exists. Cargo.lock is text and legitimately large.
MAX_BYTES=1048576

# Paths excluded from every rule: vendored upstream C, which is not ours
# to rewrite, and this script, which necessarily holds the patterns.
BASE_EXCLUDES=(
    'third_party/*'
    'tools/public-surface.sh'
)

# grep in batches. The tree is small, but an unpruned directory in
# filesystem mode could push a single argument list past ARG_MAX, and
# that would fail the run rather than scan it.
BATCH=400

# excluded <path> <pattern>...
excluded() {
    local path="$1" pattern
    shift
    for pattern in "$@"; do
        # Unquoted on the right: these are globs, and `*` spans `/`, so
        # `third_party/*` covers the whole subtree.
        # shellcheck disable=SC2053
        if [[ "$path" == $pattern ]]; then
            return 0
        fi
    done
    return 1
}

# listed <wanted> <path>... An exact match, not a glob: a sentinel is
# one known name and nothing else may stand in for it.
listed() {
    local want="$1" path
    shift
    for path in "$@"; do
        if [[ "$path" == "$want" ]]; then
            return 0
        fi
    done
    return 1
}

work="$(mktemp -d)" || die "mktemp failed"
trap 'rm -rf "$work"' EXIT

listing="$work/listing"
mode=""

# A checkout answers with its own root. A tarball unpacked inside some
# other repository answers with that repository, whose index does not
# hold these files, so `-ef` decides it rather than the exit status.
if top="$(git rev-parse --show-toplevel 2>/dev/null)" && [[ -n "$top" && "$top" -ef . ]]; then
    git ls-files -z > "$listing" || die "git ls-files failed in a checkout at $top"
    mode="the git index"
else
    prune=()
    for dir in "${PRUNE_DIRS[@]}"; do
        if (( ${#prune[@]} > 0 )); then
            prune+=(-o)
        fi
        prune+=(-name "$dir")
    done
    # Regular files only, and symbolic links are not followed, so
    # nothing here reads a device or a fifo.
    find . \( "${prune[@]}" \) -prune -o -type f -print0 > "$listing" \
        || die "find failed"
    mode="the filesystem"
    echo "public-surface: no checkout here, walking ${PWD} and skipping: ${PRUNE_DIRS[*]}" >&2
fi

all=()
while IFS= read -r -d '' path; do
    all+=("${path#./}")
done < "$listing"
(( ${#all[@]} > 0 )) || die "$mode listed no files"

for want in "${SENTINELS[@]}"; do
    if ! listed "$want" "${all[@]}"; then
        die "$mode holds no $want, so it did not list this tree"
    fi
done

# A tracked file deleted from the working tree, and a submodule, are
# both listed and neither can be read. Say so: a file dropped without a
# word is how a scan quietly stops covering something.
base=()
for path in "${all[@]}"; do
    if excluded "$path" "${BASE_EXCLUDES[@]}"; then
        continue
    fi
    if [[ -f "$path" && -r "$path" ]]; then
        base+=("$path")
    else
        echo "public-surface: not a readable file, skipped: $path" >&2
    fi
done
(( ${#base[@]} > 0 )) || die "every listed file was excluded or unreadable"

# report <rule> <pattern> [extra-path-exception ...]
report() {
    local rule="$1" pattern="$2"
    shift 2
    local path
    local -a scan=()
    for path in "${base[@]}"; do
        if ! excluded "$path" "$@"; then
            scan+=("$path")
        fi
    done
    (( ${#scan[@]} > 0 )) || die "the exceptions for '$rule' leave nothing to scan"

    local hits="$work/hits"
    : > "$hits"
    local -i first=0 rc=0 worst=0
    while (( first < ${#scan[@]} )); do
        rc=0
        # /dev/null keeps the file name in the output when a batch holds
        # one file. grep answers 0 for a hit, 1 for none, and more than
        # 1 for a file it could not read or an option it does not have.
        grep -nIE -e "$pattern" -- "${scan[@]:first:BATCH}" /dev/null >> "$hits" || rc=$?
        if (( rc > worst )); then
            worst=$rc
        fi
        first=$(( first + BATCH ))
    done
    if (( worst > 1 )); then
        die "grep answered $worst while checking '$rule'"
    fi
    if [[ -s "$hits" ]]; then
        echo "FAIL: $rule" >&2
        sed 's/^/    /' "$hits" >&2
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

large="$work/large"
: > "$large"
for path in "${base[@]}"; do
    if [[ "$path" == Cargo.lock ]]; then
        continue
    fi
    size="$(wc -c < "$path")" || die "cannot read the size of $path"
    size="${size//[[:space:]]/}"
    if (( size > MAX_BYTES )); then
        printf '%s (%s bytes)\n' "$path" "$size" >> "$large"
    fi
done
if [[ -s "$large" ]]; then
    echo "FAIL: file larger than $MAX_BYTES bytes" >&2
    sed 's/^/    /' "$large" >&2
    status=1
fi

if (( status == 0 )); then
    printf 'public-surface: clean (%d files from %s)\n' "${#base[@]}" "$mode"
fi
exit "$status"
