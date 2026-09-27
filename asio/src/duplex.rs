//! Full-duplex streaming through the kernel driver's ASIO property set (see
//! `ak1_proto::asio`), which leaves the Windows audio endpoints free: the
//! kernel driver mixes the host's output with them. It sets an event whenever
//! a period of input is complete; each period is read, handed to the host
//! and the host's output written back.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use ak1_proto::asio::{
    PROPERTY_READ, PROPERTY_SET, PROPERTY_START, PROPERTY_STOP, PROPERTY_WRITE, ReadHeader, StartParams,
    read_value_bytes,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW,
    CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Media::KernelStreaming::{
    IOCTL_KS_PROPERTY, KSCATEGORY_AUDIO, KSIDENTIFIER, KSIDENTIFIER_0, KSIDENTIFIER_0_0, KSPROPERTY_TYPE_GET,
    KSPROPERTY_TYPE_SET,
};
use windows::Win32::Media::timeGetTime;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, ResetEvent, SetEvent,
    WaitForMultipleObjects,
};
use windows::core::{Error, GUID, HSTRING, PCWSTR, Result, w};

pub use ak1_proto::asio::{INPUTS, OUTPUTS};

use crate::abi::{K_ASIO_RESET_REQUEST, K_ASIO_SELECTOR_SUPPORTED};

const USB_VID_PID: &str = "vid_17cc&pid_0815";
/// Reference string of the circuit that carries the property set.
const CIRCUIT: &str = "\\output12";
/// Without an input period for this long the device is considered gone.
const WAIT_MS: u32 = 1000;

/// Path of the card's filter that carries the property set, if the card is there.
pub fn find_device() -> Result<Option<HSTRING>> {
    loop {
        let mut len = 0;
        let status = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &KSCATEGORY_AUDIO,
                PCWSTR::null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if status != CR_SUCCESS {
            return Err(Error::new(E_FAIL, format!("listing audio devices failed with CONFIGRET {}", status.0)));
        }
        let mut list = vec![0u16; len as usize];
        let status = unsafe {
            CM_Get_Device_Interface_ListW(&KSCATEGORY_AUDIO, PCWSTR::null(), &mut list, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
        };
        // Devices that arrived in between make the list longer than measured.
        if status == CR_BUFFER_SMALL {
            continue;
        }
        if status != CR_SUCCESS {
            return Err(Error::new(E_FAIL, format!("listing audio devices failed with CONFIGRET {}", status.0)));
        }
        let found = list.split(|&c| c == 0).map(String::from_utf16_lossy).find(|path| {
            let path = path.to_ascii_lowercase();
            path.contains(USB_VID_PID) && path.ends_with(CIRCUIT)
        });
        return Ok(found.map(HSTRING::from));
    }
}

/// An open handle to the filter that carries the property set.
struct Device(HANDLE);

// The handle may be used from any thread.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    fn open(path: &HSTRING) -> Result<Device> {
        let handle = unsafe {
            CreateFileW(
                path,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )?
        };
        Ok(Device(handle))
    }

    /// Sends a property request with `value` as its value buffer; returns the bytes the driver filled.
    fn property(&self, id: u32, flags: u32, value: Option<&mut [u8]>) -> Result<usize> {
        let (data1, data2, data3, data4) = PROPERTY_SET;
        let property = KSIDENTIFIER {
            Anonymous: KSIDENTIFIER_0 {
                Anonymous: KSIDENTIFIER_0_0 { Set: GUID::from_values(data1, data2, data3, data4), Id: id, Flags: flags },
            },
        };
        let (value, len) = value.map_or((None, 0), |v| (Some(v.as_mut_ptr().cast()), v.len() as u32));
        let mut returned = 0;
        unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_KS_PROPERTY,
                Some((&raw const property).cast()),
                size_of::<KSIDENTIFIER>() as u32,
                value,
                len,
                Some(&mut returned),
                None,
            )?
        };
        Ok(returned as usize)
    }

    fn start(&self, params: StartParams) -> Result<()> {
        let mut bytes: [u8; size_of::<StartParams>()] = unsafe { std::mem::transmute(params) };
        self.property(PROPERTY_START, KSPROPERTY_TYPE_SET, Some(&mut bytes)).map(drop)
    }

    fn stop(&self) -> Result<()> {
        self.property(PROPERTY_STOP, KSPROPERTY_TYPE_SET, None).map(drop)
    }

    /// Reads the oldest complete input period into `value`; returns its frame count, 0 if none is complete.
    fn read(&self, value: &mut [u8]) -> Result<usize> {
        let filled = self.property(PROPERTY_READ, KSPROPERTY_TYPE_GET, Some(value))?;
        if filled < size_of::<ReadHeader>() {
            return Err(Error::new(E_FAIL, format!("the driver returned {filled} bytes of input")));
        }
        let header = unsafe { value.as_ptr().cast::<ReadHeader>().read_unaligned() };
        Ok(header.frames as usize)
    }

    fn write(&self, frames: &mut [[i32; OUTPUTS]]) -> Result<()> {
        let bytes = unsafe { std::slice::from_raw_parts_mut(frames.as_mut_ptr().cast(), size_of_val(frames)) };
        self.property(PROPERTY_WRITE, KSPROPERTY_TYPE_SET, Some(bytes)).map(drop)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
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
    device: Arc<Device>,
    /// Set by the kernel driver when a period of input is complete.
    period: HANDLE,
    stop: HANDLE,
    thread: Option<JoinHandle<()>>,
}

// Event handles may be used from any thread.
unsafe impl Send for Duplex {}

impl Duplex {
    pub fn open(path: &HSTRING) -> Result<Duplex> {
        let device = Arc::new(Device::open(path)?);
        let period = unsafe { CreateEventW(None, false, false, None)? };
        let stop = match unsafe { CreateEventW(None, true, false, None) } {
            Ok(stop) => stop,
            Err(e) => {
                let _ = unsafe { CloseHandle(period) };
                return Err(e);
            }
        };
        Ok(Duplex { device, period, stop, thread: None })
    }

    pub fn start(&mut self, host: Host, clock: Arc<Clock>) -> Result<()> {
        unsafe { ResetEvent(self.period)? };
        unsafe { ResetEvent(self.stop)? };
        self.device.start(StartParams {
            rate_hz: host.rate,
            period_frames: host.buffer_frames as u32,
            event: self.period.0 as u64,
        })?;
        let worker = Worker { device: self.device.clone(), events: [self.stop, self.period], host, clock };
        self.thread = Some(std::thread::spawn(move || worker.run()));
        Ok(())
    }

    /// Tells the streaming thread to end and hands it over, so that the
    /// caller can wait for it without holding locks the host's callbacks may need.
    pub fn signal_stop(&mut self) -> Option<JoinHandle<()>> {
        let thread = self.thread.take()?;
        let _ = unsafe { SetEvent(self.stop) };
        Some(thread)
    }

    pub fn stop_streams(&self) {
        let _ = self.device.stop();
    }
}

impl Drop for Duplex {
    fn drop(&mut self) {
        if let Some(thread) = self.signal_stop() {
            join_worker(thread);
        }
        self.stop_streams();
        let _ = unsafe { CloseHandle(self.stop) };
        let _ = unsafe { CloseHandle(self.period) };
    }
}

/// Waits for the streaming thread to end, unless this is that thread: a host
/// may stop the driver from a callback the thread is making.
pub fn join_worker(thread: JoinHandle<()>) {
    if thread.thread().id() != std::thread::current().id() {
        let _ = thread.join();
    }
}

/// Asks the host to tear the session down and build it again.
pub fn request_reset(callbacks: &crate::abi::AsioCallbacks) {
    let message = callbacks.asio_message;
    let null = std::ptr::null_mut();
    if unsafe { message(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_RESET_REQUEST, null, null.cast()) } == 1 {
        unsafe { message(K_ASIO_RESET_REQUEST, 0, null, null.cast()) };
    }
}

struct Worker {
    device: Arc<Device>,
    /// The stop event, then the period event.
    events: [HANDLE; 2],
    host: Host,
    clock: Arc<Clock>,
}

// Event handles may be used from any thread.
unsafe impl Send for Worker {}

/// Streaming state owned by the worker thread.
struct State {
    input: Vec<u8>,
    output: Vec<[i32; OUTPUTS]>,
    index: usize,
    position: u64,
}

impl Worker {
    fn run(self) {
        let mut task_index = 0;
        let task = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) };
        let frames = self.host.buffer_frames;
        let mut state = State {
            input: vec![0; read_value_bytes(frames as u32)],
            output: vec![[0; OUTPUTS]; frames],
            index: 0,
            position: 0,
        };
        let failed = loop {
            let wait = unsafe { WaitForMultipleObjects(&self.events, false, WAIT_MS) };
            if wait == WAIT_OBJECT_0 {
                break false;
            }
            if wait.0 != WAIT_OBJECT_0.0 + 1 || self.drain(&mut state).is_err() {
                break true;
            }
        };
        if failed {
            request_reset(&self.host.callbacks);
        }
        if let Ok(task) = task {
            let _ = unsafe { AvRevertMmThreadCharacteristics(task) };
        }
    }

    /// Hands every complete input period to the host and writes its output.
    fn drain(&self, state: &mut State) -> Result<()> {
        while self.device.read(&mut state.input)? > 0 {
            self.switch(state);
            self.device.write(&mut state.output)?;
        }
        Ok(())
    }

    fn switch(&self, state: &mut State) {
        let frames = self.host.buffer_frames;
        let index = state.index;
        // The ASIO specification requires timeGetTime as the source of system time.
        let now_ns = u64::from(unsafe { timeGetTime() }) * 1_000_000;
        self.clock.sample_position.store(state.position, Ordering::Release);
        self.clock.system_time_ns.store(now_ns, Ordering::Release);

        let half = |base: usize| (base as *mut i32).wrapping_add(index * frames);
        let samples = state.input[size_of::<ReadHeader>()..].as_chunks::<{ INPUTS * 4 }>().0;
        for (i, frame) in samples.iter().take(frames).enumerate() {
            for (channel, bytes) in frame.as_chunks::<4>().0.iter().enumerate() {
                if let Some(base) = self.host.inputs[channel] {
                    unsafe { half(base).add(i).write(i32::from_ne_bytes(*bytes)) };
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

        for (i, frame) in state.output.iter_mut().enumerate() {
            *frame = std::array::from_fn(|channel| {
                self.host.outputs[channel].map_or(0, |base| unsafe { half(base).add(i).read() })
            });
        }
        state.position += frames as u64;
        state.index ^= 1;
    }
}
