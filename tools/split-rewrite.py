#!/usr/bin/env python3
"""Mechanical half of the vmm-devices crate split.

One rewriter, three stages, so tools/split-probe.sh measures exactly the
rewrite the real commits apply. Stages are independently applicable and
match the commit boundaries: metrics, passthru, leaves (vmm-virtio plus
vmm-storage plus vmm-hid), or all.

Paths only. Shortened call sites can leave lines rustfmt would rejoin,
so the caller runs `cargo fmt --all` before it commits.
"""

import argparse
import os
import shutil
import subprocess
import sys


def dep_line(name):
    if name == "usdt":
        return 'usdt = { workspace = true, features = ["asm"] }\n'
    if name == "tempfile":
        return 'tempfile = "3"\n'
    return "%s.workspace = true\n" % name


MANIFEST = """[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
rust-version.workspace = true
license = "MPL-2.0"

[lints]
workspace = true

[dependencies]
{deps}{dev}"""

# crate -> (directory, dependencies, dev-dependencies)
CRATES = {
    "vmm_metrics": ("crates/vmm-metrics", ["serde", "serde_json"], []),
    "vmm_passthru": (
        "crates/vmm-passthru",
        ["vmm_devices", "vmm_core", "bhyve_api", "libc"],
        [],
    ),
    "vmm_virtio": (
        "crates/vmm-virtio",
        ["vmm_devices", "vmm_core", "viona_api", "anyhow", "libc", "slog",
         "thiserror", "usdt"],
        ["tempfile", "vmm_devices_testsupport"],
    ),
    "vmm_storage": (
        "crates/vmm-storage",
        ["vmm_devices", "vmm_core", "anyhow", "libc", "slog"],
        ["tempfile", "vmm_devices_testsupport"],
    ),
    "vmm_hid": (
        "crates/vmm-hid",
        ["vmm_devices", "vmm_core", "bhyve_api", "anyhow", "ascii",
         "bitflags", "des", "libc", "slog", "usdt"],
        [],
    ),
}

STAGE_CRATES = {
    "metrics": ["vmm_metrics"],
    "passthru": ["vmm_passthru"],
    "leaves": ["vmm_virtio", "vmm_storage", "vmm_hid"],
}

# Substitutions applied to every .rs file under each new crate's src/.
# Order matters. crate::virtio:: resolves first because the module tree
# loses that level, and each `use` form with braces resolves before the
# single-item forms that could otherwise match inside it.
#
# crate::pci:: always means the base PCI module here: virtio's own
# transport module is reached through super::pci, which stays valid.
CRATE_REWRITES = {
    "vmm_passthru": [
        ("crate::pci::", "vmm_devices::pci::"),
    ],
    "vmm_virtio": [
        ("crate::virtio::", "crate::"),
        ("use crate::{FlushError, FlushIntent, Lifecycle, QuiesceGate};",
         "use vmm_devices::{FlushError, FlushIntent, Lifecycle, QuiesceGate};"),
        ("use crate::{Lifecycle, QuiesceGate};",
         "use vmm_devices::{Lifecycle, QuiesceGate};"),
        ("crate::pci::", "vmm_devices::pci::"),
        ("crate::lifecycle::", "vmm_devices::lifecycle::"),
        ("crate::migrate::", "vmm_devices::migrate::"),
        ("crate::blkdev::", "vmm_devices::blkdev::"),
        ("crate::quiesce::", "vmm_devices::quiesce::"),
        ("crate::Lifecycle", "vmm_devices::Lifecycle"),
        ("crate::FlushIntent", "vmm_devices::FlushIntent"),
        ("crate::FlushError", "vmm_devices::FlushError"),
    ],
    "vmm_storage": [
        ("use crate::{FlushError, FlushIntent, Lifecycle, QuiesceGate};",
         "use vmm_devices::{FlushError, FlushIntent, Lifecycle, QuiesceGate};"),
        ("use crate::{Lifecycle, Migrator};",
         "use vmm_devices::{Lifecycle, Migrator};"),
        ("crate::pci::", "vmm_devices::pci::"),
        ("crate::lifecycle::", "vmm_devices::lifecycle::"),
        ("crate::blkdev::", "vmm_devices::blkdev::"),
        ("crate::quiesce::", "vmm_devices::quiesce::"),
    ],
    "vmm_hid": [
        ("use crate::{InputBroker, Lifecycle};",
         "use vmm_devices::{InputBroker, Lifecycle};"),
        ("crate::pci::", "vmm_devices::pci::"),
        ("crate::lifecycle::", "vmm_devices::lifecycle::"),
        ("crate::acpi_pm::", "vmm_devices::acpi_pm::"),
        ("crate::PointerSink", "vmm_devices::PointerSink"),
        ("crate::KeyboardSink", "vmm_devices::KeyboardSink"),
        ("crate::Lifecycle", "vmm_devices::Lifecycle"),
    ],
}

# Modules a new crate owns. Any other crate:: path left in its sources
# after the rewrite points at a module that stayed in the base crate.
OWNED_MODULES = {
    "vmm_passthru": [],
    "vmm_virtio": ["attach", "bits", "block", "chain_io", "console", "fs",
                   "packed_queue", "pci", "queue", "rng", "viona"],
    "vmm_storage": ["ahci", "nvme"],
    "vmm_hid": ["fbuf", "ps2", "xhci"],
}

# Dependencies each binary gains, per stage. firehyve gains vmm_virtio
# and nothing else: its whole point is that it cannot reach the parity
# device surface.
BIN_DEPS = {
    "metrics": [],
    "passthru": [("bin/rshyve/Cargo.toml", ["vmm_passthru"])],
    "leaves": [
        ("bin/rshyve/Cargo.toml", ["vmm_virtio", "vmm_storage", "vmm_hid"]),
        ("bin/firehyve/Cargo.toml", ["vmm_virtio"]),
    ],
}

# Binary source trees rewritten per stage. Whole trees, not named files,
# because the catalogs move between modules as the split lands.
BIN_REWRITES = {
    "metrics": [],
    "passthru": [
        ("bin/rshyve/src", [("vmm_devices::passthru::", "vmm_passthru::")]),
    ],
    "leaves": [
        ("bin/rshyve/src", [
            ("vmm_devices::virtio::", "vmm_virtio::"),
            ("vmm_devices::nvme::", "vmm_storage::nvme::"),
            ("vmm_devices::ahci::", "vmm_storage::ahci::"),
            ("vmm_devices::fbuf::", "vmm_hid::fbuf::"),
            ("vmm_devices::ps2::", "vmm_hid::ps2::"),
            ("vmm_devices::xhci::", "vmm_hid::xhci::"),
        ]),
        ("bin/firehyve/src", [("vmm_devices::virtio::", "vmm_virtio::")]),
    ],
}

BASE_MODULES = {
    "metrics": ["metrics"],
    "passthru": ["passthru"],
    "leaves": ["ahci", "fbuf", "nvme", "ps2", "virtio", "xhci"],
}

# Dependencies that leave crates/vmm-devices entirely, per stage. Each
# has exactly one consumer group in the tree being carved out.
BASE_DROPS = {
    "metrics": ["serde_json"],
    "passthru": [],
    "leaves": ["viona_api", "ascii", "des", "usdt"],
}

BASE_DEV_DROPS = {
    "metrics": [],
    "passthru": [],
    "leaves": ["vmm_devices_testsupport"],
}

LICENSE_HEADER = """// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""

STORAGE_LIB = LICENSE_HEADER + """
//! AHCI and NVMe storage controllers.

pub mod ahci;
pub mod nvme;
"""

HID_LIB = LICENSE_HEADER + """
//! Human-interface and display devices: PS/2, xHCI tablet, framebuffer.

pub mod fbuf;
pub mod ps2;
pub mod xhci;
"""

METRICS_ALIAS = """/// Metrics live in their own crate. The alias keeps
/// `vmm_devices::metrics::*` resolving for the control socket and the
/// vCPU plumbing, which reach it by that path.
pub use vmm_metrics as metrics;

pub use input::{InputBroker, KeyboardSink, PointerSink};"""


def read(path):
    with open(path, "r", encoding="utf-8") as handle:
        return handle.read()


def write(path, body):
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(body)


def edit(path, pairs):
    body = read(path)
    for old, new in pairs:
        body = body.replace(old, new)
    write(path, body)


def move(root, src, dst):
    dst_abs = os.path.join(root, dst)
    parent = os.path.dirname(dst_abs)
    if parent:
        os.makedirs(parent, exist_ok=True)
    rc = subprocess.call(
        ["git", "-C", root, "mv", src, dst],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    if rc != 0:
        shutil.move(os.path.join(root, src), dst_abs)


def each_rs(directory):
    for base, _dirs, files in os.walk(directory):
        for name in sorted(files):
            if name.endswith(".rs"):
                yield os.path.join(base, name)


def rewrite_tree(root, crate):
    pairs = CRATE_REWRITES[crate]
    src_dir = os.path.join(root, CRATES[crate][0], "src")
    for path in each_rs(src_dir):
        edit(path, pairs)


def check_no_foreign_paths(root, crate):
    """Fail loudly rather than emit a crate that cannot resolve.

    A crate:: path naming a module the new crate does not own is a
    rewrite rule that was never written. Catching the module forms here
    names the missing rule, where the compiler can only name the symbol
    that vanished. Item forms such as crate::Lifecycle stay its job.
    """
    owned = set(OWNED_MODULES[crate])
    src_dir = os.path.join(root, CRATES[crate][0], "src")
    stale = []
    for path in each_rs(src_dir):
        for number, line in enumerate(read(path).splitlines(), 1):
            for chunk in line.split("crate::")[1:]:
                name = ""
                for char in chunk:
                    if char.isalnum() or char == "_":
                        name += char
                    else:
                        break
                if name and name[:1].islower() and name not in owned:
                    stale.append("%s:%d: crate::%s" % (path, number, name))
    if stale:
        raise SystemExit(
            "split-rewrite: %s keeps paths into the base crate:\n  %s"
            % (crate, "\n  ".join(stale))
        )


def write_manifest(root, crate):
    directory, deps, dev = CRATES[crate]
    block = ""
    if dev:
        block = "\n[dev-dependencies]\n" + "".join(dep_line(d) for d in dev)
    write(
        os.path.join(root, directory, "Cargo.toml"),
        MANIFEST.format(
            name=crate, deps="".join(dep_line(d) for d in deps), dev=block
        ),
    )


def insert_after(body, anchor, addition):
    if anchor not in body:
        raise SystemExit("split-rewrite: anchor not found: %r" % anchor)
    return body.replace(anchor, anchor + addition, 1)


def patch_workspace(root, crates):
    path = os.path.join(root, "Cargo.toml")
    body = read(path)
    members = "".join(
        '    "%s",\n' % CRATES[c][0]
        for c in crates
        if '    "%s",\n' % CRATES[c][0] not in body
    )
    if members:
        body = insert_after(body, '    "crates/vmm-devices",\n', members)
    entries = "".join(
        '%s = { path = "%s" }\n' % (c, CRATES[c][0])
        for c in crates
        if '%s = { path = "%s" }\n' % (c, CRATES[c][0]) not in body
    )
    if entries:
        body = insert_after(
            body, 'vmm_devices = { path = "crates/vmm-devices" }\n', entries
        )
    write(path, body)


def patch_base(root, stage):
    lib = os.path.join(root, "crates/vmm-devices/src/lib.rs")
    body = read(lib)
    for module in BASE_MODULES[stage]:
        body = body.replace("pub mod %s;\n" % module, "")
    if stage == "metrics":
        body = body.replace(
            "pub use input::{InputBroker, KeyboardSink, PointerSink};",
            METRICS_ALIAS,
        )
    write(lib, body)

    manifest = os.path.join(root, "crates/vmm-devices/Cargo.toml")
    body = read(manifest)
    for dropped in BASE_DROPS[stage] + BASE_DEV_DROPS[stage]:
        body = body.replace(dep_line(dropped), "")
    if stage == "metrics":
        body = insert_after(
            body, "vmm_core.workspace = true\n", dep_line("vmm_metrics")
        )
    write(manifest, body)


def patch_binaries(root, stage):
    for rel, crates in BIN_DEPS[stage]:
        path = os.path.join(root, rel)
        body = read(path)
        added = "".join(
            dep_line(c) for c in crates if dep_line(c) not in body
        )
        if added:
            body = insert_after(body, dep_line("vmm_devices"), added)
        write(path, body)
    for rel, pairs in BIN_REWRITES[stage]:
        for path in each_rs(os.path.join(root, rel)):
            edit(path, pairs)


def run_stage(root, stage):
    crates = STAGE_CRATES[stage]
    for crate in crates:
        write_manifest(root, crate)

    if stage == "metrics":
        move(root, "crates/vmm-devices/src/metrics.rs",
             "crates/vmm-metrics/src/lib.rs")
    elif stage == "passthru":
        move(root, "crates/vmm-devices/src/passthru.rs",
             "crates/vmm-passthru/src/lib.rs")
    else:
        move(root, "crates/vmm-devices/src/virtio", "crates/vmm-virtio/src")
        move(root, "crates/vmm-virtio/src/mod.rs",
             "crates/vmm-virtio/src/lib.rs")
        for module in ("ahci", "nvme"):
            move(root, "crates/vmm-devices/src/%s" % module,
                 "crates/vmm-storage/src/%s" % module)
        for module in ("fbuf", "ps2", "xhci"):
            move(root, "crates/vmm-devices/src/%s" % module,
                 "crates/vmm-hid/src/%s" % module)
        write(os.path.join(root, "crates/vmm-storage/src/lib.rs"), STORAGE_LIB)
        write(os.path.join(root, "crates/vmm-hid/src/lib.rs"), HID_LIB)

    for crate in crates:
        if crate in CRATE_REWRITES:
            rewrite_tree(root, crate)
            check_no_foreign_paths(root, crate)

    patch_workspace(root, crates)
    patch_base(root, stage)
    patch_binaries(root, stage)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument(
        "--stage", required=True,
        choices=["metrics", "passthru", "leaves", "all"],
    )
    args = parser.parse_args()
    root = os.path.abspath(args.root)
    stages = (
        ["metrics", "passthru", "leaves"]
        if args.stage == "all"
        else [args.stage]
    )
    for stage in stages:
        run_stage(root, stage)
    print("split-rewrite: applied %s" % ", ".join(stages), file=sys.stderr)


if __name__ == "__main__":
    main()
