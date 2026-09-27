//! ASIO sessions. The ASIO driver streams through the private property set of
//! `ak1_proto::asio` on the Output12 circuit instead of through an endpoint,
//! so the endpoints stay free for Windows and both are mixed here.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use acx_sys::{
    ACX_PROPERTY_ITEM, ACX_PROPERTY_ITEM_FLAG_GET, ACX_PROPERTY_ITEM_FLAG_SET, ACX_REQUEST_PARAMETERS, ACXCIRCUIT,
    ACXOBJECT, PACXCIRCUIT_INIT, call_acx,
};
use ak1_proto::SampleRate;
use ak1_proto::asio::{self, Buffers, INPUTS, MAX_PERIOD_FRAMES, OUTPUTS, ReadHeader, StartParams};
use wdk_sys::ntddk::{IoGetRequestorProcessId, KeSetEvent, ObReferenceObjectByHandle, ObfDereferenceObject, PsGetCurrentProcessId};
use wdk_sys::{
    _MODE, EVENT_MODIFY_STATE, ExEventObjectType, GUID, HANDLE, IO_SOUND_INCREMENT, NT_SUCCESS, NTSTATUS, PKEVENT, PVOID,
    STATUS_BUFFER_TOO_SMALL, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_BUFFER_SIZE, STATUS_INVALID_DEVICE_REQUEST,
    STATUS_INVALID_DEVICE_STATE, STATUS_INVALID_PARAMETER, STATUS_NOT_SUPPORTED, STATUS_SUCCESS, WDFFILEOBJECT,
    WDFREQUEST, call_unsafe_wdf_function_binding,
};

use crate::audio::Audio;
use crate::device_context;
use crate::stream::{MICROFRAMES_PER_SECOND, PACKETS_PER_TRANSFER};

static PROPERTY_SET: GUID = {
    let (data1, data2, data3, data4) = asio::PROPERTY_SET;
    GUID { Data1: data1, Data2: data2, Data3: data3, Data4: data4 }
};

const PROPERTY_COUNT: usize = 4;

/// The framework takes the table through a mutable pointer, so it lives in
/// writable memory like the C samples' tables.
static mut PROPERTIES: [ACX_PROPERTY_ITEM; PROPERTY_COUNT] = [
    item(asio::PROPERTY_START, ACX_PROPERTY_ITEM_FLAG_SET, evt_start, size_of::<StartParams>()),
    item(asio::PROPERTY_STOP, ACX_PROPERTY_ITEM_FLAG_SET, evt_stop, 0),
    item(asio::PROPERTY_READ, ACX_PROPERTY_ITEM_FLAG_GET, evt_read, 0),
    item(asio::PROPERTY_WRITE, ACX_PROPERTY_ITEM_FLAG_SET, evt_write, 0),
];

/// Offers the property set on the circuit `init` creates.
pub unsafe fn assign_properties(init: PACXCIRCUIT_INIT) -> NTSTATUS {
    unsafe { call_acx!(AcxCircuitInitAssignProperties, init, (&raw mut PROPERTIES).cast(), PROPERTY_COUNT as u32) }
}

const fn item(
    id: u32,
    flags: u32,
    handler: unsafe extern "C" fn(ACXOBJECT, WDFREQUEST),
    value_bytes: usize,
) -> ACX_PROPERTY_ITEM {
    ACX_PROPERTY_ITEM {
        Set: &raw const PROPERTY_SET,
        Id: id,
        Flags: flags,
        EvtAcxObjectProcessRequest: Some(handler),
        Reserved: core::ptr::null_mut(),
        ControlCb: 0,
        ValueCb: value_bytes as u32,
        ValueType: 0,
    }
}

/// A session's frames, moved between the USB engine and the ASIO driver.
/// Caller holds the audio lock for every method but `new`.
pub struct Session {
    pub owner: WDFFILEOBJECT,
    event: Event,
    buffers: Buffers,
}

impl Session {
    fn new(owner: WDFFILEOBJECT, event: Event, rate: SampleRate, period: usize) -> Result<Box<Session>, NTSTATUS> {
        let transfer = (rate.hz() as usize * PACKETS_PER_TRANSFER).div_ceil(MICROFRAMES_PER_SECOND as usize);
        let buffers = Buffers::new(period, transfer).map_err(|_| STATUS_INSUFFICIENT_RESOURCES)?;
        Ok(Box::new(Session { owner, event, buffers }))
    }

    pub fn capture(&mut self, frame: [i32; INPUTS]) {
        if self.buffers.capture(frame) {
            self.event.set();
        }
    }

    pub fn render(&mut self) -> [i32; OUTPUTS] {
        self.buffers.render()
    }

    pub fn stats(&self) -> [u32; 2] {
        self.buffers.stats()
    }
}

/// Room for `len` frames. Periods pass through it because the request's own
/// buffer may be pageable, which rules it out under the audio lock.
fn frame_buffer<const C: usize>(len: usize) -> Result<Vec<[i32; C]>, NTSTATUS> {
    let mut frames = Vec::new();
    frames.try_reserve_exact(len).map_err(|_| STATUS_INSUFFICIENT_RESOURCES)?;
    frames.resize(len, [0; C]);
    Ok(frames)
}

/// An event of the ASIO driver's process, referenced so it can be set from
/// any context.
struct Event(PKEVENT);

impl Event {
    /// References `handle` in the process that sent `request`. The framework
    /// calls property handlers in the requesting thread, where the handle
    /// is valid; the check keeps a handle from being looked up in another
    /// process should that change.
    unsafe fn reference(request: WDFREQUEST, handle: u64) -> Result<Event, NTSTATUS> {
        let irp = unsafe { call_unsafe_wdf_function_binding!(WdfRequestWdmGetIrp, request) };
        if unsafe { IoGetRequestorProcessId(irp) } as usize != unsafe { PsGetCurrentProcessId() } as usize {
            return Err(STATUS_INVALID_DEVICE_STATE);
        }
        let mut object: PVOID = core::ptr::null_mut();
        let status = unsafe {
            ObReferenceObjectByHandle(
                handle as usize as HANDLE,
                EVENT_MODIFY_STATE,
                *ExEventObjectType,
                _MODE::UserMode as i8,
                &mut object,
                core::ptr::null_mut(),
            )
        };
        if !NT_SUCCESS(status) {
            return Err(status);
        }
        Ok(Event(object.cast()))
    }

    fn set(&self) {
        unsafe { KeSetEvent(self.0, IO_SOUND_INCREMENT as i32, 0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { ObfDereferenceObject(self.0.cast()) };
    }
}

unsafe extern "C" fn evt_start(object: ACXOBJECT, request: WDFREQUEST) {
    let result = unsafe { start(object.cast(), request) };
    unsafe { complete(request, result.map(|()| 0)) };
}

unsafe fn start(circuit: ACXCIRCUIT, request: WDFREQUEST) -> Result<(), NTSTATUS> {
    let (value, len) = unsafe { property_value(request) };
    if value.is_null() || len < size_of::<StartParams>() {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let params = unsafe { value.cast::<StartParams>().read_unaligned() };
    let rate = SampleRate::from_hz(params.rate_hz).ok_or(STATUS_NOT_SUPPORTED)?;
    if params.period_frames == 0 || params.period_frames > MAX_PERIOD_FRAMES {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let owner = unsafe { file_object(request)? };
    let event = unsafe { Event::reference(request, params.event)? };
    let session = Session::new(owner, event, rate, params.period_frames as usize)?;
    unsafe { audio(circuit).start_asio(session, rate) }
}

unsafe extern "C" fn evt_stop(object: ACXOBJECT, request: WDFREQUEST) {
    let result = unsafe { file_object(request) }.map(|owner| {
        unsafe { audio(object.cast()).stop_asio(owner) };
        0
    });
    unsafe { complete(request, result) };
}

unsafe extern "C" fn evt_read(object: ACXOBJECT, request: WDFREQUEST) {
    let result = unsafe { read(object.cast(), request) };
    unsafe { complete(request, result) };
}

/// Fills the value with the oldest complete period, if any; returns its length.
unsafe fn read(circuit: ACXCIRCUIT, request: WDFREQUEST) -> Result<usize, NTSTATUS> {
    let owner = unsafe { file_object(request)? };
    let (value, len) = unsafe { property_value(request) };
    let room = len.checked_sub(size_of::<ReadHeader>()).filter(|_| !value.is_null()).ok_or(STATUS_BUFFER_TOO_SMALL)?;
    let mut frames = frame_buffer((room / size_of::<[i32; INPUTS]>()).min(MAX_PERIOD_FRAMES as usize))?;
    let read = unsafe { audio(circuit).with_asio(owner, |session| session.buffers.read(&mut frames))? };
    let count = read.ok_or(STATUS_BUFFER_TOO_SMALL)?;
    unsafe { value.cast::<ReadHeader>().write_unaligned(ReadHeader { frames: count as u32 }) };
    let samples = unsafe { value.add(size_of::<ReadHeader>()) }.cast::<[i32; INPUTS]>();
    for (i, frame) in frames[..count].iter().enumerate() {
        unsafe { samples.add(i).write_unaligned(frame.map(|sample| sample << 8)) };
    }
    Ok(size_of::<ReadHeader>() + count * size_of::<[i32; INPUTS]>())
}

unsafe extern "C" fn evt_write(object: ACXOBJECT, request: WDFREQUEST) {
    let result = unsafe { write(object.cast(), request) };
    unsafe { complete(request, result.map(|()| 0)) };
}

/// Queues the value, a period of output.
unsafe fn write(circuit: ACXCIRCUIT, request: WDFREQUEST) -> Result<(), NTSTATUS> {
    let owner = unsafe { file_object(request)? };
    let (value, len) = unsafe { property_value(request) };
    let count = len / size_of::<[i32; OUTPUTS]>();
    if value.is_null() || !len.is_multiple_of(size_of::<[i32; OUTPUTS]>()) || count > MAX_PERIOD_FRAMES as usize {
        return Err(STATUS_INVALID_BUFFER_SIZE);
    }
    let mut frames = frame_buffer(count)?;
    let samples = value.cast::<[i32; OUTPUTS]>();
    for (i, frame) in frames.iter_mut().enumerate() {
        *frame = unsafe { samples.add(i).read_unaligned() }.map(|sample| sample >> 8);
    }
    if unsafe { audio(circuit).with_asio(owner, |session| session.buffers.write(&frames))? } {
        Ok(())
    } else {
        Err(STATUS_INVALID_BUFFER_SIZE)
    }
}

unsafe fn property_value(request: WDFREQUEST) -> (*mut u8, usize) {
    let mut params: ACX_REQUEST_PARAMETERS = unsafe { core::mem::zeroed() };
    params.Size = size_of::<ACX_REQUEST_PARAMETERS>() as u16;
    unsafe { call_acx!(AcxRequestGetParameters, request, &mut params) };
    let property = unsafe { params.Parameters.Property.as_ref() };
    (property.Value.cast(), property.ValueCb as usize)
}

unsafe fn file_object(request: WDFREQUEST) -> Result<WDFFILEOBJECT, NTSTATUS> {
    let file = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetFileObject, request) };
    if file.is_null() { Err(STATUS_INVALID_DEVICE_REQUEST) } else { Ok(file) }
}

unsafe fn audio<'a>(circuit: ACXCIRCUIT) -> &'a Audio {
    let device = unsafe { call_acx!(AcxCircuitGetWdfDevice, circuit) };
    unsafe { &(*device_context(device)).audio }
}

unsafe fn complete(request: WDFREQUEST, result: Result<usize, NTSTATUS>) {
    let (status, information) = match result {
        Ok(bytes) => (STATUS_SUCCESS, bytes as u64),
        Err(status) => (status, 0),
    };
    unsafe { call_unsafe_wdf_function_binding!(WdfRequestCompleteWithInformation, request, status, information) };
}
