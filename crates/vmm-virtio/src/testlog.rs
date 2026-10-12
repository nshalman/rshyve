// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A log for tests that counts its records.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Counts records, so a test can show a guest cannot drive the log.
pub(crate) struct CountingDrain(pub(crate) Arc<AtomicUsize>);

impl slog::Drain for CountingDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        _record: &slog::Record<'_>,
        _values: &slog::OwnedKVList,
    ) -> Result<(), slog::Never> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
