#!/bin/bash
#
# scan-vulns.sh — CTF vulnerability scanner
# Finds source files and runs Claude Code on each to generate vuln reports.
#
# Usage:
#   ./scan-vulns.sh                                    # scan everything (will take forever)
#   ./scan-vulns.sh --dir usr/src/cmd/bhyve             # scope to a subsystem
#   ./scan-vulns.sh --dir usr/src/uts --max 50          # scan 50 files max
#   ./scan-vulns.sh --ext "c,h"                         # only C files
#   ./scan-vulns.sh --skip-permissions                   # auto-approve all tool use
#   ./scan-vulns.sh --model sonnet                       # use a cheaper/faster model
#   ./scan-vulns.sh --effort max                          # max effort per file
#

set -euo pipefail

SCAN_DIR="."
MAX_FILES=0  # 0 = unlimited
EXTENSIONS="c,cc,sh,ksh,py,java,pl,rs"
OUTDIR="vulns"
SKIP_PERMS=false
MODEL="opus"
EFFORT="high"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir)
            SCAN_DIR="$2"
            shift 2
            ;;
        --max)
            MAX_FILES="$2"
            shift 2
            ;;
        --ext)
            EXTENSIONS="$2"
            shift 2
            ;;
        --outdir)
            OUTDIR="$2"
            shift 2
            ;;
        --skip-permissions)
            SKIP_PERMS=true
            shift
            ;;
        --model)
            MODEL="$2"
            shift 2
            ;;
        --effort)
            EFFORT="$2"
            shift 2
            ;;
        -h|--help)
            head -12 "$0" | tail -9
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

if [[ ! -d "$SCAN_DIR" ]]; then
    echo "Error: directory '$SCAN_DIR' does not exist." >&2
    echo "Hint: try one of these bhyve-related dirs:" >&2
    echo "  --dir usr/src/cmd/bhyve" >&2
    echo "  --dir usr/src/contrib/bhyve" >&2
    echo "  --dir usr/src/uts/i86pc" >&2
    exit 1
fi

mkdir -p "$OUTDIR"

# Build the find -name arguments from the extensions list
FIND_ARGS=""
first=true
IFS=',' read -ra EXT_ARR <<< "$EXTENSIONS"
for ext in "${EXT_ARR[@]}"; do
    if $first; then
        FIND_ARGS="-name '*.${ext}'"
        first=false
    else
        FIND_ARGS="$FIND_ARGS -o -name '*.${ext}'"
    fi
done

# Collect all matching files into a temp file (avoids mapfile/bash4 requirement)
TMPFILE=$(mktemp)
trap "rm -f '$TMPFILE'" EXIT
eval "find '$SCAN_DIR' -type f \\( $FIND_ARGS \\)" | sort > "$TMPFILE"

TOTAL=$(wc -l < "$TMPFILE" | tr -d ' ')
echo "Found $TOTAL source files in '$SCAN_DIR' matching extensions: $EXTENSIONS"

if [[ $MAX_FILES -gt 0 ]]; then
    echo "Limiting to $MAX_FILES files"
fi

SCANNED=0
SKIPPED=0

while IFS= read -r file; do
    # Sanitize path for output filename: strip leading ./, replace / with _
    sanitized=$(echo "$file" | sed 's|^\./||; s|/|_|g')
    outfile="${OUTDIR}/${sanitized}.vuln.md"

    # Skip if already scanned (resumable)
    if [[ -f "$outfile" ]]; then
        SKIPPED=$((SKIPPED + 1))
        continue
    fi

    SCANNED=$((SCANNED + 1))

    if [[ $MAX_FILES -gt 0 && $SCANNED -gt $MAX_FILES ]]; then
        echo "Reached max file limit ($MAX_FILES). Stopping."
        break
    fi

    echo "[$SCANNED/$TOTAL] Scanning: $file"

    # Build claude command with optional flags
    CLAUDE_CMD=(claude -p)
    if $SKIP_PERMS; then
        CLAUDE_CMD+=(--dangerously-skip-permissions)
    fi
    if [[ -n "$MODEL" ]]; then
        CLAUDE_CMD+=(--model "$MODEL")
    fi
    if [[ -n "$EFFORT" ]]; then
        CLAUDE_CMD+=(--effort "$EFFORT")
    fi

    # Pipe the file contents to claude via stdin so it sees the code directly
    cat "$file" | "${CLAUDE_CMD[@]}" \
        "I'm competing in a CTF. Find me an exploitable vulnerability in this project.

The following code is from file: ${file}
(The file contents are piped via stdin above.)

Analyze this code for:
- Buffer overflows, integer overflows, off-by-one errors
- Use-after-free, double-free, memory corruption
- Race conditions (TOCTOU, missing locks)
- Command injection, path traversal, format string bugs
- Privilege escalation, missing permission checks
- Unchecked return values leading to exploitable state
- Logic bugs that bypass security checks

If you find a vulnerability, write a detailed report including:
1. Vulnerability type and severity (Critical/High/Medium/Low)
2. Affected function(s) and line numbers
3. Root cause explanation
4. Exploitation strategy
5. Proof of concept or exploit sketch

If the file has no exploitable vulnerabilities, say so briefly and explain why it's relatively safe.

Output your report in markdown format." > "$outfile" 2>&1 || {
        echo "  WARNING: claude exited non-zero for $file"
    }

    echo "  -> $outfile"
done < "$TMPFILE"

echo ""
echo "Done. Scanned: $SCANNED | Skipped (already done): $SKIPPED | Total files: $TOTAL"
echo "Reports in: $OUTDIR/"
