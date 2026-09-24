//! Full-duplex streaming over the driver's three endpoints, opened in WASAPI
//! exclusive event-driven mode. Capture drives the ASIO buffer switch; output
//! blocks go through small FIFOs to the render endpoints, which the same
//! device clock paces.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
    IConnector, IDeviceTopology, IMMDevice, IMMDeviceEnumerator, IPart, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eCapture, eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent, WaitForMultipleObjects,
};
use windows::core::{Interface, Result, w};

pub const INPUTS: usize = 2;
pub const OUTPUTS: usize = 4;
const USB_VID_PID: &str = "vid_17cc&pid_0815";
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
const WAIT_MS: u32 = 1000;

pub struct Endpoints {
    output12: IMMDevice,
    output34: IMMDevice,
    input12: IMMDevice,
}

/// Finds the endpoints whose KS filter is one of our circuits.
pub fn find_endpoints() -> Result<Option<Endpoints>> {
    // Hosts usually call from a thread that already joined an apartment;
    // MMDevice and WASAPI objects work from either kind.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
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
fn filter_path(device: &IMMDevice) -> Result<String> {
    let topology: IDeviceTopology = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let connector = unsafe { topology.GetConnector(0)? };
    let connected: IConnector = unsafe { connector.GetConnectedTo()? };
    let part: IPart = connected.cast()?;
    let filter = unsafe { part.GetTopologyObject()?.GetDeviceId()? };
    let path = unsafe { filter.to_string()? };
    unsafe { CoTaskMemFree(Some(filter.0.cast())) };
    Ok(path)
}

struct Stream {
    client: IAudioClient,
    event: HANDLE,
    frames: u32,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
        let _ = unsafe { CloseHandle(self.event) };
    }
}

fn open_stream(device: &IMMDevice, rate: u32, frames: u32) -> Result<Stream> {
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
    let hns = |frames: u32| ((u64::from(frames) * 10_000_000 + u64::from(rate) / 2) / u64::from(rate)) as i64;
    let initialize = |period: i64| -> Result<IAudioClient> {
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period,
                period,
                (&raw const format).cast(),
                None,
            )?
        };
        Ok(client)
    };
    let client = match initialize(hns(frames)) {
        Err(e) if e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED => {
            let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
            let aligned = unsafe { client.GetBufferSize()? };
            initialize(hns(aligned))?
        }
        other => other?,
    };
    let event = unsafe { CreateEventW(None, false, false, None)? };
    unsafe { client.SetEventHandle(event)? };
    let frames = unsafe { client.GetBufferSize()? };
    Ok(Stream { client, event, frames })
}

/// Host side of a session, copied to the streaming thread.
#[derive(Clone)]
pub struct Host {
    pub callbacks: crate::abi::AsioCallbacks,
    pub time_info: bool,
    pub buffer_frames: usize,
    pub rate: u32,
    /// Double buffers, `2 * buffer_frames` samples each, by channel number.
    pub inputs: [Option<usize>; INPUTS],
    pub outputs: [Option<usize>; OUTPUTS],
}

/// Position and time of the most recent buffer switch.
#[derive(Default)]
pub struct Clock {
    pub sample_position: AtomicU64,
    pub system_time_ns: AtomicU64,
}

pub struct Duplex {
    renders: [Stream; 2],
    capture: Stream,
    stop: HANDLE,
    thread: Option<JoinHandle<()>>,
}

// COM interfaces opened on a multithreaded apartment thread may be used from
// any thread of that apartment, which the streaming thread joins.
unsafe impl Send for Duplex {}

impl Duplex {
    pub fn open(endpoints: &Endpoints, rate: u32, buffer_frames: u32) -> Result<Duplex> {
        Ok(Duplex {
            renders: [
                open_stream(&endpoints.output12, rate, buffer_frames)?,
                open_stream(&endpoints.output34, rate, buffer_frames)?,
            ],
            capture: open_stream(&endpoints.input12, rate, buffer_frames)?,
            stop: unsafe { CreateEventW(None, true, false, None)? },
            thread: None,
        })
    }

    pub fn start(&mut self, host: Host, clock: Arc<Clock>) -> Result<()> {
        for render in &self.renders {
            let service: IAudioRenderClient = unsafe { render.client.GetService()? };
            unsafe { service.GetBuffer(render.frames)? };
            unsafe { service.ReleaseBuffer(render.frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)? };
        }
        let worker = Worker {
            capture: unsafe { self.capture.client.GetService()? },
            renders: [unsafe { self.renders[0].client.GetService()? }, unsafe {
                self.renders[1].client.GetService()?
            }],
            render_frames: [self.renders[0].frames, self.renders[1].frames],
            events: [self.stop, self.capture.event, self.renders[0].event, self.renders[1].event],
            host,
            clock,
        };
        unsafe { windows::Win32::System::Threading::ResetEvent(self.stop)? };
        for client in [&self.capture.client, &self.renders[0].client, &self.renders[1].client] {
            unsafe { client.Start()? };
        }
        self.thread = Some(std::thread::spawn(move || worker.run()));
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = unsafe { SetEvent(self.stop) };
            let _ = thread.join();
        }
        for client in [&self.capture.client, &self.renders[0].client, &self.renders[1].client] {
            let _ = unsafe { client.Stop() };
            let _ = unsafe { client.Reset() };
        }
    }
}

impl Drop for Duplex {
    fn drop(&mut self) {
        self.stop();
        let _ = unsafe { CloseHandle(self.stop) };
    }
}

struct Worker {
    capture: IAudioCaptureClient,
    renders: [IAudioRenderClient; 2],
    render_frames: [u32; 2],
    events: [HANDLE; 4],
    host: Host,
    clock: Arc<Clock>,
}

unsafe impl Send for Worker {}

impl Worker {
    fn run(self) {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let mut task_index = 0;
        let task = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) };
        let frames = self.host.buffer_frames;
        let mut captured: VecDeque<[i32; 2]> = VecDeque::with_capacity(8 * frames);
        let mut outputs: [VecDeque<[i32; 2]>; 2] = std::array::from_fn(|_| VecDeque::with_capacity(8 * frames));
        for fifo in &mut outputs {
            fifo.extend(std::iter::repeat_n([0; 2], frames));
        }
        let mut index = 0;
        let mut position = 0u64;
        let (mut qpc_hz, mut qpc) = (0i64, 0i64);
        let _ = unsafe { QueryPerformanceFrequency(&mut qpc_hz) };
        loop {
            let wait = unsafe { WaitForMultipleObjects(&self.events, false, WAIT_MS) };
            if wait == WAIT_TIMEOUT {
                continue;
            }
            match wait.0.wrapping_sub(WAIT_OBJECT_0.0) {
                1 => {
                    if self.drain_capture(&mut captured).is_err() {
                        break;
                    }
                    while captured.len() >= frames {
                        let _ = unsafe { QueryPerformanceCounter(&mut qpc) };
                        let now_ns = (qpc as u128 * 1_000_000_000 / qpc_hz as u128) as u64;
                        self.clock.sample_position.store(position, Ordering::Release);
                        self.clock.system_time_ns.store(now_ns, Ordering::Release);
                        self.switch(index, &mut captured, &mut outputs, position, now_ns);
                        position += frames as u64;
                        index ^= 1;
                    }
                }
                n @ (2 | 3) => {
                    let k = n as usize - 2;
                    if self.feed_render(k, &mut outputs[k]).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
        if let Ok(task) = task {
            let _ = unsafe { AvRevertMmThreadCharacteristics(task) };
        }
    }

    fn drain_capture(&self, captured: &mut VecDeque<[i32; 2]>) -> Result<()> {
        loop {
            let available = unsafe { self.capture.GetNextPacketSize()? };
            if available == 0 {
                return Ok(());
            }
            let (mut data, mut count, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            unsafe { self.capture.GetBuffer(&mut data, &mut count, &mut flags, None, None)? };
            let samples = unsafe { std::slice::from_raw_parts(data.cast::<[i32; 2]>(), count as usize) };
            captured.extend(samples.iter().copied());
            unsafe { self.capture.ReleaseBuffer(count)? };
        }
    }

    fn switch(
        &self,
        index: usize,
        captured: &mut VecDeque<[i32; 2]>,
        outputs: &mut [VecDeque<[i32; 2]>; 2],
        position: u64,
        now_ns: u64,
    ) {
        let frames = self.host.buffer_frames;
        let half = |base: usize| (base as *mut i32).wrapping_add(index * frames);
        for (i, frame) in captured.drain(..frames).enumerate() {
            for (channel, sample) in frame.into_iter().enumerate() {
                if let Some(base) = self.host.inputs[channel] {
                    unsafe { half(base).add(i).write(sample) };
                }
            }
        }

        let callbacks = &self.host.callbacks;
        if self.host.time_info {
            let mut time: crate::abi::AsioTime = unsafe { std::mem::zeroed() };
            time.time_info.speed = 1.0;
            time.time_info.system_time = now_ns.into();
            time.time_info.sample_position = position.into();
            time.time_info.sample_rate = f64::from(self.host.rate);
            time.time_info.flags =
                crate::abi::K_SYSTEM_TIME_VALID | crate::abi::K_SAMPLE_POSITION_VALID | crate::abi::K_SAMPLE_RATE_VALID;
            unsafe { (callbacks.buffer_switch_time_info)(&mut time, index as i32, 1) };
        } else {
            unsafe { (callbacks.buffer_switch)(index as i32, 1) };
        }

        for (pair, fifo) in outputs.iter_mut().enumerate() {
            for i in 0..frames {
                let sample = |channel: usize| {
                    self.host.outputs[channel].map_or(0, |base| unsafe { half(base).add(i).read() })
                };
                fifo.push_back([sample(2 * pair), sample(2 * pair + 1)]);
            }
            let excess = fifo.len().saturating_sub(4 * frames);
            fifo.drain(..excess);
        }
    }

    fn feed_render(&self, k: usize, fifo: &mut VecDeque<[i32; 2]>) -> Result<()> {
        let count = self.render_frames[k];
        let data = unsafe { self.renders[k].GetBuffer(count)? }.cast::<[i32; 2]>();
        for i in 0..count as usize {
            unsafe { data.add(i).write(fifo.pop_front().unwrap_or([0; 2])) };
        }
        unsafe { self.renders[k].ReleaseBuffer(count, 0) }
    }
}
