use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_core::media::{
    AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE, TimestampedPcmFrame,
    audio_frame_duration, duration_for_samples,
};
use tracing::{info, warn};

const BITS_PER_SAMPLE: u16 = 16;
const CAPTURE_BUFFER_MS: u32 = 100;
const MAX_PTS_DISCONTINUITY: Duration = Duration::from_millis(20);

pub struct AudioMirror {
    receiver: Option<tokio::sync::mpsc::Receiver<TimestampedPcmFrame>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    device_name: String,
}

impl AudioMirror {
    pub async fn start(device: &AirPlayDevice) -> Result<Self> {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = device;
            return Err(RottenError::Capture(
                "system audio loopback is currently implemented for Windows only".into(),
            ));
        }

        #[cfg(target_os = "windows")]
        {
            let (sample_tx, receiver) = tokio::sync::mpsc::channel(16);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_worker = stop.clone();
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
            let worker = std::thread::Builder::new()
                .name("airmirror-wasapi-loopback".into())
                .spawn(move || {
                    let result = capture_loop(sample_tx, stop_worker, ready_tx);
                    if let Err(error) = result {
                        warn!(%error, "WASAPI loopback stopped with error");
                    }
                })
                .map_err(|error| RottenError::Capture(format!("start WASAPI thread: {error}")))?;

            match tokio::time::timeout(Duration::from_secs(5), ready_rx).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => {
                    stop.store(true, Ordering::Relaxed);
                    let _ = worker.join();
                    return Err(RottenError::Capture(format!(
                        "initialize WASAPI loopback: {error}"
                    )));
                }
                Ok(Err(error)) => {
                    stop.store(true, Ordering::Relaxed);
                    let _ = worker.join();
                    return Err(RottenError::Capture(format!(
                        "WASAPI initialization channel closed: {error}"
                    )));
                }
                Err(_) => {
                    stop.store(true, Ordering::Relaxed);
                    let _ = worker.join();
                    return Err(RottenError::Capture(
                        "WASAPI loopback initialization timed out".into(),
                    ));
                }
            }

            info!(
                device = %device.name,
                sample_rate = AUDIO_SAMPLE_RATE,
                channels = AUDIO_CHANNELS,
                bits = BITS_PER_SAMPLE,
                "WASAPI loopback capture started"
            );
            Ok(Self {
                receiver: Some(receiver),
                stop,
                worker: Some(worker),
                device_name: device.name.clone(),
            })
        }
    }

    pub fn take_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<TimestampedPcmFrame>> {
        self.receiver.take()
    }

    pub async fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            tokio::task::spawn_blocking(move || {
                let _ = worker.join();
            })
            .await
            .map_err(|error| RottenError::Capture(format!("join WASAPI thread: {error}")))?;
        }
        info!(device = %self.device_name, "WASAPI loopback capture stopped");
        Ok(())
    }
}

impl Drop for AudioMirror {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.receiver.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(target_os = "windows")]
fn capture_loop(
    sample_tx: tokio::sync::mpsc::Sender<TimestampedPcmFrame>,
    stop: Arc<AtomicBool>,
    ready: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
) -> std::result::Result<(), String> {
    let mut ready = Some(ready);
    let mut attempt = 0u32;
    let mut endpoint_mute: Option<AudioEndpointMute> = None;
    while !stop.load(Ordering::Relaxed) {
        attempt = attempt.saturating_add(1);
        match capture_once(&sample_tx, &stop, &mut ready, &mut endpoint_mute) {
            Ok(()) => return Ok(()),
            Err(error) => {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(error.clone()));
                    return Err(error);
                }
                warn!(
                    attempt,
                    %error,
                    "WASAPI loopback lost; rebuilding capture client"
                );
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn capture_once(
    sample_tx: &tokio::sync::mpsc::Sender<TimestampedPcmFrame>,
    stop: &AtomicBool,
    ready: &mut Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
    endpoint_mute: &mut Option<AudioEndpointMute>,
) -> std::result::Result<(), String> {
    use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

    initialize_mta()
        .ok()
        .map_err(|error| format!("initialize MTA: {error}"))?;
    let enumerator = DeviceEnumerator::new().map_err(|error| format!("enumerator: {error}"))?;
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(|error| format!("default render device: {error}"))?;
    let mut audio_client = device
        .get_iaudioclient()
        .map_err(|error| format!("activate audio client: {error}"))?;
    let desired_format = WaveFormat::new(
        BITS_PER_SAMPLE as usize,
        BITS_PER_SAMPLE as usize,
        &SampleType::Int,
        AUDIO_SAMPLE_RATE as usize,
        AUDIO_CHANNELS,
        None,
    );
    let mode = StreamMode::PollingShared {
        autoconvert: true,
        buffer_duration_hns: i64::from(CAPTURE_BUFFER_MS) * 10_000,
    };
    audio_client
        .initialize_client(&desired_format, &Direction::Capture, &mode)
        .map_err(|error| format!("initialize loopback: {error}"))?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .map_err(|error| format!("capture client: {error}"))?;
    let block_align = desired_format.get_blockalign() as usize;
    let frame_boundary = AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS;
    let mut pending = Vec::with_capacity(frame_boundary * 2);
    let mut pending_pts: Option<Duration> = None;
    let mut capture_clock_offset: Option<Duration> = None;
    audio_client
        .start_stream()
        .map_err(|error| format!("start loopback: {error}"))?;
    let device_id = device
        .get_id()
        .map_err(|error| format!("default render device id: {error}"))?;
    if endpoint_mute
        .as_ref()
        .is_none_or(|mute| mute.device_id != device_id)
    {
        *endpoint_mute = Some(AudioEndpointMute::mute(&device)?);
    }
    if let Some(ready) = ready.take() {
        let _ = ready.send(Ok(()));
    }

    while !stop.load(Ordering::Relaxed) {
        let mut read_any = false;
        while let Some(packet_frames) = capture_client
            .get_next_packet_size()
            .map_err(|error| format!("loopback packet size: {error}"))?
        {
            if packet_frames == 0 {
                break;
            }
            read_any = true;
            let mut bytes = vec![0u8; packet_frames as usize * block_align];
            let (frames_read, buffer_info) = capture_client
                .read_from_device(&mut bytes)
                .map_err(|error| format!("read loopback: {error}"))?;
            if frames_read == 0 {
                continue;
            }
            bytes.truncate(frames_read as usize * block_align);
            if buffer_info.flags.silent {
                bytes.fill(0);
            }

            let now = rotten_core::ntp::boot_duration();
            let raw_pts = Duration::from_nanos(buffer_info.timestamp.saturating_mul(100));
            let packet_duration = duration_for_samples(u64::from(frames_read));
            let offset = *capture_clock_offset.get_or_insert_with(|| {
                if raw_pts.abs_diff(now) <= Duration::from_secs(2) {
                    Duration::ZERO
                } else {
                    now.saturating_sub(raw_pts).saturating_sub(packet_duration)
                }
            });
            let packet_pts = if buffer_info.timestamp == 0 || buffer_info.flags.timestamp_error {
                now.saturating_sub(packet_duration)
            } else {
                raw_pts.saturating_add(offset)
            };

            if buffer_info.flags.data_discontinuity {
                pending.clear();
                pending_pts = None;
            } else if let Some(expected) = pending_pts.map(|pts| {
                pts.saturating_add(duration_for_samples(
                    (pending.len() / AUDIO_CHANNELS) as u64,
                ))
            }) {
                if packet_pts.abs_diff(expected) > MAX_PTS_DISCONTINUITY {
                    pending.clear();
                    pending_pts = None;
                }
            }

            if pending.is_empty() {
                pending_pts = Some(packet_pts);
            }
            for sample in bytes.chunks_exact(2) {
                pending.push(i16::from_le_bytes([sample[0], sample[1]]));
            }

            while pending.len() >= frame_boundary {
                let captured_at = pending_pts.unwrap_or(packet_pts);
                let samples = pending.drain(..frame_boundary).collect();
                let frame = TimestampedPcmFrame {
                    captured_at,
                    samples,
                };
                if sample_tx.blocking_send(frame).is_err() {
                    let _ = audio_client.stop_stream();
                    return Ok(());
                }
                pending_pts = Some(captured_at.saturating_add(audio_frame_duration()));
            }
        }
        if !read_any {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let _ = audio_client.stop_stream();
    Ok(())
}

#[cfg(target_os = "windows")]
struct AudioEndpointMute {
    volume: windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume,
    was_muted: bool,
    device_id: String,
}

#[cfg(target_os = "windows")]
impl AudioEndpointMute {
    fn mute(device: &wasapi::Device) -> std::result::Result<Self, String> {
        use windows::Win32::System::Com::CLSCTX_ALL;

        let volume: windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume =
            unsafe { device.as_immdevice().Activate(CLSCTX_ALL, None) }
                .map_err(|error| format!("activate endpoint volume: {error}"))?;
        let was_muted = unsafe { volume.GetMute() }
            .map_err(|error| format!("read endpoint mute state: {error}"))?
            .as_bool();
        let device_id = device
            .get_id()
            .map_err(|error| format!("read endpoint id: {error}"))?;
        unsafe { volume.SetMute(true, std::ptr::null()) }
            .map_err(|error| format!("mute local playback endpoint: {error}"))?;
        info!(
            endpoint = %device_id,
            was_muted,
            "local playback endpoint muted for AirPlay audio"
        );
        Ok(Self {
            volume,
            was_muted,
            device_id,
        })
    }
}

#[cfg(target_os = "windows")]
impl Drop for AudioEndpointMute {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.volume.SetMute(self.was_muted, std::ptr::null()) } {
            warn!(%error, endpoint = %self.device_id, "failed to restore local playback mute state");
        } else {
            info!(
                endpoint = %self.device_id,
                restored_muted = self.was_muted,
                "local playback endpoint mute state restored"
            );
        }
    }
}

#[cfg(target_os = "linux")]
pub mod platform_audio {
    pub fn list_audio_sources() -> Vec<String> {
        vec!["default".into()]
    }
}

#[cfg(target_os = "windows")]
pub mod platform_audio {
    pub fn list_audio_sources() -> Vec<String> {
        vec!["default render loopback".into()]
    }
}
