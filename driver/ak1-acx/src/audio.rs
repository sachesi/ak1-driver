//! Ties the ACX streams to the USB engine. The device has one clock, so the
//! first stream fixes the sample rate for all others until every stream is
//! gone, and the engine runs while any stream is prepared and the device is
//! in D0.

extern crate alloc;

use alloc::boxed::Box;
use core::cell::{Cell, UnsafeCell};

use ak1_proto::{DeviceSpec, SampleRate};
use wdk_sys::ntddk::{KeAcquireSpinLockRaiseToDpc, KeReleaseSpinLock};
use wdk_sys::{
    KSPIN_LOCK, NTSTATUS, STATUS_DEVICE_NOT_READY, STATUS_NOT_SUPPORTED, WDFDEVICE, WDFWAITLOCK,
    call_unsafe_wdf_function_binding,
};

use crate::rt::{RtStream, Slot};
use crate::stream::Engine;
use crate::usb::Ak1Usb;

struct Shared {
    streams: u32,
    rate: Option<SampleRate>,
}

struct Hardware {
    usb: Option<*const Ak1Usb>,
    spec: Option<DeviceSpec>,
    engine: Option<Box<Engine>>,
    prepared: u32,
}

pub struct Audio {
    device: WDFDEVICE,
    /// Serializes engine start and stop, which happen at PASSIVE_LEVEL.
    control: WDFWAITLOCK,
    hardware: UnsafeCell<Hardware>,
    /// Guards `shared`, `running` and everything the engine touches while it
    /// processes a transfer.
    lock: UnsafeCell<KSPIN_LOCK>,
    shared: UnsafeCell<Shared>,
    running: UnsafeCell<[*const RtStream; 3]>,
}

pub struct Frames<'a> {
    audio: &'a Audio,
    qpc: Cell<u64>,
}

impl Audio {
    pub fn new(device: WDFDEVICE, control: WDFWAITLOCK) -> Audio {
        Audio {
            device,
            control,
            hardware: UnsafeCell::new(Hardware { usb: None, spec: None, engine: None, prepared: 0 }),
            lock: UnsafeCell::new(0),
            shared: UnsafeCell::new(Shared { streams: 0, rate: None }),
            running: UnsafeCell::new([core::ptr::null(); 3]),
        }
    }

    /// Called from the device's PrepareHardware once the USB target is open.
    pub unsafe fn attach(&self, usb: *const Ak1Usb, spec: DeviceSpec) {
        let _guard = unsafe { self.lock_control() };
        let hardware = unsafe { &mut *self.hardware.get() };
        hardware.usb = Some(usb);
        hardware.spec = Some(spec);
    }

    /// Reserves the device clock for a new stream at `rate`.
    pub unsafe fn claim_rate(&self, rate: SampleRate) -> Result<(), NTSTATUS> {
        unsafe {
            self.with_lock(|| {
                let shared = &mut *self.shared.get();
                match shared.rate {
                    Some(current) if current != rate => Err(STATUS_NOT_SUPPORTED),
                    _ => {
                        shared.rate = Some(rate);
                        shared.streams += 1;
                        Ok(())
                    }
                }
            })
        }
    }

    pub unsafe fn release_rate(&self) {
        unsafe {
            self.with_lock(|| {
                let shared = &mut *self.shared.get();
                shared.streams -= 1;
                if shared.streams == 0 {
                    shared.rate = None;
                }
            })
        }
    }

    pub unsafe fn prepare(&self, stream: &RtStream) -> Result<(), NTSTATUS> {
        let _guard = unsafe { self.lock_control() };
        let hardware = unsafe { &mut *self.hardware.get() };
        unsafe { stream.reset() };
        if hardware.engine.is_none() {
            unsafe { self.start_engine(hardware, stream.rate)? };
        }
        hardware.prepared += 1;
        Ok(())
    }

    pub unsafe fn release(&self, stream: &RtStream) {
        unsafe { self.pause(stream) };
        let _guard = unsafe { self.lock_control() };
        let hardware = unsafe { &mut *self.hardware.get() };
        hardware.prepared = hardware.prepared.saturating_sub(1);
        if hardware.prepared == 0 {
            unsafe { self.stop_engine(hardware) };
        }
    }

    /// Stops streaming before the device leaves D0; prepared streams stay
    /// prepared and `resume` starts streaming for them again.
    pub unsafe fn suspend(&self) {
        let _guard = unsafe { self.lock_control() };
        unsafe { self.stop_engine(&mut *self.hardware.get()) };
    }

    pub unsafe fn resume(&self) -> Result<(), NTSTATUS> {
        let _guard = unsafe { self.lock_control() };
        let hardware = unsafe { &mut *self.hardware.get() };
        let rate = unsafe { self.with_lock(|| (*self.shared.get()).rate) };
        match rate {
            Some(rate) if hardware.prepared > 0 && hardware.engine.is_none() => unsafe {
                self.start_engine(hardware, rate)
            },
            _ => Ok(()),
        }
    }

    unsafe fn start_engine(&self, hardware: &mut Hardware, rate: SampleRate) -> Result<(), NTSTATUS> {
        let (Some(usb), Some(spec)) = (hardware.usb, hardware.spec) else { return Err(STATUS_DEVICE_NOT_READY) };
        let engine = unsafe { Engine::start(self.device, &*usb, rate, spec.max_packet_bytes(rate), self)? };
        hardware.engine = Some(engine);
        Ok(())
    }

    unsafe fn stop_engine(&self, hardware: &mut Hardware) {
        if let Some(engine) = hardware.engine.take() {
            unsafe { engine.stop() };
            unsafe { crate::record_stream_stats(self.device, &engine.stats) };
        }
    }

    pub unsafe fn run(&self, stream: &RtStream) {
        unsafe { self.with_lock(|| (*self.running.get())[stream.slot as usize] = stream) };
    }

    pub unsafe fn pause(&self, stream: &RtStream) {
        unsafe {
            self.with_lock(|| {
                let slot = &mut (*self.running.get())[stream.slot as usize];
                if core::ptr::eq(*slot, stream) {
                    *slot = core::ptr::null();
                }
            })
        };
    }

    pub unsafe fn position(&self, stream: &RtStream) -> (u64, u64) {
        unsafe { self.with_lock(|| stream.position()) }
    }

    /// Runs `process` with the running streams while holding the audio lock.
    pub unsafe fn process<R>(&self, process: impl FnOnce(&Frames) -> R) -> R {
        unsafe { self.with_lock(|| process(&Frames { audio: self, qpc: Cell::new(0) })) }
    }

    unsafe fn with_lock<R>(&self, f: impl FnOnce() -> R) -> R {
        let irql = unsafe { KeAcquireSpinLockRaiseToDpc(self.lock.get()) };
        let result = f();
        unsafe { KeReleaseSpinLock(self.lock.get(), irql) };
        result
    }

    unsafe fn lock_control(&self) -> ControlGuard {
        let _ = unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockAcquire, self.control, core::ptr::null_mut()) };
        ControlGuard(self.control)
    }
}

struct ControlGuard(WDFWAITLOCK);

impl Drop for ControlGuard {
    fn drop(&mut self) {
        unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockRelease, self.0) };
    }
}

impl Frames<'_> {
    /// Sets the QPC time stamped on the frames that follow.
    pub fn at(&self, qpc: u64) {
        self.qpc.set(qpc);
    }

    fn stream(&self, slot: Slot) -> Option<&RtStream> {
        let stream = unsafe { (*self.audio.running.get())[slot as usize] };
        unsafe { stream.as_ref() }
    }

    /// Next playback frame for both output pairs, silence where no stream runs.
    pub fn render(&self) -> [i32; 4] {
        let [a, b] = self.stream(Slot::Output12).map_or([0; 2], |s| unsafe { s.render_frame(self.qpc.get()) });
        let [c, d] = self.stream(Slot::Output34).map_or([0; 2], |s| unsafe { s.render_frame(self.qpc.get()) });
        [a, b, c, d]
    }

    /// Delivers a captured frame of inputs 1/2.
    pub fn capture(&self, samples: [i32; 2]) {
        if let Some(stream) = self.stream(Slot::Input12) {
            unsafe { stream.capture_frame(samples, self.qpc.get()) };
        }
    }
}
