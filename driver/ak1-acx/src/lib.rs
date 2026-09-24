//! KMDF + ACX driver for the Native Instruments Audio Kontrol 1.

#![no_std]

extern crate alloc;
#[cfg(not(test))]
extern crate wdk_panic;

mod stream;
mod usb;
mod wdf;

use alloc::boxed::Box;

use acx_sys::{ACX_DEVICE_CONFIG, ACX_DEVICEINIT_CONFIG, ACX_DRIVER_CONFIG, call_acx};
use ak1_proto::{DeviceSpec, SampleRate, mode2};
use wdk::println;
#[cfg(not(test))]
use wdk_alloc::WdkAllocator;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL, _WDF_SYNCHRONIZATION_SCOPE, ACCESS_MASK, KEY_QUERY_VALUE, KEY_SET_VALUE, NT_SUCCESS,
    NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PLUGPLAY_REGKEY_DEVICE, PWDFDEVICE_INIT, REG_BINARY,
    STATUS_NOT_SUPPORTED, STATUS_SUCCESS, UNICODE_STRING, WDF_DRIVER_CONFIG, WDF_NO_OBJECT_ATTRIBUTES,
    WDF_OBJECT_ATTRIBUTES, WDF_OBJECT_CONTEXT_TYPE_INFO, WDF_PNPPOWER_EVENT_CALLBACKS, WDF_POWER_DEVICE_STATE,
    WDFCMRESLIST, WDFDEVICE, WDFDRIVER, WDFKEY, call_unsafe_wdf_function_binding,
};

use crate::stream::Engine;
use crate::usb::Ak1Usb;

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

struct DeviceContext {
    usb: Option<Ak1Usb>,
    spec: Option<DeviceSpec>,
    engine: Option<Box<Engine>>,
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

// Values under the device's hardware key.
static FIRMWARE_VERSION_VALUE: [u16; 15] = utf16(b"FirmwareVersion");
static TEST_RATE_VALUE: [u16; 8] = utf16(b"TestRate");
static STREAM_STATS_VALUE: [u16; 11] = utf16(b"StreamStats");

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
    let status = unsafe { add_device(&mut device_init) };
    println!("ak1acx: device add status {status:#010x}");
    status
}

unsafe fn add_device(device_init: &mut PWDFDEVICE_INIT) -> NTSTATUS {
    let mut init_config = ACX_DEVICEINIT_CONFIG {
        Size: size_of::<ACX_DEVICEINIT_CONFIG>() as u32,
        SynchronizationScope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone,
        ExecutionLevel: _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive,
        ..Default::default()
    };
    let status = unsafe { call_acx!(AcxDeviceInitInitialize, *device_init, &mut init_config) };
    if !NT_SUCCESS(status) {
        return status;
    }

    let mut pnp_callbacks = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as u32,
        EvtDevicePrepareHardware: Some(evt_prepare_hardware),
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
    let status =
        unsafe { call_unsafe_wdf_function_binding!(WdfDeviceCreate, device_init, &mut attributes, &mut device) };
    if !NT_SUCCESS(status) {
        return status;
    }
    unsafe { device_context(device).write(DeviceContext { usb: None, spec: None, engine: None }) };

    let mut device_config = ACX_DEVICE_CONFIG { Size: size_of::<ACX_DEVICE_CONFIG>() as u32, ..Default::default() };
    unsafe { call_acx!(AcxDeviceInitialize, device, &mut device_config) }
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
    context.spec = Some(spec);
    let key = unsafe { DeviceKey::open(device, KEY_SET_VALUE)? };
    unsafe { key.assign(&FIRMWARE_VERSION_VALUE, &u32::from(spec.fw_version).to_ne_bytes(), wdk_sys::REG_DWORD) }
}

extern "C" fn evt_d0_entry(device: WDFDEVICE, _previous: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    let status = into_status(unsafe { start_test_stream(device) });
    println!("ak1acx: d0 entry status {status:#010x}");
    status
}

extern "C" fn evt_d0_exit(device: WDFDEVICE, _target: WDF_POWER_DEVICE_STATE) -> NTSTATUS {
    let context = unsafe { &mut *device_context(device) };
    if let Some(engine) = context.engine.take() {
        unsafe { engine.stop() };
        let bytes: [u8; 36] = unsafe { core::mem::transmute(engine.stats.snapshot()) };
        drop(engine);
        if let Ok(key) = unsafe { DeviceKey::open(device, KEY_SET_VALUE) } {
            let _ = unsafe { key.assign(&STREAM_STATS_VALUE, &bytes, REG_BINARY) };
        }
    }
    STATUS_SUCCESS
}

/// Streams silence while the device is in D0 when the hardware key names a
/// sample rate in `TestRate`. The value is consumed so that a crash while
/// streaming does not repeat on the next boot.
unsafe fn start_test_stream(device: WDFDEVICE) -> Result<(), NTSTATUS> {
    let context = unsafe { &mut *device_context(device) };
    let rate = {
        let key = unsafe { DeviceKey::open(device, KEY_QUERY_VALUE | KEY_SET_VALUE)? };
        let Ok(hz) = (unsafe { key.query_u32(&TEST_RATE_VALUE) }) else { return Ok(()) };
        unsafe { key.remove(&TEST_RATE_VALUE)? };
        SampleRate::from_hz(hz).ok_or(STATUS_NOT_SUPPORTED)?
    };
    let (Some(usb), Some(spec)) = (context.usb.as_ref(), context.spec) else { return Ok(()) };
    context.engine = Some(unsafe { Engine::start(device, usb, rate, spec.max_packet_bytes(rate))? });
    Ok(())
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

    unsafe fn remove(&self, name: &'static [u16]) -> Result<(), NTSTATUS> {
        let name = unicode(name);
        check(unsafe { call_unsafe_wdf_function_binding!(WdfRegistryRemoveValue, self.0, &name) })
    }

    unsafe fn query_u32(&self, name: &'static [u16]) -> Result<u32, NTSTATUS> {
        let name = unicode(name);
        let mut value = 0;
        check(unsafe { call_unsafe_wdf_function_binding!(WdfRegistryQueryULong, self.0, &name, &mut value) })?;
        Ok(value)
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
