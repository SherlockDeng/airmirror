//! NTP timestamps for AirPlay mirror timing (matches doubletake / UxPlay conventions).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

static APP_START: OnceLock<Instant> = OnceLock::new();

fn app_start() -> &'static Instant {
    APP_START.get_or_init(Instant::now)
}

fn boot_elapsed() -> Duration {
    app_start().elapsed()
}

fn to_ntp_fixed_point(elapsed: Duration, epoch_offset_secs: u64) -> u64 {
    let sec = elapsed.as_secs() + epoch_offset_secs;
    let frac = ((elapsed.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (sec << 32) | frac
}

/// Boot-relative NTP (no epoch). Retained for reference/testing only.
pub fn ntp_boot_relative() -> u64 {
    to_ntp_fixed_point(boot_elapsed(), 0)
}

/// Boot-relative + NTP epoch — timing UDP replies and audio sync packets.
///
/// Delegates to the single shared clock in `rotten_core::ntp` so that timing
/// replies, audio sync, and video frame headers all share one reference
/// instant and one epoch (see `rotten_core::ntp` for why this matters).
pub fn ntp_boot_with_epoch() -> u64 {
    rotten_core::ntp::ntp_epoch_now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_timestamp_is_ahead_of_boot_relative() {
        rotten_core::ntp::init_session_clock();
        let boot = rotten_core::ntp::ntp_boot_relative_with_bias(std::time::Duration::ZERO);
        let epoch = ntp_boot_with_epoch();
        assert!(epoch > boot);
        assert!(epoch - boot >= NTP_EPOCH_OFFSET << 32);
    }
}
