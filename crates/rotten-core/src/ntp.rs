//! Shared boot-relative NTP clock for AirPlay mirror timing.
//!
//! All three timestamp sources — the timing-channel UDP replies, the audio
//! sync packets, and the video frame headers — MUST derive their timestamps
//! from this single clock. The receiver synchronizes its own clock to the
//! values we send on the timing channel, then schedules every video frame for
//! presentation at the NTP time carried in the frame header. If the frame
//! headers used a different reference instant or omitted the NTP epoch (as a
//! separate boot-relative clock would), every frame would resolve to a time
//! far in the past relative to the receiver's synced clock and be dropped,
//! producing a permanently black screen on a healthy connection.

use std::time::Duration;

#[cfg(not(target_os = "windows"))]
use std::{sync::OnceLock, time::Instant};

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

#[cfg(not(target_os = "windows"))]
static NTP_START: OnceLock<Instant> = OnceLock::new();

/// Pin the shared clock at mirror session start so timing, audio, and video
/// share one reference instant (call once when RTSP setup begins).
pub fn init_session_clock() {
    #[cfg(not(target_os = "windows"))]
    let _ = ntp_start();
}

#[cfg(not(target_os = "windows"))]
fn ntp_start() -> &'static Instant {
    NTP_START.get_or_init(Instant::now)
}

/// Time elapsed since system boot.
///
/// AirPlay video timestamps are boot-relative, not process-relative. Using the
/// process start makes every frame look minutes behind on a receiver that has
/// been powered on longer than the sender application.
pub fn boot_duration() -> Duration {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetTickCount64() -> u64;
        }
        // GetTickCount64 is monotonic and includes time spent suspended.
        unsafe { Duration::from_millis(GetTickCount64()) }
    }

    #[cfg(not(target_os = "windows"))]
    {
        ntp_start().elapsed()
    }
}

fn to_fixed_point(elapsed: Duration, epoch_offset_secs: u64) -> u64 {
    let sec = elapsed.as_secs() + epoch_offset_secs;
    let frac = ((elapsed.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (sec << 32) | frac
}

/// NTP fixed-point timestamp (shared boot reference + NTP epoch).
/// Used for timing-channel replies and audio sync packets.
pub fn ntp_epoch_now() -> u64 {
    ntp_epoch_at(boot_duration())
}

/// NTP fixed-point timestamp for a shared boot-relative media timestamp.
pub fn ntp_epoch_at(boot_time: Duration) -> u64 {
    to_fixed_point(boot_time, NTP_EPOCH_OFFSET)
}

/// NTP fixed-point timestamp with playout bias, on the epoch clock.
pub fn ntp_epoch_with_bias(bias: Duration) -> u64 {
    ntp_epoch_at(boot_duration() + bias)
}

/// Boot-relative NTP (no epoch) with optional playout bias.
/// Used for video frame headers — matches doubletake `ntpTimeNow()`.
pub fn ntp_boot_relative_with_bias(bias: Duration) -> u64 {
    ntp_boot_relative_at(boot_duration() + bias)
}

/// Boot-relative NTP timestamp at an exact shared media timestamp.
pub fn ntp_boot_relative_at(boot_time: Duration) -> u64 {
    to_fixed_point(boot_time, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_timestamp_is_ahead_of_boot_relative() {
        init_session_clock();
        let boot = ntp_boot_relative_with_bias(Duration::ZERO);
        let epoch = ntp_epoch_now();
        assert!(epoch > boot);
        assert!(epoch - boot >= NTP_EPOCH_OFFSET << 32);
    }

    #[test]
    fn bias_advances_timestamp() {
        init_session_clock();
        let base = ntp_boot_relative_with_bias(Duration::ZERO);
        let biased = ntp_boot_relative_with_bias(Duration::from_millis(100));
        assert!(biased >= base);
    }

    #[test]
    fn maps_exact_source_timestamps() {
        let source = Duration::from_millis(12_345);
        assert_eq!(ntp_boot_relative_at(source), to_fixed_point(source, 0));
        assert_eq!(
            ntp_epoch_at(source),
            to_fixed_point(source, NTP_EPOCH_OFFSET)
        );
    }
}
