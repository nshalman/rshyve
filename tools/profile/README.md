# Boot-path profiling

DTrace harness for measuring VMM boot cost. These scripts need a global
zone: they attach to the kernel `sdt:vmm::vmm-vexit` probe, which a
non-global zone cannot see even as root. `run.sh` checks for the probe
and stops if it is missing, so an empty exit count is never mistaken
for a fast boot.

    export VMM_PROFILE_BIN=/opt/rust-bhyve/target/release/firehyve
    tools/profile/run.sh -d 10 -o /var/tmp/pvh -- \
        -c 1 -m 512M --kernel /var/tmp/vmlinux --cmdline "console=ttyS0" \
        -s 4,virtio-rnd -l com1,stdio bench

Scripts:

- `profile.d`   VM-exit counts, 997 Hz user and kernel stacks, off-CPU time
- `vmexit.d`    VM-exit accounting only
- `sample.d`    stack sampling only, for flamegraphs
- `collapse.awk`   DTrace `%k` output to Brendan Gregg collapsed format
- `top-stacks.awk` top-N collapsed stacks with percentages

Record results in [docs/boot-baseline.md](../../docs/boot-baseline.md).
