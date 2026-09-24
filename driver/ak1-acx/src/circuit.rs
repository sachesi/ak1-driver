//! Audio circuits: one stereo render circuit per output pair and one stereo
//! capture circuit for the inputs. Their names are the reference strings of
//! the device interfaces in the INF.

extern crate alloc;

use alloc::boxed::Box;

use acx_sys::{
    _ACX_CIRCUIT_TYPE, _ACX_PIN_COMMUNICATION, _ACX_PIN_TYPE, ACX_DATAFORMAT_CONFIG, ACX_PIN_CONFIG,
    ACX_RT_STREAM_CALLBACKS, ACX_STREAM_CALLBACKS, ACXCIRCUIT, ACXDATAFORMAT, ACXOBJECTBAG, ACXPIN, ACXSTREAM,
    PACXCIRCUIT_INIT, PACXSTREAM_INIT, call_acx,
};
use ak1_proto::SampleRate;
use wdk_sys::{GUID, NT_SUCCESS, NTSTATUS, STATUS_INSUFFICIENT_RESOURCES, STATUS_NOT_SUPPORTED, WDF_OBJECT_ATTRIBUTES, WDFDEVICE};

use crate::ks::{KSCATEGORY_AUDIO, KSNODETYPE_LINE_CONNECTOR, KsWaveFormat};
use crate::rt::{self, RtStream, Slot};
use crate::wdf::object_attributes;
use crate::{STREAM_CONTEXT_TYPE, device_context, unicode, utf16};

const HOST_PIN: u32 = 0;
const BRIDGE_PIN: u32 = 1;
const ACX_PIN_ID_DEFAULT: u32 = u32::MAX;
const ACX_INSTANCE_INDETERMINATE: u32 = u32::MAX;

/// Default format first: 48 kHz with 24 valid bits.
const FORMATS: [(SampleRate, u16, u16); 10] = [
    (SampleRate::Hz48000, 32, 24),
    (SampleRate::Hz44100, 32, 24),
    (SampleRate::Hz88200, 32, 24),
    (SampleRate::Hz96000, 32, 24),
    (SampleRate::Hz192000, 32, 24),
    (SampleRate::Hz48000, 16, 16),
    (SampleRate::Hz44100, 16, 16),
    (SampleRate::Hz88200, 16, 16),
    (SampleRate::Hz96000, 16, 16),
    (SampleRate::Hz192000, 16, 16),
];

static OUTPUT12_NAME: [u16; 8] = utf16(b"Output12");
static OUTPUT34_NAME: [u16; 8] = utf16(b"Output34");
static INPUT12_NAME: [u16; 7] = utf16(b"Input12");

const COMPONENT_IDS: [GUID; 3] = [
    GUID { Data1: 0x5c2a1f6e, Data2: 0x3b7d, Data3: 0x4e21, Data4: [0x9a, 0x4f, 0x61, 0x0d, 0x2b, 0x8e, 0x71, 0x01] },
    GUID { Data1: 0x5c2a1f6e, Data2: 0x3b7d, Data3: 0x4e21, Data4: [0x9a, 0x4f, 0x61, 0x0d, 0x2b, 0x8e, 0x71, 0x02] },
    GUID { Data1: 0x5c2a1f6e, Data2: 0x3b7d, Data3: 0x4e21, Data4: [0x9a, 0x4f, 0x61, 0x0d, 0x2b, 0x8e, 0x71, 0x03] },
];

pub unsafe fn create_all(device: WDFDEVICE) -> Result<[ACXCIRCUIT; 3], NTSTATUS> {
    let mut circuits = [core::ptr::null_mut(); 3];
    for slot in Slot::ALL {
        circuits[slot as usize] = unsafe { create(device, slot)? };
    }
    Ok(circuits)
}

unsafe fn create(device: WDFDEVICE, slot: Slot) -> Result<ACXCIRCUIT, NTSTATUS> {
    let name: &'static [u16] = match slot {
        Slot::Output12 => &OUTPUT12_NAME,
        Slot::Output34 => &OUTPUT34_NAME,
        Slot::Input12 => &INPUT12_NAME,
    };
    let mut init: PACXCIRCUIT_INIT = unsafe { call_acx!(AcxCircuitInitAllocate, device) };
    if init.is_null() {
        return Err(STATUS_INSUFFICIENT_RESOURCES);
    }
    let circuit = unsafe { create_from_init(device, slot, name, &mut init) };
    if !init.is_null() {
        unsafe { call_acx!(AcxCircuitInitFree, init) };
    }
    let circuit = circuit?;

    let (host_type, bridge_type) = if slot.is_capture() {
        (_ACX_PIN_TYPE::AcxPinTypeSource, _ACX_PIN_TYPE::AcxPinTypeSink)
    } else {
        (_ACX_PIN_TYPE::AcxPinTypeSink, _ACX_PIN_TYPE::AcxPinTypeSource)
    };
    let mut pins: [ACXPIN; 2] = [core::ptr::null_mut(); 2];
    pins[HOST_PIN as usize] = unsafe {
        create_pin(circuit, host_type, _ACX_PIN_COMMUNICATION::AcxPinCommunicationSink, &KSCATEGORY_AUDIO)?
    };
    pins[BRIDGE_PIN as usize] = unsafe {
        create_pin(circuit, bridge_type, _ACX_PIN_COMMUNICATION::AcxPinCommunicationNone, &KSNODETYPE_LINE_CONNECTOR)?
    };
    unsafe { add_formats(device, circuit, pins[HOST_PIN as usize])? };
    check(unsafe { call_acx!(AcxCircuitAddPins, circuit, pins.as_mut_ptr(), pins.len() as u32) })?;
    Ok(circuit)
}

unsafe fn create_from_init(
    device: WDFDEVICE,
    slot: Slot,
    name: &'static [u16],
    init: &mut PACXCIRCUIT_INIT,
) -> Result<ACXCIRCUIT, NTSTATUS> {
    let circuit_type =
        if slot.is_capture() { _ACX_CIRCUIT_TYPE::AcxCircuitTypeCapture } else { _ACX_CIRCUIT_TYPE::AcxCircuitTypeRender };
    let name = unicode(name);
    unsafe {
        call_acx!(AcxCircuitInitSetComponentId, *init, &COMPONENT_IDS[slot as usize]);
        check(call_acx!(AcxCircuitInitAssignName, *init, &name))?;
        call_acx!(AcxCircuitInitSetCircuitType, *init, circuit_type);
        check(call_acx!(AcxCircuitInitAssignAcxCreateStreamCallback, *init, Some(evt_create_stream)))?;
    }
    let mut attributes = object_attributes(core::ptr::null_mut());
    let mut circuit: ACXCIRCUIT = core::ptr::null_mut();
    check(unsafe { call_acx!(AcxCircuitCreate, device, &mut attributes, init, &mut circuit) })?;
    // The framework owns the init structure once the circuit exists.
    *init = core::ptr::null_mut();
    Ok(circuit)
}

unsafe fn create_pin(
    circuit: ACXCIRCUIT,
    pin_type: acx_sys::ACX_PIN_TYPE,
    communication: acx_sys::ACX_PIN_COMMUNICATION,
    category: &'static GUID,
) -> Result<ACXPIN, NTSTATUS> {
    let mut config: ACX_PIN_CONFIG = unsafe { core::mem::zeroed() };
    config.Size = size_of::<ACX_PIN_CONFIG>() as u32;
    config.Id = ACX_PIN_ID_DEFAULT;
    config.MaxStreams = ACX_INSTANCE_INDETERMINATE;
    config.Type = pin_type;
    config.Communication = communication;
    config.Category = category;
    let mut attributes = object_attributes(circuit.cast());
    let mut pin: ACXPIN = core::ptr::null_mut();
    check(unsafe { call_acx!(AcxPinCreate, circuit, &mut attributes, &mut config, &mut pin) })?;
    Ok(pin)
}

unsafe fn add_formats(device: WDFDEVICE, circuit: ACXCIRCUIT, pin: ACXPIN) -> Result<(), NTSTATUS> {
    let list = unsafe { call_acx!(AcxPinGetRawDataFormatList, pin) };
    if list.is_null() {
        return Err(STATUS_INSUFFICIENT_RESOURCES);
    }
    for (rate, container_bits, valid_bits) in FORMATS {
        let mut wave = KsWaveFormat::stereo_pcm(rate.hz(), container_bits, valid_bits);
        let mut config: ACX_DATAFORMAT_CONFIG = unsafe { core::mem::zeroed() };
        config.Size = size_of::<ACX_DATAFORMAT_CONFIG>() as u32;
        config.Type = acx_sys::_ACX_DATAFORMAT_TYPE::AcxDataFormatKsFormat;
        unsafe { *config.u.KsFormat.as_mut() = (&raw mut wave).cast() };
        let mut attributes = object_attributes(circuit.cast());
        let mut format: ACXDATAFORMAT = core::ptr::null_mut();
        check(unsafe { call_acx!(AcxDataFormatCreate, device, &mut attributes, &mut config, &mut format) })?;
        check(unsafe { call_acx!(AcxDataFormatListAddDataFormat, list, format) })?;
    }
    Ok(())
}

unsafe extern "C" fn evt_create_stream(
    device: WDFDEVICE,
    circuit: ACXCIRCUIT,
    _pin: ACXPIN,
    mut init: PACXSTREAM_INIT,
    format: ACXDATAFORMAT,
    _mode: *const GUID,
    _arguments: ACXOBJECTBAG,
) -> NTSTATUS {
    match unsafe { create_stream(device, circuit, &mut init, format) } {
        Ok(()) => wdk_sys::STATUS_SUCCESS,
        Err(status) => status,
    }
}

unsafe fn create_stream(
    device: WDFDEVICE,
    circuit: ACXCIRCUIT,
    init: &mut PACXSTREAM_INIT,
    format: ACXDATAFORMAT,
) -> Result<(), NTSTATUS> {
    let context = unsafe { &*device_context(device) };
    let slot = Slot::ALL
        .into_iter()
        .find(|slot| context.circuits[*slot as usize] == circuit)
        .ok_or(STATUS_NOT_SUPPORTED)?;
    let rate = SampleRate::from_hz(unsafe { call_acx!(AcxDataFormatGetSampleRate, format) })
        .ok_or(STATUS_NOT_SUPPORTED)?;
    let container_bits = unsafe { call_acx!(AcxDataFormatGetBitsPerSample, format) };
    if unsafe { call_acx!(AcxDataFormatGetChannelsCount, format) } != 2 || !matches!(container_bits, 16 | 32) {
        return Err(STATUS_NOT_SUPPORTED);
    }

    let mut callbacks: ACX_STREAM_CALLBACKS = unsafe { core::mem::zeroed() };
    callbacks.Size = size_of::<ACX_STREAM_CALLBACKS>() as u32;
    callbacks.EvtAcxStreamPrepareHardware = Some(rt::evt_prepare_hardware);
    callbacks.EvtAcxStreamReleaseHardware = Some(rt::evt_release_hardware);
    callbacks.EvtAcxStreamRun = Some(rt::evt_run);
    callbacks.EvtAcxStreamPause = Some(rt::evt_pause);
    check(unsafe { call_acx!(AcxStreamInitAssignAcxStreamCallbacks, *init, &mut callbacks) })?;

    let mut rt_callbacks: ACX_RT_STREAM_CALLBACKS = unsafe { core::mem::zeroed() };
    rt_callbacks.Size = size_of::<ACX_RT_STREAM_CALLBACKS>() as u32;
    rt_callbacks.EvtAcxStreamGetHwLatency = Some(rt::evt_get_hw_latency);
    rt_callbacks.EvtAcxStreamAllocateRtPackets = Some(rt::evt_allocate_rt_packets);
    rt_callbacks.EvtAcxStreamFreeRtPackets = Some(rt::evt_free_rt_packets);
    rt_callbacks.EvtAcxStreamGetPresentationPosition = Some(rt::evt_get_presentation_position);
    rt_callbacks.EvtAcxStreamGetCurrentPacket = Some(rt::evt_get_current_packet);
    if slot.is_capture() {
        rt_callbacks.EvtAcxStreamGetCapturePacket = Some(rt::evt_get_capture_packet);
    } else {
        rt_callbacks.EvtAcxStreamSetRenderPacket = Some(rt::evt_set_render_packet);
    }
    check(unsafe { call_acx!(AcxStreamInitAssignAcxRtStreamCallbacks, *init, &mut rt_callbacks) })?;
    unsafe { call_acx!(AcxStreamInitSetAcxRtStreamSupportsNotifications, *init) };

    unsafe { context.audio.claim_rate(rate)? };
    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        ContextTypeInfo: &STREAM_CONTEXT_TYPE.0,
        EvtDestroyCallback: Some(evt_stream_destroy),
        ..object_attributes(core::ptr::null_mut())
    };
    let mut stream: ACXSTREAM = core::ptr::null_mut();
    let status = unsafe { call_acx!(AcxRtStreamCreate, device, circuit, &mut attributes, init, &mut stream) };
    if !NT_SUCCESS(status) {
        unsafe { context.audio.release_rate() };
        return Err(status);
    }
    let rt = Box::new(RtStream::new(stream, slot, rate, container_bits, &context.audio));
    unsafe { crate::stream_context(stream).write(Box::into_raw(rt)) };
    Ok(())
}

unsafe extern "C" fn evt_stream_destroy(object: wdk_sys::WDFOBJECT) {
    let slot = unsafe { crate::stream_context(object.cast()) };
    let rt = unsafe { slot.read() };
    if rt.is_null() {
        return;
    }
    let rt = unsafe { Box::from_raw(rt) };
    unsafe { (*rt.audio).release_rate() };
}

fn check(status: NTSTATUS) -> Result<(), NTSTATUS> {
    if NT_SUCCESS(status) { Ok(()) } else { Err(status) }
}
