use crate::deflate::{DeflateConfig, DeflateError, Deflater, Inflater, MAX_MESSAGE_SIZE};

pub const OP_CONTINUATION: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

/// 通过帧头规则校验后的单个线上帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub fin: bool,
    pub rsv1: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// 协议违规；值为需要回复的关闭码。
    Protocol(u16, &'static str),
}

impl FrameError {
    pub fn close_code(&self) -> u16 {
        match self {
            Self::Protocol(code, _) => *code,
        }
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(code, reason) => write!(formatter, "{code}: {reason}"),
        }
    }
}

/// 增量式 RFC 6455 帧解析器。
///
/// 客户端模式（默认）拒绝掩码帧；服务端模式要求掩码并解掩码负载，
/// 镜像 RFC 6455 的对端规则。
pub struct FrameDecoder {
    buffer: Vec<u8>,
    compression_enabled: bool,
    peer_masked: bool,
}

impl FrameDecoder {
    pub fn new(compression_enabled: bool) -> Self {
        Self { buffer: Vec::new(), compression_enabled, peer_masked: false }
    }

    /// 连接服务端方向的解码器，对端必须掩码。
    pub fn for_server(compression_enabled: bool) -> Self {
        Self { buffer: Vec::new(), compression_enabled, peer_masked: true }
    }

    pub fn set_compression_enabled(&mut self, enabled: bool) {
        self.compression_enabled = enabled;
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// 字节不足时返回 `Ok(None)`。
    pub fn next_frame(&mut self) -> Result<Option<RawFrame>, FrameError> {
        if self.buffer.len() < 2 {
            return Ok(None);
        }
        let first = self.buffer[0];
        let second = self.buffer[1];
        let fin = first & 0x80 != 0;
        let rsv1 = first & 0x40 != 0;
        let rsv2 = first & 0x20 != 0;
        let rsv3 = first & 0x10 != 0;
        let opcode = first & 0x0F;
        let masked = second & 0x80 != 0;
        let mut payload_len = (second & 0x7F) as u64;
        let mut header_len = 2usize;

        if (!self.compression_enabled && rsv1) || rsv2 || rsv3 {
            return Err(FrameError::Protocol(1002, "Unexpected RSV bits"));
        }
        if rsv1 && (opcode & 0x08 != 0 || opcode == OP_CONTINUATION) {
            return Err(FrameError::Protocol(1002, "RSV1 set on control or continuation frame"));
        }
        if opcode & 0x08 != 0 && !fin {
            return Err(FrameError::Protocol(1002, "Control frame fragmented"));
        }
        if self.peer_masked {
            if !masked {
                return Err(FrameError::Protocol(1002, "Unmasked frame from client"));
            }
        } else if masked {
            return Err(FrameError::Protocol(1002, "Masked frame from server"));
        }

        if payload_len == 126 {
            if self.buffer.len() < header_len + 2 {
                return Ok(None);
            }
            payload_len = u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as u64;
            header_len += 2;
        } else if payload_len == 127 {
            if self.buffer.len() < header_len + 8 {
                return Ok(None);
            }
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&self.buffer[2..10]);
            payload_len = u64::from_be_bytes(bytes);
            header_len += 8;
        }

        if opcode & 0x08 != 0 && payload_len > 125 {
            return Err(FrameError::Protocol(1002, "Control frame payload too large"));
        }
        if opcode & 0x08 == 0 && payload_len > MAX_MESSAGE_SIZE as u64 {
            return Err(FrameError::Protocol(1009, "Frame payload too large"));
        }

        let mask_start = header_len;
        if masked {
            header_len += 4;
        }
        let need = header_len + payload_len as usize;
        if self.buffer.len() < need {
            return Ok(None);
        }
        let mut payload = self.buffer[header_len..need].to_vec();
        if masked {
            let mask = &self.buffer[mask_start..mask_start + 4];
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        self.buffer.drain(..need);
        Ok(Some(RawFrame { fin, rsv1, opcode, payload }))
    }
}

/// 编码客户端帧；按 RFC 6455 客户端总是掩码。
pub fn encode_frame(opcode: u8, payload: &[u8], fin: bool, rsv1: bool) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    let mut first = opcode;
    if fin {
        first |= 0x80;
    }
    if rsv1 {
        first |= 0x40;
    }
    frame.push(first);

    let length = payload.len();
    if length < 126 {
        frame.push(0x80 | length as u8);
    } else if length <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }

    let mask: [u8; 4] = rand::random();
    frame.extend_from_slice(&mask);
    let start = frame.len();
    frame.extend_from_slice(payload);
    for (index, byte) in frame[start..].iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    frame
}

/// 完整重组后的入站消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close { code: u16, reason: String },
}

/// 帧解析器加分片、解压与 UTF-8 处理。
pub struct MessageAssembler {
    decoder: FrameDecoder,
    inflater: Option<Inflater>,
    fragments: Vec<u8>,
    fragment_opcode: u8,
    fragment_compressed: bool,
    in_progress: bool,
}

impl MessageAssembler {
    pub fn new() -> Self {
        Self {
            decoder: FrameDecoder::new(false),
            inflater: None,
            fragments: Vec::new(),
            fragment_opcode: 0,
            fragment_compressed: false,
            in_progress: false,
        }
    }

    /// 连接服务端方向的组装器：对端帧必须掩码。
    pub fn for_server() -> Self {
        let mut assembler = Self::new();
        assembler.decoder = FrameDecoder::for_server(false);
        assembler
    }

    /// 握手协商成功后启用 permessage-deflate。
    pub fn enable_compression(&mut self, config: DeflateConfig) -> Result<(), DeflateError> {
        self.inflater = Some(Inflater::new(config)?);
        self.decoder.set_compression_enabled(true);
        Ok(())
    }

    pub fn compression_enabled(&self) -> bool {
        self.inflater.is_some()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.decoder.push(bytes);
    }

    /// 返回下一条完整消息。分片簿记在内部消费 continuation 帧，
    /// 因此 `Ok(None)` 表示“缓冲中无完整消息”，而非“消息进行中”。
    pub fn next_message(&mut self) -> Result<Option<Incoming>, FrameError> {
        loop {
            let frame = match self.decoder.next_frame()? {
                Some(frame) => frame,
                None => return Ok(None),
            };
            let message = match frame.opcode {
                OP_CONTINUATION => self.continuation(frame)?,
                OP_TEXT | OP_BINARY => self.data(frame)?,
                OP_CLOSE => return Ok(Some(close_message(&frame.payload))),
                OP_PING => return Ok(Some(Incoming::Ping(frame.payload))),
                OP_PONG => return Ok(Some(Incoming::Pong(frame.payload))),
                _ => return Err(FrameError::Protocol(1002, "Unsupported opcode")),
            };
            if message.is_some() {
                return Ok(message);
            }
        }
    }

    fn continuation(&mut self, frame: RawFrame) -> Result<Option<Incoming>, FrameError> {
        if !self.in_progress {
            return Err(FrameError::Protocol(1002, "continuation frame without initial frame"));
        }
        if self.fragments.len() > MAX_MESSAGE_SIZE
            || frame.payload.len() > MAX_MESSAGE_SIZE - self.fragments.len()
        {
            return Err(FrameError::Protocol(1009, "Message too large"));
        }
        self.fragments.extend_from_slice(&frame.payload);
        if !frame.fin {
            return Ok(None);
        }

        let mut payload = std::mem::take(&mut self.fragments);
        if self.fragment_compressed {
            payload = self.inflate(payload)?;
        }
        let opcode = self.fragment_opcode;
        self.reset_fragments();
        self.finish(opcode, payload)
    }

    fn data(&mut self, frame: RawFrame) -> Result<Option<Incoming>, FrameError> {
        if self.in_progress {
            return Err(FrameError::Protocol(
                1002,
                "Received new data frame when expecting continuation frame",
            ));
        }
        let compressed = frame.rsv1 && self.compression_enabled();
        if !frame.fin {
            self.in_progress = true;
            self.fragment_opcode = frame.opcode;
            self.fragment_compressed = compressed;
            self.fragments = frame.payload;
            return Ok(None);
        }
        let payload = if compressed { self.inflate(frame.payload)? } else { frame.payload };
        self.finish(frame.opcode, payload)
    }

    fn inflate(&mut self, payload: Vec<u8>) -> Result<Vec<u8>, FrameError> {
        let inflater = match self.inflater.as_mut() {
            Some(inflater) => inflater,
            None => return Ok(payload),
        };
        match inflater.inflate(&payload) {
            Ok(output) => {
                inflater.after_message();
                Ok(output)
            }
            Err(DeflateError::OutputTooLarge) => {
                Err(FrameError::Protocol(1009, "Decompressed message too large"))
            }
            Err(_) => Err(FrameError::Protocol(1007, "Decompression failed")),
        }
    }

    fn finish(&mut self, opcode: u8, payload: Vec<u8>) -> Result<Option<Incoming>, FrameError> {
        match opcode {
            OP_TEXT => match String::from_utf8(payload) {
                Ok(text) => Ok(Some(Incoming::Text(text))),
                Err(_) => Err(FrameError::Protocol(1007, "Invalid UTF-8 in text message")),
            },
            OP_BINARY => Ok(Some(Incoming::Binary(payload))),
            _ => Err(FrameError::Protocol(1002, "Unknown fragmented opcode")),
        }
    }

    fn reset_fragments(&mut self) {
        self.in_progress = false;
        self.fragment_compressed = false;
        self.fragment_opcode = 0;
        self.fragments = Vec::new();
    }
}

impl Default for MessageAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// 关闭帧处理：校验码与原因，并产出需要回显的码。
fn close_message(payload: &[u8]) -> Incoming {
    let mut code = 1000u16;
    let mut reason = String::new();
    let mut protocol_error = false;

    if payload.len() == 1 {
        code = 1002;
        protocol_error = true;
    } else if payload.len() >= 2 {
        code = u16::from_be_bytes([payload[0], payload[1]]);
        let valid = ((1000..=1011).contains(&code)
            && code != 1004
            && code != 1005
            && code != 1006)
            || (3000..=4999).contains(&code);
        if !valid {
            code = 1002;
            protocol_error = true;
        }
        if payload.len() > 2 {
            let raw_reason = &payload[2..payload.len().min(125)];
            match std::str::from_utf8(raw_reason) {
                Ok(text) => reason = text.to_owned(),
                Err(_) => {
                    code = 1002;
                    protocol_error = true;
                }
            }
        }
    }

    if protocol_error {
        Incoming::Close { code: 1002, reason: String::new() }
    } else {
        Incoming::Close { code, reason }
    }
}

/// 构造 close 帧负载；空负载表示“不带状态码”。
pub fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    if code == 0 {
        return Vec::new();
    }
    let mut payload = code.to_be_bytes().to_vec();
    let reason_bytes = reason.as_bytes();
    let limit = reason_bytes.len().min(123);
    payload.extend_from_slice(&reason_bytes[..limit]);
    payload
}

/// permessage-deflate 发送侧的便捷封装。
pub fn compress_payload(deflater: &mut Deflater, payload: &[u8]) -> (Vec<u8>, bool) {
    match deflater.deflate(payload) {
        Some(compressed) => (compressed, true),
        None => (payload.to_vec(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 服务端方向编码器：帧永不掩码。
    fn server_frame(opcode: u8, payload: &[u8], fin: bool, rsv1: bool) -> Vec<u8> {
        let mut frame = Vec::new();
        let mut first = opcode;
        if fin {
            first |= 0x80;
        }
        if rsv1 {
            first |= 0x40;
        }
        frame.push(first);
        if payload.len() < 126 {
            frame.push(payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        frame
    }

    fn assemble(bytes: &[u8]) -> Result<Vec<Incoming>, FrameError> {
        let mut assembler = MessageAssembler::new();
        assembler.push(bytes);
        let mut messages = Vec::new();
        while let Some(message) = assembler.next_message()? {
            messages.push(message);
        }
        Ok(messages)
    }

    #[test]
    fn client_frames_are_masked_and_round_trip_through_a_server_decoder() {
        let payload = b"hello";
        let frame = encode_frame(OP_TEXT, payload, true, false);
        assert_eq!(frame[0], 0x81);
        assert_eq!(frame[1] & 0x80, 0x80);
        let mask = [frame[2], frame[3], frame[4], frame[5]];
        let decoded: Vec<u8> = frame[6..]
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4])
            .collect();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn extended_length_headers_are_encoded() {
        let payload = vec![1u8; 300];
        let frame = encode_frame(OP_BINARY, &payload, true, false);
        assert_eq!(frame[1] & 0x7F, 126);
        assert_eq!(u16::from_be_bytes([frame[2], frame[3]]), 300);

        let payload = vec![1u8; 70000];
        let frame = encode_frame(OP_BINARY, &payload, true, false);
        assert_eq!(frame[1] & 0x7F, 127);
    }

    #[test]
    fn text_and_binary_messages_are_reassembled() {
        let mut bytes = server_frame(OP_TEXT, b"hel", false, false);
        bytes.extend(server_frame(OP_CONTINUATION, b"lo", true, false));
        bytes.extend(server_frame(OP_BINARY, &[1, 2, 3, 4], true, false));
        assert_eq!(
            assemble(&bytes).unwrap(),
            vec![
                Incoming::Text("hello".to_owned()),
                Incoming::Binary(vec![1, 2, 3, 4])
            ]
        );
    }

    #[test]
    fn control_frames_may_interleave_fragments() {
        let mut bytes = server_frame(OP_TEXT, b"hel", false, false);
        bytes.extend(server_frame(OP_PING, b"ping", true, false));
        bytes.extend(server_frame(OP_CONTINUATION, b"lo", true, false));
        assert_eq!(
            assemble(&bytes).unwrap(),
            vec![Incoming::Ping(b"ping".to_vec()), Incoming::Text("hello".to_owned())]
        );
    }

    #[test]
    fn partial_frames_wait_for_more_bytes() {
        let mut assembler = MessageAssembler::new();
        let bytes = server_frame(OP_TEXT, b"hello", true, false);
        assembler.push(&bytes[..3]);
        assert!(assembler.next_message().unwrap().is_none());
        assembler.push(&bytes[3..]);
        assert_eq!(assembler.next_message().unwrap(), Some(Incoming::Text("hello".to_owned())));
        assert!(assembler.next_message().unwrap().is_none());
    }

    #[test]
    fn protocol_violations_use_the_libwsc_close_codes() {
        let masked = {
            let mut frame = server_frame(OP_TEXT, b"x", true, false);
            frame[1] |= 0x80;
            frame
        };
        assert_eq!(assemble(&masked).unwrap_err().close_code(), 1002);

        let rsv2 = {
            let mut frame = server_frame(OP_TEXT, b"x", true, false);
            frame[0] |= 0x20;
            frame
        };
        assert_eq!(assemble(&rsv2).unwrap_err().close_code(), 1002);

        let fragmented_control = server_frame(OP_PING, b"x", false, false);
        assert_eq!(assemble(&fragmented_control).unwrap_err().close_code(), 1002);

        let big_control = server_frame(OP_PING, &[0u8; 126], true, false);
        assert_eq!(assemble(&big_control).unwrap_err().close_code(), 1002);

        let unknown = server_frame(0x3, b"x", true, false);
        assert_eq!(assemble(&unknown).unwrap_err().close_code(), 1002);

        let continuation = server_frame(OP_CONTINUATION, b"x", true, false);
        assert_eq!(assemble(&continuation).unwrap_err().close_code(), 1002);

        let invalid_utf8 = server_frame(OP_TEXT, &[0xFF, 0xFE], true, false);
        assert_eq!(assemble(&invalid_utf8).unwrap_err().close_code(), 1007);
    }

    #[test]
    fn rsv1_requires_negotiated_compression() {
        let compressed_looking = server_frame(OP_TEXT, b"x", true, true);
        assert_eq!(assemble(&compressed_looking).unwrap_err().close_code(), 1002);
    }

    #[test]
    fn close_codes_are_validated_and_echoed() {
        let normal = server_frame(OP_CLOSE, &close_payload(1000, "bye"), true, false);
        assert_eq!(
            assemble(&normal).unwrap(),
            vec![Incoming::Close { code: 1000, reason: "bye".to_owned() }]
        );

        let empty = server_frame(OP_CLOSE, &[], true, false);
        assert_eq!(
            assemble(&empty).unwrap(),
            vec![Incoming::Close { code: 1000, reason: String::new() }]
        );

        for code in [1004u16, 1005, 1006, 999, 2999, 5000] {
            let frame = server_frame(OP_CLOSE, &close_payload(code, ""), true, false);
            assert_eq!(
                assemble(&frame).unwrap(),
                vec![Incoming::Close { code: 1002, reason: String::new() }],
                "code {code}"
            );
        }

        let application = server_frame(OP_CLOSE, &close_payload(4000, "app"), true, false);
        assert_eq!(
            assemble(&application).unwrap(),
            vec![Incoming::Close { code: 4000, reason: "app".to_owned() }]
        );

        let one_byte = server_frame(OP_CLOSE, &[0x03], true, false);
        assert_eq!(
            assemble(&one_byte).unwrap(),
            vec![Incoming::Close { code: 1002, reason: String::new() }]
        );

        let bad_reason = server_frame(OP_CLOSE, &[0x03, 0xE8, 0xFF], true, false);
        assert_eq!(
            assemble(&bad_reason).unwrap(),
            vec![Incoming::Close { code: 1002, reason: String::new() }]
        );
    }

    #[test]
    fn compressed_messages_round_trip() {
        let config = DeflateConfig::default();
        let mut deflater = Deflater::new(config.clone()).unwrap();
        let mut assembler = MessageAssembler::new();
        assembler.enable_compression(config).unwrap();

        let payload = b"{\"type\":\"streamAudio\",\"audioData\":{\"type\":\"raw\"}}";
        let compressed = deflater.deflate(payload).unwrap();
        assembler.push(&server_frame(OP_TEXT, &compressed, true, true));
        assert_eq!(
            assembler.next_message().unwrap(),
            Some(Incoming::Text(String::from_utf8(payload.to_vec()).unwrap()))
        );

        let binary: Vec<u8> = (0..4096u32).map(|index| (index % 256) as u8).collect();
        let compressed = deflater.deflate(&binary).unwrap();
        let mut bytes = server_frame(OP_BINARY, &compressed[..compressed.len() / 2], false, true);
        bytes.extend(server_frame(OP_CONTINUATION, &compressed[compressed.len() / 2..], true, false));
        assembler.push(&bytes);
        assert_eq!(assembler.next_message().unwrap(), Some(Incoming::Binary(binary)));
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let mut header = vec![0x82u8, 127];
        header.extend_from_slice(&((MAX_MESSAGE_SIZE as u64) + 1).to_be_bytes());
        assert_eq!(assemble(&header).unwrap_err().close_code(), 1009);
    }
}
