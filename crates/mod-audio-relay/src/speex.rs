//! 精简的 `speex_resampler` FFI 及 `audio-relay-core` 使用的
//! `ResampleEngine` 实现，直接调用 libspeexdsp 入口。

use std::ptr;

use audio_relay_core::resampler::{ResampleEngine, ResampleError};

/// speex_resampler.h 中的 opaque `SpeexResamplerState`。
#[repr(C)]
struct SpeexResamplerState {
    _private: [u8; 0],
}

extern "C" {
    fn speex_resampler_init(
        nb_channels: u32,
        in_rate: u32,
        out_rate: u32,
        quality: i32,
        err: *mut i32,
    ) -> *mut SpeexResamplerState;
    fn speex_resampler_destroy(st: *mut SpeexResamplerState);
    fn speex_resampler_process_int(
        st: *mut SpeexResamplerState,
        channel_index: u32,
        input: *const i16,
        in_len: *mut u32,
        output: *mut i16,
        out_len: *mut u32,
    ) -> i32;
    fn speex_resampler_process_interleaved_int(
        st: *mut SpeexResamplerState,
        input: *const i16,
        in_len: *mut u32,
        output: *mut i16,
        out_len: *mut u32,
    ) -> i32;
    fn speex_resampler_get_output_latency(st: *mut SpeexResamplerState) -> u32;
}

/// 基于 libspeexdsp 的 `ResampleEngine`。由于内部是裸指针，默认不实现
/// `Send`；该引擎只会存在于媒体 bug 处理器的 `CapturePipeline` 或
/// streamer 的播放互斥锁中，两者都只在会话媒体线程上访问。
pub struct SpeexEngine {
    state: *mut SpeexResamplerState,
    channels: u32,
}

// SAFETY: 重采样状态只经由 `PcmResampler` 访问，而 streamer 将其限制在
// 播放 `Mutex` 内；不会有两个线程并发驱动同一个 `SpeexEngine`。
unsafe impl Send for SpeexEngine {}

impl SpeexEngine {
    pub fn new() -> Self {
        Self {
            state: ptr::null_mut(),
            channels: 0,
        }
    }
}

impl Default for SpeexEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ResampleEngine for SpeexEngine {
    fn init(
        &mut self,
        channels: u32,
        input_rate: u32,
        output_rate: u32,
        quality: u32,
    ) -> Result<(), ResampleError> {
        self.destroy();
        let mut err = 0i32;
        // SAFETY: 普通 C 调用；`err` 是有效的输出存储。
        let state = unsafe {
            speex_resampler_init(channels, input_rate, output_rate, quality as i32, &mut err)
        };
        if err != 0 || state.is_null() {
            return Err(ResampleError::InitFailed);
        }
        self.state = state;
        self.channels = channels;
        Ok(())
    }

    fn process(
        &mut self,
        input: &[i16],
        output: &mut [i16],
    ) -> Result<(usize, usize), ResampleError> {
        if self.state.is_null() {
            return Err(ResampleError::ProcessFailed);
        }
        let channels = self.channels.max(1) as usize;
        if !input.len().is_multiple_of(channels) || !output.len().is_multiple_of(channels) {
            return Err(ResampleError::InvalidInput);
        }
        // Speex 按每声道帧数计数，而不是交错采样总数。
        let mut in_len = (input.len() / channels) as u32;
        let mut out_len = (output.len() / channels) as u32;
        // SAFETY: `state` 存活；input/output 按各自长度有效。
        let status = unsafe {
            if self.channels == 1 {
                speex_resampler_process_int(
                    self.state,
                    0,
                    input.as_ptr(),
                    &mut in_len,
                    output.as_mut_ptr(),
                    &mut out_len,
                )
            } else {
                speex_resampler_process_interleaved_int(
                    self.state,
                    input.as_ptr(),
                    &mut in_len,
                    output.as_mut_ptr(),
                    &mut out_len,
                )
            }
        };
        if status != 0 {
            return Err(ResampleError::ProcessFailed);
        }
        Ok((in_len as usize, out_len as usize))
    }

    fn output_latency(&self) -> u32 {
        if self.state.is_null() {
            return 0;
        }
        // SAFETY: `state` 存活。
        unsafe { speex_resampler_get_output_latency(self.state) }
    }

    fn destroy(&mut self) {
        if !self.state.is_null() {
            // SAFETY: `state` 由 speex_resampler_init 创建，这里只释放一次。
            unsafe { speex_resampler_destroy(self.state) };
            self.state = ptr::null_mut();
        }
        self.channels = 0;
    }
}

impl Drop for SpeexEngine {
    fn drop(&mut self) {
        self.destroy();
    }
}
