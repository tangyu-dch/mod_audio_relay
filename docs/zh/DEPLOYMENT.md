# 部署指南

## 构建模块

### 前置依赖

| 依赖 | 用途 |
| --- | --- |
| Rust 工具链（stable） | `cargo build` |
| `libclang` | 构建期 fswtch-sys bindgen |
| `libspeexdsp`（pkg-config 探测） | 模块内重采样 |
| FreeSWITCH 头文件 | 通过 fswtch/pkg-config 对 `${FS_PREFIX}` 解析 |

### 构建

```bash
cd mod_audio_relay

# 面向 FreeSWITCH 的 Linux cdylib
cargo build --release -p mod-audio-relay
# → target/release/libmod_audio_relay.so

# 运行 workspace 测试（core + 胶水层单元测试）
cargo test --workspace
```

本仓库就是 Rust 实现本身，无需 `AUDIO_MODULE_IMPL` 之类的开关。C 参考
实现见 hummingbird 项目（`mod_audio_stream/`）。

## 安装到 FreeSWITCH

```bash
cp target/release/libmod_audio_relay.so \
   ${FS_PREFIX}/mod/libmod_audio_relay.so
```

启用模块——在 `${FS_PREFIX}/autoload_configs/modules.conf.xml` 中添加：

```xml
<load module="mod_audio_relay"/>
```

重启 FreeSWITCH（`systemctl restart freeswitch` 或重启容器）。

## 验证部署

快速检查（以 freeswitch 用户执行，或设置 `FS_CLI`）：

```bash
scripts/verify-deployment.sh
# 或手动执行：
FS=${FS_PREFIX:-/usr/local/freeswitch}
ls -l $FS/mod/libmod_audio_relay.so
fs_cli -x "module_exists mod_audio_relay"
fs_cli -x "api uuid_audio_relay"        # 期望 -USAGE → API 已注册
fs_cli -x "eval \$\${module_exists(mod_audio_relay)}"
```

`api uuid_audio_relay` 的回复必须是 `-USAGE` 风格（而非 `-ERR no such
API`）——这证明 API 回调已注册。此处返回 `-ERR` 说明模块加载失败：
查看 `freeswitch.log` 中的加载错误。

### 实呼验证

1. 发起一路到达 ANSWERED/BRIDGED 的通话（ESL 流程见
   [ESL_GUIDE.md](ESL_GUIDE.md)）。
2. `uuid_audio_relay <uuid> status` → `state=...` 应离开 `init`。
3. 观察 ESL 事件：先触发 `mod_audio_relay::start`，随后上行音频以
   L16 s16le 二进制帧到达 WS 端点（格式见 [PROTOCOL.md](PROTOCOL.md)）。
4. 发送 `streamAudio` 消息 → 期望收到 `"playback":"queued"` 的
   `mod_audio_relay::play` 事件且下行可闻。

## 配置面

所有运行时行为都通过 `start` 参数按呼叫配置——**没有**模块级配置文件。
服务端关切（TLS、请求头、metadata）通过 `ws_url` 与 `start` 参数表达：

| 配置项 | 位置 |
| --- | --- |
| 采样率 / 混音模式 | `start` 第 4–5 参数 |
| 消息 metadata | `start` 第 3 参数（记得加引号！） |
| WS 额外请求头 | `STREAM_EXTRA_HEADERS` 通道变量（JSON 对象） |
| TLS CA/证书/私钥 | `STREAM_TLS_CA_FILE` / `STREAM_TLS_CERT_FILE` / `STREAM_TLS_KEY_FILE` |
| deflate 压缩 | `STREAM_MESSAGE_DEFLATE`（注意语义相反） |
| 上行包聚合 | `STREAM_BUFFER_SIZE`（每 WS 消息的音频毫秒数，默认 20） |

## 部署清单

- [ ] `libmod_audio_relay.so` 已安装到 `${FS_PREFIX}/mod/`
- [ ] `modules.conf.xml` 含 `<load module="mod_audio_relay"/>`，日志无加载错误
- [ ] `fs_cli` 下 `uuid_audio_relay` 返回 `-USAGE`
- [ ] `ldd libmod_audio_relay.so` 无 `not found`（需 `libspeexdsp`）
- [ ] ESL 消费端订阅 `mod_audio_relay::*` 事件
- [ ] `start` 在 ANSWERED/BRIDGED 后下发，且该腿有读者应用
- [ ] WS 端点可达（wss 时经 `STREAM_TLS_CA_FILE` 提供 CA）
- [ ] 下行采样率与 `start` 的 `sampling` 参数一致

## 已知限制

| 限制 | 状态 | 规避方式 |
| --- | --- | --- |
| fswtch `attach_media_bug` 不触发 READ/WRITE_STREAM 回调 | 已通过 `bug.rs` 自带 trampoline 解决（复刻 fswtch 回调契约） | 无需处理；fswtch 上游修复后可移除约 200 行 |
| 捕获依赖会话读路径：READ 回调由 `switch_core_session_read_frame` 分发，`park()` 等不读帧状态无上行 | FreeSWITCH 固有行为 | 仅在 ANSWERED/BRIDGED 后 start；park 场景保持读者应用（playback/`uuid_broadcast`） |
| 桥接媒体代理：双方 codec 相同且 FS 走 RTP proxy 时不经 `read_frame` | FreeSWITCH 固有行为 | 部署侧保证至少一腿转码（codec prefs 错开）或保留读帧路径；下行不受影响 |

## 故障排查

- **`module_exists` 查不到**：检查 `modules.conf.xml` 拼写与
  FreeSWITCH 版本（需与 fswtch ABI 兼容）。
- **`ldd` 出现 `not found`**：安装 `libspeexdsp`（`libclang` 仅构建期需要）。
- **频繁重连**：WS 连接由传输层带退避重试；先确认端点可用性。
- **单向无声**：几乎都是 codec 相同的 RTP-proxy 桥接——用
  `uuid_audio_relay <uuid> status` 确认并强制转码。
