// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The privilege plan: what the VMM keeps once every device is open.
//!
//! The shape is the platform bhyve's (`cmd/bhyve/common/privileges.c`
//! in illumos): snapshot the inherited effective set at startup, build
//! the floor from it, and after the last device is open set PERMITTED,
//! then LIMIT, then EFFECTIVE. This tree adds one step that binary does
//! not have: a setuid to a fixed numeric id after the lock, so the
//! process can no longer open the 0600 root:sys device nodes it
//! started from.
//!
//! # The floor
//!
//! `file_read` and, when the process inherited it, `proc_clock_highres`.
//! Nothing else: `file_write` goes, so no host path can be opened for
//! writing or connected to after the lock (`secpolicy_vnode_access2`
//! refuses VWRITE outright without it, whatever the DAC bits say);
//! `proc_fork`, `proc_exec`, `net_access` and the rest of the basic
//! set go with it. Everything the VMM still touches is a descriptor it
//! already holds.
//!
//! # The setuid
//!
//! `setuid` from 0 needs `proc_setid` in E, and the kernel keeps E and
//! P across the call once the process is privilege aware
//! (`priv_reset_PA` acts only with `PRIV_AWARE_RESET`, which nothing
//! here sets). So the lock keeps `proc_setid` in P and E, does the
//! setgroups, setgid and setuid, and turns it off last. L is set to
//! the floor at once: L bounds P only across exec, so it may go below
//! P for the few calls in between.
//!
//! # What is pure and what is not
//!
//! The plan, its steps and the open-device gate are pure and unit
//! tested on every platform. The syscalls live in [`sys`] and are only
//! exercised on illumos; CI cannot prove them, so the binaries log
//! which way they went.

pub mod sys;

use std::collections::BTreeSet;
use std::fmt;

/// The one privilege that must survive: the loader and the logger
/// read files the process already opened, and a lazy library load
/// reads a new one.
pub const FILE_READ: &str = "file_read";
/// Kept when inherited, as the platform bhyve keeps it: the guest's
/// timers are worth the resolution, and bhyve runs without it.
pub const PROC_CLOCK_HIGHRES: &str = "proc_clock_highres";
/// Held through the setuid and turned off after it.
pub const PROC_SETID: &str = "proc_setid";

/// A set of privilege names, as `ppriv -v` spells them.
pub type PrivNames = BTreeSet<String>;

fn names(list: &[&str]) -> PrivNames {
    list.iter().map(|s| s.to_string()).collect()
}

/// The numeric identity the sandbox VMM ends at. No passwd entry, so
/// nothing on the host can log in as it or resolve it to a name.
pub const SANDBOX_RUN_AS: RunAs = RunAs {
    uid: 8123,
    gid: 8123,
};

/// The identity to drop to after the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunAs {
    pub uid: u32,
    pub gid: u32,
}

/// Which process set a step changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Permitted,
    Limit,
    Effective,
}

impl fmt::Display for Which {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Permitted => "P",
            Self::Limit => "L",
            Self::Effective => "E",
        })
    }
}

/// One syscall of the lock, in order. The list is the contract that
/// the tests pin; `sys::apply` runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `setppriv(PRIV_SET, which, set)`.
    Set(Which, PrivNames),
    /// `setppriv(PRIV_OFF, which, set)`.
    Off(Which, PrivNames),
    /// `setgroups` to exactly this list.
    SetGroups(Vec<u32>),
    SetGid(u32),
    SetUid(u32),
    /// `getppriv` on E, P and L must each equal this set, and the
    /// effective uid must be this one when given.
    Verify(PrivNames, Option<u32>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PrivError {
    /// The process did not start with `file_read`, so no floor can be
    /// built that keeps the VMM running.
    NoFileRead,
    /// A device the argv names is not in the registry, so its backing
    /// file may not be open yet. Locking now would leave it unopenable.
    DeviceNotOpen(Vec<String>),
    /// The `/dev/vmm` handle is not open.
    VmmNotOpen,
    /// A syscall of the lock failed. The process cannot continue: its
    /// privilege state is between two steps of a plan.
    Syscall { step: String, error: String },
    /// A set read back after the lock is not the floor.
    Verify { which: Which, expected: PrivNames },
    /// The effective uid after the setuid is not the run-as uid.
    UidNotDropped { expected: u32, actual: u32 },
    /// This platform has no privilege sets.
    Unsupported,
}

impl fmt::Display for PrivError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFileRead => write!(
                f,
                "the inherited privilege set lacks {FILE_READ}; no floor \
                 keeps this process running"
            ),
            Self::DeviceNotOpen(ids) => write!(
                f,
                "refusing to lock privileges: {} not in the device \
                 registry, so its backing file is not proven open",
                ids.join(", ")
            ),
            Self::VmmNotOpen => {
                write!(f, "refusing to lock privileges: /dev/vmm is not open")
            }
            Self::Syscall { step, error } => {
                write!(f, "privilege lock step {step} failed: {error}")
            }
            Self::Verify { which, expected } => write!(
                f,
                "privilege set {which} is not the floor after the lock \
                 (expected {})",
                render(expected)
            ),
            Self::UidNotDropped { expected, actual } => {
                write!(f, "effective uid is {actual} after setuid({expected})")
            }
            Self::Unsupported => {
                write!(f, "this platform has no privilege sets to lock")
            }
        }
    }
}

impl std::error::Error for PrivError {}

/// The floor, as one log field.
pub fn render(set: &PrivNames) -> String {
    let mut out = String::from("{");
    out.push_str(&set.iter().cloned().collect::<Vec<_>>().join(","));
    out.push('}');
    out
}

/// What the lock needs proven open before it runs.
///
/// The lock takes away `file_write` and the setuid takes away the
/// right to reopen the device nodes. A device that is not open by then
/// never will be, so the plan refuses rather than boots a machine with
/// a hole in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWitness {
    /// The VM handle exists. Always true for a caller holding a
    /// machine, and named so the contract is visible.
    pub vmm_open: bool,
    /// The device ids the argv asked for.
    pub expected_devices: Vec<String>,
    /// The device ids the registry holds after the attach pass. Every
    /// one of these opened its backing file to get there.
    pub present_devices: Vec<String>,
}

impl OpenWitness {
    pub fn check(&self) -> Result<(), PrivError> {
        if !self.vmm_open {
            return Err(PrivError::VmmNotOpen);
        }
        let present: BTreeSet<&str> =
            self.present_devices.iter().map(String::as_str).collect();
        let missing: Vec<String> = self
            .expected_devices
            .iter()
            .filter(|id| !present.contains(id.as_str()))
            .cloned()
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(PrivError::DeviceNotOpen(missing))
        }
    }
}

/// The plan, from the inherited set to the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivPlan {
    inherited: PrivNames,
    floor: PrivNames,
}

impl PrivPlan {
    /// Snapshot the inherited set and derive the floor.
    pub fn begin(inherited: PrivNames) -> Result<Self, PrivError> {
        if !inherited.contains(FILE_READ) {
            return Err(PrivError::NoFileRead);
        }
        let mut floor = names(&[FILE_READ]);
        if inherited.contains(PROC_CLOCK_HIGHRES) {
            floor.insert(PROC_CLOCK_HIGHRES.to_string());
        }
        Ok(Self { inherited, floor })
    }

    /// Snapshot this process's effective set.
    pub fn begin_from_process() -> Result<Self, PrivError> {
        Self::begin(sys::effective_set()?)
    }

    pub fn inherited(&self) -> &PrivNames {
        &self.inherited
    }

    pub fn floor(&self) -> &PrivNames {
        &self.floor
    }

    /// The syscalls of the lock, in order.
    ///
    /// With a `run_as`, `proc_setid` rides in P and E until the setuid
    /// is done and is turned off last. Without one, the three sets go
    /// straight to the floor.
    pub fn steps(&self, run_as: Option<RunAs>) -> Vec<Step> {
        let floor = self.floor.clone();
        let Some(run_as) = run_as else {
            return vec![
                Step::Set(Which::Permitted, floor.clone()),
                Step::Set(Which::Limit, floor.clone()),
                Step::Set(Which::Effective, floor.clone()),
                Step::Verify(floor, None),
            ];
        };
        let mut with_setid = floor.clone();
        with_setid.insert(PROC_SETID.to_string());
        vec![
            Step::Set(Which::Permitted, with_setid.clone()),
            Step::Set(Which::Limit, floor.clone()),
            Step::Set(Which::Effective, with_setid),
            Step::SetGroups(vec![run_as.gid]),
            Step::SetGid(run_as.gid),
            Step::SetUid(run_as.uid),
            Step::Off(Which::Permitted, names(&[PROC_SETID])),
            Step::Verify(floor, Some(run_as.uid)),
        ]
    }

    /// Check the witness, then run the steps on this process.
    ///
    /// Returns what was done, for the one log line the binary writes.
    pub fn lock(
        &self,
        witness: &OpenWitness,
        run_as: Option<RunAs>,
    ) -> Result<LockReport, PrivError> {
        witness.check()?;
        let steps = self.steps(run_as);
        sys::apply(&steps)?;
        Ok(LockReport {
            floor: self.floor.clone(),
            run_as,
        })
    }
}

/// What a completed lock left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockReport {
    pub floor: PrivNames,
    pub run_as: Option<RunAs>,
}

impl LockReport {
    /// One field for the status verb and the log line.
    pub fn summary(&self) -> String {
        match self.run_as {
            Some(run_as) => format!(
                "E=P=L={} uid={} gid={}",
                render(&self.floor),
                run_as.uid,
                run_as.gid
            ),
            None => format!("E=P=L={}", render(&self.floor)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The effective set the P-1 probe saw firehyve start with as a
    /// bhyve-brand zone's init.
    fn brand_floor() -> PrivNames {
        names(&[
            "file_link_any",
            "file_read",
            "file_write",
            "net_access",
            "proc_exec",
            "proc_fork",
            "proc_info",
            "proc_secflags",
            "proc_session",
            "net_rawaccess",
            "proc_clock_highres",
            "proc_lock_memory",
            "sys_admin",
            "sys_mount",
        ])
    }

    #[test]
    fn the_floor_is_file_read_and_the_inherited_clock() {
        let plan = PrivPlan::begin(brand_floor()).expect("plan");
        assert_eq!(
            plan.floor(),
            &names(&[FILE_READ, PROC_CLOCK_HIGHRES]),
            "the platform bhyve ends at file_read,proc_clock_highres"
        );
    }

    #[test]
    fn a_process_without_the_clock_privilege_still_locks() {
        let mut inherited = brand_floor();
        inherited.remove(PROC_CLOCK_HIGHRES);
        let plan = PrivPlan::begin(inherited).expect("plan");
        assert_eq!(plan.floor(), &names(&[FILE_READ]));
    }

    #[test]
    fn a_process_without_file_read_cannot_start() {
        let mut inherited = brand_floor();
        inherited.remove(FILE_READ);
        assert_eq!(PrivPlan::begin(inherited), Err(PrivError::NoFileRead));
    }

    /// Nothing from the brand floor above the basic set survives, and
    /// nothing is added that the process did not inherit.
    #[test]
    fn the_floor_is_a_subset_of_the_inherited_set() {
        let plan = PrivPlan::begin(brand_floor()).expect("plan");
        assert!(plan.floor().is_subset(plan.inherited()));
        for dropped in [
            "sys_admin",
            "sys_mount",
            "net_rawaccess",
            "proc_lock_memory",
            "file_write",
            "proc_fork",
            "proc_exec",
            "net_access",
        ] {
            assert!(!plan.floor().contains(dropped), "{dropped} survived");
        }
    }

    /// The order the platform bhyve uses: P, then L, then E.
    #[test]
    fn the_lock_without_a_drop_sets_p_then_l_then_e() {
        let plan = PrivPlan::begin(brand_floor()).expect("plan");
        let floor = plan.floor().clone();
        assert_eq!(
            plan.steps(None),
            vec![
                Step::Set(Which::Permitted, floor.clone()),
                Step::Set(Which::Limit, floor.clone()),
                Step::Set(Which::Effective, floor.clone()),
                Step::Verify(floor, None),
            ]
        );
    }

    /// `setuid` from 0 needs `proc_setid` in E. It has to ride through
    /// the lock and leave last, and L can go to the floor at once.
    #[test]
    fn the_drop_keeps_proc_setid_until_the_setuid_is_done() {
        let plan = PrivPlan::begin(brand_floor()).expect("plan");
        let floor = plan.floor().clone();
        let mut with_setid = floor.clone();
        with_setid.insert(PROC_SETID.to_string());

        let steps = plan.steps(Some(SANDBOX_RUN_AS));
        assert_eq!(
            steps,
            vec![
                Step::Set(Which::Permitted, with_setid.clone()),
                Step::Set(Which::Limit, floor.clone()),
                Step::Set(Which::Effective, with_setid),
                Step::SetGroups(vec![8123]),
                Step::SetGid(8123),
                Step::SetUid(8123),
                Step::Off(Which::Permitted, names(&[PROC_SETID])),
                Step::Verify(floor, Some(8123)),
            ]
        );
    }

    /// Mutation this kills: turning `proc_setid` off before the setuid,
    /// which would make the setuid fail with EPERM.
    #[test]
    fn proc_setid_is_in_e_at_the_setuid() {
        let plan = PrivPlan::begin(brand_floor()).expect("plan");
        let steps = plan.steps(Some(SANDBOX_RUN_AS));
        let mut effective = PrivNames::new();
        for step in &steps {
            match step {
                Step::Set(Which::Effective, set) => effective = set.clone(),
                Step::Set(Which::Permitted, set) => {
                    // The kernel intersects E with a new P.
                    effective = effective.intersection(set).cloned().collect();
                }
                Step::Off(Which::Permitted, set) => {
                    effective = effective.difference(set).cloned().collect();
                }
                Step::SetUid(_) | Step::SetGid(_) | Step::SetGroups(_) => {
                    assert!(
                        effective.contains(PROC_SETID),
                        "{step:?} would fail: proc_setid is not in E"
                    );
                }
                _ => {}
            }
        }
        assert_eq!(effective, *plan.floor(), "proc_setid survived the lock");
    }

    #[test]
    fn the_witness_refuses_a_device_that_is_not_registered() {
        let witness = OpenWitness {
            vmm_open: true,
            expected_devices: vec![
                "virtio-blk@4".into(),
                "virtio-vsock@17".into(),
                "virtio-net-viona@7".into(),
            ],
            present_devices: vec![
                "virtio-blk@4".into(),
                "virtio-vsock@17".into(),
            ],
        };
        assert_eq!(
            witness.check(),
            Err(PrivError::DeviceNotOpen(vec!["virtio-net-viona@7".into()]))
        );
    }

    #[test]
    fn the_witness_passes_when_every_device_is_registered() {
        let witness = OpenWitness {
            vmm_open: true,
            expected_devices: vec!["virtio-blk@4".into()],
            present_devices: vec!["virtio-blk@4".into(), "varstore".into()],
        };
        assert_eq!(witness.check(), Ok(()));
    }

    #[test]
    fn the_witness_refuses_without_the_vmm_handle() {
        let witness = OpenWitness {
            vmm_open: false,
            expected_devices: vec![],
            present_devices: vec![],
        };
        assert_eq!(witness.check(), Err(PrivError::VmmNotOpen));
    }

    /// The witness runs before any syscall, so a refused lock leaves
    /// the process as it was.
    #[test]
    fn a_refused_witness_runs_no_step() {
        let plan = PrivPlan::begin(brand_floor()).expect("plan");
        let witness = OpenWitness {
            vmm_open: false,
            expected_devices: vec![],
            present_devices: vec![],
        };
        assert_eq!(
            plan.lock(&witness, Some(SANDBOX_RUN_AS)),
            Err(PrivError::VmmNotOpen)
        );
    }

    #[test]
    fn the_report_names_the_floor_and_the_identity() {
        let report = LockReport {
            floor: names(&[FILE_READ, PROC_CLOCK_HIGHRES]),
            run_as: Some(SANDBOX_RUN_AS),
        };
        assert_eq!(
            report.summary(),
            "E=P=L={file_read,proc_clock_highres} uid=8123 gid=8123"
        );
        let report = LockReport {
            floor: names(&[FILE_READ]),
            run_as: None,
        };
        assert_eq!(report.summary(), "E=P=L={file_read}");
    }

    /// The lock is not something a unit test may run: on illumos it
    /// would drop the test process, and elsewhere it has nothing to
    /// drop. Either way the answer to a plain run here is an error,
    /// never a silent no-op.
    #[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
    #[test]
    fn a_platform_without_privilege_sets_refuses_to_lock() {
        let plan = PrivPlan::begin(brand_floor()).expect("plan");
        let witness = OpenWitness {
            vmm_open: true,
            expected_devices: vec![],
            present_devices: vec![],
        };
        assert_eq!(plan.lock(&witness, None), Err(PrivError::Unsupported));
        assert_eq!(PrivPlan::begin_from_process(), Err(PrivError::Unsupported));
    }
}
