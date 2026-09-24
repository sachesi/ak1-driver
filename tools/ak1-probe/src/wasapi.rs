use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE,
    EDataFlow, IAudioCaptureClient, IAudioClient, IAudioClock, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eAll, eCapture, eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};

use crate::usb::Result;

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const SHARED_BUFFER_HNS: i64 = 1_000_000;
const EXCLUSIVE_BUFFER_HNS: i64 = 200_000;

#[derive(Clone, Copy)]
enum Sample {
    Float,
    /// 24 valid bits, left-justified in 32.
    Int32,
}

struct Open {
    client: IAudioClient,
    rate: u32,
    channels: u32,
    sample: Sample,
}

pub fn list() -> Result<()> {
    for (id, name, flow) in endpoints(eAll)? {
        println!("{} {id} {name}", if flow == eRender { "render " } else { "capture" });
    }
    Ok(())
}

/// Plays a sine tone and reports how fast the endpoint's clock advanced.
/// With `exclusive_rate`, the endpoint is opened exclusively at that rate.
pub fn play(endpoint: &str, seconds: f64, tone_hz: f64, exclusive_rate: Option<u32>) -> Result<()> {
    let Open { client, rate, channels, sample } = open(find(eRender, endpoint)?, exclusive_rate)?;
    let buffer_frames = unsafe { client.GetBufferSize()? };
    let render: IAudioRenderClient = unsafe { client.GetService()? };
    let clock: IAudioClock = unsafe { client.GetService()? };
    let clock_hz = unsafe { clock.GetFrequency()? };
    println!("{rate} Hz, {channels} ch, buffer {buffer_frames} frames");

    let mut phase = 0.0f64;
    let step = TAU * tone_hz / f64::from(rate);
    let mut fill = |frames: u32| -> Result<()> {
        if frames == 0 {
            return Ok(());
        }
        let data = unsafe { render.GetBuffer(frames)? };
        for i in 0..(frames * channels) as usize {
            let value = phase.sin() * 0.25;
            match sample {
                Sample::Float => unsafe { data.cast::<f32>().add(i).write(value as f32) },
                Sample::Int32 => unsafe { data.cast::<i32>().add(i).write((value * f64::from(i32::MAX)) as i32) },
            }
            if (i + 1) % channels as usize == 0 {
                phase = (phase + step) % TAU;
            }
        }
        unsafe { render.ReleaseBuffer(frames, 0)? };
        Ok(())
    };
    fill(buffer_frames)?;
    unsafe { client.Start()? };
    let start = Instant::now();
    let mut first = None;
    let mut last_report = 0;
    while start.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(2));
        let padding = unsafe { client.GetCurrentPadding()? };
        fill(buffer_frames - padding)?;
        let mut position = 0;
        unsafe { clock.GetPosition(&mut position, None)? };
        let now = Instant::now();
        if position > 0 && first.is_none() {
            first = Some((position, now));
        }
        let second = now.duration_since(start).as_secs();
        if let Some((p0, t0)) = first
            && second > last_report
            && now.duration_since(t0).as_secs_f64() > 1.0
        {
            last_report = second;
            let frames = (position - p0) as f64 / clock_hz as f64 * f64::from(rate);
            println!("{second:3} s  clock {:9.1} frames/s", frames / now.duration_since(t0).as_secs_f64());
        }
    }
    unsafe { client.Stop()? };
    Ok(())
}

/// Records and reports frame rate, discontinuities and the peak level.
pub fn record(endpoint: &str, seconds: f64, exclusive_rate: Option<u32>) -> Result<()> {
    let Open { client, rate, channels, sample } = open(find(eCapture, endpoint)?, exclusive_rate)?;
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    println!("{rate} Hz, {channels} ch");
    unsafe { client.Start()? };
    let start = Instant::now();
    let (mut frames, mut discontinuities, mut peak) = (0u64, 0u32, 0f64);
    while start.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(2));
        loop {
            let available = unsafe { capture.GetNextPacketSize()? };
            if available == 0 {
                break;
            }
            let (mut data, mut count, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            unsafe { capture.GetBuffer(&mut data, &mut count, &mut flags, None, None)? };
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                discontinuities += 1;
            }
            for i in 0..(count * channels) as usize {
                let value = match sample {
                    Sample::Float => f64::from(unsafe { data.cast::<f32>().add(i).read() }),
                    Sample::Int32 => f64::from(unsafe { data.cast::<i32>().add(i).read() }) / f64::from(i32::MAX),
                };
                peak = peak.max(value.abs());
            }
            frames += u64::from(count);
            unsafe { capture.ReleaseBuffer(count)? };
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    unsafe { client.Stop()? };
    println!(
        "frames {frames} in {elapsed:.2} s = {:.1} frames/s, discontinuities {discontinuities}, peak {:.1} dBFS",
        frames as f64 / elapsed,
        20.0 * peak.max(1e-9).log10()
    );
    Ok(())
}

fn open(device: IMMDevice, exclusive_rate: Option<u32>) -> Result<Open> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    match exclusive_rate {
        None => {
            let format = unsafe { client.GetMixFormat()? };
            let (rate, channels, float) = describe(format);
            let result = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, SHARED_BUFFER_HNS, 0, format, None) };
            unsafe { CoTaskMemFree(Some(format.cast())) };
            result?;
            if !float {
                return Err("mix format is not 32-bit float".into());
            }
            Ok(Open { client, rate, channels, sample: Sample::Float })
        }
        Some(rate) => {
            let format = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                    nChannels: 2,
                    nSamplesPerSec: rate,
                    nAvgBytesPerSec: rate * 8,
                    nBlockAlign: 8,
                    wBitsPerSample: 32,
                    cbSize: 22,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 24 },
                dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
                SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
            };
            unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    0,
                    EXCLUSIVE_BUFFER_HNS,
                    EXCLUSIVE_BUFFER_HNS / 2,
                    (&raw const format).cast(),
                    None,
                )?
            };
            Ok(Open { client, rate, channels: 2, sample: Sample::Int32 })
        }
    }
}

fn describe(format: *const WAVEFORMATEX) -> (u32, u32, bool) {
    let f = unsafe { format.read_unaligned() };
    let sub_format = || {
        let ext = unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
        let sub_format = ext.SubFormat;
        sub_format
    };
    let float = f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16
        || (f.wFormatTag == WAVE_FORMAT_EXTENSIBLE && sub_format() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    (f.nSamplesPerSec, u32::from(f.nChannels), float && f.wBitsPerSample == 32)
}

fn find(flow: EDataFlow, needle: &str) -> Result<IMMDevice> {
    let enumerator = device_enumerator()?;
    let matches: Vec<_> =
        endpoints(flow)?.into_iter().filter(|(id, name, _)| id.contains(needle) || name.contains(needle)).collect();
    match matches.as_slice() {
        [(id, _, _)] => Ok(unsafe { enumerator.GetDevice(&windows::core::HSTRING::from(id.as_str()))? }),
        [] => Err(format!("no active endpoint matches {needle:?}").into()),
        _ => Err(format!("{} endpoints match {needle:?}; use more of the id", matches.len()).into()),
    }
}

fn device_enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}

fn endpoints(flow: EDataFlow) -> Result<Vec<(String, String, EDataFlow)>> {
    let enumerator = device_enumerator()?;
    let mut out = Vec::new();
    for flow in if flow == eAll { vec![eRender, eCapture] } else { vec![flow] } {
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
        for i in 0..unsafe { collection.GetCount()? } {
            let device = unsafe { collection.Item(i)? };
            let id = unsafe { device.GetId()? };
            let id_string = unsafe { id.to_string()? };
            unsafe { CoTaskMemFree(Some(id.0.cast())) };
            let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
            let name = unsafe { store.GetValue(&PKEY_Device_FriendlyName)? }.to_string();
            out.push((id_string, name, flow));
        }
    }
    Ok(out)
}
