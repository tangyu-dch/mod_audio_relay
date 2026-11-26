# WebSocket 协议与媒体字节规范

本模块是 **WebSocket 客户端**：由 `uuid_audio_relay ... start` 传入的
`ws://`/`wss://` URL 触发主动拨号。本文件规范双向线路格式。

## 连接生命周期

1. TCP 连接 → HTTP Upgrade 握手（RFC 6455），除非设置
   `STREAM_MESSAGE_DEFLATE=true`（注意语义相反：该变量**关闭**压缩）。
2. `wss://` 使用 rustls + 系统信任库；自定义 CA 用
   `STREAM_TLS_CA_FILE`，双向 TLS 用 `STREAM_TLS_CERT_FILE` /
   `STREAM_TLS_KEY_FILE`。
3. `STREAM_EXTRA_HEADERS`（JSON 对象）可注入额外握手头。
4. 握手完成后先发送 `metadata` 文本帧（即 `start` 的第三个参数，若有），
   然后进入 `connect` 状态开始上行音频。

## 上行：FS → 服务端（二进制帧）

- **格式**：L16 PCM——有符号 16 位**小端**采样，无容器头。
- **采样率**：`start` 的 `sampling` 参数（8000–64000，8 kHz 步进；接受
  `16k` 这类后缀写法）。
- **声道**：`mono`/`mixed` = 单声道；`stereo` = LR 交错。
- **分帧**：每条 WS 消息聚合 `rtp_packets` 个 20 ms 音频包，其中
  `rtp_packets = STREAM_BUFFER_SIZE / 20`（默认 20 → 每消息 1 包）。
  每包字节数 = `rate/50 × 2 × 声道数`：

  | sampling | mono/包 | stereo/包 |
  | --- | --- | --- |
  | 8000 | 320 B | 640 B |
  | 16000 | 640 B | 1280 B |
  | 24000 | 960 B | 1920 B |
  | 32000 | 1280 B | 2560 B |
  | 48000 | 1920 B | 3840 B |
  | 64000 | 2560 B | 5120 B |

  `STREAM_BUFFER_SIZE=100` → 每消息 5 包（如 16k mono 为 3200 B）。
  发送节奏跟随通话媒体帧率（约每 20 ms × rtp_packets）。

- **示例**（16k mono 默认分帧）：每条二进制消息 640 字节 = 320 个 i16
  采样 = 20 ms 通话音频：

  ```text
  WS binary frame (640 bytes)
  └─ i16le[0] i16le[1] ... i16le[319]   // -32768..32767
  ```

- Python 接收侧：

  ```python
  import numpy as np
  async for frame in ws:
      if isinstance(frame, bytes):
          pcm = np.frombuffer(frame, dtype='<i2')  # 16 kHz mono s16le @16k
  ```

## 下行：服务端 → FS

两条入口，最终都进入同一个 `PlaybackQueue`（环形缓冲，按写 codec 采样率
缓存 2 s 单声道 L16；溢出丢最旧数据）。

### 方式 A —— 二进制帧（裸 PCM）

- 与上行相同的 L16 s16le 格式；采样率**必须等于 `start` 的
  `sampling`**（内部重采样到 codec 采样率）。
- 每帧长度为偶数、≤8192 字节；建议按 20 ms 粒度
  （`rate/50 × 2 × 声道数` 字节）；更大块会被内部切片。
- `interrupt` / `stop_playback` 通过播放代际计数使已入队数据失效。

```python
import asyncio, math, struct

async def play_beep(ws, seconds=1.0, rate=16000, freq=440.0):
    """rate 必须等于 start 的 sampling 参数。"""
    frame_samples = rate // 50
    for n in range(int(seconds * 50)):
        t = n / rate
        env = min(1.0, (n % frame_samples) / (rate * 0.005))
        pcm = b''.join(
            struct.pack('<h', int(12000 * env * math.sin(2 * math.pi * freq * t)))
            for _ in range(frame_samples))
        await ws.send(pcm)
        await asyncio.sleep(0.02)
```

注意：

- `struct.pack('<h', ...)` 保证小端 s16；幅度建议 ≤ ~12000，因为播放混音
  是叠加的，满幅会削波。
- 可以不 `sleep` 连发——模块按通话速率消费队列——但队列上限约 2 s，
  按实时节奏分块发送体验最好，也让 `interrupt` 能及时生效。
- `stereo` 混音模式下，每帧需按 LR 交错排列采样。

### 方式 B —— `streamAudio` 文本消息

```json
{"type":"streamAudio","data":{
    "audioDataType":"raw",
    "sampleRate":16000,
    "audioData":"<base64 PCM16LE>",
    "sequence":1
}}
```

| data 字段 | 说明 |
| --- | --- |
| `audioDataType` | `raw` 或 `wav`/`mp3`/`ogg`/`pcmu`/`pcma`/`opus` |
| `sampleRate` | 仅 `raw`：限 8000/16000/24000/32000/48000/64000 之一，内部重采样到 codec 采样率 |
| `audioData` | base64（标准或 URL-safe 字母表；`=` 或 `.` 填充均可） |
| 其他字段 | 原样保留并回显在 `mod_audio_relay::play` 事件中 |

结果：

- `raw` → 重采样入队 → `mod_audio_relay::play` 事件
  `"playback":"queued"`。
- 编码格式 → 写临时文件 → 经 `switch_core_file_*` 按 codec 采样率解码 →
  `"playback":"file"` 并带 `"file":"<临时路径>"`；解码中途 `interrupt`
  仍可通过代际保护中止。
- 格式错误 → 回显原始消息为 `mod_audio_relay::json` 事件 + 日志记录。

```python
import base64, json
async def speak(ws, pcm_bytes):
    for off in range(0, len(pcm_bytes), 640):
        await ws.send(json.dumps({"type":"streamAudio","data":{
            "audioDataType":"raw","sampleRate":16000,
            "audioData":base64.b64encode(pcm_bytes[off:off+640]).decode()}}))
        await asyncio.sleep(0.02)
```

### 其他文本消息

任何非 `streamAudio` 的 JSON 文本消息都会原样转发为
`mod_audio_relay::json` ESL 事件——服务端可自定义消息类型
（`asrResult`、`bargeIn` 等）而无需改模块。

## 速查表

| 方向 | 载体 | 格式 | 采样率 |
| --- | --- | --- | --- |
| FS → 服务端 | WS 二进制帧 | L16 s16le PCM | `sampling`（`start` 参数） |
| 服务端 → FS | WS 二进制帧 | L16 s16le PCM，偶数长度 ≤8192 B | 必须等于 `sampling` |
| 服务端 → FS | `streamAudio` raw | base64 PCM16LE | `data.sampleRate`（6 档固定值） |
| 服务端 → FS | `streamAudio` 编码格式 | base64 文件 | 由 FS 解码为 codec 采样率 |
