#!/bin/bash
# Build a minimal fast-boot Linux kernel for vmmnew direct boot.
# Runs inside Docker (Alpine container) with full Linux build toolchain.
#
# Usage: ./build.sh [kernel_version]
# Output: ./vmlinuz-fastboot

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

# Build
make ARCH=x86_64 -j\$(nproc) bzImage 2>&1 | tail -20

# Copy output
cp arch/x86_64/boot/bzImage /build/vmlinuz-fastboot
ls -la /build/vmlinuz-fastboot
echo '=== Kernel built successfully ==='

# Show size comparison
echo \"Fast-boot kernel: \$(du -h /build/vmlinuz-fastboot | cut -f1)\"
"
