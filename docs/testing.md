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
export VMM_FIREHYVE_HOST=root@tn0.example   # firehyve / virtio-fs test node
export VMM_FIREHYVE_KEY=~/.ssh/id_lab       # SSH key for that node
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
| `$VMM_FIREHYVE_HOST` | firehyve + virtio-fs tests | No compiler and no outbound network |

### The vTPM crates on a developer host

`vmm_tpm_sys` builds the vendored libtpms from the submodule, so its
tests and the `vmm_tpm` CRB tests need a C toolchain, autotools and
OpenSSL. Every other crate needs none of that. Get them once:

```sh
git submodule update --init third_party/libtpms
brew install autoconf automake libtool pkgconf openssl@3   # macOS
```

Then the two crates build and their tests run on the laptop:

```sh
cargo test -p vmm_tpm_sys -p vmm_tpm
```

`pkgconf` is not optional. It carries `pkg.m4`, which `autoreconf` needs
for the libtpms `PKG_INSTALLDIR` macro, and the build script asks
`pkg-config` where OpenSSL is. Without it `autoreconf` stops at
`'pkgconfig_DATA' is used but 'pkgconfigdir' is undefined`.

Two limits are real, and neither is worked around:

* Only these two crates. rshyve and the rest of the workspace call
  illumos interfaces that no other operating system has.
* A developer host links the libtpms it builds against the OpenSSL that
  `pkg-config` reports. The SmartOS path, which binds the `sunw_`
  prefixed platform libcrypto through a generated header and a link
  farm, is taken only on a SmartOS host, so it is proved on
  `$VMM_BUILD_HOST` and not on a laptop.

If `autoreconf` reports `libtoolize: command not found`, the autoconf in
use is not the Homebrew build, which calls `glibtoolize`. Point it at
the GNU one: `export LIBTOOLIZE=glibtoolize`.

### Build on Headnode

Do this one time if the host still holds the checkout with its old name.
An rsync to the new path does not move the old `target/` cache, and a
full rebuild on this host is slow.

```bash
ssh $VMM_HEADNODE 'test -d /opt/rust-bhyve || mv /opt/vmmnew /opt/rust-bhyve'
```

```bash
rsync -az --checksum --exclude target $VMM_SRC/ $VMM_HEADNODE:/opt/rust-bhyve/

ssh $VMM_HEADNODE 'cd /opt/rust-bhyve && export PATH=/opt/tools/bin:$PATH && cargo build --release 2>&1 | tail -1'
```

Note: rsync may not update timestamps. Use `--checksum` or `touch` changed files before `cargo build` to force recompilation.

### Boot a Test VM

This recipe takes the whole node: `pkill -9 rshyve` ends every VMM
process on it, and the name `testvm` is shared. Give the VM your own
prefix and kill by that prefix when anybody else may be on the same
host. See [Cleanup Test VMs](#cleanup-test-vms).

```bash
ssh $VMM_HEADNODE 'pkill -9 rshyve; /usr/sbin/bhyvectl --vm=testvm --destroy 2>/dev/null; sleep 1; \
zfs rollback zones/vmm-test-disk@final 2>/dev/null; \
nohup /opt/rust-bhyve/target/release/rshyve \
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
ssh $VMM_HEADNODE 'ps -ef | grep "[r]shyve.*testvm"'

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

No fio run covers the virtio-blk bounce buffer yet. Its isolated
per-operation cost is recorded in
[block-bounce-cost.md](block-bounce-cost.md).

### Host Baseline (compare VMM overhead)

```bash
ssh $VMM_HEADNODE 'export PATH=/opt/tools/bin:$PATH; \
  fio --name=t --directory=/opt --rw=randread --bs=4k --direct=1 \
  --ioengine=psync --size=256M --runtime=10 --time_based 2>&1 | grep -E "iops|READ:"'
```

### Cleanup Test VMs

Name every VM you boot with a prefix of your own, and sweep by that
prefix at the end of the run. A session that skipped this left 14 VM
instances and 15 dataset clones alive for 12 hours.

Read the list before you destroy anything:

```bash
export PFX=<your-prefix>-
: "${PFX:?set PFX before you run any sweep below}"
ssh $VMM_HEADNODE "pgrep -fl 'rshyve.*$PFX'; ls /dev/vmm | grep \"^$PFX\""
```

Then sweep what that listed. An empty `PFX` here matches every VM on
the node, so keep the guard:

```bash
: "${PFX:?set PFX before you run any sweep below}"
SWEEP="pkill -f 'rshyve.*$PFX'; \
  for v in \$(ls /dev/vmm | grep \"^$PFX\"); do \
    /usr/sbin/bhyvectl --vm=\$v --destroy; done"
ssh $VMM_HEADNODE "$SWEEP"
ssh -i $VMM_LAB_KEY $VMM_MIGRATE_HOST "$SWEEP"
```

Dataset clones do not go with the process. List them and destroy the
ones you made, by name.

**Never touch a `SYSbhyve-*` instance.** Those belong to the platform
and each one is somebody's running guest. A sweep that matches on
`rshyve` alone, or on no prefix at all, reaches them.

### Boot-Time Profiling

DTrace harness and recorded baselines for the two direct-boot paths.
Needs a global zone: the harness reads a kernel probe.

```sh
export VMM_PROFILE_BIN=/opt/rust-bhyve/target/release/firehyve
tools/profile/run.sh -d 10 -o /var/tmp/pvh -- <vmm-argv...>
```

See [tools/profile/README.md](../tools/profile/README.md) and
[boot-baseline.md](boot-baseline.md).

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
8. **virtio-fs test**: `tools/virtiofs-stage.sh` then `virtiofs-run.sh` - see below
9. **Hotplug test**: boot with `--hotplug`, then `cpu-add`, `mem-add` and `device-add` over the control socket or the vsock `CONTROL` verb - see below
10. **Wedged lease teardown**: delay viona's TX frame release, power the guest off, and watch the shutdown budget expire - see below
11. **Device reset matrix**: reset the device from inside a running guest, repeatedly, with I/O in flight, and verify guest data after each - see below

What one live guest already established, and what it did not, is in
[hardware-run.md](hardware-run.md). Read it before repeating any of it.

### virtio-fs Test

Boots a microVM whose init mounts a virtio-fs share, checks it, and
powers off. It needs a kernel with FUSE, which the fastboot kernel does
not have: `kernel/virtiofs.config` layers the three options on
`kernel/fastboot.config` and builds a variant, so the boot benchmarks
keep measuring the same kernel.

```bash
cd kernel && KERNEL_EXTRA_CONFIG=virtiofs.config KERNEL_OUT_SUFFIX=-virtiofs ./build.sh
```

`$VMM_FIREHYVE_HOST` has no compiler, so build `firehyve` on the build
host and let the staging script carry the binary over:

```bash
tools/remote-build.sh build -p firehyve
scp -i $VMM_BUILD_KEY $VMM_BUILD_HOST:/opt/rust-bhyve/target/release/firehyve /tmp/firehyve

VMM_FIREHYVE_BIN=/tmp/firehyve tools/virtiofs-stage.sh
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST /var/tmp/virtiofs-test/virtiofs-run.sh
```

The run script boots the VM twice: once on a read-write share, where
every mutation must succeed, and once with `ro`, where every mutation
must be refused with EROFS. Each check prints one `VFSTEST:` line and
the run ends with `VIRTIOFS-TEST: PASS` or `FAIL`.

`tools/virtiofs-test/src/checks.rs` and the fixture built by
`tools/virtiofs-stage.sh` have to agree on file names, sizes and
content. Change them together.

To boot a container image as the guest root filesystem instead, stage
one and run the `container` mode. The image is exported read-only and
the guest supplies a tmpfs upper layer through overlayfs. After the run
the script probes the host export for two guest writes, a new file and a
copy-up of `/etc/hostname`. That is two probes, not a comparison of the
whole export against its staged state:

```bash
tools/virtiofs-container-stage.sh alpine:3.21
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST /var/tmp/virtiofs-test/virtiofs-run.sh container
```

The share holds `rootfs/` and `container.json`, the latter derived from
the image's own OCI config at stage time. `container.json` sits outside
`rootfs/`, so it is not inside the container's root, and the share is
exported read-only. `container` runs the harness checks inside the
image; `entrypoint` runs the image's own command instead:

```bash
tools/virtiofs-container-stage.sh hello-world
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST /var/tmp/virtiofs-test/virtiofs-run.sh entrypoint
```

`entrypoint` is not part of `all`, because whether an image's command
terminates is a property of the image: a server image runs until the
timeout, which is correct rather than a failure.

The harness also attaches virtio-rnd and virtio-vsock beside the share,
so one boot covers all three devices. The vsock check dials host port
5555 and requires its bytes back from a `socat` echo server the run
script starts on `<socket>_5555`, which exercises both directions and
the guest-initiated connect path. The bulk case pushes 512 KiB through,
which is larger than one RX chain and larger than a connection's credit
window, so it exercises splitting, credit and backpressure. A pass is
not proof that all three are correct: the ordering defect this case
caught failed only about one run in four. Run it repeatedly.

### Guest kernel comparison

`tools/virtiofs-bench.sh <runs> <kernel>...` boots each kernel through
the read-write check set and reports boot time and I/O time. The guest
also prints `boot_to_init_us` from CLOCK_MONOTONIC, which needs no
console on the command line, and its `/proc/interrupts` lines for the
device, which name the interrupt path in use.

Boot time is dominated by printk to the serial console: the same kernel
reaches init in about 172 ms with `console=ttyS0` and about 42 ms with
`quiet loglevel=0`. Compare like with like, and note the zone's own
command line carries no `console=`.

### Hotplug Test

`tools/virtiofs-stage.sh` stages `tools/hotplug-run.sh` next to the
guest kernel and initramfs, and that script runs the whole test:

```bash
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST \
  'cd /var/tmp/virtiofs-test && VMM_HOTPLUG_DISK=<zvol or file> ./hotplug-run.sh all'
```

It boots the VM, waits for the guest to record the hardware it already
has, hot-adds a disk, a CPU and memory, and exits non-zero unless the
guest reports that it read the new disk, ran a thread on the new CPU and
counted the new memory. `hotplug-run.sh disk|cpu|mem` runs one resource.
`VMM_HOTPLUG_DISK` has no default: the disk is on the operator's own
node, it is read and never written, and it must not be in use by another
VM.

The manual steps below are the fallback, for a resource or a transport
the script does not drive, and for rshyve, which the script does not
cover. They run on `$VMM_FIREHYVE_HOST` for firehyve or `$VMM_HEADNODE`
for rshyve.

#### Guest kernel

The guest needs the ACPI hotplug drivers, which `fastboot.config` turns
off. `kernel/hotplug.config` layers them back on and sets SMP itself, so
the boot benchmarks keep measuring the same kernel:

```bash
cd kernel && KERNEL_EXTRA_CONFIG="vsock.config hotplug.config" \
  KERNEL_OUT_SUFFIX=-hotplug ./build.sh
```

`vsock.config` is layered as well so one kernel also carries the guest
side of a vsock check. The `CONTROL` verb itself is answered by the VMM
and needs no guest driver, so a guest with no `AF_VSOCK` still takes
hotplug commands.

Do not add `smp.config` on top. `hotplug.config` already sets `CONFIG_SMP`
and `CONFIG_NR_CPUS`, and layering both makes `NR_CPUS` depend on
fragment order.

#### Host reservoir

Memory hot-add needs the VMM reservoir populated, and the VMM refuses
`-o hotplug.maxmem` without `-S` for the same reason:

```bash
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST '/usr/lib/rsrvrctl -c 512 -a 2048'
```

A hot-add runs `VM_ALLOC_MEMSEG` on a live VM, and that ioctl takes the
kernel write lock, which freezes every vCPU until it returns. Out of the
reservoir the call only splits spans of an already-populated pool and
costs 3 to 4 microseconds at any size. Without the reservoir the same
call creates and zeroes every page inside the lock, about 400 ms per
GiB, which the guest sees as a stall.

Read the counters back with `rsrvrctl` (its own usage output names the
query flag) before and after a `mem-add`. That is the check that the add
really took host memory: free and allocated must move by exactly the
size added.

A node whose reservoir has no free space fails VM create with ENOMEM as
soon as `-S` is given, which is why `tools/virtiofs-run.sh` leaves `-S`
off. Size the reservoir first, or that VM never boots.

#### Boot a hotplug VM

```bash
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST 'nohup /var/tmp/firehyve \
  -S --hotplug \
  -c cpus=2,maxcpus=8 -m 1G \
  -o hotplug.maxmem=2G -o hotplug.memslot=512M \
  -s 4,virtio-blk,<disk image> \
  -s 9,virtio-vsock,/var/tmp/hp/guest.sock,cid=3 \
  --kernel /var/tmp/hp/vmlinux-fastboot-hotplug \
  --initrd <initramfs> --cmdline "console=ttyS0" \
  hptest > /var/tmp/hp/vm.log 2>&1 < /dev/null &'
```

The boot log should carry `Allowing 2 present CPUs plus 6 hotplug CPUs`,
`ACPI: Enabled 3 GPEs in block 00 to 0F`, and one `acpiphp` line per
slot from 2 to 31, with no ACPI error. `-c cpus=N,maxcpus=M` with `M`
equal to `N` publishes no CPU slots at all, which is the usual reason
`cpu-add` answers that the VM has none.

#### Drive it from the host

firehyve takes the commands on the vsock host socket. Send `CONTROL`
first, then one command per line:

```bash
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST \
  "printf 'CONTROL\ncpu-list\n' | socat - UNIX-CONNECT:/var/tmp/hp/guest.sock"
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST \
  "printf 'CONTROL\ncpu-add 2\nmem-add 536870912\n' | socat - UNIX-CONNECT:/var/tmp/hp/guest.sock"
ssh -i $VMM_FIREHYVE_KEY $VMM_FIREHYVE_HOST \
  "printf 'CONTROL\ndevice-add 5,virtio-blk,<disk image>\n' | socat - UNIX-CONNECT:/var/tmp/hp/guest.sock"
```

rshyve takes the same commands as JSON on its control socket:

```bash
ssh $VMM_HEADNODE 'echo "{\"command\":\"cpu-list\"}" | socat - UNIX-CONNECT:/tmp/vmm-testvm.sock'
ssh $VMM_HEADNODE 'echo "{\"command\":\"cpu-add\",\"id\":2}" | socat - UNIX-CONNECT:/tmp/vmm-testvm.sock'
ssh $VMM_HEADNODE 'echo "{\"command\":\"mem-add\",\"bytes\":536870912}" | socat - UNIX-CONNECT:/tmp/vmm-testvm.sock'
ssh $VMM_HEADNODE 'echo "{\"command\":\"device-add\",\"spec\":\"5,virtio-blk,/dev/zvol/rdsk/zones/d1\"}" | socat - UNIX-CONNECT:/tmp/vmm-testvm.sock'
```

`mem-add` takes BYTES on both transports, not the `-m` size grammar, and
rounds up to a whole `hotplug.memslot`.

#### Check the guest took it

```bash
nproc; cat /sys/devices/system/cpu/cpu2/online
grep MemTotal /proc/meminfo
ls /sys/bus/pci/slots; lspci
dmesg | grep -i 'acpi\|hotplug' | tail -20
```

A `device-remove` is only a request. The guest has to run `_EJ0`, and a
guest that never does leaves the slot in `remove-pending` for the life
of the VM with no host state changed, which `device-list` reports.
`cpu-remove` and `mem-remove` are always refused: illumos has no
`vm_deactivate_cpu` and no `VM_FREE_MEMSEG`.

#### Two gotchas that cost real time

- **illumos has no `timeout(1)`.** A remote `timeout 30 ...` fails with
  "not found", and the failure reads like a hotplug failure rather than
  a missing command. Poll with a `for i in $(seq 1 30); do ...; sleep 1;
  done` loop, the way the boot poll above does.
- **firehyve rejects `-l com1,null`.** It serves COM1 on `stdio` only,
  and any other backend is a startup error rather than a warning. COM1 is
  attached either way and its output goes to the process stdout, so leave
  the flag off (or pass `-l com1,stdio`) and redirect the process, as the
  boot command above does with `> vm.log 2>&1 < /dev/null`.

Tear the VM down by name, `bhyvectl --destroy --vm=hptest`, and kill
only the process you started. The test nodes are shared, so a pattern
kill such as `pkill -f 'firehyv[e]'` reaches every VM on the box and not
just yours. `tools/hotplug-run.sh` destroys by name for that reason.

### Wedged lease teardown

Covers the one shutdown wedge no watchdog can end: a vmm_drv lease that
does not break. `VM_DESTROY_SELF` waits for the lease list to empty in
an untimed `cv_wait`, and process exit then waits for that LWP.
`FEATURES_TODO.md` holds the kernel detail; this section is what to run
and what to look for.

**What this does and does not show.** The shipped configuration excludes
one wait, not every wait. illumos defaults `viona_default_tx_copy` to
`B_TRUE`, and this VMM never calls `VNA_IOC_SET_PARAMS`, so a TX ring
allocates no desb handles, `vr_xfer_outstanding` stays zero, and
`viona_tx_wait_outstanding` (`viona_tx.c`) returns immediately. That
wait is the one the steps below use, and it is the only way a lease has
been shown to park.

Three things stay unshown. Any other path that keeps a ring worker away
from its `vmm_drv_lease_expired()` check holds the lease the same way,
and copying TX data says nothing about those paths. viona is not the
only `vmm_drv` consumer, and `vmm_lease_block` waits for every lease in
the list whoever signed it. `viona_default_tx_copy` is a writable
global, so a link created while it is clear loans guest memory. Read the
claim as "the known wait is excluded", not as "no lease can park".

The steps below turn loaning on and then delay the frame release, which
reproduces the VMM half: the destroy budget expiring, the log lines, and
the parked thread. They do not park it forever. DTrace `chill()` is
capped at 500 ms per CPU per second (`dtrace_chill_max`,
`dtrace_chill_interval`), so the release is delayed, not stopped. A
permanent wedge needs a consumer that never returns the frame.

Both steps 1 and 3 change kernel state on the host. Test nodes only.

1. Turn viona TX guest-memory loaning on, then boot the test VM. The
   per-link default is taken when the link is created, so the order
   matters:

```bash
ssh $VMM_HEADNODE 'mdb -kw -e "viona_default_tx_copy/W 0"'
```

   Boot the VM as in "Boot a Test VM" above.

2. Give the guest TX traffic to send:

```bash
ssh root@$VMM_GUEST_IP "ping -f -s 1400 $VMM_GUEST_GW" &
```

3. Delay viona's frame release:

```bash
ssh $VMM_HEADNODE 'dtrace -w -n "fbt::viona_desb_release:entry { chill(500000000); }"'
```

4. Power the guest off from inside it. That runs the same teardown a
   SIGTERM runs, and it is the common path on a node:

```bash
ssh root@$VMM_GUEST_IP 'poweroff'
```

5. Watch the VMM log. Fifteen seconds after the destroy starts,
   teardown gives up on it and logs `the VM destroy did not return
   inside its budget`. The record carries `likely_cause`, which names
   both a vCPU that will not freeze and an unbroken vmm_drv lease,
   because either can hold the ioctl. If the destroy is still parked
   when the teardown budget runs out, the watchdog follows with `the VMM
   outlived its own shutdown; ending the process` and an exit code of 4.

6. Find the parked thread while the destroy is stuck:

```bash
ssh $VMM_HEADNODE 'pstack $(pgrep -f "rshyve.*testvm")'
```

   The `vm-destroy` thread sits in `ioctl`. Confirm the kernel side:

```bash
ssh $VMM_HEADNODE 'echo "::pgrep rshyve | ::walk thread | ::findstack -v" | mdb -k'
```

   Look for `cv_wait` called from `vmm_lease_block`.

7. Clean up. Stop the DTrace script and the ping. The frames come back,
   the lease breaks, the destroy returns, and the process exits. Put the
   tunable back:

```bash
ssh $VMM_HEADNODE 'mdb -kw -e "viona_default_tx_copy/W 1"'
```

   On a node where a real holder cannot be released, there is no
   userspace recovery. The guest memory stays held until the node
   reboots.

### Device reset matrix

A device reset is the one guest operation this VMM has to complete
synchronously, and no test in the suite drives a real driver through
one. Drive it from inside the guest.

The shape that found bugs, and that
[hardware-run.md](hardware-run.md#the-device-reset-matrix) records:

1. Start a long `dd` against the disk inside the guest and leave it
   running. A reset with no work in flight tests almost nothing.
2. Unbind and rebind the device through sysfs, in a loop. Each rebind
   is a real driver reset, not a control-socket command.
3. Force the legacy transport for part of the run. That path writes the
   status register and reads it back once, with no polling loop, so a
   reset that has not finished when the write returns is visible.
4. After every reset, read back a file of known content in the guest and
   compare its SHA-256. A reset that loses or reorders a completion
   corrupts data quietly; a pass/fail on the reset alone does not see it.
5. Give the guest 4 or more vCPUs, drive one writer per queue with
   `taskset` pinned to the vCPU that owns it, and read
   `/proc/interrupts` per vector afterwards. Every vector must have
   moved. A completion routed to the wrong queue leaves one vector
   carrying the whole run while throughput and every aggregate counter
   stay healthy ([hardware-run.md](hardware-run.md#the-rerun-on-a-distro-guest)).

Two guest resets that also need cover, and that the run above included:
a warm reboot with a dirty page cache, and an ACPI S5 poweroff. After a
poweroff, check the host for a leaked process and for a leaked kernel VM
instance with `bhyvectl --vm=<name> --get-stats`.

Do not read a pass as a result for an illumos guest. The illumos virtio
driver writes the status register once and never reads it back, on
either transport, so it is the strictest case and it has never been run.

### Interrupt injection latency

The residual cost of a device reset is the interrupt injections already
in flight, and each is one ioctl. Measure the ioctl, under three
conditions, because the idle number is not the one that matters:

- idle,
- with the guest running one spinning thread per vCPU,
- during a memory hot-add on the same VM.

The third is the interesting one. Both injection ioctls take the bhyve
VM read lock, and illumos grants that lock to a waiting writer first, so
a hot-add stalls injection for as long as it holds the write lock. Time
`VM_ALLOC_MEMSEG` on the host at the same time, and vary the size added:
the hold scales per GiB.

Numbers from the run, and what they mean for a node, are in
[hardware-run.md](hardware-run.md#why-a-device-reset-has-no-upper-bound).

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
VARS=/var/run/win-boot-test/wintest-vars.fd tools/win-boot-test.sh vars
tools/win-boot-test.sh stop
```

The disk image, the serial log, the console FIFO and the recorded VMM pid
all live in `$RUNDIR`, which defaults to `/var/run/win-boot-test` at mode
0700. `stop` kills the pid it recorded and destroys the VM by name; it
does not `pkill rshyve`, because these nodes carry other people's live
guests.

To boot Windows Setup with the install ISO exposed as a CD-ROM, replace the
fixed NVMe ISO namespace in the harness command with this attachment:

```bash
/opt/rust-bhyve/target/release/rshyve -H -c 2 -m 4G \
  -s 0,hostbridge -s 4,nvme,/var/run/win-boot-test/wintest.img \
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

The build host is `$VMM_BUILD_HOST`, with sources at `/opt/rust-bhyve`. Cargo is
in **`/opt/local/bin`** on this host, not `/opt/tools/bin` (the headnode path).

Only one bhyve VM may exist at a time in that zone. A second `VM_CREATE` returns
`EINVAL`, and killing `rshyve` leaks its instance. Before starting, check
`ls /dev/vmm/` and destroy any stale instance with
`bhyvectl --vm=<name> --destroy`.

When killing the VMM by command line, always bracket a letter:
`pkill -9 -f 'rshyv[e]'`. Without the brackets, `pkill` matches its own ssh
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
- The EFI Shell `reset` command leaves the `rshyve` process alive and the guest wedged.
- OVMF falls through from the hardcoded slot-4 bootorder entry to slot 5, which is how the ISO boots.

### Common Issues

- **OOM on headnode**: Use `-m 512M` instead of `-m 1G` if memory is tight
- **zvol busy**: Wait 2s after killing VMM before `zfs rollback`
- **SSH host key changed**: after reprovisioning, drop the stale entry with `ssh-keygen -R <host>` and verify the new fingerprint out of band. Do not disable host key checking.
- **rsync doesn't trigger rebuild**: Use `--checksum` flag or `touch` changed source files
- **fio "Unable to locate package"**: Need `apt-get update` + `add-apt-repository universe` first
