//! API 命令、媒体 bug 与传输钩子共用的 FreeSWITCH ABI 辅助函数。
//! 这里的每个函数都是对 `fswtch::sys` 的薄封装并附说明。

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::ptr;

use fswtch::sys;

pub const MODULE_NAME: &str = "mod_audio_relay";
pub const BUG_NAME: &str = "audio_relay";

/// 供 `switch_channel_*_private` 使用的静态 C 字符串，即 channel private 键名。
pub fn context_key() -> &'static std::ffi::CStr {
    c"audio_relay_context"
}

pub const API_SYNTAX: &str = "<uuid> [start | stop | stop_drain | send_text | pause | resume | interrupt | pause_playback | resume_playback | stop_playback | status] [wss-url] [mono | mixed | stereo] [8k..64k] [metadata]";

/// `StreamConfig::from_variables` 会读取的通道变量；每次 `start` 只做一次
/// 快照，使 core crate 保持与 FreeSWITCH 无关。
const STREAM_VARIABLES: [&str; 11] = [
    "STREAM_MESSAGE_DEFLATE",
    "STREAM_SUPPRESS_LOG",
    "STREAM_TLS_CA_FILE",
    "STREAM_TLS_KEY_FILE",
    "STREAM_TLS_CERT_FILE",
    "STREAM_TLS_DISABLE_HOSTNAME_VALIDATION",
    "STREAM_HEART_BEAT",
    "STREAM_BUFFER_SIZE",
    "STREAM_AUDIO_PIPE",
    "STREAM_AUDIO_PIPE_RATE",
    "STREAM_EXTRA_HEADERS",
];

/// switch_utils.c 中的 `ESCAPE_META`。
const ESCAPE_META: u8 = b'\\';

pub fn cstring(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

pub fn bytes_cstring(bytes: &[u8]) -> CString {
    CString::new(bytes).unwrap_or_default()
}

/// 对存活 session 指针调用 `switch_core_session_get_channel`。
// SAFETY: 调用方保证 session 指针在回调或 locate 期间由 FreeSWITCH 保持存活。
pub unsafe fn channel_of(session: *mut sys::switch_core_session_t) -> *mut sys::switch_channel_t {
    // SAFETY: 调用方保证 `session` 在调用期间存活。
    unsafe { sys::switch_core_session_get_channel(session) }
}

/// `switch_core_session_get_uuid`，复制为自有字符串。
pub unsafe fn session_uuid(session: *mut sys::switch_core_session_t) -> Option<String> {
    // SAFETY: 调用方保证 `session` 在调用期间存活。
    let raw = unsafe { sys::switch_core_session_get_uuid(session) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: FreeSWITCH 对存活会话返回 NUL 结尾的 uuid。
    unsafe { CStr::from_ptr(raw) }.to_str().ok().map(str::to_owned)
}

// 释放 `switch_channel_get_variable_strdup` 返回字符串的 `free(3)`。
unsafe extern "C" {
    fn free(ptr: *mut std::ffi::c_void);
}

/// `SWITCH_GLOBAL_dirs.temp_dir`，`streamAudio` 文件的暂存目录。
pub fn temp_dir() -> Option<String> {
    // SAFETY: `SWITCH_GLOBAL_dirs` 在模块加载前已初始化；
    // 该字段经由 `addr_of` 读取，因为它是 `static mut`。
    let raw = unsafe { (*std::ptr::addr_of!(sys::SWITCH_GLOBAL_dirs)).temp_dir };
    if raw.is_null() {
        return None;
    }
    // SAFETY: `temp_dir` 是 FreeSWITCH 启动时设置的 NUL 结尾路径。
    unsafe { CStr::from_ptr(raw) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// 模块级日志行，等价于 `switch_log_printf(SWITCH_CHANNEL_LOG, ...)`。
pub fn log(level: sys::switch_log_level_t, message: &str) {
    log_at(
        sys::switch_text_channel_t_SWITCH_CHANNEL_ID_LOG,
        ptr::null(),
        level,
        message,
    );
}

/// 会话级日志行，等价于
/// `switch_log_printf(SWITCH_CHANNEL_SESSION_LOG(session), ...)`。
/// 对于 `SWITCH_CHANNEL_ID_SESSION`，FreeSWITCH 把 `userdata` 当作存活的
/// `switch_core_session_t *` 而不是 uuid 字符串，因此调用方必须传入
/// session 指针本身。
// SAFETY: `session` 必须在调用期间保持存活。
pub unsafe fn log_session_ptr(
    session: *mut sys::switch_core_session_t,
    level: sys::switch_log_level_t,
    message: &str,
) {
    log_at(
        sys::switch_text_channel_t_SWITCH_CHANNEL_ID_SESSION,
        session as *const c_char,
        level,
        message,
    );
}

/// 按 uuid 记录的会话级日志行：先 locate 会话（获取读锁），经
/// `log_session_ptr` 输出日志，再释放锁。会话已不存在时回退为模块级日志。
pub fn log_session_uuid(uuid: &CStr, level: sys::switch_log_level_t, message: &str) {
    let session = locate_session(uuid.to_bytes());
    if session.is_null() {
        log(level, message);
        return;
    }
    // SAFETY: `session` 来自 `locate_session`，在日志调用期间仍持有读锁。
    unsafe { log_session_ptr(session, level, message) };
    // SAFETY: `session` 来自 `locate_session`，仍持有其锁。
    unsafe { rwunlock(session) };
}

fn log_at(
    channel: sys::switch_text_channel_t,
    userdata: *const c_char,
    level: sys::switch_log_level_t,
    message: &str,
) {
    let text = CString::new(message).unwrap_or_default();
    // SAFETY: 所有指针在调用期间都是有效的 C 字符串。
    unsafe {
        sys::switch_log_printf(
            channel,
            c"mod_audio_relay".as_ptr(),
            c"log".as_ptr(),
            line!() as i32,
            userdata,
            level,
            c"%s".as_ptr(),
            text.as_ptr(),
        );
    }
}

/// `switch_core_session_perform_locate`，调用方必须用
/// `switch_core_session_rwunlock` 释放。uuid 未知时返回 NULL。
pub fn locate_session(uuid: &[u8]) -> *mut sys::switch_core_session_t {
    let uuid = bytes_cstring(uuid);
    // SAFETY: `uuid` 是存活的 C 字符串；非 NULL 结果带有读锁，
    // 调用方须用 switch_core_session_rwunlock 释放。
    unsafe {
        sys::switch_core_session_perform_locate(
            uuid.as_ptr(),
            c"mod_audio_relay".as_ptr(),
            c"locate_session".as_ptr(),
            line!() as i32,
        )
    }
}

// SAFETY: `session` 必须来自 `locate_session`（或存活的回调），
// 且仍持有其读锁。
pub unsafe fn rwunlock(session: *mut sys::switch_core_session_t) {
    // SAFETY: 调用方保证 `session` 持有其读锁。
    unsafe { sys::switch_core_session_rwunlock(session) };
}

/// 把一个通道变量读为自有字符串。纯 `switch_channel_get_variable` 在 C 头文件
/// 中是内联实现、未包含在绑定里，因此使用
/// `switch_channel_get_variable_strdup`，并用 libc `free` 释放返回的
/// `strdup` 分配。
pub unsafe fn channel_variable(
    channel: *mut sys::switch_channel_t,
    name: &str,
) -> Option<String> {
    let name = cstring(name);
    // SAFETY: 调用方保证 `channel` 在调用期间存活。
    let raw = unsafe { sys::switch_channel_get_variable_strdup(channel, name.as_ptr()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: FreeSWITCH 返回调用方持有的 NUL 结尾 `strdup` 分配。
    let value = unsafe { CStr::from_ptr(raw) }
        .to_str()
        .ok()
        .map(str::to_owned);
    // SAFETY: `raw` 是上面调用返回的 `strdup` 结果，复制后恰好释放一次。
    unsafe { free(raw as *mut std::ffi::c_void) };
    value
}

/// `StreamConfig::from_variables` 所有变量的快照。
pub unsafe fn stream_variables(
    channel: *mut sys::switch_channel_t,
) -> BTreeMap<String, String> {
    let mut variables = BTreeMap::new();
    for name in STREAM_VARIABLES {
        // SAFETY: `channel` 在 start 命令期间保持存活。
        if let Some(value) = unsafe { channel_variable(channel, name) } {
            variables.insert(name.to_owned(), value);
        }
    }
    variables
}

/// UTF-8 字节形状检查，直接操作原始字节，
/// 使非法输入在前置 UTF-8 解码时被拒绝而不是丢失。
pub fn is_valid_utf8(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte & 0x80 == 0x00 {
            index += 1;
        } else if byte & 0xE0 == 0xC0 {
            if index + 1 >= bytes.len() || bytes[index + 1] & 0xC0 != 0x80 {
                return false;
            }
            index += 2;
        } else if byte & 0xF0 == 0xE0 {
            if index + 2 >= bytes.len()
                || bytes[index + 1] & 0xC0 != 0x80
                || bytes[index + 2] & 0xC0 != 0x80
            {
                return false;
            }
            index += 3;
        } else if byte & 0xF8 == 0xF0 {
            if index + 3 >= bytes.len()
                || bytes[index + 1] & 0xC0 != 0x80
                || bytes[index + 2] & 0xC0 != 0x80
                || bytes[index + 3] & 0xC0 != 0x80
            {
                return false;
            }
            index += 4;
        } else {
            return false;
        }
    }
    true
}

/// switch_utils.c 中的 `unescape_char()`。
fn unescape_char(escaped: u8) -> u8 {
    match escaped {
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b's' => b' ',
        other => other,
    }
}

/// switch_utils.c 中的 `cleanup_separated_string()`：去除引号字符、
/// 解析反斜杠转义并裁剪尾部空白。
fn cleanup_separated_string(input: &[u8], delim: u8) -> Vec<u8> {
    let mut index = 0;
    while index < input.len() && input[index] == b' ' {
        index += 1;
    }

    let mut output: Vec<u8> = Vec::with_capacity(input.len() - index);
    let mut inside_quotes = false;
    // `end` 对应 C 中的 `end` 指针：输出会截断到该长度。
    let mut end = 0usize;

    while index < input.len() {
        let byte = input[index];
        let mut handled = false;

        if byte == ESCAPE_META {
            let escaped = if index + 1 < input.len() {
                input[index + 1]
            } else {
                0
            };
            let unescaped = unescape_char(escaped);
            if escaped == b'\''
                || escaped == b'"'
                || (delim != 0 && escaped == delim)
                || escaped == ESCAPE_META
                || unescaped != escaped
            {
                output.push(unescaped);
                end = output.len();
                index += 2;
                handled = true;
            }
        }

        if !handled {
            if byte == b'\'' && (inside_quotes || input[index + 1..].contains(&b'\'')) {
                inside_quotes = !inside_quotes;
                if inside_quotes {
                    end = output.len();
                }
            } else {
                output.push(byte);
                if byte != b' ' || inside_quotes {
                    end = output.len();
                }
            }
            index += 1;
        }
    }

    output.truncate(end);
    output
}

/// 空白分隔符版本的 `switch_separate_string()`，包含 `^^` 分隔符覆盖、
/// 引号分组、反斜杠转义和清理阶段。最后一个槽位保留命令未拆分的剩余
/// 部分，因此 `metadata` 可以包含空格。
pub fn separate_args(cmd: &[u8], max: usize) -> Vec<Vec<u8>> {
    let mut input = cmd;
    let mut delim = b' ';
    if input.len() >= 4 && input[0] == b'^' && input[1] == b'^' {
        delim = input[2];
        input = &input[3..];
    }

    let tokens = if delim == b' ' {
        separate_blank_delim(input, max)
    } else {
        separate_char_delim(input, delim, max)
    };
    tokens
        .iter()
        .map(|token| cleanup_separated_string(token, if delim == b' ' { 0 } else { delim }))
        .collect()
}

fn separate_blank_delim(input: &[u8], max: usize) -> Vec<&[u8]> {
    #[derive(PartialEq)]
    enum State {
        Start,
        SkipInitialSpace,
        FindDelim,
        SkipEndingSpace,
    }

    let mut tokens = Vec::new();
    let mut state = State::Start;
    let mut inside_quotes = false;
    let mut token_start = 0usize;
    let mut index = 0usize;
    let mut open_token = false;

    while index < input.len() && tokens.len() < max {
        match state {
            State::Start => {
                token_start = index;
                open_token = true;
                state = State::SkipInitialSpace;
            }
            State::SkipInitialSpace => {
                if input[index] == b' ' {
                    index += 1;
                } else {
                    state = State::FindDelim;
                }
            }
            State::FindDelim => {
                if input[index] == ESCAPE_META {
                    index += 1;
                } else if input[index] == b'\'' {
                    inside_quotes = !inside_quotes;
                } else if input[index] == b' ' && !inside_quotes {
                    tokens.push(&input[token_start..index]);
                    open_token = false;
                    state = State::SkipEndingSpace;
                }
                index += 1;
            }
            State::SkipEndingSpace => {
                if input[index] == b' ' {
                    index += 1;
                } else {
                    state = State::Start;
                }
            }
        }
    }

    if open_token {
        tokens.push(&input[token_start..]);
    }
    tokens
}

fn separate_char_delim(input: &[u8], delim: u8, max: usize) -> Vec<&[u8]> {
    #[derive(PartialEq)]
    enum State {
        Start,
        FindDelim,
    }

    let mut tokens = Vec::new();
    let mut state = State::Start;
    let mut inside_quotes = false;
    let mut token_start = 0usize;
    let mut index = 0usize;
    let mut open_token = false;

    while index < input.len() && tokens.len() < max {
        match state {
            State::Start => {
                token_start = index;
                open_token = true;
                state = State::FindDelim;
            }
            State::FindDelim => {
                if input[index] == ESCAPE_META {
                    index += 1;
                } else if input[index] == b'\''
                    && (inside_quotes || input[index + 1..].contains(&b'\''))
                {
                    inside_quotes = !inside_quotes;
                } else if input[index] == delim && !inside_quotes {
                    tokens.push(&input[token_start..index]);
                    open_token = false;
                    state = State::Start;
                }
                index += 1;
            }
        }
    }

    if open_token {
        tokens.push(&input[token_start..]);
    }
    tokens
}

/// 以携带 `body` 的 CUSTOM 事件触发 `mod_audio_relay::<suffix>`：
/// locate 会话、挂接通道事件数据、加入 JSON body 后触发。
pub fn emit_event(uuid: &[u8], subclass: &str, body: &str) {
    let session = locate_session(uuid);
    if session.is_null() {
        return;
    }

    // SAFETY: `session` 在其 locate 读锁保护下存活。
    let channel = unsafe { channel_of(session) };
    let subclass_name = cstring(subclass);
    let mut event: *mut sys::switch_event_t = ptr::null_mut();
    // SAFETY: 输出指针有效；所有字符串都是存活的 C 字符串。
    let status = unsafe {
        sys::switch_event_create_subclass_detailed(
            c"mod_audio_relay".as_ptr(),
            c"emit_event".as_ptr(),
            line!() as i32,
            &mut event,
            sys::switch_event_types_t::SWITCH_EVENT_CUSTOM,
            subclass_name.as_ptr(),
        )
    };

    if status == fswtch::SUCCESS && !event.is_null() && !channel.is_null() {
        let body_text = cstring(body);
        // SAFETY: `event` 是存活的事件，`channel` 属于 `session`。
        unsafe {
            sys::switch_channel_event_set_data(channel, event);
            sys::switch_event_add_body(event, c"%s".as_ptr(), body_text.as_ptr());
            sys::switch_event_fire_detailed(
                c"mod_audio_relay".as_ptr(),
                c"emit_event".as_ptr(),
                line!() as i32,
                &mut event,
                ptr::null_mut(),
            );
        }
    } else if !event.is_null() {
        // SAFETY: 事件已创建但无法投递；将其销毁。
        unsafe {
            sys::switch_event_destroy(&mut event);
        }
    }

    // SAFETY: `session` 来自 `locate_session`，仍持有其锁。
    unsafe { rwunlock(session) };
}

/// 经由所属 session 移除媒体 bug（`switch_core_media_bug_remove`）。
///
/// 这里刻意不用 `switch_core_media_bug_close(&bug, SWITCH_FALSE)`：它会在
/// bug 仍注册时触发 CLOSE 回调，而回调内的清理会再次移除该 bug，导致
/// CLOSE 回调被触发第二次。fswtch trampoline 每次 CLOSE 都会回收装箱的
/// 处理器状态，第二次回调即构成 double-free。`remove` 会先解除挂载，
/// 因此 CLOSE 回调里自己的 `switch_core_media_bug_remove` 找不到目标，
/// CLOSE 只触发一次。
pub fn close_bug(bug: *mut sys::switch_media_bug_t) {
    let mut raw = bug;
    // SAFETY: `raw` 是由 FreeSWITCH 持有的存活 bug 指针，直到 close 回调
    // 执行；调用方只传入 bug 存活期间保存的句柄。`get_session` 返回
    // 持有该 bug 的 session。
    let session = unsafe { sys::switch_core_media_bug_get_session(raw) };
    if session.is_null() {
        // SAFETY: 契约同上；不带销毁的 close 仍会投递 CLOSE 回调。
        unsafe {
            sys::switch_core_media_bug_close(&mut raw, sys::switch_bool_t_SWITCH_FALSE);
        }
        return;
    }
    // SAFETY: `session` 是持有 `raw` 的存活 session；`raw` 是 remove
    // 调用的可写输出存储。
    unsafe {
        sys::switch_core_media_bug_remove(session, &mut raw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_validation_accepts_ascii_and_multibyte() {
        assert!(is_valid_utf8(b"hello"));
        assert!(is_valid_utf8("你好，世界".as_bytes()));
        assert!(is_valid_utf8(&[]));
    }

    #[test]
    fn utf8_validation_rejects_truncated_and_bad_lead() {
        assert!(!is_valid_utf8(&[0xE4, 0xBD])); // 截断的三字节序列
        assert!(!is_valid_utf8(&[0xFF]));
        assert!(!is_valid_utf8(&[0xE4, 0x28, 0xA0])); // 第二字节非续字节
        assert!(!is_valid_utf8(&[0x80])); // 孤立续字节
    }

    #[test]
    fn utf8_validation_accepts_overlong_forms() {
        // 本实现只做字节形状检查，超长形式与代理项同样放行。
        assert!(is_valid_utf8(&[0xC0, 0xAF]));
    }

    #[test]
    fn separate_args_splits_on_blanks() {
        let args = separate_args(b"uuid_audio_relay abc-123 start ws://h:1 mono", 8);
        let texts: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
        assert_eq!(
            texts,
            vec![
                &b"uuid_audio_relay"[..],
                b"abc-123",
                b"start",
                b"ws://h:1",
                b"mono"
            ]
        );
    }

    #[test]
    fn separate_args_keeps_quoted_metadata_intact() {
        // metadata 以引号包裹时空格不拆分，整体保留在最后一个槽位。
        let args = separate_args(b"uuid start ws://h mono '{\"a\": 1}'", 6);
        assert_eq!(args.len(), 5);
        assert_eq!(args[4], b"{\"a\": 1}");
    }

    #[test]
    fn separate_args_supports_custom_delimiter() {
        // ^^ 分隔符覆盖：^^,<delim> 开头时按 <delim> 拆分。
        let args = separate_args(b"^^|one|two|three", 4);
        assert_eq!(args[0], b"one");
        assert_eq!(args[1], b"two");
        assert_eq!(args[2], b"three");
    }

    #[test]
    fn separate_args_groups_quoted_sections() {
        let args = separate_args(b"cmd start 'ws://h/a b' mono", 5);
        assert_eq!(args[2], b"ws://h/a b");
    }

    #[test]
    fn separate_args_unescapes_backslash_sequences() {
        let args = separate_args(b"cmd a\\tb c", 4);
        assert_eq!(args[1], b"a\tb");
    }

    #[test]
    fn cstring_round_trip() {
        let c = cstring("audio_relay");
        assert_eq!(c.to_str().unwrap(), "audio_relay");
    }
}
