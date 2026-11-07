use std::time::Duration;

pub const AUDIO_SAMPLE_RATE: u32 = 44_100;
pub const AUDIO_CHANNELS: usize = 2;
pub const AUDIO_FRAME_SAMPLES: usize = 352;
pub const VIDEO_PLAYOUT_LEAD_MS: u32 = 150;
pub const AUDIO_OFFSET_MIN_MS: i32 = -80;
pub const AUDIO_OFFSET_MAX_MS: i32 = 300;

#[derive(Debug, Clone)]
pub struct TimestampedPcmFrame {
    /// Start of the first sample on the shared boot-relative clock.
    pub captured_at: Duration,
    /// Interleaved stereo S16 samples, exactly [`AUDIO_FRAME_SAMPLES`] per channel.
    pub samples: Vec<i16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaSyncConfig {
    /// Positive values delay audio relative to video.
    pub audio_offset_ms: i32,
}

impl Default for MediaSyncConfig {
    fn default() -> Self {
        Self {
            audio_offset_ms: 10,
        }
    }
}

impl MediaSyncConfig {
    pub fn video_lead(self) -> Duration {
        Duration::from_millis(u64::from(VIDEO_PLAYOUT_LEAD_MS))
    }

    pub fn video_lead_samples(self) -> u32 {
        duration_to_samples(self.video_lead()) as u32
    }

    pub fn audio_latency(self) -> Duration {
        let milliseconds = (VIDEO_PLAYOUT_LEAD_MS as i32 + self.audio_offset_ms).clamp(
            VIDEO_PLAYOUT_LEAD_MS as i32 + AUDIO_OFFSET_MIN_MS,
            VIDEO_PLAYOUT_LEAD_MS as i32 + AUDIO_OFFSET_MAX_MS,
        );
        Duration::from_millis(milliseconds as u64)
    }

    pub fn audio_latency_samples(self) -> u32 {
        duration_to_samples(self.audio_latency()) as u32
    }
}

pub fn audio_frame_duration() -> Duration {
    duration_for_samples(AUDIO_FRAME_SAMPLES as u64)
}

pub fn duration_for_samples(samples: u64) -> Duration {
    Duration::from_nanos(
        samples
            .saturating_mul(1_000_000_000)
            .saturating_div(u64::from(AUDIO_SAMPLE_RATE)),
    )
}

pub fn duration_to_samples(duration: Duration) -> u64 {
    let samples = duration
        .as_nanos()
        .saturating_mul(u128::from(AUDIO_SAMPLE_RATE))
        .saturating_add(500_000_000)
        .saturating_div(1_000_000_000);
    samples.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::{AUDIO_FRAME_SAMPLES, MediaSyncConfig, audio_frame_duration, duration_to_samples};

    #[test]
    fn alac_frame_duration_matches_442_samples_per_second_pacing() {
        assert_eq!(audio_frame_duration().as_nanos(), 7_981_859);
        assert_eq!(
            duration_to_samples(audio_frame_duration()),
            AUDIO_FRAME_SAMPLES as u64
        );
    }

    #[test]
    fn default_audio_latency_centers_audio_inside_one_video_frame() {
        let sync = MediaSyncConfig::default();
        assert_eq!(
            sync.audio_latency(),
            sync.video_lead() + std::time::Duration::from_millis(10)
        );
        assert_eq!(sync.audio_latency_samples(), 7_056);
    }

    #[test]
    fn audio_offset_is_clamped_to_supported_range() {
        let early = MediaSyncConfig {
            audio_offset_ms: -10_000,
        };
        let late = MediaSyncConfig {
            audio_offset_ms: 10_000,
        };
        assert_eq!(early.audio_latency(), std::time::Duration::from_millis(70));
        assert_eq!(late.audio_latency(), std::time::Duration::from_millis(450));
    }
}
