//! ACX real-time packet streams. The audio engine and the driver share a ring
//! of packets; the USB engine moves frames in and out of it and reports each
//! finished packet, so the device clock paces the stream.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use acx_sys::{ACX_RTPACKET, ACXSTREAM, PACX_RTPACKET, call_acx};
use ak1_proto::SampleRate;
use wdk_sys::ntddk::{
    ExAllocatePool2, ExFreePool, IoAllocateMdl, IoFreeMdl, KeQueryPerformanceCounter, MmBuildMdlForNonPagedPool,
};
use wdk_sys::{
    _WDF_MEMORY_DESCRIPTOR_TYPE, BOOLEAN, NTSTATUS, PMDL, POOL_FLAG_NON_PAGED, PULONG, PULONGLONG,
    STATUS_DATA_LATE_ERROR, STATUS_DATA_OVERRUN, STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_PARAMETER,
    STATUS_SUCCESS, ULONG,
};

use crate::audio::Audio;

const PAGE_SIZE: usize = 4096;
const MAX_PACKETS: usize = 8;
const POOL_TAG: u32 = u32::from_le_bytes(*b"ak1r");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Output12 = 0,
    Output34 = 1,
    Input12 = 2,
}

impl Slot {
    pub const ALL: [Slot; 3] = [Slot::Output12, Slot::Output34, Slot::Input12];

    pub fn is_capture(self) -> bool {
        self == Slot::Input12
    }
}

struct Ring {
    buffers: [*mut u8; MAX_PACKETS],
    count: usize,
    packet_bytes: usize,
    first_offset: usize,
}

struct Cursor {
    /// Frames moved since the stream was prepared.
    frames: u64,
    qpc: u64,
}

pub struct RtStream {
    pub acx: ACXSTREAM,
    pub slot: Slot,
    pub rate: SampleRate,
    pub audio: *const Audio,
    container_bytes: usize,
    ring: UnsafeCell<Ring>,
    /// Guarded by the audio lock.
    cursor: UnsafeCell<Cursor>,
    current_packet: AtomicU32,
    last_packet_start_qpc: AtomicU64,
}

impl RtStream {
    pub fn new(acx: ACXSTREAM, slot: Slot, rate: SampleRate, container_bits: u32, audio: *const Audio) -> RtStream {
        RtStream {
            acx,
            slot,
            rate,
            audio,
            container_bytes: container_bits as usize / 8,
            ring: UnsafeCell::new(Ring {
                buffers: [core::ptr::null_mut(); MAX_PACKETS],
                count: 0,
                packet_bytes: 0,
                first_offset: 0,
            }),
            cursor: UnsafeCell::new(Cursor { frames: 0, qpc: 0 }),
            current_packet: AtomicU32::new(0),
            last_packet_start_qpc: AtomicU64::new(0),
        }
    }

    fn block_align(&self) -> usize {
        2 * self.container_bytes
    }

    /// Reads the next render frame as 24-bit samples. Caller holds the audio lock.
    pub unsafe fn render_frame(&self, qpc: u64) -> [i32; 2] {
        let Some(frame) = (unsafe { self.frame_bytes() }) else { return [0; 2] };
        let sample = |i: usize| {
            let at = unsafe { frame.add(i * self.container_bytes) };
            if self.container_bytes == 2 {
                i32::from(i16::from_le_bytes(unsafe { at.cast::<[u8; 2]>().read() })) << 8
            } else {
                i32::from_le_bytes(unsafe { at.cast::<[u8; 4]>().read() }) >> 8
            }
        };
        let samples = [sample(0), sample(1)];
        unsafe { self.advance(qpc) };
        samples
    }

    /// Stores the next capture frame from 24-bit samples. Caller holds the audio lock.
    pub unsafe fn capture_frame(&self, samples: [i32; 2], qpc: u64) {
        if let Some(frame) = unsafe { self.frame_bytes() } {
            for (i, sample) in samples.into_iter().enumerate() {
                let at = unsafe { frame.add(i * self.container_bytes) };
                if self.container_bytes == 2 {
                    unsafe { at.cast::<[u8; 2]>().write(((sample >> 8) as i16).to_le_bytes()) };
                } else {
                    unsafe { at.cast::<[u8; 4]>().write((sample << 8).to_le_bytes()) };
                }
            }
        }
        unsafe { self.advance(qpc) };
    }

    unsafe fn frame_bytes(&self) -> Option<*mut u8> {
        let ring = unsafe { &*self.ring.get() };
        if ring.count == 0 {
            return None;
        }
        let frames_per_packet = (ring.packet_bytes / self.block_align()) as u64;
        let frames = unsafe { (*self.cursor.get()).frames };
        let index = ((frames / frames_per_packet) % ring.count as u64) as usize;
        let offset = (frames % frames_per_packet) as usize * self.block_align();
        let base = if index == 0 { ring.first_offset } else { 0 };
        Some(unsafe { ring.buffers[index].add(base + offset) })
    }

    unsafe fn advance(&self, qpc: u64) {
        let ring = unsafe { &*self.ring.get() };
        let cursor = unsafe { &mut *self.cursor.get() };
        cursor.frames += 1;
        cursor.qpc = qpc;
        if ring.count == 0 {
            return;
        }
        let frames_per_packet = (ring.packet_bytes / self.block_align()) as u64;
        if cursor.frames % frames_per_packet == 0 {
            let completed = cursor.frames / frames_per_packet - 1;
            self.current_packet.store((completed + 1) as u32, Ordering::Release);
            self.last_packet_start_qpc.store(qpc, Ordering::Release);
            let _ = unsafe { call_acx!(AcxRtStreamNotifyPacketComplete, self.acx, completed, qpc) };
        }
    }

    /// Frames moved and the QPC at the last move. Caller holds the audio lock.
    pub unsafe fn position(&self) -> (u64, u64) {
        let cursor = unsafe { &*self.cursor.get() };
        (cursor.frames, cursor.qpc)
    }

    pub unsafe fn reset(&self) {
        let cursor = unsafe { &mut *self.cursor.get() };
        cursor.frames = 0;
        cursor.qpc = 0;
        self.current_packet.store(0, Ordering::Release);
    }

    unsafe fn allocate_packets(&self, count: usize, packet_bytes: usize) -> Result<PACX_RTPACKET, NTSTATUS> {
        if count == 0 || count > MAX_PACKETS || packet_bytes == 0 || !packet_bytes.is_multiple_of(self.block_align()) {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let packets = unsafe { pool_alloc(count * size_of::<ACX_RTPACKET>()) }.cast::<ACX_RTPACKET>();
        if packets.is_null() {
            return Err(STATUS_INSUFFICIENT_RESOURCES);
        }
        // Whole pages per packet so that no unrelated kernel memory is mapped
        // into the client; packet 0 is shifted to end on a page boundary.
        let alloc_bytes = packet_bytes.div_ceil(PAGE_SIZE) * PAGE_SIZE;
        let ring = unsafe { &mut *self.ring.get() };
        for i in 0..count {
            let buffer = unsafe { pool_alloc(alloc_bytes) };
            let mdl = if buffer.is_null() {
                core::ptr::null_mut()
            } else {
                unsafe { IoAllocateMdl(buffer.cast(), alloc_bytes as u32, 0, 1, core::ptr::null_mut()) }
            };
            if mdl.is_null() {
                if !buffer.is_null() {
                    unsafe { ExFreePool(buffer.cast()) };
                }
                unsafe { self.free_packets(packets, i) };
                return Err(STATUS_INSUFFICIENT_RESOURCES);
            }
            unsafe { MmBuildMdlForNonPagedPool(mdl) };
            let packet = unsafe { &mut *packets.add(i) };
            packet.Size = size_of::<ACX_RTPACKET>() as u32;
            packet.RtPacketBuffer.Type = _WDF_MEMORY_DESCRIPTOR_TYPE::WdfMemoryDescriptorTypeMdl;
            packet.RtPacketBuffer.u.MdlType.Mdl = mdl;
            packet.RtPacketBuffer.u.MdlType.BufferLength = alloc_bytes as u32;
            packet.RtPacketOffset = if i == 0 { (alloc_bytes - packet_bytes) as u32 } else { 0 };
            packet.RtPacketSize = packet_bytes as u32;
            ring.buffers[i] = buffer;
        }
        ring.count = count;
        ring.packet_bytes = packet_bytes;
        ring.first_offset = alloc_bytes - packet_bytes;
        Ok(packets)
    }

    unsafe fn free_packets(&self, packets: PACX_RTPACKET, count: usize) {
        let ring = unsafe { &mut *self.ring.get() };
        for i in 0..count {
            let mdl: PMDL = unsafe { (*packets.add(i)).RtPacketBuffer.u.MdlType.Mdl };
            if !mdl.is_null() {
                unsafe { IoFreeMdl(mdl) };
            }
            if !ring.buffers[i].is_null() {
                unsafe { ExFreePool(ring.buffers[i].cast()) };
                ring.buffers[i] = core::ptr::null_mut();
            }
        }
        ring.count = 0;
        unsafe { ExFreePool(packets.cast()) };
    }
}

unsafe fn pool_alloc(bytes: usize) -> *mut u8 {
    unsafe { ExAllocatePool2(POOL_FLAG_NON_PAGED, bytes as u64, POOL_TAG) }.cast()
}

pub fn qpc_now() -> u64 {
    unsafe { KeQueryPerformanceCounter(core::ptr::null_mut()).QuadPart as u64 }
}

pub unsafe fn stream_of<'a>(stream: ACXSTREAM) -> &'a RtStream {
    unsafe { &**crate::stream_context(stream) }
}

pub unsafe extern "C" fn evt_prepare_hardware(stream: ACXSTREAM) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    status_of(unsafe { (*rt.audio).prepare(rt) })
}

pub unsafe extern "C" fn evt_release_hardware(stream: ACXSTREAM) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    unsafe { (*rt.audio).release(rt) };
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_run(stream: ACXSTREAM) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    unsafe { (*rt.audio).run(rt) };
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_pause(stream: ACXSTREAM) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    unsafe { (*rt.audio).pause(rt) };
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_get_hw_latency(_stream: ACXSTREAM, fifo_size: *mut ULONG, delay: *mut ULONG) -> NTSTATUS {
    unsafe {
        *fifo_size = 0;
        *delay = 0;
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_allocate_rt_packets(
    stream: ACXSTREAM,
    count: ULONG,
    size: ULONG,
    packets: *mut PACX_RTPACKET,
) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    match unsafe { rt.allocate_packets(count as usize, size as usize) } {
        Ok(allocated) => {
            unsafe { *packets = allocated };
            STATUS_SUCCESS
        }
        Err(status) => status,
    }
}

pub unsafe extern "C" fn evt_free_rt_packets(stream: ACXSTREAM, packets: PACX_RTPACKET, count: ULONG) {
    let rt = unsafe { stream_of(stream) };
    unsafe { rt.free_packets(packets, count as usize) };
}

pub unsafe extern "C" fn evt_set_render_packet(stream: ACXSTREAM, packet: ULONG, _flags: ULONG, _eos: ULONG) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    let current = rt.current_packet.load(Ordering::Acquire);
    if packet <= current {
        STATUS_DATA_LATE_ERROR
    } else if packet > current + 1 {
        STATUS_DATA_OVERRUN
    } else {
        STATUS_SUCCESS
    }
}

pub unsafe extern "C" fn evt_get_current_packet(stream: ACXSTREAM, current: PULONG) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    unsafe { *current = rt.current_packet.load(Ordering::Acquire) };
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_get_capture_packet(
    stream: ACXSTREAM,
    last_packet: PULONG,
    qpc_packet_start: PULONGLONG,
    more_data: *mut BOOLEAN,
) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    unsafe {
        *last_packet = rt.current_packet.load(Ordering::Acquire).wrapping_sub(1);
        *qpc_packet_start = rt.last_packet_start_qpc.load(Ordering::Acquire);
        *more_data = 0;
    }
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_get_presentation_position(
    stream: ACXSTREAM,
    position: PULONGLONG,
    qpc: PULONGLONG,
) -> NTSTATUS {
    let rt = unsafe { stream_of(stream) };
    let (frames, at) = unsafe { (*rt.audio).position(rt) };
    unsafe {
        *position = frames;
        *qpc = if at == 0 { qpc_now() } else { at };
    }
    STATUS_SUCCESS
}

fn status_of(result: Result<(), NTSTATUS>) -> NTSTATUS {
    result.err().unwrap_or(STATUS_SUCCESS)
}
