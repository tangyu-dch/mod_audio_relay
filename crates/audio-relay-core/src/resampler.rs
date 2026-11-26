/// Speex 风格的重采样后端。
///
/// 基于 `libspeexdsp` 实现该 trait；core 只负责外围的缓冲算术，
/// 可用确定性引擎测试。
pub trait ResampleEngine: Send {
    /// （重新）创建重采样状态。`quality` 为 Speex 质量等级。
    fn init(
        &mut self,
        channels: u32,
        input_rate: u32,
        output_rate: u32,
        quality: u32,
    ) -> Result<(), ResampleError>;

    /// 重采样一个数据块。长度为每声道帧数；返回值为
    /// （已消费帧数，已产出帧数）。
    fn process(
        &mut self,
        input: &[i16],
        output: &mut [i16],
    ) -> Result<(usize, usize), ResampleError>;

    fn output_latency(&self) -> u32;

    /// 释放状态；引擎在之后的 `init` 调用后仍须可用。
    fn destroy(&mut self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResampleError {
    /// 字节数为奇数，或速率超出 8000..64000。
    InvalidInput,
    /// 后端拒绝创建状态。
    InitFailed,
    /// 后端报告错误或未取得进展。
    ProcessFailed,
    /// 数据块超出后端 32 位计数器范围。
    Overflow,
}

impl std::fmt::Display for ResampleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidInput => "invalid resampler input",
            Self::InitFailed => "resampler initialization failed",
            Self::ProcessFailed => "resampling failed",
            Self::Overflow => "resampler block too large",
        };
        formatter.write_str(message)
    }
}

/// 采集与播放重采样器共用的速率边界。
pub const MIN_RATE: u32 = 8000;
pub const MAX_RATE: u32 = 64000;

/// 有状态重采样器封装，覆盖播放与采集两条路径。
pub struct PcmResampler<E: ResampleEngine> {
    engine: E,
    quality: u32,
    channels: u32,
    input_rate: u32,
    output_rate: u32,
    initialized: bool,
}

impl<E: ResampleEngine> PcmResampler<E> {
    pub fn new(engine: E, quality: u32) -> Self {
        Self {
            engine,
            quality,
            channels: 0,
            input_rate: 0,
            output_rate: 0,
            initialized: false,
        }
    }

    pub fn reset(&mut self) {
        if self.initialized {
            self.engine.destroy();
            self.initialized = false;
        }
        self.channels = 0;
        self.input_rate = 0;
        self.output_rate = 0;
    }

    /// 播放路径：转换整个 PCM 数据块，循环直至所有输入帧被消费。
    /// 速率相同时原样透传。
    pub fn process_pcm(
        &mut self,
        pcm: &[u8],
        input_rate: u32,
        output_rate: u32,
        channels: u32,
    ) -> Result<Vec<u8>, ResampleError> {
        if pcm.len() & 1 != 0
            || !(MIN_RATE..=MAX_RATE).contains(&input_rate)
            || !(MIN_RATE..=MAX_RATE).contains(&output_rate)
            || channels == 0
        {
            return Err(ResampleError::InvalidInput);
        }
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        if input_rate == output_rate {
            self.reset();
            return Ok(pcm.to_vec());
        }
        self.ensure(input_rate, output_rate, channels)?;

        let frames: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let total_input = frames.len() / channels as usize;
        if total_input > u32::MAX as usize {
            return Err(ResampleError::Overflow);
        }

        let mut output: Vec<i16> = Vec::new();
        let mut consumed = 0usize;
        while consumed < total_input {
            let remaining = total_input - consumed;
            let scaled =
                (remaining as u64 * output_rate as u64).div_ceil(input_rate as u64);
            let capacity = scaled + self.engine.output_latency() as u64 + 16;
            if capacity > u32::MAX as u64 {
                return Err(ResampleError::Overflow);
            }
            let capacity = capacity as usize * channels as usize;
            let mut block = vec![0i16; capacity];
            let (used, produced) = self
                .engine
                .process(&frames[consumed * channels as usize..], &mut block)?;
            if used == 0 && produced == 0 {
                self.reset();
                return Err(ResampleError::ProcessFailed);
            }
            consumed += used;
            output.extend_from_slice(&block[..produced * channels as usize]);
        }

        Ok(output
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect())
    }

    /// 采集路径：每个 media bug 帧处理一个数据块，输出容量由发送
    /// 累加器的剩余空间决定。多余输入被丢弃。
    pub fn process_frames(
        &mut self,
        frames: &[i16],
        input_rate: u32,
        output_rate: u32,
        channels: u32,
        max_output_frames: usize,
    ) -> Result<Vec<i16>, ResampleError> {
        if !(MIN_RATE..=MAX_RATE).contains(&input_rate)
            || !(MIN_RATE..=MAX_RATE).contains(&output_rate)
            || channels == 0
        {
            return Err(ResampleError::InvalidInput);
        }
        if max_output_frames == 0 || frames.is_empty() {
            return Ok(Vec::new());
        }
        if input_rate == output_rate {
            self.reset();
            let count = frames.len().min(max_output_frames * channels as usize);
            return Ok(frames[..count].to_vec());
        }
        self.ensure(input_rate, output_rate, channels)?;
        let mut block = vec![0i16; max_output_frames * channels as usize];
        let (_, produced) = self.engine.process(frames, &mut block)?;
        block.truncate(produced * channels as usize);
        Ok(block)
    }

    fn ensure(
        &mut self,
        input_rate: u32,
        output_rate: u32,
        channels: u32,
    ) -> Result<(), ResampleError> {
        if self.initialized
            && self.input_rate == input_rate
            && self.output_rate == output_rate
            && self.channels == channels
        {
            return Ok(());
        }
        self.reset();
        self.engine
            .init(channels, input_rate, output_rate, self.quality)?;
        self.initialized = true;
        self.channels = channels;
        self.input_rate = input_rate;
        self.output_rate = output_rate;
        Ok(())
    }
}

impl ResampleEngine for Box<dyn ResampleEngine> {
    fn init(
        &mut self,
        channels: u32,
        input_rate: u32,
        output_rate: u32,
        quality: u32,
    ) -> Result<(), ResampleError> {
        (**self).init(channels, input_rate, output_rate, quality)
    }

    fn process(
        &mut self,
        input: &[i16],
        output: &mut [i16],
    ) -> Result<(usize, usize), ResampleError> {
        (**self).process(input, output)
    }

    fn output_latency(&self) -> u32 {
        (**self).output_latency()
    }

    fn destroy(&mut self) {
        (**self).destroy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// speexdsp 的确定性替身：最近邻速率转换加固定一帧延迟，
    /// 足以覆盖缓冲算术的测试。
    struct FakeEngine {
        channels: u32,
        input_rate: u32,
        output_rate: u32,
        fail: bool,
        stall: bool,
    }

    impl FakeEngine {
        fn new() -> Self {
            Self { channels: 1, input_rate: 0, output_rate: 0, fail: false, stall: false }
        }
    }

    impl ResampleEngine for FakeEngine {
        fn init(
            &mut self,
            channels: u32,
            input_rate: u32,
            output_rate: u32,
            _quality: u32,
        ) -> Result<(), ResampleError> {
            if self.fail {
                return Err(ResampleError::InitFailed);
            }
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
            if self.stall {
                return Ok((0, 0));
            }
            let channels = self.channels as usize;
            let in_frames = input.len() / channels;
            let out_frames = output.len() / channels;
            let produced = ((in_frames as u64 * self.output_rate as u64)
                / self.input_rate as u64)
                .min(out_frames as u64) as usize;
            for frame in 0..produced {
                let source = ((frame as u64 * self.input_rate as u64
                    + self.output_rate as u64 / 2)
                    / self.output_rate as u64)
                .min(in_frames.saturating_sub(1) as u64) as usize;
                for channel in 0..channels {
                    output[frame * channels + channel] = input[source * channels + channel];
                }
            }
            Ok((in_frames, produced))
        }

        fn output_latency(&self) -> u32 {
            1
        }

        fn destroy(&mut self) {
            self.input_rate = 0;
            self.output_rate = 0;
        }
    }

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|sample| sample.to_le_bytes()).collect()
    }

    fn decode(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    }

    #[test]
    fn equal_rates_pass_through() {
        let mut resampler = PcmResampler::new(FakeEngine::new(), 5);
        let input = pcm(&[1, -2, 3]);
        assert_eq!(resampler.process_pcm(&input, 16000, 16000, 1).unwrap(), input);
    }

    #[test]
    fn upsampling_consumes_every_input_frame() {
        let mut resampler = PcmResampler::new(FakeEngine::new(), 5);
        let input = pcm(&[100; 160]);
        let output = resampler.process_pcm(&input, 8000, 16000, 1).unwrap();
        assert_eq!(decode(&output).len(), 320);
        assert!(decode(&output).iter().all(|sample| *sample == 100));
    }

    #[test]
    fn downsampling_produces_the_scaled_frame_count() {
        let mut resampler = PcmResampler::new(FakeEngine::new(), 5);
        let input = pcm(&[7; 640]);
        let output = resampler.process_pcm(&input, 16000, 8000, 1).unwrap();
        assert_eq!(decode(&output).len(), 320);
    }

    #[test]
    fn stereo_blocks_keep_channel_interleaving() {
        let mut resampler = PcmResampler::new(FakeEngine::new(), 5);
        let mut input = Vec::new();
        for index in 0..160 {
            input.push(index as i16);
            input.push(-(index as i16));
        }
        let output = resampler.process_pcm(&pcm(&input), 8000, 16000, 2).unwrap();
        let decoded = decode(&output);
        assert_eq!(decoded.len(), 640);
        assert_eq!(decoded[0], 0);
        assert_eq!(decoded[1], 0);
        assert_eq!(decoded[2], 1);
        assert_eq!(decoded[3], -1);
    }

    #[test]
    fn invalid_input_is_rejected() {
        let mut resampler = PcmResampler::new(FakeEngine::new(), 5);
        assert_eq!(
            resampler.process_pcm(&[1, 2, 3], 8000, 16000, 1),
            Err(ResampleError::InvalidInput)
        );
        assert_eq!(
            resampler.process_pcm(&[1, 2], 4000, 16000, 1),
            Err(ResampleError::InvalidInput)
        );
        assert_eq!(
            resampler.process_pcm(&[1, 2], 8000, 96000, 1),
            Err(ResampleError::InvalidInput)
        );
        assert!(resampler.process_pcm(&[], 8000, 16000, 1).unwrap().is_empty());
    }

    #[test]
    fn stalled_backend_resets_and_reports_failure() {
        let mut engine = FakeEngine::new();
        engine.stall = true;
        let mut resampler = PcmResampler::new(engine, 5);
        assert_eq!(
            resampler.process_pcm(&pcm(&[1, 2]), 8000, 16000, 1),
            Err(ResampleError::ProcessFailed)
        );
    }

    #[test]
    fn capture_path_honours_the_output_capacity() {
        let mut resampler = PcmResampler::new(FakeEngine::new(), 2);
        let frames: Vec<i16> = (0..320).collect();
        let output = resampler
            .process_frames(&frames, 8000, 16000, 1, 100)
            .unwrap();
        assert_eq!(output.len(), 100);
        let output = resampler
            .process_frames(&frames, 8000, 16000, 1, 0)
            .unwrap();
        assert!(output.is_empty());
    }
}
