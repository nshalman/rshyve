// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Proof that the guest consumes a resource the host added while it was
//! running.
//!
//! The rendezvous these checks sit inside is described in
//! [`crate::hotplug`]. The rule every check here follows: snapshot the
//! state before the host is told to act, compare after, and say in the
//! failure text what was there both times. A check that cannot tell
//! "the host added nothing" from "the guest ignored it" is worth
//! nothing, so none of them assert on an absolute value.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use crate::checks::last_err;
use crate::hotplug::{mem_total_kb, parse_cpu_list, HotplugSpec, Resource};
use crate::report::{Check, Outcome, Report};

const SYS_BLOCK: &str = "/sys/block";
const SYS_CPU: &str = "/sys/devices/system/cpu";
const SYS_MEMORY: &str = "/sys/devices/system/memory";
const MEMINFO: &str = "/proc/meminfo";

/// How often the guest looks for what the host said it added.
const POLL: Duration = Duration::from_millis(200);

/// Bytes read off the hot-added disk and summed. One page: enough to
/// prove the data path serves a real request, small enough for the host
/// to read and sum with `dd` before the VM starts.
pub const DISK_PROBE_LEN: usize = 4096;

const SECTOR: u64 = 512;

/// Run the checks for every resource the spec expects.
pub fn run_hotplug_checks(rep: &mut Report, spec: &HotplugSpec) {
    let before = Snapshot::take();
    rep.note(&format!("hotplug before: {}", before.summary()));

    // Nothing the host adds before this line counts: the snapshot above
    // is what every expectation is measured against.
    rep.note(&format!(
        "{} expect={} timeout={}s",
        crate::hotplug::READY,
        spec.expect_text(),
        spec.timeout.as_secs()
    ));

    let mut onlined: BTreeSet<String> = BTreeSet::new();
    let (after, waited) = wait_for_hotplug(&before, spec, &mut onlined);
    rep.note(&format!(
        "hotplug after {} ms: {}",
        waited.as_millis(),
        after.summary()
    ));

    for resource in &spec.expect {
        match resource {
            Resource::Disk => disk_checks(rep, &before, &after, spec),
            Resource::Cpu => cpu_checks(rep, &before, &after),
            Resource::Mem => mem_checks(rep, &before, &after, spec, &onlined),
        }
    }
}

/// Wait until every expected resource has arrived, or the deadline
/// passes.
///
/// Returning on the deadline is what makes the harness safe to run: a
/// host that adds nothing gets a report of what the guest saw, not a VM
/// that never powers off.
fn wait_for_hotplug(
    before: &Snapshot,
    spec: &HotplugSpec,
    onlined: &mut BTreeSet<String>,
) -> (Snapshot, Duration) {
    let start = Instant::now();
    // The guest kernel is built with CONFIG_MEMORY_HOTPLUG_DEFAULT_ONLINE,
    // so a new block should online itself. Half the deadline is its
    // chance to; after that the guest does it by hand and the report
    // says which path was needed.
    let hand_online_at = spec.timeout / 2;

    loop {
        let now = Snapshot::take();
        if spec.expect.iter().all(|r| now.satisfies(before, *r)) {
            return (now, start.elapsed());
        }
        if spec.wants(Resource::Mem)
            && !now.satisfies(before, Resource::Mem)
            && start.elapsed() >= hand_online_at
        {
            onlined.extend(online_new_blocks(before, &now));
        }
        if start.elapsed() >= spec.timeout {
            return (Snapshot::take(), start.elapsed());
        }
        std::thread::sleep(POLL);
    }
}

/// What the guest can see of its own hardware, at one instant.
struct Snapshot {
    disks: BTreeSet<String>,
    cpus: BTreeSet<u32>,
    online_cpus: BTreeSet<u32>,
    mem_blocks: BTreeSet<String>,
    mem_total_kb: u64,
}

impl Snapshot {
    fn take() -> Self {
        Self {
            disks: dir_names(SYS_BLOCK, ""),
            cpus: numbered(&dir_names(SYS_CPU, "cpu"), "cpu"),
            online_cpus: parse_cpu_list(&read_trim(&format!(
                "{SYS_CPU}/online"
            ))),
            mem_blocks: dir_names(SYS_MEMORY, "memory"),
            mem_total_kb: mem_total_kb(&read_trim(MEMINFO)).unwrap_or(0),
        }
    }

    /// True once `self` holds something `before` did not.
    fn satisfies(&self, before: &Snapshot, resource: Resource) -> bool {
        match resource {
            Resource::Disk => grew(&self.disks, &before.disks),
            Resource::Cpu => grew(&self.cpus, &before.cpus),
            // The block directory appears as soon as the DIMM is
            // enumerated, but the memory is not the guest's until it is
            // online, which is what MemTotal counts.
            Resource::Mem => self.mem_total_kb > before.mem_total_kb,
        }
    }

    fn summary(&self) -> String {
        format!(
            "disks=[{}] cpus=[{}] online=[{}] mem_blocks={} memtotal_kb={}",
            names(&self.disks),
            ids(&self.cpus),
            ids(&self.online_cpus),
            self.mem_blocks.len(),
            self.mem_total_kb,
        )
    }
}

fn disk_checks(
    rep: &mut Report,
    before: &Snapshot,
    after: &Snapshot,
    spec: &HotplugSpec,
) {
    let new: Vec<String> =
        after.disks.difference(&before.disks).cloned().collect();
    rep.run("hotplug_disk_appeared", || {
        if new.is_empty() {
            return Err(format!(
                "no new block device: {SYS_BLOCK} held [{}] before and \
                 [{}] after",
                names(&before.disks),
                names(&after.disks)
            ));
        }
        Ok(format!(
            "[{}] appeared; [{}] was there before",
            new.join(" "),
            names(&before.disks)
        ))
    });

    let Some(name) = new.first() else {
        // Named as its own failure rather than skipped: a check that
        // does not run must still count against the report.
        rep.record(
            "hotplug_disk_read",
            Outcome::Fail("no new block device to read from".to_string()),
        );
        return;
    };
    rep.run("hotplug_disk_read", || disk_read(name, spec));
}

/// Read the hot-added disk, which is the only thing that proves the
/// data path works rather than the PCI device merely being enumerated.
fn disk_read(name: &str, spec: &HotplugSpec) -> Check {
    let sectors =
        read_u64(&format!("{SYS_BLOCK}/{name}/size")).ok_or_else(|| {
            format!("{SYS_BLOCK}/{name}/size does not read as a number")
        })?;
    // Both operands come from the guest's own sysfs, but the size of a
    // device the host named is not this program's to trust.
    let bytes = sectors
        .checked_mul(SECTOR)
        .ok_or_else(|| format!("{name} reports {sectors} sectors"))?;
    if let Some(want) = spec.disk_bytes {
        if bytes != want {
            return Err(format!(
                "{name} is {bytes} bytes, the host added {want}"
            ));
        }
    }

    let path = format!("/dev/{name}");
    let mut file =
        File::open(&path).map_err(|e| format!("open {path}: {e}"))?;
    let mut head = vec![0u8; DISK_PROBE_LEN];
    file.read_exact(&mut head).map_err(|e| {
        format!("read {DISK_PROBE_LEN} bytes at 0 of {path}: {e}")
    })?;
    let sum: u64 = head.iter().map(|b| u64::from(*b)).sum();
    let content = match spec.disk_sum {
        Some(want) if want != sum => {
            return Err(format!(
                "first {DISK_PROBE_LEN} bytes of {path} sum to {sum}, the \
                 host read {want}"
            ))
        }
        Some(_) => "the host reads the same bytes",
        None => "no host sum to compare against",
    };

    // A second read far from zero: a device wired to the wrong backing
    // store, or one that answers only the first request, fails here and
    // not above.
    let tail_at = bytes
        .checked_sub(SECTOR)
        .ok_or_else(|| format!("{path} is only {bytes} bytes"))?;
    file.seek(SeekFrom::Start(tail_at))
        .map_err(|e| format!("seek {path} to {tail_at}: {e}"))?;
    let mut tail = [0u8; SECTOR as usize];
    file.read_exact(&mut tail)
        .map_err(|e| format!("read the last sector of {path}: {e}"))?;

    Ok(format!(
        "{path} {bytes} bytes, head sum {sum} ({content}), last sector read"
    ))
}

fn cpu_checks(rep: &mut Report, before: &Snapshot, after: &Snapshot) {
    let new: Vec<u32> = after.cpus.difference(&before.cpus).copied().collect();
    rep.run("hotplug_cpu_appeared", || {
        if new.is_empty() {
            return Err(format!(
                "no new CPU: {SYS_CPU} held [{}] before and [{}] after",
                ids(&before.cpus),
                ids(&after.cpus)
            ));
        }
        Ok(format!(
            "cpu{} appeared; [{}] was there before",
            new.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" cpu"),
            ids(&before.cpus)
        ))
    });

    let Some(&id) = new.first() else {
        for name in ["hotplug_cpu_online", "hotplug_cpu_runs"] {
            rep.record(
                name,
                Outcome::Fail("no new CPU to bring up".to_string()),
            );
        }
        return;
    };
    // "The slot appeared", "the CPU is online" and "the CPU runs code"
    // are three different claims, so they are three checks.
    rep.run("hotplug_cpu_online", || cpu_online(id, before));
    rep.run("hotplug_cpu_runs", || cpu_runs(id));
}

fn cpu_online(id: u32, before: &Snapshot) -> Check {
    let path = format!("{SYS_CPU}/cpu{id}/online");
    let how = if read_trim(&path) == "1" {
        "arrived online"
    } else {
        std::fs::write(&path, "1")
            .map_err(|e| format!("write 1 to {path}: {e}"))?;
        "was onlined by the guest"
    };

    // Read back rather than trust the write: the store can be accepted
    // and the bring-up still fail.
    let state = read_trim(&path);
    if state != "1" {
        return Err(format!("{path} reads '{state}' after the write"));
    }
    let online = parse_cpu_list(&read_trim(&format!("{SYS_CPU}/online")));
    if !online.contains(&id) {
        return Err(format!(
            "cpu{id} is not in {SYS_CPU}/online, which reads [{}]",
            ids(&online)
        ));
    }
    Ok(format!(
        "cpu{id} {how}; the online set was [{}] and is now [{}]",
        ids(&before.online_cpus),
        ids(&online)
    ))
}

/// Pin a thread to the new CPU and confirm it runs there.
///
/// The strongest claim available from inside the guest: a CPU that is
/// listed but never scheduled on would pass every sysfs check and still
/// be useless.
fn cpu_runs(id: u32) -> Check {
    // On its own thread because affinity binds the caller, and PID 1
    // has the rest of the report to write.
    std::thread::spawn(move || pin_to(id))
        .join()
        .map_err(|_| format!("the thread pinned to cpu{id} panicked"))?
}

fn pin_to(id: u32) -> Check {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_ZERO(&mut set) };
    unsafe { libc::CPU_SET(id as usize, &mut set) };
    let rc = unsafe {
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set)
    };
    if rc != 0 {
        return Err(format!("sched_setaffinity to cpu{id}: {}", last_err()));
    }

    // Real work, not just a syscall: the answer has to come from the
    // CPU having executed instructions, and the kernel migrates the
    // thread inside sched_setaffinity.
    let mut acc: u64 = 0;
    for i in 0..2_000_000u64 {
        acc = acc.wrapping_add(i ^ acc.rotate_left(7));
    }
    let got = unsafe { libc::sched_getcpu() };
    if got < 0 {
        return Err(format!("sched_getcpu: {}", last_err()));
    }
    if got as u32 != id {
        return Err(format!("pinned to cpu{id} but ran on cpu{got}"));
    }
    Ok(format!(
        "a thread pinned to cpu{id} ran there (acc {acc:#x})"
    ))
}

fn mem_checks(
    rep: &mut Report,
    before: &Snapshot,
    after: &Snapshot,
    spec: &HotplugSpec,
    onlined: &BTreeSet<String>,
) {
    let new: Vec<String> = after
        .mem_blocks
        .difference(&before.mem_blocks)
        .cloned()
        .collect();
    rep.run("hotplug_mem_block", || {
        if new.is_empty() {
            return Err(format!(
                "no new block under {SYS_MEMORY}: {} before, {} after",
                before.mem_blocks.len(),
                after.mem_blocks.len()
            ));
        }
        let offline: Vec<&String> =
            new.iter().filter(|b| block_state(b) != "online").collect();
        if !offline.is_empty() {
            return Err(format!(
                "{} new blocks, {} still offline: {:?}",
                new.len(),
                offline.len(),
                offline
            ));
        }
        Ok(format!("{} new blocks, all online", new.len()))
    });

    rep.run("hotplug_mem_total", || {
        let route = if onlined.is_empty() {
            "the kernel onlined it"
        } else {
            "the guest had to online it"
        };
        let grew_kb = after.mem_total_kb.saturating_sub(before.mem_total_kb);
        if grew_kb == 0 {
            return Err(format!(
                "MemTotal is still {} kB; {} new blocks appeared and the \
                 guest onlined {}",
                before.mem_total_kb,
                new.len(),
                onlined.len()
            ));
        }
        let grew = grew_kb.saturating_mul(1024);
        if let Some(want) = spec.mem_bytes {
            // The kernel keeps a page map for the new range, so a little
            // of what the host added never reaches MemTotal. An eighth
            // is far more than that overhead and far less than a slot.
            let floor = want - want / 8;
            if grew < floor {
                return Err(format!(
                    "MemTotal grew {grew} bytes, the host added {want} \
                     ({route})"
                ));
            }
        }
        Ok(format!(
            "MemTotal {} -> {} kB, +{grew} bytes ({route})",
            before.mem_total_kb, after.mem_total_kb
        ))
    });
}

/// Online every new memory block that is still offline. Returns the
/// ones written to, which is how the report says the kernel did not do
/// it by itself.
fn online_new_blocks(before: &Snapshot, now: &Snapshot) -> Vec<String> {
    let mut wrote = Vec::new();
    for block in now.mem_blocks.difference(&before.mem_blocks) {
        if block_state(block) == "online" {
            continue;
        }
        if std::fs::write(format!("{SYS_MEMORY}/{block}/state"), "online")
            .is_ok()
        {
            wrote.push(block.clone());
        }
    }
    wrote
}

fn block_state(block: &str) -> String {
    read_trim(&format!("{SYS_MEMORY}/{block}/state"))
}

/// Names of the entries in `dir` that start with `prefix`.
///
/// An unreadable directory reads as empty. The checks compare two
/// snapshots taken the same way, so a directory that is missing on both
/// sides is reported as "nothing appeared" rather than as an error with
/// no bearing on the hot-add.
fn dir_names(dir: &str, prefix: &str) -> BTreeSet<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix))
        .collect()
}

/// The numbers off names of the form `<prefix><number>`, dropping the
/// siblings that share the prefix but are not one, such as `cpufreq`.
fn numbered(names: &BTreeSet<String>, prefix: &str) -> BTreeSet<u32> {
    names
        .iter()
        .filter_map(|n| n.strip_prefix(prefix))
        .filter_map(|n| n.parse().ok())
        .collect()
}

fn read_trim(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn read_u64(path: &str) -> Option<u64> {
    read_trim(path).parse().ok()
}

fn grew<T: Ord>(now: &BTreeSet<T>, before: &BTreeSet<T>) -> bool {
    now.difference(before).next().is_some()
}

fn names(set: &BTreeSet<String>) -> String {
    set.iter().cloned().collect::<Vec<_>>().join(" ")
}

fn ids(set: &BTreeSet<u32>) -> String {
    set.iter().map(u32::to_string).collect::<Vec<_>>().join(" ")
}
