//! KMDF + ACX driver for the Native Instruments Audio Kontrol 1.

#![no_std]

#[cfg(not(test))]
extern crate wdk_panic;

use acx_sys::{ACX_DRIVER_CONFIG, call_acx};
#[cfg(not(test))]
use wdk_alloc::WdkAllocator;
use wdk_sys::{
    NT_SUCCESS, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT, STATUS_SUCCESS, WDF_DRIVER_CONFIG,
    WDF_NO_OBJECT_ATTRIBUTES, WDFDRIVER, call_unsafe_wdf_function_binding,
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

extern "C" fn evt_device_add(_driver: WDFDRIVER, _device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    STATUS_SUCCESS
}
