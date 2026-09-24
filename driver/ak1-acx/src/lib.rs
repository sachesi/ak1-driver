//! KMDF + ACX driver for the Native Instruments Audio Kontrol 1.

#![no_std]

#[cfg(not(test))]
extern crate wdk_panic;

mod usb;

use acx_sys::{ACX_DEVICE_CONFIG, ACX_DEVICEINIT_CONFIG, ACX_DRIVER_CONFIG, call_acx};
use ak1_proto::DeviceSpec;
use wdk::println;
#[cfg(not(test))]
use wdk_alloc::WdkAllocator;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL, _WDF_SYNCHRONIZATION_SCOPE, KEY_SET_VALUE, NT_SUCCESS, NTSTATUS, PCUNICODE_STRING,
    PDRIVER_OBJECT, PLUGPLAY_REGKEY_DEVICE, PWDFDEVICE_INIT, STATUS_SUCCESS, UNICODE_STRING, WDF_DRIVER_CONFIG,
    WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES, WDF_OBJECT_CONTEXT_TYPE_INFO, WDF_PNPPOWER_EVENT_CALLBACKS,
    WDFCMRESLIST, WDFDEVICE, WDFDRIVER, WDFKEY, call_unsafe_wdf_function_binding,
};

use crate::usb::Ak1Usb;

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

struct DeviceContext {
    usb: Option<Ak1Usb>,
    spec: Option<DeviceSpec>,
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

/// Value under the device's hardware key reporting the firmware version read at start.
static FIRMWARE_VERSION_VALUE: [u16; 15] = utf16(b"FirmwareVersion");

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
        ..Default::default()
    };
    unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetPnpPowerEventCallbacks, *device_init, &mut pnp_callbacks)
    };

    let mut attributes = WDF_OBJECT_ATTRIBUTES {
        Size: size_of::<WDF_OBJECT_ATTRIBUTES>() as u32,
        EvtCleanupCallback: None,
        EvtDestroyCallback: None,
        ExecutionLevel: _WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ParentObject: core::ptr::null_mut(),
        ContextSizeOverride: 0,
        ContextTypeInfo: &DEVICE_CONTEXT_TYPE.0,
    };
    let mut device: WDFDEVICE = core::ptr::null_mut();
    let status =
        unsafe { call_unsafe_wdf_function_binding!(WdfDeviceCreate, device_init, &mut attributes, &mut device) };
    if !NT_SUCCESS(status) {
        return status;
    }
    unsafe { device_context(device).write(DeviceContext { usb: None, spec: None }) };

    let mut device_config = ACX_DEVICE_CONFIG { Size: size_of::<ACX_DEVICE_CONFIG>() as u32, ..Default::default() };
    unsafe { call_acx!(AcxDeviceInitialize, device, &mut device_config) }
}

extern "C" fn evt_prepare_hardware(device: WDFDEVICE, _raw: WDFCMRESLIST, _translated: WDFCMRESLIST) -> NTSTATUS {
    let status = match unsafe { prepare_hardware(device) } {
        Ok(()) => STATUS_SUCCESS,
        Err(status) => status,
    };
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
    context.spec = Some(spec);
    unsafe { record_firmware_version(device, spec.fw_version) }
}

unsafe fn record_firmware_version(device: WDFDEVICE, version: u16) -> Result<(), NTSTATUS> {
    let mut key: WDFKEY = core::ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceOpenRegistryKey,
            device,
            PLUGPLAY_REGKEY_DEVICE,
            KEY_SET_VALUE,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut key
        )
    };
    if !NT_SUCCESS(status) {
        return Err(status);
    }
    let name = UNICODE_STRING {
        Length: size_of_val(&FIRMWARE_VERSION_VALUE) as u16,
        MaximumLength: size_of_val(&FIRMWARE_VERSION_VALUE) as u16,
        Buffer: FIRMWARE_VERSION_VALUE.as_ptr().cast_mut(),
    };
    let status = unsafe { call_unsafe_wdf_function_binding!(WdfRegistryAssignULong, key, &name, version.into()) };
    unsafe { call_unsafe_wdf_function_binding!(WdfRegistryClose, key) };
    if NT_SUCCESS(status) { Ok(()) } else { Err(status) }
}

unsafe fn device_context(device: WDFDEVICE) -> *mut DeviceContext {
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectGetTypedContextWorker, device.cast(), &DEVICE_CONTEXT_TYPE.0)
            .cast()
    }
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
