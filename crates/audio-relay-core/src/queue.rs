/// 定长容量的 PCM 环形缓冲，溢出时丢弃最旧数据。
///
/// 缓冲最多保存两秒播放音频；生产端跑赢写出侧时丢弃最旧字节，
/// 而不是阻塞 WebSocket 读取线程。
#[derive(Debug)]
pub struct PlaybackQueue {
    max_bytes: usize,
    buffer: Vec<u8>,
    head: usize,
    size: usize,
}

impl PlaybackQueue {
    pub fn new(max_bytes: usize) -> Self {
        let max_bytes = max_bytes & !1;
        Self {
            max_bytes,
            buffer: vec![0; max_bytes],
            head: 0,
            size: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.max_bytes
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    pub fn push(&mut self, pcm: &[u8]) {
        if self.max_bytes == 0 {
            return;
        }
        let incoming = pcm.len() & !1;
        if incoming == 0 {
            return;
        }
        if incoming >= self.max_bytes {
            let start = pcm.len() - self.max_bytes;
            self.buffer.copy_from_slice(&pcm[start..start + self.max_bytes]);
            self.head = 0;
            self.size = self.max_bytes;
            return;
        }

        let trim = (self.size + incoming).saturating_sub(self.max_bytes);
        self.head = (self.head + trim) % self.max_bytes;
        self.size -= trim;

        let tail = (self.head + self.size) % self.max_bytes;
        let first = incoming.min(self.max_bytes - tail);
        self.buffer[tail..tail + first].copy_from_slice(&pcm[..first]);
        if first < incoming {
            self.buffer[..incoming - first].copy_from_slice(&pcm[first..incoming]);
        }
        self.size += incoming;
    }

    /// 最多复制 `dst.len()` 字节（向下取整到采样边界）。
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let count = (dst.len() & !1).min(self.size);
        if count == 0 {
            return 0;
        }
        let first = count.min(self.max_bytes - self.head);
        dst[..first].copy_from_slice(&self.buffer[self.head..self.head + first]);
        if first < count {
            dst[first..count].copy_from_slice(&self.buffer[..count - first]);
        }
        self.head = (self.head + count) % self.max_bytes;
        self.size -= count;
        if self.size == 0 {
            self.head = 0;
        }
        count
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.size = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_without_wrapping() {
        let mut queue = PlaybackQueue::new(8);
        queue.push(&[1, 2, 3, 4]);
        assert_eq!(queue.len(), 4);
        let mut dst = [0u8; 8];
        assert_eq!(queue.read(&mut dst), 4);
        assert_eq!(&dst[..4], &[1, 2, 3, 4]);
        assert!(queue.is_empty());
    }

    #[test]
    fn wraps_and_keeps_sample_alignment() {
        let mut queue = PlaybackQueue::new(8);
        queue.push(&[1, 2, 3, 4, 5, 6]);
        let mut dst = [0u8; 4];
        assert_eq!(queue.read(&mut dst), 4);
        queue.push(&[7, 8, 9, 10]);
        assert_eq!(queue.len(), 6);
        let mut dst = [0u8; 8];
        assert_eq!(queue.read(&mut dst), 6);
        assert_eq!(&dst[..6], &[5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn overflow_drops_the_oldest_bytes() {
        let mut queue = PlaybackQueue::new(8);
        queue.push(&[1, 2, 3, 4, 5, 6]);
        queue.push(&[7, 8, 9, 10, 11, 12]);
        assert_eq!(queue.len(), 8);
        let mut dst = [0u8; 8];
        assert_eq!(queue.read(&mut dst), 8);
        assert_eq!(&dst, &[5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn oversized_push_keeps_only_the_tail() {
        let mut queue = PlaybackQueue::new(4);
        queue.push(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(queue.len(), 4);
        let mut dst = [0u8; 4];
        assert_eq!(queue.read(&mut dst), 4);
        assert_eq!(&dst, &[7, 8, 9, 10]);
    }

    #[test]
    fn odd_lengths_are_trimmed_to_samples() {
        let mut queue = PlaybackQueue::new(8);
        queue.push(&[1, 2, 3]);
        assert_eq!(queue.len(), 2);
        let mut dst = [0u8; 3];
        assert_eq!(queue.read(&mut dst), 2);
        assert_eq!(&dst[..2], &[1, 2]);
    }

    #[test]
    fn zero_capacity_queue_is_inert() {
        let mut queue = PlaybackQueue::new(1);
        assert_eq!(queue.capacity(), 0);
        queue.push(&[1, 2, 3, 4]);
        assert!(queue.is_empty());
    }

    #[test]
    fn clear_resets_the_ring() {
        let mut queue = PlaybackQueue::new(8);
        queue.push(&[1, 2, 3, 4, 5, 6]);
        let mut dst = [0u8; 2];
        queue.read(&mut dst);
        queue.clear();
        assert!(queue.is_empty());
        queue.push(&[9, 10]);
        let mut dst = [0u8; 8];
        assert_eq!(queue.read(&mut dst), 2);
        assert_eq!(&dst[..2], &[9, 10]);
    }
}
