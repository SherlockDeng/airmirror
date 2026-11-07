# AirMirror v0.4 演进计划

## 版本目标

v0.4 聚焦 Windows 系统音频、音画同步、长时间稳定和可直接使用的桌面 GUI。
视频协议继续保持 v0.3 已验证的 2K/4K、30/60 FPS 和自动重连能力。

## P0：统一媒体时钟

视频采集、WASAPI 音频采集、视频帧头和音频 TimeAnnounce 必须使用同一个
Windows 启动时钟。

已实现：

- 视频帧携带采集 PTS，不再在发送瞬间生成时间戳。
- WASAPI 使用 `IAudioCaptureClient` 的设备/PTS 位置，按 352 样本切帧。
- 音频 TimeAnnounce 的 RTP playhead 同步写入两个 RTP 字段。
- 音频 SETUP 使用 `latencyMin=0`、`latencyMax=150 ms`。
- 视频帧头使用相同的 150 ms 播放提前量。
- 默认音频偏移为 `+10 ms`，用于抵消一帧内音频提前量。

验收标准：

- 视频 `playoutSlackMs` 与音频 `syncErrorMs` 差值不超过一个 30 FPS 帧。
- 稳态 WASAPI 源延迟保持在 20-50 ms。
- 音轨队列不超过 256 ms。
- 音频时间轴每秒最多校正 0.5 ms，避免可闻跳帧。

## P0：音频连续性

- 系统静音、WASAPI 暂无数据和采集线程退出时持续发送静音 ALAC。
- 真实 PCM 与静音帧使用同一 RTP 样本时间线。
- 输入 PTS 不连续超过 20 ms 时丢弃不完整缓存并重新对齐。
- 队列溢出或源 PTS 过旧时丢弃旧帧，不允许延迟无限增长。

验收标准：

- 电视 `AudioTrack` 保持 `started`，不会因无声音输入关闭镜像会话。
- 真实音频开始和结束时不出现会话重连。
- 1 小时测试中不出现音频队列持续增长。

## P0：音画同步验证

增加同步测试信号和长期指标：

- 视频显示周期性闪烁/编号帧。
- 音频生成与视频编号对应的短音。
- 记录视频 PTS、音频 PTS、发送时刻和播放余量。
- 接收端有设备日志时记录音频 `AudioTrack` 渲染位置。

验收标准：

- 默认配置下音画偏差不超过 20 ms。
- 电视存在固定硬件音频延迟时，可通过 GUI 音频偏移在 ±80/300 ms 内校准。
- 校准后在至少 1 小时内不出现持续漂移。

## P1：音频格式增强

- 优先保留 TV 已验证的 ALAC verbatim。
- 根据 `/info` 的 `supportedFormats.screenStream` 探测 ALAC 和 AAC-ELD。
- AAC-ELD 仅在编码器可用且接收端只支持 AAC-ELD 时启用。
- 增加音轨编码失败回退到静音，不影响视频会话。

验收标准：

- ALAC 与 AAC-ELD 都能被对应接收端解码。
- 不支持 AAC-ELD 的电视不会收到错误格式。
- 切换格式不影响视频重连。

## P1：简易 GUI

交付 `airmirror-gui.exe`：

- 接收端地址和端口。
- 画质预设、硬件编码器、系统音频开关。
- 音频偏移滑杆。
- 开始、停止和连接状态。
- 测试画面开关。

后续 GUI 项：

- 系统托盘和开机启动。

## 长稳与回归

```text
1 小时：1080p30 + 系统音频
2 小时：2K30 / 2K60 原生 GPU 路径
8 小时：1080p30 或 2K30 + 系统音频
每 30 分钟：主动断线一次
每 2 小时：重建一次采集后端
```

验收标准：

- 进程不退出，自动重连成功率 100%。
- 音频和视频队列均无持续增长。
- 工作集增长低于 10%。
- 音画偏差不随运行时间累计扩大。
- 电视端视频解码和 `AudioTrack` 均保持工作。

## 当前进度

- [x] 共享 Windows 启动时钟
- [x] 时间戳化 WASAPI 音频帧
- [x] 精确 352 样本 ALAC RTP 时间线
- [x] 静音/真实 PCM 无缝切换
- [x] WASAPI 默认设备失败后的自动重建
- [x] 150 ms 统一播放余量和 +10 ms 默认音频偏移
- [x] 音频时间轴缓慢漂移校正
- [x] 简易桌面 GUI
- [x] GUI 设备发现
- [x] GUI 实时帧数、丢帧、音频队列、A/V 偏差和时钟校正
- [x] GUI 实时 FPS
- [x] GUI 实时 CPU 核占用
- [x] A/V skew 当前值和全程最大绝对偏差
- [x] GUI/soak 工作集与私有内存指标
- [x] GUI 配置持久化到 `%APPDATA%\airmirror\gui-settings.json`
- [x] CLI `--duration-minutes` 定时长运行和 soak 汇总
- [x] `/info` `supportedFormats.screenStream` 解析和旧接收端 feature 回退
- [x] 仅声明 AAC-ELD 的接收端会在 SETUP 前返回明确错误
- [ ] AAC-ELD 协商
- [ ] 8 小时音频长稳
