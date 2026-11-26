# ESL 对接指南

`uuid_audio_relay` 通过 FreeSWITCH ESL（默认 TCP 8021）调用。每条命令按
UUID 定位单路通话；`start` 挂载媒体 bug 并拨号 WebSocket，其余动词管理
流与播放队列。

## 命令参考

| 命令 | 参数 | 作用 |
| --- | --- | --- |
| `uuid_audio_relay <uuid> start <ws_url> [metadata] [sampling] [mono\|mixed\|stereo]` | ws_url 必填，其余可选 | 挂 bug + 连接 WS；`metadata` 作为第一条文本帧发送 |
| `uuid_audio_relay <uuid> interrupt` | — | 清空上行缓冲 + 播放队列（代际保护） |
| `uuid_audio_relay <uuid> stop_playback` | — | 丢弃下行已排队音频，保留上行 |
| `uuid_audio_relay <uuid> stop` | — | 拆除 WS + 媒体 bug |
| `uuid_audio_relay <uuid> status` | — | 输出 `+OK state=...` |

参数说明：

- `sampling`：8000–64000、8 kHz 步进；接受 `16k`/`48k` 后缀写法。决定
  上行 PCM 采样率**以及**期望的下行采样率。
- `mix_mode`：`mono`（默认）、`mixed`（上行同 mono，下行接受立体声
  LR）、`stereo`。
- `metadata`：含空格时必须加引号（如 `"{\"callId\":\"abc\"}"`）；
  `separate_args` 会按未加引号的空白拆分，引号缺失会破坏 JSON。
- `start` 参数非法 → ESL 回复 `-ERR ...`；已建立会话上的非法二进制/文本
  流量只记日志，绝不使模块崩溃。

## 生命周期示例

```text
> connect 127.0.0.1 8021 ClueCon
> bgapi originate {ignore_early_media=true}sofia/internal/1001@kamailio &park()
> event plain CUSTOM
> uuid_audio_relay <uuid> start wss://asr.example.com/audio "{\"callId\":\"c-1\"}" 16000 mono
+OK
```

订阅 `mod_audio_relay::*` 事件可获得：

| 事件 | 载荷 |
| --- | --- |
| `mod_audio_relay::start` | uuid、ws_url、sampling、mix_mode |
| `mod_audio_relay::stop` | uuid |
| `mod_audio_relay::json` | 服务端发来的原始文本消息 |
| `mod_audio_relay::play` | 播放结果：`queued`/`file` + 回显的 data 字段 |
| `mod_audio_relay::error` | 传输/编解码错误，带 uuid 上下文 |

## 时序规则（媒体路径）

- `start` 必须在通话 **ANSWERED/BRIDGED 之后**执行——READ 侧捕获走
  `switch_core_session_read_frame`，只有存在消费帧的应用时才分发。
- 双腿 codec 相同且走 RTP proxy 的桥接不产生读帧；需强制转码（codec
  prefs 错开）或在该腿保留读者应用（`uuid_broadcast`、playback）。
- 仅 `park()` 状态没有上行音频——需保持一个读者应用活跃。

## 下行控制

```text
> uuid_audio_relay <uuid> interrupt
> uuid_audio_relay <uuid> stop_playback
```

`interrupt` 通过代际计数使在途 `streamAudio` 解码与已排队 PCM 同时失效；
可从任意线程安全调用。

## 常见故障

| 现象 | 可能原因 |
| --- | --- |
| `start` 返回 `-ERR` | ws_url scheme 非法、sampling 不支持、metadata 未加引号 |
| 上行无声 | 通话 parked / codec 相同的 RTP-proxy 桥接 / bug 未挂载 |
| 下行无声 | WS 已连接但服务端发送超前 >2 s（队列丢弃）或采样率不匹配 |
| `mod_audio_relay::error` 事件 | TLS 失败、连接被拒、帧格式错误 |
