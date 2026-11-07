# AirMirror 实现方案

## 目标

在 Windows 10/11 上作为 AirPlay 发送端，把真实桌面镜像到支持 AirPlay 的电视。
当前实现不使用 PIN，也不需要修改电视固件。

## 已实现流程

```text
GET /info
  -> transient POST /pair-setup
  -> raw POST /pair-verify (两次)
  -> POST /fp-setup (m1/m2、m3/m4)
  -> audio SETUP
  -> video SETUP
  -> 连接电视返回的动态 TCP video dataPort
  -> RECORD
  -> 发送 SPS/PPS codec packet
  -> 发送 AES-128-CTR 加密的 AVCC VCL packet
  -> 数据通道 heartbeat + RTSP POST /feedback
```

## 工作空间

```text
crates/
  rotten-app/         CLI、镜像会话编排、DXGI 恢复
  rotten-pairing/     transient 配对和凭据持久化
  rotten-crypto/      FairPlay、AES-CTR、pair-verify
  rotten-protocol/    RTSP、SETUP、RECORD、NTP、音频控制
  rotten-video/       OpenH264、码率控制、AVCC 封包
  rotten-capture/     DXGI / X11 抓屏
  third_party/
    fpsap-rs/         m2 SAP 与 m2/m3 一致性校验
```

## 握手细节

### 无 PIN 配对

发送端生成 Ed25519 身份，执行 transient `POST /pair-setup`。随后用 raw
`POST /pair-verify` 完成两步验证。配对模块不提供 PIN 参数，也不会请求
`pair-pin-start` 或 `pair-setup-pin`。

部分电视在 `pair-setup` 返回后才异步提交新发送端身份。首次配对完成后等待
750 ms 再启动媒体会话，第二次及后续会话直接复用已保存凭据。

凭据保存在：

```text
%APPDATA%\airmirror\credentials.json
```

### FairPlay

`fp-setup` 完成后，使用电视返回的 m2 SAP 校验 m3 尾部，然后：

```text
raw_key = legacy playfair decrypt(ekey)
stream_key = SHA-512(raw_key || pair_verify_shared_secret)[:16]
video_descriptor.shk = stream_key
```

### 视频加密

视频使用 AES-128-CTR。密钥和 IV 由：

```text
SHA-512("AirPlayStreamKey" + streamConnectionID + shk)[:16]
SHA-512("AirPlayStreamIV"  + streamConnectionID + shk)[:16]
```

跨帧保留 partial-block keystream，与接收端解密语义一致。

## 媒体管线

```text
DXGI Desktop Duplication
  -> RGBA
  -> H.264 Annex-B
  -> 拆分 SPS/PPS/VCL
  -> AVCC 4-byte length
  -> AES-128-CTR
  -> AirPlay TCP packet
```

音频使用独立的 AirPlay UDP 媒体流：

```text
WASAPI default render loopback
  -> 44.1 kHz / 16-bit / stereo PCM
  -> 352-sample ALAC verbatim frame
  -> ChaCha20-Poly1305 (8-byte nonce)
  -> AirPlay audio RTP + NTP sync packet
```

视频首帧发出后立即启动音频 RTP。WASAPI 尚没有数据、系统正在静音或采集线程
退出时，发送端继续发送静音 ALAC 帧，避免接收端因音频时间线中断而关闭镜像会话。
有 PCM 时按固定 352 样本节奏填充真实音频，并限制缓冲在约 256 ms 内以控制延迟。
WASAPI 采集中断后每秒重建默认输出设备；重建期间音频 RTP 继续发送静音，
因此切换耳机、显示器音频或蓝牙设备时不会中断镜像会话。

视频和音频共用 Windows 启动时钟。视频帧头携带“采集 PTS + 150 ms”，音频
TimeAnnounce 携带 WASAPI 源 PTS，接收端再叠加协商的音频 latency。默认音频
偏移为 `+10 ms`，用于把音频播放窗口居中到一帧视频内。音频时间线每秒最多做
0.5 ms 的缓慢校正，系统音频时钟与 QPC 发生长期漂移时不会累计成音画偏移。

当前使用 OpenH264 `ScreenContentRealTime`，Baseline Profile，默认 15 FPS、
4000 kbps。`--width/--height` 决定唯一编码画布，采集帧统一缩放到该画布。
编码器使用 48 KB size-limited slices，避免 1080p 桌面关键帧形成过大的单
NAL 并触发接收端断流。视频 codec packet 使用该编码画布作为源/目标矩形。

## 会话恢复

每次投屏作为一个可回收的会话尝试运行。会话结束时，抓屏、RTSP feedback、
heartbeat、系统音频采集和编码任务会被统一取消，然后按指数退避重新执行：

```text
1s -> 2s -> 4s -> 8s -> 16s -> 30s (最大)
```

稳定运行超过 30 秒后重置退避等级。AirPlay 视频数据写入设置 10 秒超时，
避免 Wi-Fi 黑洞或电视休眠造成永久阻塞。

## 画质预设

```text
compatibility: 1280x720,  10 FPS, 2000 kbps
balanced:      1920x1080, 15 FPS, 4000 kbps
quality:       1920x1080, 30 FPS, auto bitrate
```

默认使用 `balanced`。单独传入分辨率、帧率或码率参数时只覆盖对应字段。

## 电视兼容修正

- video SETUP 根字段必须包含 `et=32`
- `avcC` 结尾保留 4 字节，但内容必须为全零 `trailing_zero_8bits`
- 视频 NTP 时间戳提前 100 ms
- video 数据包使用 `header[4]=0x00`，IDR 使用 `header[5]=0x10`
- 首次 transient 配对后等待 750 ms 再创建媒体会话
- 1080p H.264 使用 48 KB size-limited slices
- 数据通道写入设置 10 秒超时
- 会话断开后废弃全部媒体任务并按指数退避重连
- `DXGI_ERROR_ACCESS_LOST` 后自动重建 Desktop Duplication

## 验证

项目已经完成以下实机验证：

- 电视 `/info` 返回 AirPlay `AppleTV2,1` 兼容信息
- 无 PIN pairing / pair-verify 返回 200
- fp-setup 两步返回 200
- 电视创建 `OMX.amlogic.avc.decoder.awesome`
- `onOutputFormatChanged` 输出 1280x720 或 1728x1088
- `onRenderOk` 后电视显示完整测试图案
- 全新临时凭据首次连接即可稳定显示测试图案，无需 PIN
- 非 `--test` 模式下 1280x720 和 1920x1080 均显示真实 Windows 桌面
- 1920x1080 真实桌面连续运行 98 秒，持续发送超过 290 帧
- 首段长时间会话持续约 12 分钟，发送 7707 帧、丢帧 3
- 主动断线后 1 秒自动重连并恢复电视画面
- 重连后继续运行约 13 分钟，发送 8375 帧、丢帧 0
- 长时间运行工作集稳定在约 147-169 MB
- 视频首帧后连续发送 ALAC 音频 RTP，15 秒内累计 1000 个音频包
- 播放 440 Hz 系统测试音后累计发送 1595 个真实 PCM 帧，状态由静音无缝切换
- 电视创建 `USAGE_MEDIA` 的 `AudioTrack`，播放状态保持 `started`

## 后续工作

1. 降低电脑发送端 CPU 占用，接入 Media Foundation / NVENC 硬件编码和 GPU 缩放。
2. 支持 2560x1440、3440x1440 和 3840x2160 编码，提供 30/60 FPS
   两档输出、能力探测与自动降级。
3. 增加音频时钟漂移校正、音画同步指标和 8 小时音频长稳测试。
4. 增加托盘、图形界面和安装包。
5. 针对不同厂商 AirPlay 实现增加协议探测和自动降级。

v0.4 的详细任务和量化验收标准见 `docs/roadmap-v0.4.md`。

## v0.4 当前进展

- 编码上限扩展到 4096x2176。
- 新增 2K、超宽 2K 和 4K 的 30/60 FPS 预设。
- 根据画布和帧率选择 H.264 Level 4.0 到 5.2。
- 帧节奏改为锚定时钟，支持 60 FPS 且避免累计漂移。
- 增加编码、发送、丢帧和会话指标。
- `--hwaccel auto` 在 FFmpeg `ddagrab + h264_nvenc` 可用时启用 GPU 直采直编。
- GPU 原生路径避免 RGBA CPU 下载、CPU 缩放和 OpenH264 编码。
- 2K60 原生路径实测约 58-60 FPS，CPU 约 0.15 个逻辑核心。
- 2K60 连续运行 15 分钟，发送 52180 帧，丢帧 0。
- 4K30 测试图案通过电视 H.264 解码和显示验证。
- WASAPI 系统音频、ALAC verbatim、ChaCha64 RTP 和静音保活路径已接通。
- 电视端已确认创建并启动媒体 `AudioTrack`。
- 30 FPS 实机测试中视频播放余量约 119-135 ms，音频同步余量约 114 ms；
  默认 `+10 ms` 偏移后两者中心对齐。
- 音频源延迟稳定在约 35 ms，音频队列保持在 10-19 帧，没有持续增长。
- 新增 `airmirror-gui.exe`，支持地址、端口、预设、硬件编码器、音频开关、
  音频偏移、mDNS 设备发现、实时同步指标和开始/停止。
- GUI 设置保存到 `%APPDATA%\airmirror\gui-settings.json`。
- CLI `--duration-minutes N` 可在 N 分钟后停止，并输出音视频帧数、丢帧、
  同步误差和重连次数，便于 8 小时夜间测试。
- `/info` 通过 RTSP 读取；缺少 `supportedFormats` 时用 features 18-21
  推导 PCM/ALAC/AAC-LC/AAC-ELD 屏幕音频能力。目标电视实测为
  `0x1440800`，ALAC 可用。
- Ctrl+C 会触发取消信号，确保主程序和 ffmpeg 子进程退出。
- FFmpeg 不可用时自动回退 OpenH264；2K30 软件路径已验证，但性能不足。

## 许可证

项目采用 GPL-3.0-or-later。第三方组件和参考项目记录在
`THIRD_PARTY_NOTICES.md`。
