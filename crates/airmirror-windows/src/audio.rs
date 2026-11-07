use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioCaptureConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_samples: usize,
}

impl Default for AudioCaptureConfig {
    fn default() -> Self {
        Self {
            sample_rate: 44_100,
            channels: 2,
            frame_samples: 352,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub timestamp_ns: u64,
    pub samples: Vec<i16>,
}
