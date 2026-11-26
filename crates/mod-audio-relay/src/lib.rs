//! FreeSWITCH `mod_audio_relay` 模块的 Rust 实现：为通话提供
//! WebSocket 双向音频中继能力。
//!
//! 协议、播放队列、混音器、FIFO 侧信道和流状态机位于 `audio-relay-core`；
//! 本 crate 是基于 `fswtch` 的精简 FreeSWITCH 绑定层：`uuid_audio_relay`
//! API 命令、按会话隔离的 channel-private 上下文、`audio_relay` 媒体 bug，
//! 以及 `mod_audio_relay::*` 自定义事件。
//!
//! 编译产物为 `libmod_audio_relay.so`；FreeSWITCH 按 `<name>.so` 加载模块，
//! 因此需安装为 `mod_audio_relay.so`（或建立符号链接）。

mod api;
mod bug;
mod decoder;
mod shared;
mod speex;
mod sysutil;

use fswtch::{Status, SUCCESS};

fswtch::module_exports! {
    module = mod_audio_relay,
    load = switch_module_load,
    shutdown = Some(switch_module_shutdown),
    runtime = None,
}

const MODULE_NAME: &str = "mod_audio_relay";

/// 模块加载期间预留的事件子类，卸载时释放。
const EVENT_SUBCLASSES: [&str; 5] = [
    "mod_audio_relay::connect",
    "mod_audio_relay::disconnect",
    "mod_audio_relay::error",
    "mod_audio_relay::json",
    "mod_audio_relay::play",
];

fswtch::module_load! {
    fn switch_module_load(module) for "mod_audio_relay" {
        fswtch::log_info(MODULE_NAME, "mod_audio_relay API loading..");

        for subclass in EVENT_SUBCLASSES {
            let name = std::ffi::CString::new(subclass).unwrap_or_default();
            // SAFETY: `name` 在调用期间是存活的 C 字符串；
            // FreeSWITCH 跟踪该预留，直到卸载时释放。
            let status = unsafe {
                fswtch::sys::switch_event_reserve_subclass_detailed(
                    c"mod_audio_relay".as_ptr(),
                    name.as_ptr(),
                )
            };
            if let Err(error) = fswtch::status_to_result(status) {
                fswtch::log_error(
                    MODULE_NAME,
                    format!("Couldn't register event subclass {subclass}: {error}"),
                );
                return error.0;
            }
        }

        module.api(
            "uuid_audio_relay",
            "audio_relay API",
            sysutil::API_SYNTAX,
            api::uuid_audio_relay,
        )
    }
}

// SAFETY: FreeSWITCH 以模块 ABI 调用卸载回调；它只释放加载时
// 预留的事件子类。
unsafe extern "C" fn switch_module_shutdown() -> Status {
    for subclass in EVENT_SUBCLASSES {
        let name = std::ffi::CString::new(subclass).unwrap_or_default();
        // SAFETY: `name` 在调用期间是存活的 C 字符串，且该子类
        // 是本模块在加载时预留的。
        unsafe {
            fswtch::sys::switch_event_free_subclass_detailed(
                c"mod_audio_relay".as_ptr(),
                name.as_ptr(),
            );
        }
    }
    SUCCESS
}
