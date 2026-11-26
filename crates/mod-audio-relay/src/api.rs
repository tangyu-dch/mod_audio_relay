//! `uuid_audio_relay` API 命令：参数拆分、会话定位/解锁、动作分发
//! 以及 `+OK`/`-ERR`/`-USAGE` 响应。

use std::sync::Arc;
use std::time::Duration;

use audio_relay_core::streamer::{CapturePipeline, StreamerOptions};
use audio_relay_core::transport::LogLevel;
use audio_relay_core::url::{self, WebSocketUrl};
use audio_relay_core::{PlaybackAction, ResampleEngine, StreamConfig, Streamer};
use fswtch::sys;
use fswtch::{ApiStream, Session, Status};

use crate::bug::{self, AudioBugHandler};
use crate::decoder::FsDecoder;
use crate::shared::{FsHooks, SessionShared};
use crate::speex::SpeexEngine;
use crate::sysutil;

/// WebSocket 连接/升级超时；C 客户端异步连接且无显式超时，因此该值
/// 只约束握手阶段。
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// 挂接采集时总是传入的标志集合中包含 `SMBF_FIRST`。
const SMBF_FIRST: u32 = sys::switch_media_bug_flag_enum_t_SMBF_FIRST;

/// 按混音类型返回 media bug 标志；非法值返回 `None`。
fn flags_for_mix(mix: &[u8]) -> Option<u32> {
    let base =
        sys::switch_media_bug_flag_enum_t_SMBF_READ_STREAM
            | sys::switch_media_bug_flag_enum_t_SMBF_WRITE_REPLACE
            | SMBF_FIRST;
    if mix.eq_ignore_ascii_case(b"mono") {
        Some(base)
    } else if mix.eq_ignore_ascii_case(b"mixed") {
        Some(base | sys::switch_media_bug_flag_enum_t_SMBF_WRITE_STREAM)
    } else if mix.eq_ignore_ascii_case(b"stereo") {
        Some(
            base | sys::switch_media_bug_flag_enum_t_SMBF_WRITE_STREAM
                | sys::switch_media_bug_flag_enum_t_SMBF_STEREO,
        )
    } else {
        None
    }
}

/// 提取第 `index` 个参数为 UTF-8 字符串；缺失或非法 UTF-8 返回 `None`。
fn arg_str(args: &[Vec<u8>], index: usize) -> Option<&str> {
    std::str::from_utf8(args.get(index)?).ok()
}

/// 记录 API 命令日志；有会话时走会话日志，否则走全局日志。
fn log_api(session: Option<Session>, level: sys::switch_log_level_t, message: &str) {
    match session {
        Some(session) => {
            // SAFETY: `session` 是存活的 API 回调会话。
            unsafe { sysutil::log_session_ptr(session.as_ptr(), level, message) };
        }
        None => sysutil::log(level, message),
    }
}

/// `stop`/`send_text`/`start` metadata 共用的 `is_valid_utf8` 门禁。
fn utf8_or_log(session: Option<Session>, raw: &[u8]) -> bool {
    if sysutil::is_valid_utf8(raw) {
        return true;
    }
    log_api(
        session,
        sys::switch_log_level_t_SWITCH_LOG_ERROR,
        &format!(
            "{} contains invalid utf8 characters",
            String::from_utf8_lossy(raw)
        ),
    );
    false
}

fn map_log_level(level: LogLevel) -> sys::switch_log_level_t {
    match level {
        LogLevel::Debug => sys::switch_log_level_t_SWITCH_LOG_DEBUG,
        LogLevel::Info => sys::switch_log_level_t_SWITCH_LOG_INFO,
        LogLevel::Warning => sys::switch_log_level_t_SWITCH_LOG_WARNING,
        LogLevel::Error => sys::switch_log_level_t_SWITCH_LOG_ERROR,
    }
}

/// 启动音频采集（连接 WebSocket 并挂接 media bug）。
fn start_capture(
    session: *mut sys::switch_core_session_t,
    ws_uri: &str,
    flags: u32,
    sampling: u32,
    channels: u32,
    metadata: Option<&str>,
) -> Status {
    // SAFETY: `session` 是已定位会话；调用方在整个命令期间持有其读锁。
    let channel = unsafe { sysutil::channel_of(session) };
    if channel.is_null() {
        return fswtch::FALSE;
    }

    // `switch_channel_pre_answer(channel)`。
    // SAFETY: `channel` 属于存活的已定位会话。
    let answered = unsafe {
        sys::switch_channel_perform_pre_answer(
            channel,
            c"mod_audio_relay".as_ptr(),
            c"start_capture".as_ptr(),
            line!() as i32,
        )
    };
    if answered != fswtch::SUCCESS {
        sysutil::log(
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            "mod_audio_relay: channel must have reached pre-answer status before calling start!",
        );
        return fswtch::FALSE;
    }

    // SAFETY: codec 指针仅在采样率查询期间只读借用。
    let (read_rate, write_rate) = unsafe {
        let read_codec = sys::switch_core_session_get_read_codec(session);
        let write_codec = sys::switch_core_session_get_write_codec(session);
        if read_codec.is_null()
            || write_codec.is_null()
            || (*read_codec).implementation.is_null()
            || (*write_codec).implementation.is_null()
        {
            sysutil::log(
                sys::switch_log_level_t_SWITCH_LOG_ERROR,
                "mod_audio_relay: read/write codec is unavailable",
            );
            return fswtch::FALSE;
        }
        (
            (*(*read_codec).implementation).actual_samples_per_second,
            (*(*write_codec).implementation).actual_samples_per_second,
        )
    };

    // SAFETY: `lsession` 在读锁保护下存活。
    let uuid = unsafe { sysutil::session_uuid(session) }.unwrap_or_default();
    let Some(shared) = SessionShared::install(channel, &uuid) else {
        return fswtch::FALSE;
    };

    if !shared.begin_start() {
        let state = shared.state_name();
        sysutil::log_session_uuid(
            &sysutil::cstring(shared.uuid()),
            sys::switch_log_level_t_SWITCH_LOG_WARNING,
            &format!("mod_audio_relay: cannot start while stream state is {state}"),
        );
        return fswtch::FALSE;
    }

    // SAFETY: 已定位会话持有读锁期间 `channel` 存活。
    let variables = unsafe { sysutil::stream_variables(channel) };
    let config =
        match StreamConfig::from_variables(&variables, read_rate, write_rate, sampling, channels)
        {
            Ok(config) => config,
            Err(error) => {
                sysutil::log_session_uuid(
                    &sysutil::cstring(shared.uuid()),
                    sys::switch_log_level_t_SWITCH_LOG_ERROR,
                    &error.to_string(),
                );
                shared.fail_start();
                return fswtch::FALSE;
            }
        };

    let url = match WebSocketUrl::parse(ws_uri) {
        Ok(url) => url,
        Err(error) => {
            sysutil::log_session_uuid(
                &sysutil::cstring(shared.uuid()),
                sys::switch_log_level_t_SWITCH_LOG_ERROR,
                &format!("invalid websocket uri: {error}"),
            );
            shared.fail_start();
            return fswtch::FALSE;
        }
    };

    let Some(temp_dir) = sysutil::temp_dir() else {
        sysutil::log_session_uuid(
            &sysutil::cstring(shared.uuid()),
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            "mod_audio_relay: FreeSWITCH temp dir is unavailable",
        );
        shared.fail_start();
        return fswtch::FALSE;
    };

    let uuid = shared.uuid().to_owned();
    let log_uuid = uuid.clone();
    let hooks = Arc::new(FsHooks::new(Arc::clone(&shared)));
    let engines = || -> Box<dyn ResampleEngine> { Box::new(SpeexEngine::new()) };
    let options = StreamerOptions {
        uuid: uuid.clone(),
        temp_dir,
        config: config.clone(),
        url,
        initial_metadata: metadata.map(str::to_owned),
        hooks,
        decoder: Arc::new(FsDecoder::new()),
        engines: Arc::new(engines),
        log: Some(Arc::new(move |level, message| {
            sysutil::log(map_log_level(level), &format!("({log_uuid}) {message}"));
        })),
        connection_timeout: CONNECTION_TIMEOUT,
    };

    let Some(streamer) = Streamer::start(options) else {
        sysutil::log_session_uuid(
            &sysutil::cstring(&uuid),
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            &format!("({uuid}) invalid STREAM_AUDIO_PIPE configuration"),
        );
        shared.fail_start();
        return fswtch::FALSE;
    };

    // 仅当线速率与 codec 速率不同时才创建采集重采样器。
    let engine = if config.read_rate != config.sampling {
        Some(Box::new(SpeexEngine::new()) as Box<dyn ResampleEngine>)
    } else {
        None
    };
    let pipeline = CapturePipeline::new(&config, engine);
    let handler = AudioBugHandler::new(Arc::clone(&shared), Arc::clone(&streamer), pipeline);

    // SAFETY: `session` 是存活的已定位会话；装箱 handler 状态由
    // FreeSWITCH 持有至 bug CLOSE 回调。
    let bug = bug::attach(session, flags, handler);
    let bug = match bug {
        Ok(bug) => bug,
        Err(_) => {
            discard_streamer(streamer);
            shared.fail_start();
            return fswtch::GENERR;
        }
    };

    if shared.activate(bug, streamer) {
        // 挂接 bug 期间传输层已报告启动失败；复刻 C 路径：请求关闭并
        // 使启动失败。
        sysutil::log_session_uuid(
            &sysutil::cstring(&uuid),
            sys::switch_log_level_t_SWITCH_LOG_WARNING,
            "WebSocket connection failed during stream startup.",
        );
        shared.request_close();
        return fswtch::FALSE;
    }

    fswtch::SUCCESS
}

/// 丢弃未挂接 bug 的 streamer。
fn discard_streamer(streamer: Arc<Streamer>) {
    streamer.mark_cleaned_up();
    streamer.cleanup(None);
}

/// `do_stop`：可选播放排空、可选告别文本、执行清理。
fn do_stop(
    session: *mut sys::switch_core_session_t,
    shared: &SessionShared,
    text: Option<&str>,
    drain: bool,
) -> Status {
    if drain {
        shared.drain_playback();
    }
    match text {
        Some(text) => sysutil::log_session_uuid(
            &sysutil::cstring(shared.uuid()),
            sys::switch_log_level_t_SWITCH_LOG_INFO,
            &format!("mod_audio_relay: stop w/ final text {text}"),
        ),
        None => sysutil::log_session_uuid(
            &sysutil::cstring(shared.uuid()),
            sys::switch_log_level_t_SWITCH_LOG_INFO,
            "mod_audio_relay: stop",
        ),
    }
    if shared.cleanup(session, text, false) {
        fswtch::SUCCESS
    } else {
        fswtch::FALSE
    }
}

fswtch::api_callback!(fn uuid_audio_relay_impl(cmd, session, stream) {
    // 捕获 panic，避免单个命令的 Rust panic 破坏 FreeSWITCH 进程。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(cmd, session, stream)
    }));
    match result {
        Ok(status) => status,
        Err(_) => {
            fswtch::log_error(sysutil::MODULE_NAME, "uuid_audio_relay panicked");
            fswtch::SUCCESS
        }
    }
});

/// 注册到 `module.api` 的入口；`api_callback!` 宏只生成私有函数，
/// 因此用这个薄包装把它暴露给模块加载器。
///
/// # Safety
///
/// FreeSWITCH 以存活的命令字符串、会话与流调用本函数；其对 ABI 实参
/// 的契约与生成回调一致。
pub unsafe extern "C" fn uuid_audio_relay(
    cmd: *const std::ffi::c_char,
    session: *mut sys::switch_core_session_t,
    stream: *mut sys::switch_stream_handle_t,
) -> Status {
    // SAFETY: FreeSWITCH ABI 实参原样转发。
    unsafe { uuid_audio_relay_impl(cmd, session, stream) }
}

/// `uuid_audio_relay` 的分发主函数。
fn dispatch(cmd: Option<String>, session: Option<Session>, stream: ApiStream) -> Status {
    let args = cmd
        .as_deref()
        .map(|text| sysutil::separate_args(text.as_bytes(), 6))
        .unwrap_or_default();

    let action = arg_str(&args, 1).unwrap_or("");
    let is_start = action.eq_ignore_ascii_case("start");

    if args.len() < 2 || (is_start && args.len() < 4) {
        log_api(
            session,
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            &format!(
                "Error with command {} {} {}.",
                cmd.as_deref().unwrap_or(""),
                arg_str(&args, 0).unwrap_or(""),
                action
            ),
        );
        let _ = stream.write(&format!("-USAGE: {}\n", sysutil::API_SYNTAX));
        return fswtch::SUCCESS;
    }

    let mut status = fswtch::FALSE;
    let mut response_written = false;

    // SAFETY: `uuid` 在调用期间是存活的字节 token；非 NULL 结果携带
    // 读锁，由下方 `rwunlock` 释放。
    let lsession = sysutil::locate_session(args[0].as_slice());
    if lsession.is_null() {
        log_api(
            session,
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            &format!("Error locating session {}", String::from_utf8_lossy(&args[0])),
        );
        return finish(stream, status, response_written);
    }

    if action.eq_ignore_ascii_case("stop") || action.eq_ignore_ascii_case("stop_drain") {
        let mut text = None;
        if args.len() > 2 {
            if !utf8_or_log(session, &args[2]) {
                // SAFETY: `lsession` 持有读锁。
                unsafe { sysutil::rwunlock(lsession) };
                return finish(stream, status, response_written);
            }
            text = arg_str(&args, 2).map(str::to_owned);
        }
        if let Some(shared) = located_shared(lsession) {
            status = do_stop(
                lsession,
                &shared,
                text.as_deref(),
                action.eq_ignore_ascii_case("stop_drain"),
            );
        }
    } else if action.eq_ignore_ascii_case("pause") || action.eq_ignore_ascii_case("resume") {
        let pause = action.eq_ignore_ascii_case("pause");
        log_api(
            session,
            sys::switch_log_level_t_SWITCH_LOG_INFO,
            &format!("mod_audio_relay: {}", if pause { "pause" } else { "resume" }),
        );
        if let Some(shared) = located_shared(lsession) {
            status = if shared.pause_resume(pause) {
                fswtch::SUCCESS
            } else {
                fswtch::FALSE
            };
        }
    } else if action.eq_ignore_ascii_case("interrupt") {
        if let Some(shared) = located_shared(lsession) {
            status = if shared.interrupt() {
                fswtch::SUCCESS
            } else {
                fswtch::FALSE
            };
        }
    } else if action.eq_ignore_ascii_case("pause_playback") {
        if let Some(shared) = located_shared(lsession) {
            status = if shared.playback_control(PlaybackAction::Pause) {
                fswtch::SUCCESS
            } else {
                fswtch::FALSE
            };
        }
    } else if action.eq_ignore_ascii_case("resume_playback") {
        if let Some(shared) = located_shared(lsession) {
            status = if shared.playback_control(PlaybackAction::Resume) {
                fswtch::SUCCESS
            } else {
                fswtch::FALSE
            };
        }
    } else if action.eq_ignore_ascii_case("stop_playback") {
        if let Some(shared) = located_shared(lsession) {
            status = if shared.playback_control(PlaybackAction::Stop) {
                fswtch::SUCCESS
            } else {
                fswtch::FALSE
            };
        }
    } else if action.eq_ignore_ascii_case("status") {
        let (state, paused, queued, rate) = located_shared(lsession)
            .map(|shared| {
                let status = shared.playback_status();
                (
                    status.state.to_owned(),
                    status.paused,
                    status.queued_bytes,
                    status.sample_rate,
                )
            })
            .unwrap_or_else(|| ("idle".to_owned(), false, 0, 0));
        let _ = stream.write(&format!(
            "+OK {{\"state\":\"{state}\",\"playbackPaused\":{},\"playbackQueuedBytes\":{queued},\"playbackSampleRate\":{rate}}}\n",
            if paused { "true" } else { "false" }
        ));
        response_written = true;
        status = fswtch::SUCCESS;
    } else if action.eq_ignore_ascii_case("send_text") {
        if args.len() < 3 {
            log_api(
                session,
                sys::switch_log_level_t_SWITCH_LOG_ERROR,
                "send_text requires an argument specifying text to send",
            );
            // SAFETY: `lsession` 持有读锁。
            unsafe { sysutil::rwunlock(lsession) };
            return finish(stream, status, response_written);
        }
        if !utf8_or_log(session, &args[2]) {
            // SAFETY: `lsession` 持有读锁。
            unsafe { sysutil::rwunlock(lsession) };
            return finish(stream, status, response_written);
        }
        if let (Some(shared), Some(text)) = (located_shared(lsession), arg_str(&args, 2)) {
            status = if shared.send_text(text) {
                fswtch::SUCCESS
            } else {
                fswtch::FALSE
            };
        }
    } else if is_start {
        status = start_command(lsession, session, &args);
    } else {
        log_api(
            session,
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            &format!("unsupported mod_audio_relay cmd: {action}"),
        );
    }

    // SAFETY: `lsession` 来自 `locate_session`，仍持有读锁。
    unsafe { sysutil::rwunlock(lsession) };
    finish(stream, status, response_written)
}

/// 对已定位会话做通道 private 上下文查找。
fn located_shared(lsession: *mut sys::switch_core_session_t) -> Option<Arc<SessionShared>> {
    // SAFETY: `lsession` 在读锁保护下存活。
    let channel = unsafe { sysutil::channel_of(lsession) };
    if channel.is_null() {
        return None;
    }
    SessionShared::get(channel)
}

/// `start` 分支，含校验顺序：metadata UTF-8、
/// 混音类型、采样率、URI，最后才进入 `start_capture`。
fn start_command(
    lsession: *mut sys::switch_core_session_t,
    session: Option<Session>,
    args: &[Vec<u8>],
) -> Status {
    let (Some(ws_uri), Some(mix)) = (arg_str(args, 2), arg_str(args, 3)) else {
        return fswtch::FALSE;
    };

    let metadata = if args.len() > 5 {
        if !utf8_or_log(session, &args[5]) {
            return fswtch::FALSE;
        }
        arg_str(args, 5)
    } else {
        None
    };

    let Some(flags) = flags_for_mix(args[3].as_slice()) else {
        log_api(
            session,
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            &format!(
                "invalid mix type: {}, must be mono, mixed, or stereo",
                String::from_utf8_lossy(&args[3])
            ),
        );
        return fswtch::FALSE;
    };
    let _ = mix;

    let mut sampling = 8000u32;
    if args.len() > 4 {
        let Some(rate_text) = arg_str(args, 4) else {
            return fswtch::FALSE;
        };
        sampling = url::parse_sampling(rate_text).unwrap_or(0);
    }

    if !url::validate_ws_uri(ws_uri) {
        log_api(
            session,
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            &format!("invalid websocket uri: {ws_uri}"),
        );
        return fswtch::FALSE;
    }
    if !url::validate_sampling(sampling) {
        log_api(
            session,
            sys::switch_log_level_t_SWITCH_LOG_ERROR,
            &format!(
                "invalid sample rate: {} (valid range is 8000-64000 in 8000 Hz steps)",
                if args.len() > 4 {
                    arg_str(args, 4).unwrap_or("")
                } else {
                    "8000"
                }
            ),
        );
        return fswtch::FALSE;
    }

    let channels = if flags & sys::switch_media_bug_flag_enum_t_SMBF_STEREO != 0 {
        2
    } else {
        1
    };

    start_capture(lsession, ws_uri, flags, sampling, channels, metadata)
}

/// 最终响应：`status` 已写自定义响应时不再重复写 `+OK`/`-ERR`。
fn finish(stream: ApiStream, status: Status, response_written: bool) -> Status {
    if !response_written {
        let _ = stream.write(if status == fswtch::SUCCESS {
            "+OK Success\n"
        } else {
            "-ERR Operation Failed\n"
        });
    }
    fswtch::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_flags_cover_all_modes() {
        let mono = flags_for_mix(b"mono").unwrap();
        let mixed = flags_for_mix(b"MIXED").unwrap();
        let stereo = flags_for_mix(b"stereo").unwrap();
        assert_ne!(mono, mixed);
        assert_ne!(mixed, stereo);
        // mixed/stereo 在 mono 基础上叠加 WRITE_STREAM；stereo 再叠加 STEREO。
        assert!(mixed & sys::switch_media_bug_flag_enum_t_SMBF_WRITE_STREAM != 0);
        assert!(stereo & sys::switch_media_bug_flag_enum_t_SMBF_WRITE_STREAM != 0);
        assert!(stereo & sys::switch_media_bug_flag_enum_t_SMBF_STEREO != 0);
        assert!(mono & sys::switch_media_bug_flag_enum_t_SMBF_WRITE_STREAM == 0);
    }

    #[test]
    fn mix_flags_reject_unknown() {
        assert!(flags_for_mix(b"").is_none());
        assert!(flags_for_mix(b"surround").is_none());
    }

    #[test]
    fn arg_str_extracts_utf8_args() {
        let args = vec![b"start".to_vec(), b"ws://h".to_vec(), vec![0xFF, 0xFE]];
        assert_eq!(arg_str(&args, 0), Some("start"));
        assert_eq!(arg_str(&args, 1), Some("ws://h"));
        assert_eq!(arg_str(&args, 2), None); // 非法 UTF-8
        assert_eq!(arg_str(&args, 3), None); // 越界
    }
}
