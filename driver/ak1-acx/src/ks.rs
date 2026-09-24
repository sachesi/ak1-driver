//! Kernel-streaming GUIDs and wave format layouts used by the audio circuits.
//! The GUIDs are defined here because the headers only declare them.

use wdk_sys::{DEVPROPKEY, GUID};

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> GUID {
    GUID { Data1: data1, Data2: data2, Data3: data3, Data4: data4 }
}

pub const KSCATEGORY_AUDIO: GUID = guid(0x6994ad04, 0x93ef, 0x11d0, [0xa3, 0xcc, 0x00, 0xa0, 0xc9, 0x22, 0x31, 0x96]);
pub const KSNODETYPE_LINE_CONNECTOR: GUID =
    guid(0xdff21fe3, 0xf70f, 0x11d0, [0xb9, 0x17, 0x00, 0xa0, 0xc9, 0x22, 0x31, 0x96]);
const KSDATAFORMAT_TYPE_AUDIO: GUID = guid(0x73647561, 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
const KSDATAFORMAT_SUBTYPE_PCM: GUID = guid(0x00000001, 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
const KSDATAFORMAT_SPECIFIER_WAVEFORMATEX: GUID =
    guid(0x05589f81, 0xc356, 0x11ce, [0xbf, 0x01, 0x00, 0xaa, 0x00, 0x55, 0x59, 0x5a]);

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const SPEAKER_FRONT_LEFT_RIGHT: u32 = 0x3;
const WAVEFORMATEXTENSIBLE_EXTRA_BYTES: u16 = 22;

/// `KSDATAFORMAT_WAVEFORMATEXTENSIBLE`. Every field of the packed
/// `WAVEFORMATEX` prefix happens to be naturally aligned, so `repr(C)`
/// reproduces the 104-byte header layout.
#[repr(C, align(8))]
pub struct KsWaveFormat {
    format_size: u32,
    flags: u32,
    sample_size: u32,
    reserved: u32,
    major_format: GUID,
    sub_format: GUID,
    specifier: GUID,
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    cb_size: u16,
    valid_bits_per_sample: u16,
    channel_mask: u32,
    pcm_sub_format: GUID,
}

const _: () = assert!(size_of::<KsWaveFormat>() == 104);

pub const DEVPKEY_KSAUDIO_PACKETSIZE_CONSTRAINTS2: DEVPROPKEY = DEVPROPKEY {
    fmtid: guid(0x9404f781, 0x7191, 0x409b, [0x8b, 0x0b, 0x80, 0xbf, 0x6e, 0xc2, 0x29, 0xae]),
    pid: 2,
};
const AUDIO_SIGNALPROCESSINGMODE_DEFAULT: GUID =
    guid(0xc18e2f7e, 0x933d, 0x4965, [0xb7, 0xd1, 0x1e, 0xef, 0x22, 0x8d, 0x2a, 0xf3]);
const AUDIO_SIGNALPROCESSINGMODE_RAW: GUID =
    guid(0x9e90ea20, 0xb493, 0x4fd1, [0xa1, 0xa8, 0x7e, 0x13, 0x61, 0xa9, 0x56, 0xcf]);
const HNS_PER_MS: u32 = 10_000;

/// `KSAUDIO_PACKETSIZE_CONSTRAINTS2` with one entry per processing mode.
#[repr(C)]
pub struct PacketSizeConstraints {
    min_packet_period_hns: u32,
    packet_size_file_alignment: u32,
    max_packet_size_bytes: u32,
    mode_count: u32,
    modes: [ModeConstraint; 2],
}

#[repr(C)]
struct ModeConstraint {
    mode: GUID,
    samples_per_packet: u32,
    packet_duration_hns: u32,
}

/// Lets exclusive-mode clients use periods down to one USB transfer instead
/// of the default of about 3 ms.
pub static PACKET_SIZE_CONSTRAINTS: PacketSizeConstraints = PacketSizeConstraints {
    min_packet_period_hns: HNS_PER_MS,
    packet_size_file_alignment: 0,
    max_packet_size_bytes: 0,
    mode_count: 2,
    modes: [
        ModeConstraint { mode: AUDIO_SIGNALPROCESSINGMODE_RAW, samples_per_packet: 0, packet_duration_hns: HNS_PER_MS },
        ModeConstraint { mode: AUDIO_SIGNALPROCESSINGMODE_DEFAULT, samples_per_packet: 0, packet_duration_hns: HNS_PER_MS },
    ],
};

impl KsWaveFormat {
    /// Interleaved stereo integer PCM in `container_bits`-bit samples.
    pub fn stereo_pcm(rate_hz: u32, container_bits: u16, valid_bits: u16) -> KsWaveFormat {
        let block_align = 2 * container_bits / 8;
        KsWaveFormat {
            format_size: size_of::<KsWaveFormat>() as u32,
            flags: 0,
            sample_size: block_align.into(),
            reserved: 0,
            major_format: KSDATAFORMAT_TYPE_AUDIO,
            sub_format: KSDATAFORMAT_SUBTYPE_PCM,
            specifier: KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
            format_tag: WAVE_FORMAT_EXTENSIBLE,
            channels: 2,
            samples_per_sec: rate_hz,
            avg_bytes_per_sec: rate_hz * u32::from(block_align),
            block_align,
            bits_per_sample: container_bits,
            cb_size: WAVEFORMATEXTENSIBLE_EXTRA_BYTES,
            valid_bits_per_sample: valid_bits,
            channel_mask: SPEAKER_FRONT_LEFT_RIGHT,
            pcm_sub_format: KSDATAFORMAT_SUBTYPE_PCM,
        }
    }
}
