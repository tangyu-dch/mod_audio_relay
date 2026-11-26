//! 基于 `switch_core_file_*` 的 `AudioDecoder`：文件按播放采样率打开，
//! 以单声道有符号 16 位小端采样读取。

use audio_relay_core::streamer::AudioDecoder;

use crate::sysutil;
use fswtch::sys;

/// 每次读取的采样数，对应 C++ 中的 `int16_t samples[4096]`。
const SAMPLES_PER_READ: usize = 4096;

pub struct FsDecoder;

impl FsDecoder {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDecoder for FsDecoder {
    fn decode(&self, path: &str, rate: u32, sink: &mut dyn FnMut(&[u8]) -> bool) -> bool {
        let path = sysutil::cstring(path);
        // SAFETY: `switch_file_handle_t` 是普通 C 结构体；清零等价于
        // 在使用前 memset。
        let mut handle: sys::switch_file_handle_t = unsafe { std::mem::zeroed() };

        // 等价于 `switch_core_file_open(&fh, path, 1, rate, READ|DATA_SHORT, NULL)`。
        // SAFETY: `handle` 是已清零的可写存储，`path` 是存活的 C 字符串；
        // 成功时由 FreeSWITCH 初始化句柄。
        let status = unsafe {
            sys::switch_core_perform_file_open(
                c"mod_audio_relay".as_ptr(),
                c"decode".as_ptr(),
                line!() as i32,
                &mut handle,
                path.as_ptr(),
                1,
                rate,
                sys::switch_file_flag_enum_t_SWITCH_FILE_FLAG_READ
                    | sys::switch_file_flag_enum_t_SWITCH_FILE_DATA_SHORT,
                std::ptr::null_mut(),
            )
        };
        if status != fswtch::SUCCESS {
            return false;
        }

        let mut samples = [0i16; SAMPLES_PER_READ];
        loop {
            let mut count = SAMPLES_PER_READ as sys::switch_size_t;
            // SAFETY: `handle` 已打开；`samples`/`count` 是有效存储，
            // `count` 返回时为实际读取的采样数。
            let status = unsafe {
                sys::switch_core_file_read(
                    &mut handle,
                    samples.as_mut_ptr().cast(),
                    &mut count,
                )
            };
            if status != fswtch::SUCCESS || count == 0 {
                break;
            }
            // `count` 是采样数，换算字节数为 `count * sizeof(int16_t)`。
            // SAFETY: `samples` 中存放着 `switch_core_file_read` 写入的
            // `count` 个有效 i16 采样。
            let chunk = unsafe {
                std::slice::from_raw_parts(
                    samples.as_ptr().cast::<u8>(),
                    count as usize * std::mem::size_of::<i16>(),
                )
            };
            if !sink(chunk) {
                break;
            }
        }

        // SAFETY: `handle` 在上方已打开，这里只关闭一次。
        unsafe {
            sys::switch_core_file_close(&mut handle);
        }
        true
    }
}
