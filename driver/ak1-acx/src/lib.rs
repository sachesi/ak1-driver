//! KMDF + ACX driver for the Native Instruments Audio Kontrol 1.

#![no_std]

extern crate alloc;
#[cfg(not(test))]
extern crate wdk_panic;

mod audio;
mod circuit;
mod ks;
mod rt;
mod stream;
mod usb;
mod wdf;

use acx_sys::{ACX_DEVICE_CONFIG, ACX_DEVICEINIT_CONFIG, ACX_DRIVER_CONFIG, ACXCIRCUIT, ACXSTREAM, call_acx};
use ak1_proto::mode2;
use wdk::println;
#[cfg(not(test))]
use wdk_alloc::WdkAllocator;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL, _WDF_POWER_DEVICE_STATE, _WDF_SYNCHRONIZATION_SCOPE, ACCESS_MASK, KEY_SET_VALUE, NT_SUCCESS, NTSTATUS,
    PCUNICODE_STRING, PDRIVER_OBJECT, PLUGPLAY_REGKEY_DEVICE, PWDFDEVICE_INIT, REG_BINARY, REG_DWORD,
    STATUS_NOT_SUPPORTED, STATUS_SUCCESS, UNICODE_STRING, WDF_DRIVER_CONFIG, WDF_NO_OBJECT_ATTRIBUTES,
    WDF_OBJECT_ATTRIBUTES, WDF_OBJECT_CONTEXT_TYPE_INFO, WDF_PNPPOWER_EVENT_CALLBACKS, WDFCMRESLIST, WDFDEVICE,
    WDF_POWER_DEVICE_STATE, WDFDRIVER, WDFKEY, WDFWAITLOCK, call_unsafe_wdf_function_binding,
};

use crate::audio::Audio;
use crate::stream::Stats;
use crate::usb::Ak1Usb;

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

struct DeviceContext {
    usb: Option<Ak1Usb>,
    audio: Audio,
    circuits: [ACXCIRCUIT; 3],
    circuits_added: bool,
}

#[repr(transparent)]
struct ContextTypeInfo(WDF_OBJECT_CONTEXT_TYPE_INFO);

// Immutable after initialization; the framework only reads it.
unsafe impl Sync for ContextTypeInfo {}

static DEVICE_CONTEXT_TYPE: ContextTypeInfo = ContextTypeInfo(WDF_OBJECT_CONTEXT_TYPE_INFO {
    Size: size_of::<WDF_OBJECT_CONTEXT_TYPE_INFO>() as u32,
    ContextName: c"DeviceContext".as_ptr(),
    ContextSize: size_of::<DeviceContext>(),
    UniqueType: &DEVICE_CONTEXT_TYPE.0,
    EvtDriverGetUniqueContextType: None,
});

static STREAM_CONTEXT_TYPE: ContextTypeInfo = ContextTypeInfo(WDF_OBJECT_CONTEXT_TYPE_INFO {
    Size: size_of::<WDF_OBJECT_CONTEXT_TYPE_INFO>() as u32,
    ContextName: c"StreamContext".as_ptr(),
    ContextSize: size_of::<*mut rt::RtStream>(),
    UniqueType: &STREAM_CONTEXT_TYPE.0,
    EvtDriverGetUniqueContextType: None,
});

// Values under the device's hardware key.
static FIRMWARE_VERSION_VALUE: [u16; 15] = utf16(b"FirmwareVersion");
static STREAM_STATS_VALUE: [u16; 11] = utf16(b"StreamStats");

/// # Safety
///
/// Called only by the I/O manager, with its driver object and registry path.
#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(driver: PDRIVER_OBJECT, registry_path: PCUNICODE_STRING) -> NTSTATUS {
    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of::<WDF_DRIVER_CONFIG>() as u32,
        EvtDriverDeviceAdd: Some(evt_device_add),
        ..Default::default()
    };
    let mut wdf_driver: WDFDRIVER = core::ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut config,
            &mut wdf_driver
        )
    };
    if !NT_SUCCESS(status) {
        return status;
    }

    let mut acx_config = ACX_DRIVER_CONFIG { Size: size_of::<ACX_DRIVER_CONFIG>() as u32, ..Default::default() };
    unsafe { call_acx!(AcxDriverInitialize, wdf_driver, &mut acx_config) }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    let status = into_status(unsafe { add_device(&mut device_init) });
    println!("ak1acx: device add status {status:#010x}");
    status
}

unsafe fn add_device(device_init: &mut PWDFDEVICE_INIT) -> Result<(), NTSTATUS> {
    let mut init_config = ACX_DEVICEINIT_CONFIG {
        Size: size_of::<ACX_DEVICEINIT_CONFIG>() as u32,
        SynchronizationScope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone,
        ExecutionLevel: _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive,
        ..Default::default()
    };
    check(unsafe { call_acx!(AcxDeviceInitInitialize, *device_init, &mut init_config) })?;

    let mut pnp_callbacks = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as u32,
        EvtDevicePrepareHardware: Some(evt_prepare_hardware),
        EvtDeviceReleaseHardware: Some(evt_release_hardware),
        EvtDeviceD0Entry: Some(evt_d0_entry),
        EvtDeviceD0Exit: Some(evt_d0_exit),
        ..Default::default()
    };
    unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetPnpPowerEventCallbacks, *device_init, &mut pnp_callbacks)
    };

    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        ContextTypeInfo: &DEVICE_CONTEXT_TYPE.0,
        ..wdf::object_attributes(core::ptr::null_mut())
    };
    let mut device: WDFDEVICE = core::ptr::null_mut();
    check(unsafe { call_unsafe_wdf_function_binding!(WdfDeviceCreate, device_init, &mut attributes, &mut device) })?;

    let mut attributes = wdf::object_attributes(device.cast());
    let mut control: WDFWAITLOCK = core::ptr::null_mut();
    check(unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockCreate, &mut attributes, &mut control) })?;
    unsafe {
        device_context(device).write(DeviceContext {
            usb: None,
            audio: Audio::new(device, control),
            circuits: [core::ptr::null_mut(); 3],
            circuits_added: false,
        })
    };

    let mut device_config = ACX_DEVICE_CONFIG { Size: size_of::<ACX_DEVICE_CONFIG>() as u32, ..Default::default() };
    check(unsafe { call_acx!(AcxDeviceInitialize, device, &mut device_config) })?;

    let circuits = unsafe { circuit::create_all(device)? };
    unsafe { (*device_context(device)).circuits = circuits };
    Ok(())
}

extern "C" fn evt_prepare_hardware(device: WDFDEVICE, _raw: WDFCMRESLIST, _translated: WDFCMRESLIST) -> NTSTATUS {
    let status = into_status(unsafe { prepare_hardware(device) });
    println!("ak1acx: prepare hardware status {status:#010x}");
    status
}

unsafe fn prepare_hardware(device: WDFDEVICE) -> Result<(), NTSTATUS> {
    let context = unsafe { &mut *device_context(device) };
    if context.usb.is_none() {
        context.usb = Some(unsafe { Ak1Usb::open(device)? });
    }
    let usb = context.usb.as_ref().expect("opened above");
    let spec = unsafe { usb.sync()? };
    println!("ak1acx: {spec:?}");
    if spec.data_alignment != 2 || spec.streams() != mode2::STREAMS {
        return Err(STATUS_NOT_SUPPORTED);
    }
    unsafe { context.audio.attach(usb, spec) };

    if !context.circuits_added {
        for circuit in context.circuits {
            check(unsafe { call_acx!(AcxDeviceAddCircuit, device, circuit) })?;
        }
        context.circuits_added = true;
    }

    if let Ok(key) = unsafe { DeviceKey::open(device, KEY_SET_VALUE) } {
        let _ = unsafe { key.assign(&FIRMWARE_VERSION_VALUE, &u32::from(spec.fw_version).to_ne_bytes(), REG_DWORD) };
    }
    Ok(())
}

extern "C" fn evt_d0_entry(device: WDFDEVICE, previous: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    // On first start, PrepareHardware has just set the device up.
    if previous == _WDF_POWER_DEVICE_STATE::WdfPowerDeviceD3Final {
        return STATUS_SUCCESS;
    }
    let status = into_status(unsafe { restore(device) });
    println!("ak1acx: restore from power state {previous} status {status:#010x}");
    if !NT_SUCCESS(status) {
        return status;
    }
    let status = into_status(unsafe { (*device_context(device)).audio.resume() });
    println!("ak1acx: resume streaming status {status:#010x}");
    // Streams that cannot restart fail on their own; the device stays usable.
    STATUS_SUCCESS
}

/// After system sleep the card ignores commands until its port is reset, and
/// then needs the same setup as in PrepareHardware.
unsafe fn restore(device: WDFDEVICE) -> Result<(), NTSTATUS> {
    let usb = unsafe { (*device_context(device)).usb.as_mut().expect("opened in PrepareHardware") };
    unsafe { usb.reset_port()? };
    unsafe { usb.select_streaming()? };
    unsafe { usb.sync()? };
    Ok(())
}

extern "C" fn evt_d0_exit(device: WDFDEVICE, _target: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    unsafe { (*device_context(device)).audio.suspend() };
    STATUS_SUCCESS
}

extern "C" fn evt_release_hardware(device: WDFDEVICE, _translated: WDFCMRESLIST) -> NTSTATUS {
    let context = unsafe { &mut *device_context(device) };
    if context.circuits_added {
        for circuit in context.circuits {
            let _ = unsafe { call_acx!(AcxDeviceRemoveCircuit, device, circuit) };
        }
        context.circuits_added = false;
    }
    STATUS_SUCCESS
}

/// Leaves the engine's counters under the device key for diagnostics. Windows
/// briefly opens an endpoint after an exclusive stream closes; such streams
/// move no audio and would hide the one before.
unsafe fn record_stream_stats(device: WDFDEVICE, stats: &Stats) {
    let snapshot = stats.snapshot();
    if snapshot[0] == 0 {
        return;
    }
    let bytes: [u8; 36] = unsafe { core::mem::transmute(snapshot) };
    if let Ok(key) = unsafe { DeviceKey::open(device, KEY_SET_VALUE) } {
        let _ = unsafe { key.assign(&STREAM_STATS_VALUE, &bytes, REG_BINARY) };
    }
}

struct DeviceKey(WDFKEY);

impl DeviceKey {
    unsafe fn open(device: WDFDEVICE, access: ACCESS_MASK) -> Result<DeviceKey, NTSTATUS> {
        let mut key: WDFKEY = core::ptr::null_mut();
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfDeviceOpenRegistryKey,
                device,
                PLUGPLAY_REGKEY_DEVICE,
                access,
                WDF_NO_OBJECT_ATTRIBUTES,
                &mut key
            )
        })?;
        Ok(DeviceKey(key))
    }

    unsafe fn assign(&self, name: &'static [u16], data: &[u8], value_type: u32) -> Result<(), NTSTATUS> {
        let name = unicode(name);
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRegistryAssignValue,
                self.0,
                &name,
                value_type,
                data.len() as u32,
                data.as_ptr().cast_mut().cast()
            )
        })
    }
}

impl Drop for DeviceKey {
    fn drop(&mut self) {
        unsafe { call_unsafe_wdf_function_binding!(WdfRegistryClose, self.0) };
    }
}

unsafe fn device_context(device: WDFDEVICE) -> *mut DeviceContext {
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectGetTypedContextWorker, device.cast(), &DEVICE_CONTEXT_TYPE.0)
            .cast()
    }
}

unsafe fn stream_context(stream: ACXSTREAM) -> *mut *mut rt::RtStream {
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectGetTypedContextWorker, stream.cast(), &STREAM_CONTEXT_TYPE.0)
            .cast()
    }
}

fn unicode(name: &'static [u16]) -> UNICODE_STRING {
    let bytes = size_of_val(name) as u16;
    UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: name.as_ptr().cast_mut() }
}

const fn utf16<const N: usize>(ascii: &[u8; N]) -> [u16; N] {
    let mut out = [0; N];
    let mut i = 0;
    while i < N {
        out[i] = ascii[i] as u16;
        i += 1;
    }
    out
}

fn check(status: NTSTATUS) -> Result<(), NTSTATUS> {
    if NT_SUCCESS(status) { Ok(()) } else { Err(status) }
}

fn into_status(result: Result<(), NTSTATUS>) -> NTSTATUS {
    result.err().unwrap_or(STATUS_SUCCESS)
}
