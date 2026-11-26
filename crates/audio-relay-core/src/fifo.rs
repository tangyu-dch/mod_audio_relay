use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// 可选的命名管道播放音频侧信道。
///
/// 字节在到达本类型前由调用方完成重采样。未挂接读者时音频会被缓冲
/// （最多 64 KiB，丢弃最旧数据）而不是阻塞 media bug 线程。
pub struct PlaybackFifo {
    path: Option<PathBuf>,
    fd: i32,
    pending: Vec<u8>,
}

/// 未挂接读者时保留的最大音频量。
pub const MAX_PENDING_BYTES: usize = 65536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackFifoError {
    CreateFailed(String),
    NotAFifo(String),
    OpenFailed(String),
    WriteFailed(String),
    OddLength,
}

impl std::fmt::Display for PlaybackFifoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateFailed(path) => write!(formatter, "cannot create FIFO {path}"),
            Self::NotAFifo(path) => write!(formatter, "{path} is not a FIFO"),
            Self::OpenFailed(path) => write!(formatter, "cannot open FIFO {path}"),
            Self::WriteFailed(path) => write!(formatter, "cannot write FIFO {path}"),
            Self::OddLength => formatter.write_str("FIFO payload has an odd byte count"),
        }
    }
}

impl Default for PlaybackFifo {
    fn default() -> Self {
        Self { path: None, fd: -1, pending: Vec::new() }
    }
}

impl Drop for PlaybackFifo {
    fn drop(&mut self) {
        self.close_pipe();
    }
}

impl PlaybackFifo {
    pub fn new() -> Self {
        Self::default()
    }

    /// 路径不存在时创建 FIFO；已存在的路径会校验确实是 FIFO。
    /// 空路径表示禁用该侧信道。
    pub fn configure(&mut self, path: &str) -> Result<(), PlaybackFifoError> {
        self.close_pipe();
        self.pending.clear();
        self.path = None;
        if path.is_empty() {
            return Ok(());
        }

        let c_path = CString::new(path).map_err(|_| PlaybackFifoError::NotAFifo(path.to_owned()))?;
                // SAFETY: `libc::stat` 是普通 C 结构体，清零即为合法值。
        let mut info: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: `c_path` 是存活的 C 字符串，`info` 是可写的 stat 存储。
        if unsafe { libc::lstat(c_path.as_ptr(), &mut info) } != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno != libc::ENOENT
                                // SAFETY: `c_path` 是存活的 C 字符串。
                || unsafe { libc::mkfifo(c_path.as_ptr(), 0o600 as libc::mode_t) } != 0
            {
                return Err(PlaybackFifoError::CreateFailed(path.to_owned()));
            }
                        // SAFETY: `c_path` 是存活的 C 字符串，`info` 是可写的 stat 存储。
            if unsafe { libc::lstat(c_path.as_ptr(), &mut info) } != 0 {
                return Err(PlaybackFifoError::CreateFailed(path.to_owned()));
            }
        }
        if info.st_mode & libc::S_IFMT != libc::S_IFIFO {
            return Err(PlaybackFifoError::NotAFifo(path.to_owned()));
        }

        self.path = Some(PathBuf::from(path));
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.path.is_some()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn pending_bytes(&self) -> usize {
        self.pending.len()
    }

    pub fn write_pcm(&mut self, pcm: &[u8]) -> Result<(), PlaybackFifoError> {
        if !self.enabled() || pcm.is_empty() {
            return Ok(());
        }
        if pcm.len() & 1 != 0 {
            return Err(PlaybackFifoError::OddLength);
        }

        self.pending.extend_from_slice(pcm);
        if self.pending.len() > MAX_PENDING_BYTES {
            let drop = self.pending.len() - MAX_PENDING_BYTES;
            self.pending.drain(..drop);
            if self.pending.len() & 1 != 0 {
                self.pending.remove(0);
            }
        }

        match self.open_pipe() {
            PipeState::Ready => self.flush_pending(),
            // 尚未挂接读者：音频保持缓冲。
            PipeState::NoReader => Ok(()),
            PipeState::Failed => Err(PlaybackFifoError::OpenFailed(self.describe_path())),
        }
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    fn describe_path(&self) -> String {
        self.path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    }

    fn open_pipe(&mut self) -> PipeState {
        if self.fd >= 0 {
            return PipeState::Ready;
        }
        let path = match self.path.as_ref().and_then(|path| CString::new(path.as_os_str().as_bytes()).ok()) {
            Some(path) => path,
            None => return PipeState::Failed,
        };
                // SAFETY: `path` 是存活的 C 字符串。
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd >= 0 {
                        // SAFETY: `info` 是可写的 stat 存储。
            let mut info: libc::stat = unsafe { std::mem::zeroed() };
                        // SAFETY: `fd` 已打开，`info` 是可写的 stat 存储。
            if unsafe { libc::fstat(fd, &mut info) } == 0 && info.st_mode & libc::S_IFMT == libc::S_IFIFO {
                self.fd = fd;
                return PipeState::Ready;
            }
                        // SAFETY: `fd` 在上方已打开，这里只关闭一次。
            unsafe { libc::close(fd) };
            return PipeState::Failed;
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::ENXIO {
            PipeState::NoReader
        } else {
            PipeState::Failed
        }
    }

    fn flush_pending(&mut self) -> Result<(), PlaybackFifoError> {
        // 读者断开不应向 FreeSWITCH 投递 SIGPIPE。
                // SAFETY: `blocked` 是可写的 sigset 存储。
        let mut blocked: libc::sigset_t = unsafe { std::mem::zeroed() };
                // SAFETY: `previous` 是可写的 sigset 存储。
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
                // SAFETY: `blocked` 是可写的 sigset 存储。
        unsafe {
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGPIPE);
        }
                // SAFETY: `blocked`/`previous` 是已初始化的 sigset 存储。
        if unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) } != 0 {
            return Err(PlaybackFifoError::WriteFailed(self.describe_path()));
        }
        let was_pending = sigpipe_pending();

        let mut written = 0usize;
        let mut success = true;
        while written < self.pending.len() {
                        // SAFETY: `self.pending` 与 `buffer` 在写入期间有效。
            let count = unsafe {
                libc::write(
                    self.fd,
                    self.pending[written..].as_ptr().cast(),
                    self.pending.len() - written,
                )
            };
            if count > 0 {
                written += count as usize;
                continue;
            }
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if count < 0 && errno == libc::EINTR {
                continue;
            }
            if count < 0 && (errno == libc::EAGAIN || errno == libc::EWOULDBLOCK) {
                break;
            }
            self.close_pipe();
            self.pending.clear();
            success = false;
            break;
        }
        if written > 0 {
            self.pending.drain(..written);
        }

        if !was_pending && sigpipe_pending() {
            // 信号已被阻塞且是线程定向的，sigwait 会立即返回并消费它，
            // 而不是杀死进程。
            let mut signal = 0i32;
                        // SAFETY: `blocked`/`signal` 是有效的可写存储。
            unsafe { libc::sigwait(&blocked, &mut signal) };
        }
                // SAFETY: `previous` 保存着 `pthread_sigmask` 保存的掩码。
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };

        if success {
            Ok(())
        } else {
            Err(PlaybackFifoError::WriteFailed(self.describe_path()))
        }
    }

    fn close_pipe(&mut self) {
        if self.fd >= 0 {
                        // SAFETY: `self.fd` 是已打开的描述符，这里只关闭一次。
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}

enum PipeState {
    Ready,
    NoReader,
    Failed,
}

fn sigpipe_pending() -> bool {
        // SAFETY: `pending` 是可写的 sigset 存储。
    let mut pending: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `pending` 是可写的 sigset 存储。
    unsafe {
        libc::sigpending(&mut pending) == 0 && libc::sigismember(&pending, libc::SIGPIPE) == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let unique = format!(
                "audio-relay-fifo-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn fifo(&self, name: &str) -> String {
            self.0.join(name).display().to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open_reader(path: &str) -> i32 {
        let c_path = CString::new(path).unwrap();
                // SAFETY: `c_path` 是存活的 C 字符串。
        unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) }
    }

    fn read_all(fd: i32) -> Vec<u8> {
        let mut buffer = [0u8; 4096];
        let mut collected = Vec::new();
        loop {
                        // SAFETY: `fd` 已打开，`buffer` 在 `len` 字节内可写。
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count <= 0 {
                break;
            }
            collected.extend_from_slice(&buffer[..count as usize]);
        }
        collected
    }

    #[test]
    fn empty_path_disables_the_channel() {
        let mut fifo = PlaybackFifo::new();
        fifo.configure("").unwrap();
        assert!(!fifo.enabled());
        assert!(fifo.write_pcm(&[1, 2]).is_ok());
    }

    #[test]
    fn configure_creates_a_fifo_and_reuses_it() {
        let dir = TempDir::new("create");
        let path = dir.fifo("pipe");
        let mut fifo = PlaybackFifo::new();
        fifo.configure(&path).unwrap();
        assert!(fifo.enabled());
        assert!(Path::new(&path).exists());
        fifo.configure(&path).unwrap();
        assert!(fifo.enabled());
    }

    #[test]
    fn regular_files_are_rejected() {
        let dir = TempDir::new("regular");
        let path = dir.fifo("file");
        std::fs::write(&path, b"not a fifo").unwrap();
        let mut fifo = PlaybackFifo::new();
        assert_eq!(fifo.configure(&path), Err(PlaybackFifoError::NotAFifo(path.clone())));
        assert!(!fifo.enabled());
    }

    #[test]
    fn audio_is_buffered_until_a_reader_attaches() {
        let dir = TempDir::new("buffer");
        let path = dir.fifo("pipe");
        let mut fifo = PlaybackFifo::new();
        fifo.configure(&path).unwrap();
        fifo.write_pcm(&[1, 2, 3, 4]).unwrap();
        assert_eq!(fifo.pending_bytes(), 4);

        let reader = open_reader(&path);
        assert!(reader >= 0);
        fifo.write_pcm(&[5, 6]).unwrap();
        assert_eq!(read_all(reader), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(fifo.pending_bytes(), 0);
                // SAFETY: `reader` 是测试持有的打开描述符。
        unsafe { libc::close(reader) };
    }

    #[test]
    fn pending_audio_is_capped_and_sample_aligned() {
        let dir = TempDir::new("cap");
        let path = dir.fifo("pipe");
        let mut fifo = PlaybackFifo::new();
        fifo.configure(&path).unwrap();
        fifo.write_pcm(&vec![7u8; MAX_PENDING_BYTES + 100]).unwrap();
        assert_eq!(fifo.pending_bytes(), MAX_PENDING_BYTES);
        assert_eq!(fifo.pending_bytes() % 2, 0);
        fifo.clear_pending();
        assert_eq!(fifo.pending_bytes(), 0);
    }

    #[test]
    fn odd_payloads_are_rejected() {
        let dir = TempDir::new("odd");
        let path = dir.fifo("pipe");
        let mut fifo = PlaybackFifo::new();
        fifo.configure(&path).unwrap();
        assert_eq!(fifo.write_pcm(&[1, 2, 3]), Err(PlaybackFifoError::OddLength));
    }

    #[test]
    fn a_detached_reader_does_not_kill_the_process() {
        let dir = TempDir::new("detach");
        let path = dir.fifo("pipe");
        let mut fifo = PlaybackFifo::new();
        fifo.configure(&path).unwrap();
        let reader = open_reader(&path);
        fifo.write_pcm(&[1, 2, 3, 4]).unwrap();
                // SAFETY: `reader` 是测试持有的打开描述符。
        unsafe { libc::close(reader) };
        // 缓存的描述符已失效；写入必须干净地失败，且读者恢复后下一次
        // 写入必须能重新打开。
        let _ = fifo.write_pcm(&[5, 6]);
        let reader = open_reader(&path);
        assert!(reader >= 0);
        let _ = fifo.write_pcm(&[7, 8]);
                // SAFETY: `reader` 是测试持有的打开描述符。
        unsafe { libc::close(reader) };
    }
}
