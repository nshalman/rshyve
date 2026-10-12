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

/// Records each record as its message and key=value pairs, so a test
/// can see what was logged.
#[derive(Clone, Default)]
pub(crate) struct RecordingDrain(pub(crate) Arc<std::sync::Mutex<Vec<String>>>);

impl slog::Drain for RecordingDrain {
    type Ok = ();
    type Err = slog::Never;

    fn log(
        &self,
        record: &slog::Record<'_>,
        _values: &slog::OwnedKVList,
    ) -> Result<(), slog::Never> {
        let mut line = Line(record.msg().to_string());
        let _ = slog::KV::serialize(&record.kv(), record, &mut line);
        self.0.lock().unwrap().push(line.0);
        Ok(())
    }
}

struct Line(String);

impl slog::Serializer for Line {
    fn emit_arguments(
        &mut self,
        key: slog::Key,
        val: &std::fmt::Arguments<'_>,
    ) -> slog::Result {
        self.0.push_str(&format!(" {key}={val}"));
        Ok(())
    }
}
