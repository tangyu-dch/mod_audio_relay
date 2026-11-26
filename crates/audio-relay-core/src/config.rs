use std::collections::BTreeMap;

use crate::transport::TlsOptions;
use crate::url;

/// `STREAM_AUDIO_PIPE_RATE` 接受的采样率集合。
pub const PIPE_RATES: [u32; 5] = [8000, 16000, 24000, 32000, 48000];

/// 单条音频流所需的全部配置，由 API 参数与通道变量解析而来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    /// `start` 命令请求的 WebSocket 采样率。
    pub sampling: u32,
    /// 1 为单声道/mixed，2 为立体声。
    pub channels: u32,
    /// 读 codec 速率，即采集重采样器的输入速率。
    pub read_rate: u32,
    /// 写 codec 速率，即播放混音的输出速率。
    pub write_rate: u32,
    /// 发送一条 WebSocket 消息前聚合的 20 ms 包数。
    pub rtp_packets: u32,
    /// 逐消息 deflate，除非 `STREAM_MESSAGE_DEFLATE` 为真否则启用。
    pub compression: bool,
    /// 心跳间隔（秒），0 表示禁用。
    pub heart_beat_seconds: u32,
    pub suppress_log: bool,
    pub extra_headers: Vec<(String, String)>,
    pub tls: TlsOptions,
    pub audio_pipe: Option<String>,
    pub audio_pipe_rate: u32,
}

/// 通道变量来源，镜像模块使用的两个 FreeSWITCH 访问器。
pub trait Variables {
    fn get(&self, name: &str) -> Option<&str>;
    fn is_true(&self, name: &str) -> bool;
}

impl Variables for BTreeMap<String, String> {
    fn get(&self, name: &str) -> Option<&str> {
        self.get(name).map(String::as_str)
    }

    fn is_true(&self, name: &str) -> bool {
        self.get(name).map(|value| switch_true(value)).unwrap_or(false)
    }
}

/// FreeSWITCH `switch_true()`：接受 y、yes、t、true、on 与 1。
pub fn switch_true(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "y" | "yes" | "t" | "true" | "on" | "1"
    )
}

/// 通道变量中无法使用的配置值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidAudioPipeRate(String),
    InvalidExtraHeaders,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAudioPipeRate(value) => {
                write!(formatter, "invalid STREAM_AUDIO_PIPE_RATE: {value}")
            }
            Self::InvalidExtraHeaders => {
                formatter.write_str("STREAM_EXTRA_HEADERS is not a JSON object of string values")
            }
        }
    }
}

impl StreamConfig {
    pub fn from_variables<V: Variables>(variables: &V, read_rate: u32, write_rate: u32, sampling: u32, channels: u32) -> Result<Self, ConfigError> {
        let mut config = Self {
            sampling,
            channels,
            read_rate,
            write_rate,
            rtp_packets: 1,
            compression: !variables.is_true("STREAM_MESSAGE_DEFLATE"),
            heart_beat_seconds: 0,
            suppress_log: variables.is_true("STREAM_SUPPRESS_LOG"),
            extra_headers: Vec::new(),
            tls: TlsOptions::default(),
            audio_pipe: variables.get("STREAM_AUDIO_PIPE").map(str::to_owned),
            audio_pipe_rate: write_rate,
        };

        if let Some(value) = variables.get("STREAM_TLS_CA_FILE") {
            config.tls.ca_file = Some(value.to_owned());
        }
        if let Some(value) = variables.get("STREAM_TLS_KEY_FILE") {
            config.tls.key_file = Some(value.to_owned());
        }
        if let Some(value) = variables.get("STREAM_TLS_CERT_FILE") {
            config.tls.cert_file = Some(value.to_owned());
        }
        config.tls.disable_hostname_validation =
            variables.is_true("STREAM_TLS_DISABLE_HOSTNAME_VALIDATION");

        if let Some(value) = variables.get("STREAM_HEART_BEAT") {
            if let Some(seconds) = parse_whole_number(value) {
                config.heart_beat_seconds = seconds.max(0) as u32;
            }
        }

        if let Some(value) = variables.get("STREAM_BUFFER_SIZE") {
            match parse_whole_number(value) {
                Some(size) if size % 20 != 0 => {
                    // 原模块警告后保留默认包数。
                    config.rtp_packets = 1;
                }
                Some(size) if size >= 20 => config.rtp_packets = (size / 20) as u32,
                _ => config.rtp_packets = 1,
            }
        }

        if let Some(value) = variables.get("STREAM_AUDIO_PIPE_RATE") {
            let rate = parse_whole_number(value)
                .filter(|rate| PIPE_RATES.contains(&(*rate as u32)))
                .ok_or_else(|| ConfigError::InvalidAudioPipeRate(value.to_owned()))?;
            config.audio_pipe_rate = rate as u32;
        }

        if let Some(value) = variables.get("STREAM_EXTRA_HEADERS") {
            config.extra_headers = parse_extra_headers(value)?;
        }

        Ok(config)
    }

    /// 播放队列容量：写速率下单声道 L16 的两秒音频。
    pub fn playback_queue_capacity(&self) -> usize {
        (self.write_rate as usize * 2 * 2) & !1
    }

    /// 单条 WebSocket 消息的采集累加器容量。
    pub fn capture_buffer_capacity(&self) -> usize {
        crate::FRAME_SIZE_8000 * (self.sampling as usize / 8000) * self.channels as usize
            * self.rtp_packets as usize
    }
}

/// 对数值型通道变量使用的严格 `strtol`（要求字符串结束）；接受范围
/// 中尾部的 `%` 或 `-` 会被拒绝。
fn parse_whole_number(value: &str) -> Option<i64> {
    let trimmed = value.trim_start();
    let (sign, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1, &trimmed[1..]),
        Some(b'+') => (1, &trimmed[1..]),
        _ => (1, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().map(|parsed| sign * parsed)
}

/// 额外请求头以 JSON 对象形式传入；与原模块一致，只使用字符串值。
pub fn parse_extra_headers(value: &str) -> Result<Vec<(String, String)>, ConfigError> {
    let parsed: serde_json::Value =
        serde_json::from_str(value).map_err(|_| ConfigError::InvalidExtraHeaders)?;
    let object = parsed
        .as_object()
        .ok_or(ConfigError::InvalidExtraHeaders)?;

    let mut headers = Vec::with_capacity(object.len());
    for (name, value) in object {
        if let Some(value) = value.as_str() {
            headers.push((name.clone(), value.to_owned()));
        }
    }
    headers.sort();
    Ok(headers)
}

/// 应用于 `start` 命令参数的 `validate_ws_uri()` 校验。
pub fn validate_ws_uri(value: &str) -> bool {
    url::validate_ws_uri(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variables(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn defaults_match_the_documented_values() {
        let config = StreamConfig::from_variables(&variables(&[]), 8000, 8000, 8000, 1).unwrap();
        assert!(config.compression);
        assert_eq!(config.heart_beat_seconds, 0);
        assert!(!config.suppress_log);
        assert_eq!(config.rtp_packets, 1);
        assert_eq!(config.audio_pipe_rate, 8000);
        assert_eq!(config.tls.ca_file.as_deref(), Some("SYSTEM"));
    }

    #[test]
    fn deflate_variable_disables_compression() {
        for value in ["true", "TRUE", "1", "yes"] {
            let config = StreamConfig::from_variables(
                &variables(&[("STREAM_MESSAGE_DEFLATE", value)]),
                8000,
                8000,
                8000,
                1,
            )
            .unwrap();
            assert!(!config.compression, "{value} must disable compression");
        }
    }

    #[test]
    fn buffer_size_must_be_a_multiple_of_twenty() {
        let config = StreamConfig::from_variables(
            &variables(&[("STREAM_BUFFER_SIZE", "100")]),
            16000,
            16000,
            16000,
            1,
        )
        .unwrap();
        assert_eq!(config.rtp_packets, 5);
        // 320 字节 * (16000/8000) * 1 声道 * 5 包，即采集缓冲区
        // 的容量上限。
        assert_eq!(config.capture_buffer_capacity(), 3200);

        let ignored = StreamConfig::from_variables(
            &variables(&[("STREAM_BUFFER_SIZE", "50")]),
            8000,
            8000,
            8000,
            1,
        )
        .unwrap();
        assert_eq!(ignored.rtp_packets, 1);
    }

    #[test]
    fn pipe_rate_is_restricted_and_otherwise_fails_startup() {
        let config = StreamConfig::from_variables(
            &variables(&[("STREAM_AUDIO_PIPE", "/tmp/x"), ("STREAM_AUDIO_PIPE_RATE", "16000")]),
            8000,
            8000,
            8000,
            1,
        )
        .unwrap();
        assert_eq!(config.audio_pipe_rate, 16000);

        for value in ["8k", "44100", "", "-1", "16000x"] {
            let error = StreamConfig::from_variables(
                &variables(&[("STREAM_AUDIO_PIPE_RATE", value)]),
                8000,
                8000,
                8000,
                1,
            )
            .expect_err("{value} must be rejected");
            assert!(matches!(error, ConfigError::InvalidAudioPipeRate(_)));
        }
    }

    #[test]
    fn extra_headers_keep_only_string_values() {
        let headers = parse_extra_headers(r#"{"A":"1","B":2,"C":"3"}"#).unwrap();
        assert_eq!(headers, vec![("A".to_owned(), "1".to_owned()), ("C".to_owned(), "3".to_owned())]);
        assert!(parse_extra_headers("not json").is_err());
        assert!(parse_extra_headers("[1,2]").is_err());
    }

    #[test]
    fn queue_capacity_is_two_seconds_of_playback_audio() {
        let config = StreamConfig::from_variables(&variables(&[]), 8000, 16000, 8000, 1).unwrap();
        assert_eq!(config.playback_queue_capacity(), 64000);
    }
}
