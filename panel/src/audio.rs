//! Test tones and the input meter, through the Windows shared-mode endpoints.

use std::f64::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IConnector,
    IDeviceTopology, IMMDevice, IMMDeviceEnumerator, IPart, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    eCapture, eRender,
};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::Interface;

pub type Result<T> = std::result::Result<T, String>;

const USB_VID_PID: &str = "vid_17cc&pid_0815";
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const BUFFER_HNS: i64 = 1_000_000;
const LEVEL: f64 = 0.25;
const FADE_SECONDS: f64 = 0.01;
/// Low tone on the left channel, then a high tone on the right.
const TONES: [(usize, f64, f64, f64); 2] = [(0, 440.0, 0.0, 0.8), (1, 880.0, 1.0, 1.8)];
const TEST_SECONDS: f64 = 1.8;

#[derive(Clone, Copy)]
pub enum Pair {
    Outputs12,
    Outputs34,
}

impl Pair {
    fn device(self, endpoints: Endpoints) -> IMMDevice {
        match self {
            Pair::Outputs12 => endpoints.output12,
            Pair::Outputs34 => endpoints.output34,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Pair::Outputs12 => "outputs 1/2",
            Pair::Outputs34 => "outputs 3/4",
        }
    }
}

/// Plays the channel test on `pair` and returns once it has played out.
pub fn play_test(pair: Pair) -> Result<()> {
    in_mta(|| {
        let device = pair.device(endpoints()?);
        let fail = |e: windows::core::Error| format!("Playing on {} failed: {}", pair.name(), e.message());
        let Shared { client, rate, channels } = open_shared(&device).map_err(fail)?;
        let render: IAudioRenderClient = unsafe { client.GetService() }.map_err(fail)?;
        let buffer_frames = unsafe { client.GetBufferSize() }.map_err(fail)?;
        let total = (TEST_SECONDS * f64::from(rate)) as u64;
        let fill = |first: u64, frames: u32| -> windows::core::Result<()> {
            let data = unsafe { render.GetBuffer(frames)? }.cast::<f32>();
            for frame in 0..frames as usize {
                let seconds = (first + frame as u64) as f64 / f64::from(rate);
                for channel in 0..channels {
                    unsafe { data.add(frame * channels + channel).write(test_signal(seconds, channel) as f32) };
                }
            }
            unsafe { render.ReleaseBuffer(frames, 0) }
        };
        fill(0, buffer_frames).map_err(fail)?;
        let mut written = u64::from(buffer_frames);
        unsafe { client.Start() }.map_err(fail)?;
        while written < total {
            std::thread::sleep(Duration::from_millis(10));
            let frames = buffer_frames - unsafe { client.GetCurrentPadding() }.map_err(fail)?;
            fill(written, frames).map_err(fail)?;
            written += u64::from(frames);
        }
        while unsafe { client.GetCurrentPadding() }.map_err(fail)? > 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe { client.Stop() }.map_err(fail)
    })
}

fn test_signal(seconds: f64, channel: usize) -> f64 {
    TONES
        .iter()
        .filter(|&&(c, _, start, end)| c == channel && (start..end).contains(&seconds))
        .map(|&(_, hz, start, end)| {
            let fade = ((seconds - start).min(end - seconds) / FADE_SECONDS).min(1.0);
            (TAU * hz * seconds).sin() * LEVEL * fade
        })
        .sum()
}

/// Peak levels of inputs 1 and 2, measured on a thread while the meter exists.
pub struct Meter {
    shared: Arc<MeterShared>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct MeterShared {
    stop: AtomicBool,
    /// Peaks since the last `take_peaks`, as f32 bits.
    peaks: [AtomicU32; 2],
    error: std::sync::Mutex<Option<String>>,
}

impl Meter {
    pub fn start() -> Meter {
        let shared = Arc::new(MeterShared::default());
        let thread_shared = shared.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = in_mta(|| measure(&thread_shared)) {
                *thread_shared.error.lock().unwrap() = Some(e);
            }
        });
        Meter { shared, thread: Some(thread) }
    }

    pub fn take_peaks(&self) -> [f32; 2] {
        self.shared.peaks.each_ref().map(|peak| f32::from_bits(peak.swap(0, Ordering::AcqRel)))
    }

    pub fn take_error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().take()
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn measure(shared: &MeterShared) -> Result<()> {
    let device = endpoints()?.input12;
    let fail = |e: windows::core::Error| format!("Opening inputs 1/2 failed: {}", e.message());
    let Shared { client, channels, .. } = open_shared(&device).map_err(fail)?;
    let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(fail)?;
    unsafe { client.Start() }.map_err(fail)?;
    let fail = |e: windows::core::Error| format!("Reading inputs 1/2 failed: {}", e.message());
    while !shared.stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(10));
        while unsafe { capture.GetNextPacketSize() }.map_err(fail)? > 0 {
            let (mut data, mut count, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            unsafe { capture.GetBuffer(&mut data, &mut count, &mut flags, None, None) }.map_err(fail)?;
            let samples = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), count as usize * channels) };
            for (i, peak) in shared.peaks.iter().enumerate().take(channels) {
                let loudest = samples.iter().skip(i).step_by(channels).fold(0f32, |max, s| max.max(s.abs()));
                let _ = peak.fetch_update(Ordering::AcqRel, Ordering::Acquire, |bits| {
                    Some(f32::from_bits(bits).max(loudest).to_bits())
                });
            }
            unsafe { capture.ReleaseBuffer(count) }.map_err(fail)?;
        }
    }
    let _ = unsafe { client.Stop() };
    Ok(())
}

struct Shared {
    client: IAudioClient,
    rate: u32,
    channels: usize,
}

/// Opens `device` in shared mode with the engine's float mix format.
fn open_shared(device: &IMMDevice) -> windows::core::Result<Shared> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let format = unsafe { client.GetMixFormat()? };
    let (rate, channels, float) = describe(format);
    let result = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, format, None) };
    unsafe { CoTaskMemFree(Some(format.cast())) };
    result?;
    if !float {
        return Err(windows::core::Error::new(
            windows::Win32::Foundation::E_UNEXPECTED,
            "the Windows mix format is not 32-bit float",
        ));
    }
    Ok(Shared { client, rate, channels })
}

fn describe(format: *const WAVEFORMATEX) -> (u32, usize, bool) {
    let f = unsafe { format.read_unaligned() };
    let sub_format = || unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() }.SubFormat;
    let float = f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16
        || (f.wFormatTag == WAVE_FORMAT_EXTENSIBLE && sub_format() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    (f.nSamplesPerSec, usize::from(f.nChannels), float && f.wBitsPerSample == 32)
}

struct Endpoints {
    output12: IMMDevice,
    output34: IMMDevice,
    input12: IMMDevice,
}

/// Finds the endpoints whose KS filter is one of the card's circuits.
fn find_endpoints() -> windows::core::Result<Option<Endpoints>> {
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let (mut output12, mut output34, mut input12) = (None, None, None);
    for flow in [eRender, eCapture] {
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
        for i in 0..unsafe { collection.GetCount()? } {
            let device = unsafe { collection.Item(i)? };
            let Ok(filter) = filter_path(&device) else { continue };
            let filter = filter.to_ascii_lowercase();
            if !filter.contains(USB_VID_PID) {
                continue;
            }
            let slot = if filter.ends_with("\\output12") {
                &mut output12
            } else if filter.ends_with("\\output34") {
                &mut output34
            } else if filter.ends_with("\\input12") {
                &mut input12
            } else {
                continue;
            };
            *slot = Some(device);
        }
    }
    Ok(match (output12, output34, input12) {
        (Some(output12), Some(output34), Some(input12)) => Some(Endpoints { output12, output34, input12 }),
        _ => None,
    })
}

/// Device path of the KS filter behind an endpoint, whose reference string
/// is the circuit name.
fn filter_path(device: &IMMDevice) -> windows::core::Result<String> {
    let topology: IDeviceTopology = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let connector = unsafe { topology.GetConnector(0)? };
    let connected: IConnector = unsafe { connector.GetConnectedTo()? };
    let part: IPart = connected.cast()?;
    let filter = unsafe { part.GetTopologyObject()?.GetDeviceId()? };
    let path = unsafe { filter.to_string()? };
    unsafe { CoTaskMemFree(Some(filter.0.cast())) };
    Ok(path)
}

fn endpoints() -> Result<Endpoints> {
    match find_endpoints() {
        Ok(Some(endpoints)) => Ok(endpoints),
        Ok(None) => Err("The card's audio endpoints are not there; is it connected?".into()),
        Err(e) => Err(format!("Listing audio endpoints failed: {}", e.message())),
    }
}

/// Runs `f` on this thread in the multithreaded apartment, which the
/// endpoint lookup needs.
fn in_mta<R>(f: impl FnOnce() -> Result<R>) -> Result<R> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().map_err(|e| e.message())?;
    let result = f();
    unsafe { CoUninitialize() };
    result
}
