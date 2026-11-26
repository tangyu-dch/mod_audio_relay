//! 下行 `streamAudio` 消息解析与分发。
//!
//! 解析是纯函数，可在没有 switch 的环境中测试；副作用（排队 PCM、
//! 写临时文件、经 FreeSWITCH 解码）位于 [`crate::streamer`]。

use serde_json::Value;

/// 调用方对原始消息文本应执行的操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageAction {
    /// 以未改动的消息触发 `mod_audio_relay::json`。
    Forward,
    /// 以改写后的 `data` 对象触发 `mod_audio_relay::play`。
    Play(String),
}

/// 镜像 `ProcessResult`：`ok` 加上针对会话记录的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAudioOutcome {
    pub action: MessageAction,
    /// 以 error 级别记录的消息，不含 `(uuid) ` 前缀。
    pub errors: Vec<String>,
}

impl StreamAudioOutcome {
    pub fn forward() -> Self {
        Self { action: MessageAction::Forward, errors: Vec::new() }
    }

    pub fn play(data_json: String) -> Self {
        Self { action: MessageAction::Play(data_json), errors: Vec::new() }
    }

    pub fn reject(errors: Vec<String>) -> Self {
        Self { action: MessageAction::Forward, errors }
    }

    pub fn is_play(&self) -> bool {
        matches!(self.action, MessageAction::Play(_))
    }
}

/// 解析后的 `streamAudio` 请求，或拒绝原因。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamAudioRequest {
    /// 非 JSON，或 `type` 不是 `streamAudio`：不属于本模块。
    NotStreamAudio,
    /// 格式错误的请求；原始文本按 json 事件转发。
    Invalid(Vec<String>),
    /// `sample_rate` 采样率的小端有符号 16 位单声道 PCM。
    Raw { sample_rate: i32, pcm: Vec<u8>, data: Value },
    /// 需要 FreeSWITCH 解码的编码文件。
    File { extension: &'static str, bytes: Vec<u8>, data: Value },
}

/// `audioDataType: raw` 在指定采样率下的文件后缀。
pub fn raw_extension(sample_rate: i32) -> Option<&'static str> {
    match sample_rate {
        8000 => Some(".r8"),
        16000 => Some(".r16"),
        24000 => Some(".r24"),
        32000 => Some(".r32"),
        48000 => Some(".r48"),
        64000 => Some(".r64"),
        _ => None,
    }
}

/// 编码类 `audioDataType` 对应的文件后缀。
pub fn encoded_extension(kind: &str) -> Option<&'static str> {
    match kind {
        "wav" => Some(".wav"),
        "mp3" => Some(".mp3"),
        "ogg" => Some(".ogg"),
        "pcmu" => Some(".pcmu"),
        "pcma" => Some(".pcma"),
        "opus" => Some(".opus"),
        _ => None,
    }
}

/// 解析一条下行文本消息。`data` 在移除 `audioData` 后返回，供调用方
/// 在添加 `playback`/`file` 等字段前使用。
pub fn parse(message: &str) -> StreamAudioRequest {
    // 解析器接受首个值之后的垃圾数据；只取第一个值，使行为确定。
    let mut root = match serde_json::Deserializer::from_str(message).into_iter::<Value>().next() {
        Some(Ok(root)) => root,
        _ => return StreamAudioRequest::NotStreamAudio,
    };

    if root.get("type").and_then(Value::as_str) != Some("streamAudio") {
        return StreamAudioRequest::NotStreamAudio;
    }

    let mut data = match root.as_object_mut().and_then(|object| object.remove("data")) {
        Some(data) => data,
        None => {
            return StreamAudioRequest::Invalid(vec![
                "processMessage - no data in streamAudio".to_owned(),
            ]);
        }
    };

    let kind = data
        .get("audioDataType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let audio = match data.as_object_mut().and_then(|object| object.remove("audioData")) {
        Some(audio) => audio,
        None => {
            return StreamAudioRequest::Invalid(vec![
                "processMessage - streamAudio missing 'audioData' field".to_owned(),
            ]);
        }
    };
    let Some(encoded) = audio.as_str() else {
        return StreamAudioRequest::Invalid(vec![
            "processMessage - 'audioData' is not a string (expected base64 string)".to_owned(),
        ]);
    };

    let sample_rate = data.get("sampleRate").map(cjson_valueint).unwrap_or(0);

    let extension = if kind == "raw" {
        match raw_extension(sample_rate) {
            Some(extension) => extension,
            None => {
                return StreamAudioRequest::Invalid(vec![format!(
                    "processMessage - unsupported sample rate: {sample_rate}"
                )]);
            }
        }
    } else {
        match encoded_extension(&kind) {
            Some(extension) => extension,
            None => {
                return StreamAudioRequest::Invalid(vec![format!(
                    "processMessage - unsupported audio type: {kind}"
                )]);
            }
        }
    };

    let decoded = match decode(encoded) {
        Ok(decoded) => decoded,
        Err(reason) => {
            return StreamAudioRequest::Invalid(vec![format!(
                "processMessage - base64 decode error: {reason}"
            )]);
        }
    };

    // 除 audioData 外保留所有字段，`data` 移交调用方。
    if kind == "raw" {
        StreamAudioRequest::Raw { sample_rate, pcm: decoded, data }
    } else {
        StreamAudioRequest::File { extension, bytes: decoded, data }
    }
}

/// `cJSON` 以 double 存储数字，并提供截断的 `valueint`。
fn cjson_valueint(value: &Value) -> i32 {
    match value {
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                return integer.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            }
            let double = number.as_f64().unwrap_or(0.0);
            if double >= i32::MAX as f64 {
                i32::MAX
            } else if double <= i32::MIN as f64 {
                i32::MIN
            } else {
                double as i32
            }
        }
        _ => 0,
    }
}

/// 添加 `playback`（编码路径还加 `file`），并以 `cJSON_PrintUnformatted`
/// 的方式序列化对象。
pub fn finish_data(data: &mut Value, playback: &str, file: Option<&str>) -> String {
    if let Some(object) = data.as_object_mut() {
        if let Some(file) = file {
            object.insert("file".to_owned(), Value::String(file.to_owned()));
        }
        object.insert("playback".to_owned(), Value::String(playback.to_owned()));
    }
    serde_json::to_string(data).unwrap_or_default()
}

/// Base64 解码：标准与 URL 安全字母表、`=` 或 `.` 填充、
/// 无填充的两/三字符末组，不容忍空白。
pub fn decode(input: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Ok(Vec::new());
    }

    let mut output = Vec::with_capacity(bytes.len() * 3 / 4 + 3);
    let mut pos = 0;
    while pos < bytes.len() {
        let remaining = bytes.len() - pos;
        if remaining == 1 {
            return Err("Invalid Base64 length");
        }
        let c0 = bytes[pos];
        let c1 = bytes[pos + 1];
        if is_padding(c0) || is_padding(c1) {
            return Err("Invalid Base64 padding");
        }
        let v0 = decode_value(c0).ok_or("Invalid Base64 character")?;
        let v1 = decode_value(c1).ok_or("Invalid Base64 character")?;
        output.push(((v0 << 2) | (v1 >> 4)) as u8);

        if remaining == 2 {
            return Ok(output);
        }

        let c2 = bytes[pos + 2];
        if is_padding(c2) {
            if remaining != 4 || !is_padding(bytes[pos + 3]) {
                return Err("Invalid Base64 padding");
            }
            return Ok(output);
        }
        let v2 = decode_value(c2).ok_or("Invalid Base64 character")?;
        output.push((((v1 & 0x0f) << 4) | (v2 >> 2)) as u8);

        if remaining == 3 {
            return Ok(output);
        }

        let c3 = bytes[pos + 3];
        if is_padding(c3) {
            if remaining != 4 {
                return Err("Invalid Base64 padding");
            }
            return Ok(output);
        }
        let v3 = decode_value(c3).ok_or("Invalid Base64 character")?;
        output.push((((v2 & 0x03) << 6) | v3) as u8);
        pos += 4;
    }
    Ok(output)
}

fn decode_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

fn is_padding(c: u8) -> bool {
    c == b'=' || c == b'.'
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw_message(rate: i32, pcm: &[u8]) -> String {
        use base64::Engine;
        json!({
            "type": "streamAudio",
            "data": {
                "audioDataType": "raw",
                "sampleRate": rate,
                "audioData": base64::engine::general_purpose::STANDARD.encode(pcm),
                "sequence": 7
            }
        })
        .to_string()
    }

    #[test]
    fn ignores_other_messages() {
        assert_eq!(parse("not json"), StreamAudioRequest::NotStreamAudio);
        assert_eq!(parse(r#"{"type":"other"}"#), StreamAudioRequest::NotStreamAudio);
        assert_eq!(parse(r#"{"type":123}"#), StreamAudioRequest::NotStreamAudio);
    }

    #[test]
    fn missing_data_and_audio_data() {
        assert_eq!(
            parse(r#"{"type":"streamAudio"}"#),
            StreamAudioRequest::Invalid(vec!["processMessage - no data in streamAudio".to_owned()])
        );
        assert_eq!(
            parse(r#"{"type":"streamAudio","data":{}}"#),
            StreamAudioRequest::Invalid(vec![
                "processMessage - streamAudio missing 'audioData' field".to_owned()
            ])
        );
        assert_eq!(
            parse(r#"{"type":"streamAudio","data":{"audioData":5}}"#),
            StreamAudioRequest::Invalid(vec![
                "processMessage - 'audioData' is not a string (expected base64 string)".to_owned()
            ])
        );
    }

    #[test]
    fn raw_rates_map_to_suffixes() {
        for (rate, suffix) in
            [(8000, ".r8"), (16000, ".r16"), (24000, ".r24"), (32000, ".r32"), (48000, ".r48"), (64000, ".r64")]
        {
            assert_eq!(raw_extension(rate), Some(suffix));
        }
        assert_eq!(raw_extension(44100), None);
    }

    #[test]
    fn unsupported_rate_and_type() {
        assert_eq!(
            parse(&raw_message(44100, &[0, 0])),
            StreamAudioRequest::Invalid(vec![
                "processMessage - unsupported sample rate: 44100".to_owned()
            ])
        );
        let message = json!({
            "type": "streamAudio",
            "data": { "audioDataType": "flac", "audioData": "AAAA" }
        })
        .to_string();
        assert_eq!(
            parse(&message),
            StreamAudioRequest::Invalid(vec![
                "processMessage - unsupported audio type: flac".to_owned()
            ])
        );
    }

    #[test]
    fn raw_request_detaches_audio_data() {
        let pcm = [1u8, 2, 3, 4];
        match parse(&raw_message(16000, &pcm)) {
            StreamAudioRequest::Raw { sample_rate, pcm: decoded, data } => {
                assert_eq!(sample_rate, 16000);
                assert_eq!(decoded, pcm);
                assert!(data.get("audioData").is_none());
                assert_eq!(data.get("sequence"), Some(&json!(7)));
                assert_eq!(data.get("sampleRate"), Some(&json!(16000)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn file_request_keeps_extension() {
        let message = json!({
            "type": "streamAudio",
            "data": { "audioDataType": "mp3", "audioData": "AAAA" }
        })
        .to_string();
        match parse(&message) {
            StreamAudioRequest::File { extension, bytes, .. } => {
                assert_eq!(extension, ".mp3");
                assert_eq!(bytes, vec![0, 0, 0]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sample_rate_uses_cjson_valueint() {
        let message = json!({
            "type": "streamAudio",
            "data": { "audioDataType": "raw", "sampleRate": "16000", "audioData": "AAAA" }
        })
        .to_string();
        assert_eq!(
            parse(&message),
            StreamAudioRequest::Invalid(vec![
                "processMessage - unsupported sample rate: 0".to_owned()
            ])
        );
    }

    #[test]
    fn base64_errors_match_the_c_decoder() {
        assert_eq!(decode("A"), Err("Invalid Base64 length"));
        assert_eq!(decode("=AAA"), Err("Invalid Base64 padding"));
        assert_eq!(decode("AA=A"), Err("Invalid Base64 padding"));
        assert_eq!(decode("AA==A"), Err("Invalid Base64 padding"));
        assert_eq!(decode("A*AA"), Err("Invalid Base64 character"));
        assert_eq!(decode("AAAA\n"), Err("Invalid Base64 length"));
        assert_eq!(decode(""), Ok(Vec::new()));
        assert_eq!(decode("QQ"), Ok(vec![b'A']));
        assert_eq!(decode("QUI"), Ok(vec![b'A', b'B']));
        assert_eq!(decode("QUJD"), Ok(vec![b'A', b'B', b'C']));
        assert_eq!(decode("QQ.."), Ok(vec![b'A']));
        assert_eq!(decode("QUJ."), Ok(vec![b'A', b'B']));
        assert_eq!(decode("-_8_"), Ok(vec![0xfb, 0xff, 0x3f]));
    }

    #[test]
    fn base64_decode_error_is_reported() {
        let message = json!({
            "type": "streamAudio",
            "data": { "audioDataType": "raw", "sampleRate": 8000, "audioData": "A" }
        })
        .to_string();
        assert_eq!(
            parse(&message),
            StreamAudioRequest::Invalid(vec![
                "processMessage - base64 decode error: Invalid Base64 length".to_owned()
            ])
        );
    }

    #[test]
    fn finish_data_appends_playback_and_file() {
        let mut data = json!({ "audioDataType": "raw", "sampleRate": 8000 });
        assert_eq!(finish_data(&mut data, "queued", None), r#"{"audioDataType":"raw","sampleRate":8000,"playback":"queued"}"#);

        let mut data = json!({ "audioDataType": "mp3" });
        assert_eq!(
            finish_data(&mut data, "file", Some("/tmp/x_0.tmp.mp3")),
            r#"{"audioDataType":"mp3","file":"/tmp/x_0.tmp.mp3","playback":"file"}"#
        );
    }
}