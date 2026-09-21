// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The syscalls behind [`super::PrivPlan`].
//!
//! illumos only. Every other platform answers `Unsupported`, so a
//! binary that asked for the drop refuses to start rather than run
//! with a privilege set it never reduced.

use super::{PrivError, PrivNames, Step};

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
mod illumos {
    use std::io;

    use illumos_priv::{
        getppriv, setppriv, PrivOp, PrivPtype, PrivSet, Privilege,
    };

    use super::super::{PrivError, PrivNames, RunAs, Step, Which};

    /// The names this module can put in or read out of a set.
    ///
    /// `priv_set_to_str` is not bound, so the inherited set is read by
    /// membership probe over this list. It covers the basic set, what
    /// the bhyve brand hands its init, and the few the plan names; a
    /// privilege outside it is neither kept nor reported, and the
    /// verify step compares whole sets, so an unlisted one cannot
    /// survive the lock unseen.
    const KNOWN: &[&str] = &[
        "file_chown",
        "file_dac_read",
        "file_dac_write",
        "file_link_any",
        "file_owner",
        "file_read",
        "file_write",
        "net_access",
        "net_rawaccess",
        "proc_clock_highres",
        "proc_exec",
        "proc_fork",
        "proc_info",
        "proc_lock_memory",
        "proc_secflags",
        "proc_session",
        "proc_setid",
        "proc_zone",
        "sys_admin",
        "sys_config",
        "sys_devices",
        "sys_mount",
        "sys_net_config",
    ];

    fn syscall(step: &Step, error: impl ToString) -> PrivError {
        PrivError::Syscall {
            step: format!("{step:?}"),
            error: error.to_string(),
        }
    }

    /// The crate's enum is neither `Clone` nor parsed from a name, so
    /// the name is the handle and this is the one table.
    fn by_name(name: &str) -> Option<Privilege> {
        Some(match name {
            "file_chown" => Privilege::FileChown,
            "file_dac_read" => Privilege::FileDacRead,
            "file_dac_write" => Privilege::FileDacWrite,
            "file_link_any" => Privilege::FileLinkAny,
            "file_owner" => Privilege::FileOwner,
            "file_read" => Privilege::FileRead,
            "file_write" => Privilege::FileWrite,
            "net_access" => Privilege::NetAccess,
            "net_rawaccess" => Privilege::NetRawaccess,
            "proc_clock_highres" => Privilege::ProcClockHighres,
            "proc_exec" => Privilege::ProcExec,
            "proc_fork" => Privilege::ProcFork,
            "proc_info" => Privilege::ProcInfo,
            "proc_lock_memory" => Privilege::ProcLockMemory,
            "proc_secflags" => Privilege::ProcSecflags,
            "proc_session" => Privilege::ProcSession,
            "proc_setid" => Privilege::ProcSetid,
            "proc_zone" => Privilege::ProcZone,
            "sys_admin" => Privilege::SysAdmin,
            "sys_config" => Privilege::SysConfig,
            "sys_devices" => Privilege::SysDevices,
            "sys_mount" => Privilege::SysMount,
            "sys_net_config" => Privilege::SysNetConfig,
            _ => return None,
        })
    }

    /// Build a kernel set from names. A name outside [`KNOWN`] is a
    /// plan bug, not a runtime condition, and fails the lock.
    fn set_from(names: &PrivNames, step: &Step) -> Result<PrivSet, PrivError> {
        let mut set = PrivSet::new_empty().map_err(|e| syscall(step, e))?;
        for name in names {
            let privilege = by_name(name).ok_or_else(|| {
                syscall(
                    step,
                    format!("{name} is not a privilege this tree knows"),
                )
            })?;
            set.addset(privilege).map_err(|e| syscall(step, e))?;
        }
        Ok(set)
    }

    fn ptype(which: Which) -> PrivPtype {
        match which {
            Which::Permitted => PrivPtype::Permitted,
            Which::Limit => PrivPtype::Limit,
            Which::Effective => PrivPtype::Effective,
            Which::Inheritable => PrivPtype::Inheritable,
        }
    }

    pub fn effective_set() -> Result<PrivNames, PrivError> {
        let set =
            getppriv(PrivPtype::Effective).map_err(|e| PrivError::Syscall {
                step: "getppriv(E)".into(),
                error: e.to_string(),
            })?;
        Ok(KNOWN
            .iter()
            .filter(|name| by_name(name).is_some_and(|p| set.is_member(p)))
            .map(|name| name.to_string())
            .collect())
    }

    fn verify(
        expected: &PrivNames,
        run_as: Option<RunAs>,
        step: &Step,
    ) -> Result<(), PrivError> {
        let want = set_from(expected, step)?;
        for which in [Which::Effective, Which::Permitted, Which::Limit] {
            let have = getppriv(ptype(which)).map_err(|e| syscall(step, e))?;
            if !have.is_equal(&want) {
                return Err(PrivError::Verify {
                    which,
                    expected: expected.clone(),
                });
            }
        }
        let inheritable =
            getppriv(PrivPtype::Inheritable).map_err(|e| syscall(step, e))?;
        if !inheritable.is_empty() {
            return Err(PrivError::Verify {
                which: Which::Inheritable,
                expected: PrivNames::new(),
            });
        }
        if let Some(run_as) = run_as {
            // SAFETY: geteuid and getegid take no argument and cannot
            // fail.
            let uid = unsafe { libc::geteuid() };
            if uid != run_as.uid {
                return Err(PrivError::UidNotDropped {
                    expected: run_as.uid,
                    actual: uid,
                });
            }
            let gid = unsafe { libc::getegid() };
            if gid != run_as.gid {
                return Err(PrivError::GidNotDropped {
                    expected: run_as.gid,
                    actual: gid,
                });
            }
        }
        Ok(())
    }

    fn check(ret: libc::c_int, step: &Step) -> Result<(), PrivError> {
        if ret == 0 {
            Ok(())
        } else {
            Err(syscall(step, io::Error::last_os_error()))
        }
    }

    pub fn apply(steps: &[Step]) -> Result<(), PrivError> {
        for step in steps {
            match step {
                Step::Set(which, names) => {
                    let set = set_from(names, step)?;
                    setppriv(PrivOp::Set, ptype(*which), &set)
                        .map_err(|e| syscall(step, e))?;
                }
                Step::Off(which, names) => {
                    let set = set_from(names, step)?;
                    setppriv(PrivOp::Off, ptype(*which), &set)
                        .map_err(|e| syscall(step, e))?;
                }
                Step::SetGroups(groups) => {
                    let gids: Vec<libc::gid_t> = groups.clone();
                    // SAFETY: the pointer and count describe one live
                    // vector for the length of the call.
                    let ret = unsafe {
                        libc::setgroups(
                            gids.len() as libc::c_int,
                            gids.as_ptr(),
                        )
                    };
                    check(ret, step)?;
                }
                // SAFETY: setgid and setuid take one integer each.
                Step::SetGid(gid) => {
                    check(unsafe { libc::setgid(*gid) }, step)?
                }
                Step::SetUid(uid) => {
                    check(unsafe { libc::setuid(*uid) }, step)?
                }
                Step::Verify(expected, run_as) => {
                    verify(expected, *run_as, step)?
                }
            }
        }
        Ok(())
    }
}

/// The effective set of this process, by name.
pub fn effective_set() -> Result<PrivNames, PrivError> {
    #[cfg(any(target_os = "illumos", target_os = "solaris"))]
    {
        illumos::effective_set()
    }
    #[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
    {
        Err(PrivError::Unsupported)
    }
}

/// Run the steps on this process, in order, stopping at the first
/// failure.
///
/// A failure part way leaves the process between two steps of the
/// plan. The caller must not carry on: it has neither the set it
/// started with nor the floor.
pub fn apply(steps: &[Step]) -> Result<(), PrivError> {
    #[cfg(any(target_os = "illumos", target_os = "solaris"))]
    {
        illumos::apply(steps)
    }
    #[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
    {
        let _ = steps;
        Err(PrivError::Unsupported)
    }
}
