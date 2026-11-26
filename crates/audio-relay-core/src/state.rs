/// 附着在单个通道上的音频流生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    Starting,
    Active,
    Paused,
    Stopping,
}

impl StreamState {
    /// `status` API 命令上报的值。
    pub fn status_name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
        }
    }

    /// start 命令守卫写入日志的值。
    pub fn log_name(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Starting => "STARTING",
            Self::Active => "ACTIVE",
            Self::Paused => "PAUSED",
            Self::Stopping => "STOPPING",
        }
    }
}

/// 仅控制播放的动作，保持可寻址形式，使 API 层无需暴露 streamer 即可
/// 分发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackAction {
    Pause,
    Resume,
    Stop,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_names_are_lowercase_as_the_api_emits_them() {
        assert_eq!(StreamState::Idle.status_name(), "idle");
        assert_eq!(StreamState::Starting.status_name(), "starting");
        assert_eq!(StreamState::Active.status_name(), "active");
        assert_eq!(StreamState::Paused.status_name(), "paused");
        assert_eq!(StreamState::Stopping.status_name(), "stopping");
        assert_eq!(StreamState::Starting.log_name(), "STARTING");
    }
}
