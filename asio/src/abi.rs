//! Binary interface of an ASIO 2 driver object as hosts see it on 64-bit
//! Windows: a COM object whose vtable follows `IUnknown` with the `IASIO`
//! methods, and the plain structs exchanged through them.

use std::ffi::c_void;

pub type AsioBool = i32;
pub type AsioError = i32;

pub const ASE_OK: AsioError = 0;
pub const ASE_SUCCESS: AsioError = 0x3f48_47a0;
pub const ASE_NOT_PRESENT: AsioError = -1000;
pub const ASE_HW_MALFUNCTION: AsioError = -999;
pub const ASE_INVALID_PARAMETER: AsioError = -998;
pub const ASE_INVALID_MODE: AsioError = -997;
pub const ASE_NO_CLOCK: AsioError = -995;
pub const ASE_NO_MEMORY: AsioError = -994;

pub const ASIO_ST_INT32_LSB: i32 = 18;

pub const K_ASIO_SELECTOR_SUPPORTED: i32 = 1;
pub const K_ASIO_ENGINE_VERSION: i32 = 2;
pub const K_ASIO_RESET_REQUEST: i32 = 3;
pub const K_ASIO_SUPPORTS_TIME_INFO: i32 = 7;
pub const K_ASIO_CAN_TIME_INFO: i32 = 0x2311_1961;

pub const K_SYSTEM_TIME_VALID: u32 = 1;
pub const K_SAMPLE_POSITION_VALID: u32 = 1 << 1;
pub const K_SAMPLE_RATE_VALID: u32 = 1 << 2;

/// A 64-bit count split into words, high word first (`ASIOSamples` and
/// `ASIOTimeStamp` without `NATIVE_INT64`).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct AsioU64 {
    pub hi: u32,
    pub lo: u32,
}

impl From<u64> for AsioU64 {
    fn from(value: u64) -> AsioU64 {
        AsioU64 { hi: (value >> 32) as u32, lo: value as u32 }
    }
}

impl From<AsioU64> for u64 {
    fn from(value: AsioU64) -> u64 {
        (u64::from(value.hi) << 32) | u64::from(value.lo)
    }
}

#[repr(C)]
pub struct AsioClockSource {
    pub index: i32,
    pub associated_channel: i32,
    pub associated_group: i32,
    pub is_current_source: AsioBool,
    pub name: [u8; 32],
}

#[repr(C)]
pub struct AsioChannelInfo {
    pub channel: i32,
    pub is_input: AsioBool,
    pub is_active: AsioBool,
    pub channel_group: i32,
    pub sample_type: i32,
    pub name: [u8; 32],
}

#[repr(C)]
pub struct AsioBufferInfo {
    pub is_input: AsioBool,
    pub channel_num: i32,
    pub buffers: [*mut c_void; 2],
}

#[repr(C)]
pub struct AsioTimeInfo {
    pub speed: f64,
    pub system_time: AsioU64,
    pub sample_position: AsioU64,
    pub sample_rate: f64,
    pub flags: u32,
    pub reserved: [u8; 12],
}

#[repr(C)]
pub struct AsioTimeCode {
    pub speed: f64,
    pub time_code_samples: AsioU64,
    pub flags: u32,
    pub future: [u8; 64],
}

#[repr(C)]
pub struct AsioTime {
    pub reserved: [i32; 4],
    pub time_info: AsioTimeInfo,
    pub time_code: AsioTimeCode,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioCallbacks {
    pub buffer_switch: unsafe extern "C" fn(index: i32, direct_process: AsioBool),
    pub sample_rate_did_change: unsafe extern "C" fn(rate: f64),
    pub asio_message: unsafe extern "C" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32,
    pub buffer_switch_time_info:
        unsafe extern "C" fn(params: *mut AsioTime, index: i32, direct_process: AsioBool) -> *mut AsioTime,
}

/// `IUnknown` followed by `IASIO`, in declaration order.
#[repr(C)]
pub struct IAsioVtbl<T> {
    pub query_interface:
        unsafe extern "system" fn(this: *mut T, iid: *const windows_core::GUID, out: *mut *mut c_void) -> i32,
    pub add_ref: unsafe extern "system" fn(this: *mut T) -> u32,
    pub release: unsafe extern "system" fn(this: *mut T) -> u32,
    pub init: unsafe extern "system" fn(this: *mut T, sys_handle: *mut c_void) -> AsioBool,
    pub get_driver_name: unsafe extern "system" fn(this: *mut T, name: *mut u8),
    pub get_driver_version: unsafe extern "system" fn(this: *mut T) -> i32,
    pub get_error_message: unsafe extern "system" fn(this: *mut T, message: *mut u8),
    pub start: unsafe extern "system" fn(this: *mut T) -> AsioError,
    pub stop: unsafe extern "system" fn(this: *mut T) -> AsioError,
    pub get_channels: unsafe extern "system" fn(this: *mut T, inputs: *mut i32, outputs: *mut i32) -> AsioError,
    pub get_latencies: unsafe extern "system" fn(this: *mut T, input: *mut i32, output: *mut i32) -> AsioError,
    pub get_buffer_size: unsafe extern "system" fn(
        this: *mut T,
        min: *mut i32,
        max: *mut i32,
        preferred: *mut i32,
        granularity: *mut i32,
    ) -> AsioError,
    pub can_sample_rate: unsafe extern "system" fn(this: *mut T, rate: f64) -> AsioError,
    pub get_sample_rate: unsafe extern "system" fn(this: *mut T, rate: *mut f64) -> AsioError,
    pub set_sample_rate: unsafe extern "system" fn(this: *mut T, rate: f64) -> AsioError,
    pub get_clock_sources:
        unsafe extern "system" fn(this: *mut T, clocks: *mut AsioClockSource, count: *mut i32) -> AsioError,
    pub set_clock_source: unsafe extern "system" fn(this: *mut T, reference: i32) -> AsioError,
    pub get_sample_position:
        unsafe extern "system" fn(this: *mut T, position: *mut AsioU64, timestamp: *mut AsioU64) -> AsioError,
    pub get_channel_info: unsafe extern "system" fn(this: *mut T, info: *mut AsioChannelInfo) -> AsioError,
    pub create_buffers: unsafe extern "system" fn(
        this: *mut T,
        infos: *mut AsioBufferInfo,
        channels: i32,
        buffer_size: i32,
        callbacks: *const AsioCallbacks,
    ) -> AsioError,
    pub dispose_buffers: unsafe extern "system" fn(this: *mut T) -> AsioError,
    pub control_panel: unsafe extern "system" fn(this: *mut T) -> AsioError,
    pub future: unsafe extern "system" fn(this: *mut T, selector: i32, opt: *mut c_void) -> AsioError,
    pub output_ready: unsafe extern "system" fn(this: *mut T) -> AsioError,
}

/// Copies `text` into a host-provided, NUL-terminated C string of `capacity` bytes.
pub(crate) unsafe fn write_c_string(out: *mut u8, capacity: usize, text: &str) {
    let len = text.len().min(capacity - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), out, len);
        out.add(len).write(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_the_sdk() {
        assert_eq!(size_of::<AsioTimeInfo>(), 48);
        assert_eq!(std::mem::offset_of!(AsioTime, time_info), 16);
        assert_eq!(std::mem::offset_of!(AsioTime, time_code), 64);
        assert_eq!(size_of::<AsioBufferInfo>(), 24);
        assert_eq!(size_of::<AsioChannelInfo>(), 52);
        assert_eq!(size_of::<AsioClockSource>(), 48);
    }

    #[test]
    fn split_counts_put_the_high_word_first() {
        let value = AsioU64::from(0x1_0000_0002);
        assert_eq!((value.hi, value.lo), (1, 2));
        assert_eq!(u64::from(value), 0x1_0000_0002);
    }
}
