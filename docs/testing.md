# Test infrastructure

The lab this VMM is developed against is private. Every host, address,
and key below is a variable; set them for your own environment. Nothing
in this file names a real machine.

Put the real values in `CLAUDE.local.md` or a shell profile. Both are
gitignored, and none of these values belong in a tracked file.

```sh
export VMM_SRC=/path/to/this/checkout
export VMM_BUILD_HOST=root@build.example    # illumos build host
export VMM_HEADNODE=root@headnode.example   # illumos build + test host
export VMM_MIGRATE_HOST=root@nuc1.example   # migration peer, older CPU
export VMM_PLATFORM_HOST=root@nuc0.example  # platform image test host
export VMM_GUEST_IP=192.0.2.50              # test guest address
export VMM_GUEST_GW=192.0.2.1               # test guest gateway
export VMM_LAB_KEY=~/.ssh/id_lab            # SSH key for the lab hosts
```

Host names in `ssh`/`rsync` commands below already include the user, so
they read `ssh $VMM_HEADNODE ...` rather than `ssh root@$VMM_HEADNODE`.

### Hosts

| Variable | Role | Properties that matter |
|------|------|-------|
| `$VMM_HEADNODE` | Build + test | Low RAM, slow spinning root disk, NVMe pool for zvols |
| `$VMM_MIGRATE_HOST` | Cross-host migration peer | Older CPU: AVX2, no AVX-512 |
| `$VMM_PLATFORM_HOST` | Platform image test | Runs a production platform image |
| `$VMM_BUILD_HOST` | Platform image builds | Full autotools + OpenSSL toolchain |

### Build on Headnode

```bash
rsync -az --checksum --exclude target $VMM_SRC/ $VMM_HEADNODE:/opt/vmmnew/

ssh $VMM_HEADNODE 'cd /opt/vmmnew && export PATH=/opt/tools/bin:$PATH && cargo build --release 2>&1 | tail -1'
```

Note: rsync may not update timestamps. Use `--checksum` or `touch` changed files before `cargo build` to force recompilation.

### Boot a Test VM

```bash
ssh $VMM_HEADNODE 'pkill -9 vmm; /usr/sbin/bhyvectl --vm=testvm --destroy 2>/dev/null; sleep 1; \
zfs rollback zones/vmm-test-disk@final 2>/dev/null; \
nohup /opt/vmmnew/target/release/vmm \
  -H -c 2 -m 1G \
  -s 0,hostbridge -s 4,virtio-blk,/dev/zvol/dsk/zones/vmm-test-disk \
  -s 6,virtio-net-viona,vmm_test0 -s 31,lpc \
  -l bootrom,/usr/share/bhyve/uefi-rom.bin -l com1,stdio \
  --control-socket /tmp/vmm-testvm.sock \
  --mdata-nics "$(cat /tmp/nics.json)" \
  --mdata-ssh-keys "$(cat $VMM_LAB_KEY.pub)" \
  testvm > /tmp/vmm-test.log 2>&1 &
echo started'
```

### Poll for Boot (don't sleep long)

```bash
for i in $(seq 1 20); do
  ping $VMM_GUEST_IP 1 2>/dev/null && echo "up ${i}0s" && break
  sleep 10
done
```

### SSH into Guest VM

Host key checking is disabled here and only here: the test guest is
rebuilt from a ZFS snapshot on every run, so its key legitimately
changes each boot. Never carry these flags over to the lab hosts
themselves.

```bash
ssh $VMM_HEADNODE 'ssh -o UserKnownHostsFile=/dev/null -o StrictHostKeyChecking=no \
  -i $VMM_LAB_KEY root@$VMM_GUEST_IP "COMMAND"'
```

Or directly from macOS (may need host key cleanup):
```bash
ssh-keygen -R $VMM_GUEST_IP 2>/dev/null
ssh -i $VMM_LAB_KEY -o StrictHostKeyChecking=no root@$VMM_GUEST_IP "COMMAND"
```

### Check VM Health

```bash
# Process status
ssh $VMM_HEADNODE 'ps -ef | grep "[v]mm.*testvm"'

# Boot log
ssh $VMM_HEADNODE 'tail -10 /tmp/vmm-test.log'

# Control socket metrics
ssh $VMM_HEADNODE 'echo "{\"command\":\"metrics\"}" | socat - UNIX-CONNECT:/tmp/vmm-testvm.sock'

# Check for IRQ errors
ssh $VMM_HEADNODE 'grep "nobody cared\|Disabling IRQ" /tmp/vmm-test.log'
```

### Install fio in Guest

fio is not in the base image. Install after boot:
```bash
ssh root@$VMM_GUEST_IP "apt-get update -qq 2>/dev/null; \
  add-apt-repository -y universe 2>/dev/null; \
  apt-get install -y fio 2>/dev/null; \
  fio --version"
```

This takes 3-5 minutes on the headnode (slow disk for apt).

### fio Benchmarks

**Important**: Never write to `/dev/vda` — it's the boot disk. Use files in `/tmp/` or read-only raw device tests.

```bash
# Create and warm test file
dd if=/dev/urandom of=/tmp/test.dat bs=1M count=64 2>/dev/null
dd if=/tmp/test.dat of=/dev/null bs=1M 2>/dev/null

# Rand Read 4K sync (single-threaded IOPS)
fio --name=t --filename=/tmp/test.dat --rw=randread --bs=4k --direct=1 \
  --ioengine=sync --runtime=10 --time_based 2>&1 | grep -E "iops|READ:"

# Rand Read 4K libaio (multi-threaded IOPS)
fio --name=t --filename=/tmp/test.dat --rw=randread --bs=4k --direct=1 \
  --ioengine=libaio --iodepth=32 --numjobs=4 --runtime=10 --time_based \
  --group_reporting 2>&1 | grep -E "iops|READ:"

# Seq Read 1M sync
fio --name=t --filename=/tmp/test.dat --rw=read --bs=1M --direct=1 \
  --ioengine=sync --runtime=10 --time_based 2>&1 | grep -E "iops|READ:"

# Cleanup
rm /tmp/test.dat
```

### Host Baseline (compare VMM overhead)

```bash
ssh $VMM_HEADNODE 'export PATH=/opt/tools/bin:$PATH; \
  fio --name=t --directory=/opt --rw=randread --bs=4k --direct=1 \
  --ioengine=psync --size=256M --runtime=10 --time_based 2>&1 | grep -E "iops|READ:"'
```

### Cleanup Test VMs

```bash
ssh $VMM_HEADNODE 'pkill -9 vmm; /usr/sbin/bhyvectl --vm=testvm --destroy 2>/dev/null'
ssh -i $VMM_LAB_KEY $VMM_MIGRATE_HOST 'pkill -9 vmm; /usr/sbin/bhyvectl --vm=testvm --destroy 2>/dev/null'
```

### Sync to Build Host

```bash
rsync -az --exclude target $VMM_SRC/ $VMM_BUILD_HOST:/opt/vmmctl/
```

### Cross-Host Migration Test

```bash
# Source on nuc1 (old CPU)
ssh -i $VMM_LAB_KEY $VMM_MIGRATE_HOST '... start VM with --track-dirty --control-socket ...'

# Destination on headnode (new CPU)
ssh $VMM_HEADNODE '... start VM with --migrate-listen 0.0.0.0:4567 ...'

# Trigger migration
ssh -i $VMM_LAB_KEY $VMM_MIGRATE_HOST \
  'echo "{\"command\":\"migrate-source\",\"target_addr\":\"$VMM_HEADNODE:4567\"}" | \
  socat - UNIX-CONNECT:/tmp/vmm-testvm.sock'
```

### Network Config (mdata)

The NIC JSON is stored at `/tmp/nics.json` on the headnode:
```json
[{"interface":"net0","mac":"02:08:20:aa:bb:cc","ips":["$VMM_GUEST_IP/24"],"gateways":["$VMM_GUEST_GW"],"primary":true}]
```

Both headnode and nuc1 have `vmm_test0` VNICs with MAC `02:08:20:aa:bb:cc`.

### Key Test Scenarios

1. **Boot test**: VM reaches login prompt without errors
2. **Network test**: Guest gets IP via mdata, is pingable, SSH works
3. **I/O test**: fio runs without errors or "nobody cared" IRQ messages
4. **Migration test**: nuc1→headnode (old→new CPU) succeeds
5. **SIGTERM test**: `kill -TERM <pid>` triggers ACPI poweroff
6. **Metrics test**: `echo '{"command":"metrics"}' | socat - UNIX-CONNECT:/tmp/vmm-testvm.sock`
7. **Windows boot test**: `tools/win-boot-test.sh start` then `wait 'Press any key to boot' 120` - see below

### Windows Boot Test

`tools/win-boot-test.sh` manages the file-backed Windows boot/reset test VM. Its
subcommands start and stop the VM, wait for serial-log output, send input to
COM1, inspect the log, and report process/device status:

```bash
tools/win-boot-test.sh start
tools/win-boot-test.sh wait 'Press any key to boot' 120
tools/win-boot-test.sh send '\r'
tools/win-boot-test.sh log 4000
tools/win-boot-test.sh status
VARS=/tmp/wintest-vars.fd tools/win-boot-test.sh vars
tools/win-boot-test.sh stop
```

To boot Windows Setup with the install ISO exposed as a CD-ROM, replace the
fixed NVMe ISO namespace in the harness command with this attachment:

```bash
/opt/vmmnew/target/release/vmm -H -c 2 -m 4G \
  -s 0,hostbridge -s 4,nvme,/tmp/wintest.img \
  -s 5,ahci-cd,/opt/winassets/winsrv2025-eval.iso \
  -s 31,lpc -l bootrom,/usr/share/bhyve/uefi-rom.bin -l com1,stdio wintest
```

At the Windows SAC command prompt, verify that Plug and Play reports a CD-ROM
and that DiskPart mounted the ISO as a CD-ROM volume:

```text
pnputil /enum-devices /connected /class CDROM
diskpart
list volume
exit
```

For the Server 2025 evaluation ISO, `list volume` should report
`SSS_X64FREE`, `UDF`, `CD-ROM`, and `Healthy` on the mounted volume.

The build host is `$VMM_BUILD_HOST`, with sources at `/opt/vmmnew`. Cargo is
in **`/opt/local/bin`** on this host, not `/opt/tools/bin` (the headnode path).

Only one bhyve VM may exist at a time in that zone. A second `VM_CREATE` returns
`EINVAL`, and killing `vmm` leaks its instance. Before starting, check
`ls /dev/vmm/` and destroy any stale instance with
`bhyvectl --vm=<name> --destroy`.

When killing the VMM by command line, always bracket a letter:
`pkill -9 -f 'relea[s]e/vmm'`. Without the brackets, `pkill` matches its own ssh
command line, kills the session, and ssh returns 255. illumos does not provide
`setsid`; use `nohup ... </dev/null &` instead.

- The COM1 FIFO writer-holder must be `nohup ... > $FIFO < /dev/null 2>/dev/null &`. Without `nohup` it dies with the ssh session and the guest's COM1 stdin sees EOF; without the stderr redirect it holds the ssh channel open and `start` never returns.

The Windows assets are `/opt/winassets/winsrv2025-eval.iso` and
`/opt/winassets/virtio-win.iso`; the persistent test disk is
`/opt/wintest/win.raw`.

### Firmware

Two UEFI code images are supported:

- `/usr/share/bhyve/uefi-rom.bin` is the illumos bhyve firmware and includes
  the EFI Internal Shell.
- `/opt/fw/BHYVE_UEFI_CODE.fd` is the pinned FreeBSD edk2-bhyve firmware
  installed by `tools/fetch-edk2-bhyve.sh`. It has TPM and Secure Boot modules
  but no EFI Internal Shell.

The bootrom grammar is `-l bootrom,<rom>[,<varfile>]`. A varfile is writable
guest state, so every VM must use a private copy. Never share a varfile between
VMs; the VMM's whole-file `fcntl` write lock refuses a second user.

With the VM stopped, reset its firmware state from the pristine template and
restrict the new writable copy:

```bash
cp /opt/fw/BHYVE_UEFI_VARS.pristine.fd /var/db/vmm/<vm>/VARS.fd
chmod 600 /var/db/vmm/<vm>/VARS.fd
```

This procedure resets all UEFI variable state. A BitLocker-protected guest will
prompt for its recovery key on the next boot.

`VARS.fd` and the vTPM state directory are one atomic unit of VM state. If
either is lost, the other is useless. Snapshots and backups must capture both
together.

A size-valid, page-aligned image filled entirely with `0xFF` is also accepted
as erased flash and is formatted by firmware on first boot. Do not add a
`--bootvars-reset` flag. Guest reset re-executes the VMM with its existing argv,
so a flag retained in a Triton VM configuration would wipe persistent state on
every reboot.

For the Windows harness, set `ROM=/opt/fw/BHYVE_UEFI_CODE.fd` and `VARS` to a
VM-private path. Each `start` copies `VARS_TEMPLATE` to `VARS`; `vars` prints
the current SHA-256 digest. Do not rerun `start` when testing persistence across
a guest reset.

The EFI Shell `reset` path applies only to the illumos firmware. With the
FreeBSD firmware, configure a control socket and send `{"command":"reset"}`
instead. If an interactive shell is needed, put `ShellX64.efi` at
`\\EFI\\BOOT\\BOOTX64.EFI` on a small FAT image attached as a second read-only
NVMe namespace. The same image can later carry Secure Boot key-enrollment
files, so rebuilding edk2 is unnecessary.

Measured baseline as of commit `212a83c` (known-broken behavior this harness is
intended to catch):

- Boot logs contain `WARN failed to register reset handler, error: would conflict with existing entry`.
- The EFI Shell `reset` command leaves the `vmm` process alive and the guest wedged.
- OVMF falls through from the hardcoded slot-4 bootorder entry to slot 5, which is how the ISO boots.

### Common Issues

- **OOM on headnode**: Use `-m 512M` instead of `-m 1G` if memory is tight
- **zvol busy**: Wait 2s after killing VMM before `zfs rollback`
- **SSH host key changed**: after reprovisioning, drop the stale entry with `ssh-keygen -R <host>` and verify the new fingerprint out of band. Do not disable host key checking.
- **rsync doesn't trigger rebuild**: Use `--checksum` flag or `touch` changed source files
- **fio "Unable to locate package"**: Need `apt-get update` + `add-apt-repository universe` first
