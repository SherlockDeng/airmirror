use rotten_core::config::HwAccel;
use rotten_core::debug_log::agent_log;
use rotten_core::error::{Result, RottenError};
use tracing::warn;

use crate::ffmpeg_encoder::FfmpegNvencEncoder;
use crate::mirror_packet::nal_type_summary;

#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
use openh264::OpenH264API;
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, Level, UsageType};
#[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
use openh264::formats::{RgbSliceU8, YUVBuffer};

#[cfg(feature = "software-encode-source")]
fn create_openh264_api() -> Result<OpenH264API> {
    Ok(OpenH264API::from_source())
}

#[cfg(feature = "software-encode-dll")]
fn create_openh264_api() -> Result<OpenH264API> {
    use std::path::PathBuf;

    const DLL_NAME: &str = "openh264-2.6.0-win64.dll";
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(DLL_NAME));
        }
    }
    candidates.push(PathBuf::from(DLL_NAME));

    for path in candidates {
        if !path.exists() {
            continue;
        }
        // #region agent log
        agent_log(
            "encoder.rs:create_openh264_api",
            "loading openh264 dll",
            "H18",
            serde_json::json!({ "path": path.to_string_lossy() }),
        );
        // #endregion
        return OpenH264API::from_blob_path(&path)
            .map_err(|e| RottenError::Video(format!("openh264 dll {}: {e}", path.display())));
    }

    Err(RottenError::Video(format!(
        "missing {DLL_NAME} next to rottingapple.exe — copy it from the build output or https://www.openh264.org/"
    )))
}

/// Hardware encoder preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwEncoderKind {
    Software,
    Nvenc,
    Vaapi,
}

impl HwEncoderKind {
    pub fn resolve(pref: HwAccel) -> Self {
        match pref {
            HwAccel::Nvenc => Self::Nvenc,
            HwAccel::Vaapi => Self::Vaapi,
            HwAccel::Auto => Self::Nvenc,
            HwAccel::None => Self::Software,
        }
    }
}

/// A single H.264 encoded frame with timing metadata.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts_us: u64,
    pub encode_duration_us: u64,
    pub is_keyframe: bool,
    pub coded_width: u32,
    pub coded_height: u32,
    pub display_width: u32,
    pub display_height: u32,
}

impl EncodedFrame {
    pub fn is_keyframe_data(data: &[u8]) -> bool {
        nal_type_summary(data).iter().any(|nal_type| *nal_type == 5)
    }
}

const MAX_STREAM_WIDTH: u32 = 4096;
const MAX_STREAM_HEIGHT: u32 = 2176;

/// Build stamp for debug sessions; bump when verifying a new Windows binary.
pub const ENCODER_BUILD_ID: &str = rotten_core::debug_log::DEBUG_BUILD_ID;

/// Round down to a multiple of 16 (H.264 macroblock grid).
fn align16(v: u32) -> u32 {
    v & !15
}

/// Round up to a multiple of 16 (1080p → 1088 coded lines with bottom crop/pad).
fn align16_ceil(v: u32) -> u32 {
    v.div_ceil(16) * 16
}

pub fn fit_stream_dims(width: u32, height: u32) -> (u32, u32) {
    let w = align16(width);
    let h = align16_ceil(height);
    if w == 0 || h == 0 {
        return (16, 16);
    }
    if w <= MAX_STREAM_WIDTH && h <= MAX_STREAM_HEIGHT {
        return (w, h);
    }
    let scale = (MAX_STREAM_WIDTH as f64 / w as f64).min(MAX_STREAM_HEIGHT as f64 / h as f64);
    let nw = align16((((w as f64) * scale) as u32).max(16));
    let nh = align16_ceil((((h as f64) * scale) as u32).max(16));
    (nw, nh)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Level {
    Level4_0,
    Level4_2,
    Level5_0,
    Level5_1,
    Level5_2,
}

impl H264Level {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Level4_0 => "4.0",
            Self::Level4_2 => "4.2",
            Self::Level5_0 => "5.0",
            Self::Level5_1 => "5.1",
            Self::Level5_2 => "5.2",
        }
    }

    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    const fn to_openh264(self) -> Level {
        match self {
            Self::Level4_0 => Level::Level_4_0,
            Self::Level4_2 => Level::Level_4_2,
            Self::Level5_0 => Level::Level_5_0,
            Self::Level5_1 => Level::Level_5_1,
            Self::Level5_2 => Level::Level_5_2,
        }
    }
}

pub fn h264_level_for(width: u32, height: u32, fps: u32) -> H264Level {
    let macroblocks = width.div_ceil(16) as u64 * height.div_ceil(16) as u64;
    let macroblocks_per_second = macroblocks * fps.max(1) as u64;

    if macroblocks <= 8_704 && macroblocks_per_second <= 245_760 {
        H264Level::Level4_0
    } else if macroblocks <= 8_704 && macroblocks_per_second <= 522_240 {
        H264Level::Level4_2
    } else if macroblocks <= 22_080 && macroblocks_per_second <= 589_824 {
        H264Level::Level5_0
    } else if macroblocks <= 36_864 && macroblocks_per_second <= 983_040 {
        H264Level::Level5_1
    } else {
        H264Level::Level5_2
    }
}

pub fn downscale_rgba(rgba: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let src_w = src_w as usize;
    let src_h = src_h as usize;
    let dst_w = dst_w as usize;
    let dst_h = dst_h as usize;
    let mut out = vec![0u8; dst_w * dst_h * 4];
    for y in 0..dst_h {
        let sy = y * src_h / dst_h;
        for x in 0..dst_w {
            let sx = x * src_w / dst_w;
            let src_i = (sy * src_w + sx) * 4;
            let dst_i = (y * dst_w + x) * 4;
            if src_i + 3 < rgba.len() {
                out[dst_i..dst_i + 4].copy_from_slice(&rgba[src_i..src_i + 4]);
            }
        }
    }
    out
}

/// Pad or scale RGBA to the coded picture size (pad bottom rows instead of downscaling height).
pub fn fit_rgba_to_coded(
    rgba: &[u8],
    display_w: u32,
    display_h: u32,
    coded_w: u32,
    coded_h: u32,
) -> Vec<u8> {
    if coded_w == display_w && coded_h == display_h {
        return rgba.to_vec();
    }
    if coded_w == display_w
        && coded_h > display_h
        && rgba.len() == (display_w as usize) * (display_h as usize) * 4
    {
        let mut out = Vec::with_capacity((coded_w as usize) * (coded_h as usize) * 4);
        out.extend_from_slice(rgba);
        let pad_pixels = ((coded_h - display_h) * coded_w * 4) as usize;
        out.extend(vec![0u8; pad_pixels]);
        let base = (display_w as usize) * (display_h as usize) * 4;
        for i in (base..out.len()).step_by(4) {
            out[i + 3] = 255;
        }
        return out;
    }
    downscale_rgba(rgba, display_w, display_h, coded_w, coded_h)
}

/// H.264 encoder trait.
pub trait EncoderTrait: Send {
    fn encode(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        pts_us: u64,
    ) -> Result<Option<EncodedFrame>>;
    fn force_keyframe(&mut self);
    fn kind(&self) -> HwEncoderKind;
}

/// Software H.264 encoder (OpenH264).
pub struct SoftwareEncoder {
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    encoder: Encoder,
    rgb_buf: Vec<u8>,
    width: u32,
    height: u32,
    frame_count: u64,
    force_idr: bool,
    bitrate_kbps: u32,
}

impl SoftwareEncoder {
    #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
    pub fn new(width: u32, height: u32, bitrate_kbps: u32, fps: u32) -> Result<Self> {
        #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
        let encoder = {
            let bps = (bitrate_kbps.max(500) as u32) * 1000;
            let config = EncoderConfig::new()
                .bitrate(BitRate::from_bps(bps))
                .usage_type(UsageType::ScreenContentRealTime)
                .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
                .level(h264_level_for(width, height, fps).to_openh264())
                .num_threads(if width >= 2560 { 4 } else { 2 })
                // Legacy AirPlay receivers can reset the data channel when a
                // single NAL exceeds their parser limit. Size-limited slices
                // keep 1080p keyframes below that boundary.
                .max_slice_len(48_000);
            let api = create_openh264_api()?;
            Encoder::with_api_config(api, config)
                .map_err(|e| RottenError::Video(format!("openh264 encoder init: {e}")))?
        };

        let enc = Self {
            #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
            encoder,
            rgb_buf: Vec::new(),
            width,
            height,
            frame_count: 0,
            force_idr: true,
            bitrate_kbps,
        };
        // #region agent log
        agent_log(
            "encoder.rs:new",
            "openh264 encoder initialized",
            "H8",
            serde_json::json!({
                "width": width,
                "height": height,
                "fps": fps,
                "level": h264_level_for(width, height, fps).label(),
                "bitrateKbps": bitrate_kbps,
                "buildId": ENCODER_BUILD_ID,
            }),
        );
        // #endregion
        Ok(enc)
    }

    #[cfg(not(any(feature = "software-encode-source", feature = "software-encode-dll")))]
    pub fn new(_width: u32, _height: u32, _bitrate_kbps: u32, _fps: u32) -> Result<Self> {
        Err(RottenError::Video(
            "software encoder not enabled (rebuild with encode-source or encode-dll)".into(),
        ))
    }

    pub fn from_hw_pref(
        width: u32,
        height: u32,
        bitrate_kbps: u32,
        pref: HwAccel,
        fps: u32,
    ) -> Result<Box<dyn EncoderTrait>> {
        let kind = HwEncoderKind::resolve(pref);
        match kind {
            HwEncoderKind::Nvenc => {
                match FfmpegNvencEncoder::new(width, height, bitrate_kbps, fps) {
                    Ok(encoder) => {
                        tracing::info!(width, height, fps, "using NVENC hardware encoder");
                        return Ok(Box::new(encoder));
                    }
                    Err(error) if pref == HwAccel::Auto => {
                        warn!(%error, "NVENC unavailable; falling back to OpenH264");
                    }
                    Err(error) => return Err(error),
                }
            }
            HwEncoderKind::Vaapi => {
                return Err(RottenError::Video(
                    "VAAPI hardware encoding is not implemented yet; use --hwaccel none or auto"
                        .into(),
                ));
            }
            HwEncoderKind::Software => {}
        }
        Ok(Box::new(Self::new(width, height, bitrate_kbps, fps)?))
    }

    /// H.264 needs even width/height; do not macroblock-align here (fit_stream_dims handles that).
    fn even_dim(v: u32) -> u32 {
        v & !1
    }

    fn fill_rgb(&mut self, rgba: &[u8], width: usize, height: usize) {
        let pixels = width * height;
        self.rgb_buf.resize(pixels * 3, 0);
        for i in 0..pixels {
            let src = i * 4;
            let dst = i * 3;
            if src + 3 < rgba.len() {
                self.rgb_buf[dst] = rgba[src];
                self.rgb_buf[dst + 1] = rgba[src + 1];
                self.rgb_buf[dst + 2] = rgba[src + 2];
            }
        }
    }
}

impl EncoderTrait for SoftwareEncoder {
    fn encode(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        pts_us: u64,
    ) -> Result<Option<EncodedFrame>> {
        let display_w = Self::even_dim(width);
        let display_h = Self::even_dim(height);
        if display_w == 0 || display_h == 0 {
            return Ok(None);
        }

        let (coded_w, coded_h) = fit_stream_dims(display_w, display_h);

        if coded_w != self.width || coded_h != self.height {
            self.width = coded_w;
            self.height = coded_h;
            self.force_idr = true;
        }

        self.frame_count += 1;

        // #region agent log
        if self.frame_count == 1 {
            agent_log(
                "encoder.rs:encode",
                "encode started",
                "H9",
                serde_json::json!({
                    "displayW": display_w,
                    "displayH": display_h,
                    "codedW": coded_w,
                    "codedH": coded_h,
                    "codedWMod16": coded_w % 16,
                    "codedHMod16": coded_h % 16,
                    "rgbaBytes": rgba.len(),
                    "frameCount": self.frame_count,
                }),
            );
        }
        // #endregion

        let encode_start = std::time::Instant::now();

        #[cfg(any(feature = "software-encode-source", feature = "software-encode-dll"))]
        {
            let (fit_mode, work_rgba): (&str, Vec<u8>) =
                if coded_w != display_w || coded_h != display_h {
                    let padded = fit_rgba_to_coded(rgba, display_w, display_h, coded_w, coded_h);
                    let mode = if coded_w == display_w && coded_h > display_h {
                        "pad-bottom"
                    } else {
                        "downscale"
                    };
                    // #region agent log
                    if self.frame_count == 1 {
                        agent_log(
                            "encoder.rs:encode",
                            "fitting rgba to coded size",
                            "H111",
                            serde_json::json!({
                                "mode": mode,
                                "fromW": display_w,
                                "fromH": display_h,
                                "toW": coded_w,
                                "toH": coded_h,
                            }),
                        );
                    }
                    // #endregion
                    (mode, padded)
                } else {
                    ("none", rgba.to_vec())
                };
            let rgba_slice: &[u8] = &work_rgba;
            let _ = fit_mode;

            let w = coded_w as usize;
            let h = coded_h as usize;
            self.fill_rgb(rgba_slice, w, h);
            let rgb = RgbSliceU8::new(&self.rgb_buf, (w, h));
            let yuv = YUVBuffer::from_rgb8_source(rgb);

            if self.force_idr {
                self.encoder.force_intra_frame();
                self.force_idr = false;
            }

            let bitstream = self
                .encoder
                .encode(&yuv)
                .map_err(|e| RottenError::Video(format!("openh264 encode: {e}")))?;
            let data = bitstream.to_vec();
            if data.is_empty() {
                return Ok(None);
            }

            let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);

            let encode_duration = encode_start.elapsed();
            let duration_ms = encode_duration.as_millis();

            // #region agent log
            if self.frame_count <= 3 || self.frame_count % 30 == 0 {
                agent_log(
                    "encoder.rs:encode",
                    "openh264 frame encoded",
                    "H10",
                    serde_json::json!({
                        "frameCount": self.frame_count,
                        "h264Bytes": data.len(),
                        "keyframe": is_keyframe,
                        "codedW": coded_w,
                        "codedH": coded_h,
                        "durationMs": duration_ms,
                    }),
                );
            }
            // #endregion

            return Ok(Some(EncodedFrame {
                data,
                pts_us,
                encode_duration_us: encode_duration.as_micros() as u64,
                is_keyframe,
                coded_width: coded_w,
                coded_height: coded_h,
                display_width: display_w,
                display_height: display_h,
            }));
        }

        #[cfg(not(any(feature = "software-encode-source", feature = "software-encode-dll")))]
        {
            let _ = (rgba, pts_us);
            Err(RottenError::Video(
                "software encoder not enabled (rebuild with encode-source or encode-dll)".into(),
            ))
        }
    }

    fn force_keyframe(&mut self) {
        self.force_idr = true;
    }

    fn kind(&self) -> HwEncoderKind {
        HwEncoderKind::Software
    }
}

/// Deferred encoder init so OpenH264 setup runs only on a blocking thread.
pub struct LazyEncoder {
    inner: Option<Box<dyn EncoderTrait>>,
    init_width: u32,
    init_height: u32,
    bitrate_kbps: u32,
    fps: u32,
    hw_accel: HwAccel,
}

impl LazyEncoder {
    pub fn new(width: u32, height: u32, bitrate_kbps: u32, fps: u32, hw_accel: HwAccel) -> Self {
        let (coded_w, coded_h) = fit_stream_dims(width, height);
        Self {
            inner: None,
            init_width: coded_w,
            init_height: coded_h,
            bitrate_kbps,
            fps,
            hw_accel,
        }
    }

    fn ensure(&mut self) -> Result<&mut dyn EncoderTrait> {
        if self.inner.is_none() {
            // #region agent log
            agent_log(
                "encoder.rs:lazy",
                "lazy encoder init starting",
                "H13",
                serde_json::json!({
                    "codedW": self.init_width,
                    "codedH": self.init_height,
                    "fps": self.fps,
                    "level": h264_level_for(self.init_width, self.init_height, self.fps).label(),
                    "bitrateKbps": self.bitrate_kbps,
                    "buildId": ENCODER_BUILD_ID,
                }),
            );
            // #endregion
            let enc = SoftwareEncoder::from_hw_pref(
                self.init_width,
                self.init_height,
                self.bitrate_kbps,
                self.hw_accel,
                self.fps,
            )?;
            self.inner = Some(enc);
            // #region agent log
            agent_log(
                "encoder.rs:lazy",
                "lazy encoder init finished",
                "H13",
                serde_json::json!({
                    "codedW": self.init_width,
                    "codedH": self.init_height,
                    "fps": self.fps,
                    "buildId": ENCODER_BUILD_ID,
                }),
            );
            // #endregion
        }
        Ok(self.inner.as_mut().expect("lazy encoder").as_mut())
    }

    pub fn encode(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        pts_us: u64,
    ) -> Result<Option<EncodedFrame>> {
        let result = self.ensure()?.encode(rgba, width, height, pts_us);
        match result {
            Err(error) if self.hw_accel == HwAccel::Auto => {
                warn!(%error, "hardware encoder failed; falling back to OpenH264");
                self.hw_accel = HwAccel::None;
                self.inner = Some(SoftwareEncoder::from_hw_pref(
                    self.init_width,
                    self.init_height,
                    self.bitrate_kbps,
                    HwAccel::None,
                    self.fps,
                )?);
                self.ensure()?.encode(rgba, width, height, pts_us)
            }
            result => result,
        }
    }
}

pub fn auto_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    let pixels = width as u64 * height as u64 * fps as u64;
    ((pixels / 1000) as u32).clamp(2000, 40000)
}

#[cfg(test)]
mod tests {
    use super::{H264Level, HwAccel, SoftwareEncoder, fit_stream_dims, h264_level_for};
    use crate::SyntheticSource;
    use std::io::Write;

    #[test]
    fn ultrawide_fits_macroblock_grid() {
        let (w, h) = fit_stream_dims(3440, 1440);
        assert_eq!(w, 3440);
        assert_eq!(h, 1440);
        assert_eq!(w % 16, 0);
        assert_eq!(h % 16, 0);
    }

    #[test]
    fn hd1080_rounds_height_up_to_1088() {
        let (w, h) = fit_stream_dims(1920, 1080);
        assert_eq!(w, 1920);
        assert_eq!(h, 1088);
    }

    #[test]
    fn high_resolution_streams_fit_4k_canvas() {
        assert_eq!(fit_stream_dims(2560, 1440), (2560, 1440));
        assert_eq!(fit_stream_dims(3840, 2160), (3840, 2160));
    }

    #[test]
    fn h264_level_covers_2k_and_4k_high_refresh() {
        assert_eq!(h264_level_for(1920, 1088, 60), H264Level::Level4_2);
        assert_eq!(h264_level_for(2560, 1440, 60), H264Level::Level5_1);
        assert_eq!(h264_level_for(3440, 1440, 60), H264Level::Level5_2);
        assert_eq!(h264_level_for(3840, 2160, 30), H264Level::Level5_1);
        assert_eq!(h264_level_for(3840, 2160, 60), H264Level::Level5_2);
    }

    #[test]
    #[ignore = "manual diagnostic for a local H.264 dump"]
    fn dump_synthetic_h264_for_manual_inspection() {
        let path = std::env::temp_dir().join("airmirror-synthetic.h264");
        let mut source = SyntheticSource::new(1280, 720);
        let mut encoder =
            SoftwareEncoder::from_hw_pref(1280, 720, 2000, HwAccel::None, 30).expect("encoder");
        let mut output = std::fs::File::create(&path).expect("create dump");
        for index in 0..30 {
            let (rgba, width, height) = source.next_frame().expect("synthetic frame");
            if let Some(frame) = encoder
                .encode(&rgba, width, height, index * 100_000)
                .expect("encode")
            {
                output.write_all(&frame.data).expect("write dump");
            }
        }
        println!("{}", path.display());
    }
}
