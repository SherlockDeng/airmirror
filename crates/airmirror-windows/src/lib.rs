pub mod audio;
pub mod capture;
pub mod encoder;

pub use audio::{AudioCaptureConfig, AudioFrame};
pub use capture::{CaptureConfig, CapturedFrame, DisplayInfo};
pub use encoder::{EncodedVideoFrame, VideoEncoderConfig};

pub fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unsupported"
    }
}
