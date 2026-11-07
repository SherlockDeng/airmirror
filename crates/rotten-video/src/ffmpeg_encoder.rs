use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use rotten_core::error::{Result, RottenError};
use rotten_core::hidden_command;

use crate::encoder::{EncodedFrame, EncoderTrait, HwEncoderKind};
use crate::mirror_packet::nal_type_summary;

const OUTPUT_QUEUE: usize = 4;
const FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone)]
struct FrameMetadata {
    pts_us: u64,
    width: u32,
    height: u32,
}

/// FFmpeg-backed NVENC encoder. FFmpeg performs RGB-to-YUV conversion while
/// NVENC performs the expensive H.264 encoding on the GPU.
pub struct FfmpegNvencEncoder {
    child: Child,
    stdin: std::process::ChildStdin,
    output: Receiver<Vec<u8>>,
    reader: Option<std::thread::JoinHandle<()>>,
    pending: VecDeque<FrameMetadata>,
    width: u32,
    height: u32,
    frames: u64,
}

impl FfmpegNvencEncoder {
    pub fn available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            let Ok(output) = hidden_command("ffmpeg")
                .args(["-hide_banner", "-encoders"])
                .output()
            else {
                return false;
            };
            String::from_utf8_lossy(&output.stdout).contains("h264_nvenc")
        })
    }

    pub fn new(width: u32, height: u32, bitrate_kbps: u32, fps: u32) -> Result<Self> {
        if !Self::available() {
            return Err(RottenError::Video(
                "ffmpeg with h264_nvenc is not available".into(),
            ));
        }

        let bitrate = bitrate_kbps.max(500).saturating_mul(1000);
        let gop = fps.max(1).saturating_mul(2);
        let mut command = hidden_command("ffmpeg");
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-s",
            &format!("{width}x{height}"),
            "-r",
            &fps.max(1).to_string(),
            "-i",
            "pipe:0",
            "-an",
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p4",
            "-tune",
            "ll",
            "-profile:v",
            "main",
            "-rc",
            "cbr",
            "-cbr",
            "1",
            "-b:v",
            &bitrate.to_string(),
            "-maxrate",
            &bitrate.to_string(),
            "-bufsize",
            &bitrate.saturating_mul(2).to_string(),
            "-g",
            &gop.to_string(),
            "-bf",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-bsf:v",
            "h264_metadata=aud=insert",
            "-flush_packets",
            "1",
            "-fflags",
            "+flush_packets",
            "-avioflags",
            "direct",
            "-fps_mode",
            "passthrough",
            "-max_delay",
            "0",
            "-delay",
            "0",
            "-f",
            "h264",
            "pipe:1",
        ]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|error| RottenError::Video(format!("start ffmpeg NVENC: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RottenError::Video("ffmpeg NVENC stdin unavailable".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| RottenError::Video("ffmpeg NVENC stdout unavailable".into()))?;
        let (sender, output) = sync_channel(OUTPUT_QUEUE);
        let reader = std::thread::Builder::new()
            .name("airmirror-nvenc-output".into())
            .spawn(move || read_access_units(&mut stdout, sender))
            .map_err(|error| RottenError::Video(format!("start NVENC reader: {error}")))?;

        Ok(Self {
            child,
            stdin,
            output,
            reader: Some(reader),
            pending: VecDeque::with_capacity(OUTPUT_QUEUE),
            width,
            height,
            frames: 0,
        })
    }
}

impl EncoderTrait for FfmpegNvencEncoder {
    fn encode(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        pts_us: u64,
    ) -> Result<Option<EncodedFrame>> {
        if width != self.width
            || height != self.height
            || rgba.len() != width as usize * height as usize * 4
        {
            return Err(RottenError::Video(format!(
                "NVENC frame shape changed: expected {}x{}, got {width}x{height}, {} bytes",
                self.width,
                self.height,
                rgba.len()
            )));
        }

        let encode_start = std::time::Instant::now();
        self.stdin
            .write_all(rgba)
            .and_then(|_| self.stdin.flush())
            .map_err(|error| RottenError::Video(format!("write NVENC frame: {error}")))?;
        self.pending.push_back(FrameMetadata {
            pts_us,
            width,
            height,
        });
        self.frames += 1;

        // The AUD-delimited parser needs the next access unit to close the
        // previous one. First frame primes the pipeline and returns None.
        if self.frames <= 2 {
            return Ok(None);
        }

        let data = self
            .output
            .recv_timeout(FRAME_TIMEOUT)
            .map_err(|error| RottenError::Video(format!("read NVENC output: {error}")))?;
        let metadata = self
            .pending
            .pop_front()
            .ok_or_else(|| RottenError::Video("missing NVENC frame metadata".into()))?;
        let is_keyframe = nal_type_summary(&data)
            .iter()
            .any(|nal_type| *nal_type == 5);
        Ok(Some(EncodedFrame {
            data,
            pts_us: metadata.pts_us,
            encode_duration_us: encode_start.elapsed().as_micros() as u64,
            is_keyframe,
            coded_width: metadata.width,
            coded_height: metadata.height,
            display_width: metadata.width,
            display_height: metadata.height,
        }))
    }

    fn force_keyframe(&mut self) {}

    fn kind(&self) -> HwEncoderKind {
        HwEncoderKind::Nvenc
    }
}

impl Drop for FfmpegNvencEncoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub(crate) fn read_access_units(stdout: &mut impl Read, sender: SyncSender<Vec<u8>>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);

        loop {
            let Some((_, second)) = first_two_aud_positions(&buffer) else {
                break;
            };
            let access_unit: Vec<u8> = buffer.drain(..second).collect();
            if sender.send(access_unit).is_err() {
                return;
            }
        }
    }
}

fn first_two_aud_positions(data: &[u8]) -> Option<(usize, usize)> {
    let mut first = None;
    let mut index = 0;
    while index + 3 < data.len() {
        let start_len = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        if data.get(index + start_len).copied() == Some(9) {
            if first.is_none() {
                first = Some(index);
            } else {
                return Some((first.unwrap(), index));
            }
        }
        index += start_len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::first_two_aud_positions;

    #[test]
    fn finds_aud_delimited_access_units() {
        let bytes = [
            0, 0, 0, 1, 9, 0xf0, 0, 0, 0, 1, 0x65, 1, 2, 0, 0, 0, 1, 9, 0xf0,
        ];
        assert_eq!(first_two_aud_positions(&bytes), Some((0, 13)));
    }
}
