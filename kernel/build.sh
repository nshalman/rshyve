#!/bin/bash
# Build a minimal fast-boot Linux kernel for vmmnew direct boot.
# Runs inside Docker (Alpine container) with full Linux build toolchain.
#
# Usage: ./build.sh [kernel_version]
# Output: ./vmlinuz-fastboot  bzImage, gzip self-decompressing
#         ./vmlinux-fastboot  uncompressed ELF with the PVH entry note
#
# Both come from one compile. The PVH image skips the real-mode stub
# and the self-decompression, which the Firecracker NSDI paper measures
# at about 40 ms.

set -euo pipefail

KERNEL_VERSION="${1:-6.12.13}"
KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "=== Building Linux ${KERNEL_VERSION} fast-boot kernel ==="

docker run --rm \
    --platform linux/amd64 \
    -v "${SCRIPT_DIR}:/build" \
    -w /tmp/kernel \
    alpine:3.21 \
    sh -c "
set -ex

# Install build dependencies
apk add --no-cache \
    build-base bc flex bison elfutils-dev openssl-dev \
    perl linux-headers ncurses-dev diffutils findutils

# Download kernel source
TARBALL=linux-${KERNEL_VERSION}.tar.xz
if [ ! -f /build/\${TARBALL} ]; then
    wget -q https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/\${TARBALL} -O /build/\${TARBALL}
fi
tar xf /build/\${TARBALL}
cd linux-${KERNEL_VERSION}

# Start from tinyconfig (absolute minimum) then layer our config
make ARCH=x86_64 tinyconfig

# Apply our fast-boot config on top
cat /build/fastboot.config >> .config
make ARCH=x86_64 olddefconfig

# Show kernel size estimate
echo '=== Config summary ==='
grep -c '=y' .config | xargs printf 'Static options: %s\n'
grep -c '=m' .config | xargs printf 'Module options: %s (should be 0)\n'

# olddefconfig silently drops an option whose dependencies are unmet.
# Catch that here rather than after a full build and a failed boot.
grep -q '^CONFIG_PVH=y' .config || {
    echo 'BUILD-FAIL: CONFIG_PVH was dropped by olddefconfig'
    exit 1
}

# Build
make ARCH=x86_64 -j\$(nproc) bzImage 2>&1 | tail -20

# Copy output. --strip-all keeps allocatable sections, so the PT_NOTE
# carrying the PVH entry survives.
cp arch/x86_64/boot/bzImage /build/vmlinuz-fastboot
objcopy --strip-all vmlinux /build/vmlinux-fastboot

readelf -n /build/vmlinux-fastboot | grep -qi xen || {
    echo 'BUILD-FAIL: vmlinux carries no Xen PVH note'
    exit 1
}

ls -la /build/vmlinuz-fastboot /build/vmlinux-fastboot
echo '=== Kernels built successfully ==='
echo \"bzImage: \$(du -h /build/vmlinuz-fastboot | cut -f1)\"
echo \"PVH ELF: \$(du -h /build/vmlinux-fastboot | cut -f1)\"
"
