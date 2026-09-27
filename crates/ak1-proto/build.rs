//! Generates the resampler's kernel: one side of a Kaiser-windowed sinc, in
//! fixed point, sampled `STEPS` times per zero crossing of the lower rate.

use std::f64::consts::PI;
use std::fmt::Write;

const ZERO_CROSSINGS: usize = 32;
const STEPS: usize = 256;
/// Cutoff as a fraction of the lower rate's Nyquist frequency, low enough
/// that the stopband starts at that Nyquist frequency.
const CUTOFF: f64 = 0.92;
/// Kaiser window shape for about 80 dB of stopband attenuation.
const BETA: f64 = 7.86;
const ONE: f64 = (1u64 << 30) as f64;

fn main() {
    let mut code = format!(
        "pub const ZERO_CROSSINGS: usize = {ZERO_CROSSINGS};\npub const STEPS: usize = {STEPS};\n\
         /// Q30 kernel values at 0, 1/STEPS, ... ZERO_CROSSINGS zero crossings, and a closing zero.\n\
         pub static KERNEL: [i32; {}] = [\n",
        ZERO_CROSSINGS * STEPS + 2
    );
    for i in 0..=ZERO_CROSSINGS * STEPS {
        let x = i as f64 / STEPS as f64;
        let value = CUTOFF * sinc(CUTOFF * x) * kaiser(x / ZERO_CROSSINGS as f64);
        writeln!(code, "    {},", (value * ONE).round() as i32).unwrap();
    }
    code.push_str("    0,\n];\n");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("kernel.rs");
    std::fs::write(out, code).unwrap();
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 { 1.0 } else { (PI * x).sin() / (PI * x) }
}

fn kaiser(t: f64) -> f64 {
    bessel_i0(BETA * (1.0 - t * t).max(0.0).sqrt()) / bessel_i0(BETA)
}

/// Modified Bessel function of the first kind, order 0, by its power series.
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > sum * 1e-17 {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}
