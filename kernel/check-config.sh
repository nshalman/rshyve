#!/bin/sh
# Check a resolved kernel .config against the fragments that made it.
#
# Usage: check-config.sh <resolved .config> <fragment dir> <fragment>...
#
# kconfig drops an option whose dependencies are unmet and says nothing,
# and it turns a "default y" option back on for the same reason. Both
# give a kernel that builds, boots and is not the kernel the fragments
# asked for. This reports every difference before it exits, so one run
# names them all.
#
# build.sh runs this in the container before the compile, and
# .github/workflows/guest-kernel.yml runs it again on the config the
# build wrote out.

set -eu

if [ $# -lt 3 ]; then
    echo "usage: $0 <resolved .config> <fragment dir> <fragment>..." >&2
    exit 2
fi

cfg=$1
dir=$2
shift 2
rc=0

if [ ! -f "$cfg" ]; then
    echo "FAIL: no resolved config at $cfg"
    exit 1
fi

# kconfig reads a negative line only as "# CONFIG_X is not set", with one
# space after the hash (scripts/kconfig/confdata.c tests line[1] == ' ').
# Every other spelling is a plain comment, so the option comes back on
# and nothing reports it.
for frag in "$@"; do
    path="$dir/$frag"
    if [ ! -f "$path" ]; then
        echo "FAIL: no config fragment at $path"
        rc=1
        continue
    fi
    bad=$(grep -nE '^#[[:space:]]*CONFIG_[A-Z0-9_]+[[:space:]]+is not set[[:space:]]*$' "$path" \
          | grep -vE '^[0-9]+:# CONFIG_[A-Z0-9_]+ is not set$' || true)
    if [ -n "$bad" ]; then
        echo "FAIL: $frag holds a line kconfig reads as a comment, not as an option:"
        echo "$bad"
        rc=1
    fi
done
[ "$rc" -eq 0 ] || exit "$rc"

# kconfig keeps the LAST assignment of a symbol, so the fragments are
# read in order and only the last line for each symbol is asserted. A
# fragment that turns off what an earlier one turned on is the whole
# point of the ordering rule, and must not read as a dropped option.
want=$(mktemp)
trap 'rm -f "$want"' EXIT
for frag in "$@"; do
    cat "$dir/$frag"
done | awk '
    /^CONFIG_[A-Z0-9_]+=/ {
        eq = index($0, "=")
        want[substr($0, 1, eq - 1)] = $0
    }
    /^# CONFIG_[A-Z0-9_]+ is not set$/ {
        want[$2] = "# " $2 " is not set"
    }
    END { for (sym in want) print want[sym] }
' | sort > "$want"

while read -r line; do
    case "$line" in
    "# "*)
        sym=${line#\# }
        sym=${sym% is not set}
        if grep -q "^$sym=" "$cfg"; then
            echo "FAIL: $sym is set in $cfg, and a fragment asks for it off"
            rc=1
        fi
        ;;
    "")
        ;;
    *)
        if ! grep -qxF "$line" "$cfg"; then
            echo "FAIL: $line is not in $cfg"
            rc=1
        fi
        ;;
    esac
done < "$want"

exit "$rc"
