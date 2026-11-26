//! `audio_relay` media bug：把上行 PCM 采集送入 WebSocket，并把排队
//! 的播放音频混入写替换帧。
//!
//! 本模块刻意不使用 `fswtch::attach_media_bug`：fswtch 的
//! `MediaBugDispatch` 只在 `switch_core_media_bug_get_native_*_frame`
//! 返回非空帧时才回调 `on_read`/`on_write`，但 FreeSWITCH 只为
//! `SMBF_TAP_NATIVE_*` bug 设置 `native_*_frame`。对于本模块所需的
//! `SMBF_READ_STREAM`/`SMBF_WRITE_STREAM` 标志，回调永远不会触发。
//! 下方 trampoline 复刻 fswtch 的所有权契约（装箱状态由唯一一次
//! CLOSE 回调回收），同时分发 media bug 的全部回调类型。

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use audio_relay_core::streamer::CapturePipeline;
use audio_relay_core::{RECOMMENDED_BUFFER_SIZE, Streamer};
use fswtch::sys;

use crate::shared::SessionShared;
use crate::sysutil;

/// 单个已挂接 bug 在媒体线程上所需的全部状态。装箱状态由 CLOSE 回调
/// 回收，每个 FreeSWITCH media bug 生命周期恰好一次。
pub struct AudioBugHandler {
    shared: Arc<SessionShared>,
    streamer: Arc<Streamer>,
    pipeline: CapturePipeline,
}

impl AudioBugHandler {
    pub fn new(
        shared: Arc<SessionShared>,
        streamer: Arc<Streamer>,
        pipeline: CapturePipeline,
    ) -> Self {
        Self {
            shared,
            streamer,
            pipeline,
        }
    }
}

struct BugState {
    handler: AudioBugHandler,
}

/// 挂接 `audio_relay` media bug：
/// `switch_core_media_bug_add(session, name, NULL, callback,
/// pUserData, 0, flags, &bug)`。
///
/// 成功后返回的 bug 指针必须存入会话上下文；装箱 `BugState` 由
/// FreeSWITCH 持有直至 CLOSE 回调。
pub fn attach(
    session: *mut sys::switch_core_session_t,
    flags: u32,
    handler: AudioBugHandler,
) -> Result<*mut sys::switch_media_bug_t, sys::switch_status_t> {
    let state = Box::into_raw(Box::new(BugState { handler }));
    let function = sysutil::cstring(sysutil::BUG_NAME);
    let mut bug = std::ptr::null_mut();
    // SAFETY: `session` 是已定位的存活会话；`state` 保持分配直至
    // CLOSE 回调（失败时在此处回收）；`bug` 是可写的输出存储。
    let status = unsafe {
        sys::switch_core_media_bug_add(
            session,
            function.as_ptr(),
            std::ptr::null(),
            Some(media_bug_trampoline),
            state.cast(),
            0,
            flags,
            &mut bug,
        )
    };
    if status != sys::switch_status_t::SWITCH_STATUS_SUCCESS || bug.is_null() {
        // SAFETY: 挂接失败时 FreeSWITCH 未接管所有权。
        unsafe { drop(Box::from_raw(state)) };
        return Err(status);
    }
    Ok(bug)
}

/// FreeSWITCH media bug 回调。分发给 `AudioBugHandler`；CLOSE 为终止性
/// 回调并回收装箱状态，与 fswtch trampoline 的所有权契约一致。
///
/// # Safety
///
/// `user_data` 必须是传给 `switch_core_media_bug_add` 的装箱
/// `BugState` 指针；FreeSWITCH 对其至多触发一次 CLOSE。
unsafe extern "C" fn media_bug_trampoline(
    bug: *mut sys::switch_media_bug_t,
    user_data: *mut c_void,
    callback_type: sys::switch_abc_type_t,
) -> sys::switch_bool_t {
    if user_data.is_null() {
        return sys::switch_bool_t_SWITCH_TRUE;
    }

    if callback_type == sys::switch_abc_type_t_SWITCH_ABC_TYPE_CLOSE {
        // SAFETY: CLOSE 是该指针的终止回调。
        let mut state = unsafe { Box::from_raw(user_data.cast::<BugState>()) };
        let _ = catch_unwind(AssertUnwindSafe(|| state.handler.on_close(bug)));
        return sys::switch_bool_t_SWITCH_TRUE;
    }

    // SAFETY: `user_data` 是本回调对应的存活装箱 `BugState`。
    let state = unsafe { &mut *user_data.cast::<BugState>() };
    let result = catch_unwind(AssertUnwindSafe(|| dispatch(state, bug, callback_type)));
    match result {
        Ok(()) => sys::switch_bool_t_SWITCH_TRUE,
        Err(_) => {
            eprintln!("[mod_audio_relay] media bug callback panicked");
            sys::switch_bool_t_SWITCH_FALSE
        }
    }
}

fn dispatch(state: &mut BugState, bug: *mut sys::switch_media_bug_t, callback_type: sys::switch_abc_type_t) {
    match callback_type {
        sys::switch_abc_type_t_SWITCH_ABC_TYPE_INIT => state.handler.on_init(),
        sys::switch_abc_type_t_SWITCH_ABC_TYPE_READ => state.handler.on_read(bug),
        // `SMBF_WRITE_STREAM` 帧由 FreeSWITCH 自身缓冲进
        // `raw_write_buffer`；C 回调同样没有 WRITE 分支。
        sys::switch_abc_type_t_SWITCH_ABC_TYPE_WRITE_REPLACE => {
            state.handler.on_write_replace(bug)
        }
        _ => {}
    }
}

impl AudioBugHandler {
    fn on_init(&mut self) {}

    /// 读帧处理：把排队的读帧逐个经 `switch_core_media_bug_read` 排空，
    /// 送入采集管线。
    fn on_read(&mut self, bug: *mut sys::switch_media_bug_t) {
        // audio_paused/cleanup_started 时提前返回。
        if self.streamer.capture_blocked() {
            return;
        }

        let mut buffer = [0u8; RECOMMENDED_BUFFER_SIZE];
        // SAFETY: `switch_frame_t` 是纯 C 结构体；清零等价于在填充
        // 字段前执行 memset。
        let mut frame: sys::switch_frame_t = unsafe { std::mem::zeroed() };
        frame.data = buffer.as_mut_ptr().cast();
        frame.buflen = buffer.len() as u32;

        loop {
            // SAFETY: `bug` 在回调期间存活；`frame` 是调用方提供的可写帧
            // 存储；`fill` 对应 C 排空循环中的 `SWITCH_TRUE` 实参。
            let status = unsafe {
                sys::switch_core_media_bug_read(
                    bug,
                    &mut frame,
                    sys::switch_bool_t_SWITCH_TRUE,
                )
            };
            if status != sys::switch_status_t::SWITCH_STATUS_SUCCESS {
                break;
            }
            if frame.datalen == 0 {
                continue;
            }
            // SAFETY: `frame.data` 指向 `buffer`，FreeSWITCH 在回调期间
            // 保证 `datalen` 字节有效。
            let pcm = unsafe {
                std::slice::from_raw_parts(frame.data.cast::<u8>(), frame.datalen as usize)
            };
            for message in self.pipeline.push(pcm, frame.samples) {
                self.streamer.write_binary(&message);
            }
        }
    }

    /// 写替换帧处理。
    fn on_write_replace(&mut self, bug: *mut sys::switch_media_bug_t) {
        // audio_paused / cleanup_started / playback_paused / cleaned_up
        // 均由 `Streamer::write_replace` 内部门禁，这里提前返回。
        //
        // SAFETY: `bug` 是本回调存活的 media bug。
        let frame_ptr = unsafe { sys::switch_core_media_bug_get_write_replace_frame(bug) };
        if frame_ptr.is_null() {
            return;
        }
        // SAFETY: FreeSWITCH 返回本回调存活的写帧；`data`/`datalen`
        // 描述 `datalen` 字节可写区域。
        let frame = unsafe { &mut *frame_ptr };
        if frame.data.is_null() || frame.datalen < 2 {
            return;
        }
        let datalen = frame.datalen as usize;
        let channels = frame.channels.max(1) as usize;
        // SAFETY: `frame.data` 在回调期间按 media bug 契约可写
        // `datalen` 字节。
        let pcm = unsafe {
            std::slice::from_raw_parts_mut(frame.data.cast::<u8>(), datalen)
        };
        if self.streamer.write_replace(pcm, datalen, channels) {
            // SAFETY: 两个指针在本回调期间均存活。
            unsafe {
                sys::switch_core_media_bug_set_write_replace_frame(bug, frame_ptr);
            }
        }
    }

    /// CLOSE 回调：记录日志、区分通道挂断与
    /// 主动请求关闭、回收通道 private 状态并执行会话清理。
    fn on_close(&mut self, bug: *mut sys::switch_media_bug_t) {
        sysutil::log_session_uuid(
            &sysutil::cstring(self.shared.uuid()),
            sys::switch_log_level_t_SWITCH_LOG_INFO,
            "Got SWITCH_ABC_TYPE_CLOSE.\n",
        );

        // `close_requested ? 0 : 1`：主动请求关闭与通道挂断的区分。
        let channel_is_closing = !self.shared.close_requested();

        // SAFETY: `bug` 在 CLOSE 回调期间存活，且仍指向其所属会话。
        let session = unsafe { sys::switch_core_media_bug_get_session(bug) };

        if channel_is_closing && !session.is_null() {
            // 通道 private 槽位随通道销毁；回收装箱弱指针，避免重复
            // 调用累积条目。
            // SAFETY: `session` 在 bug CLOSE 回调期间存活。
            let channel = unsafe { sysutil::channel_of(session) };
            if !channel.is_null() {
                SessionShared::uninstall(channel);
            }
        }

        self.shared
            .cleanup(session, None, channel_is_closing);
    }
}
