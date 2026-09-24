//! State of the card as Plug and Play and the kernel driver report it.

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_GETIDLIST_FILTER_ENUMERATOR, CM_GETIDLIST_FILTER_PRESENT, CM_Get_DevNode_PropertyW,
    CM_Get_DevNode_Status, CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW, CM_LOCATE_DEVNODE_NORMAL,
    CM_Locate_DevNodeW, CM_Open_DevNode_Key, CM_PROB, CM_REGISTRY_HARDWARE, CR_SUCCESS, DN_HAS_PROBLEM,
    RegDisposition_OpenExisting,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_DriverVersion, DEVPKEY_Device_Service, DEVPROPTYPE};
use windows::Win32::Foundation::{DEVPROPKEY, ERROR_SUCCESS};
use windows::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegQueryValueExW};
use windows::core::{HSTRING, PCWSTR, w};

const INSTANCE_PREFIX: &str = "USB\\VID_17CC&PID_0815\\";
const KERNEL_SERVICE: &str = "ak1acx";
/// Each capture transfer the driver counts covers 4 ms.
const TRANSFER_SECONDS: f64 = 0.004;

pub enum Connection {
    Missing,
    Working { service: String },
    Problem(u32),
}

pub struct Status {
    pub connection: Connection,
    pub driver_version: Option<String>,
    pub firmware: Option<u32>,
    /// Counters the kernel driver leaves when a stream ends, in `Stats::snapshot` order.
    pub last_stream: Option<[u32; 9]>,
}

impl Status {
    pub fn query() -> Status {
        let Some(node) = present_node() else {
            return Status { connection: Connection::Missing, driver_version: None, firmware: None, last_stream: None };
        };
        let (mut flags, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        let status = unsafe { CM_Get_DevNode_Status(&mut flags, &mut problem, node, 0) };
        let connection = if status == CR_SUCCESS && flags.0 & DN_HAS_PROBLEM.0 != 0 {
            Connection::Problem(problem.0)
        } else {
            Connection::Working { service: string_property(node, &DEVPKEY_Device_Service).unwrap_or_default() }
        };
        let key = hardware_key(node);
        let status = Status {
            connection,
            driver_version: string_property(node, &DEVPKEY_Device_DriverVersion),
            firmware: key.and_then(|key| read_value::<4>(key, w!("FirmwareVersion"))).map(u32::from_ne_bytes),
            last_stream: key.and_then(|key| read_value::<36>(key, w!("StreamStats"))).map(|bytes| {
                std::array::from_fn(|i| u32::from_ne_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap()))
            }),
        };
        if let Some(key) = key {
            let _ = unsafe { RegCloseKey(key) };
        }
        status
    }

    pub fn summary(&self) -> String {
        match &self.connection {
            Connection::Missing => "Not connected.".into(),
            Connection::Working { service } if service.eq_ignore_ascii_case(KERNEL_SERVICE) => {
                "Connected and working.".into()
            }
            Connection::Working { service } => format!("Connected, but using the {service} driver instead of this one."),
            Connection::Problem(code) => {
                format!("Connected, but stopped with problem code {code}. Unplug the card and plug it back in.")
            }
        }
    }

    pub fn versions(&self) -> String {
        let driver = self.driver_version.as_deref().unwrap_or("unknown");
        match self.firmware {
            Some(firmware) => format!("Driver {driver}, firmware {firmware}"),
            None => format!("Driver {driver}"),
        }
    }

    pub fn last_stream(&self) -> String {
        let Some([capture_transfers, _, _, check_errors, _, _, _, overruns, lost_packets]) = self.last_stream else {
            return "No stream since the driver started.".into();
        };
        format!(
            "Last stream: {:.1} s, {lost_packets} lost packets, {check_errors} check errors, {overruns} output overruns",
            f64::from(capture_transfers) * TRANSFER_SECONDS
        )
    }
}

fn present_node() -> Option<u32> {
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
    let mut len = 0;
    if unsafe { CM_Get_Device_ID_List_SizeW(&mut len, w!("USB"), flags) } != CR_SUCCESS {
        return None;
    }
    let mut list = vec![0u16; len as usize];
    if unsafe { CM_Get_Device_ID_ListW(w!("USB"), &mut list, flags) } != CR_SUCCESS {
        return None;
    }
    let id = list
        .split(|&c| c == 0)
        .map(String::from_utf16_lossy)
        .find(|id| id.to_ascii_uppercase().starts_with(INSTANCE_PREFIX))?;
    let mut node = 0;
    let status = unsafe { CM_Locate_DevNodeW(&mut node, &HSTRING::from(id), CM_LOCATE_DEVNODE_NORMAL) };
    (status == CR_SUCCESS).then_some(node)
}

fn string_property(node: u32, key: &DEVPROPKEY) -> Option<String> {
    let mut buffer = [0u16; 256];
    let mut size = size_of_val(&buffer) as u32;
    let mut kind = DEVPROPTYPE(0);
    let status =
        unsafe { CM_Get_DevNode_PropertyW(node, key, &mut kind, Some(buffer.as_mut_ptr().cast()), &mut size, 0) };
    if status != CR_SUCCESS {
        return None;
    }
    let text = &buffer[..size as usize / 2];
    Some(String::from_utf16_lossy(text.split(|&c| c == 0).next().unwrap_or_default()))
}

fn hardware_key(node: u32) -> Option<HKEY> {
    let mut key = HKEY::default();
    let status = unsafe {
        CM_Open_DevNode_Key(node, KEY_READ.0, 0, RegDisposition_OpenExisting, &mut key, CM_REGISTRY_HARDWARE)
    };
    (status == CR_SUCCESS).then_some(key)
}

fn read_value<const N: usize>(key: HKEY, name: PCWSTR) -> Option<[u8; N]> {
    let mut data = [0u8; N];
    let mut size = N as u32;
    let status = unsafe { RegQueryValueExW(key, name, None, None, Some(data.as_mut_ptr()), Some(&mut size)) };
    (status == ERROR_SUCCESS && size as usize == N).then_some(data)
}
