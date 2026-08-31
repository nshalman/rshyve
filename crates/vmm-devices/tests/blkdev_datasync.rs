// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `datasync` must be reachable from outside this crate. The device
//! crates that the split creates call it across a crate boundary, where
//! the old `pub(crate)` form does not resolve.
//!
//! `blkdev` stays in this crate when the device crates split out,
//! so this test target stays here too.

use std::io::Write;
use std::os::fd::AsRawFd;

use vmm_devices::blkdev::datasync;

#[test]
fn datasync_is_public_and_syncs_a_file() {
    let mut file = tempfile::tempfile().expect("create tempfile");
    file.write_all(b"quiesce").expect("write tempfile");
    assert_eq!(datasync(file.as_raw_fd()), 0);
}

#[test]
fn datasync_reports_a_bad_descriptor() {
    assert_ne!(datasync(-1), 0);
}
