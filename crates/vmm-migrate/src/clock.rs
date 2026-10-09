// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The skew between the two hosts' wall clocks.
//!
//! The time import counts the wall-clock gap between the source's
//! export and the destination's import as time the guest was paused, so
//! a destination clock ahead by N seconds moves the guest clock N
//! seconds forward. Propolis assumes synced clocks. Here the source
//! sends its clock in the preamble, and the destination measures the
//! skew before a single page moves.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::codec::MigrateError;

/// Above this the skew is logged as a warning. A synced pair is within
/// a few milliseconds, plus the time the preamble takes to arrive.
pub const SKEW_WARN_MS: i64 = 1_000;

/// Above this the destination refuses the migration: NTP is broken on
/// one of the hosts, and the guest clock would jump by the skew.
pub const SKEW_MAX_MS: i64 = 60_000;

/// This host's wall clock, in ns since the epoch.
pub fn now_ns() -> Result<u64, MigrateError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .ok_or_else(|| MigrateError::Io("the wall clock is before 1970".into()))
}

/// The skew in ms, positive when this host is ahead of the source.
///
/// The source's value is the peer's choice, so the arithmetic is wide
/// enough for any `u64` pair.
pub fn check_skew(
    source_ns: u64,
    local_ns: u64,
    log: &slog::Logger,
) -> Result<i64, MigrateError> {
    let skew_ms = (i128::from(local_ns) - i128::from(source_ns)) / 1_000_000;
    let skew_ms = i64::try_from(skew_ms).unwrap_or(i64::MAX);
    let ahead = if skew_ms >= 0 { "ahead of" } else { "behind" };
    if skew_ms.unsigned_abs() > SKEW_MAX_MS.unsigned_abs() {
        return Err(MigrateError::PreambleMismatch(format!(
            "this host's clock is {} ms {ahead} the source's; the limit is \
             {SKEW_MAX_MS} ms, so sync both hosts first",
            skew_ms.unsigned_abs()
        )));
    }
    if skew_ms.unsigned_abs() > SKEW_WARN_MS.unsigned_abs() {
        slog::warn!(log, "the host clocks disagree; the guest clock moves \
            by the skew"; "skew_ms" => skew_ms);
    } else {
        slog::info!(log, "host clock skew"; "skew_ms" => skew_ms);
    }
    Ok(skew_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn log() -> slog::Logger {
        slog::Logger::root(slog::Discard, slog::o!())
    }

    #[test]
    fn synced_hosts_show_a_small_skew() {
        let now = 1_700_000_000 * S;
        assert_eq!(check_skew(now, now + 3_000_000, &log()).expect("ok"), 3);
        assert_eq!(check_skew(now + 3_000_000, now, &log()).expect("ok"), -3);
    }

    #[test]
    fn a_skew_past_the_limit_is_refused_either_way() {
        let now = 1_700_000_000 * S;
        let limit = SKEW_MAX_MS as u64 * 1_000_000;
        check_skew(now, now + limit, &log()).expect("at the limit");
        let error = check_skew(now, now + limit + 1_000_000, &log())
            .expect_err("ahead");
        assert!(error.to_string().contains("ahead of"), "{error}");
        let error = check_skew(now + limit + 1_000_000, now, &log())
            .expect_err("behind");
        assert!(error.to_string().contains("behind"), "{error}");
    }

    #[test]
    fn a_peer_clock_at_an_extreme_is_refused_without_overflow() {
        let now = 1_700_000_000 * S;
        check_skew(0, now, &log()).expect_err("zero");
        check_skew(u64::MAX, now, &log()).expect_err("u64::MAX");
        check_skew(u64::MAX, 0, &log()).expect_err("both extremes");
    }
}
