//! Sample rate conversion between the card's rates, for streams that run at
//! another rate than the card. Both rates come from the card's clock, so the
//! ratio is exact and never drifts.
//!
//! Band-limited interpolation: each output sample is the input weighted by a
//! windowed sinc centred on the output's position between input samples,
//! read from a table and interpolated linearly. The sinc is scaled to the
//! lower of the two rates, so converting down also removes what lies above
//! the output's Nyquist frequency. Integer arithmetic only, for the kernel.

mod kernel {
    include!(concat!(env!("OUT_DIR"), "/kernel.rs"));
}

use alloc::boxed::Box;

use kernel::{KERNEL, STEPS, ZERO_CROSSINGS};

/// Input frames kept; must exceed twice the widest kernel, 32 zero crossings
/// of 44.1 kHz at 192 kHz, plus the input one output can take.
const HISTORY: usize = 512;
const MAX_SAMPLE: i64 = (1 << 23) - 1;
const MIN_SAMPLE: i64 = -(1 << 23);

/// Converts frames of `C` channels of 24-bit samples. Push input frames and
/// pop output frames as they become available; an output needs the input up
/// to half the kernel width past its position.
///
/// Every field must stay an integer or an array of them: [`Resampler::boxed`]
/// makes one from zero bytes.
#[derive(Clone)]
pub struct Resampler<const C: usize> {
    rates: (u32, u32),
    /// Output rate and input rate in lowest terms.
    up: u32,
    down: u32,
    /// Kernel positions are in units of 1/`scale` input samples.
    scale: u32,
    taps: i64,
    /// Q16 gain for converting down, where the kernel is spread over more
    /// input samples; 0 when converting up.
    gain: i64,
    /// `2^32 / scale`, to interpolate between table entries without dividing.
    reciprocal: u64,
    step: (usize, u64),
    pushed: i64,
    /// Input sample at or just before the next output, and the output's
    /// distance past it in units of 1/`up`.
    center: i64,
    phase: u32,
    history: [[i32; C]; HISTORY],
}

impl<const C: usize> Resampler<C> {
    pub fn new(from_hz: u32, to_hz: u32) -> Resampler<C> {
        let mut resampler = Resampler {
            rates: (0, 0),
            up: 0,
            down: 0,
            scale: 0,
            taps: 0,
            gain: 0,
            reciprocal: 0,
            step: (0, 0),
            pushed: 0,
            center: 0,
            phase: 0,
            history: [[0; C]; HISTORY],
        };
        resampler.reset(from_hz, to_hz);
        resampler
    }

    /// A resampler that converts nothing until [`Resampler::reset`], built in
    /// place because its history is too large for a kernel stack.
    pub fn boxed() -> Box<Resampler<C>> {
        // Zero bytes are a valid value of every field, and zero rates mean
        // no conversion.
        unsafe { Box::new_zeroed().assume_init() }
    }

    /// Stops converting until the next [`Resampler::reset`].
    pub fn clear(&mut self) {
        self.rates = (0, 0);
        self.up = 0;
    }

    /// Starts over converting from `from_hz` to `to_hz`.
    pub fn reset(&mut self, from_hz: u32, to_hz: u32) {
        let common = gcd(from_hz, to_hz);
        let (up, down) = (to_hz / common, from_hz / common);
        let scale = up.max(down);
        self.rates = (from_hz, to_hz);
        self.up = up;
        self.down = down;
        self.scale = scale;
        self.taps = (ZERO_CROSSINGS as u64 * u64::from(scale)).div_ceil(u64::from(up)) as i64;
        self.gain = if down > up { (i64::from(up) << 16) / i64::from(down) } else { 0 };
        self.reciprocal = (1u64 << 32) / u64::from(scale);
        let step = up as usize * STEPS;
        self.step = (step / scale as usize, (step % scale as usize) as u64);
        self.pushed = 0;
        self.center = 0;
        self.phase = 0;
        self.history = [[0; C]; HISTORY];
        debug_assert!(2 * self.taps as usize + down.div_ceil(up) as usize + 1 < HISTORY);
    }

    /// The rates the last [`Resampler::reset`] set, input first.
    pub fn rates(&self) -> (u32, u32) {
        self.rates
    }

    pub fn push(&mut self, frame: [i32; C]) {
        self.history[self.pushed as usize % HISTORY] = frame;
        self.pushed += 1;
    }

    /// The next output frame, once enough input has been pushed.
    pub fn pop(&mut self) -> Option<[i32; C]> {
        if self.up == 0 || self.pushed <= self.center + self.taps {
            return None;
        }
        let mut sums = [0i64; C];
        // The taps from the centre back, then those after it, each side
        // moving away from the output by one input sample per tap.
        self.add_side(&mut sums, -1, self.phase);
        self.add_side(&mut sums, 1, self.up - self.phase);
        self.phase += self.down;
        self.center += i64::from(self.phase / self.up);
        self.phase %= self.up;
        Some(sums.map(|sum| ((sum + (1 << 29)) >> 30).clamp(MIN_SAMPLE, MAX_SAMPLE) as i32))
    }

    /// Adds the taps on one side of the output, the first `distance` / `up`
    /// input samples away from it.
    fn add_side(&self, sums: &mut [i64; C], direction: i64, distance: u32) {
        let position = u64::from(distance) * STEPS as u64;
        let scale = u64::from(self.scale);
        let (mut index, mut fraction) = ((position / scale) as usize, position % scale);
        let first = if direction < 0 { self.center } else { self.center + 1 };
        for tap in 0..self.taps {
            if index >= ZERO_CROSSINGS * STEPS {
                break;
            }
            let (left, right) = (i64::from(KERNEL[index]), i64::from(KERNEL[index + 1]));
            let weight = ((fraction * self.reciprocal) >> 16) as i64;
            let mut coefficient = left + (((right - left) * weight) >> 16);
            if self.gain != 0 {
                coefficient = (coefficient * self.gain) >> 16;
            }
            // Before the first push the history is zero, which stands for
            // the silence before the stream.
            let frame = &self.history[(first + direction * tap) as usize % HISTORY];
            for (sum, &sample) in sums.iter_mut().zip(frame) {
                *sum += i64::from(sample) * coefficient;
            }
            index += self.step.0;
            fraction += self.step.1;
            if fraction >= scale {
                fraction -= scale;
                index += 1;
            }
        }
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::f64::consts::TAU;
    use std::vec::Vec;

    use super::*;
    use crate::SampleRate;

    const FULL_SCALE: f64 = (1 << 23) as f64;

    fn tone(hz: f64, rate: u32, frames: usize, level: f64) -> impl Iterator<Item = [i32; 1]> {
        (0..frames).map(move |n| [(level * FULL_SCALE * (TAU * hz * n as f64 / f64::from(rate)).sin()).round() as i32])
    }

    fn convert(from: u32, to: u32, input: impl Iterator<Item = [i32; 1]>) -> Vec<f64> {
        let mut resampler = Resampler::<1>::new(from, to);
        let mut output = Vec::new();
        for frame in input {
            resampler.push(frame);
            while let Some([sample]) = resampler.pop() {
                output.push(f64::from(sample) / FULL_SCALE);
            }
        }
        output
    }

    fn rms(samples: impl Iterator<Item = f64>) -> f64 {
        let (sum, count) = samples.fold((0.0, 0), |(sum, count), s| (sum + s * s, count + 1));
        (sum / f64::from(count)).sqrt()
    }

    fn pairs() -> impl Iterator<Item = (u32, u32)> {
        SampleRate::ALL
            .into_iter()
            .flat_map(|from| SampleRate::ALL.map(|to| (from.hz(), to.hz())))
            .filter(|(from, to)| from != to)
    }

    #[test]
    fn a_tone_comes_out_at_the_new_rate_with_the_error_80_db_down() {
        for (from, to) in pairs() {
            let output = convert(from, to, tone(1000.0, from, from as usize / 4, 0.5));
            // Outputs keep their time: output n lies at input n * from / to.
            let skip = 200;
            let error = rms(output.iter().enumerate().skip(skip).map(|(n, &sample)| {
                sample - 0.5 * (TAU * 1000.0 * n as f64 / f64::from(to)).sin()
            }));
            let snr_db = 20.0 * (0.5 / 2f64.sqrt() / error).log10();
            assert!(snr_db > 80.0, "{from} -> {to}: {snr_db:.1} dB");
        }
    }

    #[test]
    fn output_frames_follow_the_ratio() {
        for (from, to) in pairs() {
            let input = 3 * from as usize / 10;
            let output = convert(from, to, core::iter::repeat_n([0], input)).len();
            let expected = input * to as usize / from as usize;
            let taps = Resampler::<1>::new(from, to).taps as usize;
            let lag = taps * to as usize / from as usize + 1;
            assert!((expected - lag..=expected).contains(&output), "{from} -> {to}: {output} of {expected}");
        }
    }

    #[test]
    fn a_constant_keeps_its_level() {
        for (from, to) in pairs() {
            let output = convert(from, to, core::iter::repeat_n([1 << 22], from as usize / 20));
            let settled = &output[output.len() / 2..];
            assert!(settled.iter().all(|&s| (s / 0.5 - 1.0).abs() < 1e-4), "{from} -> {to}: {:?}", &settled[..4]);
        }
    }

    #[test]
    fn converting_down_removes_what_the_lower_rate_cannot_carry() {
        for (from, to, hz) in [(48_000, 44_100, 23_000.0), (96_000, 44_100, 30_000.0), (192_000, 48_000, 40_000.0)] {
            let output = convert(from, to, tone(hz, from, from as usize / 4, 0.5));
            let level_db = 20.0 * (rms(output.iter().skip(500).copied()) / (0.5 / 2f64.sqrt())).log10();
            assert!(level_db < -70.0, "{from} -> {to}, {hz} Hz: {level_db:.1} dB");
        }
    }

    #[test]
    fn channels_are_converted_independently() {
        let mut resampler = Resampler::<2>::new(44_100, 48_000);
        let mut last = None;
        for _ in 0..2000 {
            resampler.push([1 << 20, -(1 << 21)]);
            while let Some(frame) = resampler.pop() {
                last = Some(frame);
            }
        }
        let [left, right] = last.unwrap();
        let off = |sample: i32, expected: i32| (f64::from(sample) / f64::from(expected) - 1.0).abs();
        assert!(off(left, 1 << 20) < 1e-4 && off(right, -(1 << 21)) < 1e-4, "{left} {right}");
    }

    #[test]
    fn a_boxed_or_cleared_resampler_converts_nothing_until_reset() {
        let mut resampler = Resampler::<2>::boxed();
        resampler.push([1, 1]);
        assert_eq!((resampler.rates(), resampler.pop()), ((0, 0), None));
        resampler.reset(48_000, 44_100);
        for _ in 0..100 {
            resampler.push([1, 1]);
        }
        assert!(resampler.pop().is_some());
        resampler.clear();
        resampler.push([1, 1]);
        assert_eq!((resampler.rates(), resampler.pop()), ((0, 0), None));
    }
}
