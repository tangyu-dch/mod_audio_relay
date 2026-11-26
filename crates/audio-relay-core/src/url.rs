use std::net::{Ipv4Addr, Ipv6Addr};

/// WebSocket URI 长度上限。
pub const MAX_WS_URI: usize = 4096;

/// 校验 ws/wss URI：仅校验 scheme、host 字符集与数字端口。
///
/// URI 超过 `MAX_WS_URI` 的输入直接拒绝。
pub fn validate_ws_uri(url: &str) -> bool {
    if url.len() >= MAX_WS_URI || url.as_bytes().contains(&0) {
        return false;
    }

    let host_start = if let Some(rest) = url.strip_prefix("ws://") {
        rest
    } else if let Some(rest) = url.strip_prefix("wss://") {
        rest
    } else {
        return false;
    };

    let mut host_len = 0usize;
    for byte in host_start.bytes() {
        if byte == b':' || byte == b'/' {
            break;
        }
        if !(byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.') {
            return false;
        }
        host_len += 1;
    }
    if host_len == 0 {
        return false;
    }
    let rest = &host_start[host_len..];

    if let Some(port) = rest.strip_prefix(':') {
        let mut digits = 0usize;
        for byte in port.bytes() {
            if byte == b'/' {
                break;
            }
            if !byte.is_ascii_digit() {
                return false;
            }
            digits += 1;
        }
        // 超过 65535 的端口永远不可能合法；这里只检查数字形状，
        // 由后续 URL 解析器拒绝。
        if digits > 5 {
            return false;
        }
    }

    true
}

/// 采样率参数解析：`strtol` 数字加可选符号，然后是
/// 字符串结束或单个 `k`/`K` 后缀（乘 1000 并封顶 64k），
/// 之后限定在 1..64000。
pub fn parse_sampling(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    let (sign, rest) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1i64, &trimmed[1..]),
        Some(b'+') => (1i64, &trimmed[1..]),
        _ => (1i64, trimmed),
    };
    let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 {
        return None;
    }
    let (digits, suffix) = rest.split_at(digits_len);
    let mut parsed: i64 = digits.parse().ok()?;
    if suffix == "k" || suffix == "K" {
        parsed = if parsed > 0 && parsed <= 64 { parsed * 1000 } else { 0 };
    } else if !suffix.is_empty() {
        return None;
    }
    parsed *= sign;
    if parsed > 0 && parsed <= 64000 {
        Some(parsed as u32)
    } else {
        None
    }
}

/// 采样率还必须落在 8 kHz 整数倍上。
pub fn validate_sampling(rate: u32) -> bool {
    (8000..=64000).contains(&rate) && rate.is_multiple_of(8000)
}

/// UTF-8 字节形状检查（非完整 RFC 3629 校验）。
/// 超长形式与代理项同样接受。
pub fn is_valid_utf8(value: &[u8]) -> bool {
    let mut index = 0usize;
    while index < value.len() {
        let lead = value[index];
        if lead == 0 {
            return true;
        }
        let width = if lead & 0x80 == 0x00 {
            1
        } else if lead & 0xE0 == 0xC0 {
            2
        } else if lead & 0xF0 == 0xE0 {
            3
        } else if lead & 0xF8 == 0xF0 {
            4
        } else {
            return false;
        };
        for offset in 1..width {
            match value.get(index + offset) {
                Some(byte) if byte & 0xC0 == 0x80 => {}
                _ => return false,
            }
        }
        index += width;
    }
    true
}

/// 解析后的 `ws://`/`wss://` 端点，镜像 libwsc
/// `WebSocketClient::setUrl`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketUrl {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// 请求目标，总是以 `/` 开头。
    pub path: String,
    /// host 是否为 IP 字面量；TLS SNI 对该类地址跳过。
    pub ip_literal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    ForbiddenCharacter,
    UnsupportedScheme,
    EmptyHost,
    InvalidPort,
}

impl std::fmt::Display for UrlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::ForbiddenCharacter => "URL contains CR, LF or NUL",
            Self::UnsupportedScheme => "URL must start with ws:// or wss://",
            Self::EmptyHost => "URL has no host",
            Self::InvalidPort => "URL port is not a number in 1..65535",
        };
        formatter.write_str(message)
    }
}

impl WebSocketUrl {
    pub fn parse(url: &str) -> Result<Self, UrlError> {
        if url.bytes().any(|byte| byte == b'\r' || byte == b'\n' || byte == 0) {
            return Err(UrlError::ForbiddenCharacter);
        }
        let (secure, rest) = if let Some(rest) = url.strip_prefix("ws://") {
            (false, rest)
        } else if let Some(rest) = url.strip_prefix("wss://") {
            (true, rest)
        } else {
            return Err(UrlError::UnsupportedScheme);
        };

        let (hostport, path) = match rest.find('/') {
            Some(index) => (&rest[..index], rest[index..].to_owned()),
            None => (rest, "/".to_owned()),
        };

        let (host, port) = match hostport.find(':') {
            Some(index) => {
                let host = &hostport[..index];
                let port = parse_stoi(&hostport[index + 1..]).ok_or(UrlError::InvalidPort)?;
                if !(1..=65535).contains(&port) {
                    return Err(UrlError::InvalidPort);
                }
                (host, port as u16)
            }
            None => (hostport, if secure { 443 } else { 80 }),
        };

        if host.is_empty() {
            return Err(UrlError::EmptyHost);
        }

        Ok(Self {
            secure,
            host: host.to_owned(),
            port,
            path,
            ip_literal: is_host_ip_address(host),
        })
    }

    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// libwsc 总是发送 `http://` origin，即使端点是 `wss://`。
    pub fn origin(&self) -> String {
        format!("http://{}", self.authority())
    }
}

/// `std::stoi` 语义：前导空白与符号、随后数字，且整个字符串必须被消费。
fn parse_stoi(value: &str) -> Option<i64> {
    let trimmed = value.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (sign, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1i64, &trimmed[1..]),
        Some(b'+') => (1i64, &trimmed[1..]),
        _ => (1i64, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().map(|parsed| sign * parsed)
}

/// 判断 host 是否为 IP 字面量，含 IPv6 方括号剥离。
pub fn is_host_ip_address(host: &str) -> bool {
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let cleaned = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    cleaned.parse::<Ipv6Addr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_validation_matches_the_c_guard() {
        for url in [
            "ws://host/path",
            "wss://host:8443/path?x=1",
            "ws://127.0.0.1:9001",
            "ws://host.example-1.com",
            "ws://host:/path",
        ] {
            assert!(validate_ws_uri(url), "{url} should be valid");
        }
        for url in [
            "http://host/path",
            "ws:///path",
            "ws://ho_st/path",
            "ws://[::1]:9001/x",
            "ws://host:abc/x",
            "ws://host:99999999999999999999/x",
            "ws://",
        ] {
            assert!(!validate_ws_uri(url), "{url} should be invalid");
        }
    }

    #[test]
    fn sampling_accepts_k_suffix_and_rejects_garbage() {
        assert_eq!(parse_sampling("8k"), Some(8000));
        assert_eq!(parse_sampling("16K"), Some(16000));
        assert_eq!(parse_sampling("64k"), Some(64000));
        assert_eq!(parse_sampling("16000"), Some(16000));
        assert_eq!(parse_sampling("70k"), None);
        assert_eq!(parse_sampling("-8k"), None);
        assert_eq!(parse_sampling("8000x"), None);
        assert_eq!(parse_sampling("0x8000"), None);
        assert_eq!(parse_sampling(""), None);
        assert_eq!(parse_sampling(" 16000 "), Some(16000));
        assert!(!validate_sampling(44100));
        assert!(validate_sampling(24000));
    }

    #[test]
    fn utf8_shape_check_is_lenient_like_the_original() {
        assert!(is_valid_utf8("hello".as_bytes()));
        assert!(is_valid_utf8("你好".as_bytes()));
        // '/' 的超长编码被 C 版字节形状检查接受。
        assert!(is_valid_utf8(&[0xC0, 0xAF]));
        assert!(!is_valid_utf8(&[0xE4, 0xBD]));
        assert!(!is_valid_utf8(&[0xFF]));
        assert!(!is_valid_utf8(&[0xE4, 0x28, 0xA0]));
    }

    #[test]
    fn url_parser_defaults_ports_and_paths() {
        let url = WebSocketUrl::parse("ws://example.com/stream").unwrap();
        assert_eq!((url.host.as_str(), url.port, url.path.as_str(), url.secure), ("example.com", 80, "/stream", false));
        assert!(!url.ip_literal);

        let url = WebSocketUrl::parse("wss://example.com").unwrap();
        assert_eq!((url.port, url.path.as_str(), url.secure), (443, "/", true));
        assert_eq!(url.origin(), "http://example.com:443");

        let url = WebSocketUrl::parse("ws://127.0.0.1:9001/x?y=2").unwrap();
        assert!(url.ip_literal);
        assert_eq!(url.path, "/x?y=2");

        assert_eq!(WebSocketUrl::parse("ws://host:0/x"), Err(UrlError::InvalidPort));
        assert_eq!(WebSocketUrl::parse("ws://host:70000/x"), Err(UrlError::InvalidPort));
        assert_eq!(WebSocketUrl::parse("ws://host:12ab/x"), Err(UrlError::InvalidPort));
        assert_eq!(WebSocketUrl::parse("ws:///x"), Err(UrlError::EmptyHost));
        assert_eq!(WebSocketUrl::parse("https://host/x"), Err(UrlError::UnsupportedScheme));
        assert_eq!(WebSocketUrl::parse("ws://host/x\r\n"), Err(UrlError::ForbiddenCharacter));
        // libwsc 按第一个冒号拆分，因此方括号 IPv6 字面量会失败。
        assert_eq!(WebSocketUrl::parse("ws://[::1]:9001/x"), Err(UrlError::InvalidPort));
    }
}
