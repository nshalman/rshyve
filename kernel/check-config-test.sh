#!/bin/sh
# Self-test for check-config.sh.
#
# A gate nobody has watched fail is not known to work, and this one
# stands between a hostile guest and an option that came back on.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
chk="$here/check-config.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
rc=0

case_() { # name, expected status, config, fragments...
    name=$1
    want=$2
    cfg=$3
    shift 3
    got=0
    out=$("$chk" "$tmp/$cfg" "$tmp" "$@" 2>&1) || got=$?
    if [ "$got" != "$want" ]; then
        echo "FAIL: $name: status $got, expected $want"
        echo "$out"
        rc=1
    fi
}

cat > "$tmp/on.config" <<'EOF'
CONFIG_USER_NS=y
CONFIG_SMP=y
CONFIG_NR_CPUS=8
EOF

cat > "$tmp/off.config" <<'EOF'
# CONFIG_USER_NS is not set
# CONFIG_VSOCKETS_LOOPBACK is not set
EOF

# The spelling kconfig reads as a comment, so the option stays on.
printf '#CONFIG_VSOCKETS_LOOPBACK is not set\n' > "$tmp/nospace.config"
printf '# CONFIG_VSOCKETS_LOOPBACK is not set \n' > "$tmp/trailing.config"

# Prose that ends in something else must not read as an option.
cat > "$tmp/prose.config" <<'EOF'
# CONFIG_CPUSETS lives in smp.config: it depends on SMP, and
# olddefconfig drops it silently on a uniprocessor kernel.
CONFIG_CPUSETS=y
EOF

cat > "$tmp/resolved.config" <<'EOF'
CONFIG_SMP=y
CONFIG_NR_CPUS=8
CONFIG_CPUSETS=y
# CONFIG_USER_NS is not set
# CONFIG_VSOCKETS_LOOPBACK is not set
EOF

cat > "$tmp/re-enabled.config" <<'EOF'
CONFIG_SMP=y
CONFIG_NR_CPUS=8
CONFIG_CPUSETS=y
CONFIG_USER_NS=y
CONFIG_VSOCKETS_LOOPBACK=y
EOF

cat > "$tmp/clamped.config" <<'EOF'
CONFIG_SMP=y
CONFIG_NR_CPUS=1
CONFIG_CPUSETS=y
# CONFIG_USER_NS is not set
# CONFIG_VSOCKETS_LOOPBACK is not set
EOF

# A later fragment turns off what an earlier one turned on. That is the
# ordering rule, and it must not read as a dropped option.
case_ "last assignment wins" 0 resolved.config on.config prose.config off.config
case_ "an option that came back on" 1 re-enabled.config on.config prose.config off.config
case_ "an option that went missing" 1 clamped.config on.config prose.config off.config
case_ "no space after the hash" 1 re-enabled.config on.config nospace.config
case_ "a trailing space" 1 re-enabled.config on.config trailing.config
case_ "prose is not an option" 0 resolved.config prose.config
# A base fragment given with -s: its malformed line still fails, but
# its values are not asserted, so a later fragment may override them.
case_ "syntax-only still catches the malformed line" 1 resolved.config -s nospace.config on.config
case_ "syntax-only does not assert values" 0 re-enabled.config -s off.config on.config
case_ "syntax-only alone passes" 0 resolved.config -s off.config
case_ "a fragment that is not there" 1 resolved.config gone.config
case_ "a config that is not there" 1 gone.config on.config

[ "$rc" -eq 0 ] && echo "check-config.sh: all cases behave"
exit "$rc"
