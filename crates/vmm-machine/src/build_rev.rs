// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The source revision that a VMM binary reports as its version.
//
// `bin/firehyve/build.rs` and `bin/rshyve/build.rs` `include!` this
// file, and lib.rs compiles it again under `cfg(test)` so the rules
// below are tested. A build script cannot import the crate it helps to
// build, which is why the text is shared and not the compiled code.
// Plain `//` comments only: an inner doc comment is an error at the
// point of an `include!`.
//
// The revision comes from the environment and never from git. The
// illumos build host holds no clone, because tools/remote-build.sh
// syncs the tree with `--exclude .git`. A `git rev-parse` there answers
// for whatever repository the build directory sits under, or fails, and
// the binary in the platform image then named a commit that resolves in
// no clone. An operator who cannot map a running VMM to a commit cannot
// tell which code runs a hostile guest.

/// The variable the build driver sets to the source revision.
const BUILD_REV_ENV: &str = "TRITON_BUILD_REV";

/// The stamp of a build whose revision was not supplied.
///
/// A wrong commit is worse than no commit, so an absent or malformed
/// value gives this marker and the CI gate refuses a release build that
/// carries it.
const UNSTAMPED_REV: &str = "unstamped";

/// The marker the build driver appends when the tree has changes that
/// no commit holds.
const DIRTY_SUFFIX: &str = "-dirty";

/// The shortest accepted abbreviation. git abbreviates to 7 digits by
/// default and lengthens the name when 7 is ambiguous.
const MIN_REV_LEN: usize = 7;

/// A full SHA-1 object name.
const MAX_REV_LEN: usize = 40;

/// The revision to stamp into the binary, or [`UNSTAMPED_REV`].
fn build_rev() -> String {
    let raw = std::env::var(BUILD_REV_ENV).unwrap_or_default();
    stamp(raw.trim()).to_owned()
}

/// `value` if it names an object, else [`UNSTAMPED_REV`].
fn stamp(value: &str) -> &str {
    if is_object_name(value) {
        value
    } else {
        UNSTAMPED_REV
    }
}

/// Is `value` an object name, with an optional dirty marker?
///
/// The value is printed into a `cargo:rustc-env` directive, and cargo
/// reads those one line at a time. A value that holds a newline would
/// add directives of its own, such as a link argument, to every build
/// that trusts the variable. Hex digits and one known suffix keep that
/// closed, and the length bound keeps an over-long value out of the
/// binary.
fn is_object_name(value: &str) -> bool {
    let core = value.strip_suffix(DIRTY_SUFFIX).unwrap_or(value);
    (MIN_REV_LEN..=MAX_REV_LEN).contains(&core.len())
        && core.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{build_rev, is_object_name, stamp, UNSTAMPED_REV};

    #[test]
    fn accepts_an_abbreviated_or_full_object_name() {
        assert_eq!(stamp("3a40bd9"), "3a40bd9");
        assert_eq!(stamp("3a40bd9f"), "3a40bd9f");
        let full = "a".repeat(40);
        assert_eq!(stamp(&full), full);
    }

    #[test]
    fn accepts_the_dirty_marker() {
        assert_eq!(stamp("3a40bd9-dirty"), "3a40bd9-dirty");
        assert!(is_object_name(&format!("{}-dirty", "b".repeat(40))));
    }

    /// A build the driver did not stamp must say so. Naming a commit
    /// that resolves in no clone is the defect this replaces.
    #[test]
    fn an_absent_value_gives_the_marker() {
        assert_eq!(stamp(""), UNSTAMPED_REV);
    }

    #[test]
    fn rejects_a_value_that_does_not_name_an_object() {
        for bad in [
            "main",
            "HEAD",
            "3a40bd",              // one digit short
            "3a40bd9g",            // not hex
            "3a40bd9-dirty-dirty", // one suffix only
            "3a40bd9 ",            // a space is not a digit
            "3a40bd9-clean",
        ] {
            assert_eq!(stamp(bad), UNSTAMPED_REV, "accepted {bad}");
        }
        let too_long = "c".repeat(41);
        assert_eq!(stamp(&too_long), UNSTAMPED_REV);
    }

    /// cargo reads build-script output one line at a time, so a value
    /// holding a newline would add directives of its own.
    #[test]
    fn rejects_a_value_that_could_add_a_cargo_directive() {
        for bad in [
            "3a40bd9\ncargo:rustc-link-arg=-zexecstack",
            "3a40bd9\rcargo:rustc-env=PATH=/tmp",
            "3a40bd9\u{0}",
        ] {
            assert_eq!(stamp(bad), UNSTAMPED_REV, "accepted {bad:?}");
        }
    }

    /// Whatever the environment holds, the stamp is an object name or
    /// the marker. Nothing else can reach the version string.
    #[test]
    fn the_stamp_is_a_name_or_the_marker() {
        let rev = build_rev();
        assert!(
            is_object_name(&rev) || rev == UNSTAMPED_REV,
            "unexpected stamp: {rev:?}"
        );
    }
}
