use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use rotten_core::config::{
    MirrorCipherMode, MirrorConfig, StreamConfig, StreamPreset, resolve_credentials_path,
};
use rotten_core::debug_log::{DEBUG_BUILD_ID, agent_log};
use rotten_core::media::MediaSyncConfig;
use rotten_discovery::{discover_devices, discover_for, resolve_device};
use rotten_pairing::PairingManager;
use tracing::info;

use crate::mirror::{run_mirror, run_mirror_with_metrics};

#[derive(Parser)]
#[command(name = "airmirror")]
#[command(about = "Mirror or extend your PC display to Apple TV via AirPlay")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Write a boot marker and exit (diagnose startup hangs)
    Probe,
    /// Launch the desktop GUI
    Gui,
    /// Scan the LAN for AirPlay receivers (Apple TVs)
    Discover {
        /// Discovery timeout in seconds
        #[arg(long, default_value = "5")]
        timeout: u64,
    },
    /// Check local dependencies, credentials, and receiver reachability
    Doctor {
        /// Optional AirPlay receiver hostname or IP
        #[arg(short, long)]
        target: Option<String>,
        /// AirPlay port used with --target
        #[arg(long, default_value = "7000")]
        port: u16,
    },
    /// Pair with an AirPlay receiver using transient pair-setup
    Pair {
        /// Apple TV hostname or IP address
        #[arg(short, long)]
        target: String,
        /// AirPlay port
        #[arg(long, default_value = "7000")]
        port: u16,
        /// Force re-pairing even if credentials exist
        #[arg(long)]
        force: bool,
        /// Path to credentials file
        #[arg(long)]
        creds: Option<PathBuf>,
    },
    /// Mirror the PC screen to an Apple TV
    Mirror {
        /// Apple TV hostname or IP (skips mDNS discovery)
        #[arg(short, long)]
        target: Option<String>,
        /// AirPlay port
        #[arg(long, default_value = "7000")]
        port: u16,
        /// Stream quality preset
        #[arg(long, value_enum, default_value = "balanced")]
        preset: PresetArg,
        /// Stream width
        #[arg(long)]
        width: Option<u32>,
        /// Stream height
        #[arg(long)]
        height: Option<u32>,
        /// Frames per second
        #[arg(long)]
        fps: Option<u32>,
        /// Video bitrate in kbps (0 = auto)
        #[arg(long)]
        bitrate: Option<u32>,
        /// Hardware encoder: auto, nvenc, vaapi, none
        #[arg(long, default_value = "auto")]
        hwaccel: String,
        /// Use synthetic test pattern instead of screen capture
        #[arg(long)]
        test: bool,
        /// Enable system audio capture (default)
        #[arg(long, default_value_t = true)]
        audio: bool,
        /// Disable system audio capture
        #[arg(long)]
        no_audio: bool,
        /// Stop automatically after this many minutes and print a soak summary
        #[arg(long)]
        duration_minutes: Option<u64>,
        /// Stop automatically after this many seconds and print a soak summary
        #[arg(long, hide = true)]
        duration_seconds: Option<u64>,
        /// Delay audio relative to video in milliseconds (negative advances audio)
        #[arg(long, default_value_t = 10)]
        audio_offset_ms: i32,
        /// Force new pairing
        #[arg(long)]
        pair: bool,
        /// Capture only virtual displays (extend mode)
        #[arg(long)]
        virtual_display: bool,
        /// Display index to capture
        #[arg(long)]
        display: Option<u32>,
        /// Path to credentials file
        #[arg(long)]
        creds: Option<PathBuf>,
        /// Verbose debug logging
        #[arg(long)]
        debug: bool,
        /// Send video frames without encryption (debug cipher issues)
        #[arg(long)]
        no_encrypt: bool,
        /// Video cipher: aes (Apple TV-compatible) or cha-cha
        #[arg(long, value_enum, default_value = "aes")]
        cipher: CipherArg,
    },
}

#[derive(Clone, Copy, ValueEnum, Default)]
pub(crate) enum CipherArg {
    #[default]
    Aes,
    #[value(alias = "chacha")]
    ChaCha,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub(crate) enum PresetArg {
    Compatibility,
    #[default]
    Balanced,
    Quality,
    #[value(name = "2k30")]
    TwoK30,
    #[value(name = "2k60")]
    TwoK60,
    #[value(name = "ultrawide30")]
    Ultrawide30,
    #[value(name = "ultrawide60")]
    Ultrawide60,
    #[value(name = "4k30")]
    FourK30,
    #[value(name = "4k60")]
    FourK60,
}

impl From<PresetArg> for StreamPreset {
    fn from(value: PresetArg) -> Self {
        match value {
            PresetArg::Compatibility => StreamPreset::Compatibility,
            PresetArg::Balanced => StreamPreset::Balanced,
            PresetArg::Quality => StreamPreset::Quality,
            PresetArg::TwoK30 => StreamPreset::TwoK30,
            PresetArg::TwoK60 => StreamPreset::TwoK60,
            PresetArg::Ultrawide30 => StreamPreset::Ultrawide30,
            PresetArg::Ultrawide60 => StreamPreset::Ultrawide60,
            PresetArg::FourK30 => StreamPreset::FourK30,
            PresetArg::FourK60 => StreamPreset::FourK60,
        }
    }
}

impl From<CipherArg> for MirrorCipherMode {
    fn from(v: CipherArg) -> Self {
        match v {
            CipherArg::Aes => MirrorCipherMode::AesCtr,
            CipherArg::ChaCha => MirrorCipherMode::ChaCha,
        }
    }
}

impl Cli {
    pub async fn run(self) -> anyhow::Result<()> {
        match self.command {
            Commands::Probe => {}
            Commands::Gui => {
                let exe = std::env::current_exe()?;
                let gui = exe
                    .parent()
                    .map(|directory| directory.join("airmirror-gui.exe"))
                    .ok_or_else(|| anyhow::anyhow!("cannot locate airmirror-gui.exe"))?;
                std::process::Command::new(&gui)
                    .spawn()
                    .map_err(|error| anyhow::anyhow!("launch {}: {error}", gui.display()))?;
            }
            Commands::Discover { timeout } => {
                let devices = discover_for(Duration::from_secs(timeout)).await?;
                if devices.is_empty() {
                    println!("No AirPlay devices found.");
                } else {
                    println!("Found {} AirPlay device(s):\n", devices.len());
                    for d in &devices {
                        println!("  {} — {}:{} ({})", d.name, d.host, d.port, d.device_id);
                        if let Some(model) = &d.model {
                            println!("    model: {model}");
                        }
                    }
                }
            }
            Commands::Doctor { target, port } => {
                let exe = std::env::current_exe()?;
                let helper = exe
                    .parent()
                    .map(|directory| directory.join("fpsap-helper.exe"));
                let credentials = resolve_credentials_path(None);
                println!("executable: {}", exe.display());
                println!(
                    "fpsap helper: {}",
                    helper
                        .as_ref()
                        .filter(|path| path.exists())
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "missing".into())
                );
                println!("credentials: {}", credentials.display());
                println!("credentials present: {}", credentials.exists());
                let ffmpeg = std::process::Command::new("ffmpeg")
                    .args(["-hide_banner", "-version"])
                    .output();
                let ffmpeg_available = ffmpeg.as_ref().is_ok_and(|output| output.status.success());
                println!("ffmpeg available: {ffmpeg_available}");
                if ffmpeg_available {
                    let encoders = std::process::Command::new("ffmpeg")
                        .args(["-hide_banner", "-encoders"])
                        .output()
                        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                        .unwrap_or_default();
                    let filters = std::process::Command::new("ffmpeg")
                        .args(["-hide_banner", "-filters"])
                        .output()
                        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                        .unwrap_or_default();
                    println!("h264_nvenc: {}", encoders.contains("h264_nvenc"));
                    println!("ddagrab: {}", filters.contains("ddagrab"));
                }

                if let Some(target) = target {
                    let device = resolve_device(&target, port).await?;
                    println!("receiver: {}:{}", device.host, device.port);
                    println!("receiver name: {}", device.name);
                    println!(
                        "receiver model: {}",
                        device.model.as_deref().unwrap_or("unknown")
                    );
                    println!("receiver features: 0x{:x}", device.features.raw);
                    println!(
                        "receiver screen audio formats: {}",
                        device
                            .screen_audio_formats
                            .map(|formats| format!("0x{formats:x}"))
                            .unwrap_or_else(|| "unknown".into())
                    );
                    println!(
                        "receiver ALAC screen audio: {}",
                        match device.supports_screen_alac() {
                            Some(true) => "yes",
                            Some(false) => "no",
                            None => "unknown",
                        }
                    );
                } else {
                    println!("receiver check: skipped (use --target)");
                }
            }
            Commands::Pair {
                target,
                port,
                force,
                creds,
            } => {
                let device = resolve_device(&target, port).await?;
                let creds_path = resolve_credentials_path(creds);
                let mut manager = PairingManager::load(creds_path)?;
                let stored = manager.pair(&device, force).await?;
                println!("Paired with {} ({})", device.name, stored.device_id);
            }
            Commands::Mirror {
                target,
                port,
                preset,
                width,
                height,
                fps,
                bitrate,
                hwaccel,
                test,
                audio,
                no_audio,
                duration_minutes,
                duration_seconds,
                audio_offset_ms,
                pair,
                virtual_display,
                display,
                creds,
                debug,
                no_encrypt,
                cipher,
            } => {
                // #region agent log
                agent_log(
                    "cli.rs:mirror",
                    "mirror command starting",
                    "H17",
                    serde_json::json!({
                        "buildId": DEBUG_BUILD_ID,
                        "target": target.as_deref(),
                        "test": test,
                        "noEncrypt": no_encrypt,
                        "cipher": match cipher {
                            CipherArg::Aes => "aes",
                            CipherArg::ChaCha => "chacha",
                        },
                    }),
                );
                // #endregion

                if debug {
                    tracing::subscriber::set_global_default(
                        tracing_subscriber::fmt().with_env_filter("debug").finish(),
                    )
                    .ok();
                }

                let device = if let Some(t) = target {
                    // #region agent log
                    agent_log(
                        "cli.rs:mirror",
                        "resolving target",
                        "H17",
                        serde_json::json!({ "target": &t, "port": port }),
                    );
                    // #endregion
                    let device = resolve_device(&t, port).await?;
                    // #region agent log
                    agent_log(
                        "cli.rs:mirror",
                        "target resolved",
                        "H17",
                        serde_json::json!({
                            "name": device.name,
                            "host": device.host,
                        }),
                    );
                    // #endregion
                    device
                } else {
                    let devices = discover_devices().await?;
                    devices
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("no AirPlay devices found; use --target"))?
                };

                info!(name = %device.name, host = %device.host, "selected device");

                let preset_config = StreamPreset::from(preset).config();
                let config = MirrorConfig {
                    stream: StreamConfig {
                        width: width.unwrap_or(preset_config.width),
                        height: height.unwrap_or(preset_config.height),
                        fps: fps.unwrap_or(preset_config.fps),
                        bitrate_kbps: bitrate.unwrap_or(preset_config.bitrate_kbps),
                    },
                    force_pair: pair,
                    test_mode: test,
                    audio: audio && !no_audio,
                    hw_accel: rotten_core::config::HwAccel::from_str(&hwaccel),
                    credentials_path: resolve_credentials_path(creds),
                    display_index: display,
                    virtual_display_only: virtual_display,
                    no_encrypt,
                    cipher: cipher.into(),
                    sync: MediaSyncConfig { audio_offset_ms },
                };

                if let Some(duration) = duration_seconds.map(Duration::from_secs).or_else(|| {
                    duration_minutes.map(|minutes| Duration::from_secs(minutes.saturating_mul(60)))
                }) {
                    let metrics =
                        std::sync::Arc::new(rotten_core::metrics::MirrorMetrics::default());
                    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
                    let target_seconds = duration.as_secs();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {}
                            _ = tokio::time::sleep(duration) => {}
                        }
                        let _ = shutdown_tx.send(true);
                    });
                    run_mirror_with_metrics(device, config, shutdown_rx, None, metrics.clone())
                        .await?;
                    let snapshot = metrics.snapshot();
                    println!("Soak summary:");
                    println!("  duration target: {target_seconds} s");
                    println!("  video frames:    {}", snapshot.video_frames);
                    println!("  final FPS:       {}", snapshot.video_fps);
                    println!("  dropped frames:  {}", snapshot.video_dropped);
                    println!("  audio real:      {}", snapshot.audio_real_frames);
                    println!("  audio silent:    {}", snapshot.audio_silent_frames);
                    println!("  audio queue:     {}", snapshot.audio_queue_depth);
                    println!("  audio sync:      {} ms", snapshot.audio_sync_error_ms);
                    println!("  video slack:     {} ms", snapshot.video_playout_slack_ms);
                    println!("  A/V skew:        {} ms", snapshot.av_skew_ms);
                    println!("  max |A/V skew|:  {} ms", snapshot.max_abs_av_skew_ms);
                    println!(
                        "  clock trim:      {} us/s",
                        snapshot.timeline_correction_us
                    );
                    println!(
                        "  CPU cores:       {:.2}",
                        snapshot.cpu_permille as f64 / 1000.0
                    );
                    println!(
                        "  working set:     {:.1} MB",
                        snapshot.working_set_bytes as f64 / 1_048_576.0
                    );
                    println!(
                        "  private memory:  {:.1} MB",
                        snapshot.private_memory_bytes as f64 / 1_048_576.0
                    );
                    println!("  reconnects:      {}", snapshot.reconnects);
                } else {
                    run_mirror(device, config).await?;
                }
            }
        }
        Ok(())
    }
}
