use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, EDataFlow, IAudioCaptureClient,
    IAudioClient, IAudioClock, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eAll, eCapture, eRender,
};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};

use crate::usb::Result;

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const BUFFER_HNS: i64 = 1_000_000;

pub fn list() -> Result<()> {
    for (id, name, flow) in endpoints(eAll)? {
        println!("{} {id} {name}", if flow == eRender { "render " } else { "capture" });
    }
    Ok(())
}

/// Plays a sine tone and reports how fast the endpoint's clock advanced.
pub fn play(endpoint: &str, seconds: f64, tone_hz: f64) -> Result<()> {
    let device = find(eRender, endpoint)?;
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let format = unsafe { client.GetMixFormat()? };
    let (rate, channels, float) = describe(format);
    if !float {
        return Err("mix format is not 32-bit float".into());
    }
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, format, None)? };
    let buffer_frames = unsafe { client.GetBufferSize()? };
    let render: IAudioRenderClient = unsafe { client.GetService()? };
    let clock: IAudioClock = unsafe { client.GetService()? };
    let clock_hz = unsafe { clock.GetFrequency()? };
    println!("mix format {rate} Hz, {channels} ch float, buffer {buffer_frames} frames");

    let mut phase = 0.0f64;
    let step = TAU * tone_hz / f64::from(rate);
    let mut fill = |frames: u32| -> Result<()> {
        if frames == 0 {
            return Ok(());
        }
        let data = unsafe { render.GetBuffer(frames)? }.cast::<f32>();
        let samples = unsafe { std::slice::from_raw_parts_mut(data, (frames * channels) as usize) };
        for frame in samples.chunks_exact_mut(channels as usize) {
            frame.fill((phase.sin() * 0.25) as f32);
            phase = (phase + step) % TAU;
        }
        unsafe { render.ReleaseBuffer(frames, 0)? };
        Ok(())
    };
    fill(buffer_frames)?;
    unsafe { client.Start()? };
    let start = Instant::now();
    let mut first = None;
    while start.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(10));
        let padding = unsafe { client.GetCurrentPadding()? };
        fill(buffer_frames - padding)?;
        let mut position = 0;
        unsafe { clock.GetPosition(&mut position, None)? };
        let now = Instant::now();
        if position > 0 && first.is_none() {
            first = Some((position, now));
        }
        if let Some((p0, t0)) = first {
            let elapsed = now.duration_since(t0).as_secs_f64();
            if elapsed > 1.0 && (now.duration_since(start).as_millis() % 1000) < 10 {
                let rate = (position - p0) as f64 / clock_hz as f64 * f64::from(rate) / elapsed;
                println!("{:5.1} s  clock {:9.1} frames/s", now.duration_since(start).as_secs_f64(), rate);
            }
        }
    }
    unsafe { client.Stop()? };
    unsafe { CoTaskMemFree(Some(format.cast())) };
    Ok(())
}

/// Records and reports frame rate, discontinuities and the peak level.
pub fn record(endpoint: &str, seconds: f64) -> Result<()> {
    let device = find(eCapture, endpoint)?;
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let format = unsafe { client.GetMixFormat()? };
    let (rate, channels, float) = describe(format);
    if !float {
        return Err("mix format is not 32-bit float".into());
    }
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, format, None)? };
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    println!("mix format {rate} Hz, {channels} ch float");
    unsafe { client.Start()? };
    let start = Instant::now();
    let (mut frames, mut discontinuities, mut peak) = (0u64, 0u32, 0f32);
    while start.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(10));
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
            let samples = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), (count * channels) as usize) };
            peak = samples.iter().fold(peak, |p, s| p.max(s.abs()));
            frames += u64::from(count);
            unsafe { capture.ReleaseBuffer(count)? };
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    unsafe { client.Stop()? };
    unsafe { CoTaskMemFree(Some(format.cast())) };
    println!(
        "frames {frames} in {elapsed:.2} s = {:.1} frames/s, discontinuities {discontinuities}, peak {:.1} dBFS",
        frames as f64 / elapsed,
        20.0 * peak.max(1e-9).log10()
    );
    Ok(())
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
    let matches: Vec<_> = endpoints(flow)?.into_iter().filter(|(id, name, _)| id.contains(needle) || name.contains(needle)).collect();
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
