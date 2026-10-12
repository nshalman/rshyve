// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IronRDP's `tracing` events, into the VMM's slog log.
//!
//! IronRDP reports a rejected login, a failed TLS handshake or a dropped
//! connection only through `tracing`, which nothing in the VMM listens
//! to. This subscriber, installed on the RDP server's thread alone,
//! turns each event at or above a level into a slog record: the message,
//! then its other fields as `name=value`. Spans are not tracked. Events
//! below INFO go out at INFO, marked with their level, because slog drops
//! lower records from a release build.

use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

pub(crate) struct SlogSubscriber {
    log: slog::Logger,
    /// The most verbose level forwarded.
    max: Level,
    next_span: AtomicU64,
}

impl SlogSubscriber {
    pub(crate) fn new(log: slog::Logger, max: Level) -> Self {
        Self {
            log,
            max,
            next_span: AtomicU64::new(1),
        }
    }
}

/// Collects an event's message and its other fields.
#[derive(Default)]
struct Message {
    text: String,
    fields: String,
}

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.text, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.text.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

impl Subscriber for SlogSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        // tracing orders levels by verbosity: TRACE is the greatest.
        *metadata.level() <= self.max
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut msg = Message::default();
        event.record(&mut msg);
        let text = format!("{}{}", msg.text, msg.fields);
        let target = event.metadata().target();
        match *event.metadata().level() {
            Level::ERROR => {
                slog::error!(self.log, "{}", text; "target" => target)
            }
            Level::WARN => {
                slog::warn!(self.log, "{}", text; "target" => target)
            }
            Level::INFO => {
                slog::info!(self.log, "{}", text; "target" => target)
            }
            // slog compiles records below INFO out of a release build.
            // These arrive only when an operator raised the level, so
            // they go out at INFO, tagged with the level they had.
            Level::DEBUG => {
                slog::info!(self.log, "[debug] {}", text; "target" => target)
            }
            Level::TRACE => {
                slog::info!(self.log, "[trace] {}", text; "target" => target)
            }
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A slog drain that keeps each record's level and message.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<(slog::Level, String)>>>);

    impl slog::Drain for Capture {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _: &slog::OwnedKVList,
        ) -> Result<(), slog::Never> {
            self.0
                .lock()
                .unwrap()
                .push((record.level(), record.msg().to_string()));
            Ok(())
        }
    }

    fn logged(max: Level, emit: impl FnOnce()) -> Vec<(slog::Level, String)> {
        let capture = Capture::default();
        let log = slog::Logger::root(capture.clone(), slog::o!());
        tracing::subscriber::with_default(SlogSubscriber::new(log, max), emit);
        let out = capture.0.lock().unwrap().clone();
        out
    }

    /// A failed login is a WARN in IronRDP and has to reach the log an
    /// operator reads, with its fields.
    #[test]
    fn warnings_and_errors_reach_slog() {
        let got = logged(Level::INFO, || {
            tracing::warn!("Credential validation rejected");
            tracing::error!(error = "client loop failure", "Connection error");
        });
        assert_eq!(
            got,
            [
                (
                    slog::Level::Warning,
                    "Credential validation rejected".to_string()
                ),
                (
                    slog::Level::Error,
                    "Connection error error=client loop failure".to_string()
                ),
            ]
        );
    }

    #[test]
    fn events_below_the_level_are_dropped() {
        let got = logged(Level::INFO, || {
            tracing::debug!("Client accepted");
            tracing::info!("listening");
        });
        assert_eq!(got, [(slog::Level::Info, "listening".to_string())]);
    }

    /// slog compiles records below INFO out of a release build, so a
    /// level raised past INFO would forward into nothing. Those events
    /// go out at INFO, tagged with the level IronRDP gave them.
    #[test]
    fn a_raised_level_reaches_a_release_build() {
        let got = logged(Level::TRACE, || {
            tracing::debug!("Client accepted");
            tracing::trace!("frame");
        });
        assert_eq!(
            got,
            [
                (slog::Level::Info, "[debug] Client accepted".to_string()),
                (slog::Level::Info, "[trace] frame".to_string()),
            ]
        );
    }
}
