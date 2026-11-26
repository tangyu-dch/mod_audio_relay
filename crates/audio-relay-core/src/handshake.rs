use base64::Engine;
use sha1::{Digest, Sha1};

use crate::deflate::DeflateConfig;
use crate::error::{ErrorCode, StreamError};
use crate::url::WebSocketUrl;

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// 超过该大小的握手响应被拒绝，与 libwsc 一致。
pub const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;

/// libwsc 发送的扩展协商请求，逐字节一致。
pub const COMPRESSION_OFFER: &str =
    "permessage-deflate; client_no_context_takeover; server_no_context_takeover; client_max_window_bits=9";

pub fn generate_key() -> String {
    let bytes: [u8; 16] = rand::random();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn expected_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

fn has_forbidden_characters(value: &str) -> bool {
    value.bytes().any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
}

/// 构造升级请求，头字段拼写与顺序完全复刻 libwsc，
/// 包括冒号后缺空格的写法。
pub fn build_request(
    url: &WebSocketUrl,
    key: &str,
    compression_requested: bool,
    extra_headers: &[(String, String)],
) -> Result<Vec<u8>, StreamError> {
    if has_forbidden_characters(&url.path) || has_forbidden_characters(&url.host) {
        return Err(StreamError::new(
            ErrorCode::ConnectFailed,
            "Invalid WebSocket handshake configuration",
        ));
    }
    for (name, value) in extra_headers {
        if has_forbidden_characters(name) || has_forbidden_characters(value) {
            return Err(StreamError::new(
                ErrorCode::ConnectFailed,
                "Invalid WebSocket handshake header",
            ));
        }
    }

    let mut request = String::new();
    request.push_str(&format!("GET {} HTTP/1.1\r\n", url.path));
    request.push_str(&format!("Host:{}\r\n", url.authority()));
    request.push_str("Upgrade:websocket\r\n");
    request.push_str("Connection:upgrade\r\n");
    request.push_str(&format!("Sec-WebSocket-Key:{key}\r\n"));
    request.push_str("Sec-WebSocket-Version:13\r\n");
    if compression_requested {
        request.push_str(&format!("Sec-WebSocket-Extensions:{COMPRESSION_OFFER}\r\n"));
    }
    request.push_str(&format!("Origin:{}\r\n", url.origin()));
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}:{value}\r\n"));
    }
    request.push_str("\r\n");
    Ok(request.into_bytes())
}

/// 解析后的升级响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// HTTP 头消耗的字节数；其后的内容属于 WS 帧。
    pub header_bytes: usize,
    /// 服务器接受时协商出的 permessage-deflate 参数。
    pub compression: Option<DeflateConfig>,
}

/// 响应尚不完整时返回 `Ok(None)`。
pub fn parse_response(
    bytes: &[u8],
    key: &str,
    compression_requested: bool,
) -> Result<Option<Handshake>, StreamError> {
    if bytes.len() < 4 {
        return Ok(None);
    }
    let terminator = find_subslice(bytes, b"\r\n\r\n");
    let header_bytes = match terminator {
        Some(index) => index + 4,
        None => {
            if bytes.len() > MAX_HANDSHAKE_BYTES {
                return Err(StreamError::new(ErrorCode::ConnectFailed, "handshake response too large"));
            }
            return Ok(None);
        }
    };
    if header_bytes > MAX_HANDSHAKE_BYTES {
        return Err(StreamError::new(ErrorCode::ConnectFailed, "handshake response too large"));
    }

    let headers = String::from_utf8_lossy(&bytes[..header_bytes]).into_owned();
    let status_end = headers.find("\r\n").unwrap_or(headers.len());
    let status_line = &headers[..status_end];
    let is_101 = headers.len() >= 12
        && status_line.starts_with("HTTP/1.1 101")
        && (status_end == 12 || matches!(headers.as_bytes()[12], b' ' | b'\t'));

    let accept = header_value(&headers, "sec-websocket-accept").unwrap_or_default();
    if !is_101 || accept.is_empty() || accept != expected_accept(key) {
        return Err(StreamError::new(ErrorCode::ConnectFailed, "WebSocket upgrade failed"));
    }

    let mut compression = None;
    if compression_requested {
        if let Some(extensions) = header_value(&headers, "sec-websocket-extensions") {
            let lowered = extensions.to_ascii_lowercase();
            if lowered.contains("permessage-deflate") {
                compression = Some(DeflateConfig {
                    client_no_context_takeover: lowered.contains("client_no_context_takeover"),
                    server_no_context_takeover: lowered.contains("server_no_context_takeover"),
                    client_max_window_bits: parse_bits(&lowered, "client_max_window_bits"),
                    server_max_window_bits: parse_bits(&lowered, "server_max_window_bits"),
                    compression_level: 6,
                });
            }
        }
    }

    Ok(Some(Handshake { header_bytes, compression }))
}

/// libwsc 在整个头块中搜索小写化的字段名加冒号，然后把值
/// 截取到下一个 CRLF 为止并去除空白。
fn header_value(headers: &str, name: &str) -> Option<String> {
    let lowered = headers.to_ascii_lowercase();
    let needle = format!("{name}:");
    let start = lowered.find(&needle)? + needle.len();
    let rest = &headers[start..];
    let end = rest.find("\r\n").unwrap_or(rest.len());
    Some(rest[..end].trim().to_owned())
}

/// window bits 默认 15，仅接受 8..15。
fn parse_bits(extensions: &str, name: &str) -> i32 {
    let needle = format!("{name}=");
    let Some(start) = extensions.find(&needle) else {
        return 15;
    };
    let rest = &extensions[start + needle.len()..];
    let end = rest
        .find([' ', ';', '\r', '\n'])
        .unwrap_or(rest.len());
    rest[..end].parse::<i32>().ok().filter(|bits| (8..=15).contains(bits)).unwrap_or(15)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> WebSocketUrl {
        WebSocketUrl::parse("wss://example.com:8443/audio?x=1").unwrap()
    }

    fn response(key: &str, extensions: Option<&str>) -> Vec<u8> {
        let mut text = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n",
            expected_accept(key)
        );
        if let Some(extensions) = extensions {
            text.push_str(&format!("Sec-WebSocket-Extensions: {extensions}\r\n"));
        }
        text.push_str("\r\n");
        text.into_bytes()
    }

    #[test]
    fn request_matches_the_libwsc_bytes() {
        let request = build_request(
            &url(),
            "dGhlIHNhbXBsZSBub25jZQ==",
            true,
            &[("X-Api-Key".to_owned(), "secret".to_owned())],
        )
        .unwrap();
        let text = String::from_utf8(request).unwrap();
        assert_eq!(
            text,
            "GET /audio?x=1 HTTP/1.1\r\n\
             Host:example.com:8443\r\n\
             Upgrade:websocket\r\n\
             Connection:upgrade\r\n\
             Sec-WebSocket-Key:dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version:13\r\n\
             Sec-WebSocket-Extensions:permessage-deflate; client_no_context_takeover; server_no_context_takeover; client_max_window_bits=9\r\n\
             Origin:http://example.com:8443\r\n\
             X-Api-Key:secret\r\n\
             \r\n"
        );
    }

    #[test]
    fn compression_offer_can_be_disabled() {
        let request = build_request(&url(), "key", false, &[]).unwrap();
        let text = String::from_utf8(request).unwrap();
        assert!(!text.contains("Sec-WebSocket-Extensions"));
        assert!(text.ends_with("Origin:http://example.com:8443\r\n\r\n"));
    }

    #[test]
    fn header_injection_is_rejected() {
        let error = build_request(
            &url(),
            "key",
            true,
            &[("X".to_owned(), "a\r\nEvil:1".to_owned())],
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ConnectFailed);
    }

    #[test]
    fn accept_value_follows_rfc6455() {
        assert_eq!(expected_accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn incomplete_responses_wait() {
        assert!(parse_response(b"HTTP/1.1 101", "key", true).unwrap().is_none());
        let full = response("key", None);
        assert!(parse_response(&full[..full.len() - 2], "key", true).unwrap().is_none());
        assert!(parse_response(&full, "key", true).unwrap().is_some());
    }

    #[test]
    fn upgrade_failures_report_connect_failed() {
        let bad_accept = response("other-key", None);
        let error = parse_response(&bad_accept, "key", true).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConnectFailed);

        let not_101 = b"HTTP/1.1 403 Forbidden\r\n\r\n";
        assert_eq!(
            parse_response(not_101, "key", true).unwrap_err().code,
            ErrorCode::ConnectFailed
        );

        let oversized = vec![b'a'; MAX_HANDSHAKE_BYTES + 1];
        assert_eq!(
            parse_response(&oversized, "key", true).unwrap_err().message,
            "handshake response too large"
        );
    }

    #[test]
    fn extension_parameters_are_parsed_like_libwsc() {
        let key = "key";
        let bytes = response(key, Some("permessage-deflate; client_no_context_takeover; server_no_context_takeover; server_max_window_bits=12; client_max_window_bits=10"));
        let handshake = parse_response(&bytes, key, true).unwrap().unwrap();
        let config = handshake.compression.unwrap();
        assert_eq!(config.client_max_window_bits, 10);
        assert_eq!(config.server_max_window_bits, 12);
        assert!(config.client_no_context_takeover);
        assert!(config.server_no_context_takeover);
        assert_eq!(handshake.header_bytes, bytes.len());

        let bytes = response(key, Some("permessage-deflate"));
        let config = parse_response(&bytes, key, true).unwrap().unwrap().compression.unwrap();
        assert_eq!(config.client_max_window_bits, 15);
        assert_eq!(config.server_max_window_bits, 15);
        assert!(!config.client_no_context_takeover);

        let bytes = response(key, Some("permessage-deflate; client_max_window_bits=99"));
        let config = parse_response(&bytes, key, true).unwrap().unwrap().compression.unwrap();
        assert_eq!(config.client_max_window_bits, 15);

        let bytes = response(key, Some("x-webkit-deflate-frame"));
        assert!(parse_response(&bytes, key, true).unwrap().unwrap().compression.is_none());

        let bytes = response(key, Some("permessage-deflate"));
        assert!(parse_response(&bytes, key, false).unwrap().unwrap().compression.is_none());
    }

    #[test]
    fn leftover_frame_bytes_are_not_consumed() {
        let key = "key";
        let mut bytes = response(key, None);
        bytes.extend_from_slice(&[0x81, 0x02, b'h', b'i']);
        let handshake = parse_response(&bytes, key, true).unwrap().unwrap();
        assert_eq!(&bytes[handshake.header_bytes..], &[0x81, 0x02, b'h', b'i']);
    }
}
