use std::{
    net::{IpAddr, ToSocketAddrs},
    path::PathBuf,
    time::Duration,
};

use airmirror_airplay::{AIRPLAY_DEFAULT_PORT, AirPlayInfoClient};
use airmirror_core::Receiver;
use airmirror_discovery::{AIRPLAY_SERVICE, discover};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rotten_core::config::{HwAccel, MirrorCipherMode, MirrorConfig, StreamConfig, StreamPreset};
use rotten_core::device::{AirPlayDevice, DeviceFeatures};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "airmirror",
    version,
    about = "Windows AirPlay screen mirroring sender"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "AIRMIRROR_LOG",
        default_value = "airmirror=info"
    )]
    log: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Discover AirPlay receivers on the local network.
    Discover {
        /// Browse duration in seconds.
        #[arg(long, default_value_t = 5)]
        timeout: u64,

        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,

        /// Include receivers that do not advertise screen mirroring support.
        #[arg(long)]
        all: bool,
    },

    /// Query an AirPlay receiver using the /info endpoint.
    Info {
        /// Receiver IPv4 or IPv6 address.
        #[arg(long)]
        target: IpAddr,

        /// AirPlay control port.
        #[arg(long, default_value_t = AIRPLAY_DEFAULT_PORT)]
        port: u16,

        /// Network timeout in seconds.
        #[arg(long, default_value_t = 3)]
        timeout: u64,

        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Print environment and media backend status.
    Doctor,

    /// Pair and mirror the Windows desktop to an AirPlay receiver.
    Mirror(MirrorArgs),
}

#[derive(Debug, clap::Args)]
struct MirrorArgs {
    /// Receiver IP address or hostname.
    #[arg(short, long)]
    target: String,

    /// AirPlay control port.
    #[arg(long, default_value_t = AIRPLAY_DEFAULT_PORT)]
    port: u16,

    /// Stream quality preset.
    #[arg(long, value_enum, default_value = "balanced")]
    preset: PresetArg,

    /// Force a new pairing even if saved credentials exist.
    #[arg(long)]
    pair: bool,

    /// Stream width.
    #[arg(long)]
    width: Option<u32>,

    /// Stream height.
    #[arg(long)]
    height: Option<u32>,

    /// Frames per second.
    #[arg(long)]
    fps: Option<u32>,

    /// Video bitrate in kbps; zero selects automatically.
    #[arg(long)]
    bitrate: Option<u32>,

    /// Display index to capture.
    #[arg(long, default_value_t = 0)]
    display: u32,

    /// Send a synthetic test pattern instead of capturing the desktop.
    #[arg(long)]
    test: bool,

    /// Enable system audio mirroring.
    #[arg(long, default_value_t = true)]
    audio: bool,

    /// Disable system audio mirroring.
    #[arg(long)]
    no_audio: bool,

    /// Override the credential file.
    #[arg(long)]
    credentials: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum PresetArg {
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
            PresetArg::Compatibility => Self::Compatibility,
            PresetArg::Balanced => Self::Balanced,
            PresetArg::Quality => Self::Quality,
            PresetArg::TwoK30 => Self::TwoK30,
            PresetArg::TwoK60 => Self::TwoK60,
            PresetArg::Ultrawide30 => Self::Ultrawide30,
            PresetArg::Ultrawide60 => Self::Ultrawide60,
            PresetArg::FourK30 => Self::FourK30,
            PresetArg::FourK60 => Self::FourK60,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_target(false)
        .compact()
        .init();

    match cli.command {
        Command::Discover { timeout, json, all } => {
            let timeout = Duration::from_secs(timeout.max(1));
            tracing::info!(service = AIRPLAY_SERVICE, "browsing AirPlay receivers");
            let mut receivers = discover(timeout).context("mDNS discovery failed")?;
            if !all {
                receivers.retain(Receiver::supports_screen_mirroring);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&receivers)?);
            } else {
                print_receivers(&receivers);
            }
        }
        Command::Info {
            target,
            port,
            timeout,
            json,
        } => {
            let client = AirPlayInfoClient::with_timeout(Duration::from_secs(timeout.max(1)));
            let info = client
                .probe(target, port)
                .with_context(|| format!("failed to query {target}:{port}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                println!("Name:             {}", info.name);
                println!("Model:            {}", info.model);
                println!("Device ID:        {}", info.device_id);
                println!(
                    "Features:         {}",
                    info.features
                        .map(|value| format!("0x{value:016X}"))
                        .unwrap_or_else(|| "unknown".into())
                );
                println!(
                    "Screen mirroring: {}",
                    yes_no_unknown(info.supports_screen_mirroring())
                );
                println!(
                    "Audio:            {}",
                    yes_no_unknown(info.supports_audio())
                );
                println!(
                    "Screen audio:     {}",
                    info.screen_audio_formats
                        .map(|formats| format!("0x{formats:X}"))
                        .unwrap_or_else(|| "unknown".into())
                );
                if let Some(value) = info.protocol_version {
                    println!("Protocol:         {value}");
                }
                if let Some(value) = info.source_version {
                    println!("Source version:   {value}");
                }
                println!("Plist keys:       {}", info.raw_keys.join(", "));
            }
        }
        Command::Doctor => {
            println!("platform: {}", airmirror_windows::platform_name());
            println!("airplay default port: {AIRPLAY_DEFAULT_PORT}");
            println!("mdns service: {AIRPLAY_SERVICE}");
        }
        Command::Mirror(args) => run_mirror(args)?,
    }
    Ok(())
}

fn run_mirror(args: MirrorArgs) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create Tokio runtime")?;

    runtime.block_on(async move {
        let target_ip = resolve_ipv4(&args.target, args.port)?;
        let info = AirPlayInfoClient::with_timeout(Duration::from_secs(5))
            .probe(target_ip, args.port)
            .with_context(|| format!("failed to query AirPlay /info for {target_ip}"))?;
        let device = AirPlayDevice {
            name: info.name.clone(),
            host: target_ip.to_string(),
            port: args.port,
            device_id: if info.device_id.is_empty() {
                target_ip.to_string()
            } else {
                info.device_id.to_ascii_uppercase()
            },
            model: Some(info.model.clone()),
            features: DeviceFeatures {
                raw: info.features.unwrap_or_default(),
            },
            display_width: info.display_width,
            display_height: info.display_height,
            screen_audio_formats: info.screen_audio_formats,
            pi: info.pi,
            pk: info.pk,
        };
        let preset_config = StreamPreset::from(args.preset).config();
        let config = MirrorConfig {
            stream: StreamConfig {
                width: args.width.unwrap_or(preset_config.width),
                height: args.height.unwrap_or(preset_config.height),
                fps: args.fps.unwrap_or(preset_config.fps),
                bitrate_kbps: args.bitrate.unwrap_or(preset_config.bitrate_kbps),
            },
            force_pair: args.pair,
            test_mode: args.test,
            audio: args.audio && !args.no_audio,
            hw_accel: HwAccel::Auto,
            credentials_path: args.credentials.unwrap_or_else(default_credentials_path),
            display_index: Some(args.display),
            virtual_display_only: false,
            no_encrypt: false,
            cipher: MirrorCipherMode::AesCtr,
            sync: rotten_core::MediaSyncConfig::default(),
        };
        rotten_app::mirror::run_mirror(device, config)
            .await
            .context("AirPlay mirror session failed")
    })?;
    Ok(())
}

fn default_credentials_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("airmirror")
        .join("credentials.json")
}

fn resolve_ipv4(target: &str, port: u16) -> Result<IpAddr> {
    (target, port)
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve {target}:{port}"))?
        .find(|address| address.is_ipv4())
        .map(|address| address.ip())
        .with_context(|| format!("{target}:{port} has no IPv4 address"))
}

fn print_receivers(receivers: &[Receiver]) {
    if receivers.is_empty() {
        println!("No AirPlay screen receivers found.");
        return;
    }

    println!(
        "{:<32} {:<22} {:<24} {:<8}",
        "NAME", "MODEL", "ADDRESS", "SCREEN"
    );
    for receiver in receivers {
        println!(
            "{:<32} {:<22} {:<24} {:<8}",
            truncate(&receiver.name, 32),
            truncate(&receiver.model, 22),
            receiver.display_address(),
            yes_no_unknown(receiver.capabilities.screen_mirroring),
        );
    }
}

fn yes_no_unknown(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let mut truncated: String = value.chars().take(width.saturating_sub(1)).collect();
    truncated.push('~');
    truncated
}
