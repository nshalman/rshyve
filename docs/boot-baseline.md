# Boot-time baseline

Reference numbers for the two direct-boot paths, so a later
optimization has something to move and a regression has something to
fail against. Measured with [`tools/profile/run.sh`](../tools/profile/run.sh).

Both paths use the same VM shape: 1 vCPU, 512 MiB, virtio-rnd only,
COM1 on stdio, and the same initramfs. Only the kernel image differs,
and with it the protocol.

## Method

    export VMM_PROFILE_BIN=/opt/rust-bhyve/target/release/firehyve

    tools/profile/run.sh -d 10 -o /var/tmp/direct -- \
        -c 1 -m 512M --kernel /var/tmp/bzImage \
        --initrd /var/tmp/initramfs.cpio \
        --cmdline "console=ttyS0 earlyprintk=serial" -s 4,virtio-rnd \
        -l com1,stdio bench-direct

    tools/profile/run.sh -d 10 -o /var/tmp/pvh -- \
        -c 1 -m 512M --kernel /var/tmp/vmlinux \
        --initrd /var/tmp/initramfs.cpio \
        --cmdline "console=ttyS0 earlyprintk=serial" -s 4,virtio-rnd \
        -l com1,stdio bench-pvh

No flag selects the protocol. `--kernel` is classified from the file:
`HdrS` at 0x202 means bzImage, and an ELF header carrying a
`XEN_ELFNOTE_PHYS32_ENTRY` note means PVH. The VMM logs which one it
found as `protocol` on the `kernel validated` line, so a run that
profiles the wrong path is visible in `vmm.log`.

Where each number comes from, per output directory:

| Number | Source |
|---|---|
| VM exits | `profile.out`, the `--- VM exits ---` block |
| Off-CPU time | `profile.out`, the `--- off-CPU time (ns) ---` block |
| Top user stack | `top.txt`, first line |
| Guest-side boot time | `guest.log`, the kernel's own `[ nn.nnnnnn]` stamps |

## Results

No numbers are recorded yet.

`sdt:vmm::vmm-vexit` is a kernel probe. It needs the global zone and the
`dtrace_kernel` privilege, and the current build host is a non-global
zone, so the harness cannot run there. Record a run from a global-zone
host with bhyve, and add a row per path here with the host, the date,
and the commit. Numbers are only comparable against a later run on the
same host.
