use ak1_proto::{
    DeviceSpec, EP_AUDIO_IN, EP_AUDIO_OUT, EP_CMD_IN, EP_CMD_OUT, MAX_PACKET_SIZE, Reply, STREAMING_ALT_SETTING,
    SampleRate, audio_params_request, get_device_info_request,
};
use wdk_sys::{
    _WDF_MEMORY_DESCRIPTOR_TYPE, _WDF_REQUEST_SEND_OPTIONS_FLAGS, _WdfUsbTargetDeviceSelectConfigType,
    _WdfUsbTargetDeviceSelectSettingType, NT_SUCCESS, NTSTATUS, STATUS_DEVICE_CONFIGURATION_ERROR,
    STATUS_IO_TIMEOUT, STATUS_NOT_SUPPORTED, ULONG, WDF_MEMORY_DESCRIPTOR, WDF_NO_OBJECT_ATTRIBUTES, WDF_REQUEST_SEND_OPTIONS,
    WDF_USB_DEVICE_CREATE_CONFIG, WDF_USB_DEVICE_SELECT_CONFIG_PARAMS, WDF_USB_INTERFACE_SELECT_SETTING_PARAMS,
    WDF_USB_PIPE_INFORMATION, WDFDEVICE, WDFUSBDEVICE, WDFUSBINTERFACE, WDFUSBPIPE,
    call_unsafe_wdf_function_binding,
};

const USBD_CLIENT_CONTRACT_VERSION_602: ULONG = 0x602;
const COMMAND_TIMEOUT_MS: i64 = 1000;
const SYNC_TIMEOUT_MS: i64 = 100;
const SYNC_ATTEMPTS: usize = 4;
const REPLY_ATTEMPTS: usize = 8;

pub struct Ak1Usb {
    pub device: WDFUSBDEVICE,
    interface: WDFUSBINTERFACE,
    pub cmd_out: WDFUSBPIPE,
    pub cmd_in: WDFUSBPIPE,
    pub audio_in: WDFUSBPIPE,
    pub audio_out: WDFUSBPIPE,
}

impl Ak1Usb {
    /// Opens the USB target of `device` and selects the streaming alternate setting.
    pub unsafe fn open(device: WDFDEVICE) -> Result<Ak1Usb, NTSTATUS> {
        let mut create_config = WDF_USB_DEVICE_CREATE_CONFIG {
            Size: size_of::<WDF_USB_DEVICE_CREATE_CONFIG>() as ULONG,
            USBDClientContractVersion: USBD_CLIENT_CONTRACT_VERSION_602,
        };
        let mut usb_device: WDFUSBDEVICE = core::ptr::null_mut();
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfUsbTargetDeviceCreateWithParameters,
                device,
                &mut create_config,
                WDF_NO_OBJECT_ATTRIBUTES,
                &mut usb_device
            )
        })?;

        let mut select_config: WDF_USB_DEVICE_SELECT_CONFIG_PARAMS = unsafe { core::mem::zeroed() };
        select_config.Size = size_of::<WDF_USB_DEVICE_SELECT_CONFIG_PARAMS>() as ULONG;
        select_config.Type = _WdfUsbTargetDeviceSelectConfigType::WdfUsbTargetDeviceSelectConfigTypeSingleInterface;
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfUsbTargetDeviceSelectConfig,
                usb_device,
                WDF_NO_OBJECT_ATTRIBUTES,
                &mut select_config
            )
        })?;
        let mut usb = Ak1Usb {
            device: usb_device,
            interface: unsafe { select_config.Types.SingleInterface.ConfiguredUsbInterface },
            cmd_out: core::ptr::null_mut(),
            cmd_in: core::ptr::null_mut(),
            audio_in: core::ptr::null_mut(),
            audio_out: core::ptr::null_mut(),
        };
        unsafe { usb.select_streaming()? };
        Ok(usb)
    }

    /// Selects the streaming alternate setting, which replaces all pipe objects.
    pub unsafe fn select_streaming(&mut self) -> Result<(), NTSTATUS> {
        let interface = self.interface;
        let mut select_setting: WDF_USB_INTERFACE_SELECT_SETTING_PARAMS = unsafe { core::mem::zeroed() };
        select_setting.Size = size_of::<WDF_USB_INTERFACE_SELECT_SETTING_PARAMS>() as ULONG;
        select_setting.Type = _WdfUsbTargetDeviceSelectSettingType::WdfUsbInterfaceSelectSettingTypeSetting;
        select_setting.Types.Interface.SettingIndex = STREAMING_ALT_SETTING;
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfUsbInterfaceSelectSetting,
                interface,
                WDF_NO_OBJECT_ATTRIBUTES,
                &mut select_setting
            )
        })?;

        let pipe = |address| unsafe { find_pipe(interface, address) };
        self.cmd_out = pipe(EP_CMD_OUT)?;
        self.cmd_in = pipe(EP_CMD_IN)?;
        self.audio_in = pipe(EP_AUDIO_IN)?;
        self.audio_out = pipe(EP_AUDIO_OUT)?;
        Ok(())
    }

    pub unsafe fn reset_port(&self) -> Result<(), NTSTATUS> {
        check(unsafe { call_unsafe_wdf_function_binding!(WdfUsbTargetDeviceResetPortSynchronously, self.device) })
    }

    /// The firmware keeps its endpoint 1 data toggles across SET_INTERFACE and
    /// CLEAR_FEATURE(ENDPOINT_HALT), so after a session that ended with odd
    /// toggles the first transfer in each direction is silently discarded.
    /// Repeating an idempotent query realigns both pipes within a few attempts.
    pub unsafe fn sync(&self) -> Result<DeviceSpec, NTSTATUS> {
        let mut buf = [0u8; MAX_PACKET_SIZE];
        for _ in 0..SYNC_ATTEMPTS {
            unsafe { self.send(&get_device_info_request())? };
            match unsafe { self.receive(&mut buf, SYNC_TIMEOUT_MS) } {
                Ok(len) => {
                    if let Some(Reply::DeviceInfo(spec)) = Reply::parse(&buf[..len]) {
                        return Ok(spec);
                    }
                }
                Err(STATUS_IO_TIMEOUT) => {}
                Err(status) => return Err(status),
            }
        }
        Err(STATUS_IO_TIMEOUT)
    }

    pub unsafe fn set_audio_params(&self, rate: SampleRate, max_packet_bytes: u16) -> Result<(), NTSTATUS> {
        unsafe { self.send(&audio_params_request(rate, max_packet_bytes))? };
        let mut buf = [0u8; MAX_PACKET_SIZE];
        // Unsolicited input reports may arrive before the answer.
        for _ in 0..REPLY_ATTEMPTS {
            let len = unsafe { self.receive(&mut buf, COMMAND_TIMEOUT_MS)? };
            if let Some(Reply::AudioParams { accepted }) = Reply::parse(&buf[..len]) {
                return if accepted { Ok(()) } else { Err(STATUS_NOT_SUPPORTED) };
            }
        }
        Err(STATUS_IO_TIMEOUT)
    }

    pub unsafe fn send(&self, request: &[u8]) -> Result<(), NTSTATUS> {
        // The framework only reads from the buffer of a write request.
        let mut memory = buffer_descriptor(request.as_ptr().cast_mut(), request.len());
        let mut options = timeout_options(COMMAND_TIMEOUT_MS);
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfUsbTargetPipeWriteSynchronously,
                self.cmd_out,
                core::ptr::null_mut(),
                &mut options,
                &mut memory,
                core::ptr::null_mut()
            )
        })
    }

    /// Reads one reply; `buf` must be a whole number of max-size packets.
    pub unsafe fn receive(&self, buf: &mut [u8], timeout_ms: i64) -> Result<usize, NTSTATUS> {
        let mut memory = buffer_descriptor(buf.as_mut_ptr(), buf.len());
        let mut options = timeout_options(timeout_ms);
        let mut read: ULONG = 0;
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfUsbTargetPipeReadSynchronously,
                self.cmd_in,
                core::ptr::null_mut(),
                &mut options,
                &mut memory,
                &mut read
            )
        })?;
        Ok(read as usize)
    }
}

unsafe fn find_pipe(interface: WDFUSBINTERFACE, address: u8) -> Result<WDFUSBPIPE, NTSTATUS> {
    let count = unsafe { call_unsafe_wdf_function_binding!(WdfUsbInterfaceGetNumConfiguredPipes, interface) };
    for index in 0..count {
        let mut info: WDF_USB_PIPE_INFORMATION = unsafe { core::mem::zeroed() };
        info.Size = size_of::<WDF_USB_PIPE_INFORMATION>() as ULONG;
        let pipe =
            unsafe { call_unsafe_wdf_function_binding!(WdfUsbInterfaceGetConfiguredPipe, interface, index, &mut info) };
        if info.EndpointAddress == address {
            return Ok(pipe);
        }
    }
    Err(STATUS_DEVICE_CONFIGURATION_ERROR)
}

fn buffer_descriptor(buf: *mut u8, len: usize) -> WDF_MEMORY_DESCRIPTOR {
    let mut memory: WDF_MEMORY_DESCRIPTOR = unsafe { core::mem::zeroed() };
    memory.Type = _WDF_MEMORY_DESCRIPTOR_TYPE::WdfMemoryDescriptorTypeBuffer;
    memory.u.BufferType.Buffer = buf.cast();
    memory.u.BufferType.Length = len as ULONG;
    memory
}

fn timeout_options(ms: i64) -> WDF_REQUEST_SEND_OPTIONS {
    WDF_REQUEST_SEND_OPTIONS {
        Size: size_of::<WDF_REQUEST_SEND_OPTIONS>() as ULONG,
        Flags: _WDF_REQUEST_SEND_OPTIONS_FLAGS::WDF_REQUEST_SEND_OPTION_TIMEOUT as ULONG,
        // Negative means relative, in 100 ns units.
        Timeout: -ms * 10_000,
    }
}

fn check(status: NTSTATUS) -> Result<(), NTSTATUS> {
    if NT_SUCCESS(status) { Ok(()) } else { Err(status) }
}
