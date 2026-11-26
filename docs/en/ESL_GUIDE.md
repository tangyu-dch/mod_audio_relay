# ESL Integration Guide

`uuid_audio_relay` is invoked over FreeSWITCH ESL (TCP 8021 by default).
Every command targets one call by UUID; `start` arms a media bug and
dials the WebSocket, the other verbs manage the stream and the playback
queue.

## Command reference

| command | arguments | effect |
| --- | --- | --- |
| `uuid_audio_relay <uuid> start <ws_url> [metadata] [sampling] [mono\|mixed\|stereo]` | ws_url required; rest optional | arm bug + connect WS; `metadata` sent as first text frame |
| `uuid_audio_relay <uuid> interrupt` | — | clear uplink buffer + playback queue (generation-guarded) |
| `uuid_audio_relay <uuid> stop_playback` | — | drop queued downlink audio, keep uplink |
| `uuid_audio_relay <uuid> stop` | — | tear down WS + media bug |
| `uuid_audio_relay <uuid> status` | — | prints `+OK state=...` line |

Argument notes:

- `sampling`: 8000–64000 in 8 kHz steps; `16k`/`48k` suffix style accepted.
  Determines uplink PCM rate **and** the expected downlink rate.
- `mix_mode`: `mono` (default), `mixed` (same as mono uplink, stereo
  downlink accepts LR), `stereo`.
- `metadata`: pass a **quoted** string if it contains spaces
  (e.g. `"{\"callId\":\"abc\"}"`); `separate_args` splits unquoted
  whitespace, which corrupts JSON.
- Malformed `start` → `-ERR ...` on the ESL reply; malformed binary/text
  traffic on an established session is logged, never crashes the module.

## Lifecycle example

```text
> connect 127.0.0.1 8021 ClueCon
> bgapi originate {ignore_early_media=true}sofia/internal/1001@kamailio &park()
> event plain CUSTOM
> uuid_audio_relay <uuid> start wss://asr.example.com/audio "{\"callId\":\"c-1\"}" 16000 mono
+OK
```

Subscribing to `mod_audio_relay::*` events gives you:

| event | payload |
| --- | --- |
| `mod_audio_relay::start` | uuid, ws_url, sampling, mix_mode |
| `mod_audio_relay::stop` | uuid |
| `mod_audio_relay::json` | verbatim text message the server sent |
| `mod_audio_relay::play` | playback result: `queued`/`file` + echoed data fields |
| `mod_audio_relay::error` | transport/codec errors with uuid context |

## Timing rules (media path)

- `start` must run **after** the call is ANSWERED/BRIDGED — the READ-side
  capture rides `switch_core_session_read_frame`, which only runs while
  an application consumes frames.
- Bridge legs with identical codecs over RTP-proxy produce no read frames;
  force transcoding (divergent codec prefs) or keep a reader application
  (`uuid_broadcast`, playback) on the leg you want to capture.
- `park()` alone produces no uplink audio — keep a reader active.

## Downlink control

```text
> uuid_audio_relay <uuid> interrupt
> uuid_audio_relay <uuid> stop_playback
```

`interrupt` invalidates in-flight `streamAudio` decodes as well as queued
PCM via a generation counter; safe to call from any thread.

## Failure modes

| symptom | likely cause |
| --- | --- |
| `-ERR` on `start` | bad ws_url scheme, unsupported sampling, missing metadata quotes |
| uplink silence | call parked / codec-identical RTP-proxy bridge / bug not armed |
| downlink silent | WS connected but server sends >2 s ahead (queue drop) or wrong sample rate |
| `mod_audio_relay::error` events | TLS failure, refused connection, malformed frames |
