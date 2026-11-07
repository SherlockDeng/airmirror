use serde::{Deserialize, Serialize};

use airmirror_core::{VideoCodec, VideoProfile};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoEncoderConfig {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub bitrate_kbps: u32,
    pub codec: VideoCodec,
    pub profile: VideoProfile,
}

impl Default for VideoEncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            frames_per_second: 30,
            bitrate_kbps: 4_000,
            codec: VideoCodec::H264,
            profile: VideoProfile::Main,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideoFrame {
    pub timestamp_ns: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}
