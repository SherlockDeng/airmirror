use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Discovering,
    Probing,
    Pairing,
    FairPlaySetup,
    Negotiating,
    Streaming,
    Reconnecting,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    H264,
    Hevc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoProfile {
    Baseline,
    Main,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorOptions {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub bitrate_kbps: u32,
    pub codec: VideoCodec,
    pub profile: VideoProfile,
    pub mirror_audio: bool,
}

impl Default for MirrorOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            frames_per_second: 30,
            bitrate_kbps: 4_000,
            codec: VideoCodec::H264,
            profile: VideoProfile::Main,
            mirror_audio: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    StateChanged { state: SessionState },
    PairingRequired { pin_length: u8 },
    StreamingStarted,
    StreamError { message: String },
    Stopped,
}
