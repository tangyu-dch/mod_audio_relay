//! FreeSWITCH 音频流中继桥的实现无关逻辑。
//!
//! 所有不依赖 FreeSWITCH ABI 的部分都放在这里，可在没有 switch 的
//! 环境中单元测试：带 permessage-deflate 的 RFC 6455 传输层、播放环形
//! 缓冲、饱和混音器、FIFO 侧信道、`streamAudio` 协议与流状态机。

pub mod config;
pub mod deflate;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod protocol;
pub mod queue;
pub mod resampler;
pub mod fifo;
pub mod mixer;
pub mod state;
pub mod streamer;
pub mod transport;
pub mod url;

pub use config::StreamConfig;
pub use error::ErrorCode;
pub use protocol::{MessageAction, StreamAudioOutcome};
pub use queue::PlaybackQueue;
pub use resampler::{PcmResampler, ResampleError, ResampleEngine};
pub use fifo::{PlaybackFifo, PlaybackFifoError};
pub use state::{PlaybackAction, StreamState};
pub use streamer::{AudioDecoder, EventKind, Streamer};
pub use transport::{LogSink, TlsOptions, WebSocketTransport};
pub use url::WebSocketUrl;

/// 桥接接受的最大二进制 PCM 消息大小，对应模块契约。
pub const MAX_BINARY_PCM_BYTES: usize = 8192;

/// 8 kHz 20 ms L16 帧的字节数。
pub const FRAME_SIZE_8000: usize = 320;

/// `switch_core_media_bug_read` 风格传输使用的缓冲区大小。
pub const RECOMMENDED_BUFFER_SIZE: usize = 8192;
