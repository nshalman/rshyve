// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Stamp the source revision into the binary.
//!
//! The build driver supplies the revision. This script runs no git,
//! because the illumos build host holds no clone. See the shared file
//! below for the failure that rule prevents.
//!
//! No runpath is emitted here on purpose. The pkgsrc gcc driver already
//! injects /opt/local/lib into every binary it links, and vmm-tpm-sys
//! resolves OpenSSL from the platform image, so an extra `-Wl,-R` only
//! duplicated an entry that was already there.

include!("../../crates/vmm-machine/src/build_rev.rs");

/// Where the shared rule lives, relative to this package.
const BUILD_REV_SRC: &str = "../../crates/vmm-machine/src/build_rev.rs";

fn main() {
    // Cargo re-runs a build script only for the inputs it is told
    // about. Without this line a stamped build after an unstamped one
    // keeps the stale value.
    println!("cargo:rerun-if-env-changed={BUILD_REV_ENV}");
    println!("cargo:rerun-if-changed={BUILD_REV_SRC}");

    println!("cargo:rustc-env=VMM_GIT_COMMIT={}", build_rev());
}
