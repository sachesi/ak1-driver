//! Isochronous streaming with implicit feedback: the device is the clock
//! master, so every capture transfer that completes is answered by a playback
//! transfer whose packets have the same lengths.

extern crate alloc;

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ak1_proto::mode2::{self, CHANNELS, PlaybackEncoder};
use ak1_proto::{MAX_PACKET_SIZE, SampleRate};
use wdk_sys::ntddk::{IoAllocateMdl, IoFreeMdl, KeDelayExecutionThread, MmBuildMdlForNonPagedPool};
use wdk_sys::{
    _MODE, _WDF_IO_TARGET_PURGE_IO_ACTION, LARGE_INTEGER, NT_SUCCESS, NTSTATUS, PMDL,
    PWDF_REQUEST_COMPLETION_PARAMS, STATUS_INSUFFICIENT_RESOURCES, STATUS_SUCCESS, URB, URB_FUNCTION_ISOCH_TRANSFER,
    USBD_ISO_PACKET_DESCRIPTOR, USBD_START_ISO_TRANSFER_ASAP, USBD_TRANSFER_DIRECTION_IN,
    USBD_TRANSFER_DIRECTION_OUT, WDF_REQUEST_REUSE_PARAMS, WDFCONTEXT, WDFDEVICE, WDFIOTARGET, WDFMEMORY,
    WDFREQUEST, WDFUSBPIPE, _URB_ISOCH_TRANSFER, call_unsafe_wdf_function_binding,
};

use crate::audio::Audio;
use crate::rt::{qpc_frequency, qpc_now};
use crate::usb::Ak1Usb;
use crate::wdf::object_attributes;

/// One millisecond of high-speed microframes, the least a URB may carry,
/// which bounds the smallest ASIO buffer. USB passthrough in a QEMU guest
/// loses capture packets at this size and reports them as successful and
/// full-sized; it needed 32 packets.
pub const PACKETS_PER_TRANSFER: usize = 8;
const TRANSFERS: usize = 16;
const TRANSFER_BYTES: usize = PACKETS_PER_TRANSFER * MAX_PACKET_SIZE;
const USBD_STATUS_SUCCESS: i32 = 0;
pub const MICROFRAMES_PER_SECOND: u32 = 8000;
const MAX_FRAMES_PER_PACKET: usize = MAX_PACKET_SIZE / mode2::FRAME_BYTES;

#[derive(Default)]
pub struct Stats {
    pub capture_transfers: AtomicU32,
    pub capture_failures: AtomicU32,
    pub frames: AtomicU32,
    pub check_errors: AtomicU32,
    pub rejected_packets: AtomicU32,
    pub playback_transfers: AtomicU32,
    pub playback_failures: AtomicU32,
    pub playback_overruns: AtomicU32,
    pub invalid_packets: AtomicU32,
}

impl Stats {
    pub fn snapshot(&self) -> [u32; 9] {
        [
            &self.capture_transfers,
            &self.capture_failures,
            &self.frames,
            &self.check_errors,
            &self.rejected_packets,
            &self.playback_transfers,
            &self.playback_failures,
            &self.playback_overruns,
            &self.invalid_packets,
        ]
        .map(|counter| counter.load(Ordering::Relaxed))
    }
}

struct Transfer {
    engine: *const Engine,
    request: WDFREQUEST,
    urb_memory: WDFMEMORY,
    urb: *mut URB,
    buffer: *mut u8,
    mdl: PMDL,
    busy: AtomicBool,
}

pub struct Engine {
    capture_pipe: WDFUSBPIPE,
    playback_pipe: WDFUSBPIPE,
    rate_hz: u32,
    max_packet_bytes: usize,
    qpc_per_microframe: u64,
    captures: Vec<Transfer>,
    playbacks: Vec<Transfer>,
    running: AtomicBool,
    in_flight: AtomicU32,
    audio: *const Audio,
    /// Only touched while holding the audio lock.
    codec: UnsafeCell<Codec>,
    pub stats: Stats,
}

#[derive(Default)]
struct Codec {
    encoder: PlaybackEncoder,
    /// Remainder, in frames times 8000, of the nominal frames per microframe.
    nominal_remainder: u32,
}

impl Codec {
    /// Frames the device consumed in a microframe whose capture packet was lost.
    fn nominal_frames(&mut self, rate_hz: u32) -> usize {
        self.nominal_remainder += rate_hz;
        let frames = self.nominal_remainder / MICROFRAMES_PER_SECOND;
        self.nominal_remainder %= MICROFRAMES_PER_SECOND;
        frames as usize
    }
}

// The codec is only touched under the audio lock; everything else is atomic
// or immutable after `start`.
unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    /// Configures the device for `rate` and starts streaming the running
    /// streams of `audio`, silence where none runs.
    pub unsafe fn start(
        device: WDFDEVICE,
        usb: &Ak1Usb,
        rate: SampleRate,
        max_packet_bytes: u16,
        audio: &Audio,
    ) -> Result<alloc::boxed::Box<Engine>, NTSTATUS> {
        unsafe { usb.set_audio_params(rate, max_packet_bytes)? };

        let mut engine = alloc::boxed::Box::new(Engine {
            capture_pipe: usb.audio_in,
            playback_pipe: usb.audio_out,
            rate_hz: rate.hz(),
            max_packet_bytes: max_packet_bytes.into(),
            qpc_per_microframe: qpc_frequency() / u64::from(MICROFRAMES_PER_SECOND),
            captures: Vec::with_capacity(TRANSFERS),
            playbacks: Vec::with_capacity(TRANSFERS),
            running: AtomicBool::new(false),
            in_flight: AtomicU32::new(0),
            audio,
            codec: UnsafeCell::new(Codec::default()),
            stats: Stats::default(),
        });
        let engine_ptr: *const Engine = &*engine;
        for _ in 0..TRANSFERS {
            let capture = unsafe { Transfer::new(engine_ptr, device, usb, usb.audio_in)? };
            engine.captures.push(capture);
            let playback = unsafe { Transfer::new(engine_ptr, device, usb, usb.audio_out)? };
            engine.playbacks.push(playback);
        }

        engine.running.store(true, Ordering::Release);
        for transfer in &engine.captures {
            if let Err(status) = unsafe { engine.submit_capture(transfer) } {
                unsafe { engine.stop() };
                return Err(status);
            }
        }
        Ok(engine)
    }

    /// Cancels all transfers and waits for their completion routines to finish.
    pub unsafe fn stop(&self) {
        self.running.store(false, Ordering::Release);
        // A completion routine may still send a transfer after this point. A
        // stopped target would queue it until the restart below, which waits
        // for it; a purged target fails it instead.
        for pipe in [self.capture_pipe, self.playback_pipe] {
            let target = io_target(pipe);
            unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfIoTargetPurge,
                    target,
                    _WDF_IO_TARGET_PURGE_IO_ACTION::WdfIoTargetPurgeIoAndWait
                )
            };
        }
        while self.in_flight.load(Ordering::Acquire) != 0 {
            let mut interval = LARGE_INTEGER { QuadPart: -10_000 };
            let _ = unsafe { KeDelayExecutionThread(_MODE::KernelMode as i8, 0, &mut interval) };
        }
        for pipe in [self.capture_pipe, self.playback_pipe] {
            let target = io_target(pipe);
            // A failure here resurfaces as a send error on the next start.
            let _ = unsafe { call_unsafe_wdf_function_binding!(WdfIoTargetStart, target) };
        }
    }

    unsafe fn submit_capture(&self, transfer: &Transfer) -> Result<(), NTSTATUS> {
        let urb = unsafe { transfer.prepare_urb(self.capture_pipe, USBD_TRANSFER_DIRECTION_IN, TRANSFER_BYTES) };
        for (i, packet) in unsafe { iso_packets(urb) }.iter_mut().enumerate() {
            packet.Offset = (i * MAX_PACKET_SIZE) as u32;
            packet.Length = MAX_PACKET_SIZE as u32;
            packet.Status = USBD_STATUS_SUCCESS;
        }
        unsafe { self.send(transfer, self.capture_pipe, Some(capture_complete)) }
    }

    unsafe fn send(
        &self,
        transfer: &Transfer,
        pipe: WDFUSBPIPE,
        routine: wdk_sys::PFN_WDF_REQUEST_COMPLETION_ROUTINE,
    ) -> Result<(), NTSTATUS> {
        let mut reuse = WDF_REQUEST_REUSE_PARAMS {
            Size: size_of::<WDF_REQUEST_REUSE_PARAMS>() as u32,
            Flags: 0,
            Status: STATUS_SUCCESS,
            NewIrp: core::ptr::null_mut(),
        };
        check(unsafe { call_unsafe_wdf_function_binding!(WdfRequestReuse, transfer.request, &mut reuse) })?;
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfUsbTargetPipeFormatRequestForUrb,
                pipe,
                transfer.request,
                transfer.urb_memory,
                core::ptr::null_mut()
            )
        })?;
        unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSetCompletionRoutine,
                transfer.request,
                routine,
                (transfer as *const Transfer).cast_mut().cast()
            )
        };
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let sent = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestSend,
                transfer.request,
                io_target(pipe),
                core::ptr::null_mut()
            )
        };
        if sent == 0 {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, transfer.request) });
        }
        Ok(())
    }

    /// Decodes a completed capture transfer and sends the matching playback transfer.
    unsafe fn answer(&self, capture: &Transfer) {
        let playback = self
            .playbacks
            .iter()
            .find(|t| t.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok());
        if playback.is_none() {
            self.stats.playback_overruns.fetch_add(1, Ordering::Relaxed);
        }

        let captured = unsafe { iso_packets(capture.urb) };
        let mut playback_packets = playback.map(|t| unsafe { iso_packets(t.urb) });
        let audio = unsafe { &*self.audio };
        // The transfer has just ended; each earlier packet ended one microframe before the next.
        let completed = qpc_now();
        let last = captured.len() as u64 - 1;
        let offset = unsafe {
            audio.process(|frames| {
                let codec = &mut *self.codec.get();
                let mut offset = 0;
                for (i, packet) in captured.iter().enumerate() {
                    frames.at(completed - (last - i as u64) * self.qpc_per_microframe);
                    let received = packet.Length as usize;
                    // Some hosts report lost packets as successful and full-sized.
                    let valid = packet.Status == USBD_STATUS_SUCCESS
                        && received <= self.max_packet_bytes
                        && received.is_multiple_of(mode2::FRAME_BYTES);
                    let len = if valid { received } else { 0 };
                    if !valid {
                        self.stats.invalid_packets.fetch_add(1, Ordering::Relaxed);
                    }
                    let data = core::slice::from_raw_parts(capture.buffer.add(packet.Offset as usize), len);
                    let mut decoded = [[0; CHANNELS]; MAX_FRAMES_PER_PACKET];
                    let mut decoded_len = 0;
                    let status = mode2::decode_capture(data, |frame| {
                        decoded[decoded_len] = frame;
                        decoded_len += 1;
                    });
                    self.stats.frames.fetch_add(status.frames as u32, Ordering::Relaxed);
                    self.stats.check_errors.fetch_add(status.check_errors as u32, Ordering::Relaxed);
                    if status.output_rejected {
                        self.stats.rejected_packets.fetch_add(1, Ordering::Relaxed);
                    }

                    // A lost packet still took device time: keep both streams
                    // moving by the nominal frame count.
                    let frame_count = if valid { status.frames } else { codec.nominal_frames(self.rate_hz) };
                    let clean = valid && status.check_errors == 0;
                    for frame in &decoded[..frame_count] {
                        frames.capture(if clean { [frame[0], frame[1]] } else { [0; 2] });
                    }

                    let out_len = frame_count * mode2::FRAME_BYTES;
                    if let (Some(transfer), Some(packets)) = (playback, playback_packets.as_deref_mut()) {
                        packets[i].Offset = offset as u32;
                        packets[i].Length = out_len as u32;
                        let out = core::slice::from_raw_parts_mut(transfer.buffer.add(offset), out_len);
                        codec.encoder.encode(out, || frames.render());
                        offset += out_len;
                    }
                }
                offset
            })
        };

        let Some(transfer) = playback else { return };
        if offset == 0 {
            transfer.busy.store(false, Ordering::Release);
            return;
        }
        unsafe { transfer.prepare_urb(self.playback_pipe, USBD_TRANSFER_DIRECTION_OUT, offset) };
        if unsafe { self.send(transfer, self.playback_pipe, Some(playback_complete)) }.is_err() {
            self.stats.playback_failures.fetch_add(1, Ordering::Relaxed);
            transfer.busy.store(false, Ordering::Release);
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if self.running.load(Ordering::Acquire) {
            unsafe { self.stop() };
        }
        for transfer in self.captures.iter().chain(&self.playbacks) {
            unsafe { transfer.free() };
        }
    }
}

impl Transfer {
    unsafe fn new(
        engine: *const Engine,
        device: WDFDEVICE,
        usb: &Ak1Usb,
        pipe: WDFUSBPIPE,
    ) -> Result<Transfer, NTSTATUS> {
        let mut transfer = Transfer {
            engine,
            request: core::ptr::null_mut(),
            urb_memory: core::ptr::null_mut(),
            urb: core::ptr::null_mut(),
            buffer: core::ptr::null_mut(),
            mdl: core::ptr::null_mut(),
            busy: AtomicBool::new(false),
        };
        let result = unsafe { transfer.allocate(device, usb, pipe) };
        if result.is_err() {
            unsafe { transfer.free() };
        }
        result.map(|()| transfer)
    }

    unsafe fn allocate(&mut self, device: WDFDEVICE, usb: &Ak1Usb, pipe: WDFUSBPIPE) -> Result<(), NTSTATUS> {
        let mut attributes = object_attributes(device.cast());
        check(unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestCreate, &mut attributes, io_target(pipe), &mut self.request)
        })?;
        // The URB must belong to the request: parenting it to the USB device
        // crashes USBXHCI when the device goes away.
        let mut attributes = object_attributes(self.request.cast());
        check(unsafe {
            call_unsafe_wdf_function_binding!(
                WdfUsbTargetDeviceCreateIsochUrb,
                usb.device,
                &mut attributes,
                PACKETS_PER_TRANSFER as u32,
                &mut self.urb_memory,
                &mut self.urb
            )
        })?;
        self.buffer = alloc::boxed::Box::into_raw(alloc::vec![0u8; TRANSFER_BYTES].into_boxed_slice()).cast();
        self.mdl = unsafe {
            IoAllocateMdl(self.buffer.cast(), TRANSFER_BYTES as u32, 0, 0, core::ptr::null_mut())
        };
        if self.mdl.is_null() {
            return Err(STATUS_INSUFFICIENT_RESOURCES);
        }
        unsafe { MmBuildMdlForNonPagedPool(self.mdl) };
        Ok(())
    }

    unsafe fn free(&self) {
        if !self.mdl.is_null() {
            unsafe { IoFreeMdl(self.mdl) };
        }
        if !self.buffer.is_null() {
            drop(unsafe {
                alloc::boxed::Box::from_raw(core::ptr::slice_from_raw_parts_mut(self.buffer, TRANSFER_BYTES))
            });
        }
        if !self.request.is_null() {
            unsafe { call_unsafe_wdf_function_binding!(WdfObjectDelete, self.request.cast()) };
        }
    }

    /// Fills in the URB fields shared by both directions; the header was set
    /// up by the framework and must not be cleared.
    unsafe fn prepare_urb(&self, pipe: WDFUSBPIPE, direction: u32, length: usize) -> *mut URB {
        let iso = unsafe { &mut (*self.urb).__bindgen_anon_1.UrbIsochronousTransfer };
        iso.Hdr.Length = iso_urb_size(PACKETS_PER_TRANSFER) as u16;
        iso.Hdr.Function = URB_FUNCTION_ISOCH_TRANSFER as u16;
        iso.PipeHandle = unsafe { call_unsafe_wdf_function_binding!(WdfUsbTargetPipeWdmGetPipeHandle, pipe) };
        iso.TransferFlags = direction | USBD_START_ISO_TRANSFER_ASAP;
        iso.TransferBufferLength = length as u32;
        iso.TransferBuffer = core::ptr::null_mut();
        iso.TransferBufferMDL = self.mdl;
        iso.UrbLink = core::ptr::null_mut();
        iso.StartFrame = 0;
        iso.NumberOfPackets = PACKETS_PER_TRANSFER as u32;
        iso.ErrorCount = 0;
        self.urb
    }
}

unsafe extern "C" fn capture_complete(
    _request: WDFREQUEST,
    _target: WDFIOTARGET,
    params: PWDF_REQUEST_COMPLETION_PARAMS,
    context: WDFCONTEXT,
) {
    let transfer = unsafe { &*context.cast::<Transfer>() };
    let engine = unsafe { &*transfer.engine };
    let status = unsafe { (*params).IoStatus.__bindgen_anon_1.Status };
    if NT_SUCCESS(status) {
        engine.stats.capture_transfers.fetch_add(1, Ordering::Relaxed);
        if engine.running.load(Ordering::Acquire) {
            unsafe { engine.answer(transfer) };
            if unsafe { engine.submit_capture(transfer) }.is_err() {
                engine.stats.capture_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    } else {
        // Not sent again: once the device is gone the hub completes a resend
        // inline, which would recurse through this routine until the stack
        // overflows.
        engine.stats.capture_failures.fetch_add(1, Ordering::Relaxed);
    }
    engine.in_flight.fetch_sub(1, Ordering::AcqRel);
}

unsafe extern "C" fn playback_complete(
    _request: WDFREQUEST,
    _target: WDFIOTARGET,
    params: PWDF_REQUEST_COMPLETION_PARAMS,
    context: WDFCONTEXT,
) {
    let transfer = unsafe { &*context.cast::<Transfer>() };
    let engine = unsafe { &*transfer.engine };
    let status = unsafe { (*params).IoStatus.__bindgen_anon_1.Status };
    let counter = if NT_SUCCESS(status) { &engine.stats.playback_transfers } else { &engine.stats.playback_failures };
    counter.fetch_add(1, Ordering::Relaxed);
    transfer.busy.store(false, Ordering::Release);
    engine.in_flight.fetch_sub(1, Ordering::AcqRel);
}

unsafe fn iso_packets<'a>(urb: *mut URB) -> &'a mut [USBD_ISO_PACKET_DESCRIPTOR] {
    let first =
        unsafe { (&raw mut (*urb).__bindgen_anon_1.UrbIsochronousTransfer.IsoPacket).cast::<USBD_ISO_PACKET_DESCRIPTOR>() };
    unsafe { core::slice::from_raw_parts_mut(first, PACKETS_PER_TRANSFER) }
}

fn iso_urb_size(packets: usize) -> usize {
    size_of::<_URB_ISOCH_TRANSFER>() + (packets - 1) * size_of::<USBD_ISO_PACKET_DESCRIPTOR>()
}

/// `WdfUsbTargetPipeGetIoTarget` is an inline cast in the KMDF headers.
fn io_target(pipe: WDFUSBPIPE) -> WDFIOTARGET {
    pipe.cast()
}

fn check(status: NTSTATUS) -> Result<(), NTSTATUS> {
    if NT_SUCCESS(status) { Ok(()) } else { Err(status) }
}
