// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared utilities for VMM API crates (bhyve-api, viona-api).

use std::io::{Error, Result};
use std::sync::atomic::{AtomicI64, Ordering};

/// Thread-safe, one-shot cache for a kernel API version query.
///
/// Stores the result of a version ioctl (or its error) in an [`AtomicI64`] so
/// that subsequent calls avoid the syscall overhead.  The encoding is:
///
/// - `0`  -- not yet queried
/// - `>0` -- cached version (fits in `u32`)
/// - `<0` -- negated `errno` from a failed query
pub struct CachedVersion(AtomicI64);

impl CachedVersion {
    /// Create a new, uninitialized cache.
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }

    /// Return the cached version, or call `query` exactly once to populate it.
    ///
    /// Concurrent callers may race to fill the cache; only one value wins via
    /// `compare_exchange`, and all callers see the same result afterwards.
    pub fn get_or_init(&self, query: impl FnOnce() -> Result<u32>) -> Result<u32> {
        if self.0.load(Ordering::Acquire) == 0 {
            let newval = match query() {
                Ok(x) => i64::from(x),
                Err(e) => {
                    -i64::from(e.raw_os_error().unwrap_or(libc::ENOENT))
                }
            };
            // CAS failure means another thread already cached the value
            let _ = self.0.compare_exchange(
                0,
                newval,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }

        match self.0.load(Ordering::Acquire) {
            0 => {
                panic!("expected version cache to be initialized")
            }
            x if x < 0 => Err(Error::from_raw_os_error(-x as i32)),
            y => {
                assert!(y < i64::from(u32::MAX));
                Ok(y as u32)
            }
        }
    }
}
