#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use clap::Parser;
use eframe::egui;
use rotten_app::mirror::{MirrorStatus, run_mirror_with_shutdown};
use rotten_core::config::{
    HwAccel, MirrorCipherMode, MirrorConfig, StreamConfig, StreamPreset, default_credentials_path,
};
use rotten_core::device::AirPlayDevice;
use rotten_core::media::{AUDIO_OFFSET_MAX_MS, AUDIO_OFFSET_MIN_MS, MediaSyncConfig};
use rotten_core::metrics::MirrorMetricsSnapshot;
use rotten_discovery::{discover_for, resolve_device};
use serde::{Deserialize, Serialize};

const PRESETS: &[StreamPreset] = &[
    StreamPreset::Compatibility,
    StreamPreset::Balanced,
    StreamPreset::Quality,
    StreamPreset::TwoK30,
    StreamPreset::TwoK60,
    StreamPreset::Ultrawide30,
    StreamPreset::Ultrawide60,
    StreamPreset::FourK30,
    StreamPreset::FourK60,
];
const DEFAULT_PORT: u16 = 3553;
const DEFAULT_NATIVE_FPS: u32 = 30;

#[derive(Debug, Parser)]
#[command(
    name = "airmirror-gui",
    version,
    about = "AirMirror desktop controller"
)]
struct GuiArgs {
    /// Receiver IP address or hostname.
    #[arg(short, long)]
    target: Option<String>,

    /// AirPlay control port.
    #[arg(long)]
    port: Option<u16>,

    /// Start mirroring as soon as the window opens.
    #[arg(long)]
    autostart: bool,

    /// Scan for AirPlay receivers as soon as the window opens.
    #[arg(long)]
    discover: bool,

    /// Use the synthetic test pattern.
    #[arg(long)]
    test: bool,
}

fn main() -> eframe::Result {
    let args = GuiArgs::parse();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 700.0])
            .with_min_inner_size([620.0, 600.0])
            .with_title("AirMirror"),
        ..Default::default()
    };

    eframe::run_native(
        "AirMirror",
        options,
        Box::new(move |cc| {
            let mut app = AirMirrorApp::new(cc, &args);
            if args.discover || !args.autostart {
                app.discover();
            }
            if args.autostart {
                app.start();
            }
            Ok(Box::new(app))
        }),
    )
}

struct AirMirrorApp {
    target: String,
    port: u16,
    preset_index: usize,
    native_resolution: bool,
    native_fps: u32,
    native_width: u32,
    native_height: u32,
    native_display_name: String,
    language: Language,
    audio_enabled: bool,
    audio_offset_ms: i32,
    hardware: HwAccel,
    test_mode: bool,
    state: UiState,
    detail: DetailMessage,
    metrics: MirrorMetricsSnapshot,
    devices: Vec<DeviceChoice>,
    discovering: bool,
    event_tx: Sender<GuiEvent>,
    events: Receiver<GuiEvent>,
    session: Option<SessionHandle>,
}

struct SessionHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    thread: JoinHandle<()>,
}

enum GuiEvent {
    Status(MirrorStatus),
    Devices(Result<Vec<DeviceChoice>, String>),
    Fatal(String),
}

#[derive(Clone)]
struct DeviceChoice {
    name: String,
    host: String,
    port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GuiSettings {
    target: String,
    port: u16,
    preset_index: usize,
    #[serde(default = "default_true")]
    native_resolution: bool,
    #[serde(default = "default_native_fps")]
    native_fps: u32,
    #[serde(default)]
    language: Language,
    audio_enabled: bool,
    audio_offset_ms: i32,
    hardware: String,
    test_mode: bool,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            target: String::new(),
            port: DEFAULT_PORT,
            preset_index: 0,
            native_resolution: true,
            native_fps: DEFAULT_NATIVE_FPS,
            language: Language::Zh,
            audio_enabled: true,
            audio_offset_ms: 10,
            hardware: "auto".into(),
            test_mode: false,
        }
    }
}

impl GuiSettings {
    fn load() -> Self {
        let Some(path) = gui_settings_path() else {
            return Self::default();
        };
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .map(|mut settings: Self| {
                settings.preset_index = settings.preset_index.min(PRESETS.len() - 1);
                settings.native_fps = normalize_native_fps(settings.native_fps);
                settings.audio_offset_ms = settings
                    .audio_offset_ms
                    .clamp(AUDIO_OFFSET_MIN_MS, AUDIO_OFFSET_MAX_MS);
                settings
            })
            .unwrap_or_default()
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = gui_settings_path() else {
            return Err("cannot resolve the user config directory".into());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(path, data).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum Language {
    #[default]
    Zh,
    En,
}

impl Language {
    fn pick(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }
}

#[derive(Debug)]
enum DetailMessage {
    Ready,
    Searching,
    Resolving {
        target: String,
        port: u16,
    },
    Opening {
        attempt: u32,
    },
    Streaming {
        attempt: u32,
        width: u32,
        height: u32,
        fps: u32,
    },
    Retrying {
        attempt: u32,
        delay_seconds: f32,
        error: String,
    },
    Stopping,
    Stopped,
    NoDevices,
    FoundDevices(usize),
    DiscoveryFailed(String),
    NoTarget,
    WorkerStartFailed,
    SessionFailed(String),
}

impl DetailMessage {
    fn text(&self, language: Language) -> String {
        match self {
            Self::Ready => language
                .pick(
                    "已就绪，可扫描或直接开始投屏",
                    "Ready to scan or start mirroring",
                )
                .into(),
            Self::Searching => language
                .pick(
                    "正在扫描同一网络中的投屏设备...",
                    "Searching for AirPlay devices on the local network...",
                )
                .into(),
            Self::Resolving { target, port } => match language {
                Language::Zh => format!("正在连接 {target}:{port}"),
                Language::En => format!("Connecting to {target}:{port}"),
            },
            Self::Opening { attempt } => match language {
                Language::Zh => format!("正在建立 AirPlay 会话，第 {attempt} 次尝试"),
                Language::En => format!("Opening AirPlay session, attempt {attempt}"),
            },
            Self::Streaming {
                attempt,
                width,
                height,
                fps,
            } => match language {
                Language::Zh => {
                    format!("正在投屏 {width} x {height} @ {fps} FPS，第 {attempt} 次连接")
                }
                Language::En => {
                    format!("Streaming {width} x {height} at {fps} FPS, attempt {attempt}")
                }
            },
            Self::Retrying {
                attempt,
                delay_seconds,
                error,
            } => match language {
                Language::Zh => {
                    format!("第 {attempt} 次连接结束：{error}。{delay_seconds:.1} 秒后重试")
                }
                Language::En => {
                    format!("Attempt {attempt} ended: {error}. Retrying in {delay_seconds:.1}s")
                }
            },
            Self::Stopping => language
                .pick("正在关闭投屏会话", "Closing the mirror session")
                .into(),
            Self::Stopped => language.pick("投屏已停止", "Session stopped").into(),
            Self::NoDevices => language
                .pick(
                    "未发现可投屏设备，请确认电脑和电视连接在同一网络",
                    "No screen mirroring devices found. Check that the PC and TV share a network.",
                )
                .into(),
            Self::FoundDevices(count) => match language {
                Language::Zh => format!("发现 {count} 台可投屏设备"),
                Language::En => format!("Found {count} screen mirroring device(s)"),
            },
            Self::DiscoveryFailed(error) => match language {
                Language::Zh => format!("设备扫描失败：{error}"),
                Language::En => format!("Device discovery failed: {error}"),
            },
            Self::NoTarget => language
                .pick(
                    "请先选择一台可投屏设备，或在高级设置中填写地址",
                    "Select a device, or enter a receiver address in Advanced settings",
                )
                .into(),
            Self::WorkerStartFailed => language
                .pick("无法启动投屏线程", "Could not start the mirror worker")
                .into(),
            Self::SessionFailed(error) => error.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UiState {
    Idle,
    Connecting,
    Streaming,
    Reconnecting,
    Stopping,
    Error,
}

impl UiState {
    fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Idle, Language::Zh) => "空闲",
            (Self::Idle, Language::En) => "Idle",
            (Self::Connecting, Language::Zh) => "连接中",
            (Self::Connecting, Language::En) => "Connecting",
            (Self::Streaming, Language::Zh) => "投屏中",
            (Self::Streaming, Language::En) => "Streaming",
            (Self::Reconnecting, Language::Zh) => "重连中",
            (Self::Reconnecting, Language::En) => "Reconnecting",
            (Self::Stopping, Language::Zh) => "停止中",
            (Self::Stopping, Language::En) => "Stopping",
            (Self::Error, Language::Zh) => "错误",
            (Self::Error, Language::En) => "Error",
        }
    }

    fn color(self, visuals: &egui::Visuals) -> egui::Color32 {
        match self {
            Self::Streaming => egui::Color32::from_rgb(52, 168, 110),
            Self::Connecting | Self::Reconnecting => egui::Color32::from_rgb(214, 158, 46),
            Self::Error => visuals.error_fg_color,
            Self::Idle | Self::Stopping => visuals.weak_text_color(),
        }
    }
}

impl AirMirrorApp {
    fn new(cc: &eframe::CreationContext<'_>, args: &GuiArgs) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(24, 27, 31);
        visuals.window_fill = visuals.panel_fill;
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(38, 43, 49);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(50, 58, 66);
        visuals.selection.bg_fill = egui::Color32::from_rgb(41, 112, 158);
        cc.egui_ctx.set_visuals(visuals);
        install_cjk_font(&cc.egui_ctx);

        let (event_tx, events) = mpsc::channel();
        let settings = GuiSettings::load();
        let (native_width, native_height, native_display_name) = detect_primary_display();
        Self {
            target: args.target.clone().unwrap_or(settings.target),
            port: args.port.unwrap_or(settings.port),
            preset_index: settings.preset_index,
            native_resolution: settings.native_resolution,
            native_fps: settings.native_fps,
            native_width,
            native_height,
            native_display_name,
            language: settings.language,
            audio_enabled: settings.audio_enabled,
            audio_offset_ms: settings
                .audio_offset_ms
                .clamp(AUDIO_OFFSET_MIN_MS, AUDIO_OFFSET_MAX_MS),
            hardware: HwAccel::from_str(&settings.hardware),
            test_mode: args.test || settings.test_mode,
            state: UiState::Idle,
            detail: DetailMessage::Ready,
            metrics: MirrorMetricsSnapshot::default(),
            devices: Vec::new(),
            discovering: false,
            event_tx,
            events,
            session: None,
        }
    }

    fn running(&self) -> bool {
        self.session.is_some()
    }

    fn stream_config(&self) -> StreamConfig {
        if self.native_resolution {
            let (width, height) = encoder_dimensions(self.native_width, self.native_height);
            StreamConfig {
                width,
                height,
                fps: self.native_fps,
                bitrate_kbps: 0,
            }
        } else {
            PRESETS[self.preset_index].config()
        }
    }

    fn start(&mut self) {
        if self.running() {
            return;
        }
        if self.target.trim().is_empty() {
            self.detail = DetailMessage::NoTarget;
            return;
        }
        self.save_settings();

        let stream = self.stream_config();
        let config = MirrorConfig {
            stream,
            force_pair: false,
            test_mode: self.test_mode,
            audio: self.audio_enabled,
            hw_accel: self.hardware,
            credentials_path: default_credentials_path(),
            display_index: None,
            virtual_display_only: false,
            no_encrypt: false,
            cipher: MirrorCipherMode::AesCtr,
            sync: MediaSyncConfig {
                audio_offset_ms: self.audio_offset_ms,
            },
        };

        let target = self.target.trim().to_string();
        let port = self.port;
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let thread_events = self.event_tx.clone();
        let event_tx = self.event_tx.clone();
        let thread = std::thread::Builder::new()
            .name("airmirror-gui-session".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = thread_events.send(GuiEvent::Fatal(error.to_string()));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let device = match resolve_device(&target, port).await {
                        Ok(device) => device,
                        Err(error) => {
                            let _ = thread_events.send(GuiEvent::Fatal(error.to_string()));
                            return;
                        }
                    };
                    let (status_tx, status_rx) = mpsc::channel::<MirrorStatus>();
                    let bridge_events = event_tx.clone();
                    std::thread::spawn(move || {
                        for status in status_rx {
                            if bridge_events.send(GuiEvent::Status(status)).is_err() {
                                break;
                            }
                        }
                    });
                    if let Err(error) =
                        run_mirror_with_shutdown(device, config, shutdown_rx, Some(status_tx)).await
                    {
                        let _ = event_tx.send(GuiEvent::Fatal(error.to_string()));
                    }
                });
            })
            .ok();

        if let Some(thread) = thread {
            self.session = Some(SessionHandle { shutdown, thread });
            self.state = UiState::Connecting;
            self.detail = DetailMessage::Resolving {
                target: self.target.trim().to_string(),
                port: self.port,
            };
            self.metrics = MirrorMetricsSnapshot::default();
        } else {
            self.state = UiState::Error;
            self.detail = DetailMessage::WorkerStartFailed;
        }
    }

    fn stop(&mut self) {
        if let Some(session) = &self.session {
            let _ = session.shutdown.send(true);
            self.state = UiState::Stopping;
            self.detail = DetailMessage::Stopping;
        }
    }

    fn discover(&mut self) {
        if self.discovering {
            return;
        }
        self.discovering = true;
        self.detail = DetailMessage::Searching;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = event_tx.send(GuiEvent::Devices(Err(error.to_string())));
                    return;
                }
            };
            let result = runtime.block_on(async {
                discover_for(Duration::from_secs(5))
                    .await
                    .map(|devices| {
                        devices
                            .into_iter()
                            .filter(supports_screen_mirroring)
                            .map(|device| DeviceChoice {
                                name: device.name,
                                host: device.host,
                                port: device.port,
                            })
                            .collect()
                    })
                    .map_err(|error| error.to_string())
            });
            let _ = event_tx.send(GuiEvent::Devices(result));
        });
    }

    fn current_settings(&self) -> GuiSettings {
        GuiSettings {
            target: self.target.clone(),
            port: self.port,
            preset_index: self.preset_index,
            native_resolution: self.native_resolution,
            native_fps: self.native_fps,
            language: self.language,
            audio_enabled: self.audio_enabled,
            audio_offset_ms: self.audio_offset_ms,
            hardware: hardware_key(self.hardware).into(),
            test_mode: self.test_mode,
        }
    }

    fn save_settings(&self) {
        if let Err(error) = self.current_settings().save() {
            tracing::warn!(%error, "failed to save GUI settings");
        }
    }

    fn poll_events(&mut self, ctx: &egui::Context) {
        let received: Vec<_> = self.events.try_iter().collect();
        for event in received {
            match event {
                GuiEvent::Status(MirrorStatus::Connecting { attempt }) => {
                    self.state = UiState::Connecting;
                    self.detail = DetailMessage::Opening { attempt };
                }
                GuiEvent::Status(MirrorStatus::Streaming { attempt }) => {
                    self.state = UiState::Streaming;
                    let stream = self.stream_config();
                    self.detail = DetailMessage::Streaming {
                        attempt,
                        width: stream.width,
                        height: stream.height,
                        fps: stream.fps,
                    };
                }
                GuiEvent::Status(MirrorStatus::Reconnecting {
                    attempt,
                    delay,
                    error,
                }) => {
                    self.state = UiState::Reconnecting;
                    self.detail = DetailMessage::Retrying {
                        attempt,
                        delay_seconds: delay.as_secs_f32(),
                        error,
                    };
                }
                GuiEvent::Status(MirrorStatus::Stopped) => {
                    self.state = UiState::Idle;
                    self.detail = DetailMessage::Stopped;
                    self.session = None;
                }
                GuiEvent::Status(MirrorStatus::Metrics(metrics)) => {
                    self.metrics = metrics;
                }
                GuiEvent::Devices(Ok(devices)) => {
                    self.discovering = false;
                    self.detail = if devices.is_empty() {
                        DetailMessage::NoDevices
                    } else {
                        DetailMessage::FoundDevices(devices.len())
                    };
                    if !devices
                        .iter()
                        .any(|device| device.host == self.target && device.port == self.port)
                        && let Some(first) = devices.first()
                    {
                        self.target = first.host.clone();
                        self.port = first.port;
                    }
                    self.devices = devices;
                }
                GuiEvent::Devices(Err(error)) => {
                    self.discovering = false;
                    self.detail = DetailMessage::DiscoveryFailed(error);
                }
                GuiEvent::Fatal(error) => {
                    self.state = UiState::Error;
                    self.detail = DetailMessage::SessionFailed(error);
                    self.session = None;
                }
            }
        }
        if self.running() || self.discovering {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl eframe::App for AirMirrorApp {
    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        self.poll_events(&ctx);

        egui::CentralPanel::default().show(root_ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal(|ui| {
                ui.heading("AirMirror");
                ui.add_space(12.0);
                let color = self.state.color(ui.visuals());
                ui.colored_label(color, format!("[{}]", self.state.label(self.language)));
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let mut language = self.language;
                        egui::ComboBox::from_id_salt("language")
                            .selected_text(match language {
                                Language::Zh => "中文",
                                Language::En => "English",
                            })
                            .width(96.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut language, Language::Zh, "中文");
                                ui.selectable_value(&mut language, Language::En, "English");
                            });
                        self.language = language;
                        ui.label(self.language.pick("语言", "Language"));
                    },
                );
            });
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(6.0);

            let language = self.language;
            let settings_enabled = !self.running();
            ui.add_enabled_ui(settings_enabled, |ui| {
                let discovered = self.devices.clone();
                ui.horizontal(|ui| {
                    ui.label(language.pick("投屏设备", "Device"));
                    let selected_name = discovered
                        .iter()
                        .find(|device| device.host == self.target && device.port == self.port)
                        .map(|device| device.name.as_str())
                        .unwrap_or_else(|| {
                            language.pick("请选择设备", "Select a device")
                        });
                    let mut chosen = None;
                    egui::ComboBox::from_id_salt("discovered_receiver")
                        .selected_text(selected_name)
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            if discovered.is_empty() {
                                ui.label(language.pick(
                                    "尚未发现设备",
                                    "No devices discovered yet",
                                ));
                            }
                            for device in &discovered {
                                let selected =
                                    device.host == self.target && device.port == self.port;
                                if ui.selectable_label(selected, &device.name).clicked() {
                                    chosen = Some((device.host.clone(), device.port));
                                }
                            }
                        });
                    if let Some((host, port)) = chosen {
                        self.target = host;
                        self.port = port;
                    }
                    if ui
                        .add_enabled(
                            !self.discovering,
                            egui::Button::new(if self.discovering {
                                language.pick("扫描中...", "Scanning...")
                            } else {
                                language.pick("重新扫描", "Scan again")
                            }),
                        )
                        .clicked()
                    {
                        self.discover();
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(language.pick("分辨率", "Resolution"));
                    let (native_width, native_height) =
                        encoder_dimensions(self.native_width, self.native_height);
                    let native_label = match language {
                        Language::Zh => {
                            format!("{native_width} x {native_height}（当前屏幕）")
                        }
                        Language::En => {
                            format!("{native_width} x {native_height} (Current screen)")
                        }
                    };
                    let selected_resolution = if self.native_resolution {
                        native_label.clone()
                    } else {
                        preset_label(PRESETS[self.preset_index])
                    };
                    let mut use_native = self.native_resolution;
                    let mut chosen_preset = None;
                    egui::ComboBox::from_id_salt("resolution")
                        .selected_text(selected_resolution)
                        .width(230.0)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(use_native, &native_label).clicked() {
                                use_native = true;
                                chosen_preset = None;
                            }
                            for (index, preset) in PRESETS.iter().enumerate() {
                                if ui
                                    .selectable_label(
                                        !use_native && self.preset_index == index,
                                        preset_label(*preset),
                                    )
                                    .clicked()
                                {
                                    use_native = false;
                                    chosen_preset = Some(index);
                                }
                            }
                        });
                    self.native_resolution = use_native;
                    if let Some(index) = chosen_preset {
                        self.preset_index = index;
                    }

                    if self.native_resolution {
                        ui.add_space(12.0);
                        ui.label(language.pick("帧率", "Frame rate"));
                        ui.selectable_value(&mut self.native_fps, 30, "30 FPS");
                        ui.selectable_value(&mut self.native_fps, 60, "60 FPS");
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(language.pick("系统音频", "System audio"));
                    ui.checkbox(
                        &mut self.audio_enabled,
                        language.pick(
                            "转发到电视并静音电脑",
                            "Send to TV and mute this PC",
                        ),
                    );
                });

                ui.add_space(2.0);
                egui::CollapsingHeader::new(language.pick("高级设置", "Advanced settings"))
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::Grid::new("advanced_settings_grid")
                            .num_columns(2)
                            .spacing([18.0, 7.0])
                            .show(ui, |ui| {
                                ui.label(language.pick("采集屏幕", "Capture display"));
                                ui.label(format!(
                                    "{}  ({} x {})",
                                    self.native_display_name,
                                    self.native_width,
                                    self.native_height
                                ));
                                ui.end_row();

                                ui.label(language.pick("地址", "Address"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.target)
                                        .desired_width(260.0)
                                        .hint_text(language.pick(
                                            "IP 地址或主机名",
                                            "IP address or hostname",
                                        )),
                                );
                                ui.end_row();

                                ui.label(language.pick("AirPlay 端口", "AirPlay port"));
                                ui.add(
                                    egui::DragValue::new(&mut self.port)
                                        .range(1..=u16::MAX)
                                        .speed(1),
                                );
                                ui.end_row();

                                ui.label(language.pick("硬件编码器", "Hardware encoder"));
                                egui::ComboBox::from_id_salt("hardware")
                                    .selected_text(hardware_label(language, self.hardware))
                                    .width(220.0)
                                    .show_ui(ui, |ui| {
                                        for value in
                                            [HwAccel::Auto, HwAccel::Nvenc, HwAccel::None]
                                        {
                                            ui.selectable_value(
                                                &mut self.hardware,
                                                value,
                                                hardware_label(language, value),
                                            );
                                        }
                                    });
                                ui.end_row();

                                ui.label(language.pick("音频偏移", "Audio offset"));
                                ui.add(
                                    egui::Slider::new(
                                        &mut self.audio_offset_ms,
                                        AUDIO_OFFSET_MIN_MS..=AUDIO_OFFSET_MAX_MS,
                                    )
                                    .suffix(" ms")
                                    .text(language.pick(
                                        "正值会让音频延后",
                                        "positive delays audio",
                                    )),
                                );
                                ui.end_row();

                                ui.label(language.pick("测试画面", "Test pattern"));
                                ui.checkbox(
                                    &mut self.test_mode,
                                    language.pick("使用合成视频", "Use synthetic video"),
                                );
                                ui.end_row();
                            });
                    });
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if self.running() {
                    if ui
                        .add_sized(
                            [140.0, 36.0],
                            egui::Button::new(
                                egui::RichText::new(
                                    language.pick("停止投屏", "Stop mirroring"),
                                )
                                .color(egui::Color32::WHITE),
                            ),
                        )
                        .clicked()
                    {
                        self.stop();
                    }
                } else {
                    let can_start = !self.target.trim().is_empty();
                    if ui
                        .add_enabled(
                            can_start,
                            egui::Button::new(
                                egui::RichText::new(
                                    language.pick("开始投屏", "Start mirroring"),
                                )
                                .color(egui::Color32::WHITE),
                            )
                            .min_size(egui::vec2(140.0, 36.0))
                            .fill(egui::Color32::from_rgb(41, 112, 158)),
                        )
                        .clicked()
                    {
                        self.start();
                    }
                }

                let stream = self.stream_config();
                ui.label(format!(
                    "{} x {}  |  {} FPS  |  {}",
                    stream.width,
                    stream.height,
                    stream.fps,
                    if stream.bitrate_kbps == 0 {
                        language.pick("自动码率", "auto bitrate").to_string()
                    } else {
                        format!("{} kbps", stream.bitrate_kbps)
                    }
                ));
            });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(5.0);
            ui.label(language.pick("状态", "Status"));
            ui.add(
                egui::Label::new(
                    egui::RichText::new(self.detail.text(language)).monospace(),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );

            if self.running() || self.metrics.video_frames > 0 {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{} {}",
                        language.pick("视频帧", "Video frames"),
                        self.metrics.video_frames
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} {}",
                        language.pick("帧率", "FPS"),
                        self.metrics.video_fps
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} {}",
                        language.pick("丢帧", "Dropped"),
                        self.metrics.video_dropped
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} {}",
                        language.pick("重连", "Reconnects"),
                        self.metrics.reconnects
                    ));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{} {} / {}",
                        language.pick("音频 真实/静音", "Audio real/silent"),
                        self.metrics.audio_real_frames, self.metrics.audio_silent_frames
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} {}",
                        language.pick("队列", "Queue"),
                        self.metrics.audio_queue_depth
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} {:.2}",
                        language.pick("CPU 核心", "CPU cores"),
                        self.metrics.cpu_permille as f64 / 1000.0
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} {:.1}/{:.1} MB",
                        language.pick("内存", "Memory"),
                        self.metrics.working_set_bytes as f64 / 1_048_576.0,
                        self.metrics.private_memory_bytes as f64 / 1_048_576.0
                    ));
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{} {} ms",
                        language.pick("音频同步误差", "Audio sync"),
                        self.metrics.audio_sync_error_ms
                    ));
                    ui.separator();
                    ui.label(format!(
                        "{} {} ms",
                        language.pick("视频缓冲余量", "Video slack"),
                        self.metrics.video_playout_slack_ms
                    ));
                    ui.separator();
                    let skew_color = if self.metrics.av_skew_ms.unsigned_abs() > 20 {
                        ui.visuals().error_fg_color
                    } else {
                        egui::Color32::from_rgb(52, 168, 110)
                    };
                    ui.colored_label(
                        skew_color,
                        format!(
                            "{} {} ms",
                            language.pick("音画偏差", "A/V skew"),
                            self.metrics.av_skew_ms
                        ),
                    );
                    ui.separator();
                    ui.label(format!(
                        "{} {} us/s",
                        language.pick("时钟校正", "Trim"),
                        self.metrics.timeline_correction_us
                    ));
                });
            }

            ui.add_space(14.0);
            ui.label(
                egui::RichText::new(
                    language.pick(
                        "音视频共用同一采集时钟。仅在电视存在固定音频延迟时调整偏移。",
                        "Audio and video share one capture clock. Adjust the offset only if the TV applies a fixed audio delay.",
                    ),
                )
                .small()
                .weak(),
            );
        });
    }
}

impl Drop for AirMirrorApp {
    fn drop(&mut self) {
        self.save_settings();
        if let Some(session) = self.session.take() {
            let _ = session.shutdown.send(true);
            drop(session.thread);
        }
    }
}

fn preset_label(preset: StreamPreset) -> String {
    let config = preset.config();
    format!("{} x {} / {} FPS", config.width, config.height, config.fps)
}

fn hardware_label(language: Language, value: HwAccel) -> &'static str {
    match (language, value) {
        (Language::Zh, HwAccel::Auto) => "自动选择",
        (Language::Zh, HwAccel::Nvenc) => "NVIDIA NVENC",
        (Language::Zh, HwAccel::Vaapi) => "VAAPI",
        (Language::Zh, HwAccel::None) => "软件编码",
        (Language::En, HwAccel::Auto) => "Auto",
        (Language::En, HwAccel::Nvenc) => "NVIDIA NVENC",
        (Language::En, HwAccel::Vaapi) => "VAAPI",
        (Language::En, HwAccel::None) => "Software",
    }
}

fn hardware_key(value: HwAccel) -> &'static str {
    match value {
        HwAccel::Auto => "auto",
        HwAccel::Nvenc => "nvenc",
        HwAccel::Vaapi => "vaapi",
        HwAccel::None => "none",
    }
}

fn supports_screen_mirroring(device: &AirPlayDevice) -> bool {
    device.features.supports_screen_mirroring()
}

fn detect_primary_display() -> (u32, u32, String) {
    match rotten_capture::list_displays() {
        Ok(displays) => {
            if let Some(display) = displays.first() {
                return (display.width, display.height, display.name.clone());
            }
            tracing::warn!("no display outputs found; using 1920x1080 fallback");
        }
        Err(error) => {
            tracing::warn!(%error, "failed to read the current display mode");
        }
    }
    (1920, 1080, "Display 0".into())
}

fn encoder_dimensions(width: u32, height: u32) -> (u32, u32) {
    (width.max(2) & !1, height.max(2) & !1)
}

fn default_true() -> bool {
    true
}

fn default_native_fps() -> u32 {
    DEFAULT_NATIVE_FPS
}

fn normalize_native_fps(fps: u32) -> u32 {
    if fps >= 60 { 60 } else { 30 }
}

fn gui_settings_path() -> Option<std::path::PathBuf> {
    Some(
        dirs::config_dir()?
            .join("airmirror")
            .join("gui-settings.json"),
    )
}

fn install_cjk_font(ctx: &egui::Context) {
    #[cfg(target_os = "windows")]
    {
        let candidates = [r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\simsun.ttc"];
        let Some(bytes) = candidates.iter().find_map(|path| std::fs::read(path).ok()) else {
            return;
        };

        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "windows-cjk".into(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(fonts) = fonts.families.get_mut(&family) {
                fonts.push("windows-cjk".into());
            }
        }
        ctx.set_fonts(fonts);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ctx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_settings_default_to_chinese_and_native_resolution() {
        let settings: GuiSettings = serde_json::from_str(
            r#"{
                "target": "10.10.10.42",
                "port": 3553,
                "preset_index": 1,
                "audio_enabled": true,
                "audio_offset_ms": 10,
                "hardware": "auto",
                "test_mode": false
            }"#,
        )
        .expect("legacy settings should deserialize");

        assert_eq!(settings.language, Language::Zh);
        assert!(settings.native_resolution);
        assert_eq!(settings.native_fps, DEFAULT_NATIVE_FPS);
        assert_eq!(settings.target, "10.10.10.42");
        assert_eq!(settings.port, DEFAULT_PORT);
    }

    #[test]
    fn native_dimensions_are_safe_for_h264_chroma() {
        assert_eq!(encoder_dimensions(1707, 1067), (1706, 1066));
        assert_eq!(encoder_dimensions(2560, 1440), (2560, 1440));
        assert_eq!(encoder_dimensions(1, 0), (2, 2));
    }

    #[test]
    fn native_frame_rate_has_two_supported_tiers() {
        assert_eq!(normalize_native_fps(29), 30);
        assert_eq!(normalize_native_fps(30), 30);
        assert_eq!(normalize_native_fps(59), 30);
        assert_eq!(normalize_native_fps(60), 60);
        assert_eq!(normalize_native_fps(120), 60);
    }

    #[test]
    fn resolution_labels_use_pixel_dimensions() {
        assert_eq!(preset_label(StreamPreset::Balanced), "1920 x 1080 / 15 FPS");
        assert_eq!(preset_label(StreamPreset::FourK60), "3840 x 2160 / 60 FPS");
    }
}
