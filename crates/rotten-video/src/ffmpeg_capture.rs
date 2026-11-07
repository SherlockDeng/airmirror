use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};

use rotten_core::error::{Result, RottenError};

use crate::ffmpeg_encoder::read_access_units;

const OUTPUT_QUEUE: usize = 4;

/// FFmpeg Desktop Duplication capture piped directly into NVENC.
pub struct FfmpegDirectCapture {
    child: Child,
    output: Option<Receiver<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    width: u32,
    height: u32,
    fps: u32,
}

impl FfmpegDirectCapture {
    pub fn available() -> bool {
        let Ok(output) = Command::new("ffmpeg")
            .args(["-hide_banner", "-filters"])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout).contains("ddagrab")
            && crate::ffmpeg_encoder::FfmpegNvencEncoder::available()
    }

    pub fn start(
        output_index: u32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        native_size: bool,
    ) -> Result<Self> {
        if !Self::available() {
            return Err(RottenError::Capture(
                "ffmpeg ddagrab + h264_nvenc is not available".into(),
            ));
        }

        let fps = fps.max(1);
        let bitrate = bitrate_kbps.max(500).saturating_mul(1000);
        let gop = fps.saturating_mul(2);
        let mut command = Command::new("ffmpeg");
        command.args([
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("ddagrab=output_idx={output_index}:framerate={fps}"),
            "-an",
        ]);
        if !native_size {
            command.args([
                "-vf",
                &format!("hwdownload,format=bgra,scale={width}:{height}:flags=fast_bilinear"),
            ]);
        }
        command.args([
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
        ]);
        if !native_size {
            command.args(["-pix_fmt", "yuv420p"]);
        }
        command.args([
            "-bsf:v",
            "h264_metadata=aud=insert",
            "-flush_packets",
            "1",
            "-fflags",
            "+flush_packets",
            "-avioflags",
            "direct",
            "-f",
            "h264",
            "pipe:1",
        ]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }

        let mut child = command
            .spawn()
            .map_err(|error| RottenError::Capture(format!("start ddagrab NVENC: {error}")))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| RottenError::Capture("ddagrab NVENC stdout unavailable".into()))?;
        let (sender, output) = sync_channel(OUTPUT_QUEUE);
        let reader = std::thread::Builder::new()
            .name("airmirror-dda-output".into())
            .spawn(move || read_access_units(&mut stdout, sender))
            .map_err(|error| RottenError::Capture(format!("start ddagrab reader: {error}")))?;

        Ok(Self {
            child,
            output: Some(output),
            reader: Some(reader),
            width,
            height,
            fps,
        })
    }

    pub fn take_output(&mut self) -> Option<Receiver<Vec<u8>>> {
        self.output.take()
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }
}

impl Drop for FfmpegDirectCapture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
