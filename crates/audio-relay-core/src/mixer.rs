/// 带饱和的 L16 混音与单声道复制。
///
/// `channels` 是 `destination` 的声道数；单声道源会被复制到每个
/// 声道。返回被写入的目标字节数。
pub fn mix_s16le(destination: &mut [u8], source: &[u8], channels: usize) -> usize {
    if channels == 0 {
        return 0;
    }
    let samples = (source.len() / 2) * channels;
    let mut mixed_bytes = 0usize;
    for index in 0..samples {
        let dst_offset = index * 2;
        let src_offset = (index / channels) * 2;
        if dst_offset + 2 > destination.len() || src_offset + 2 > source.len() {
            break;
        }
        let dst_sample = i16::from_le_bytes([destination[dst_offset], destination[dst_offset + 1]]);
        let src_sample = i16::from_le_bytes([source[src_offset], source[src_offset + 1]]);
        let mixed = (dst_sample as i32 + src_sample as i32).clamp(-32768, 32767) as i16;
        destination[dst_offset..dst_offset + 2].copy_from_slice(&mixed.to_le_bytes());
        mixed_bytes = dst_offset + 2;
    }
    mixed_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    }

    #[test]
    fn mono_mix_adds_sample_by_sample() {
        let mut dst = [0u8; 4];
        dst[..2].copy_from_slice(&1000i16.to_le_bytes());
        dst[2..].copy_from_slice(&(-2000i16).to_le_bytes());
        let src = [0u8; 4];
        let mut source = src;
        source[..2].copy_from_slice(&500i16.to_le_bytes());
        source[2..].copy_from_slice(&100i16.to_le_bytes());
        assert_eq!(mix_s16le(&mut dst, &source, 1), 4);
        assert_eq!(samples(&dst), vec![1500, -1900]);
    }

    #[test]
    fn overflow_saturates_instead_of_wrapping() {
        let mut dst = [0u8; 2];
        dst.copy_from_slice(&32000i16.to_le_bytes());
        let mut src = [0u8; 2];
        src.copy_from_slice(&32000i16.to_le_bytes());
        mix_s16le(&mut dst, &src, 1);
        assert_eq!(samples(&dst), vec![32767]);

        dst.copy_from_slice(&(-32000i16).to_le_bytes());
        src.copy_from_slice(&(-32000i16).to_le_bytes());
        mix_s16le(&mut dst, &src, 1);
        assert_eq!(samples(&dst), vec![-32768]);
    }

    #[test]
    fn mono_source_is_replicated_across_stereo_destination() {
        let mut dst = [0u8; 4];
        dst[..2].copy_from_slice(&10i16.to_le_bytes());
        dst[2..].copy_from_slice(&20i16.to_le_bytes());
        let mut src = [0u8; 2];
        src.copy_from_slice(&5i16.to_le_bytes());
        assert_eq!(mix_s16le(&mut dst, &src, 2), 4);
        assert_eq!(samples(&dst), vec![15, 25]);
    }

    #[test]
    fn zero_channels_and_short_buffers_are_no_ops() {
        let mut dst = [0u8; 4];
        let src = [1u8, 0, 2, 0];
        assert_eq!(mix_s16le(&mut dst, &src, 0), 0);
        assert_eq!(mix_s16le(&mut dst[..1], &src, 1), 0);
        assert_eq!(dst, [0u8; 4]);
    }
}
