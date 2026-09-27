//! Ties the ACX streams and the ASIO session to the USB engine, which plays
//! their sum. The device has one clock: an ASIO session sets its rate,
//! otherwise the first stream does, until every stream is gone. Streams at
//! other rates are resampled. The engine runs while any stream is prepared
//! and the device is in D0.

extern crate alloc;

use alloc::boxed::Box;
use core::cell::{Cell, UnsafeCell};

use ak1_proto::{DeviceSpec, SampleRate};
use wdk_sys::ntddk::{KeAcquireSpinLockRaiseToDpc, KeReleaseSpinLock};
use wdk_sys::{
    KSPIN_LOCK, NTSTATUS, STATUS_DEVICE_BUSY, STATUS_DEVICE_NOT_READY, STATUS_INVALID_DEVICE_STATE, WDFDEVICE,
    WDFFILEOBJECT, WDFWAITLOCK, call_unsafe_wdf_function_binding,
};

use crate::asio::Session;
use crate::rt::{RtStream, Slot};
use crate::stream::Engine;
use crate::usb::Ak1Usb;

const MAX_SAMPLE: i32 = (1 << 23) - 1;
const MIN_SAMPLE: i32 = -(1 << 23);

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
    /// Guards `shared`, `running`, `asio` and everything the engine touches
    /// while it processes a transfer.
    lock: UnsafeCell<KSPIN_LOCK>,
    shared: UnsafeCell<Shared>,
    running: UnsafeCell<[*const RtStream; 3]>,
    asio: UnsafeCell<Option<Box<Session>>>,
}

pub struct Frames<'a> {
    audio: &'a Audio,
    rate_hz: u32,
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
            asio: UnsafeCell::new(None),
        }
    }

    /// Called from the device's PrepareHardware once the USB target is open.
    pub unsafe fn attach(&self, usb: *const Ak1Usb, spec: DeviceSpec) {
        let _guard = unsafe { self.lock_control() };
        let hardware = unsafe { &mut *self.hardware.get() };
        hardware.usb = Some(usb);
        hardware.spec = Some(spec);
    }

    /// Counts a new stream, whose rate becomes the device's if it has none yet.
    pub unsafe fn claim_rate(&self, rate: SampleRate) {
        unsafe {
            self.with_lock(|| {
                let shared = &mut *self.shared.get();
                shared.rate.get_or_insert(rate);
                shared.streams += 1;
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
            let rate = unsafe { self.with_lock(|| (*self.shared.get()).rate) }.unwrap_or(stream.rate);
            unsafe { self.start_engine(hardware, rate)? };
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

    /// Starts the ASIO driver's session; there is one at a time. It sets the
    /// device's rate, restarting the engine if that changes; the streams
    /// already running are resampled from then on.
    pub unsafe fn start_asio(&self, session: Box<Session>, rate: SampleRate) -> Result<(), NTSTATUS> {
        let _guard = unsafe { self.lock_control() };
        if unsafe { self.with_lock(|| (*self.asio.get()).is_some()) } {
            return Err(STATUS_DEVICE_BUSY);
        }
        let previous = unsafe {
            self.with_lock(|| {
                let shared = &mut *self.shared.get();
                shared.streams += 1;
                shared.rate.replace(rate)
            })
        };
        let hardware = unsafe { &mut *self.hardware.get() };
        if previous != Some(rate) {
            unsafe { self.stop_engine(hardware) };
        }
        if hardware.engine.is_none()
            && let Err(status) = unsafe { self.start_engine(hardware, rate) }
        {
            let restored = unsafe {
                self.with_lock(|| {
                    let shared = &mut *self.shared.get();
                    shared.streams -= 1;
                    shared.rate = if shared.streams == 0 { None } else { previous };
                    shared.rate
                })
            };
            // Streams that were playing at the old rate keep playing.
            if let Some(restored) = restored
                && hardware.prepared > 0
            {
                let _ = unsafe { self.start_engine(hardware, restored) };
            }
            return Err(status);
        }
        hardware.prepared += 1;
        unsafe { self.with_lock(|| *self.asio.get() = Some(session)) };
        Ok(())
    }

    /// Ends the session `owner` started, if any.
    pub unsafe fn stop_asio(&self, owner: WDFFILEOBJECT) {
        let take = || unsafe {
            self.with_lock(|| {
                let slot = &mut *self.asio.get();
                if slot.as_ref().is_some_and(|s| s.owner == owner) { slot.take() } else { None }
            })
        };
        // Every handle to the device is cleaned up through here, and most
        // never started a session; those should not wait for the control lock.
        if unsafe { self.with_lock(|| (*self.asio.get()).as_ref().is_none_or(|s| s.owner != owner)) } {
            return;
        }
        let _guard = unsafe { self.lock_control() };
        let Some(session) = take() else { return };
        unsafe { crate::record_asio_stats(self.device, session.stats()) };
        drop(session);
        let hardware = unsafe { &mut *self.hardware.get() };
        hardware.prepared = hardware.prepared.saturating_sub(1);
        if hardware.prepared == 0 {
            unsafe { self.stop_engine(hardware) };
        }
        unsafe { self.release_rate() };
    }

    /// Runs `f` on the session `owner` started.
    pub unsafe fn with_asio<R>(&self, owner: WDFFILEOBJECT, f: impl FnOnce(&mut Session) -> R) -> Result<R, NTSTATUS> {
        unsafe {
            self.with_lock(|| match (*self.asio.get()).as_deref_mut() {
                Some(session) if session.owner == owner => Ok(f(session)),
                _ => Err(STATUS_INVALID_DEVICE_STATE),
            })
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

    /// Runs `process` with the running streams while holding the audio lock;
    /// the device runs at `rate_hz`.
    pub unsafe fn process<R>(&self, rate_hz: u32, process: impl FnOnce(&Frames) -> R) -> R {
        unsafe { self.with_lock(|| process(&Frames { audio: self, rate_hz, qpc: Cell::new(0) })) }
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

    fn asio<R>(&self, f: impl FnOnce(&mut Session) -> R) -> Option<R> {
        unsafe { (*self.audio.asio.get()).as_deref_mut() }.map(f)
    }

    /// Next playback frame of all four outputs: the ASIO output plus each
    /// pair's stream, silence where nothing plays.
    pub fn render(&self) -> [i32; 4] {
        let mut frame = self.asio(Session::render).unwrap_or([0; 4]);
        for (slot, first) in [(Slot::Output12, 0), (Slot::Output34, 2)] {
            if let Some(stream) = self.stream(slot) {
                let [left, right] = unsafe { stream.render(self.rate_hz, self.qpc.get()) };
                frame[first] += left;
                frame[first + 1] += right;
            }
        }
        frame.map(|sample| sample.clamp(MIN_SAMPLE, MAX_SAMPLE))
    }

    /// Delivers a captured frame of inputs 1/2.
    pub fn capture(&self, samples: [i32; 2]) {
        self.asio(|session| session.capture(samples));
        if let Some(stream) = self.stream(Slot::Input12) {
            unsafe { stream.capture(samples, self.rate_hz, self.qpc.get()) };
        }
    }
}
