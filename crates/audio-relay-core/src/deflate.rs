use libz_sys as z;

/// 握手期间协商出的 permessage-deflate 参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeflateConfig {
    pub client_no_context_takeover: bool,
    pub server_no_context_takeover: bool,
    pub client_max_window_bits: i32,
    pub server_max_window_bits: i32,
    pub compression_level: i32,
}

impl Default for DeflateConfig {
    fn default() -> Self {
        Self {
            client_no_context_takeover: false,
            server_no_context_takeover: false,
            client_max_window_bits: 15,
            server_max_window_bits: 15,
            compression_level: 6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeflateError {
    InitFailed(i32),
    InflateFailed(i32),
    InflateStalled,
    /// 解压输出将超过 16 MiB 消息上限。
    OutputTooLarge,
}

impl std::fmt::Display for DeflateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InitFailed(code) => write!(formatter, "zlib init failed ({code})"),
            Self::InflateFailed(code) => write!(formatter, "inflate failed ({code})"),
            Self::InflateStalled => formatter.write_str("inflate stalled"),
            Self::OutputTooLarge => formatter.write_str("decompressed message too large"),
        }
    }
}

/// 桥接接受的最大消息大小，与 libwsc `MAX_MESSAGE_SIZE` 一致。
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

const SYNC_FLUSH_TRAILER: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];

fn version() -> *const std::ffi::c_char {
        // SAFETY: `zlibVersion` 无前置条件。
    unsafe { z::zlibVersion() }
}

// `zalloc`/`zfree` 在 `z_stream` 中是裸函数指针，清零的流不是合法
// 值；zlib 接受 Z_NULL 钩子并回退到默认实现，因此这里提供基于 libc
// 的可用钩子。
unsafe extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn free(ptr: *mut std::ffi::c_void);
}

unsafe extern "C" fn zalloc(
    _opaque: z::voidpf,
    items: z::uInt,
    size: z::uInt,
) -> z::voidpf {
    // SAFETY: 转发到 libc `malloc`；zlib 分别传入元素个数与大小，
    // 与 C `zalloc` 契约一致。
    unsafe { malloc(items as usize * size as usize) }
}

unsafe extern "C" fn zfree(_opaque: z::voidpf, ptr: z::voidpf) {
    // SAFETY: `ptr` 由 `zalloc`/libc `malloc` 分配。
    if !ptr.is_null() {
                // SAFETY: `ptr` 为 null 或由 `zalloc`/libc `malloc` 分配。
        unsafe { free(ptr) };
    }
}

fn blank_stream() -> z::z_stream {
    z::z_stream {
        next_in: std::ptr::null_mut(),
        avail_in: 0,
        total_in: 0,
        next_out: std::ptr::null_mut(),
        avail_out: 0,
        total_out: 0,
        msg: std::ptr::null_mut(),
        state: std::ptr::null_mut(),
        zalloc,
        zfree,
        opaque: std::ptr::null_mut(),
        data_type: 0,
        adler: 0,
        reserved: 0,
    }
}

fn stream_size() -> i32 {
    std::mem::size_of::<z::z_stream>() as i32
}

/// 发送侧：raw deflate 并剥掉 SYNC_FLUSH 尾部。
pub struct Deflater {
    // 装箱原因：zlib 在其内部状态中保存指向 `z_stream` 的反向指针；
    // `init` 之后移动结构体会使该指针失效。
    stream: Box<z::z_stream>,
    config: DeflateConfig,
    initialized: bool,
}

impl Deflater {
    pub fn new(config: DeflateConfig) -> Result<Self, DeflateError> {
        let mut deflater = Self {
            stream: Box::new(blank_stream()),
            config,
            initialized: false,
        };
        deflater.init()?;
        Ok(deflater)
    }

    fn init(&mut self) -> Result<(), DeflateError> {
                // SAFETY: `self.stream` 由 `blank_stream` 初始化，是可写存储。
        let result = unsafe {
            z::deflateInit2_(
                &mut *self.stream,
                self.config.compression_level,
                z::Z_DEFLATED,
                -self.config.client_max_window_bits,
                8,
                z::Z_DEFAULT_STRATEGY,
                version(),
                stream_size(),
            )
        };
        if result != z::Z_OK {
            return Err(DeflateError::InitFailed(result));
        }
        self.initialized = true;
        Ok(())
    }

    /// 无法产出合法 SYNC_FLUSH 块时返回 `None`；调用方随后按 libwsc
    /// 的做法发送未压缩负载。
    pub fn deflate(&mut self, input: &[u8]) -> Option<Vec<u8>> {
        if !self.initialized {
            return None;
        }
        for attempt in 0..4 {
                        // SAFETY: 流已由 `init` 初始化。
            unsafe { z::deflateReset(&mut *self.stream) };
            let extra = 64 * (attempt + 1);
                        // SAFETY: 流已由 `init` 初始化。
            let bound = unsafe { z::deflateBound(&mut *self.stream, input.len() as z::uLong) } as usize
                + extra;
            let mut buffer = vec![0u8; bound];

            self.stream.next_in = input.as_ptr() as *mut u8;
            self.stream.avail_in = input.len() as z::uInt;
            self.stream.next_out = buffer.as_mut_ptr();
            self.stream.avail_out = bound as z::uInt;

                        // SAFETY: 流指针如上设置，`buffer` 在 `avail_out`
                        // 字节内可写。
            let result = unsafe { z::deflate(&mut *self.stream, z::Z_SYNC_FLUSH) };
            if result != z::Z_OK {
                if result == z::Z_BUF_ERROR || self.stream.avail_out == 0 {
                    continue;
                }
                return None;
            }

            let produced = bound - self.stream.avail_out as usize;
            if produced < 4 || buffer[produced - 4..produced] != SYNC_FLUSH_TRAILER {
                continue;
            }
            buffer.truncate(produced - 4);

            if self.config.client_no_context_takeover {
                                // SAFETY: 流已由 `init` 初始化，且只结束一次。
                unsafe { z::deflateEnd(&mut *self.stream) };
                self.initialized = false;
                *self.stream = blank_stream();
                if self.init().is_err() {
                    return None;
                }
            }
            return Some(buffer);
        }
        None
    }
}

impl Drop for Deflater {
    fn drop(&mut self) {
        if self.initialized {
                        // SAFETY: 流已由 `init` 初始化，且只结束一次。
            unsafe { z::deflateEnd(&mut *self.stream) };
            self.initialized = false;
        }
    }
}

/// 接收侧：追加 SYNC_FLUSH 尾部并解压整条消息。
pub struct Inflater {
    // 装箱原因与 `Deflater::stream` 相同。
    stream: Box<z::z_stream>,
    config: DeflateConfig,
    initialized: bool,
}

impl Inflater {
    pub fn new(config: DeflateConfig) -> Result<Self, DeflateError> {
        let mut inflater = Self {
            stream: Box::new(blank_stream()),
            config,
            initialized: false,
        };
        inflater.init()?;
        Ok(inflater)
    }

    fn init(&mut self) -> Result<(), DeflateError> {
                // SAFETY: `self.stream` 由 `blank_stream` 初始化，是可写存储。
        let result = unsafe {
            z::inflateInit2_(
                &mut *self.stream,
                -self.config.server_max_window_bits,
                version(),
                stream_size(),
            )
        };
        if result != z::Z_OK {
            return Err(DeflateError::InitFailed(result));
        }
        self.initialized = true;
        Ok(())
    }

    pub fn inflate(&mut self, input: &[u8]) -> Result<Vec<u8>, DeflateError> {
        if !self.initialized {
            return Ok(input.to_vec());
        }
        let mut source = Vec::with_capacity(input.len() + 4);
        source.extend_from_slice(input);
        source.extend_from_slice(&SYNC_FLUSH_TRAILER);

        if self.config.server_no_context_takeover {
                        // SAFETY: 流已由 `init` 初始化。
            unsafe { z::inflateReset(&mut *self.stream) };
        }

        self.stream.next_in = source.as_ptr() as *mut u8;
        self.stream.avail_in = source.len() as z::uInt;

        let mut output: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            self.stream.next_out = chunk.as_mut_ptr();
            self.stream.avail_out = chunk.len() as z::uInt;
            let before_in = self.stream.avail_in;

                        // SAFETY: 流指针如上设置，`chunk` 在 `avail_out`
                        // 字节内可写。
            let result = unsafe { z::inflate(&mut *self.stream, z::Z_SYNC_FLUSH) };
            if result == z::Z_BUF_ERROR {
                if self.stream.avail_out == 0 {
                    continue;
                }
                if self.stream.avail_in == 0 {
                    break;
                }
                return Err(DeflateError::InflateStalled);
            }
            if result != z::Z_OK && result != z::Z_STREAM_END {
                return Err(DeflateError::InflateFailed(result));
            }

            let produced = chunk.len() - self.stream.avail_out as usize;
            if produced > 0 {
                if output.len() > MAX_MESSAGE_SIZE || produced > MAX_MESSAGE_SIZE - output.len() {
                    return Err(DeflateError::OutputTooLarge);
                }
                output.extend_from_slice(&chunk[..produced]);
            }
            if result == z::Z_STREAM_END {
                break;
            }
            if self.stream.avail_in == 0 && self.stream.avail_out != 0 {
                break;
            }
            if produced == 0 && self.stream.avail_in == before_in {
                return Err(DeflateError::InflateStalled);
            }
        }
        Ok(output)
    }

    /// 每条完整消息之后调用；服务器要求不保留上下文时重新初始化窗口。
    pub fn after_message(&mut self) {
        if !self.config.server_no_context_takeover || !self.initialized {
            return;
        }
                // SAFETY: 流已由 `init` 初始化，且只结束一次。
        unsafe { z::inflateEnd(&mut *self.stream) };
        self.initialized = false;
        *self.stream = blank_stream();
        let _ = self.init();
    }
}

impl Drop for Inflater {
    fn drop(&mut self) {
        if self.initialized {
                        // SAFETY: 流已由 `init` 初始化，且只结束一次。
            unsafe { z::inflateEnd(&mut *self.stream) };
            self.initialized = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(config: DeflateConfig, payload: &[u8]) -> Vec<u8> {
        let mut deflater = Deflater::new(config.clone()).unwrap();
        let mut inflater = Inflater::new(config).unwrap();
        let compressed = deflater.deflate(payload).expect("compression must succeed");
        let decompressed = inflater.inflate(&compressed).unwrap();
        inflater.after_message();
        decompressed
    }

    #[test]
    fn text_round_trips_with_context_takeover() {
        let payload = b"hello permessage-deflate, hello again, hello once more";
        assert_eq!(round_trip(DeflateConfig::default(), payload), payload);
    }

    #[test]
    fn binary_pcm_round_trips() {
        let payload: Vec<u8> = (0..8192u32).map(|index| (index % 251) as u8).collect();
        assert_eq!(round_trip(DeflateConfig::default(), &payload), payload);
    }

    #[test]
    fn no_context_takeover_round_trips_repeated_messages() {
        let config = DeflateConfig {
            client_no_context_takeover: true,
            server_no_context_takeover: true,
            ..Default::default()
        };
        let mut deflater = Deflater::new(config.clone()).unwrap();
        let mut inflater = Inflater::new(config).unwrap();
        for index in 0..5 {
            let payload = format!("message {index} with some repeated content content content");
            let compressed = deflater.deflate(payload.as_bytes()).unwrap();
            assert_eq!(inflater.inflate(&compressed).unwrap(), payload.as_bytes());
            inflater.after_message();
        }
    }

    #[test]
    fn reduced_window_bits_round_trip() {
        for bits in [9, 12, 15] {
            let config = DeflateConfig {
                client_max_window_bits: bits,
                server_max_window_bits: bits,
                ..Default::default()
            };
            let payload = vec![7u8; 4096];
            let mut deflater = match Deflater::new(config.clone()) {
                Ok(deflater) => deflater,
                // zlib 可能拒绝 8/9 位 deflate 窗口；libwsc 此时禁用
                // 压缩，本实现镜像该行为。
                Err(_) => continue,
            };
            let mut inflater = Inflater::new(config).unwrap();
            let compressed = deflater.deflate(&payload).unwrap();
            assert_eq!(inflater.inflate(&compressed).unwrap(), payload);
        }
    }

    #[test]
    fn empty_payload_round_trips() {
        assert_eq!(round_trip(DeflateConfig::default(), b""), b"");
    }

    #[test]
    fn decompression_bomb_is_capped() {
        let mut deflater = Deflater::new(DeflateConfig::default()).unwrap();
        let payload = vec![0u8; MAX_MESSAGE_SIZE + 4096];
        let compressed = deflater.deflate(&payload).unwrap();
        let mut inflater = Inflater::new(DeflateConfig::default()).unwrap();
        assert_eq!(inflater.inflate(&compressed), Err(DeflateError::OutputTooLarge));
    }

    #[test]
    fn corrupt_payload_reports_failure() {
        let mut inflater = Inflater::new(DeflateConfig::default()).unwrap();
        assert!(matches!(
            inflater.inflate(&[0xFF, 0xFF, 0xFF, 0xFF, 0x01]),
            Err(DeflateError::InflateFailed(_)) | Err(DeflateError::InflateStalled)
        ));
    }
}
