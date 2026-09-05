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

Measured on `$VMM_FIREHYVE_HOST`, firehyve, 512 MiB, 1 vCPU, PVH direct boot,
one virtio-blk disk, three runs each. `vm-create` is `creating VM` to
`VM ready` in the VMM log; `guest-to-init` is the guest kernel's own
timestamp on `Run /init as init process`.

| memory backing | vm-create | setup total | guest-to-init |
|---|---|---|---|
| transient (no `-S`) | 310 / 221 / 218 ms | 321 / 231 / 228 ms | 0.404 / 0.405 / 0.431 s |
| reservoir (`-S`) | 31 / 19 / 19 ms | 41 / 28 / 28 ms | 0.403 / 0.404 / 0.405 s |

`-S` makes VM creation about eleven times faster and leaves guest time
untouched, which is what it should do: it changes only how the host gets
the pages.

The numbers follow the kernel cost model. `vmmr_alloc` splits AVL spans
for a reservoir region, so allocation is a few microseconds at any size,
and what remains is `VM_MMAP_MEMSEG` populating EPT entries at roughly
58 ms per GiB, because `map_memseg` always passes `VM_MEMMAP_F_WIRED`.
Without `-S` the kernel instead runs `page_create_va` plus a per-page
`bzero`, about 400 ms per GiB, inside one ioctl. A 4 GiB guest therefore
stalls about 1.8 s at `VM_CREATE` with no reservoir, and about 230 ms
with one.

Populate the reservoir before measuring, or `-S` fails `VM_CREATE` with
ENOMEM:

    ssh $VMM_FIREHYVE_HOST /usr/lib/rsrvrctl -c 512 -a 8192

`rsrvrctl -q` counts only live VMs: `Allocated MiB` moves for a
reservoir-backed VM and `Transient Allocated MiB` for one without, and a
VM that has exited moves neither. Sample while the VM is running, or the
counters look like nothing happened.

### The same guest under Linux and KVM

The guest built for the KVM portability spike boots unmodified under
firehyve, so the two can be compared directly. Same kernel, initramfs,
memory, vCPU count and disk:

| host | guest-to-init |
|---|---|
| illumos, bhyve, firehyve | 0.404 s |
| Linux, KVM, the spike | 0.202 s |

Both reach the guest harness's `VERDICT PASS`. The gap is inside the
guest, after entry, so it is not VMM setup: with `-S` that is already
down to 28 ms. The spike counts 38533 exits for a whole boot, which is
the number to compare against a bhyve exit profile from
[`tools/profile`](../tools/profile/README.md) before drawing any
conclusion about why.

The comparison is not firehyve against firehyve. The spike is a
throwaway of about 1250 lines with no ACPI, SMBIOS or fw_cfg, so some of
what firehyve does before entry has no counterpart there. It also ran on
different hardware.

`sdt:vmm::vmm-vexit` is a kernel probe. It needs the global zone and the
`dtrace_kernel` privilege, and the current build host is a non-global
zone, so the harness cannot run there. Record a run from a global-zone
host with bhyve, and add a row per path here with the host, the date,
and the commit. Numbers are only comparable against a later run on the
same host.
