use std::ffi::c_void;
use std::mem::size_of;

use ak1_proto::{
    CMD_BUF_SIZE, DeviceSpec, EP_CMD_IN, EP_CMD_OUT, Reply, STREAMING_ALT_SETTING, get_device_info_request,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW, CR_SUCCESS,
};
use windows::Win32::Devices::Usb::{
    PIPE_TRANSFER_TIMEOUT, USBD_ISO_PACKET_DESCRIPTOR, WINUSB_INTERFACE_HANDLE, WinUsb_AbortPipe, WinUsb_Free,
    WinUsb_GetOverlappedResult, WinUsb_Initialize, WinUsb_ReadIsochPipeAsap, WinUsb_ReadPipe,
    WinUsb_RegisterIsochBuffer, WinUsb_SetCurrentAlternateSetting, WinUsb_SetPipePolicy,
    WinUsb_UnregisterIsochBuffer, WinUsb_WritePipe,
};
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, ERROR_IO_PENDING, ERROR_SEM_TIMEOUT, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{GUID, PCWSTR};

/// Matches `DeviceInterfaceGUIDs` in `driver/winusb/ak1-winusb.inf`.
const AK1_WINUSB_INTERFACE: GUID = GUID::from_u128(0x1ceaa755_a4c9_4fb8_a4a0_2e913917f458);
const COMMAND_TIMEOUT_MS: u32 = 1000;
const SYNC_TIMEOUT_MS: u32 = 100;
const SYNC_ATTEMPTS: usize = 4;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Ak1 {
    file: HANDLE,
    usb: WINUSB_INTERFACE_HANDLE,
    spec: Option<DeviceSpec>,
}

impl Ak1 {
    pub fn open() -> Result<Ak1> {
        let path = device_path()?;
        // WinUSB keeps the device exclusively open for up to a second after the
        // previous handle closes.
        let mut attempts = 0;
        let file = loop {
            let opened = unsafe {
                CreateFileW(
                    PCWSTR(path.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            };
            match opened {
                Err(e) if e.code() == E_ACCESSDENIED && attempts < 30 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                other => break other?,
            }
        };
        let mut usb = WINUSB_INTERFACE_HANDLE::default();
        if let Err(e) = unsafe { WinUsb_Initialize(file, &mut usb) } {
            unsafe { CloseHandle(file)? };
            return Err(e.into());
        }
        let mut dev = Ak1 { file, usb, spec: None };
        unsafe { WinUsb_SetCurrentAlternateSetting(usb, STREAMING_ALT_SETTING)? };
        dev.set_reply_timeout(SYNC_TIMEOUT_MS)?;
        dev.spec = Some(dev.sync()?);
        dev.set_reply_timeout(COMMAND_TIMEOUT_MS)?;
        Ok(dev)
    }

    pub fn spec(&self) -> DeviceSpec {
        self.spec.expect("set by open")
    }

    /// The firmware keeps its endpoint 1 data toggles across SET_INTERFACE and
    /// CLEAR_FEATURE(ENDPOINT_HALT), so when an earlier session ended with odd
    /// toggles the first transfer in each direction is silently discarded. Each
    /// lost transfer flips the host's toggle, so repeating an idempotent query
    /// realigns both pipes within a few attempts.
    fn sync(&self) -> Result<DeviceSpec> {
        let mut buf = [0u8; CMD_BUF_SIZE];
        for _ in 0..SYNC_ATTEMPTS {
            self.send(&get_device_info_request())?;
            match self.receive(&mut buf) {
                Ok(msg) => {
                    if let Some(Reply::DeviceInfo(spec)) = Reply::parse(msg) {
                        return Ok(spec);
                    }
                }
                Err(e) if e.code() == ERROR_SEM_TIMEOUT.to_hresult() => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(format!("no device info reply after {SYNC_ATTEMPTS} attempts").into())
    }

    fn set_reply_timeout(&self, ms: u32) -> Result<()> {
        unsafe {
            WinUsb_SetPipePolicy(
                self.usb,
                EP_CMD_IN,
                PIPE_TRANSFER_TIMEOUT,
                size_of::<u32>() as u32,
                (&ms as *const u32).cast(),
            )?
        };
        Ok(())
    }

    pub fn send(&self, request: &[u8]) -> Result<()> {
        let mut written = 0;
        unsafe { WinUsb_WritePipe(self.usb, EP_CMD_OUT, request, Some(&mut written), None)? };
        if written as usize != request.len() {
            return Err(format!("command 0x{:02x}: wrote {written} of {} bytes", request[0], request.len()).into());
        }
        Ok(())
    }

    /// Blocks until the next message on the command IN pipe or the timeout.
    pub fn receive<'a>(&self, buf: &'a mut [u8; CMD_BUF_SIZE]) -> windows::core::Result<&'a [u8]> {
        let mut read = 0;
        unsafe { WinUsb_ReadPipe(self.usb, EP_CMD_IN, Some(buf), Some(&mut read), None)? };
        Ok(&buf[..read as usize])
    }
}

impl Drop for Ak1 {
    fn drop(&mut self) {
        unsafe {
            let _ = WinUsb_Free(self.usb);
            let _ = CloseHandle(self.file);
        }
    }
}

fn device_path() -> Result<Vec<u16>> {
    let mut len = 0;
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(&mut len, &AK1_WINUSB_INTERFACE, PCWSTR::null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
    };
    if cr != CR_SUCCESS {
        return Err(format!("CM_Get_Device_Interface_List_SizeW failed: {}", cr.0).into());
    }
    let mut list = vec![0u16; len as usize];
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(&AK1_WINUSB_INTERFACE, PCWSTR::null(), &mut list, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
    };
    if cr != CR_SUCCESS {
        return Err(format!("CM_Get_Device_Interface_ListW failed: {}", cr.0).into());
    }
    let end = list.iter().position(|&c| c == 0).unwrap_or(0);
    if end == 0 {
        return Err("no Audio Kontrol 1 bound to WinUSB found".into());
    }
    list.truncate(end + 1);
    Ok(list)
}

/// A ring of isochronous IN transfers kept continuously queued on one pipe.
pub struct IsochReader<'d> {
    dev: &'d Ak1,
    pipe: u8,
    buffer: Box<[u8]>,
    registration: *mut c_void,
    overlapped: Box<[OVERLAPPED]>,
    packets: Box<[USBD_ISO_PACKET_DESCRIPTOR]>,
    packets_per_transfer: usize,
    packet_size: usize,
    next: usize,
}

pub struct CompletedTransfer<'a> {
    pub data: &'a [u8],
    pub packets: &'a [USBD_ISO_PACKET_DESCRIPTOR],
}

impl<'d> IsochReader<'d> {
    /// `packets_per_transfer` must be a multiple of 8 (one high-speed frame).
    pub fn start(dev: &'d Ak1, pipe: u8, packet_size: usize, packets_per_transfer: usize, transfers: usize) -> Result<Self> {
        let transfer_len = packet_size * packets_per_transfer;
        let mut buffer = vec![0u8; transfer_len * transfers].into_boxed_slice();
        let mut registration = std::ptr::null_mut();
        unsafe { WinUsb_RegisterIsochBuffer(dev.usb, pipe, &mut buffer, &mut registration)? };
        let mut overlapped: Box<[OVERLAPPED]> = (0..transfers).map(|_| OVERLAPPED::default()).collect();
        for o in overlapped.iter_mut() {
            o.hEvent = unsafe { CreateEventW(None, true, false, None)? };
        }
        let mut reader = IsochReader {
            dev,
            pipe,
            buffer,
            registration,
            overlapped,
            packets: vec![USBD_ISO_PACKET_DESCRIPTOR::default(); packets_per_transfer * transfers].into_boxed_slice(),
            packets_per_transfer,
            packet_size,
            next: 0,
        };
        for i in 0..transfers {
            reader.submit(i, i != 0)?;
        }
        Ok(reader)
    }

    fn submit(&mut self, index: usize, continue_stream: bool) -> Result<()> {
        let transfer_len = self.packet_size * self.packets_per_transfer;
        let packets = &mut self.packets[index * self.packets_per_transfer..][..self.packets_per_transfer];
        let result = unsafe {
            WinUsb_ReadIsochPipeAsap(
                self.registration,
                (index * transfer_len) as u32,
                transfer_len as u32,
                continue_stream,
                packets,
                Some(&self.overlapped[index]),
            )
        };
        match result {
            Err(e) if e.code() != ERROR_IO_PENDING.to_hresult() => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Waits for the oldest transfer, hands it to `consume`, then requeues it.
    pub fn next(&mut self, consume: impl FnOnce(CompletedTransfer<'_>)) -> Result<()> {
        let index = self.next;
        let mut ignored = 0;
        unsafe { WinUsb_GetOverlappedResult(self.dev.usb, &self.overlapped[index], &mut ignored, true)? };
        let transfer_len = self.packet_size * self.packets_per_transfer;
        consume(CompletedTransfer {
            data: &self.buffer[index * transfer_len..][..transfer_len],
            packets: &self.packets[index * self.packets_per_transfer..][..self.packets_per_transfer],
        });
        self.submit(index, true)?;
        self.next = (index + 1) % self.overlapped.len();
        Ok(())
    }
}

impl Drop for IsochReader<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = WinUsb_AbortPipe(self.dev.usb, self.pipe);
            for o in self.overlapped.iter() {
                let mut ignored = 0;
                let _ = WinUsb_GetOverlappedResult(self.dev.usb, o, &mut ignored, true);
                let _ = CloseHandle(o.hEvent);
            }
            let _ = WinUsb_UnregisterIsochBuffer(self.registration);
        }
    }
}
