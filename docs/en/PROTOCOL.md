# WebSocket Protocol & Media Byte Format

The module is a **WebSocket client** that dials the `ws://`/`wss://` URL
passed to `uuid_audio_relay ... start`. This document specifies the wire
protocol in both directions.

## Connection lifecycle

1. TCP connect → HTTP Upgrade handshake (RFC 6455), permessage-deflate
   negotiated unless `STREAM_MESSAGE_DEFLATE=true` (which *disables*
   compression — inverted naming).
2. `wss://` uses rustls with the system trust store, or the bundle given by
   `STREAM_TLS_CA_FILE`; mutual TLS via `STREAM_TLS_CERT_FILE` /
   `STREAM_TLS_KEY_FILE`.
3. Extra handshake headers may be injected with `STREAM_EXTRA_HEADERS`
   (JSON object of string values).
4. After the handshake the `metadata` text message (the `start` argument,
   if any) is sent first, then the stream enters `connect` state and uplink
   audio starts flowing.

## Uplink: FS → server (binary frames)

- **Format**: L16 PCM — signed 16-bit **little-endian** samples, no
  container header.
- **Sample rate**: the `sampling` argument of `start` (8000–64000, 8 kHz
  steps; `16k`-style suffix accepted).
- **Channels**: `mono`/`mixed` = mono; `stereo` = interleaved LR.
- **Framing**: each WS message aggregates `rtp_packets` × 20 ms audio
  packets, where `rtp_packets = STREAM_BUFFER_SIZE / 20` (default 20 →
  1 packet/message). Bytes per 20 ms packet = `rate/50 × 2 × channels`:

  | sampling | mono pkt | stereo pkt |
  | --- | --- | --- |
  | 8000 | 320 B | 640 B |
  | 16000 | 640 B | 1280 B |
  | 24000 | 960 B | 1920 B |
  | 32000 | 1280 B | 2560 B |
  | 48000 | 1920 B | 3840 B |
  | 64000 | 2560 B | 5120 B |

  `STREAM_BUFFER_SIZE=100` → 5 packets/message (e.g. 3200 B @16k mono).
  Send cadence follows the call media frame rate (~every
  20 ms × rtp_packets).

- **Example** (16k mono, default framing): each binary message is 640 bytes
  = 320 i16 samples = 20 ms of call audio:

  ```text
  WS binary frame (640 bytes)
  └─ i16le[0] i16le[1] ... i16le[319]   // -32768..32767
  ```

- Python receive side:

  ```python
  import numpy as np
  async for frame in ws:
      if isinstance(frame, bytes):
          pcm = np.frombuffer(frame, dtype='<i2')  # 16 kHz mono s16le @16k
  ```

## Downlink: server → FS

Two entry points; both feed the same `PlaybackQueue` (ring buffer holding
2 s of mono L16 at the write-codec rate; overflow drops oldest data).

### Option A — binary frames (raw PCM)

- Same L16 s16le format; sample rate **must equal `start`'s `sampling`**
  (resampled internally to the codec rate).
- Any even length ≤8192 bytes per frame; 20 ms granularity recommended
  (`rate/50 × 2 × channels` bytes); larger chunks are sliced as needed.
- `interrupt` / `stop_playback` invalidate queued data via the playback
  generation counter.

```python
import asyncio, math, struct

async def play_beep(ws, seconds=1.0, rate=16000, freq=440.0):
    """rate must equal the sampling arg of start."""
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

Notes:

- `struct.pack('<h', ...)` guarantees little-endian s16; keep amplitude
  ≤ ~12000 because playback mixing is additive and full-scale clips.
- Sending without `sleep` is allowed — the module drains the queue at call
  rate — but the queue caps at ~2 s, so chunked realtime pacing gives the
  best experience and lets `interrupt` take effect promptly.
- For `stereo` mix mode, interleave LR samples per frame.

### Option B — `streamAudio` text messages

```json
{"type":"streamAudio","data":{
    "audioDataType":"raw",
    "sampleRate":16000,
    "audioData":"<base64 of PCM16LE>",
    "sequence":1
}}
```

| data field | notes |
| --- | --- |
| `audioDataType` | `raw` or `wav`/`mp3`/`ogg`/`pcmu`/`pcma`/`opus` |
| `sampleRate` | `raw` only: one of 8000/16000/24000/32000/48000/64000; resampled to codec rate internally |
| `audioData` | base64 (standard or URL-safe alphabet; `=` or `.` padding accepted) |
| other fields | preserved and echoed in the `mod_audio_relay::play` event |

Results:

- `raw` → resampled & queued → `mod_audio_relay::play` with
  `"playback":"queued"`.
- Encoded formats → written to a temp file → decoded via
  `switch_core_file_*` at codec rate → `"playback":"file"` plus
  `"file":"<temp path>"`; mid-decode `interrupt` still aborts via the
  generation guard.
- Malformed input → `mod_audio_relay::json` echo of the original message +
  log entry.

```python
import base64, json
async def speak(ws, pcm_bytes):
    for off in range(0, len(ppcm_bytes := pcm_bytes), 640):
        await ws.send(json.dumps({"type":"streamAudio","data":{
            "audioDataType":"raw","sampleRate":16000,
            "audioData":base64.b64encode(pcm_bytes[off:off+640]).decode()}}))
        await asyncio.sleep(0.02)
```

### Other text messages

Any JSON text message that is not `streamAudio` is forwarded verbatim to ESL
consumers as a `mod_audio_relay::json` event — the server can define its
own message types (`asrResult`, `bargeIn`, …) without module changes.

## Quick reference

| direction | carrier | format | sample rate |
| --- | --- | --- | --- |
| FS → server | WS binary frame | L16 s16le PCM | `sampling` (`start` arg) |
| server → FS | WS binary frame | L16 s16le PCM, even length ≤8192 B | must equal `sampling` |
| server → FS | `streamAudio` raw | base64 PCM16LE | `data.sampleRate` (6 fixed rates) |
| server → FS | `streamAudio` encoded | base64 file | decoded to codec rate by FS |
