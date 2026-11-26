use std::fmt;

/// `mod_audio_relay::error` 事件体中上报的错误码。
///
/// 数值属于对外发布契约（继承自原模块传输层），不得重新编号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrorCode {
    Io = 1,
    InvalidHeader = 2,
    ServerMasked = 3,
    NotSupported = 4,
    PingTimeout = 5,
    ConnectFailed = 6,
    TlsInitFailed = 7,
    SslHandshakeFailed = 8,
    SslError = 9,
    Timeout = 10,
    Protocol = 11,
}

impl ErrorCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// 传输或协议错误，携带写入 error 事件 JSON 的错误码与可读消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamError {
    pub code: ErrorCode,
    pub message: String,
}

impl StreamError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Io, message)
    }

    pub fn invalid_header(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidHeader, message)
    }

    pub fn not_supported(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotSupported, message)
    }

    pub fn ping_timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PingTimeout, message)
    }

    pub fn connect_failed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ConnectFailed, message)
    }

    pub fn tls_init_failed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::TlsInitFailed, message)
    }

    pub fn handshake_failed(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::SslHandshakeFailed, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Timeout, message)
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Protocol, message)
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for StreamError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_keep_published_numbers() {
        for (code, expected) in [
            (ErrorCode::Io, 1),
            (ErrorCode::InvalidHeader, 2),
            (ErrorCode::ServerMasked, 3),
            (ErrorCode::NotSupported, 4),
            (ErrorCode::PingTimeout, 5),
            (ErrorCode::ConnectFailed, 6),
            (ErrorCode::TlsInitFailed, 7),
            (ErrorCode::SslHandshakeFailed, 8),
            (ErrorCode::SslError, 9),
            (ErrorCode::Timeout, 10),
            (ErrorCode::Protocol, 11),
        ] {
            assert_eq!(code.as_i32(), expected);
        }
    }
}
