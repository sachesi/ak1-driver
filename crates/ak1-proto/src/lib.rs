//! Wire protocol of the Native Instruments Audio Kontrol 1 (USB 17cc:0815).
//!
//! The device exposes one vendor-specific interface. Alternate setting 0 has
//! the bulk command pipes on endpoint 1; alternate setting 1 adds the
//! isochronous audio pipes. The device is the clock master: every OUT packet
//! must have the same length as the IN packet it answers.
//!
//! The firmware ignores data toggle resets from SET_INTERFACE and
//! CLEAR_FEATURE(ENDPOINT_HALT), so each session must first repeat an
//! idempotent command such as [`get_device_info_request`] until it is answered.

#![no_std]

pub mod mode2;

pub const VENDOR_ID: u16 = 0x17cc;
pub const PRODUCT_ID: u16 = 0x0815;

pub const EP_CMD_OUT: u8 = 0x01;
pub const EP_CMD_IN: u8 = 0x81;
pub const EP_AUDIO_IN: u8 = 0x82;
pub const EP_AUDIO_OUT: u8 = 0x06;
pub const STREAMING_ALT_SETTING: u8 = 1;

pub const CMD_BUF_SIZE: usize = 64;
pub const MAX_PACKET_SIZE: usize = 512;
pub const CHANNELS_PER_STREAM: usize = 2;

/// Extra frames per microframe the device may send beyond the nominal rate.
const CLOCK_DRIFT_TOLERANCE: u32 = 5;
const DEPTH_24: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    GetDeviceInfo = 0x01,
    ReadErp = 0x02,
    ReadAnalog = 0x03,
    ReadIo = 0x04,
    WriteIo = 0x05,
    MidiRead = 0x06,
    MidiWrite = 0x07,
    AudioParams = 0x09,
    AutoMsg = 0x0b,
    DimmLeds = 0x0c,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleRate {
    Hz44100,
    Hz48000,
    Hz88200,
    Hz96000,
    Hz192000,
}

impl SampleRate {
    pub const ALL: [SampleRate; 5] = [
        SampleRate::Hz44100,
        SampleRate::Hz48000,
        SampleRate::Hz88200,
        SampleRate::Hz96000,
        SampleRate::Hz192000,
    ];

    pub const fn hz(self) -> u32 {
        match self {
            SampleRate::Hz44100 => 44_100,
            SampleRate::Hz48000 => 48_000,
            SampleRate::Hz88200 => 88_200,
            SampleRate::Hz96000 => 96_000,
            SampleRate::Hz192000 => 192_000,
        }
    }

    pub fn from_hz(hz: u32) -> Option<SampleRate> {
        SampleRate::ALL.into_iter().find(|r| r.hz() == hz)
    }

    /// Rate selector used by [`Command::AudioParams`]; not in ascending order.
    const fn code(self) -> u8 {
        match self {
            SampleRate::Hz44100 => 0,
            SampleRate::Hz48000 => 1,
            SampleRate::Hz96000 => 2,
            SampleRate::Hz192000 => 3,
            SampleRate::Hz88200 => 4,
        }
    }
}

/// Reply payload of [`Command::GetDeviceInfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceSpec {
    pub fw_version: u16,
    pub hw_subtype: u8,
    pub num_erp: u8,
    pub num_analog_in: u8,
    pub num_digital_in: u8,
    pub num_digital_out: u8,
    pub num_analog_audio_out: u8,
    pub num_analog_audio_in: u8,
    pub num_digital_audio_out: u8,
    pub num_digital_audio_in: u8,
    pub num_midi_out: u8,
    pub num_midi_in: u8,
    pub data_alignment: u8,
}

impl DeviceSpec {
    pub const WIRE_SIZE: usize = 14;

    pub fn parse(b: &[u8]) -> Option<DeviceSpec> {
        let b: &[u8; Self::WIRE_SIZE] = b.get(..Self::WIRE_SIZE)?.try_into().ok()?;
        Some(DeviceSpec {
            fw_version: u16::from_le_bytes([b[0], b[1]]),
            hw_subtype: b[2],
            num_erp: b[3],
            num_analog_in: b[4],
            num_digital_in: b[5],
            num_digital_out: b[6],
            num_analog_audio_out: b[7],
            num_analog_audio_in: b[8],
            num_digital_audio_out: b[9],
            num_digital_audio_in: b[10],
            num_midi_out: b[11],
            num_midi_in: b[12],
            data_alignment: b[13],
        })
    }

    pub fn capture_streams(&self) -> usize {
        usize::from(self.num_analog_audio_in.max(self.num_digital_audio_in)) / CHANNELS_PER_STREAM
    }

    pub fn playback_streams(&self) -> usize {
        usize::from(self.num_analog_audio_out.max(self.num_digital_audio_out)) / CHANNELS_PER_STREAM
    }

    /// Stereo streams carried in each packet, in both directions.
    pub fn streams(&self) -> usize {
        self.capture_streams().max(self.playback_streams())
    }

    /// Bytes each sample occupies on the wire; alignment modes 2 and 3 add a check byte.
    pub fn wire_bytes_per_sample(&self) -> usize {
        if self.data_alignment >= 2 { 4 } else { 3 }
    }

    /// Largest isochronous packet the device may send at `rate`.
    pub fn max_packet_bytes(&self, rate: SampleRate) -> u16 {
        let frames = (rate.hz() / 8000 + CLOCK_DRIFT_TOLERANCE) as usize;
        let bytes = frames * self.wire_bytes_per_sample() * CHANNELS_PER_STREAM * self.streams();
        bytes.min(MAX_PACKET_SIZE) as u16
    }
}

pub fn get_device_info_request() -> [u8; 1] {
    [Command::GetDeviceInfo as u8]
}

/// Selects sample rate, 24-bit depth and the maximum packet size.
pub fn audio_params_request(rate: SampleRate, max_packet_bytes: u16) -> [u8; 6] {
    let [lo, hi] = max_packet_bytes.to_le_bytes();
    [Command::AudioParams as u8, rate.code(), DEPTH_24, lo, hi, 1]
}

/// Enables unsolicited input reports; `erp` is the encoder report threshold.
pub fn auto_msg_request(digital: u8, analog: u8, erp: u8) -> [u8; 4] {
    [Command::AutoMsg as u8, digital, analog, erp]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply<'a> {
    DeviceInfo(DeviceSpec),
    AudioParams { accepted: bool },
    MidiRead { port: u8, data: &'a [u8] },
    ReadIo(&'a [u8]),
    ReadErp(&'a [u8]),
    ReadAnalog(&'a [u8]),
    Other { command: u8, payload: &'a [u8] },
}

impl<'a> Reply<'a> {
    pub fn parse(buf: &'a [u8]) -> Option<Reply<'a>> {
        let (&command, payload) = buf.split_first()?;
        let reply = match command {
            0x01 => Reply::DeviceInfo(DeviceSpec::parse(payload)?),
            0x09 => Reply::AudioParams { accepted: *payload.first()? == 1 },
            0x06 => {
                let (&port, rest) = payload.split_first()?;
                let (&len, rest) = rest.split_first()?;
                Reply::MidiRead { port, data: rest.get(..usize::from(len))? }
            }
            0x04 => Reply::ReadIo(payload),
            0x02 => Reply::ReadErp(payload),
            0x03 => Reply::ReadAnalog(payload),
            _ => Reply::Other { command, payload },
        };
        Some(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ak1_like_spec(data_alignment: u8) -> DeviceSpec {
        DeviceSpec {
            fw_version: 0x16,
            hw_subtype: 0,
            num_erp: 1,
            num_analog_in: 0,
            num_digital_in: 3,
            num_digital_out: 4,
            num_analog_audio_out: 4,
            num_analog_audio_in: 2,
            num_digital_audio_out: 0,
            num_digital_audio_in: 0,
            num_midi_out: 1,
            num_midi_in: 1,
            data_alignment,
        }
    }

    #[test]
    fn device_info_reply_round_trips_little_endian_firmware() {
        let buf = [0x01, 0x16, 0x00, 0, 1, 0, 3, 4, 4, 2, 0, 0, 1, 1, 0];
        let Some(Reply::DeviceInfo(spec)) = Reply::parse(&buf) else { panic!("not device info") };
        assert_eq!(spec, ak1_like_spec(0));
    }

    #[test]
    fn short_device_info_reply_is_rejected() {
        assert_eq!(Reply::parse(&[0x01, 0x16, 0x00]), None);
    }

    #[test]
    fn two_in_four_out_needs_two_streams() {
        let spec = ak1_like_spec(0);
        assert_eq!((spec.capture_streams(), spec.playback_streams(), spec.streams()), (1, 2, 2));
    }

    #[test]
    fn packet_size_at_192k_fits_endpoint() {
        assert_eq!(ak1_like_spec(0).max_packet_bytes(SampleRate::Hz192000), 29 * 3 * 2 * 2);
        assert_eq!(ak1_like_spec(3).max_packet_bytes(SampleRate::Hz192000), 29 * 4 * 2 * 2);
        assert_eq!(ak1_like_spec(3).max_packet_bytes(SampleRate::Hz44100), 10 * 4 * 2 * 2);
    }

    #[test]
    fn audio_params_encodes_rate_code_and_le_size() {
        assert_eq!(audio_params_request(SampleRate::Hz88200, 0x015c), [0x09, 4, 2, 0x5c, 0x01, 1]);
        assert_eq!(audio_params_request(SampleRate::Hz192000, 348), [0x09, 3, 2, 0x5c, 0x01, 1]);
    }

    #[test]
    fn midi_reply_length_is_bounded_by_buffer() {
        assert_eq!(
            Reply::parse(&[0x06, 0, 2, 0x90, 0x40]),
            Some(Reply::MidiRead { port: 0, data: &[0x90, 0x40] })
        );
        assert_eq!(Reply::parse(&[0x06, 0, 3, 0x90, 0x40]), None);
    }
}
