//! KMDF + ACX driver for the Native Instruments Audio Kontrol 1.

#![no_std]

#[cfg(not(test))]
extern crate wdk_panic;

use acx_sys::{ACX_DEVICE_CONFIG, ACX_DEVICEINIT_CONFIG, ACX_DRIVER_CONFIG, call_acx};
use wdk::println;
#[cfg(not(test))]
use wdk_alloc::WdkAllocator;
use wdk_sys::{
    _WDF_EXECUTION_LEVEL, _WDF_SYNCHRONIZATION_SCOPE, NT_SUCCESS, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT,
    PWDFDEVICE_INIT, WDF_DRIVER_CONFIG, WDF_NO_OBJECT_ATTRIBUTES, WDFDEVICE, WDFDRIVER,
    call_unsafe_wdf_function_binding,
};

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

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

    let mut device: WDFDEVICE = core::ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceCreate, device_init, WDF_NO_OBJECT_ATTRIBUTES, &mut device)
    };
    if !NT_SUCCESS(status) {
        return status;
    }

    let mut device_config = ACX_DEVICE_CONFIG { Size: size_of::<ACX_DEVICE_CONFIG>() as u32, ..Default::default() };
    unsafe { call_acx!(AcxDeviceInitialize, device, &mut device_config) }
}
