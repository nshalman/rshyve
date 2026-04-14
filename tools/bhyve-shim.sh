#!/usr/bin/bash
#
# bhyve shim - allows per-zone override of the bhyve binary.
#
# If /bhyve.rust exists and is executable in the zone root, exec it.
# Otherwise fall back to the stock illumos bhyve (bhyve.orig).
#
# To deploy a Rust vmmctl for testing:
#   cp vmm /zones/<uuid>/root/bhyve.rust
#   chmod 0555 /zones/<uuid>/root/bhyve.rust
#

if [[ -f /bhyve.rust ]]; then
    if [[ -x /bhyve.rust ]]; then
        exec /bhyve.rust "$@"
    fi
    echo "bhyve-shim: /bhyve.rust exists but is not executable" >&2
    exit 1
fi

exec /usr/sbin/bhyve.orig "$@"
