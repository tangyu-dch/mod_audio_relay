//! 每会话音频中继上下文：通道 private 槽位与 media bug 用户数据。
//!
//! 通道 private 槽位 `audio_relay_context` 持有一个装箱的
//! `Weak<SessionShared>`：media bug 处理器与在途 API 调用各自持有强
//! `Arc`，因此注册表条目永远不会悬空。weak 指针失效只表示“该通道无音频
//! 流”，API 据此报告 `idle`/失败。

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use audio_relay_core::transport::LogLevel;
use audio_relay_core::{PlaybackAction, Streamer};
use audio_relay_core::streamer::{EventKind, StreamHooks};
use fswtch::sys;

use crate::sysutil;

/// 音频中继状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxState {
    Idle,
    Starting,
    Active,
    Paused,
    Stopping,
}

impl CtxState {
    /// “cannot start” 警告使用的全大写状态名。
    pub fn log_name(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Starting => "STARTING",
            Self::Active => "ACTIVE",
            Self::Paused => "PAUSED",
            Self::Stopping => "STOPPING",
        }
    }

    /// `status` API 响应使用的小写状态名。
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
        }
    }
}

/// 播放状态查询的返回值。
#[derive(Debug, Clone, Copy)]
pub struct PlaybackStatus {
    pub state: &'static str,
    pub paused: bool,
    pub queued_bytes: usize,
    pub sample_rate: u32,
}

/// 已挂接的 media bug，以裸指针持有。指针存为 usize，仅用于传给
/// FreeSWITCH 自身串行化的 API 调用（`flush`/`remove`/`close`）。
/// 句柄在 bug 的 CLOSE 回调之前始终有效，而该回调必定先于本上下文被
/// 释放而运行。
#[derive(Debug, Copy, Clone)]
struct BugHandle(usize);

// SAFETY: 见结构体注释——指针只交给 FreeSWITCH 使用。
unsafe impl Send for BugHandle {}
// SAFETY: 见结构体注释——指针只交给 FreeSWITCH 使用。
unsafe impl Sync for BugHandle {}

impl BugHandle {
    fn new(bug: *mut sys::switch_media_bug_t) -> Self {
        Self(bug as usize)
    }

    fn as_ptr(self) -> *mut sys::switch_media_bug_t {
        self.0 as *mut sys::switch_media_bug_t
    }
}

struct Inner {
    state: CtxState,
    bug: Option<BugHandle>,
    startup_failed: bool,
    streamer: Option<Arc<Streamer>>,
}

/// 每个通道一个实例的共享状态。同一 `start` 只创建一个：media bug 处理器
/// 与通道 private 弱指针指向同一对象。
pub struct SessionShared {
    uuid: String,
    inner: Mutex<Inner>,
    /// 对应 `tech_pvt->close_requested`：音频流自身请求关闭 bug 时置位，
    /// 使 CLOSE 回调能够区分“通道挂断”与“主动请求关闭”。
    close_requested: AtomicBool,
    /// 对应 `tech_pvt->cleanup_started`：使 API 线程、传输线程与媒体
    /// 线程三方并发的清理路径幂等。
    cleanup_started: AtomicBool,
}

impl SessionShared {
    pub fn new(uuid: String) -> Self {
        Self {
            uuid,
            inner: Mutex::new(Inner {
                state: CtxState::Idle,
                bug: None,
                startup_failed: false,
                streamer: None,
            }),
            close_requested: AtomicBool::new(false),
            cleanup_started: AtomicBool::new(false),
        }
    }

    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    /// 供诊断使用的当前上下文状态。
    pub fn state_name(&self) -> &'static str {
        self.inner().state.log_name()
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("session context lock")
    }

    // ------------------------------------------------------------------
    // 通道 private 注册表
    // ------------------------------------------------------------------

    /// 返回 `channel` 的存活上下文；槽位为空或 weak 指针已失效时创建
    /// 并安装一个新的。
    pub fn install(channel: *mut sys::switch_channel_t, uuid: &str) -> Option<Arc<Self>> {
        let _guard = private_lock().lock().expect("private registry lock");
        // SAFETY: `channel` 在 API 回调期间存活。
        let raw = unsafe { sys::switch_channel_get_private(channel, sysutil::context_key().as_ptr()) };
        if !raw.is_null() {
            // SAFETY: 非空 private 数据总是 `install` 安装的装箱 Weak。
            let weak = unsafe { &*raw.cast::<Weak<SessionShared>>() };
            if let Some(shared) = weak.upgrade() {
                return Some(shared);
            }
            let shared = Arc::new(SessionShared::new(uuid.to_owned()));
            // SAFETY: 覆盖同一个装箱 Weak 槽位；通道存活期内槽位保持有效。
            unsafe { *raw.cast::<Weak<SessionShared>>() = Arc::downgrade(&shared) };
            return Some(shared);
        }

        let shared = Arc::new(SessionShared::new(uuid.to_owned()));
        let boxed = Box::into_raw(Box::new(Arc::downgrade(&shared)));
        // SAFETY: `boxed` 存活至 `uninstall` 或通道销毁；通道 private 表
        // 不接管所有权，因此通道关闭路径需显式回收。
        let status = unsafe {
            sys::switch_channel_set_private(
                channel,
                sysutil::context_key().as_ptr(),
                boxed.cast::<std::ffi::c_void>(),
            )
        };
        if status != fswtch::SUCCESS {
            // SAFETY: `boxed` 从未发布。
            drop(unsafe { Box::from_raw(boxed) });
            return None;
        }
        Some(shared)
    }

    /// 升级通道 private 弱指针，不创建新上下文。
    pub fn get(channel: *mut sys::switch_channel_t) -> Option<Arc<Self>> {
        // 持锁期间 `uninstall` 无法回收装箱弱指针，保证下方解引用安全。
        let _guard = private_lock().lock().expect("private registry lock");
        // SAFETY: `channel` 在 API 回调期间存活。
        let raw = unsafe { sys::switch_channel_get_private(channel, sysutil::context_key().as_ptr()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: 非空 private 数据总是 `install` 安装的装箱 Weak；该 box
        // 仅在通道关闭时被回收。
        unsafe { &*raw.cast::<Weak<SessionShared>>() }.upgrade()
    }

    /// 通道自身销毁时丢弃装箱弱指针。在途 `Arc` 克隆保持上下文存活，
    /// 只移除注册表条目。
    pub fn uninstall(channel: *mut sys::switch_channel_t) {
        let _guard = private_lock().lock().expect("private registry lock");
        // SAFETY: `channel` 在驱动本路径的 media bug CLOSE 回调期间存活
        // （通道正被拆除）。
        let raw = unsafe { sys::switch_channel_get_private(channel, sysutil::context_key().as_ptr()) };
        if raw.is_null() {
            return;
        }
        // SAFETY: 先清空槽位可保证后续不会再有升级与下面的 box 回收竞争。
        unsafe {
            sys::switch_channel_set_private(channel, sysutil::context_key().as_ptr(), std::ptr::null());
            drop(Box::from_raw(raw.cast::<Weak<SessionShared>>()));
        }
    }

    // ------------------------------------------------------------------
    // 生命周期（启动采集 / 会话清理）
    // ------------------------------------------------------------------

    /// IDLE -> STARTING 状态迁移；已有音频流运行时返回 false。
    pub fn begin_start(&self) -> bool {
        let mut inner = self.inner();
        if inner.state != CtxState::Idle {
            return false;
        }
        inner.state = CtxState::Starting;
        inner.bug = None;
        inner.startup_failed = false;
        true
    }

    /// `reset_failed_start`：`start` 失败后回到 IDLE。
    pub fn fail_start(&self) {
        let mut inner = self.inner();
        inner.bug = None;
        inner.state = CtxState::Idle;
        inner.startup_failed = false;
    }

    /// 发布已挂接的 bug 并将状态置为 ACTIVE。返回传输线程期间可能已置位
    /// 的启动失败标志。
    pub fn activate(
        &self,
        bug: *mut sys::switch_media_bug_t,
        streamer: Arc<Streamer>,
    ) -> bool {
        let mut inner = self.inner();
        inner.bug = Some(BugHandle::new(bug));
        inner.streamer = Some(streamer);
        inner.state = CtxState::Active;
        let failed = inner.startup_failed;
        inner.startup_failed = false;
        failed
    }

    /// 请 FreeSWITCH 关闭 bug，由此驱动 CLOSE 回调完成真正的清理。
    pub fn request_close(&self) {
        let bug = {
            let mut inner = self.inner();
            if inner.state == CtxState::Starting && inner.bug.is_none() {
                inner.startup_failed = true;
                return;
            }
            match (inner.bug, inner.state) {
                (Some(bug), CtxState::Active | CtxState::Paused) => {
                    self.close_requested.store(true, Ordering::Release);
                    Some(bug)
                }
                _ => None,
            }
        };
        if let Some(bug) = bug {
            sysutil::close_bug(bug.as_ptr());
        }
    }

    pub fn close_requested(&self) -> bool {
        self.close_requested.load(Ordering::Acquire)
    }

    /// 会话清理。`session` 是 `switch_core_media_bug_remove` 调用所需的
    /// 参数，必须是存活会话（API 线程持有其读锁；CLOSE 回调从 bug 取得）。
    /// `channel_is_closing`：通道自身销毁时为 true，经 API/传输层请求
    /// 关闭时为 false。
    pub fn cleanup(
        &self,
        session: *mut sys::switch_core_session_t,
        text: Option<&str>,
        channel_is_closing: bool,
    ) -> bool {
        let bug = {
            let mut inner = self.inner();
            if inner.bug.is_none() {
                return false;
            }
            inner.state = CtxState::Stopping;
            inner.bug.take()
        };
        let Some(bug) = bug else {
            return false;
        };

        if self.cleanup_started.swap(true, Ordering::AcqRel) {
            return true;
        }

        sysutil::log_session_uuid(
            &self.uuid_cstr(),
            sys::switch_log_level_t_SWITCH_LOG_DEBUG,
            &format!("({}) audio_relay_cleanup", self.uuid),
        );

        let streamer = self.inner().streamer.take();

        if !channel_is_closing && !session.is_null() {
            // SAFETY: `session` 存活，`bug` 是已挂接的句柄。
            unsafe {
                let mut raw = bug.as_ptr();
                sys::switch_core_media_bug_remove(session, &mut raw);
            }
        }

        if let Some(streamer) = streamer {
            // 与 C 清理顺序一致：deleteFiles + 告别文本 +
            // markCleanedUp + disconnect。
            streamer.cleanup(text);
        }

        if !channel_is_closing {
            self.inner().state = CtxState::Idle;
        }

        sysutil::log_session_uuid(
            &self.uuid_cstr(),
            sys::switch_log_level_t_SWITCH_LOG_INFO,
            &format!("({}) audio_relay_cleanup: connection closed", self.uuid),
        );
        true
    }

    fn uuid_cstr(&self) -> CString {
        sysutil::cstring(&self.uuid)
    }

    // ------------------------------------------------------------------
    // API 操作
    // ------------------------------------------------------------------

    /// 仅在 bug 已挂接且流为 ACTIVE/PAUSED 时返回 streamer；否则返回
    /// None，复现 C 版所有“非活跃”失败路径。
    pub fn active_streamer(&self) -> Option<Arc<Streamer>> {
        let inner = self.inner();
        if inner.bug.is_some()
            && matches!(inner.state, CtxState::Active | CtxState::Paused)
        {
            inner.streamer.clone()
        } else {
            None
        }
    }

    /// 暂停/恢复：flush bug、暂停采集并在 ACTIVE 与
    /// PAUSED 之间迁移状态机。
    pub fn pause_resume(&self, pause: bool) -> bool {
        let mut inner = self.inner();
        let expected = if pause { CtxState::Active } else { CtxState::Paused };
        let Some(bug) = inner.bug else {
            return false;
        };
        if inner.state != expected {
            return false;
        }
        let streamer = inner.streamer.clone();
        // SAFETY: `bug` 是存活的已挂接句柄。
        unsafe { sys::switch_core_media_bug_flush(bug.as_ptr()) };
        if let Some(streamer) = streamer {
            streamer.set_audio_paused(pause);
        }
        inner.state = if pause { CtxState::Paused } else { CtxState::Active };
        true
    }

    /// 中断：丢弃排队播放数据并通知服务端。
    pub fn interrupt(&self) -> bool {
        match self.active_streamer() {
            Some(streamer) => {
                streamer.interrupt();
                true
            }
            None => false,
        }
    }

    /// 播放控制（play/pause/stop 等动作）。
    pub fn playback_control(&self, action: PlaybackAction) -> bool {
        match self.active_streamer() {
            Some(streamer) => {
                streamer.playback_control(action);
                true
            }
            None => false,
        }
    }

    /// 发送文本消息。
    pub fn send_text(&self, text: &str) -> bool {
        match self.active_streamer() {
            Some(streamer) => {
                streamer.write_text(text);
                true
            }
            None => false,
        }
    }

    /// 排空播放队列，含其暂停前置条件。
    pub fn drain_playback(&self) -> bool {
        {
            let inner = self.inner();
            if inner.bug.is_none()
                || !matches!(inner.state, CtxState::Active | CtxState::Paused)
            {
                return false;
            }
        }
        let Some(streamer) = self.active_streamer() else {
            return false;
        };
        if streamer.audio_paused() {
            sysutil::log_session_uuid(
                &self.uuid_cstr(),
                sys::switch_log_level_t_SWITCH_LOG_WARNING,
                "cannot drain playback while the stream is paused",
            );
            return false;
        }
        if streamer.playback_paused() {
            sysutil::log_session_uuid(
                &self.uuid_cstr(),
                sys::switch_log_level_t_SWITCH_LOG_WARNING,
                "cannot drain playback while playback is paused",
            );
            return false;
        }
        if !streamer.drain_playback() {
            sysutil::log_session_uuid(
                &self.uuid_cstr(),
                sys::switch_log_level_t_SWITCH_LOG_WARNING,
                "playback drain timed out; remaining queued audio will be discarded",
            );
        }
        true
    }

    /// 播放状态查询；“无上下文”的 idle 情况由调用方处理，
    /// 本函数只在上下文存在时运行。
    pub fn playback_status(&self) -> PlaybackStatus {
        let inner = self.inner();
        let mut status = PlaybackStatus {
            state: inner.state.name(),
            paused: false,
            queued_bytes: 0,
            sample_rate: 0,
        };
        if inner.bug.is_some() {
            if let Some(streamer) = inner.streamer.clone() {
                status.queued_bytes = streamer.queued_playback_bytes();
                status.sample_rate = streamer.playback_rate();
                status.paused =
                    streamer.playback_paused() || streamer.audio_paused();
            }
        }
        status
    }
}

/// 串行化通道 private 表中装箱弱指针的创建/查找；API 命令本就低频，
/// 单把互斥锁足够。
fn private_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

/// `StreamHooks` 实现：通过 `switch_event_*` 发出 `mod_audio_relay::*`
/// 事件，并把传输层驱动的关闭路由到 media bug。
pub struct FsHooks {
    shared: Arc<SessionShared>,
    uuid: CString,
}

impl FsHooks {
    pub fn new(shared: Arc<SessionShared>) -> Self {
        Self {
            uuid: sysutil::cstring(shared.uuid()),
            shared,
        }
    }
}

impl StreamHooks for FsHooks {
    fn emit(&self, kind: EventKind, body: &str) {
        let subclass = format!("mod_audio_relay::{}", kind.suffix());
        sysutil::emit_event(self.uuid.as_bytes(), &subclass, body);
    }

    fn close_stream(&self) {
        self.shared.request_close();
    }

    fn log(&self, level: LogLevel, message: &str) {
        sysutil::log_session_uuid(&self.uuid, map_log_level(level), message);
    }
}

fn map_log_level(level: LogLevel) -> sys::switch_log_level_t {
    match level {
        LogLevel::Debug => sys::switch_log_level_t_SWITCH_LOG_DEBUG,
        LogLevel::Info => sys::switch_log_level_t_SWITCH_LOG_INFO,
        LogLevel::Warning => sys::switch_log_level_t_SWITCH_LOG_WARNING,
        LogLevel::Error => sys::switch_log_level_t_SWITCH_LOG_ERROR,
    }
}
