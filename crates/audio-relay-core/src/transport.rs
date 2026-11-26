use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread::{JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::server::ParsedCertificate;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::deflate::Deflater;
use crate::error::{ErrorCode, StreamError};
use crate::frame::{
    close_payload, encode_frame, Incoming, MessageAssembler, OP_BINARY, OP_CLOSE, OP_PING, OP_PONG,
    OP_TEXT,
};
use crate::handshake;
use crate::url::WebSocketUrl;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

/// 宿主日志桥；FreeSWITCH crate 将其映射到 `switch_log_printf`。
pub type LogSink = Arc<dyn Fn(LogLevel, &str) + Send + Sync>;

/// TLS 信任配置，镜像 `WebSocketTLSOptions`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsOptions {
    /// `SYSTEM`（默认）使用操作系统信任库，`NONE` 关闭对端校验，
    /// 其他值视为 PEM CA 包路径。
    pub ca_file: Option<String>,
    pub key_file: Option<String>,
    pub cert_file: Option<String>,
    pub disable_hostname_validation: bool,
}

impl Default for TlsOptions {
    fn default() -> Self {
        Self {
            ca_file: Some("SYSTEM".to_owned()),
            key_file: None,
            cert_file: None,
            disable_hostname_validation: false,
        }
    }
}

impl TlsOptions {
    pub fn peer_verify_disabled(&self) -> bool {
        self.ca_file.as_deref() == Some("NONE")
    }

    pub fn using_system_ca(&self) -> bool {
        matches!(self.ca_file.as_deref(), None | Some("") | Some("SYSTEM"))
    }

    pub fn has_cert_and_key(&self) -> bool {
        self.cert_file.as_deref().is_some_and(|value| !value.is_empty())
            && self.key_file.as_deref().is_some_and(|value| !value.is_empty())
    }
}

#[derive(Clone)]
pub struct TransportOptions {
    pub tls: TlsOptions,
    pub compression_requested: bool,
    pub heart_beat_seconds: u32,
    /// TCP 连接、TLS 握手与 HTTP 升级的总时限。
    pub connection_timeout: Duration,
    pub extra_headers: Vec<(String, String)>,
    /// 升级完成后发送的第一条文本消息，先于 Connected 事件。
    pub initial_metadata: Option<String>,
    /// 有界出站队列；超出容量的消息被丢弃并计数。
    pub outbound_capacity: usize,
    pub log: Option<LogSink>,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self {
            tls: TlsOptions::default(),
            compression_requested: true,
            heart_beat_seconds: 0,
            connection_timeout: Duration::from_secs(1),
            extra_headers: Vec::new(),
            initial_metadata: None,
            outbound_capacity: 256,
            log: None,
        }
    }
}

/// 调用方推向服务器的消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
    Abort,
}

/// 在传输线程上按连接顺序投递的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    Connected { compression: bool },
    Text(String),
    Binary(Vec<u8>),
    Error(StreamError),
    Closed { code: u16, reason: String },
}

/// 运行中 WebSocket 连接的句柄。
///
/// 一个 OS 线程独占 socket；生产端只接触有界队列与唤醒管道，
/// 因此 media bug 线程永远不会阻塞在网络 I/O 上。
pub struct WebSocketTransport {
    outbound: SyncSender<Outbound>,
    waker: Arc<Waker>,
    dropped: Arc<AtomicU64>,
    connected: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    thread_id: ThreadId,
}

impl WebSocketTransport {
    pub fn spawn<F>(url: WebSocketUrl, options: TransportOptions, mut on_event: F) -> Self
    where
        F: FnMut(TransportEvent) + Send + 'static,
    {
        let capacity = options.outbound_capacity.max(1);
        let (outbound, receiver) = sync_channel(capacity);
        let waker = Arc::new(Waker::new().expect("cannot create transport wakeup pipe"));
        let dropped = Arc::new(AtomicU64::new(0));
        let connected = Arc::new(AtomicBool::new(false));
        let thread_waker = Arc::clone(&waker);
        let thread_connected = Arc::clone(&connected);
        let thread = std::thread::Builder::new()
            .name("audio-relay-ws".to_owned())
            .spawn(move || {
                run(url, options, receiver, thread_waker, thread_connected, &mut on_event);
            })
            .expect("cannot spawn transport thread");
        let thread_id = thread.thread().id();
        Self { outbound, waker, dropped, connected, thread: Some(thread), thread_id }
    }

    /// 升级完成到连接断开之间为 true，是允许发送消息的窗口。
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// 将消息入队。连接已断开或有界队列已满时返回 false，此时消息被丢弃。
    pub fn send(&self, message: Outbound) -> bool {
        match self.outbound.try_send(message) {
            Ok(()) => {
                self.waker.wake();
                true
            }
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn send_text(&self, text: &str) -> bool {
        self.send(Outbound::Text(text.to_owned()))
    }

    pub fn send_binary(&self, payload: &[u8]) -> bool {
        self.send(Outbound::Binary(payload.to_vec()))
    }

    pub fn close(&self, code: u16, reason: &str) -> bool {
        self.send(Outbound::Close(code, reason.to_owned()))
    }

    pub fn abort(&self) -> bool {
        self.send(Outbound::Abort)
    }

    pub fn dropped_messages(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 等待传输线程结束。当从传输线程自身调用（回调可能释放最后一个
    /// `Streamer` 引用）时，改为分离句柄而不是自连接死锁；`Drop`
    /// 排队的 abort 仍会结束循环。
    pub fn join(&mut self) {
        if std::thread::current().id() == self.thread_id {
            self.thread = None;
            return;
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WebSocketTransport {
    fn drop(&mut self) {
        let _ = self.outbound.try_send(Outbound::Abort);
        self.waker.wake();
        self.join();
    }
}

/// 生产端入队消息时中断 poll 循环的自管道。
struct Waker {
    read_fd: RawFd,
    write_fd: RawFd,
}

impl Waker {
    fn new() -> io::Result<Self> {
        let mut fds = [0 as RawFd; 2];
                // SAFETY: `fds` 是两个描述符的可写存储。
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            set_nonblocking_cloexec(fd)?;
        }
        Ok(Self { read_fd: fds[0], write_fd: fds[1] })
    }

    fn wake(&self) {
        let byte = 1u8;
                // SAFETY: `write_fd` 已打开，`byte` 在调用期间存活。
        unsafe { libc::write(self.write_fd, (&byte as *const u8).cast(), 1) };
    }

    fn drain(&self) {
        let mut buffer = [0u8; 64];
        loop {
                        // SAFETY: `read_fd` 已打开，`buffer` 在 `len` 字节内可写。
            let count = unsafe { libc::read(self.read_fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count <= 0 {
                break;
            }
        }
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
                // SAFETY: 两个描述符均由本对象持有，且只关闭一次。
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

fn set_nonblocking_cloexec(fd: RawFd) -> io::Result<()> {
        // SAFETY: `fd` 是有效的打开描述符。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: `fd` 是有效的打开描述符。
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
        // SAFETY: `fd` 是有效的打开描述符。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        // SAFETY: `fd` 是有效的打开描述符。
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

enum Connection {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Connection {
    fn fd(&self) -> RawFd {
        match self {
            Self::Plain(stream) => stream.as_raw_fd(),
            Self::Tls(stream) => stream.get_ref().as_raw_fd(),
        }
    }

    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }

    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }

    /// 冲刷已缓冲的 TLS 密文并处理待处理的 TLS 记录。
    fn complete_io(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(_) => Ok(()),
            Self::Tls(stream) => stream.conn.complete_io(&mut stream.sock).map(|_| ()),
        }
    }

    fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
}

fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

fn log(options: &TransportOptions, level: LogLevel, message: &str) {
    if let Some(sink) = options.log.as_ref() {
        sink(level, message);
    }
}

fn run<F>(
    url: WebSocketUrl,
    options: TransportOptions,
    receiver: Receiver<Outbound>,
    waker: Arc<Waker>,
    connected: Arc<AtomicBool>,
    on_event: &mut F,
) where
    F: FnMut(TransportEvent) + Send + 'static,
{
    let deadline = Instant::now() + options.connection_timeout;
    let mut connection = match connect(&url, &options, deadline) {
        Ok(connection) => connection,
        Err(error) => {
            on_event(TransportEvent::Error(error));
            return;
        }
    };

    let key = handshake::generate_key();
    let request = match handshake::build_request(
        &url,
        &key,
        options.compression_requested,
        &options.extra_headers,
    ) {
        Ok(request) => request,
        Err(error) => {
            on_event(TransportEvent::Error(error));
            return;
        }
    };

    let mut response: Vec<u8> = Vec::new();
    let negotiated = match upgrade(&mut connection, &request, &key, &options, deadline, &mut response) {
        Ok(handshake) => handshake,
        Err(error) => {
            on_event(TransportEvent::Error(error));
            return;
        }
    };

    let mut assembler = MessageAssembler::new();
    let mut deflater: Option<Deflater> = None;
    if let Some(config) = negotiated.compression.clone() {
        match assembler.enable_compression(config.clone()) {
            Ok(()) => match Deflater::new(config) {
                Ok(created) => deflater = Some(created),
                Err(error) => {
                    log(&options, LogLevel::Error, &format!("compression init failed: {error}"));
                    on_event(TransportEvent::Error(StreamError::not_supported(
                        "Compression negotiation failed",
                    )));
                    return;
                }
            },
            Err(error) => {
                log(&options, LogLevel::Error, &format!("compression init failed: {error}"));
                on_event(TransportEvent::Error(StreamError::not_supported(
                    "Compression negotiation failed",
                )));
                return;
            }
        }
    }

    log(
        &options,
        LogLevel::Info,
        &format!(
            "connected to {} (compression={})",
            url.authority(),
            deflater.is_some()
        ),
    );
    connected.store(true, Ordering::Release);
    on_event(TransportEvent::Connected { compression: deflater.is_some() });

    let mut state = LoopState::default();

    // 服务器在升级头之后立即发来的内容。
    let leftover = response.split_off(negotiated.header_bytes);
    if !leftover.is_empty() {
        assembler.push(&leftover);
        process_messages(&mut assembler, &mut state, &mut deflater, &options, on_event);
    }
    loop {
        drain_outbound(&receiver, &mut state, &mut deflater, &options);
        if state.abort {
            break;
        }
        if let Err(error) = flush_output(&mut connection, &mut state) {
            emit_io_error(&options, connection.is_tls(), error, on_event);
            break;
        }
        if let Err(error) = connection.complete_io() {
            if !retryable(&error) {
                emit_io_error(&options, connection.is_tls(), error, on_event);
                break;
            }
        }

        if state.finished() {
            break;
        }

        let timeout = poll_timeout(&state, &options);
        let mut polls = [
            libc::pollfd { fd: connection.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: waker.read_fd, events: libc::POLLIN, revents: 0 },
        ];
                // SAFETY: `polls` 是两个描述符的有效存储。
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), 2, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            emit_io_error(&options, connection.is_tls(), error, on_event);
            break;
        }

        if polls[1].revents & libc::POLLIN != 0 {
            waker.drain();
        }

        if polls[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut buffer = [0u8; 16384];
            match connection.read(&mut buffer) {
                Ok(0) => {
                    if state.close_received {
                        on_event(TransportEvent::Closed { code: 1000, reason: "Normal closure".to_owned() });
                    } else {
                        on_event(TransportEvent::Error(StreamError::io(
                            "Connection closed without WebSocket CLOSE (abnormal EOF)",
                        )));
                    }
                    break;
                }
                Ok(count) => {
                    assembler.push(&buffer[..count]);
                    if !process_messages(&mut assembler, &mut state, &mut deflater, &options, on_event) {
                        // 已处理协议错误或关闭；冲刷回复。
                        let _ = flush_output(&mut connection, &mut state);
                        let _ = connection.complete_io();
                        if state.close_received || state.abort {
                            break;
                        }
                    }
                }
                Err(error) if retryable(&error) => {}
                Err(error) => {
                    emit_io_error(&options, connection.is_tls(), error, on_event);
                    break;
                }
            }
        }

        if polls[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            emit_io_error(
                &options,
                connection.is_tls(),
                io::Error::other("socket error"),
                on_event,
            );
            break;
        }

        if heartbeat_due(&mut state, &options) {
            if state.pings_outstanding >= MAX_MISSED_PONGS {
                on_event(TransportEvent::Error(StreamError::ping_timeout("Ping timeout (no pong)")));
                break;
            }
            state.output.extend(encode_frame(OP_PING, b"", true, false));
            state.pings_outstanding += 1;
        }

        if let Some(deadline) = state.close_deadline {
            if Instant::now() >= deadline && state.output.is_empty() {
                break;
            }
        }
    }

    connected.store(false, Ordering::Release);
    let _ = flush_output(&mut connection, &mut state);
    let _ = connection.complete_io();
    log(&options, LogLevel::Debug, "transport loop exited");
}

const MAX_MISSED_PONGS: u32 = 2;
const CLOSE_GRACE: Duration = Duration::from_millis(500);

#[derive(Default)]
struct LoopState {
    output: Vec<u8>,
    close_sent: bool,
    close_received: bool,
    abort: bool,
    protocol_failed: bool,
    pings_outstanding: u32,
    last_ping: Option<Instant>,
    close_deadline: Option<Instant>,
}

impl LoopState {
    fn finished(&self) -> bool {
        self.abort || (self.close_sent && self.close_received)
    }
}

fn drain_outbound(
    receiver: &Receiver<Outbound>,
    state: &mut LoopState,
    deflater: &mut Option<Deflater>,
    options: &TransportOptions,
) {
    loop {
        match receiver.try_recv() {
            Ok(Outbound::Text(text)) => {
                state.output.extend(encode_payload(OP_TEXT, text.as_bytes(), deflater));
            }
            Ok(Outbound::Binary(payload)) => {
                state.output.extend(encode_payload(OP_BINARY, &payload, deflater));
            }
            Ok(Outbound::Close(code, reason)) => {
                if !state.close_sent {
                    state.output.extend(encode_frame(OP_CLOSE, &close_payload(code, &reason), true, false));
                    state.close_sent = true;
                    state.close_deadline = Some(Instant::now() + CLOSE_GRACE);
                }
            }
            Ok(Outbound::Abort) => {
                state.abort = true;
                return;
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                state.abort = true;
                return;
            }
        }
        if state.output.len() > MAX_BUFFERED_OUTPUT {
            log(options, LogLevel::Warning, "outbound buffer grew beyond 4 MiB; peer is not reading");
        }
    }
}

/// 对端停止读取时的保险丝：队列虽有界，但单条消息仍可能很大。
const MAX_BUFFERED_OUTPUT: usize = 4 * 1024 * 1024;

fn encode_payload(opcode: u8, payload: &[u8], deflater: &mut Option<Deflater>) -> Vec<u8> {
    let (payload, compressed) = match deflater.as_mut() {
        Some(deflater) => match deflater.deflate(payload) {
            Some(deflated) => (deflated, true),
            None => (payload.to_vec(), false),
        },
        None => (payload.to_vec(), false),
    };
    encode_frame(opcode, &payload, true, compressed)
}

fn flush_output(connection: &mut Connection, state: &mut LoopState) -> io::Result<()> {
    let mut written = 0usize;
    while written < state.output.len() {
        match connection.write(&state.output[written..]) {
            Ok(0) => return Err(io::Error::other("socket write returned 0")),
            Ok(count) => written += count,
            Err(error) if retryable(&error) => break,
            Err(error) => return Err(error),
        }
    }
    if written > 0 {
        state.output.drain(..written);
    }
    Ok(())
}

fn process_messages<F>(
    assembler: &mut MessageAssembler,
    state: &mut LoopState,
    deflater: &mut Option<Deflater>,
    options: &TransportOptions,
    on_event: &mut F,
) -> bool
where
    F: FnMut(TransportEvent),
{
    loop {
        match assembler.next_message() {
            Ok(Some(Incoming::Text(text))) => on_event(TransportEvent::Text(text)),
            Ok(Some(Incoming::Binary(payload))) => on_event(TransportEvent::Binary(payload)),
            Ok(Some(Incoming::Ping(payload))) => {
                state.output.extend(encode_frame(OP_PONG, &payload, true, false));
            }
            Ok(Some(Incoming::Pong(_))) => state.pings_outstanding = 0,
            Ok(Some(Incoming::Close { code, reason })) => {
                state.close_received = true;
                if !state.close_sent {
                    state.output.extend(encode_frame(OP_CLOSE, &close_payload(code, &reason), true, false));
                    state.close_sent = true;
                }
                on_event(TransportEvent::Closed { code, reason });
                return false;
            }
            Ok(None) => return true,
            Err(error) => {
                if state.protocol_failed {
                    return false;
                }
                state.protocol_failed = true;
                let code = error.close_code();
                let message = if code == 1007 {
                    format!("WebSocket invalid UTF-8 (1007): {error}")
                } else {
                    format!("WebSocket protocol error ({code}): {error}")
                };
                log(options, LogLevel::Error, &message);
                on_event(TransportEvent::Error(StreamError::protocol(message.clone())));
                if !state.close_sent {
                    state.output.extend(encode_frame(OP_CLOSE, &close_payload(code, ""), true, false));
                    state.close_sent = true;
                    state.close_deadline = Some(Instant::now() + CLOSE_GRACE);
                }
                let _ = deflater.take();
                return false;
            }
        }
    }
}

fn heartbeat_due(state: &mut LoopState, options: &TransportOptions) -> bool {
    if options.heart_beat_seconds == 0 {
        return false;
    }
    let interval = Duration::from_secs(options.heart_beat_seconds as u64);
    match state.last_ping {
        Some(last) if last.elapsed() < interval => false,
        _ => {
            state.last_ping = Some(Instant::now());
            true
        }
    }
}

fn poll_timeout(state: &LoopState, options: &TransportOptions) -> i32 {
    let mut timeout = Duration::from_secs(1);
    if options.heart_beat_seconds > 0 {
        let interval = Duration::from_secs(options.heart_beat_seconds as u64);
        let elapsed = state.last_ping.map(|last| last.elapsed()).unwrap_or(interval);
        timeout = timeout.min(interval.saturating_sub(elapsed));
    }
    if let Some(deadline) = state.close_deadline {
        timeout = timeout.min(deadline.saturating_duration_since(Instant::now()));
    }
    if !state.output.is_empty() {
        timeout = Duration::ZERO;
    }
    timeout.as_millis().min(i32::MAX as u128) as i32
}

fn emit_io_error<F>(options: &TransportOptions, tls: bool, error: io::Error, on_event: &mut F)
where
    F: FnMut(TransportEvent),
{
    let message = format!("{error}");
    log(options, LogLevel::Error, &message);
    let code = if tls { ErrorCode::SslError } else { ErrorCode::Io };
    on_event(TransportEvent::Error(StreamError::new(code, message)));
}

fn connect(url: &WebSocketUrl, options: &TransportOptions, deadline: Instant) -> Result<Connection, StreamError> {
    let addresses = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|error| StreamError::connect_failed(format!("Failed to resolve host: {error}")))?;
    let mut last_error = None;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(StreamError::timeout("Connection/handshake timeout"));
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => {
                stream
                    .set_nonblocking(true)
                    .map_err(|error| StreamError::connect_failed(format!("{error}")))?;
                let _ = stream.set_nodelay(true);
                if !url.secure {
                    return Ok(Connection::Plain(stream));
                }
                return tls_connect(stream, url, options, deadline);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(match last_error {
        Some(error) if error.kind() == io::ErrorKind::TimedOut => {
            StreamError::timeout("Connection/handshake timeout")
        }
        Some(error) => StreamError::connect_failed(format!("Failed to start connection: {error}")),
        None => StreamError::connect_failed("Failed to start connection"),
    })
}

fn tls_connect(
    stream: TcpStream,
    url: &WebSocketUrl,
    options: &TransportOptions,
    deadline: Instant,
) -> Result<Connection, StreamError> {
    let config = build_tls_config(&options.tls)?;
    let server_name = if url.ip_literal {
        url.host
            .parse::<std::net::IpAddr>()
            .map(ServerName::from)
            .map_err(|error| StreamError::tls_init_failed(format!("invalid IP host: {error}")))?
    } else {
        ServerName::try_from(url.host.clone())
            .map_err(|error| StreamError::tls_init_failed(format!("invalid server name: {error}")))?
    };
    let connection = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|error| StreamError::tls_init_failed(format!("{error}")))?;
    let mut tls = Box::new(rustls::StreamOwned::new(connection, stream));
    loop {
        match tls.conn.complete_io(&mut tls.sock) {
            Ok(_) => break,
            Err(error) if retryable(&error) => {
                if Instant::now() >= deadline {
                    return Err(StreamError::timeout("Connection/handshake timeout"));
                }
                let mut poll = libc::pollfd {
                    fd: tls.get_ref().as_raw_fd(),
                    events: libc::POLLIN | libc::POLLOUT,
                    revents: 0,
                };
                                // SAFETY: `poll` 描述存活的 TLS socket fd。
                unsafe { libc::poll(&mut poll, 1, 20) };
            }
            Err(error) => {
                return Err(StreamError::handshake_failed(format!("TLS handshake failed: {error}")));
            }
        }
    }
    Ok(Connection::Tls(tls))
}

fn upgrade(
    connection: &mut Connection,
    request: &[u8],
    key: &str,
    options: &TransportOptions,
    deadline: Instant,
    response: &mut Vec<u8>,
) -> Result<handshake::Handshake, StreamError> {
    let mut written = 0usize;
    while written < request.len() {
        match connection.write(&request[written..]) {
            Ok(count) => written += count,
            Err(error) if retryable(&error) => {
                if Instant::now() >= deadline {
                    return Err(StreamError::timeout("Connection/handshake timeout"));
                }
                let _ = connection.complete_io();
                wait_readable(connection, 20);
            }
            Err(error) => {
                return Err(StreamError::connect_failed(format!("{error}")));
            }
        }
        let _ = connection.complete_io();
    }

    let mut buffer = [0u8; 8192];
    loop {
        match handshake::parse_response(response, key, options.compression_requested) {
            Ok(Some(handshake)) => return Ok(handshake),
            Ok(None) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(StreamError::timeout("Connection/handshake timeout"));
        }
        match connection.read(&mut buffer) {
            Ok(0) => {
                return Err(StreamError::connect_failed(
                    "Connection closed during handshake (EOF)",
                ));
            }
            Ok(count) => response.extend_from_slice(&buffer[..count]),
            Err(error) if retryable(&error) => {
                let _ = connection.complete_io();
                wait_readable(connection, 20);
            }
            Err(error) => return Err(StreamError::connect_failed(format!("{error}"))),
        }
    }
}

fn wait_readable(connection: &Connection, millis: i32) {
    let mut poll = libc::pollfd { fd: connection.fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: `poll` 描述存活的 socket fd。
    unsafe { libc::poll(&mut poll, 1, millis) };
}

fn build_tls_config(options: &TlsOptions) -> Result<ClientConfig, StreamError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| StreamError::tls_init_failed(format!("{error}")))?;

    let builder = if options.peer_verify_disabled() {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification::new()))
    } else {
        let mut roots = RootCertStore::empty();
        if options.using_system_ca() {
            let result = rustls_native_certs::load_native_certs();
            for error in &result.errors {
                log_default(LogLevel::Warning, &format!("cannot load a system certificate: {error}"));
            }
            let (added, ignored) = roots.add_parsable_certificates(result.certs);
            if added == 0 {
                return Err(StreamError::tls_init_failed("no usable system CA certificates"));
            }
            let _ = ignored;
        } else {
            let path = options.ca_file.as_deref().unwrap_or_default();
            let pem = std::fs::read(path)
                .map_err(|error| StreamError::tls_init_failed(format!("cannot read CA file {path}: {error}")))?;
            let mut cursor = io::Cursor::new(pem);
            let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cursor)
                .collect::<Result<_, _>>()
                .map_err(|error| StreamError::tls_init_failed(format!("invalid CA file {path}: {error}")))?;
            if certs.is_empty() {
                return Err(StreamError::tls_init_failed(format!("CA file {path} contains no certificates")));
            }
            roots.add_parsable_certificates(certs);
        }
        if options.disable_hostname_validation {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoHostnameVerification::new(roots)))
        } else {
            builder.with_root_certificates(roots)
        }
    };

    let config = if options.has_cert_and_key() {
        let cert_path = options.cert_file.as_deref().unwrap_or_default();
        let key_path = options.key_file.as_deref().unwrap_or_default();
        let cert_pem = std::fs::read(cert_path)
            .map_err(|error| StreamError::tls_init_failed(format!("cannot read certificate {cert_path}: {error}")))?;
        let key_pem = std::fs::read(key_path)
            .map_err(|error| StreamError::tls_init_failed(format!("cannot read key {key_path}: {error}")))?;
        let chain: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut io::Cursor::new(cert_pem))
                .collect::<Result<_, _>>()
                .map_err(|error| StreamError::tls_init_failed(format!("invalid certificate: {error}")))?;
        let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut io::Cursor::new(key_pem))
            .map_err(|error| StreamError::tls_init_failed(format!("invalid key: {error}")))?
            .ok_or_else(|| StreamError::tls_init_failed("key file contains no private key"))?;
        builder
            .with_client_auth_cert(chain, key)
            .map_err(|error| StreamError::tls_init_failed(format!("client certificate rejected: {error}")))?
    } else {
        builder.with_no_client_auth()
    };
    Ok(config)
}

fn log_default(level: LogLevel, message: &str) {
    eprintln!("[audio-relay] {level:?}: {message}");
}

/// `STREAM_TLS_CA_FILE=NONE`：接受任意证书。
#[derive(Debug)]
struct NoVerification {
    algs: WebPkiSupportedAlgorithms,
}

impl NoVerification {
    fn new() -> Self {
        Self {
            algs: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// `STREAM_TLS_DISABLE_HOSTNAME_VALIDATION=true`：校验证书链但跳过
/// 主机名校验。
#[derive(Debug)]
struct NoHostnameVerification {
    roots: Arc<RootCertStore>,
    algs: WebPkiSupportedAlgorithms,
}

impl NoHostnameVerification {
    fn new(roots: RootCertStore) -> Self {
        Self {
            roots: Arc::new(roots),
            algs: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for NoHostnameVerification {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)
            .map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.algs.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// 升级完成前由生产端排队的帧暂存在这里，镜像 libwsc 的待发送队列。
#[derive(Default)]
pub struct PendingQueue {
    messages: VecDeque<Outbound>,
}

impl PendingQueue {
    pub fn push(&mut self, message: Outbound) {
        self.messages.push_back(message);
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Outbound> + '_ {
        self.messages.drain(..)
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::DeflateConfig;
    use std::sync::mpsc::{channel, Receiver as StdReceiver};

    /// 测试用最小服务器：完成升级后把数据帧回显给客户端。
    struct TestServer {
        address: std::net::SocketAddr,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start(compression: bool, script: ServerScript) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let thread = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let key = text
                    .lines()
                    .find_map(|line| line.strip_prefix("Sec-WebSocket-Key:"))
                    .unwrap()
                    .trim()
                    .to_owned();
                let mut response = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {}\r\n",
                    handshake::expected_accept(&key)
                );
                if compression && text.contains("permessage-deflate") {
                    response.push_str(
                        "Sec-WebSocket-Extensions: permessage-deflate; client_no_context_takeover; server_no_context_takeover\r\n",
                    );
                }
                response.push_str("\r\n");
                stream.write_all(response.as_bytes()).unwrap();
                script.run(&mut stream, compression);
            });
            Self { address, thread: Some(thread) }
        }

        fn url(&self, path: &str) -> WebSocketUrl {
            WebSocketUrl::parse(&format!("ws://{}{path}", self.address)).unwrap()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    enum ServerScript {
        Echo,
        CloseAfterHello,
        SendInvalidUtf8,
        IgnorePings,
        DropTcp,
    }

    impl ServerScript {
        fn run(self, stream: &mut TcpStream, compression: bool) {
            let mut deflater = if compression {
                Some(Deflater::new(DeflateConfig {
                    client_no_context_takeover: true,
                    server_no_context_takeover: true,
                    ..Default::default()
                }).unwrap())
            } else {
                None
            };
            let mut assembler = MessageAssembler::for_server();
            if compression {
                assembler.enable_compression(DeflateConfig {
                    client_no_context_takeover: true,
                    server_no_context_takeover: true,
                    ..Default::default()
                }).unwrap();
            }
            let mut buffer = [0u8; 8192];
            let mut pings = 0;
            match self {
                ServerScript::SendInvalidUtf8 => {
                    let _ = stream.write_all(&server_frame(OP_TEXT, &[0xFF, 0xFE], true, false));
                    return;
                }
                ServerScript::DropTcp => return,
                _ => {}
            }
            loop {
                let count = match stream.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(count) => count,
                    Err(_) => return,
                };
                assembler.push(&buffer[..count]);
                while let Ok(Some(message)) = assembler.next_message() {
                    match message {
                        Incoming::Text(text) => {
                            if matches!(self, ServerScript::CloseAfterHello) {
                                let _ = stream.write_all(&server_frame(OP_CLOSE, &close_payload(1000, "bye"), true, false));
                                return;
                            }
                            let payload = encode_server(OP_TEXT, text.as_bytes(), &mut deflater);
                            let _ = stream.write_all(&payload);
                        }
                        Incoming::Binary(payload) => {
                            let encoded = encode_server(OP_BINARY, &payload, &mut deflater);
                            let _ = stream.write_all(&encoded);
                        }
                        Incoming::Ping(payload) => {
                            pings += 1;
                            if matches!(self, ServerScript::IgnorePings) {
                                if pings >= 4 {
                                    return;
                                }
                                continue;
                            }
                            let _ = stream.write_all(&server_frame(OP_PONG, &payload, true, false));
                        }
                        Incoming::Close { .. } => {
                            let _ = stream.write_all(&server_frame(OP_CLOSE, &close_payload(1000, ""), true, false));
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn encode_server(opcode: u8, payload: &[u8], deflater: &mut Option<Deflater>) -> Vec<u8> {
        let (payload, compressed) = match deflater.as_mut() {
            Some(deflater) => match deflater.deflate(payload) {
                Some(compressed) => (compressed, true),
                None => (payload.to_vec(), false),
            },
            None => (payload.to_vec(), false),
        };
        server_frame(opcode, &payload, true, compressed)
    }

    fn server_frame(opcode: u8, payload: &[u8], fin: bool, rsv1: bool) -> Vec<u8> {
        let mut frame = Vec::new();
        let mut first = opcode;
        if fin {
            first |= 0x80;
        }
        if rsv1 {
            first |= 0x40;
        }
        frame.push(first);
        if payload.len() < 126 {
            frame.push(payload.len() as u8);
        } else {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        frame
    }

    fn collect(events: &StdReceiver<TransportEvent>, count: usize) -> Vec<TransportEvent> {
        let mut collected = Vec::new();
        for _ in 0..count {
            match events.recv_timeout(Duration::from_secs(5)) {
                Ok(event) => collected.push(event),
                Err(_) => break,
            }
        }
        collected
    }

    fn options() -> TransportOptions {
        TransportOptions { connection_timeout: Duration::from_secs(5), ..Default::default() }
    }

    #[test]
    fn text_and_binary_round_trip() {
        let server = TestServer::start(false, ServerScript::Echo);
        let (sender, receiver) = channel();
        let mut transport = WebSocketTransport::spawn(server.url("/audio"), options(), move |event| {
            let _ = sender.send(event);
        });
        assert_eq!(collect(&receiver, 1), vec![TransportEvent::Connected { compression: false }]);

        assert!(transport.send_text("hello"));
        assert!(transport.send_binary(&[1, 2, 3, 4]));
        assert_eq!(
            collect(&receiver, 2),
            vec![
                TransportEvent::Text("hello".to_owned()),
                TransportEvent::Binary(vec![1, 2, 3, 4])
            ]
        );
        transport.abort();
        transport.join();
    }

    #[test]
    fn compressed_round_trip() {
        let server = TestServer::start(true, ServerScript::Echo);
        let (sender, receiver) = channel();
        let mut transport = WebSocketTransport::spawn(server.url("/audio"), options(), move |event| {
            let _ = sender.send(event);
        });
        assert_eq!(collect(&receiver, 1), vec![TransportEvent::Connected { compression: true }]);
        let payload: Vec<u8> = (0..4096u32).map(|index| (index % 256) as u8).collect();
        assert!(transport.send_binary(&payload));
        assert_eq!(collect(&receiver, 1), vec![TransportEvent::Binary(payload)]);
        transport.abort();
        transport.join();
    }

    #[test]
    fn server_close_is_reported_and_echoed() {
        let server = TestServer::start(false, ServerScript::CloseAfterHello);
        let (sender, receiver) = channel();
        let mut transport = WebSocketTransport::spawn(server.url("/audio"), options(), move |event| {
            let _ = sender.send(event);
        });
        collect(&receiver, 1);
        transport.send_text("hello");
        let events = collect(&receiver, 1);
        assert_eq!(events, vec![TransportEvent::Closed { code: 1000, reason: "bye".to_owned() }]);
        transport.join();
    }

    #[test]
    fn protocol_errors_use_code_11() {
        let server = TestServer::start(false, ServerScript::SendInvalidUtf8);
        let (sender, receiver) = channel();
        let mut transport = WebSocketTransport::spawn(server.url("/audio"), options(), move |event| {
            let _ = sender.send(event);
        });
        collect(&receiver, 1);
        let events = collect(&receiver, 1);
        match &events[0] {
            TransportEvent::Error(error) => {
                assert_eq!(error.code, ErrorCode::Protocol);
                assert!(error.message.contains("1007"), "{}", error.message);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        transport.join();
    }

    #[test]
    fn abnormal_eof_reports_io_error() {
        let server = TestServer::start(false, ServerScript::DropTcp);
        let (sender, receiver) = channel();
        let mut transport = WebSocketTransport::spawn(server.url("/audio"), options(), move |event| {
            let _ = sender.send(event);
        });
        collect(&receiver, 1);
        transport.send_text("hello");
        let events = collect(&receiver, 1);
        match &events[0] {
            TransportEvent::Error(error) => assert_eq!(error.code, ErrorCode::Io),
            other => panic!("unexpected event: {other:?}"),
        }
        transport.join();
    }

    #[test]
    fn unanswered_pings_time_out() {
        let server = TestServer::start(false, ServerScript::IgnorePings);
        let (sender, receiver) = channel();
        let options = TransportOptions { heart_beat_seconds: 1, ..options() };
        let mut transport = WebSocketTransport::spawn(server.url("/audio"), options, move |event| {
            let _ = sender.send(event);
        });
        collect(&receiver, 1);
        let events = collect(&receiver, 1);
        match &events[0] {
            TransportEvent::Error(error) => assert_eq!(error.code, ErrorCode::PingTimeout),
            other => panic!("unexpected event: {other:?}"),
        }
        transport.join();
    }

    #[test]
    fn refused_upgrade_reports_connect_failed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n");
        });
        let (sender, receiver) = channel();
        let url = WebSocketUrl::parse(&format!("ws://{address}/x")).unwrap();
        let mut transport = WebSocketTransport::spawn(url, options(), move |event| {
            let _ = sender.send(event);
        });
        let events = collect(&receiver, 1);
        match &events[0] {
            TransportEvent::Error(error) => assert_eq!(error.code, ErrorCode::ConnectFailed),
            other => panic!("unexpected event: {other:?}"),
        }
        transport.join();
        let _ = thread.join();
    }

    #[test]
    fn unreachable_host_times_out() {
        let (sender, receiver) = channel();
        // TEST-NET-1 地址：连接被丢弃而非拒绝。
        let url = WebSocketUrl::parse("ws://192.0.2.1:9/x").unwrap();
        let options = TransportOptions { connection_timeout: Duration::from_millis(300), ..Default::default() };
        let mut transport = WebSocketTransport::spawn(url, options, move |event| {
            let _ = sender.send(event);
        });
        let events = collect(&receiver, 1);
        match &events[0] {
            TransportEvent::Error(error) => assert!(
                error.code == ErrorCode::Timeout || error.code == ErrorCode::ConnectFailed,
                "{error:?}"
            ),
            other => panic!("unexpected event: {other:?}"),
        }
        transport.join();
    }

    #[test]
    fn sends_after_shutdown_are_rejected() {
        let server = TestServer::start(false, ServerScript::Echo);
        let (sender, receiver) = channel();
        let mut transport = WebSocketTransport::spawn(server.url("/audio"), options(), move |event| {
            let _ = sender.send(event);
        });
        collect(&receiver, 1);
        transport.abort();
        transport.join();
        assert!(!transport.send_text("late"));
    }
}
