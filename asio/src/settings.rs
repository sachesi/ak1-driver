//! Per-user ASIO settings, written by the control panel and used for hosts
//! that do not choose a sample rate or buffer size themselves.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_OPTION_NON_VOLATILE, RRF_RT_REG_DWORD, RegCreateKeyExW,
    RegGetValueW, RegSetKeyValueW,
};
use windows::core::{PCWSTR, Result, w};

pub const RATES: [u32; 5] = [44_100, 48_000, 88_200, 96_000, 192_000];
/// Buffer sizes offered to hosts, as multiples of the smallest one.
pub const BUFFER_MULTIPLES: [u32; 4] = [1, 2, 4, 8];
/// Audio the kernel driver moves per USB transfer, which bounds the smallest buffer.
pub const TRANSFER_MS: u32 = 4;
const KEY: PCWSTR = w!("Software\\Audio Kontrol 1\\ASIO");

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub sample_rate: u32,
    pub buffer_multiple: u32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { sample_rate: 48_000, buffer_multiple: 2 }
    }
}

impl Settings {
    /// Reads the settings, falling back to the default for each missing or invalid value.
    pub fn load() -> Settings {
        let default = Settings::default();
        let sample_rate = read_dword(w!("SampleRate")).filter(|r| RATES.contains(r));
        let buffer_multiple = read_dword(w!("BufferMultiple")).filter(|m| BUFFER_MULTIPLES.contains(m));
        Settings {
            sample_rate: sample_rate.unwrap_or(default.sample_rate),
            buffer_multiple: buffer_multiple.unwrap_or(default.buffer_multiple),
        }
    }

    pub fn save(&self) -> Result<()> {
        write_dword(w!("SampleRate"), self.sample_rate)?;
        write_dword(w!("BufferMultiple"), self.buffer_multiple)
    }

    /// The buffer size, in frames, to offer a host running at `rate`.
    pub fn buffer_frames(&self, rate: u32) -> u32 {
        min_buffer_frames(rate) * self.buffer_multiple
    }
}

/// At least one USB transfer plus some slack, as a power of two.
pub fn min_buffer_frames(rate: u32) -> u32 {
    (rate * TRANSFER_MS * 9 / 8).div_ceil(1000).next_power_of_two()
}

pub fn max_buffer_frames(rate: u32) -> u32 {
    min_buffer_frames(rate) * BUFFER_MULTIPLES[BUFFER_MULTIPLES.len() - 1]
}

/// Opens the settings key, creating it if needed, for change notifications.
pub fn open_key() -> Result<HKEY> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(HKEY_CURRENT_USER, KEY, None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_READ, None, &mut key, None)
    }
    .ok()?;
    Ok(key)
}

fn read_dword(name: PCWSTR) -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            KEY,
            name,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(value)
}

fn write_dword(name: PCWSTR, value: u32) -> Result<()> {
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            KEY,
            name,
            REG_DWORD.0,
            Some((&raw const value).cast()),
            size_of::<u32>() as u32,
        )
    };
    status.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_hold_a_usb_transfer_and_keep_their_length_across_rates() {
        let sizes = RATES.map(min_buffer_frames);
        assert_eq!(sizes, [256, 256, 512, 512, 1024]);
        for (rate, size) in RATES.into_iter().zip(sizes) {
            assert!(size * 1000 >= rate * TRANSFER_MS);
        }
        let settings = Settings { sample_rate: 48_000, buffer_multiple: 4 };
        assert_eq!(settings.buffer_frames(96_000), 2048);
        assert_eq!(max_buffer_frames(192_000), 8192);
    }
}
