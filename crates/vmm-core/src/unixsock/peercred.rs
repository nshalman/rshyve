// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Peer credentials for every Unix socket this workspace listens on,
//! and the one policy that admits a peer.
//!
//! # The fact the policy is built on
//!
//! On illumos `getpeerucred(3C)` returns the credential the connecting
//! process held when it opened its endpoint (`tl_open` in
//! `uts/common/io/tl.c` keeps `credp`, and `crgetzoneid` reads
//! `cr_zone`). The path the peer connected through plays no part. So a
//! global-zone process that connects to `/zones/<S>/root/tmp/x.sock`
//! is reported with zone id 0, never with the zone's own id. A rule of
//! "same zone id only" would refuse the CN agent on every connection.
//! The rule here admits two peers and nothing else:
//!
//! - the process's own uid in its own zone, which after the sandbox
//!   privilege drop is the VMM itself and nothing that did not escape
//!   it;
//! - uid 0 in the global zone, which already owns the zone.
//!
//! A platform without zones reports no zone id, and there the uid is
//! the whole policy. One side reporting a zone and the other not is
//! refused: a credential that cannot be compared admits nobody.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;

/// The illumos global zone. A global-zone peer reports this id
/// whatever path it connected through.
pub const GLOBAL_ZONEID: i32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub uid: u32,
    pub gid: u32,
    pub zoneid: Option<i32>,
}

/// Who a listener in this process admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerPolicy {
    /// The uid this process runs as, or will run as once it drops.
    pub uid: u32,
    /// This process's zone, or `None` on a platform without zones.
    pub zoneid: Option<i32>,
}

impl PeerPolicy {
    /// The policy for this process, as it is now or as it will be
    /// after a privilege drop to `run_as_uid`.
    ///
    /// The devices bind their sockets before the drop, so the policy
    /// has to name the uid the process ends at, not the one it starts
    /// with.
    pub fn for_process(run_as_uid: Option<u32>) -> io::Result<Self> {
        // SAFETY: geteuid takes no argument and cannot fail.
        let euid = unsafe { libc::geteuid() };
        Ok(Self {
            uid: run_as_uid.unwrap_or(euid),
            zoneid: process_zoneid()?,
        })
    }

    /// Whether a peer with these credentials may use a listener.
    pub fn admits(&self, peer: &PeerCred) -> bool {
        match (peer.zoneid, self.zoneid) {
            (Some(peer_zone), Some(own_zone)) => {
                (peer_zone == own_zone && peer.uid == self.uid)
                    || (peer_zone == GLOBAL_ZONEID && peer.uid == 0)
            }
            (None, None) => peer.uid == self.uid,
            _ => false,
        }
    }
}

static POLICY: OnceLock<PeerPolicy> = OnceLock::new();

/// Fix the process-wide policy. The first call wins; a later one is
/// refused, because the sockets bound under the first policy would
/// otherwise change hands.
pub fn install_policy(policy: PeerPolicy) -> Result<(), PeerPolicy> {
    POLICY.set(policy)
}

/// The process-wide policy, or the process's current identity when
/// none was installed.
///
/// A credential that cannot be read admits nobody: the fallback is
/// a uid no peer can present.
pub fn policy() -> PeerPolicy {
    if let Some(policy) = POLICY.get() {
        return *policy;
    }
    PeerPolicy::for_process(None).unwrap_or(PeerPolicy {
        uid: u32::MAX,
        zoneid: None,
    })
}

/// Decide one accepted connection against the process policy.
///
/// A credential the platform cannot read refuses the peer: nothing
/// here is served on a guess.
pub fn admit(stream: &UnixStream) -> Result<PeerCred, PeerRefused> {
    let peer = peer_cred(stream).map_err(PeerRefused::Unreadable)?;
    if policy().admits(&peer) {
        Ok(peer)
    } else {
        Err(PeerRefused::Policy(peer))
    }
}

#[derive(Debug)]
pub enum PeerRefused {
    Unreadable(io::Error),
    Policy(PeerCred),
}

impl std::fmt::Display for PeerRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "peer credential unreadable: {e}"),
            Self::Policy(peer) => write!(
                f,
                "peer uid {} zone {:?} is not admitted",
                peer.uid, peer.zoneid
            ),
        }
    }
}

impl std::error::Error for PeerRefused {}

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    struct Ucred(*mut libc::ucred_t);

    impl Drop for Ucred {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from getpeerucred and is
                // freed once, here.
                unsafe {
                    libc::ucred_free(self.0);
                }
            }
        }
    }

    let mut raw = std::ptr::null_mut();
    // SAFETY: the descriptor is a live socket and `raw` is a valid out
    // pointer that getpeerucred fills or leaves null.
    let result = unsafe { libc::getpeerucred(stream.as_raw_fd(), &mut raw) };
    let cred = Ucred(raw);
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if cred.0.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "getpeerucred returned a null credential",
        ));
    }

    // SAFETY: the credential is non-null and owned until `cred` drops.
    let zoneid = unsafe { libc::ucred_getzoneid(cred.0) };
    if zoneid < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the peer credential carries no zone id",
        ));
    }
    Ok(PeerCred {
        uid: unsafe { libc::ucred_geteuid(cred.0) },
        gid: unsafe { libc::ucred_getegid(cred.0) },
        zoneid: Some(zoneid),
    })
}

#[cfg(target_os = "linux")]
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    let mut cred = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the out buffer is sized for one ucred and `len` says so.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an unexpected credential size",
        ));
    }
    // SAFETY: getsockopt filled the whole structure.
    let cred = unsafe { cred.assume_init() };

    Ok(PeerCred {
        uid: cred.uid,
        gid: cred.gid,
        zoneid: None,
    })
}

#[cfg(target_os = "macos")]
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: both out pointers are valid for the call.
    let result =
        unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(PeerCred {
        uid,
        gid,
        zoneid: None,
    })
}

#[cfg(not(any(
    target_os = "illumos",
    target_os = "solaris",
    target_os = "linux",
    target_os = "macos",
)))]
pub fn peer_cred(_stream: &UnixStream) -> io::Result<PeerCred> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix peer credentials are unsupported on this platform",
    ))
}

#[cfg(any(target_os = "illumos", target_os = "solaris"))]
pub fn process_zoneid() -> io::Result<Option<i32>> {
    unsafe extern "C" {
        fn getzoneid() -> libc::zoneid_t;
    }

    // SAFETY: getzoneid takes no argument and cannot fail.
    Ok(Some(unsafe { getzoneid() }))
}

#[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
pub fn process_zoneid() -> io::Result<Option<i32>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zoned(uid: u32, zone: i32) -> PeerCred {
        PeerCred {
            uid,
            gid: 7,
            zoneid: Some(zone),
        }
    }

    fn unzoned(uid: u32) -> PeerCred {
        PeerCred {
            uid,
            gid: 7,
            zoneid: None,
        }
    }

    #[test]
    fn a_platform_without_zones_checks_the_uid_alone() {
        let policy = PeerPolicy {
            uid: 42,
            zoneid: None,
        };
        assert!(policy.admits(&unzoned(42)));
        assert!(!policy.admits(&unzoned(43)));
        assert!(!policy.admits(&unzoned(0)));
    }

    /// The CN agent connects from the global zone through a path under
    /// the zone root, and the kernel reports it with zone id 0. The
    /// policy has to admit that, or the sandbox has no control plane.
    #[test]
    fn global_zone_root_is_admitted_through_a_zone_root_path() {
        let policy = PeerPolicy {
            uid: 8123,
            zoneid: Some(7),
        };
        assert!(policy.admits(&zoned(0, GLOBAL_ZONEID)));
    }

    /// The zone's own uid is the VMM after the drop. Anything else in
    /// the zone, and root in the zone, is not the VMM.
    #[test]
    fn only_the_run_as_uid_is_admitted_from_the_own_zone() {
        let policy = PeerPolicy {
            uid: 8123,
            zoneid: Some(7),
        };
        assert!(policy.admits(&zoned(8123, 7)));
        assert!(!policy.admits(&zoned(0, 7)));
        assert!(!policy.admits(&zoned(8124, 7)));
    }

    #[test]
    fn a_global_zone_peer_that_is_not_root_is_refused() {
        let policy = PeerPolicy {
            uid: 8123,
            zoneid: Some(7),
        };
        // Not even the run-as uid: from the global zone only root owns
        // the zone.
        assert!(!policy.admits(&zoned(8123, GLOBAL_ZONEID)));
        assert!(!policy.admits(&zoned(1, GLOBAL_ZONEID)));
    }

    #[test]
    fn a_sibling_zone_is_refused_whatever_its_uid() {
        let policy = PeerPolicy {
            uid: 0,
            zoneid: Some(7),
        };
        assert!(!policy.admits(&zoned(0, 8)));
        assert!(!policy.admits(&zoned(8123, 8)));
    }

    /// A VMM that has not dropped, in a zone, is root there. rshyve
    /// runs this way, and a global-zone orchestrator drives it.
    #[test]
    fn an_undropped_vmm_admits_its_own_root_and_global_root() {
        let policy = PeerPolicy {
            uid: 0,
            zoneid: Some(7),
        };
        assert!(policy.admits(&zoned(0, 7)));
        assert!(policy.admits(&zoned(0, GLOBAL_ZONEID)));
        assert!(!policy.admits(&zoned(1, 7)));
    }

    #[test]
    fn mismatched_zone_reporting_fails_closed() {
        let zoned_policy = PeerPolicy {
            uid: 0,
            zoneid: Some(0),
        };
        assert!(!zoned_policy.admits(&unzoned(0)));

        let unzoned_policy = PeerPolicy {
            uid: 0,
            zoneid: None,
        };
        assert!(!unzoned_policy.admits(&zoned(0, 0)));
    }

    /// The policy names the uid the process ends at, because the
    /// sockets are bound before the drop.
    #[test]
    fn the_process_policy_names_the_run_as_uid() {
        let policy = PeerPolicy::for_process(Some(8123)).expect("policy");
        assert_eq!(policy.uid, 8123);
        let own = PeerPolicy::for_process(None).expect("policy");
        assert_eq!(own.uid, unsafe { libc::geteuid() });
        assert_eq!(own.zoneid, process_zoneid().expect("zone"));
    }

    // Only the Linux and macOS arms are exercisable from a plain unit
    // test. The illumos path needs a real zone to say anything.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn peer_credentials_match_current_process() {
        let (left, _right) =
            UnixStream::pair().expect("create Unix stream pair");

        let cred = peer_cred(&left).expect("read peer credentials");

        assert_eq!(cred.uid, unsafe { libc::geteuid() });
        assert_eq!(cred.gid, unsafe { libc::getegid() });
        assert_eq!(cred.zoneid, None);
    }

    /// A socket pair in this process is the case every listener test
    /// in the workspace relies on: the default policy admits it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_default_policy_admits_this_process() {
        let (left, _right) = UnixStream::pair().expect("socketpair");
        admit(&left).expect("own process is admitted");
    }
}
