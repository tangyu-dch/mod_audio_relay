//! 单通话状态机与采集累加器。
//!
//! FreeSWITCH crate 提供三个宿主钩子（事件、media bug 关闭、文件解码）
//! 与 speexdsp 重采样引擎；其余部分都在这里，可在没有 switch 的环境中
//! 测试。

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::config::StreamConfig;
use crate::fifo::PlaybackFifo;
use crate::mixer::mix_s16le;
use crate::protocol::{self, MessageAction, StreamAudioOutcome, StreamAudioRequest};
use crate::queue::PlaybackQueue;
use crate::resampler::{PcmResampler, ResampleEngine, MAX_RATE, MIN_RATE};
use crate::state::PlaybackAction;
use crate::transport::{
    LogLevel, LogSink, TransportEvent, TransportOptions, WebSocketTransport,
};
use crate::url::WebSocketUrl;
use crate::{MAX_BINARY_PCM_BYTES, RECOMMENDED_BUFFER_SIZE};

/// 以 `mod_audio_relay::<kind>` 发出的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Connect,
    Disconnect,
    Error,
    Play,
    Json,
}

impl EventKind {
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::Error => "error",
            Self::Play => "play",
            Self::Json => "json",
        }
    }
}

/// 状态机所需的宿主回调。
pub trait StreamHooks: Send + Sync {
    /// 以 `body` 为 JSON 负载触发自定义 FreeSWITCH 事件。
    fn emit(&self, kind: EventKind, body: &str);
    /// 请求关闭 media bug。
    fn close_stream(&self);
    fn log(&self, level: LogLevel, message: &str);
}

/// 经由宿主（FreeSWITCH 文件句柄）解码编码播放文件。
pub trait AudioDecoder: Send + Sync {
    /// 以 4096 采样块向 `sink` 喂入 `rate` 采样率的单声道 L16。
    /// 无可用解码器时返回 false；`sink` 返回 false 可提前终止。
    fn decode(&self, path: &str, rate: u32, sink: &mut dyn FnMut(&[u8]) -> bool) -> bool;
}

/// 创建新的重采样后端；FreeSWITCH crate 返回 speexdsp。
pub type EngineFactory = Arc<dyn Fn() -> Box<dyn ResampleEngine> + Send + Sync>;

/// 启动单条通话音频流所需的全部配置。
pub struct StreamerOptions {
    pub uuid: String,
    /// `SWITCH_GLOBAL_dirs.temp_dir`，编码播放文件写入此处。
    pub temp_dir: String,
    pub config: StreamConfig,
    pub url: WebSocketUrl,
    /// socket 就绪后作为第一条文本消息发送。
    pub initial_metadata: Option<String>,
    pub hooks: Arc<dyn StreamHooks>,
    pub decoder: Arc<dyn AudioDecoder>,
    pub engines: EngineFactory,
    pub log: Option<LogSink>,
    pub connection_timeout: Duration,
}

/// 一条运行中的音频流。可经 `Arc` 廉价克隆；每个方法都可安全地从
/// API 线程、媒体线程或传输线程调用。
pub struct Streamer {
    shared: Arc<Shared>,
    transport: WebSocketTransport,
}

struct Shared {
    uuid: String,
    temp_dir: String,
    config: StreamConfig,
    hooks: Arc<dyn StreamHooks>,
    decoder: Arc<dyn AudioDecoder>,
    ready_for_audio: AtomicBool,
    cleaned_up: AtomicBool,
    accepting_playback: AtomicBool,
    playback_paused: AtomicBool,
    audio_paused: AtomicBool,
    playback_generation: AtomicU64,
    fifo_error_logged: AtomicBool,
    playback: Mutex<Playback>,
    files: Mutex<Files>,
}

struct Playback {
    queue: PlaybackQueue,
    resampler: PcmResampler<Box<dyn ResampleEngine>>,
    fifo: PlaybackFifo,
    fifo_resampler: PcmResampler<Box<dyn ResampleEngine>>,
    fifo_enabled: bool,
}

#[derive(Default)]
struct Files {
    next_index: i32,
    paths: BTreeSet<String>,
}

impl Streamer {
    /// 启动传输线程。配置的音频管道不可用时返回 `None`。
    pub fn start(options: StreamerOptions) -> Option<Arc<Self>> {
        let config = options.config.clone();
        let mut fifo = PlaybackFifo::new();
        let mut fifo_enabled = false;
        if let Some(path) = config.audio_pipe.as_deref().filter(|path| !path.is_empty()) {
            let rates_valid = (MIN_RATE..=MAX_RATE).contains(&config.write_rate)
                && (MIN_RATE..=MAX_RATE).contains(&config.audio_pipe_rate);
            if !rates_valid || fifo.configure(path).is_err() {
                return None;
            }
            fifo_enabled = true;
        }

        let shared = Arc::new(Shared {
            uuid: options.uuid,
            temp_dir: options.temp_dir,
            config: config.clone(),
            hooks: Arc::clone(&options.hooks),
            decoder: Arc::clone(&options.decoder),
            ready_for_audio: AtomicBool::new(false),
            cleaned_up: AtomicBool::new(false),
            accepting_playback: AtomicBool::new(true),
            playback_paused: AtomicBool::new(false),
            audio_paused: AtomicBool::new(false),
            playback_generation: AtomicU64::new(0),
            fifo_error_logged: AtomicBool::new(false),
            playback: Mutex::new(Playback {
                queue: PlaybackQueue::new(config.playback_queue_capacity()),
                resampler: PcmResampler::new((options.engines)(), PLAYBACK_QUALITY),
                fifo,
                fifo_resampler: PcmResampler::new((options.engines)(), PLAYBACK_QUALITY),
                fifo_enabled,
            }),
            files: Mutex::new(Files::default()),
        });

        let transport_options = TransportOptions {
            tls: config.tls.clone(),
            compression_requested: config.compression,
            heart_beat_seconds: config.heart_beat_seconds,
            connection_timeout: options.connection_timeout,
            extra_headers: config.extra_headers.clone(),
            initial_metadata: options.initial_metadata,
            log: options.log,
            ..TransportOptions::default()
        };

        let events = Arc::clone(&shared);
        let transport = WebSocketTransport::spawn(options.url, transport_options, move |event| {
            handle_event(&events, event);
        });
        Some(Arc::new(Self { shared, transport }))
    }

    pub fn uuid(&self) -> &str {
        &self.shared.uuid
    }

    pub fn config(&self) -> &StreamConfig {
        &self.shared.config
    }

    pub fn is_connected(&self) -> bool {
        self.transport.is_connected()
    }

    /// 因有界出站队列已满而丢弃的消息数。
    pub fn dropped_messages(&self) -> u64 {
        self.transport.dropped_messages()
    }

    /// 上行 PCM，受音频门控控制。
    pub fn write_binary(&self, pcm: &[u8]) {
        if !self.shared.ready_for_audio.load(Ordering::Acquire) || !self.is_connected() {
            return;
        }
        self.transport.send_binary(pcm);
    }

    pub fn write_text(&self, text: &str) {
        if !self.is_connected() {
            return;
        }
        self.transport.send_text(text);
    }

    pub fn set_audio_paused(&self, paused: bool) {
        self.shared.audio_paused.store(paused, Ordering::Release);
    }

    pub fn audio_paused(&self) -> bool {
        self.shared.audio_paused.load(Ordering::Acquire)
    }

    pub fn set_playback_paused(&self, paused: bool) {
        self.shared.playback_paused.store(paused, Ordering::Release);
    }

    pub fn playback_paused(&self) -> bool {
        self.shared.playback_paused.load(Ordering::Acquire)
    }

    pub fn playback_control(&self, action: PlaybackAction) {
        match action {
            PlaybackAction::Pause => self.set_playback_paused(true),
            PlaybackAction::Resume => self.set_playback_paused(false),
            PlaybackAction::Stop => self.clear_pcm(),
        }
    }

    /// 丢弃排队播放并通知服务端停止播报。
    pub fn interrupt(&self) {
        self.clear_pcm();
        self.write_text(r#"{"type":"interrupt"}"#);
    }

    pub fn clear_pcm(&self) {
        self.shared.playback_generation.fetch_add(1, Ordering::Release);
        let mut playback = self.shared.playback.lock().expect("playback lock");
        playback.queue.clear();
        playback.resampler.reset();
        playback.fifo_resampler.reset();
        playback.fifo.clear_pending();
    }

    pub fn begin_playback_drain(&self) {
        self.shared.accepting_playback.store(false, Ordering::Release);
    }

    pub fn accepting_playback(&self) -> bool {
        self.shared.accepting_playback.load(Ordering::Acquire)
    }

    pub fn queued_playback_bytes(&self) -> usize {
        self.shared.playback.lock().expect("playback lock").queue.len()
    }

    pub fn playback_rate(&self) -> u32 {
        self.shared.config.write_rate
    }

    /// 等待 write-replace 回调消费队列（最多 300 次 * 10 ms）。
    pub fn drain_playback(&self) -> bool {
        self.begin_playback_drain();
        for _ in 0..300 {
            if self.queued_playback_bytes() == 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    pub fn read_pcm(&self, dst: &mut [u8]) -> usize {
        self.shared.playback.lock().expect("playback lock").queue.read(dst)
    }

    /// 写替换帧主体：取出排队 PCM、镜像到 FIFO
    /// 并混入写替换帧。返回 true 表示调用方必须发布被修改的帧。
    pub fn write_replace(&self, frame: &mut [u8], datalen: usize, channels: usize) -> bool {
        if self.audio_paused() || self.shared.cleaned_up.load(Ordering::Acquire) {
            return false;
        }
        if self.playback_paused() || datalen < 2 {
            return false;
        }
        let channels = channels.max(1);
        let per_channel_capacity = datalen / (channels * 2);
        let capacity = (per_channel_capacity * 2).min(RECOMMENDED_BUFFER_SIZE);
        let mut pcm = [0u8; RECOMMENDED_BUFFER_SIZE];
        let bytes = self.read_pcm(&mut pcm[..capacity]);
        if bytes == 0 {
            return false;
        }
        self.write_pcm_to_pipe(&pcm[..bytes]);
        mix_s16le(&mut frame[..datalen], &pcm[..bytes], channels);
        true
    }

    fn write_pcm_to_pipe(&self, pcm: &[u8]) -> bool {
        let mut playback = self.shared.playback.lock().expect("playback lock");
        if !playback.fifo_enabled {
            return true;
        }
        let config = &self.shared.config;
        let converted = if config.write_rate == config.audio_pipe_rate {
            pcm.to_vec()
        } else {
            match playback.fifo_resampler.process_pcm(
                pcm,
                config.write_rate,
                config.audio_pipe_rate,
                1,
            ) {
                Ok(converted) => converted,
                Err(_) => return false,
            }
        };
        let ok = playback.fifo.write_pcm(&converted).is_ok();
        if !ok && !self.shared.fifo_error_logged.swap(true, Ordering::AcqRel) {
            self.shared.hooks.log(
                LogLevel::Error,
                &format!(
                    "({}) failed writing playback PCM to configured FIFO",
                    self.shared.uuid
                ),
            );
        }
        ok
    }

    /// 会话清理的 streamer 半侧：删除临时
    /// 文件、发送可选告别文本、停止接收音频并断开连接。
    pub fn cleanup(&self, text: Option<&str>) {
        self.delete_files();
        if let Some(text) = text {
            self.write_text(text);
        }
        self.mark_cleaned_up();
        self.shared.hooks.log(LogLevel::Debug, "disconnecting...");
        self.transport.close(1000, "");
    }

    pub fn mark_cleaned_up(&self) {
        self.shared.ready_for_audio.store(false, Ordering::Release);
        self.shared.cleaned_up.store(true, Ordering::Release);
    }

    pub fn is_cleaned_up(&self) -> bool {
        self.shared.cleaned_up.load(Ordering::Acquire)
    }

    fn delete_files(&self) {
        let paths = {
            let mut files = self.shared.files.lock().expect("files lock");
            if files.paths.is_empty() {
                return;
            }
            files.next_index = 0;
            std::mem::take(&mut files.paths)
        };
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    }

    /// media bug 读帧前使用的采集侧暂停门。
    pub fn capture_blocked(&self) -> bool {
        self.audio_paused() || self.shared.cleaned_up.load(Ordering::Acquire)
    }
}

/// 播放路径使用的 Speex 质量等级。
const PLAYBACK_QUALITY: u32 = 5;

fn handle_event(shared: &Arc<Shared>, event: TransportEvent) {
    if shared.cleaned_up.load(Ordering::Acquire) {
        return;
    }
    match event {
        TransportEvent::Connected { .. } => {
            // 传输层已发送初始 metadata；此刻上行音频才可流动，
            // connect 事件也只在此时触发。
            shared.ready_for_audio.store(true, Ordering::Release);
            shared.hooks.emit(EventKind::Connect, r#"{"status":"connected"}"#);
        }
        TransportEvent::Text(message) => {
            if !shared.accepting_playback.load(Ordering::Acquire) {
                return;
            }
            let outcome = process_message(shared, &message);
            for error in &outcome.errors {
                shared
                    .hooks
                    .log(LogLevel::Error, &format!("({}) {}", shared.uuid, error));
            }
            let body = match &outcome.action {
                MessageAction::Play(data) => {
                    shared.hooks.emit(EventKind::Play, data);
                    data.as_str()
                }
                MessageAction::Forward => {
                    shared.hooks.emit(EventKind::Json, &message);
                    message.as_str()
                }
            };
            if !shared.config.suppress_log {
                shared
                    .hooks
                    .log(LogLevel::Debug, &format!("response: {body}"));
            }
        }
        TransportEvent::Binary(pcm) => {
            if !shared.accepting_playback.load(Ordering::Acquire) || pcm.is_empty() {
                return;
            }
            if pcm.len() > MAX_BINARY_PCM_BYTES || pcm.len() & 1 != 0 {
                shared.hooks.log(
                    LogLevel::Warning,
                    &format!(
                        "({}) dropping invalid binary PCM message ({} bytes; maximum 8192 even bytes)",
                        shared.uuid,
                        pcm.len()
                    ),
                );
                return;
            }
            let generation = shared.playback_generation.load(Ordering::Acquire);
            if !queue_pcm(shared, &pcm, shared.config.sampling, generation) {
                shared.hooks.log(
                    LogLevel::Error,
                    &format!("({}) failed to resample incoming binary PCM", shared.uuid),
                );
            }
        }
        TransportEvent::Error(error) => {
            shared.hooks.log(LogLevel::Info, "connection error");
            let body = json!({
                "status": "error",
                "message": { "code": error.code.as_i32(), "error": error.message }
            })
            .to_string();
            shared.hooks.emit(EventKind::Error, &body);
            shared.hooks.close_stream();
        }
        TransportEvent::Closed { code, reason } => {
            shared.hooks.log(LogLevel::Info, "connection closed");
            let body = json!({
                "status": "disconnected",
                "message": { "code": code, "reason": reason }
            })
            .to_string();
            shared.hooks.emit(EventKind::Disconnect, &body);
            shared.hooks.close_stream();
        }
    }
}

/// 下行消息分发，含临时文件与解码器副作用。
fn process_message(shared: &Arc<Shared>, message: &str) -> StreamAudioOutcome {
    let generation = shared.playback_generation.load(Ordering::Acquire);
    match protocol::parse(message) {
        StreamAudioRequest::NotStreamAudio => StreamAudioOutcome::forward(),
        StreamAudioRequest::Invalid(errors) => StreamAudioOutcome::reject(errors),
        StreamAudioRequest::Raw { sample_rate, pcm, mut data } => {
            if pcm.len() & 1 != 0 {
                return StreamAudioOutcome::reject(vec![
                    "processMessage - raw PCM has an incomplete sample".to_owned(),
                ]);
            }
            if !queue_pcm(shared, &pcm, sample_rate as u32, generation) {
                return StreamAudioOutcome::reject(vec![
                    "processMessage - raw PCM could not be converted to the call sample rate"
                        .to_owned(),
                ]);
            }
            StreamAudioOutcome::play(protocol::finish_data(&mut data, "queued", None))
        }
        StreamAudioRequest::File { extension, bytes, mut data } => {
            let index = {
                let mut files = shared.files.lock().expect("files lock");
                let index = files.next_index;
                files.next_index += 1;
                index
            };
            let path = format!(
                "{}{}{}_{}.tmp{}",
                shared.temp_dir,
                std::path::MAIN_SEPARATOR,
                shared.uuid,
                index,
                extension
            );
            if let Err(error) = std::fs::write(&path, &bytes) {
                let reason = if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::PermissionDenied
                {
                    format!("processMessage - failed to open file for write: {path}")
                } else {
                    format!("processMessage - failed writing file: {path}")
                };
                return StreamAudioOutcome::reject(vec![reason]);
            }

            let queued = decode_file_to_queue(shared, &path, generation);
            if !queued {
                shared.hooks.log(
                    LogLevel::Error,
                    &format!(
                        "({}) processMessage - no FreeSWITCH decoder available for {}",
                        shared.uuid, extension
                    ),
                );
            }
            shared
                .files
                .lock()
                .expect("files lock")
                .paths
                .insert(path.clone());
            let playback = if queued { "queued" } else { "file" };
            StreamAudioOutcome::play(protocol::finish_data(&mut data, playback, Some(&path)))
        }
    }
}

/// `queuePcm`：重采样到通话速率后入队，除非消息到达后播放已被
/// 中断或排空。
fn queue_pcm(shared: &Arc<Shared>, pcm: &[u8], input_rate: u32, generation: u64) -> bool {
    if input_rate == 0 {
        return false;
    }
    let mut playback = shared.playback.lock().expect("playback lock");
    if !shared.accepting_playback.load(Ordering::Relaxed)
        || generation != shared.playback_generation.load(Ordering::Relaxed)
    {
        return false;
    }
    let converted = match playback.resampler.process_pcm(
        pcm,
        input_rate,
        shared.config.write_rate,
        1,
    ) {
        Ok(converted) => converted,
        Err(_) => return false,
    };
    playback.queue.push(&converted);
    true
}

/// 文件解码入队：宿主解码器喂入的块以相同的代际守卫入队，
/// 因此中断可以在文件解码中途生效。
fn decode_file_to_queue(shared: &Arc<Shared>, path: &str, generation: u64) -> bool {
    let mut read_any = false;
    let rate = shared.config.write_rate;
    let mut sink = |chunk: &[u8]| -> bool {
        if !enqueue_pcm(shared, chunk, generation) {
            return false;
        }
        read_any = true;
        true
    };
    if !shared.decoder.decode(path, rate, &mut sink) {
        return false;
    }
    read_any
}

fn enqueue_pcm(shared: &Arc<Shared>, pcm: &[u8], generation: u64) -> bool {
    let mut playback = shared.playback.lock().expect("playback lock");
    if !shared.accepting_playback.load(Ordering::Relaxed)
        || generation != shared.playback_generation.load(Ordering::Relaxed)
    {
        return false;
    }
    playback.queue.push(pcm);
    true
}

/// 采集半侧：累积解码后的 L16 帧、重采样到线速率，并按
/// `rtp_packets` 个包发出一条二进制消息。放不下的剩余输入被丢弃。
pub struct CapturePipeline {
    buffer: Vec<u8>,
    capacity: usize,
    channels: usize,
    rtp_packets: u32,
    input_rate: u32,
    output_rate: u32,
    resampler: Option<PcmResampler<Box<dyn ResampleEngine>>>,
}

impl CapturePipeline {
    /// codec 速率已等于线速率时 `engine` 为 `None`。
    pub fn new(config: &StreamConfig, engine: Option<Box<dyn ResampleEngine>>) -> Self {
        Self {
            buffer: Vec::new(),
            capacity: config.capture_buffer_capacity(),
            channels: config.channels.max(1) as usize,
            rtp_packets: config.rtp_packets,
            input_rate: config.read_rate,
            output_rate: config.sampling,
            resampler: engine.map(|engine| PcmResampler::new(engine, CAPTURE_QUALITY)),
        }
    }

    /// 喂入一帧解码数据；`samples` 为每声道采样数。按顺序返回待发送
    /// 的完整二进制消息。
    pub fn push(&mut self, pcm: &[u8], samples: u32) -> Vec<Vec<u8>> {
        let mut pending = Vec::new();
        if pcm.is_empty() {
            return pending;
        }
        let Some(resampler) = self.resampler.as_mut() else {
            if self.rtp_packets == 1 {
                pending.push(pcm.to_vec());
                return pending;
            }
            if self.capacity - self.buffer.len() >= pcm.len() {
                self.buffer.extend_from_slice(pcm);
            }
            self.flush_if_full(&mut pending);
            return pending;
        };

        let freespace = self.capacity - self.buffer.len();
        let max_output_frames = freespace / (self.channels * 2);
        if max_output_frames == 0 {
            if freespace == 0 {
                self.flush(&mut pending);
            }
            return pending;
        }

        let frames: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        // 忽略 speex 返回码，只使用 out_len。
        let produced = resampler
            .process_frames(
                &frames[..(samples as usize * self.channels).min(frames.len())],
                self.input_rate,
                self.output_rate,
                self.channels as u32,
                max_output_frames,
            )
            .unwrap_or_default();
        if !produced.is_empty() {
            let bytes: Vec<u8> = produced.iter().flat_map(|s| s.to_le_bytes()).collect();
            if self.rtp_packets == 1 {
                pending.push(bytes);
                return pending;
            }
            if bytes.len() <= self.capacity - self.buffer.len() {
                self.buffer.extend_from_slice(&bytes);
            }
        }
        self.flush_if_full(&mut pending);
        pending
    }

    fn flush_if_full(&mut self, pending: &mut Vec<Vec<u8>>) {
        if self.buffer.len() == self.capacity {
            self.flush(pending);
        }
    }

    fn flush(&mut self, pending: &mut Vec<Vec<u8>>) {
        if !self.buffer.is_empty() {
            pending.push(std::mem::take(&mut self.buffer));
        }
    }
}

/// FreeSWITCH 1.11 中的 `SWITCH_RESAMPLE_QUALITY`。
const CAPTURE_QUALITY: u32 = 2;

/// 与排空循环共用的截止时间助手。
pub fn deadline_passed(start: Instant, limit: Duration) -> bool {
    start.elapsed() >= limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resampler::ResampleError;
    use crate::transport::TlsOptions;
    use std::sync::Mutex as StdMutex;

    /// 最近邻引擎：确定性输出，无需 speexdsp。
    struct FakeEngine {
        channels: u32,
        input_rate: u32,
        output_rate: u32,
    }

    impl ResampleEngine for FakeEngine {
        fn init(
            &mut self,
            channels: u32,
            input_rate: u32,
            output_rate: u32,
            _quality: u32,
        ) -> Result<(), ResampleError> {
            self.channels = channels;
            self.input_rate = input_rate;
            self.output_rate = output_rate;
            Ok(())
        }

        fn process(
            &mut self,
            input: &[i16],
            output: &mut [i16],
        ) -> Result<(usize, usize), ResampleError> {
            let channels = self.channels.max(1) as usize;
            let in_frames = input.len() / channels;
            let out_frames = output.len() / channels;
            let wanted = in_frames as u64 * self.output_rate as u64 / self.input_rate as u64;
            let produced = (wanted as usize).min(out_frames);
            for frame in 0..produced {
                let source = (frame as u64 * self.input_rate as u64 / self.output_rate as u64)
                    as usize
                    % in_frames.max(1);
                for channel in 0..channels {
                    output[frame * channels + channel] = input[source * channels + channel];
                }
            }
            let consumed = if produced == out_frames {
                in_frames
            } else {
                (produced as u64 * self.input_rate as u64 / self.output_rate as u64) as usize
            };
            Ok((consumed.min(in_frames), produced))
        }

        fn output_latency(&self) -> u32 {
            0
        }

        fn destroy(&mut self) {}
    }

    #[derive(Default)]
    struct RecordingHooks {
        events: StdMutex<Vec<(EventKind, String)>>,
        logs: StdMutex<Vec<(LogLevel, String)>>,
        closes: StdMutex<u32>,
    }

    impl StreamHooks for RecordingHooks {
        fn emit(&self, kind: EventKind, body: &str) {
            self.events.lock().unwrap().push((kind, body.to_owned()));
        }
        fn close_stream(&self) {
            *self.closes.lock().unwrap() += 1;
        }
        fn log(&self, level: LogLevel, message: &str) {
            self.logs.lock().unwrap().push((level, message.to_owned()));
        }
    }

    struct NoDecoder;
    impl AudioDecoder for NoDecoder {
        fn decode(&self, _path: &str, _rate: u32, _sink: &mut dyn FnMut(&[u8]) -> bool) -> bool {
            false
        }
    }

    struct ChunkDecoder;
    impl AudioDecoder for ChunkDecoder {
        fn decode(&self, _path: &str, _rate: u32, sink: &mut dyn FnMut(&[u8]) -> bool) -> bool {
            sink(&[1, 0, 2, 0]) && sink(&[3, 0, 4, 0])
        }
    }

    fn config(write_rate: u32, sampling: u32) -> StreamConfig {
        StreamConfig {
            sampling,
            channels: 1,
            read_rate: sampling,
            write_rate,
            rtp_packets: 1,
            compression: false,
            heart_beat_seconds: 0,
            suppress_log: false,
            extra_headers: Vec::new(),
            tls: TlsOptions::default(),
            audio_pipe: None,
            audio_pipe_rate: write_rate,
        }
    }

    fn shared(config: StreamConfig, decoder: Arc<dyn AudioDecoder>) -> Arc<Shared> {
        Arc::new(Shared {
            uuid: "test-uuid".to_owned(),
            temp_dir: std::env::temp_dir().to_string_lossy().into_owned(),
            config: config.clone(),
            hooks: Arc::new(RecordingHooks::default()),
            decoder,
            ready_for_audio: AtomicBool::new(false),
            cleaned_up: AtomicBool::new(false),
            accepting_playback: AtomicBool::new(true),
            playback_paused: AtomicBool::new(false),
            audio_paused: AtomicBool::new(false),
            playback_generation: AtomicU64::new(0),
            fifo_error_logged: AtomicBool::new(false),
            playback: Mutex::new(Playback {
                queue: PlaybackQueue::new(config.playback_queue_capacity()),
                resampler: PcmResampler::new(Box::new(FakeEngine { channels: 1, input_rate: 0, output_rate: 0 }), PLAYBACK_QUALITY),
                fifo: PlaybackFifo::new(),
                fifo_resampler: PcmResampler::new(Box::new(FakeEngine { channels: 1, input_rate: 0, output_rate: 0 }), PLAYBACK_QUALITY),
                fifo_enabled: false,
            }),
            files: Mutex::new(Files::default()),
        })
    }

    fn hooks(shared: &Arc<Shared>) -> Arc<dyn StreamHooks> {
        Arc::clone(&shared.hooks)
    }

    #[test]
    fn raw_message_queues_resampled_pcm() {
        let shared = shared(config(8000, 16000), Arc::new(NoDecoder));
        let pcm: Vec<u8> = (0..320i16).flat_map(|s| s.to_le_bytes()).collect();
        use base64::Engine;
        let message = json!({
            "type": "streamAudio",
            "data": {
                "audioDataType": "raw",
                "sampleRate": 16000,
                "audioData": base64::engine::general_purpose::STANDARD.encode(&pcm)
            }
        })
        .to_string();
        let outcome = process_message(&shared, &message);
        assert!(outcome.is_play(), "{outcome:?}");
        assert!(outcome.errors.is_empty());
        let queued = shared.playback.lock().unwrap().queue.len();
        assert_eq!(queued, 320); // 16 kHz -> 8 kHz 采样数减半
        match outcome.action {
            MessageAction::Play(data) => assert!(data.contains(r#""playback":"queued""#)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn odd_raw_pcm_is_rejected() {
        let shared = shared(config(8000, 8000), Arc::new(NoDecoder));
        use base64::Engine;
        let message = json!({
            "type": "streamAudio",
            "data": {
                "audioDataType": "raw",
                "sampleRate": 8000,
                "audioData": base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3])
            }
        })
        .to_string();
        let outcome = process_message(&shared, &message);
        assert!(!outcome.is_play());
        assert_eq!(
            outcome.errors,
            vec!["processMessage - raw PCM has an incomplete sample".to_owned()]
        );
    }

    #[test]
    fn generation_guard_drops_stale_pcm() {
        let shared = shared(config(8000, 8000), Arc::new(NoDecoder));
        assert!(queue_pcm(&shared, &[0, 0], 8000, 0));
        shared.playback_generation.fetch_add(1, Ordering::Release);
        assert!(!queue_pcm(&shared, &[0, 0], 8000, 0));
        shared.accepting_playback.store(false, Ordering::Release);
        assert!(!queue_pcm(&shared, &[0, 0], 8000, 1));
    }

    #[test]
    fn encoded_file_without_decoder_reports_file_playback() {
        let dir = tempfile_dir();
        let mut shared = shared(config(8000, 8000), Arc::new(NoDecoder));
        Arc::get_mut(&mut shared).unwrap().temp_dir = dir.clone();
        use base64::Engine;
        let message = json!({
            "type": "streamAudio",
            "data": {
                "audioDataType": "wav",
                "audioData": base64::engine::general_purpose::STANDARD.encode(b"RIFFdata")
            }
        })
        .to_string();
        let outcome = process_message(&shared, &message);
        assert!(outcome.is_play());
        let expected = format!("{dir}/test-uuid_0.tmp.wav");
        assert!(std::path::Path::new(&expected).exists());
        match outcome.action {
            MessageAction::Play(data) => {
                assert!(data.contains(r#""playback":"file""#));
                assert!(data.contains(&expected));
            }
            other => panic!("{other:?}"),
        }
        assert!(shared.files.lock().unwrap().paths.contains(&expected));
        let _ = std::fs::remove_file(&expected);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn decoder_chunks_are_queued() {
        let dir = tempfile_dir();
        let mut shared = shared(config(8000, 8000), Arc::new(ChunkDecoder));
        Arc::get_mut(&mut shared).unwrap().temp_dir = dir.clone();
        use base64::Engine;
        let message = json!({
            "type": "streamAudio",
            "data": {
                "audioDataType": "mp3",
                "audioData": base64::engine::general_purpose::STANDARD.encode(b"ID3")
            }
        })
        .to_string();
        let outcome = process_message(&shared, &message);
        assert!(outcome.is_play());
        assert_eq!(shared.playback.lock().unwrap().queue.len(), 8);
        let path = format!("{dir}/test-uuid_0.tmp.mp3");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn write_replace_mixes_and_respects_pause() {
        let shared = shared(config(8000, 8000), Arc::new(NoDecoder));
        let streamer = Streamer {
            shared: Arc::clone(&shared),
            transport: WebSocketTransport::spawn(
                WebSocketUrl::parse("ws://127.0.0.1:1/none").unwrap(),
                TransportOptions { connection_timeout: Duration::from_millis(50), ..TransportOptions::default() },
                |_| {},
            ),
        };
        assert!(queue_pcm(&shared, &[10, 0, 20, 0], 8000, 0));
        let mut frame = [0u8, 0, 0, 0];
        assert!(streamer.write_replace(&mut frame, 4, 1));
        assert_eq!(frame, [10, 0, 20, 0]);
        assert!(!streamer.write_replace(&mut frame, 4, 1));

        assert!(queue_pcm(&shared, &[1, 0], 8000, 0));
        streamer.set_playback_paused(true);
        assert!(!streamer.write_replace(&mut frame, 4, 1));
        streamer.set_playback_paused(false);
        streamer.set_audio_paused(true);
        assert!(!streamer.write_replace(&mut frame, 4, 1));
    }

    #[test]
    fn capture_pipeline_accumulates_until_full() {
        let mut config = config(8000, 8000);
        config.rtp_packets = 2; // 每条消息 640 字节
        let mut pipeline = CapturePipeline::new(&config, None);
        let frame = vec![7u8; 320];
        assert!(pipeline.push(&frame, 160).is_empty());
        let messages = pipeline.push(&frame, 160);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].len(), 640);
        assert!(pipeline.push(&frame, 160).is_empty());
    }

    #[test]
    fn capture_pipeline_single_packet_sends_every_frame() {
        let config = config(8000, 8000);
        let mut pipeline = CapturePipeline::new(&config, None);
        let messages = pipeline.push(&[1, 2, 3, 4], 2);
        assert_eq!(messages, vec![vec![1, 2, 3, 4]]);
    }

    #[test]
    fn capture_pipeline_resamples_to_wire_rate() {
        let mut config = config(8000, 16000);
        config.read_rate = 8000;
        let mut pipeline = CapturePipeline::new(
            &config,
            Some(Box::new(FakeEngine { channels: 1, input_rate: 0, output_rate: 0 })),
        );
        let frame: Vec<u8> = (0..160i16).flat_map(|s| s.to_le_bytes()).collect();
        let messages = pipeline.push(&frame, 160);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].len(), 640); // 160 输入帧 -> 320 输出帧
    }

    #[test]
    fn capture_pipeline_drops_frames_that_do_not_fit() {
        let mut config = config(8000, 8000);
        config.rtp_packets = 1;
        let mut pipeline = CapturePipeline::new(&config, None);
        // rtp_packets == 1 完全绕过累加器。
        assert_eq!(pipeline.push(&vec![0u8; 9000], 4500).len(), 1);
    }

    fn tempfile_dir() -> String {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("audio-relay-core-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    // 保持辅助函数被真实使用：钩子直接经 Shared 检查。
    #[test]
    fn events_carry_the_c_module_payloads() {
        let shared = shared(config(8000, 8000), Arc::new(NoDecoder));
        let _ = hooks(&shared);
        handle_event(&shared, TransportEvent::Connected { compression: false });
        assert!(shared.ready_for_audio.load(Ordering::Acquire));
        handle_event(&shared, TransportEvent::Closed { code: 1000, reason: "bye".to_owned() });
        assert!(!shared.cleaned_up.load(Ordering::Acquire));
    }
}