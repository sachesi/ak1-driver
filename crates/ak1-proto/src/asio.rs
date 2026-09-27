//! Private property set through which the ASIO driver streams: the kernel
//! driver offers it on the Output12 circuit's filter and mixes the ASIO
//! streams with the Windows audio endpoints.
//!
//! The ASIO driver starts a session with [`StartParams`] and an event the
//! kernel driver sets whenever a period of input is complete. It then reads
//! each input period ([`ReadHeader`] and `period` frames of [`INPUTS`]
//! samples) and writes one period of [`OUTPUTS`] samples back. Samples are
//! 24-bit audio in the high bits of an `i32`, as in ASIO's Int32LSB.

use alloc::collections::TryReserveError;
use alloc::vec::Vec;

pub const PROPERTY_SET: (u32, u16, u16, [u8; 8]) =
    (0x8a3e4d71, 0x2c5b, 0x4f0e, [0xb1, 0x96, 0x3d, 0x7a, 0x52, 0xe0, 0x4c, 0x18]);

/// Set, value [`StartParams`]. Fails with `STATUS_DEVICE_BUSY` while
/// another handle has a session.
pub const PROPERTY_START: u32 = 0;
/// Set, no value.
pub const PROPERTY_STOP: u32 = 1;
/// Get, value [`ReadHeader`] followed by the input frames.
pub const PROPERTY_READ: u32 = 2;
/// Set, value `period` output frames.
pub const PROPERTY_WRITE: u32 = 3;

pub const INPUTS: usize = 2;
pub const OUTPUTS: usize = 4;
pub const MAX_PERIOD_FRAMES: u32 = 8192;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartParams {
    pub rate_hz: u32,
    pub period_frames: u32,
    /// Handle, in the caller's process, of an event the driver sets.
    pub event: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadHeader {
    /// Frames that follow: a whole period, or none while it is incomplete.
    pub frames: u32,
}

pub const fn read_value_bytes(period_frames: u32) -> usize {
    size_of::<ReadHeader>() + period_frames as usize * INPUTS * size_of::<i32>()
}

/// A session's frames on their way between the card and the host.
///
/// The host fills a period while the one before it plays, and the card takes
/// output a transfer at a time, so output starts that far ahead. Every
/// captured frame is answered by one played frame, so the output level only
/// drops when the host is late; its late output then finds the buffer full
/// and pushes out the oldest frames, which keeps the latency.
pub struct Buffers {
    period: usize,
    input: Fifo<INPUTS>,
    output: Fifo<OUTPUTS>,
    captured: usize,
    silent_frames: u32,
    dropped_frames: u32,
}

impl Buffers {
    /// Buffers for periods of `period` frames on a card that moves `transfer` frames at a time.
    pub fn new(period: usize, transfer: usize) -> Result<Buffers, TryReserveError> {
        let mut output = Fifo::new(2 * period + transfer)?;
        while output.push([0; OUTPUTS]) {}
        Ok(Buffers {
            period,
            input: Fifo::new(4 * period + 2 * transfer)?,
            output,
            captured: 0,
            silent_frames: 0,
            dropped_frames: 0,
        })
    }

    /// Takes a captured frame; returns whether it completed a period.
    pub fn capture(&mut self, frame: [i32; INPUTS]) -> bool {
        if self.input.push_over(frame) {
            self.dropped_frames = self.dropped_frames.saturating_add(1);
        }
        self.captured += 1;
        if self.captured < self.period {
            return false;
        }
        self.captured = 0;
        true
    }

    /// The next frame to play, silence if the host has not written it.
    pub fn render(&mut self) -> [i32; OUTPUTS] {
        self.output.pop().unwrap_or_else(|| {
            self.silent_frames = self.silent_frames.saturating_add(1);
            [0; OUTPUTS]
        })
    }

    /// Moves the oldest complete period into `into`. Returns its length, 0
    /// while none is complete, or `None` if `into` is shorter than a period.
    pub fn read(&mut self, into: &mut [[i32; INPUTS]]) -> Option<usize> {
        let into = into.get_mut(..self.period)?;
        if self.input.len < self.period {
            return Some(0);
        }
        for frame in into {
            *frame = self.input.pop().unwrap_or_default();
        }
        Some(self.period)
    }

    /// Queues a period of output; returns false unless `from` is one.
    pub fn write(&mut self, from: &[[i32; OUTPUTS]]) -> bool {
        if from.len() != self.period {
            return false;
        }
        for &frame in from {
            self.output.push_over(frame);
        }
        true
    }

    /// Output frames played as silence because the host was late, and input
    /// frames dropped because they were not read in time.
    pub fn stats(&self) -> [u32; 2] {
        [self.silent_frames, self.dropped_frames]
    }
}

struct Fifo<const C: usize> {
    frames: Vec<[i32; C]>,
    start: usize,
    len: usize,
}

impl<const C: usize> Fifo<C> {
    fn new(capacity: usize) -> Result<Fifo<C>, TryReserveError> {
        let mut frames = Vec::new();
        frames.try_reserve_exact(capacity)?;
        frames.resize(capacity, [0; C]);
        Ok(Fifo { frames, start: 0, len: 0 })
    }

    /// Appends `frame` unless the ring is full.
    fn push(&mut self, frame: [i32; C]) -> bool {
        if self.len == self.frames.len() {
            return false;
        }
        let end = (self.start + self.len) % self.frames.len();
        self.frames[end] = frame;
        self.len += 1;
        true
    }

    /// Appends `frame`, dropping the oldest one if the ring is full; returns whether it did.
    fn push_over(&mut self, frame: [i32; C]) -> bool {
        let full = !self.push(frame);
        if full {
            self.pop();
            self.push(frame);
        }
        full
    }

    fn pop(&mut self) -> Option<[i32; C]> {
        if self.len == 0 {
            return None;
        }
        let frame = self.frames[self.start];
        self.start = (self.start + 1) % self.frames.len();
        self.len -= 1;
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    const PERIOD: usize = 64;
    const TRANSFER: usize = 48;

    fn input(step: usize) -> [i32; INPUTS] {
        [step as i32 + 1, -(step as i32) - 1]
    }

    /// Runs the card for `steps` frames against a host that plays its input
    /// back as outputs 1/2 whenever `responds(step)`; returns what played.
    /// Like the kernel driver, the card captures and plays a frame before
    /// the host gets a turn.
    fn loop_back(buffers: &mut Buffers, steps: usize, responds: impl Fn(usize) -> bool) -> Vec<[i32; OUTPUTS]> {
        let mut period = vec![[0; INPUTS]; PERIOD];
        let mut played = Vec::new();
        for step in 0..steps {
            buffers.capture(input(step));
            played.push(buffers.render());
            if responds(step) {
                while buffers.read(&mut period) == Some(PERIOD) {
                    let output: Vec<_> = period.iter().map(|&[l, r]| [l, r, 0, 0]).collect();
                    assert!(buffers.write(&output));
                }
            }
        }
        played
    }

    /// Frames between each input frame and its output, for outputs from step `from` on.
    fn delays(played: &[[i32; OUTPUTS]], from: usize) -> Vec<usize> {
        let played = played.iter().enumerate().skip(from).filter(|(_, f)| f[0] != 0);
        played.map(|(step, f)| step + 1 - f[0] as usize).collect()
    }

    #[test]
    fn a_prompt_host_hears_its_input_two_periods_and_a_transfer_later() {
        let mut buffers = Buffers::new(PERIOD, TRANSFER).unwrap();
        let played = loop_back(&mut buffers, 20 * PERIOD, |_| true);
        let delays = delays(&played, 0);
        assert!(!delays.is_empty() && delays.iter().all(|&d| d == 2 * PERIOD + TRANSFER), "{delays:?}");
        assert_eq!(buffers.stats(), [0, 0]);
    }

    #[test]
    fn a_late_host_gets_silence_and_then_the_same_latency_again() {
        let mut buffers = Buffers::new(PERIOD, TRANSFER).unwrap();
        let stall = 10 * PERIOD..17 * PERIOD;
        let played = loop_back(&mut buffers, 30 * PERIOD, |step| !stall.contains(&step));
        let [silent, dropped] = buffers.stats();
        assert!(silent > 0 && dropped > 0, "{silent} {dropped}");
        let delays = delays(&played, 20 * PERIOD);
        assert!(!delays.is_empty() && delays.iter().all(|&d| d == 2 * PERIOD + TRANSFER), "{delays:?}");
    }

    #[test]
    fn a_period_is_signalled_and_read_once_complete() {
        let mut buffers = Buffers::new(PERIOD, TRANSFER).unwrap();
        let mut period = vec![[0; INPUTS]; PERIOD];
        for step in 0..PERIOD - 1 {
            assert!(!buffers.capture(input(step)));
        }
        assert_eq!(buffers.read(&mut period), Some(0));
        assert!(buffers.capture(input(PERIOD - 1)));
        assert_eq!(buffers.read(&mut period), Some(PERIOD));
        assert_eq!((period[0], period[PERIOD - 1]), (input(0), input(PERIOD - 1)));
    }

    #[test]
    fn unread_input_is_dropped_oldest_first() {
        let mut buffers = Buffers::new(PERIOD, TRANSFER).unwrap();
        let capacity = 4 * PERIOD + 2 * TRANSFER;
        for step in 0..capacity + 5 {
            buffers.capture(input(step));
        }
        let mut period = vec![[0; INPUTS]; PERIOD];
        assert_eq!(buffers.read(&mut period), Some(PERIOD));
        assert_eq!((period[0], buffers.stats()[1]), (input(5), 5));
    }

    #[test]
    fn reads_and_writes_must_fit_a_period() {
        let mut buffers = Buffers::new(PERIOD, TRANSFER).unwrap();
        assert_eq!(buffers.read(&mut [[0; INPUTS]; PERIOD - 1]), None);
        assert!(!buffers.write(&[[0; OUTPUTS]; PERIOD + 1]));
        assert!(buffers.write(&[[0; OUTPUTS]; PERIOD]));
    }
}
