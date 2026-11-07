mod backend;
#[cfg(target_os = "windows")]
pub mod dxgi;
#[cfg(target_os = "linux")]
mod linux;
pub mod virtual_display;

pub use backend::{
    CaptureBackend, CaptureFrame, DisplayInfo, create_capture_backend, list_displays,
};
