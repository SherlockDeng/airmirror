//! Minimal mirror audio RTP (ChaCha + ALAC silence) — Apple TV expects audio after frame 1.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use alac_encoder::{AlacEncoder, FormatDescription};
use plist::Value;
use rotten_core::debug_log::agent_log;
use rotten_core::device::DeviceFeatures;
use rotten_core::error::{Result, RottenError};
use rotten_core::media::{
    AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES, AUDIO_SAMPLE_RATE, TimestampedPcmFrame,
    audio_frame_duration, duration_for_samples,
};
use rotten_core::metrics::MirrorMetrics;
use rotten_crypto::chacha64_seal;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::{Duration as TokioDuration, MissedTickBehavior};
use tracing::info;

const AUDIO_SPF: u32 = AUDIO_FRAME_SAMPLES as u32;
/// Legacy default kept for callers; prefer [`playout_latency_samples`].
pub const AUDIO_LATENCY_SAMPLES: u32 = 44;
const TARGET_LATENCY_MS: u64 = 1;
const AUDIO_CHACHA_NONCE_SIZE: usize = 8;
const AUDIO_MAX_BUFFERED_FRAMES: usize = 32;
const AUDIO_ALIGN_TOLERANCE: Duration = Duration::from_millis(4);
const AUDIO_SEND_LEAD: Duration = Duration::from_millis(20);
const LEGACY_AUDIO_REDUNDANCY_DEPTH: usize = 3;

#[derive(Clone)]
struct AudioPacketFrame {
    payload: Vec<u8>,
    rtp_time: u32,
    seq: u16,
}

#[derive(Clone)]
pub enum MirrorAudioCipher {
    ChaCha20Poly1305([u8; 32]),
    FairPlayAesCbc { key: [u8; 16], iv: [u8; 16] },
    Plaintext,
}

struct AlacFrameEncoder {
    encoder: AlacEncoder,
    input_format: FormatDescription,
    output: Vec<u8>,
    pcm_bytes: Vec<u8>,
    verbatim: bool,
}

impl AlacFrameEncoder {
    fn new() -> Self {
        let output_format =
            FormatDescription::alac(AUDIO_SAMPLE_RATE as f64, AUDIO_SPF, AUDIO_CHANNELS as u32);
        Self {
            encoder: AlacEncoder::new(&output_format),
            input_format: FormatDescription::pcm::<i16>(
                AUDIO_SAMPLE_RATE as f64,
                AUDIO_CHANNELS as u32,
            ),
            output: vec![
                0;
                output_format
                    .max_packet_size()
                    .max(AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS * 2 + 64)
            ],
            pcm_bytes: Vec::with_capacity(AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS * 2),
            verbatim: std::env::var("AIRMIRROR_ALAC_VERBATIM")
                .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
        }
    }

    fn encode(&mut self, samples: &[i16]) -> &[u8] {
        self.pcm_bytes.clear();
        for sample in samples {
            self.pcm_bytes.extend_from_slice(&sample.to_le_bytes());
        }
        if self.verbatim {
            let encoded = encode_alac_verbatim(
                &mut self.output,
                &self.pcm_bytes,
                AUDIO_FRAME_SAMPLES,
                AUDIO_CHANNELS,
                16,
            );
            return &self.output[..encoded];
        }
        let encoded = self
            .encoder
            .encode(&self.input_format, &self.pcm_bytes, &mut self.output);
        &self.output[..encoded]
    }
}

fn audio_source_pts(origin: Duration, frame_index: u64) -> Duration {
    origin.saturating_add(duration_for_samples(
        frame_index.saturating_mul(u64::from(AUDIO_SPF)),
    ))
}

fn audio_rtp_time(origin: u32, frame_index: u64) -> u32 {
    let samples = frame_index.saturating_mul(u64::from(AUDIO_SPF));
    origin.wrapping_add(samples as u32)
}

fn encode_pcm_frame(samples: &[i16]) -> Vec<u8> {
    let mut output = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        output.extend_from_slice(&sample.to_le_bytes());
    }
    output
}

fn correct_audio_timeline(
    origin: Duration,
    observed_source_pts: Duration,
    target_source_pts: Duration,
) -> (Duration, i64) {
    let error_ns = observed_source_pts.as_nanos() as i128 - target_source_pts.as_nanos() as i128;
    if error_ns.abs() <= 2_000_000 || error_ns.abs() > 30_000_000 {
        return (origin, 0);
    }
    let correction_ns = error_ns.clamp(-500_000, 500_000);
    let corrected = if correction_ns >= 0 {
        origin.saturating_add(Duration::from_nanos(correction_ns as u64))
    } else {
        origin.saturating_sub(Duration::from_nanos((-correction_ns) as u64))
    };
    (corrected, correction_ns as i64 / 1_000)
}

/// Playout latency in 44.1 kHz samples (doubletake `samplesFor44k1(TargetLatency())`).
pub fn playout_latency_samples(features: &DeviceFeatures) -> u32 {
    let floor_ms = if features.raw == 0 {
        // `/info` or mDNS features missing: mirror targets Apple TV; use low latency.
        0
    } else {
        features.playout_latency_floor_ms()
    };
    let target_ms = TARGET_LATENCY_MS.max(floor_ms);
    samples_for_44k1(Duration::from_millis(target_ms))
}

fn samples_for_44k1(d: Duration) -> u32 {
    let samples = (d.as_secs_f64() * 44_100.0).round() as i64;
    samples.clamp(1, i64::from(u32::MAX)) as u32
}

/// Parsed audio stream ports from RTSP SETUP response (stream type 96).
pub fn plist_audio_ports(body: &[u8]) -> Option<(u16, u16)> {
    let value: Value = plist::from_bytes(body).ok()?;
    let dict = value.as_dictionary()?;
    let streams = dict.get("streams")?.as_array()?;
    for stream in streams {
        let sd = stream.as_dictionary()?;
        if plist_int(sd.get("type")?) != 96 {
            continue;
        }
        let (data, control) = plist_stream_ports(sd);
        if data > 0 && control > 0 {
            return Some((data, control));
        }
    }
    None
}

/// Match doubletake `plistStreamPorts`: legacy dataPort/controlPort or streamConnections RTP/RTCP keys.
fn plist_stream_ports(stream: &plist::Dictionary) -> (u16, u16) {
    let mut data_port =
        plist_int(stream.get("dataPort").unwrap_or(&Value::Integer(0.into()))) as u16;
    let mut control_port = plist_int(
        stream
            .get("controlPort")
            .unwrap_or(&Value::Integer(0.into())),
    ) as u16;

    if let Some(sc) = stream
        .get("streamConnections")
        .and_then(|v| v.as_dictionary())
    {
        if let Some(rtp) = sc
            .get("streamConnectionTypeRTP")
            .and_then(|v| v.as_dictionary())
        {
            if let Some(port) = rtp.get("streamConnectionKeyPort") {
                let p = plist_int(port);
                if p > 0 {
                    data_port = p as u16;
                }
            }
        }
        if let Some(rtcp) = sc
            .get("streamConnectionTypeRTCP")
            .and_then(|v| v.as_dictionary())
        {
            if let Some(port) = rtcp.get("streamConnectionKeyPort") {
                let p = plist_int(port);
                if p > 0 {
                    control_port = p as u16;
                }
            }
        }
    }

    (data_port, control_port)
}

fn plist_int(value: &Value) -> i64 {
    match value {
        Value::Integer(i) => i.as_signed().unwrap_or(0),
        Value::Real(f) => *f as i64,
        _ => 0,
    }
}

/// UDP sockets and keys needed to stream the mirror audio track.
pub struct MirrorAudioSetup {
    pub host: String,
    pub cipher: MirrorAudioCipher,
    pub remote_data_port: u16,
    pub remote_control_port: u16,
    pub ctrl_socket: UdpSocket,
    pub data_socket: UdpSocket,
    pub latency_samples: u32,
    pub legacy_ntp_layout: bool,
    pub legacy_redundancy: bool,
    pub pcm_audio: bool,
}

/// Spawn the silence fallback audio RTP after the first video frame broadcast fires.
pub fn spawn_mirror_audio_silence(
    setup: MirrorAudioSetup,
    _first_frame: tokio::sync::broadcast::Receiver<u64>,
    metrics: Arc<MirrorMetrics>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_audio_silence_loop(setup, 0, metrics).await {
            agent_log(
                "audio_rtp.rs:run_audio_silence_loop",
                "mirror audio stream error",
                "H61",
                serde_json::json!({ "error": e.to_string() }),
            );
        }
    })
}

/// Spawn the real PCM audio RTP track after video becomes active. A silent
/// ALAC frame is sent whenever WASAPI has no data yet, so the receiver always
/// gets a continuous audio timeline after the first video frame.
pub fn spawn_mirror_audio_pcm(
    setup: MirrorAudioSetup,
    _first_frame: tokio::sync::broadcast::Receiver<u64>,
    mut samples: tokio::sync::mpsc::Receiver<TimestampedPcmFrame>,
    metrics: Arc<MirrorMetrics>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = run_audio_pcm_loop(setup, 0, &mut samples, metrics).await {
            agent_log(
                "audio_rtp.rs:run_audio_pcm_loop",
                "mirror PCM audio stream error",
                "AUD0",
                serde_json::json!({ "error": error.to_string() }),
            );
        }
    })
}

async fn run_audio_pcm_loop(
    setup: MirrorAudioSetup,
    first_frame_pts_us: u64,
    samples: &mut tokio::sync::mpsc::Receiver<TimestampedPcmFrame>,
    metrics: Arc<MirrorMetrics>,
) -> Result<()> {
    let data_addr = format!("{}:{}", setup.host, setup.remote_data_port);
    let ctrl_addr = format!("{}:{}", setup.host, setup.remote_control_port);
    info!(
        %data_addr,
        local_data_addr = ?setup.data_socket.local_addr().ok(),
        %ctrl_addr,
        "starting mirror PCM audio RTP"
    );
    let audio_cipher = &setup.cipher;
    let chacha_nonce = Arc::new(AtomicU64::new(0));
    let ssrc: u32 = 0;
    let frame_samples = AUDIO_FRAME_SAMPLES;
    let stereo_samples = frame_samples * AUDIO_CHANNELS;
    let mut timeline_origin = rotten_core::ntp::boot_duration().saturating_sub(AUDIO_SEND_LEAD);
    let rtp_origin = rand::random::<u32>();
    let latency_samples = setup.latency_samples;
    let initial_playhead = rtp_origin.wrapping_sub(latency_samples);
    let initial_ntp = rotten_core::ntp::ntp_epoch_at(timeline_origin);
    send_sync_packet(
        &setup.ctrl_socket,
        &ctrl_addr,
        initial_ntp,
        initial_playhead,
        rtp_origin,
        setup.legacy_ntp_layout,
        true,
    )
    .await?;
    agent_log(
        "audio_rtp.rs:run_audio_pcm_loop",
        "audio PCM initial sync sent",
        "AUD1",
        serde_json::json!({
            "firstFramePtsUs": first_frame_pts_us,
            "timelineOriginUs": timeline_origin.as_micros() as u64,
            "latencySamples": latency_samples,
            "rtpOrigin": rtp_origin,
        }),
    );

    let mut seq: u16 = 1;
    let mut packets_sent: u64 = 0;
    let mut real_audio_frames: u64 = 0;
    let mut silent_audio_frames: u64 = 0;
    let mut stale_audio_frames: u64 = 0;
    let mut overflow_audio_frames: u64 = 0;
    let mut last_timeline_correction_us: i64 = 0;
    let mut max_alac_bytes = 0usize;
    let mut samples_closed = false;
    let mut queue = VecDeque::with_capacity(AUDIO_MAX_BUFFERED_FRAMES);
    let mut redundancy = VecDeque::with_capacity(LEGACY_AUDIO_REDUNDANCY_DEPTH);
    let mut alac_encoder = AlacFrameEncoder::new();
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now(),
        TokioDuration::from(audio_frame_duration()),
    );
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);

    loop {
        ticker.tick().await;
        if !samples_closed {
            loop {
                match samples.try_recv() {
                    Ok(frame) => {
                        if frame.samples.len() == stereo_samples {
                            queue.push_back(frame);
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        samples_closed = true;
                        agent_log(
                            "audio_rtp.rs:run_audio_pcm_loop",
                            "WASAPI audio source closed; continuing with silence",
                            "AUD3",
                            serde_json::json!({
                                "realAudioFrames": real_audio_frames,
                                "silentAudioFrames": silent_audio_frames,
                                "staleAudioFrames": stale_audio_frames,
                            }),
                        );
                        break;
                    }
                }
            }
        }

        while queue.len() > AUDIO_MAX_BUFFERED_FRAMES {
            queue.pop_front();
            overflow_audio_frames = overflow_audio_frames.saturating_add(1);
        }
        metrics
            .audio_queue_depth
            .store(queue.len() as u64, Ordering::Relaxed);

        let source_pts = audio_source_pts(timeline_origin, packets_sent);
        let mut audio_samples = None;
        while let Some(front) = queue.front() {
            let frame_end = front.captured_at.saturating_add(audio_frame_duration());
            if frame_end.saturating_add(AUDIO_ALIGN_TOLERANCE) < source_pts {
                queue.pop_front();
                stale_audio_frames = stale_audio_frames.saturating_add(1);
                continue;
            }
            if front.captured_at > source_pts.saturating_add(AUDIO_ALIGN_TOLERANCE) {
                break;
            }
            if source_pts.saturating_add(AUDIO_ALIGN_TOLERANCE) >= front.captured_at {
                if let Some(frame) = queue.pop_front() {
                    audio_samples = Some(frame.samples);
                }
            }
            break;
        }

        let source_age_ms = rotten_core::ntp::boot_duration()
            .saturating_sub(source_pts)
            .as_millis() as u64;
        let (samples_for_frame, real_frame) = if let Some(audio_samples) = audio_samples {
            real_audio_frames = real_audio_frames.saturating_add(1);
            (audio_samples, true)
        } else {
            silent_audio_frames = silent_audio_frames.saturating_add(1);
            (vec![0; stereo_samples], false)
        };
        metrics
            .audio_real_frames
            .store(real_audio_frames, Ordering::Relaxed);
        metrics
            .audio_silent_frames
            .store(silent_audio_frames, Ordering::Relaxed);
        let pcm = setup
            .pcm_audio
            .then(|| encode_pcm_frame(&samples_for_frame));
        let alac = (!setup.pcm_audio).then(|| alac_encoder.encode(&samples_for_frame).to_vec());
        let payload = pcm.as_deref().or(alac.as_deref()).unwrap_or_default();
        max_alac_bytes = max_alac_bytes.max(payload.len());
        let rtp_time = audio_rtp_time(rtp_origin, packets_sent);
        send_audio_frame(
            &setup.data_socket,
            &data_addr,
            audio_cipher,
            chacha_nonce.clone(),
            &mut redundancy,
            setup.legacy_redundancy,
            payload,
            rtp_time,
            seq,
            ssrc,
        )
        .await?;
        seq = seq.wrapping_add(1);
        packets_sent = packets_sent.saturating_add(1);

        if packets_sent % 125 == 0 {
            info!(
                packets_sent,
                max_alac_bytes,
                last_alac_bytes = payload.len(),
                real_audio_frames,
                "mirror PCM audio RTP stats"
            );
            if let Some(front) = queue.front() {
                let target = audio_source_pts(timeline_origin, packets_sent);
                let (corrected, correction_us) =
                    correct_audio_timeline(timeline_origin, front.captured_at, target);
                timeline_origin = corrected;
                last_timeline_correction_us = correction_us;
            }
            metrics
                .timeline_correction_us
                .store(last_timeline_correction_us, Ordering::Relaxed);
            let announced_pts = audio_source_pts(timeline_origin, packets_sent);
            let announced_rtp = audio_rtp_time(rtp_origin, packets_sent);
            let _ = send_sync_packet(
                &setup.ctrl_socket,
                &ctrl_addr,
                rotten_core::ntp::ntp_epoch_at(announced_pts),
                announced_rtp.wrapping_sub(latency_samples),
                announced_rtp,
                setup.legacy_ntp_layout,
                false,
            )
            .await;
        }
        if packets_sent == 1 || packets_sent % 1000 == 0 {
            let now = rotten_core::ntp::boot_duration();
            let presentation =
                source_pts.saturating_add(duration_for_samples(u64::from(setup.latency_samples)));
            let sync_error_ms =
                ((presentation.as_nanos() as i128 - now.as_nanos() as i128) / 1_000_000) as i64;
            metrics
                .audio_sync_error_ms
                .store(sync_error_ms, Ordering::Relaxed);
            agent_log(
                "audio_rtp.rs:run_audio_pcm_loop",
                "real audio RTP packet sent",
                "AUD2",
                serde_json::json!({
                    "packetsSent": packets_sent,
                    "seq": seq,
                    "rtpTime": rtp_time,
                    "alacBytes": payload.len(),
                    "realFrame": real_frame,
                    "sourceAgeMs": source_age_ms,
                    "syncErrorMs": sync_error_ms,
                    "queuedAudioFrames": queue.len(),
                    "realAudioFrames": real_audio_frames,
                    "silentAudioFrames": silent_audio_frames,
                    "staleAudioFrames": stale_audio_frames,
                    "overflowAudioFrames": overflow_audio_frames,
                    "timelineCorrectionUs": last_timeline_correction_us,
                }),
            );
        }
    }
}

async fn run_audio_silence_loop(
    setup: MirrorAudioSetup,
    first_frame_pts_us: u64,
    metrics: Arc<MirrorMetrics>,
) -> Result<()> {
    let data_addr = format!("{}:{}", setup.host, setup.remote_data_port);
    let ctrl_addr = format!("{}:{}", setup.host, setup.remote_control_port);

    let audio_cipher = &setup.cipher;
    let chacha_nonce = Arc::new(AtomicU64::new(0));
    let ssrc: u32 = 0;
    let mut redundancy = VecDeque::with_capacity(LEGACY_AUDIO_REDUNDANCY_DEPTH);
    let mut alac_encoder = AlacFrameEncoder::new();
    let silence_samples = vec![0; AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS];
    let alac_frame = (!setup.pcm_audio).then(|| alac_encoder.encode(&silence_samples).to_vec());
    let pcm_frame = setup.pcm_audio.then(|| encode_pcm_frame(&silence_samples));
    let payload = pcm_frame
        .as_deref()
        .or(alac_frame.as_deref())
        .unwrap_or_default();
    let timeline_origin = rotten_core::ntp::boot_duration().saturating_sub(AUDIO_SEND_LEAD);
    let rtp_origin = rand::random::<u32>();
    let latency_samples = setup.latency_samples;
    let initial_playhead = rtp_origin.wrapping_sub(latency_samples);
    // #region agent log
    agent_log(
        "audio_rtp.rs:run_audio_silence_loop",
        "mirror audio silence starting",
        "H61",
        serde_json::json!({
            "remoteDataPort": setup.remote_data_port,
            "remoteControlPort": setup.remote_control_port,
            "alacBytes": payload.len(),
            "dataAddr": data_addr,
            "firstFramePtsUs": first_frame_pts_us,
            "timelineOriginUs": timeline_origin.as_micros() as u64,
            "rtpOrigin": rtp_origin,
        }),
    );
    // #endregion

    send_sync_packet(
        &setup.ctrl_socket,
        &ctrl_addr,
        rotten_core::ntp::ntp_epoch_at(timeline_origin),
        initial_playhead,
        rtp_origin,
        setup.legacy_ntp_layout,
        true,
    )
    .await?;

    let mut seq: u16 = 1;
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now(),
        TokioDuration::from(audio_frame_duration()),
    );
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut packets_sent: u64 = 0;

    loop {
        ticker.tick().await;
        let rtp_time = audio_rtp_time(rtp_origin, packets_sent);
        send_audio_frame(
            &setup.data_socket,
            &data_addr,
            audio_cipher,
            chacha_nonce.clone(),
            &mut redundancy,
            setup.legacy_redundancy,
            payload,
            rtp_time,
            seq,
            ssrc,
        )
        .await?;
        seq = seq.wrapping_add(1);
        packets_sent += 1;
        metrics
            .audio_silent_frames
            .store(packets_sent, Ordering::Relaxed);
        metrics.audio_queue_depth.store(0, Ordering::Relaxed);

        if packets_sent == 1 || packets_sent % 125 == 0 {
            let announced_pts = audio_source_pts(timeline_origin, packets_sent);
            let announced_rtp = audio_rtp_time(rtp_origin, packets_sent);
            let now = rotten_core::ntp::boot_duration();
            let presentation = announced_pts
                .saturating_add(duration_for_samples(u64::from(setup.latency_samples)));
            let sync_error_ms =
                ((presentation.as_nanos() as i128 - now.as_nanos() as i128) / 1_000_000) as i64;
            metrics
                .audio_sync_error_ms
                .store(sync_error_ms, Ordering::Relaxed);
            let _ = send_sync_packet(
                &setup.ctrl_socket,
                &ctrl_addr,
                rotten_core::ntp::ntp_epoch_at(announced_pts),
                announced_rtp.wrapping_sub(latency_samples),
                announced_rtp,
                setup.legacy_ntp_layout,
                false,
            )
            .await;
        }
        if packets_sent == 1 || packets_sent % 1000 == 0 {
            agent_log(
                "audio_rtp.rs:run_audio_silence_loop",
                "audio RTP packet sent",
                "H61",
                serde_json::json!({
                    "packetsSent": packets_sent,
                    "seq": seq,
                    "rtpTime": rtp_time,
                }),
            );
        }
    }
}

async fn send_audio_packet(
    socket: &UdpSocket,
    remote: &str,
    cipher: &MirrorAudioCipher,
    nonce_counter: Arc<AtomicU64>,
    payload: &[u8],
    rtp_time: u32,
    seq: u16,
    ssrc: u32,
) -> Result<()> {
    let mut header = [0u8; 12];
    header[0] = 0x80;
    header[1] = 0x60;
    header[2..4].copy_from_slice(&seq.to_be_bytes());
    header[4..8].copy_from_slice(&rtp_time.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());

    let (protected, nonce_bytes, cipher_name) = match cipher {
        MirrorAudioCipher::ChaCha20Poly1305(chacha_key) => {
            let nonce_val = nonce_counter.fetch_add(1, Ordering::Relaxed);
            let nonce_bytes = nonce_val.to_le_bytes();
            let sealed = chacha64_seal(chacha_key, &nonce_bytes, payload, &header[4..12]);
            (sealed, Some(nonce_bytes), "chacha64")
        }
        MirrorAudioCipher::FairPlayAesCbc { key, iv } => (
            encrypt_fairplay_audio_cbc(key, iv, payload),
            None,
            "aes-cbc",
        ),
        MirrorAudioCipher::Plaintext => (payload.to_vec(), None, "none"),
    };

    let mut packet = Vec::with_capacity(
        12 + protected.len() + nonce_bytes.map_or(0, |_| AUDIO_CHACHA_NONCE_SIZE),
    );
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&protected);
    if let Some(nonce) = nonce_bytes {
        packet.extend_from_slice(&nonce);
    }

    if seq == 1 {
        info!(
            packet_len = packet.len(),
            header = %hex::encode(&packet[..12]),
            cipher = cipher_name,
            "first mirror audio RTP packet"
        );
        // #region agent log
        agent_log(
            "audio_rtp.rs:send_audio_packet",
            "first audio packet",
            "H69",
            serde_json::json!({
                "cipher": cipher_name,
                "packetLen": packet.len(),
                "sealedLen": protected.len(),
                "payloadLen": payload.len(),
                "seq": seq,
            }),
        );
        // #endregion
    }

    socket
        .send_to(&packet, remote)
        .await
        .map_err(|e| RottenError::Protocol(format!("audio RTP send: {e}")))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_audio_frame(
    socket: &UdpSocket,
    remote: &str,
    cipher: &MirrorAudioCipher,
    nonce_counter: Arc<AtomicU64>,
    redundancy: &mut VecDeque<AudioPacketFrame>,
    redundant_audio: bool,
    payload: &[u8],
    rtp_time: u32,
    seq: u16,
    ssrc: u32,
) -> Result<()> {
    if !redundant_audio {
        return send_audio_packet(
            socket,
            remote,
            cipher,
            nonce_counter,
            payload,
            rtp_time,
            seq,
            ssrc,
        )
        .await;
    }

    redundancy.push_back(AudioPacketFrame {
        payload: payload.to_vec(),
        rtp_time,
        seq,
    });
    while redundancy.len() > LEGACY_AUDIO_REDUNDANCY_DEPTH {
        redundancy.pop_front();
    }
    for frame in redundancy.iter() {
        send_audio_packet(
            socket,
            remote,
            cipher,
            nonce_counter.clone(),
            &frame.payload,
            frame.rtp_time,
            frame.seq,
            ssrc,
        )
        .await?;
    }
    Ok(())
}

fn encrypt_fairplay_audio_cbc(key: &[u8; 16], iv: &[u8; 16], payload: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key.into());
    let mut output = payload.to_vec();
    let encrypted_len = output.len() / 16 * 16;
    let mut previous = *iv;

    for block in output[..encrypted_len].chunks_exact_mut(16) {
        for (byte, chain) in block.iter_mut().zip(previous.iter()) {
            *byte ^= *chain;
        }
        let block = GenericArray::from_mut_slice(block);
        cipher.encrypt_block(block);
        previous.copy_from_slice(block);
    }

    output
}

async fn send_sync_packet(
    socket: &UdpSocket,
    remote: &str,
    ntp_time: u64,
    playhead_rtp: u32,
    rtp_now: u32,
    legacy_ntp_layout: bool,
    is_first: bool,
) -> Result<()> {
    let packet = build_sync_packet(ntp_time, playhead_rtp, rtp_now, legacy_ntp_layout, is_first);
    if is_first {
        // #region agent log
        agent_log(
            "audio_rtp.rs:send_sync_packet",
            "audio initial sync packet",
            "H71",
            serde_json::json!({
                "playheadRtp": playhead_rtp,
                "networkTime": ntp_time,
            }),
        );
        // #endregion
    }

    socket
        .send_to(&packet, remote)
        .await
        .map_err(|e| RottenError::Protocol(format!("audio sync send: {e}")))?;
    Ok(())
}

fn build_sync_packet(
    ntp_time: u64,
    playhead_rtp: u32,
    rtp_now: u32,
    legacy_ntp_layout: bool,
    is_first: bool,
) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[0] = if is_first { 0x90 } else { 0x80 };
    packet[1] = 0xd4;
    packet[2..4].copy_from_slice(&4u16.to_be_bytes());
    packet[4..8].copy_from_slice(&playhead_rtp.to_be_bytes());
    packet[8..16].copy_from_slice(&ntp_time.to_be_bytes());
    let next_rtp = if legacy_ntp_layout {
        rtp_now
    } else {
        playhead_rtp
    };
    packet[16..20].copy_from_slice(&next_rtp.to_be_bytes());
    packet
}

/// ALAC verbatim frame for stereo silence (spf samples per channel).
fn encode_alac_verbatim_silence(spf: u16) -> Vec<u8> {
    let pcm = vec![0u8; spf as usize * 2 * 2];
    encode_alac_verbatim_bytes(&pcm, spf)
}

fn encode_alac_verbatim_bytes(pcm: &[u8], spf: u16) -> Vec<u8> {
    let mut out = vec![0u8; pcm.len() + 64];
    let n = encode_alac_verbatim(&mut out, pcm, spf as usize, 2, 16);
    out.truncate(n);
    out
}

struct BitWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
    bit_buf: u32,
    bit_pos: u8,
}

impl<'a> BitWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            bit_buf: 0,
            bit_pos: 0,
        }
    }

    fn write(&mut self, val: u32, nbits: u32) {
        let mut v = val & bit_mask(nbits);
        let mut remaining = nbits;
        while remaining > 0 {
            let space = (8 - self.bit_pos) as u32;
            if remaining <= space {
                self.bit_buf |= (v & bit_mask(remaining)) << (space - remaining);
                self.bit_pos += remaining as u8;
                if self.bit_pos == 8 {
                    if self.pos < self.buf.len() {
                        self.buf[self.pos] = self.bit_buf as u8;
                    }
                    self.pos += 1;
                    self.bit_buf = 0;
                    self.bit_pos = 0;
                }
                return;
            }

            let shift = remaining - space;
            self.bit_buf |= (v >> shift) & bit_mask(space);
            if self.pos < self.buf.len() {
                self.buf[self.pos] = self.bit_buf as u8;
            }
            self.pos += 1;
            self.bit_buf = 0;
            self.bit_pos = 0;
            remaining = shift;
            v &= bit_mask(shift);
        }
    }

    fn flush(mut self) -> usize {
        if self.bit_pos > 0 {
            if self.pos < self.buf.len() {
                self.buf[self.pos] = self.bit_buf as u8;
            }
            self.pos += 1;
        }
        self.pos
    }
}

fn bit_mask(bits: u32) -> u32 {
    if bits >= 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    }
}

fn encode_alac_verbatim(
    out: &mut [u8],
    pcm: &[u8],
    frame_size: usize,
    channels: usize,
    bit_depth: u32,
) -> usize {
    let mut bw = BitWriter::new(out);
    if channels == 2 {
        bw.write(1, 3);
    } else {
        bw.write(0, 3);
    }
    bw.write(0, 4);
    bw.write(0, 12);
    bw.write(1, 1);
    bw.write(0, 2);
    bw.write(1, 1);
    bw.write(frame_size as u32, 32);

    for i in 0..frame_size * channels {
        let off = i * 2;
        let sample = if off + 1 < pcm.len() {
            u16::from_le_bytes([pcm[off], pcm[off + 1]])
        } else {
            0
        };
        bw.write(sample as u32, bit_depth);
    }
    bw.write(7, 3);
    bw.flush()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rotten_core::media::duration_for_samples;

    use super::{
        AUDIO_SPF, audio_rtp_time, audio_source_pts, build_sync_packet, correct_audio_timeline,
        encode_alac_verbatim_bytes, encode_alac_verbatim_silence, encrypt_fairplay_audio_cbc,
    };

    #[test]
    fn silence_frame_has_expected_alac_layout() {
        let frame = encode_alac_verbatim_silence(AUDIO_SPF as u16);

        assert_eq!(frame.len(), 1416);
        assert_eq!(&frame[..4], &[0x20, 0x00, 0x12, 0x00]);
        assert_eq!(
            &frame[..8],
            &[0x20, 0x00, 0x12, 0x00, 0x00, 0x02, 0xc0, 0x00]
        );
    }

    #[test]
    fn verbatim_samples_are_big_endian() {
        let pcm = [0x1234i16, 0x5678i16]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();

        let frame = encode_alac_verbatim_bytes(&pcm, 1);

        assert_eq!(
            frame,
            vec![
                0x20, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x24, 0x68, 0xac, 0xf1, 0xc0
            ]
        );
    }

    #[test]
    fn audio_timeline_advances_by_exact_sample_count() {
        let origin = Duration::from_secs(123);
        assert_eq!(audio_rtp_time(17, 0), 17);
        assert_eq!(audio_rtp_time(17, 125), 17 + 125 * AUDIO_SPF);
        assert_eq!(
            audio_source_pts(origin, 125).saturating_sub(origin),
            duration_for_samples(125 * u64::from(AUDIO_SPF))
        );
    }

    #[test]
    fn sync_packet_uses_one_rtp_playhead_in_both_fields() {
        let ntp = 0x1122_3344_5566_7788;
        let playhead = 0xfeed_1234;
        let packet = build_sync_packet(ntp, playhead, playhead, false, true);

        assert_eq!(packet[0], 0x90);
        assert_eq!(packet[1], 0xd4);
        assert_eq!(
            u32::from_be_bytes(packet[4..8].try_into().unwrap()),
            playhead
        );
        assert_eq!(u64::from_be_bytes(packet[8..16].try_into().unwrap()), ntp);
        assert_eq!(
            u32::from_be_bytes(packet[16..20].try_into().unwrap()),
            playhead
        );
    }

    #[test]
    fn legacy_sync_packet_carries_playhead_and_current_rtp() {
        let packet = build_sync_packet(0x1122_3344_5566_7788, 0x1000, 0x2000, true, true);
        assert_eq!(u32::from_be_bytes(packet[4..8].try_into().unwrap()), 0x1000);
        assert_eq!(
            u32::from_be_bytes(packet[16..20].try_into().unwrap()),
            0x2000
        );
    }

    #[test]
    fn long_run_timeline_correction_is_bounded_to_half_a_millisecond() {
        let origin = Duration::from_secs(100);
        let target = Duration::from_secs(100);
        let observed = Duration::from_millis(100_010);
        let (corrected, correction_us) = correct_audio_timeline(origin, observed, target);

        assert_eq!(correction_us, 500);
        assert_eq!(corrected.saturating_sub(origin), Duration::from_micros(500));
    }

    #[test]
    fn fairplay_audio_cbc_matches_nist_vector_and_keeps_partial_tail() {
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let iv = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let mut payload = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
        payload.extend_from_slice(&[0xaa, 0x55]);
        let encrypted = encrypt_fairplay_audio_cbc(&key, &iv, &payload);

        assert_eq!(
            hex::encode(&encrypted[..16]),
            "7649abac8119b246cee98e9b12e9197d"
        );
        assert_eq!(&encrypted[16..], &[0xaa, 0x55]);
    }
}
