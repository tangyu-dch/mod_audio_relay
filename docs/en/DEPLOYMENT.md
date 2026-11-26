# Deployment Guide

## Build the module

### Prerequisites

| dependency | why |
| --- | --- |
| Rust toolchain (stable) | `cargo build` |
| `libclang` | fswtch-sys bindgen at build time |
| `libspeexdsp` (pkg-config) | resampling inside the module |
| FreeSWITCH headers | via fswtch/pkg-config against `${FS_PREFIX}` |

### Build

```bash
cd mod_audio_relay

# Linux cdylib for FreeSWITCH
cargo build --release -p mod-audio-relay
# → target/release/libmod_audio_relay.so

# Run the workspace tests (core + glue unit tests)
cargo test --workspace
```

`AUDIO_MODULE_IMPL=c` style builds are irrelevant here — this repo *is*
the Rust implementation. For the C reference implementation see the
hummingbird project (`mod_audio_stream/`).

## Install into FreeSWITCH

```bash
cp target/release/libmod_audio_relay.so \
   ${FS_PREFIX}/mod/libmod_audio_relay.so
```

Enable the module — add to `${FS_PREFIX}/autoload_configs/modules.conf.xml`:

```xml
<load module="mod_audio_relay"/>
```

Restart FreeSWITCH (`systemctl restart freeswitch` or container restart).

## Verify the deployment

Quick checks (run as the freeswitch user or with `FS_CLI` set):

```bash
scripts/verify-deployment.sh
# or manually:
FS=${FS_PREFIX:-/usr/local/freeswitch}
ls -l $FS/mod/libmod_audio_relay.so
fs_cli -x "module_exists mod_audio_relay"
fs_cli -x "api uuid_audio_relay"        # expect -USAGE → API registered
fs_cli -x "eval \$\${module_exists(mod_audio_relay)}"
```

The `api uuid_audio_relay` reply must be `-USAGE`-style (not `-ERR
no such API`) — that proves the API callback registered. `-ERR` here means
the module failed to load: check `freeswitch.log` for the load error.

### Live-call verification

1. Originate a call that reaches ANSWERED/BRIDGED (see
   [ESL_GUIDE.md](ESL_GUIDE.md) for the ESL flow).
2. `uuid_audio_relay <uuid> status` → `state=...` should leave `init`.
3. Watch ESL events: `mod_audio_relay::start` fires, then uplink audio
   arrives at the WS endpoint as L16 s16le binary frames
   ([PROTOCOL.md](PROTOCOL.md)).
4. Send a `streamAudio` message → expect `mod_audio_relay::play` with
   `"playback":"queued"` and audible downlink.

## Configuration surface

All runtime behavior is per-call via `start` arguments — there is **no**
module-level config file. Server-side concerns (TLS, headers, metadata)
are expressed through the `ws_url` and `start` arguments:

| knob | where |
| --- | --- |
| sample rate / mix mode | `start` args 4–5 |
| message metadata | `start` arg 3 (quote it!) |
| WS extra headers | `STREAM_EXTRA_HEADERS` channel variable (JSON object) |
| TLS CA/cert/key | `STREAM_TLS_CA_FILE` / `STREAM_TLS_CERT_FILE` / `STREAM_TLS_KEY_FILE` |
| deflate | `STREAM_MESSAGE_DEFLATE` (note inverted semantics) |
| uplink packet aggregation | `STREAM_BUFFER_SIZE` (ms of audio per WS message, default 20) |

## Deployment checklist

- [ ] `libmod_audio_relay.so` installed under `${FS_PREFIX}/mod/`
- [ ] `<load module="mod_audio_relay"/>` present, module loads clean in log
- [ ] `uuid_audio_relay` API returns `-USAGE` via `fs_cli`
- [ ] `libspeexdsp` resolvable (`ldd` on the .so shows no `not found`)
- [ ] ESL consumer subscribes `mod_audio_relay::*` events
- [ ] `start` issued only after ANSWERED/BRIDGED with a reader active on the leg
- [ ] WS endpoint reachable (wss → CA bundle via `STREAM_TLS_CA_FILE`)
- [ ] Downlink sample rate matches `start`'s `sampling` arg

## Known limitations

| limitation | status | workaround |
| --- | --- | --- |
| fswtch `attach_media_bug` doesn't fire READ/WRITE_STREAM callbacks | solved via trampoline in `bug.rs` (reproduces fswtch callback contract) | none needed; upstream fswtch fix would let us drop ~200 lines |
| Capture needs a session read path: READ callbacks dispatch via `switch_core_session_read_frame`; `park()`-style states produce no uplink | inherent FreeSWITCH behavior | start after ANSWERED/BRIDGED; keep a reader app for park scenarios |
| RTP-proxy bridge with identical codecs bypasses `read_frame` | inherent FreeSWITCH behavior | force transcoding (divergent codec prefs) or keep a reader app; downlink unaffected |

## Troubleshooting

- **Module absent from `module_exists`**: check `modules.conf.xml` spelling
  and FreeSWITCH version (needs fswtch-compatible ABI).
- **`not found` in `ldd libmod_audio_relay.so`**: install `libspeexdsp`
  (and `libclang` only matters at build time).
- **Connect storms / rapid reconnect**: WS connect is retried by the
  transport layer with backoff; check endpoint availability first.
- **One-way audio**: almost always the codec-identical RTP-proxy case —
  verify with `uuid_audio_relay <uuid> status` and force a transcode.
