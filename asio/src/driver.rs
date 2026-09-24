//! The `IASIO` object handed to hosts.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Media::Audio::AUDCLNT_E_UNSUPPORTED_FORMAT;
use windows::Win32::System::Com::{CO_MTA_USAGE_COOKIE, CoDecrementMTAUsage, CoIncrementMTAUsage};
use windows_core::{GUID, IUnknown, Interface};

use crate::abi::*;
use crate::duplex::{Clock, Duplex, Endpoints, Host, INPUTS, OUTPUTS, find_endpoints};

pub const CLSID: GUID = GUID::from_u128(0x3f1c6b2a_8e4d_4c7b_9a15_2d6e8f0b4c31);
pub const DRIVER_NAME: &str = "Audio Kontrol 1";
const DRIVER_VERSION: i32 = 1;
const RATES: [u32; 5] = [44_100, 48_000, 88_200, 96_000, 192_000];
const DEFAULT_RATE: u32 = 48_000;
/// Audio the driver moves per USB transfer, which bounds the smallest buffer.
const TRANSFER_MS: u32 = 4;

struct Session {
    duplex: Duplex,
    #[expect(dead_code, reason = "owns the memory behind the host's channel pointers")]
    buffers: Vec<Box<[i32]>>,
    host: Host,
    active_inputs: [bool; INPUTS],
    active_outputs: [bool; OUTPUTS],
    running: bool,
}

struct Driver {
    /// Keeps a multithreaded apartment alive so the endpoints work from the
    /// host's threads and the streaming thread without joining the host's
    /// threads to an apartment.
    mta: Option<CO_MTA_USAGE_COOKIE>,
    endpoints: Option<Endpoints>,
    error: String,
    rate: u32,
    session: Option<Session>,
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.session = None;
        self.endpoints = None;
        if let Some(cookie) = self.mta.take() {
            let _ = unsafe { CoDecrementMTAUsage(cookie) };
        }
    }
}

#[repr(C)]
pub struct AsioObject {
    vtbl: &'static IAsioVtbl<AsioObject>,
    refs: AtomicU32,
    driver: Mutex<Driver>,
    clock: Arc<Clock>,
}

// The endpoints and streams are free-threaded COM objects.
unsafe impl Send for Driver {}

impl AsioObject {
    pub fn create() -> *mut AsioObject {
        Box::into_raw(Box::new(AsioObject {
            vtbl: &VTBL,
            refs: AtomicU32::new(1),
            driver: Mutex::new(Driver {
                mta: None,
                endpoints: None,
                error: String::new(),
                rate: DEFAULT_RATE,
                session: None,
            }),
            clock: Arc::new(Clock::default()),
        }))
    }

    fn driver(&self) -> MutexGuard<'_, Driver> {
        self.driver.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Driver {
    fn fail(&mut self, error: AsioError, message: impl Into<String>) -> AsioError {
        self.error = message.into();
        error
    }
}

fn min_buffer_frames(rate: u32) -> i32 {
    // At least one USB transfer plus some slack, as a power of two.
    ((rate * TRANSFER_MS * 9 / 8).div_ceil(1000)).next_power_of_two() as i32
}

static VTBL: IAsioVtbl<AsioObject> = IAsioVtbl {
    query_interface,
    add_ref,
    release,
    init,
    get_driver_name,
    get_driver_version,
    get_error_message,
    start,
    stop,
    get_channels,
    get_latencies,
    get_buffer_size,
    can_sample_rate,
    get_sample_rate,
    set_sample_rate,
    get_clock_sources,
    set_clock_source,
    get_sample_position,
    get_channel_info,
    create_buffers,
    dispose_buffers,
    control_panel,
    future,
    output_ready,
};

unsafe extern "system" fn query_interface(this: *mut AsioObject, iid: *const GUID, out: *mut *mut c_void) -> i32 {
    if out.is_null() || iid.is_null() {
        return E_POINTER.0;
    }
    // Hosts ask for the driver's CLSID as the interface id.
    let iid = unsafe { *iid };
    if iid == IUnknown::IID || iid == CLSID {
        unsafe { add_ref(this) };
        unsafe { *out = this.cast() };
        S_OK.0
    } else {
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE.0
    }
}

unsafe extern "system" fn add_ref(this: *mut AsioObject) -> u32 {
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn release(this: *mut AsioObject) -> u32 {
    let left = unsafe { (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        drop(unsafe { Box::from_raw(this) });
        crate::object_released();
    }
    left
}

unsafe extern "system" fn init(this: *mut AsioObject, _sys_handle: *mut c_void) -> AsioBool {
    let mut driver = unsafe { (*this).driver() };
    if driver.mta.is_none() {
        match unsafe { CoIncrementMTAUsage() } {
            Ok(cookie) => driver.mta = Some(cookie),
            Err(e) => {
                driver.error = format!("starting COM failed: {e}");
                return 0;
            }
        }
    }
    match find_endpoints() {
        Ok(Some(endpoints)) => {
            driver.endpoints = Some(endpoints);
            1
        }
        Ok(None) => {
            driver.error = "Audio Kontrol 1 endpoints not found; is the device connected?".into();
            0
        }
        Err(e) => {
            driver.error = format!("enumerating audio endpoints failed: {e}");
            0
        }
    }
}

unsafe extern "system" fn get_driver_name(_this: *mut AsioObject, name: *mut u8) {
    unsafe { write_c_string(name, 32, DRIVER_NAME) };
}

unsafe extern "system" fn get_driver_version(_this: *mut AsioObject) -> i32 {
    DRIVER_VERSION
}

unsafe extern "system" fn get_error_message(this: *mut AsioObject, message: *mut u8) {
    let driver = unsafe { (*this).driver() };
    unsafe { write_c_string(message, 124, &driver.error) };
}

unsafe extern "system" fn start(this: *mut AsioObject) -> AsioError {
    let object = unsafe { &*this };
    let mut driver = object.driver();
    let Some(session) = driver.session.as_mut() else {
        return driver.fail(ASE_INVALID_MODE, "start called before createBuffers");
    };
    if session.running {
        return ASE_OK;
    }
    match session.duplex.start(session.host.clone(), object.clock.clone()) {
        Ok(()) => {
            session.running = true;
            ASE_OK
        }
        Err(e) => driver.fail(ASE_HW_MALFUNCTION, format!("starting streams failed: {e}")),
    }
}

unsafe extern "system" fn stop(this: *mut AsioObject) -> AsioError {
    let mut driver = unsafe { (*this).driver() };
    if let Some(session) = driver.session.as_mut()
        && session.running
    {
        session.duplex.stop();
        session.running = false;
    }
    ASE_OK
}

unsafe extern "system" fn get_channels(_this: *mut AsioObject, inputs: *mut i32, outputs: *mut i32) -> AsioError {
    unsafe {
        *inputs = INPUTS as i32;
        *outputs = OUTPUTS as i32;
    }
    ASE_OK
}

unsafe extern "system" fn get_latencies(this: *mut AsioObject, input: *mut i32, output: *mut i32) -> AsioError {
    let driver = unsafe { (*this).driver() };
    let frames = driver.session.as_ref().map_or(min_buffer_frames(driver.rate), |s| s.host.buffer_frames as i32);
    let transfer = (driver.rate * TRANSFER_MS / 1000) as i32;
    unsafe {
        *input = frames + transfer;
        *output = 2 * frames + transfer;
    }
    ASE_OK
}

unsafe extern "system" fn get_buffer_size(
    this: *mut AsioObject,
    min: *mut i32,
    max: *mut i32,
    preferred: *mut i32,
    granularity: *mut i32,
) -> AsioError {
    let driver = unsafe { (*this).driver() };
    let smallest = min_buffer_frames(driver.rate);
    unsafe {
        *min = smallest;
        *max = 8 * smallest;
        *preferred = 2 * smallest;
        *granularity = -1;
    }
    ASE_OK
}

unsafe extern "system" fn can_sample_rate(_this: *mut AsioObject, rate: f64) -> AsioError {
    if RATES.iter().any(|&r| f64::from(r) == rate) { ASE_OK } else { ASE_NO_CLOCK }
}

unsafe extern "system" fn get_sample_rate(this: *mut AsioObject, rate: *mut f64) -> AsioError {
    unsafe { *rate = f64::from((*this).driver().rate) };
    ASE_OK
}

unsafe extern "system" fn set_sample_rate(this: *mut AsioObject, rate: f64) -> AsioError {
    let mut driver = unsafe { (*this).driver() };
    let Some(&rate) = RATES.iter().find(|&&r| f64::from(r) == rate) else {
        return driver.fail(ASE_NO_CLOCK, format!("unsupported sample rate {rate}"));
    };
    if rate == driver.rate {
        return ASE_OK;
    }
    if let Some(session) = driver.session.as_ref() {
        // The streams are opened at one rate; ask the host to rebuild them.
        let callbacks = session.host.callbacks;
        driver.rate = rate;
        drop(driver);
        unsafe { (callbacks.asio_message)(K_ASIO_RESET_REQUEST, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
        return ASE_OK;
    }
    driver.rate = rate;
    ASE_OK
}

unsafe extern "system" fn get_clock_sources(
    _this: *mut AsioObject,
    clocks: *mut AsioClockSource,
    count: *mut i32,
) -> AsioError {
    if clocks.is_null() || count.is_null() || unsafe { *count } < 1 {
        return ASE_INVALID_PARAMETER;
    }
    let mut name = [0u8; 32];
    unsafe { write_c_string(name.as_mut_ptr(), name.len(), "Internal") };
    unsafe {
        clocks.write(AsioClockSource {
            index: 0,
            associated_channel: -1,
            associated_group: -1,
            is_current_source: 1,
            name,
        });
        *count = 1;
    }
    ASE_OK
}

unsafe extern "system" fn set_clock_source(_this: *mut AsioObject, reference: i32) -> AsioError {
    if reference == 0 { ASE_OK } else { ASE_INVALID_PARAMETER }
}

unsafe extern "system" fn get_sample_position(
    this: *mut AsioObject,
    position: *mut AsioU64,
    timestamp: *mut AsioU64,
) -> AsioError {
    let clock = unsafe { &(*this).clock };
    unsafe {
        *position = clock.sample_position.load(Ordering::Acquire).into();
        *timestamp = clock.system_time_ns.load(Ordering::Acquire).into();
    }
    ASE_OK
}

unsafe extern "system" fn get_channel_info(this: *mut AsioObject, info: *mut AsioChannelInfo) -> AsioError {
    let driver = unsafe { (*this).driver() };
    let info = unsafe { &mut *info };
    let (count, prefix) = if info.is_input != 0 { (INPUTS, "In") } else { (OUTPUTS, "Out") };
    let Ok(channel) = usize::try_from(info.channel) else { return ASE_INVALID_PARAMETER };
    if channel >= count {
        return ASE_INVALID_PARAMETER;
    }
    info.is_active = driver.session.as_ref().is_some_and(|s| {
        if info.is_input != 0 { s.active_inputs[channel] } else { s.active_outputs[channel] }
    }) as AsioBool;
    info.channel_group = 0;
    info.sample_type = ASIO_ST_INT32_LSB;
    unsafe { write_c_string(info.name.as_mut_ptr(), info.name.len(), &format!("{prefix} {}", channel + 1)) };
    ASE_OK
}

unsafe extern "system" fn create_buffers(
    this: *mut AsioObject,
    infos: *mut AsioBufferInfo,
    channels: i32,
    buffer_size: i32,
    callbacks: *const AsioCallbacks,
) -> AsioError {
    let mut driver = unsafe { (*this).driver() };
    if driver.session.is_some() {
        return driver.fail(ASE_INVALID_MODE, "buffers already exist");
    }
    let Some(endpoints) = driver.endpoints.as_ref() else {
        return driver.fail(ASE_NOT_PRESENT, "driver not initialized");
    };
    let rate = driver.rate;
    let min = min_buffer_frames(rate);
    if infos.is_null() || callbacks.is_null() || channels <= 0 || buffer_size < min || buffer_size > 8 * min {
        return driver.fail(ASE_INVALID_PARAMETER, format!("buffer size {buffer_size} outside {min}..={}", 8 * min));
    }
    let frames = buffer_size as usize;
    let infos = unsafe { std::slice::from_raw_parts_mut(infos, channels as usize) };
    let mut active_inputs = [false; INPUTS];
    let mut active_outputs = [false; OUTPUTS];
    for info in infos.iter() {
        let limit = if info.is_input != 0 { INPUTS } else { OUTPUTS };
        if !usize::try_from(info.channel_num).is_ok_and(|c| c < limit) {
            return driver.fail(ASE_INVALID_PARAMETER, format!("no channel {}", info.channel_num));
        }
    }

    let mut buffers = Vec::with_capacity(infos.len());
    for info in infos.iter_mut() {
        let mut buffer = vec![0i32; 2 * frames].into_boxed_slice();
        let base = buffer.as_mut_ptr();
        info.buffers = [base.cast(), unsafe { base.add(frames) }.cast()];
        if info.is_input != 0 {
            active_inputs[info.channel_num as usize] = true;
        } else {
            active_outputs[info.channel_num as usize] = true;
        }
        buffers.push(buffer);
    }
    let mut host = Host {
        callbacks: unsafe { *callbacks },
        time_info: false,
        buffer_frames: frames,
        rate,
        inputs: [None; INPUTS],
        outputs: [None; OUTPUTS],
    };
    for info in infos.iter() {
        let base = info.buffers[0] as usize;
        if info.is_input != 0 {
            host.inputs[info.channel_num as usize] = Some(base);
        } else {
            host.outputs[info.channel_num as usize] = Some(base);
        }
    }
    host.time_info = unsafe {
        ((*callbacks).asio_message)(K_ASIO_SUPPORTS_TIME_INFO, 0, std::ptr::null_mut(), std::ptr::null_mut())
    } == 1;
    let duplex = match Duplex::open(endpoints, &host) {
        Ok(duplex) => duplex,
        // The device has one clock, which another stream may hold at another rate.
        Err(e) if e.code() == AUDCLNT_E_UNSUPPORTED_FORMAT => {
            return driver.fail(ASE_NO_CLOCK, format!("another application is using the device at a rate other than {rate} Hz"));
        }
        Err(e) => return driver.fail(ASE_HW_MALFUNCTION, format!("opening the device at {rate} Hz failed: {e}")),
    };
    driver.session = Some(Session { duplex, buffers, host, active_inputs, active_outputs, running: false });
    ASE_OK
}

unsafe extern "system" fn dispose_buffers(this: *mut AsioObject) -> AsioError {
    let session = unsafe { (*this).driver() }.session.take();
    drop(session);
    ASE_OK
}

unsafe extern "system" fn control_panel(_this: *mut AsioObject) -> AsioError {
    ASE_NOT_PRESENT
}

unsafe extern "system" fn future(_this: *mut AsioObject, selector: i32, _opt: *mut c_void) -> AsioError {
    if selector == K_ASIO_CAN_TIME_INFO { ASE_SUCCESS } else { ASE_INVALID_PARAMETER }
}

unsafe extern "system" fn output_ready(_this: *mut AsioObject) -> AsioError {
    ASE_NOT_PRESENT
}

