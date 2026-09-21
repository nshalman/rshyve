// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The source revision that a VMM binary reports as its version.
//
// The revision comes from the environment, never from git: the illumos
// build host holds no clone, and a git there answers for the repository
// the build directory sits under, or fails.
//
// `bin/firehyve/build.rs` and `bin/rshyve/build.rs` `include!` this
// file, so plain `//` comments only: an inner doc comment is an error
// at the point of an `include!`. lib.rs compiles it again under
// `cfg(test)` so the rules below are tested.

const BUILD_REV_ENV: &str = "TRITON_BUILD_REV";

/// A wrong commit is worse than no commit.
const UNSTAMPED_REV: &str = "unstamped";

const DIRTY_SUFFIX: &str = "-dirty";

/// git abbreviates to 7 digits and lengthens the name when 7 is
/// ambiguous. 40 is a full SHA-1 name.
const MIN_REV_LEN: usize = 7;
const MAX_REV_LEN: usize = 40;

fn build_rev() -> String {
    let raw = std::env::var(BUILD_REV_ENV).unwrap_or_default();
    stamp(raw.trim()).to_owned()
}

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
/// reads those one line at a time, so a value holding a newline could
/// add directives of its own to every build that trusts the variable.
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

    /// Nothing the environment holds can reach the version string
    /// except an object name or the marker.
    #[test]
    fn the_stamp_is_a_name_or_the_marker() {
        let rev = build_rev();
        assert!(
            is_object_name(&rev) || rev == UNSTAMPED_REV,
            "unexpected stamp: {rev:?}"
        );
    }
}
