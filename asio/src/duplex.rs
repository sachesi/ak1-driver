//! Full-duplex streaming over the driver's endpoints, opened in WASAPI
//! exclusive event-driven mode. Only the endpoints whose channels the host
//! activated are opened. Capture drives the ASIO buffer switch when inputs are
//! active, otherwise the first render endpoint does; output blocks go through
//! small FIFOs to the render endpoints, which the same device clock paces.

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
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, ResetEvent, SetEvent,
    WaitForMultipleObjects,
};
use windows::core::{Interface, Result, w};

use crate::abi::{K_ASIO_RESET_REQUEST, K_ASIO_SELECTOR_SUPPORTED};

pub const INPUTS: usize = 2;
pub const OUTPUTS: usize = 4;
const USB_VID_PID: &str = "vid_17cc&pid_0815";
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;
/// Without an audio event for this long the device is considered gone.
const WAIT_MS: u32 = 1000;

pub struct Endpoints {
    output12: IMMDevice,
    output34: IMMDevice,
    input12: IMMDevice,
}

/// Finds the endpoints whose KS filter is one of our circuits. Needs a
/// multithreaded apartment in the process (see `CoIncrementMTAUsage`).
pub fn find_endpoints() -> Result<Option<Endpoints>> {
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

impl Host {
    fn uses_input(&self) -> bool {
        self.inputs.iter().any(Option::is_some)
    }

    fn uses_output_pair(&self, pair: usize) -> bool {
        self.outputs[2 * pair..2 * pair + 2].iter().any(Option::is_some)
    }
}

/// Position and time of the most recent buffer switch.
#[derive(Default)]
pub struct Clock {
    pub sample_position: AtomicU64,
    pub system_time_ns: AtomicU64,
}

pub struct Duplex {
    renders: [Option<Stream>; 2],
    capture: Option<Stream>,
    stop: HANDLE,
    thread: Option<JoinHandle<()>>,
}

// COM interfaces opened on a multithreaded apartment thread may be used from
// any thread of that apartment, which the streaming thread joins.
unsafe impl Send for Duplex {}

impl Duplex {
    /// Opens the endpoints that carry the channels `host` uses.
    pub fn open(endpoints: &Endpoints, host: &Host) -> Result<Duplex> {
        let (rate, frames) = (host.rate, host.buffer_frames as u32);
        let open = |device: &IMMDevice, used: bool| used.then(|| open_stream(device, rate, frames)).transpose();
        Ok(Duplex {
            renders: [
                open(&endpoints.output12, host.uses_output_pair(0))?,
                open(&endpoints.output34, host.uses_output_pair(1))?,
            ],
            capture: open(&endpoints.input12, host.uses_input())?,
            stop: unsafe { CreateEventW(None, true, false, None)? },
            thread: None,
        })
    }

    fn streams(&self) -> impl Iterator<Item = &Stream> {
        self.capture.iter().chain(self.renders.iter().flatten())
    }

    pub fn start(&mut self, host: Host, clock: Arc<Clock>) -> Result<()> {
        let mut renders = [None, None];
        let mut events = vec![self.stop];
        let mut sources = vec![Source::Stop];
        if let Some(capture) = &self.capture {
            events.push(capture.event);
            sources.push(Source::Capture);
        }
        for (pair, render) in self.renders.iter().enumerate() {
            let Some(render) = render else { continue };
            let service: IAudioRenderClient = unsafe { render.client.GetService()? };
            unsafe { service.GetBuffer(render.frames)? };
            unsafe { service.ReleaseBuffer(render.frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)? };
            renders[pair] = Some(Render { client: service, frames: render.frames });
            events.push(render.event);
            sources.push(Source::Render(pair));
        }
        let capture = self.capture.as_ref().map(|c| unsafe { c.client.GetService() }).transpose()?;
        let worker = Worker { capture, renders, events, sources, host, clock };
        unsafe { ResetEvent(self.stop)? };
        for stream in self.streams() {
            unsafe { stream.client.Start()? };
        }
        self.thread = Some(std::thread::spawn(move || worker.run()));
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = unsafe { SetEvent(self.stop) };
            let _ = thread.join();
        }
        for stream in self.streams() {
            let _ = unsafe { stream.client.Stop() };
            let _ = unsafe { stream.client.Reset() };
        }
    }
}

impl Drop for Duplex {
    fn drop(&mut self) {
        self.stop();
        let _ = unsafe { CloseHandle(self.stop) };
    }
}

#[derive(Clone, Copy)]
enum Source {
    Stop,
    Capture,
    Render(usize),
}

struct Render {
    client: IAudioRenderClient,
    frames: u32,
}

struct Worker {
    capture: Option<IAudioCaptureClient>,
    renders: [Option<Render>; 2],
    events: Vec<HANDLE>,
    sources: Vec<Source>,
    host: Host,
    clock: Arc<Clock>,
}

unsafe impl Send for Worker {}

/// Streaming state owned by the worker thread.
struct State {
    captured: VecDeque<[i32; 2]>,
    outputs: [VecDeque<[i32; 2]>; 2],
    index: usize,
    position: u64,
    qpc_hz: i64,
}

impl Worker {
    fn run(self) {
        let mut task_index = 0;
        let task = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) };
        let frames = self.host.buffer_frames;
        let mut state = State {
            captured: VecDeque::with_capacity(8 * frames),
            outputs: std::array::from_fn(|_| VecDeque::from(vec![[0; 2]; frames])),
            index: 0,
            position: 0,
            qpc_hz: 0,
        };
        let _ = unsafe { QueryPerformanceFrequency(&mut state.qpc_hz) };
        // Without inputs, the first render endpoint paces the buffer switches.
        let pacing_render = if self.capture.is_some() { None } else { self.renders.iter().position(Option::is_some) };

        let failed = loop {
            let wait = unsafe { WaitForMultipleObjects(&self.events, false, WAIT_MS) };
            if wait == WAIT_TIMEOUT {
                break true;
            }
            let signaled = wait.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            let result = match self.sources.get(signaled) {
                Some(Source::Stop) => break false,
                Some(Source::Capture) => self.drain_capture(&mut state.captured).map(|()| {
                    while state.captured.len() >= frames {
                        self.switch(&mut state);
                    }
                }),
                Some(&Source::Render(pair)) => {
                    if pacing_render == Some(pair) {
                        let needed = self.renders[pair].as_ref().map_or(0, |r| r.frames as usize);
                        while state.outputs[pair].len() < needed {
                            self.switch(&mut state);
                        }
                    }
                    self.feed_render(pair, &mut state.outputs[pair])
                }
                None => break true,
            };
            if result.is_err() {
                break true;
            }
        };
        if failed {
            self.request_reset();
        }
        if let Ok(task) = task {
            let _ = unsafe { AvRevertMmThreadCharacteristics(task) };
        }
    }

    /// Asks the host to tear the session down and build it again.
    fn request_reset(&self) {
        let message = self.host.callbacks.asio_message;
        let null = std::ptr::null_mut();
        if unsafe { message(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_RESET_REQUEST, null, null.cast()) } == 1 {
            unsafe { message(K_ASIO_RESET_REQUEST, 0, null, null.cast()) };
        }
    }

    fn drain_capture(&self, captured: &mut VecDeque<[i32; 2]>) -> Result<()> {
        let Some(capture) = &self.capture else { return Ok(()) };
        loop {
            let available = unsafe { capture.GetNextPacketSize()? };
            if available == 0 {
                return Ok(());
            }
            let (mut data, mut count, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            unsafe { capture.GetBuffer(&mut data, &mut count, &mut flags, None, None)? };
            let samples = unsafe { std::slice::from_raw_parts(data.cast::<[i32; 2]>(), count as usize) };
            captured.extend(samples.iter().copied());
            unsafe { capture.ReleaseBuffer(count)? };
        }
    }

    fn switch(&self, state: &mut State) {
        let frames = self.host.buffer_frames;
        let index = state.index;
        let mut qpc = 0;
        let _ = unsafe { QueryPerformanceCounter(&mut qpc) };
        let now_ns = (qpc as u128 * 1_000_000_000 / state.qpc_hz as u128) as u64;
        self.clock.sample_position.store(state.position, Ordering::Release);
        self.clock.system_time_ns.store(now_ns, Ordering::Release);

        let half = |base: usize| (base as *mut i32).wrapping_add(index * frames);
        if self.capture.is_some() {
            for (i, frame) in state.captured.drain(..frames).enumerate() {
                for (channel, sample) in frame.into_iter().enumerate() {
                    if let Some(base) = self.host.inputs[channel] {
                        unsafe { half(base).add(i).write(sample) };
                    }
                }
            }
        }

        let callbacks = &self.host.callbacks;
        if self.host.time_info {
            let mut time: crate::abi::AsioTime = unsafe { std::mem::zeroed() };
            time.time_info.speed = 1.0;
            time.time_info.system_time = now_ns.into();
            time.time_info.sample_position = state.position.into();
            time.time_info.sample_rate = f64::from(self.host.rate);
            time.time_info.flags =
                crate::abi::K_SYSTEM_TIME_VALID | crate::abi::K_SAMPLE_POSITION_VALID | crate::abi::K_SAMPLE_RATE_VALID;
            unsafe { (callbacks.buffer_switch_time_info)(&mut time, index as i32, 1) };
        } else {
            unsafe { (callbacks.buffer_switch)(index as i32, 1) };
        }

        for (pair, fifo) in state.outputs.iter_mut().enumerate() {
            if self.renders[pair].is_none() {
                continue;
            }
            for i in 0..frames {
                let sample = |channel: usize| {
                    self.host.outputs[channel].map_or(0, |base| unsafe { half(base).add(i).read() })
                };
                fifo.push_back([sample(2 * pair), sample(2 * pair + 1)]);
            }
            let excess = fifo.len().saturating_sub(4 * frames);
            fifo.drain(..excess);
        }
        state.position += frames as u64;
        state.index ^= 1;
    }

    fn feed_render(&self, pair: usize, fifo: &mut VecDeque<[i32; 2]>) -> Result<()> {
        let Some(render) = &self.renders[pair] else { return Ok(()) };
        let data = unsafe { render.client.GetBuffer(render.frames)? }.cast::<[i32; 2]>();
        for i in 0..render.frames as usize {
            unsafe { data.add(i).write(fifo.pop_front().unwrap_or([0; 2])) };
        }
        unsafe { render.client.ReleaseBuffer(render.frames, 0) }
    }
}
