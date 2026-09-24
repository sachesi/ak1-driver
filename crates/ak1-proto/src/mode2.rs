//! Isochronous sample layout for `data_alignment == 2` with two stereo streams.
//!
//! Both directions are built from 8-byte blocks that carry one 24-bit
//! big-endian sample and one check byte per stream, the streams interleaved
//! byte by byte. A frame (left and right of every stream) takes two blocks.
//!
//! Capture blocks are `[chk0, chk1, b0, b0, b1, b1, b2, b2]`; the even block of
//! a frame holds the right channel. Playback blocks are rotated by four bytes,
//! `[b1, b1, b2, b2, chk0, chk1, b0, b0]`: a sample begins right after its
//! check byte and ends in the next block, so the last sample of one packet
//! finishes in the first block of the next. The even block of a playback frame
//! begins the left channel.
//!
//! Check bytes are `(stream << 1) | parity`, where parity is 1 for even blocks
//! counted from the start of the packet. On capture, bit 7 reports that the
//! device rejected playback data.

use crate::CHANNELS_PER_STREAM;

pub const STREAMS: usize = 2;
pub const CHANNELS: usize = STREAMS * CHANNELS_PER_STREAM;
pub const BLOCK_BYTES: usize = 8;
pub const FRAME_BYTES: usize = 2 * BLOCK_BYTES;

const CHECK_MASK: u8 = 0x3f;
const OUTPUT_PANIC: u8 = 0x80;

/// One sample per channel, stream-major: stream 0 left, stream 0 right, stream 1 left, ...
/// Samples are signed 24-bit values in the low bits.
pub type Frame = [i32; CHANNELS];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureStatus {
    pub frames: usize,
    pub check_errors: usize,
    pub output_rejected: bool,
}

pub fn frames_in(packet_len: usize) -> usize {
    packet_len / FRAME_BYTES
}

/// Decodes whole frames of a capture packet; trailing partial frames are ignored.
pub fn decode_capture(packet: &[u8], mut frame: impl FnMut(Frame)) -> CaptureStatus {
    let mut status = CaptureStatus::default();
    for (index, bytes) in packet.chunks_exact(FRAME_BYTES).enumerate() {
        let (right, left) = bytes.split_at(BLOCK_BYTES);
        let mut samples = [0; CHANNELS];
        for (block_in_frame, block) in [left, right].into_iter().enumerate() {
            let block_index = 2 * index + usize::from(block_in_frame == 0);
            for stream in 0..STREAMS {
                let check = block[stream];
                if check & CHECK_MASK != check_byte(stream, block_index) {
                    status.check_errors += 1;
                }
                status.output_rejected |= check & OUTPUT_PANIC != 0;
                let sample = [block[2 + stream], block[4 + stream], block[6 + stream]];
                samples[stream * CHANNELS_PER_STREAM + block_in_frame] = from_be24(sample);
            }
        }
        frame(samples);
        status.frames += 1;
    }
    status
}

/// Carries the tail of the last playback sample from one packet into the next.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlaybackEncoder {
    tail: [[u8; 2]; STREAMS],
}

impl PlaybackEncoder {
    /// Fills `packet` (a whole number of frames) with samples from `next_frame`.
    pub fn encode(&mut self, packet: &mut [u8], mut next_frame: impl FnMut() -> Frame) {
        for (index, bytes) in packet.chunks_exact_mut(FRAME_BYTES).enumerate() {
            let samples = next_frame();
            for (block_in_frame, block) in bytes.chunks_exact_mut(BLOCK_BYTES).enumerate() {
                let block_index = 2 * index + block_in_frame;
                for stream in 0..STREAMS {
                    let [b0, b1, b2] = to_be24(samples[stream * CHANNELS_PER_STREAM + block_in_frame]);
                    let [t1, t2] = self.tail[stream];
                    block[stream] = t1;
                    block[2 + stream] = t2;
                    block[4 + stream] = check_byte(stream, block_index);
                    block[6 + stream] = b0;
                    self.tail[stream] = [b1, b2];
                }
            }
        }
    }
}

fn check_byte(stream: usize, block_index: usize) -> u8 {
    ((stream as u8) << 1) | u8::from(block_index % 2 == 0)
}

fn from_be24([b0, b1, b2]: [u8; 3]) -> i32 {
    i32::from_be_bytes([b0, b1, b2, 0]) >> 8
}

fn to_be24(sample: i32) -> [u8; 3] {
    let [_, b0, b1, b2] = sample.to_be_bytes();
    [b0, b1, b2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_block(block_index: usize, samples: [i32; STREAMS], panic: bool) -> [u8; BLOCK_BYTES] {
        let mut block = [0; BLOCK_BYTES];
        for stream in 0..STREAMS {
            block[stream] = check_byte(stream, block_index) | if panic { OUTPUT_PANIC } else { 0 };
            let [b0, b1, b2] = to_be24(samples[stream]);
            block[2 + stream] = b0;
            block[4 + stream] = b1;
            block[6 + stream] = b2;
        }
        block
    }

    #[test]
    fn capture_even_block_is_right_channel() {
        let mut packet = [0; 2 * FRAME_BYTES];
        for (i, block) in packet.chunks_exact_mut(BLOCK_BYTES).enumerate() {
            let value = i as i32 + 1;
            block.copy_from_slice(&capture_block(i, [value, -value], false));
        }
        let mut frames = [[0; CHANNELS]; 2];
        let mut n = 0;
        let status = decode_capture(&packet, |f| {
            frames[n] = f;
            n += 1;
        });
        assert_eq!(status, CaptureStatus { frames: 2, check_errors: 0, output_rejected: false });
        assert_eq!(frames, [[2, 1, -2, -1], [4, 3, -4, -3]]);
    }

    #[test]
    fn capture_reports_check_errors_and_output_rejection() {
        let mut packet = [0; FRAME_BYTES];
        packet[..BLOCK_BYTES].copy_from_slice(&capture_block(1, [0, 0], false));
        packet[BLOCK_BYTES..].copy_from_slice(&capture_block(1, [0, 0], true));
        let status = decode_capture(&packet, |_| {});
        assert_eq!(status, CaptureStatus { frames: 1, check_errors: 2, output_rejected: true });
    }

    #[test]
    fn capture_sign_extends_24_bit_samples() {
        let block = capture_block(0, [-0x80_0000, 0x7f_ffff], false);
        let mut packet = [0; FRAME_BYTES];
        packet[..BLOCK_BYTES].copy_from_slice(&block);
        packet[BLOCK_BYTES..].copy_from_slice(&capture_block(1, [0, 0], false));
        let mut got = [0; CHANNELS];
        decode_capture(&packet, |f| got = f);
        assert_eq!(got, [0, -0x80_0000, 0, 0x7f_ffff]);
    }

    #[test]
    fn playback_sample_starts_after_check_byte_and_spills_into_next_packet() {
        let mut encoder = PlaybackEncoder::default();
        let mut first = [0xee; FRAME_BYTES];
        encoder.encode(&mut first, || [0x112233, 0x445566, 0x778899, -1]);
        assert_eq!(
            first,
            [
                0x00, 0x00, 0x00, 0x00, 0x01, 0x03, 0x11, 0x77, // left starts
                0x22, 0x88, 0x33, 0x99, 0x00, 0x02, 0x44, 0xff, // left ends, right starts
            ]
        );
        let mut second = [0; FRAME_BYTES];
        encoder.encode(&mut second, || [0; CHANNELS]);
        assert_eq!(second[..BLOCK_BYTES], [0x55, 0xff, 0x66, 0xff, 0x01, 0x03, 0x00, 0x00]);
    }

    #[test]
    fn playback_check_parity_restarts_each_packet() {
        let mut encoder = PlaybackEncoder::default();
        let mut packet = [0; 2 * FRAME_BYTES];
        encoder.encode(&mut packet, || [0; CHANNELS]);
        let checks: [[u8; 2]; 4] = core::array::from_fn(|i| [packet[i * 8 + 4], packet[i * 8 + 5]]);
        assert_eq!(checks, [[1, 3], [0, 2], [1, 3], [0, 2]]);
    }
}
