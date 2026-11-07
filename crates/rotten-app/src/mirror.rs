use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rotten_core::config::{HwAccel, MirrorConfig};
use rotten_core::debug_log::{DEBUG_BUILD_ID, agent_log};
use rotten_core::device::AirPlayDevice;
use rotten_core::error::{Result, RottenError};
use rotten_core::metrics::{MirrorMetrics, MirrorMetricsSnapshot};
use rotten_pairing::PairingManager;
use rotten_protocol::MirrorConnection;
use rotten_video::{
    EncodedFrame, FfmpegDirectCapture, MirrorStreamer, SyntheticSource, auto_bitrate_kbps,
    downscale_rgba, fit_stream_dims, frame_channel,
};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::audio::AudioMirror;

const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(30);
const STABLE_SESSION_THRESHOLD: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub enum MirrorStatus {
    Connecting {
        attempt: u32,
    },
    Streaming {
        attempt: u32,
    },
    Reconnecting {
        attempt: u32,
        delay: Duration,
        error: String,
    },
    Metrics(MirrorMetricsSnapshot),
    Stopped,
}

pub async fn run_mirror(device: AirPlayDevice, config: MirrorConfig) -> Result<()> {
    if matches!(config.hw_accel, HwAccel::Vaapi) {
        return Err(RottenError::Video(
            "VAAPI is not available on Windows; use auto, nvenc, or none".into(),
        ));
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(true);
        }
    });

    run_mirror_loop(
        device,
        config,
        shutdown_rx,
        None,
        Arc::new(MirrorMetrics::default()),
    )
    .await
}

pub async fn run_mirror_with_shutdown(
    device: AirPlayDevice,
    config: MirrorConfig,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    status_tx: Option<std::sync::mpsc::Sender<MirrorStatus>>,
) -> Result<()> {
    run_mirror_with_metrics(
        device,
        config,
        shutdown_rx,
        status_tx,
        Arc::new(MirrorMetrics::default()),
    )
    .await
}

pub async fn run_mirror_with_metrics(
    device: AirPlayDevice,
    config: MirrorConfig,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    status_tx: Option<std::sync::mpsc::Sender<MirrorStatus>>,
    metrics: Arc<MirrorMetrics>,
) -> Result<()> {
    let result = run_mirror_loop(device, config, shutdown_rx, status_tx.clone(), metrics).await;
    if let Some(status_tx) = status_tx {
        let _ = status_tx.send(MirrorStatus::Stopped);
    }
    result
}

async fn run_mirror_loop(
    device: AirPlayDevice,
    config: MirrorConfig,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    status_tx: Option<std::sync::mpsc::Sender<MirrorStatus>>,
    metrics: Arc<MirrorMetrics>,
) -> Result<()> {
    let mut consecutive_failures: u32 = 0;
    let mut backoff = ReconnectBackoff::default();
    loop {
        let attempt = consecutive_failures.saturating_add(1);
        info!(attempt, "starting mirror session");
        emit_status(status_tx.as_ref(), MirrorStatus::Connecting { attempt });
        let started = Instant::now();
        let outcome = tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() {
                    info!("mirror shutdown requested");
                    return Ok(());
                }
                run_mirror_session(
                    &device,
                    &config,
                    attempt,
                    status_tx.as_ref(),
                    metrics.clone(),
                )
                .await
            }
            outcome = run_mirror_session(
                &device,
                &config,
                attempt,
                status_tx.as_ref(),
                metrics.clone(),
            ) => outcome,
        };
        let elapsed = started.elapsed();

        match &outcome {
            Ok(()) => warn!(
                attempt,
                elapsed_seconds = elapsed.as_secs(),
                "mirror session ended unexpectedly; reconnecting"
            ),
            Err(error) => warn!(
                attempt,
                elapsed_seconds = elapsed.as_secs(),
                %error,
                "mirror session failed; reconnecting"
            ),
        }

        if elapsed >= STABLE_SESSION_THRESHOLD {
            consecutive_failures = 0;
            backoff.reset();
        } else {
            consecutive_failures = consecutive_failures.saturating_add(1);
        }

        let delay = backoff.next_delay();
        metrics
            .reconnects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        info!(
            delay_ms = delay.as_millis(),
            consecutive_failures, "waiting before reconnect"
        );
        let error = match &outcome {
            Ok(()) => "mirror session ended".to_string(),
            Err(error) => error.to_string(),
        };
        emit_status(
            status_tx.as_ref(),
            MirrorStatus::Reconnecting {
                attempt,
                delay,
                error,
            },
        );
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() {
                    info!("mirror shutdown requested");
                    return Ok(());
                }
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

async fn run_mirror_session(
    device: &AirPlayDevice,
    config: &MirrorConfig,
    attempt: u32,
    status_tx: Option<&std::sync::mpsc::Sender<MirrorStatus>>,
    metrics: Arc<MirrorMetrics>,
) -> Result<()> {
    // #region agent log
    agent_log(
        "mirror.rs:run_mirror",
        "mirror session starting",
        "H0",
        serde_json::json!({
            "buildId": DEBUG_BUILD_ID,
            "testMode": config.test_mode,
            "host": device.host,
        }),
    );
    // #endregion

    let mut pairing = PairingManager::load(config.credentials_path.clone())?;

    let creds = pairing.pair(device, config.force_pair).await?;

    let stream_config = config.stream.clone();
    let mut handle = MirrorConnection::connect(device.clone(), &creds, config).await?;
    emit_status(status_tx, MirrorStatus::Streaming { attempt });

    let bitrate = if stream_config.bitrate_kbps == 0 {
        auto_bitrate_kbps(stream_config.width, stream_config.height, stream_config.fps)
    } else {
        stream_config.bitrate_kbps
    };

    info!(
        encoder = ?config.hw_accel,
        bitrate_kbps = bitrate,
        test_mode = config.test_mode,
        "starting mirror stream"
    );

    let video_crypto = handle.video_crypto.clone();
    let data_port = handle.data_port();
    let control_uri = handle.control_uri().to_string();
    let session_uuid = handle.session().session_id.clone().unwrap_or_default();
    let rtsp_conn = handle.take_rtsp_conn().ok_or_else(|| {
        rotten_core::error::RottenError::Protocol("missing RTSP connection".into())
    })?;
    let data_stream = handle
        .take_data_stream()
        .ok_or_else(|| rotten_core::error::RottenError::Protocol("missing data stream".into()))?;
    let audio_setup = handle.audio;
    info!(
        requested = config.audio,
        negotiated = audio_setup.is_some(),
        "mirror audio transport"
    );
    if config.audio && audio_setup.is_none() {
        warn!("receiver did not negotiate the AirPlay mirror audio stream");
    }
    let streamer = MirrorStreamer::new(device.host.clone(), data_port, video_crypto);
    let rtsp_conn = Arc::new(tokio::sync::Mutex::new(rtsp_conn));
    let mut tasks = TaskGroup::default();
    let mut audio_handle = if config.audio {
        Some(AudioMirror::start(device).await?)
    } else {
        None
    };
    let (first_frame_broadcast, _) = tokio::sync::broadcast::channel::<u64>(3);
    let mut rtsp_first_frame = first_frame_broadcast.subscribe();
    let mut heartbeat_first_frame = first_frame_broadcast.subscribe();
    let _audio_latency_samples = audio_setup
        .as_ref()
        .map(|a| a.latency_samples)
        .unwrap_or_else(|| config.sync.audio_latency_samples());
    let video_timestamp_bias_samples = config.sync.video_lead_samples();
    if let Some(audio) = audio_setup {
        if let Some(samples) = audio_handle.as_mut().and_then(AudioMirror::take_receiver) {
            tasks.push(rotten_protocol::spawn_mirror_audio_pcm(
                audio,
                first_frame_broadcast.subscribe(),
                samples,
                metrics.clone(),
            ));
        } else {
            tasks.push(rotten_protocol::spawn_mirror_audio_silence(
                audio,
                first_frame_broadcast.subscribe(),
                metrics.clone(),
            ));
        }
    }
    let (first_frame_tx, first_frame_rx) = tokio::sync::oneshot::channel();
    let first_frame_notify = first_frame_broadcast.clone();
    tasks.spawn(async move {
        if let Ok(first_frame_pts_us) = first_frame_rx.await {
            let _ = first_frame_notify.send(first_frame_pts_us);
        }
    });
    let stats = streamer.stats();
    {
        let stats = stats.clone();
        let metrics = metrics.clone();
        let status_tx = status_tx.cloned();
        tasks.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_video_frames = 0u64;
            let mut last_cpu_time = rotten_core::metrics::process_cpu_time_100ns();
            let mut last_cpu_at = Instant::now();
            loop {
                ticker.tick().await;
                let video_frames = stats.frames_sent.load(std::sync::atomic::Ordering::Relaxed);
                metrics
                    .video_frames
                    .store(video_frames, std::sync::atomic::Ordering::Relaxed);
                metrics.video_fps.store(
                    video_frames.saturating_sub(last_video_frames),
                    std::sync::atomic::Ordering::Relaxed,
                );
                last_video_frames = video_frames;
                metrics.video_dropped.store(
                    stats
                        .dropped_frames
                        .load(std::sync::atomic::Ordering::Relaxed),
                    std::sync::atomic::Ordering::Relaxed,
                );
                metrics.video_playout_slack_ms.store(
                    stats
                        .last_playout_slack_ms
                        .load(std::sync::atomic::Ordering::Relaxed),
                    std::sync::atomic::Ordering::Relaxed,
                );
                let audio_sync = metrics
                    .audio_sync_error_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let video_slack = metrics
                    .video_playout_slack_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                if audio_sync != 0 && video_slack != 0 {
                    let av_skew_ms = audio_sync.saturating_sub(video_slack);
                    metrics
                        .av_skew_ms
                        .store(av_skew_ms, std::sync::atomic::Ordering::Relaxed);
                    metrics.max_abs_av_skew_ms.fetch_max(
                        av_skew_ms.unsigned_abs() as i64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                if let (Some(previous), Some(current)) = (
                    last_cpu_time,
                    rotten_core::metrics::process_cpu_time_100ns(),
                ) {
                    let elapsed_ns = last_cpu_at.elapsed().as_nanos().max(1);
                    let cpu_permille = current
                        .saturating_sub(previous)
                        .saturating_mul(100_000)
                        .saturating_div(elapsed_ns as u64);
                    metrics
                        .cpu_permille
                        .store(cpu_permille, std::sync::atomic::Ordering::Relaxed);
                    last_cpu_time = Some(current);
                    last_cpu_at = Instant::now();
                }
                if let Some((working_set, private_memory)) =
                    rotten_core::metrics::process_memory_bytes()
                {
                    metrics
                        .working_set_bytes
                        .store(working_set, std::sync::atomic::Ordering::Relaxed);
                    metrics
                        .private_memory_bytes
                        .store(private_memory, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(status_tx) = &status_tx {
                    let _ = status_tx.send(MirrorStatus::Metrics(metrics.snapshot()));
                }
            }
        });
    }
    tasks.spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let frames = stats.frames_sent.load(std::sync::atomic::Ordering::Relaxed);
            let encode_total = stats
                .encode_time_us
                .load(std::sync::atomic::Ordering::Relaxed);
            let send_total = stats
                .send_time_us
                .load(std::sync::atomic::Ordering::Relaxed);
            info!(
                frames_sent = frames,
                bytes_sent = stats.bytes_sent.load(std::sync::atomic::Ordering::Relaxed),
                dropped_frames = stats
                    .dropped_frames
                    .load(std::sync::atomic::Ordering::Relaxed),
                avg_encode_ms = if frames > 0 {
                    encode_total as f64 / frames as f64 / 1000.0
                } else {
                    0.0
                },
                max_encode_ms = stats
                    .max_encode_us
                    .load(std::sync::atomic::Ordering::Relaxed)
                    as f64
                    / 1000.0,
                avg_send_ms = if frames > 0 {
                    send_total as f64 / frames as f64 / 1000.0
                } else {
                    0.0
                },
                max_send_ms =
                    stats.max_send_us.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0,
                "mirror stream statistics"
            );
        }
    });
    let rtsp_feedback = rtsp_conn.clone();
    tasks.spawn(async move {
        if rtsp_first_frame.recv().await.is_err() {
            return;
        }
        loop {
            let mut conn = rtsp_feedback.lock().await;
            match conn.rtsp_post_feedback().await {
                Ok((status, body)) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:rtsp_feedback",
                        "RTSP POST /feedback sent",
                        "H97",
                        serde_json::json!({
                            "httpStatus": status,
                            "bodyLen": body.len(),
                            "bodyByte0": body.first().copied().unwrap_or(0),
                            "feedback": feedback_plist_summary(&body),
                        }),
                    );
                    // #endregion
                }
                Err(e) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:rtsp_feedback",
                        "RTSP POST /feedback failed",
                        "H35",
                        serde_json::json!({ "error": e.to_string() }),
                    );
                    // #endregion
                    break;
                }
            }
            drop(conn);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });

    let rtsp_heartbeat = rtsp_conn.clone();
    let heartbeat_uri = control_uri.clone();
    let heartbeat_session = session_uuid.clone();
    tasks.spawn(async move {
        if heartbeat_first_frame.recv().await.is_err() {
            return;
        }
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(15));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let mut conn = rtsp_heartbeat.lock().await;
            match conn
                .rtsp_get_parameter(&heartbeat_uri, &heartbeat_session)
                .await
            {
                Ok((status, body)) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:rtsp_heartbeat",
                        "RTSP GET_PARAMETER sent",
                        "H118",
                        serde_json::json!({
                            "httpStatus": status,
                            "bodyLen": body.len(),
                        }),
                    );
                    // #endregion
                    if status == 400 {
                        break;
                    }
                }
                Err(e) => {
                    // #region agent log
                    agent_log(
                        "mirror.rs:rtsp_heartbeat",
                        "RTSP GET_PARAMETER failed",
                        "H118",
                        serde_json::json!({ "error": e.to_string() }),
                    );
                    // #endregion
                    break;
                }
            }
        }
    });

    let direct_capture = if !config.test_mode
        && matches!(config.hw_accel, HwAccel::Auto | HwAccel::Nvenc)
        && FfmpegDirectCapture::available()
    {
        let display = rotten_capture::create_capture_backend(
            config.display_index,
            config.virtual_display_only,
        )
        .and_then(|capture| capture.displays())
        .ok()
        .and_then(|displays| displays.into_iter().next());

        let (width, height, native_size) = display
            .as_ref()
            .filter(|display| display.width == stream_config.width)
            .map(|display| (display.width, display.height, true))
            .unwrap_or((stream_config.width, stream_config.height, false));

        match FfmpegDirectCapture::start(
            config.display_index.unwrap_or(0),
            width,
            height,
            stream_config.fps,
            bitrate,
            native_size,
        ) {
            Ok(capture) => {
                info!(
                    width = capture.width(),
                    height = capture.height(),
                    fps = capture.fps(),
                    native_size,
                    "direct GPU capture and NVENC active"
                );
                Some(capture)
            }
            Err(error) => {
                warn!(%error, "direct GPU capture unavailable; using DXGI pipeline");
                None
            }
        }
    } else {
        None
    };

    if let Some(mut capture) = direct_capture {
        let output = capture
            .take_output()
            .ok_or_else(|| RottenError::Capture("direct capture output receiver missing".into()))?;
        let direct_width = capture.width();
        let direct_height = capture.height();
        let direct_fps = capture.fps();
        let (encoded_tx, encoded_rx) = tokio::sync::mpsc::channel(4);
        tasks.spawn_blocking(move || {
            while let Ok(data) = output.recv() {
                let is_keyframe = rotten_video::EncodedFrame::is_keyframe_data(&data);
                let captured_at_us = rotten_core::ntp::boot_duration().as_micros() as u64;
                let frame = EncodedFrame {
                    data,
                    pts_us: captured_at_us,
                    encode_duration_us: 0,
                    is_keyframe,
                    coded_width: direct_width,
                    coded_height: direct_height,
                    display_width: direct_width,
                    display_height: direct_height,
                };
                if encoded_tx.blocking_send(frame).is_err() {
                    break;
                }
            }
        });

        let stream_result = streamer
            .stream_encoded_from_channel_on(
                data_stream,
                direct_width,
                direct_height,
                direct_width,
                direct_height,
                video_timestamp_bias_samples,
                direct_fps,
                encoded_rx,
                Some(first_frame_tx),
            )
            .await;

        if let Some(audio) = audio_handle {
            if let Err(error) = audio.stop().await {
                warn!(%error, "failed to stop system audio capture");
            }
        }
        drop(capture);
        return stream_result;
    }

    let (frame_tx, frame_rx) = frame_channel();

    if !config.test_mode {
        let capture = rotten_capture::create_capture_backend(
            config.display_index,
            config.virtual_display_only,
        )?;
        let displays = capture.displays()?;
        if let Some(capture_display) = displays.first() {
            info!(
                name = %capture_display.name,
                width = capture_display.width,
                height = capture_display.height,
                target_width = stream_config.width,
                target_height = stream_config.height,
                virtual_display_only = config.virtual_display_only,
                "screen capture display selected"
            );
        }

        info!(backend = capture.backend_name(), "screen capture ready");

        let fps = stream_config.fps;
        let (target_width, target_height) =
            fit_stream_dims(stream_config.width, stream_config.height);
        let display_index = config.display_index;
        let virtual_display_only = config.virtual_display_only;
        let capture = std::sync::Arc::new(std::sync::Mutex::new(capture));
        let capture_worker = capture.clone();
        tasks.spawn(async move {
            let mut produced: u64 = 0;
            let mut capture_errors: u32 = 0;
            let mut last_watchdog = std::time::Instant::now();
            let mut frame_ticker = tokio::time::interval(frame_interval(fps));
            frame_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                frame_ticker.tick().await;
                if last_watchdog.elapsed() >= std::time::Duration::from_secs(2) {
                    // #region agent log
                    agent_log(
                        "mirror.rs:producer",
                        "capture producer alive",
                        "H22",
                        serde_json::json!({ "produced": produced }),
                    );
                    // #endregion
                    last_watchdog = std::time::Instant::now();
                }

                let cap = capture_worker.clone();
                let grabbed = tokio::task::spawn_blocking(move || {
                    let mut cap = cap.lock().expect("capture mutex");
                    cap.grab_frame()
                })
                .await;

                match grabbed {
                    Ok(Ok(frame)) => {
                        capture_errors = 0;
                        produced += 1;
                        let (cw, ch) = (target_width, target_height);
                        let rgba = if cw != frame.width || ch != frame.height {
                            if produced == 1 {
                                // #region agent log
                                agent_log(
                                    "mirror.rs:producer",
                                    "downscaling capture before encode queue",
                                    "H23",
                                    serde_json::json!({
                                        "fromW": frame.width,
                                        "fromH": frame.height,
                                        "toW": cw,
                                        "toH": ch,
                                    }),
                                );
                                // #endregion
                            }
                            downscale_rgba(&frame.rgba, frame.width, frame.height, cw, ch)
                        } else {
                            frame.rgba
                        };
                        // #region agent log
                        if produced == 1 {
                            let sample_len = rgba.len().min(4096);
                            let mut r_sum = 0u64;
                            let mut g_sum = 0u64;
                            let mut b_sum = 0u64;
                            let mut samples = 0u64;
                            for chunk in rgba[..sample_len].chunks_exact(4) {
                                r_sum += chunk[0] as u64;
                                g_sum += chunk[1] as u64;
                                b_sum += chunk[2] as u64;
                                samples += 1;
                            }
                            agent_log(
                                "mirror.rs:producer",
                                "first capture frame luma sample",
                                "H37",
                                serde_json::json!({
                                    "produced": produced,
                                    "width": cw,
                                    "height": ch,
                                    "avgR": if samples > 0 { r_sum / samples } else { 0 },
                                    "avgG": if samples > 0 { g_sum / samples } else { 0 },
                                    "avgB": if samples > 0 { b_sum / samples } else { 0 },
                                    "cornerRgba": format!(
                                        "{:02x}{:02x}{:02x}{:02x}",
                                        rgba[0], rgba[1], rgba[2], rgba[3]
                                    ),
                                }),
                            );
                        }
                        if produced == 1 || produced % 30 == 0 {
                            agent_log(
                                "mirror.rs:producer",
                                "capture frame queued for encoder",
                                "H2",
                                serde_json::json!({
                                    "produced": produced,
                                    "width": cw,
                                    "height": ch,
                                    "rgbaBytes": rgba.len(),
                                }),
                            );
                        }
                        // #endregion
                        let captured_at_us = rotten_core::ntp::boot_duration().as_micros() as u64;
                        frame_tx.send((rgba, cw, ch, captured_at_us));
                    }
                    Ok(Err(e)) => {
                        capture_errors = capture_errors.saturating_add(1);
                        if capture_errors == 1 {
                            tracing::warn!(error = %e, "capture error; attempting recovery");
                        }
                        if capture_errors >= 5 {
                            match rotten_capture::create_capture_backend(
                                display_index,
                                virtual_display_only,
                            ) {
                                Ok(replacement) => {
                                    *capture_worker.lock().expect("capture mutex") = replacement;
                                    capture_errors = 0;
                                    tracing::info!("screen capture backend recreated");
                                }
                                Err(recovery_error) => {
                                    if capture_errors % 10 == 0 {
                                        tracing::warn!(
                                            error = %recovery_error,
                                            "screen capture recovery failed"
                                        );
                                    }
                                }
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "capture task join error");
                    }
                }
            }
        });
    } else {
        let fps = stream_config.fps;
        let width = stream_config.width;
        let height = stream_config.height;
        tasks.spawn(async move {
            let mut synthetic = SyntheticSource::new(width, height);
            let mut frame_ticker = tokio::time::interval(frame_interval(fps));
            frame_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                frame_ticker.tick().await;
                match synthetic.next_frame() {
                    Ok((rgba, w, h)) => {
                        let captured_at_us = rotten_core::ntp::boot_duration().as_micros() as u64;
                        frame_tx.send((rgba, w, h, captured_at_us));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "synthetic frame error");
                        break;
                    }
                }
            }
        });
    }

    let (stream_w, stream_h) = fit_stream_dims(stream_config.width, stream_config.height);
    // Codec-header offsets 16/40 = coded picture size; 56/60 = visible presentation size.
    // Apple TV /info often omits display size — use capture dimensions (e.g. 1080), not coded pad (1088).
    let capture_w = stream_config.width & !1;
    let capture_h = stream_config.height & !1;
    // The codec packet's source and destination CGRects describe the encoded
    // raster, not the receiver's advertised panel. Reporting a 1920x1080
    // destination for a 1280x720 bitstream makes some receivers configure a
    // mismatched video layer and never present a frame.
    let (presentation_w, presentation_h, presentation_source) =
        (capture_w, capture_h, "encoded-raster");

    // #region agent log
    agent_log(
        "mirror.rs:run_mirror",
        "starting video streamer",
        "H0",
        serde_json::json!({
            "buildId": DEBUG_BUILD_ID,
            "width": stream_config.width,
            "height": stream_config.height,
            "streamW": stream_w,
            "streamH": stream_h,
            "presentationW": presentation_w,
            "presentationH": presentation_h,
            "presentationSource": presentation_source,
            "bitrateKbps": bitrate,
        }),
    );
    // #endregion

    let stream_result = streamer
        .stream_from_channel_on(
            data_stream,
            stream_w,
            stream_h,
            stream_w,
            stream_h,
            presentation_w,
            presentation_h,
            video_timestamp_bias_samples,
            stream_config.fps,
            bitrate,
            config.hw_accel,
            frame_rx,
            Some(first_frame_tx),
        )
        .await;

    if let Some(audio) = audio_handle {
        if let Err(error) = audio.stop().await {
            warn!(%error, "failed to stop system audio capture");
        }
    }

    stream_result
}

#[derive(Default)]
struct TaskGroup(Vec<JoinHandle<()>>);

impl TaskGroup {
    fn spawn<F>(&mut self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.0.push(tokio::spawn(future));
    }

    fn push(&mut self, handle: JoinHandle<()>) {
        self.0.push(handle);
    }

    fn spawn_blocking<F>(&mut self, future: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.0.push(tokio::task::spawn_blocking(future));
    }
}

impl Drop for TaskGroup {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

struct ReconnectBackoff {
    next: Duration,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            next: RECONNECT_BASE_DELAY,
        }
    }
}

impl ReconnectBackoff {
    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(RECONNECT_MAX_DELAY);
        delay
    }

    fn reset(&mut self) {
        self.next = RECONNECT_BASE_DELAY;
    }
}

fn feedback_plist_summary(body: &[u8]) -> serde_json::Value {
    let Ok(value) = plist::from_bytes::<plist::Value>(body) else {
        return serde_json::json!({ "parsed": false });
    };
    let Some(dict) = value.as_dictionary() else {
        return serde_json::json!({ "parsed": true, "root": "non-dict" });
    };
    let keys: Vec<&str> = dict.keys().map(String::as_str).collect();
    let mut out = serde_json::json!({ "parsed": true, "keys": keys });
    for key in [
        "status",
        "statusFlags",
        "error",
        "mirroring",
        "video",
        "audio",
        "reason",
    ] {
        if let Some(v) = dict.get(key) {
            out[key] = plist_value_json(v);
        }
    }
    if let Some(streams) = dict.get("streams").and_then(|v| v.as_array()) {
        let entries: Vec<serde_json::Value> = streams
            .iter()
            .filter_map(|s| s.as_dictionary())
            .map(|sd| {
                serde_json::json!({
                    "type": sd.get("type").and_then(|v| v.as_signed_integer()),
                    "buffered": sd.get("buffered").and_then(plist::Value::as_boolean),
                    "playing": sd.get("playing").and_then(plist::Value::as_boolean),
                    "ready": sd.get("ready").and_then(plist::Value::as_boolean),
                    "state": sd.get("state").map(plist_value_json),
                })
            })
            .collect();
        out["streams"] = serde_json::json!(entries);
    }
    out
}

fn plist_value_json(value: &plist::Value) -> serde_json::Value {
    match value {
        plist::Value::String(s) => serde_json::Value::String(s.clone()),
        plist::Value::Boolean(b) => serde_json::Value::Bool(*b),
        plist::Value::Integer(i) => serde_json::json!(i.as_signed().unwrap_or(0)),
        plist::Value::Real(f) => serde_json::json!(*f),
        plist::Value::Data(d) => serde_json::json!({ "dataLen": d.len() }),
        _ => serde_json::json!(format!("{value:?}")),
    }
}

fn frame_interval(fps: u32) -> Duration {
    Duration::from_secs_f64(1.0 / fps.max(1) as f64)
}

fn emit_status(status_tx: Option<&std::sync::mpsc::Sender<MirrorStatus>>, status: MirrorStatus) {
    if let Some(status_tx) = status_tx {
        let _ = status_tx.send(status);
    }
}

#[cfg(test)]
mod tests {
    use super::{RECONNECT_BASE_DELAY, RECONNECT_MAX_DELAY, ReconnectBackoff};

    #[test]
    fn reconnect_backoff_is_bounded_and_resettable() {
        let mut backoff = ReconnectBackoff::default();
        assert_eq!(backoff.next_delay(), RECONNECT_BASE_DELAY);
        assert_eq!(backoff.next_delay(), RECONNECT_BASE_DELAY * 2);
        for _ in 0..8 {
            backoff.next_delay();
        }
        assert_eq!(backoff.next_delay(), RECONNECT_MAX_DELAY);

        backoff.reset();
        assert_eq!(backoff.next_delay(), RECONNECT_BASE_DELAY);
    }
}
