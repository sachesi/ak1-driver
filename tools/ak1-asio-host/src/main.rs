//! Minimal ASIO host: loads the registered driver through COM, streams a tone
//! on every output while recording every input, and reports callback timing.

use std::ffi::c_void;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ak1_asio::CLSID;
use ak1_asio::abi::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows_core::IUnknown;

const USAGE: &str = "usage: ak1-asio-host <rate-hz> <buffer-frames|pref> <seconds> [channels, e.g. in1,out3,out4]";
const INPUTS: usize = 2;
const OUTPUTS: usize = 4;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Driver = *mut *const IAsioVtbl<c_void>;

struct Session {
    frames: usize,
    rate: f64,
    inputs: Vec<[*mut i32; 2]>,
    outputs: Vec<[*mut i32; 2]>,
    phase: f64,
    peak: f64,
    last: Option<Instant>,
    worst_gap: Duration,
}

unsafe impl Send for Session {}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static SWITCHES: AtomicU64 = AtomicU64::new(0);
static LAST_POSITION: AtomicU64 = AtomicU64::new(0);
static POSITION_ERRORS: AtomicUsize = AtomicUsize::new(0);
static RESET_REQUESTS: AtomicUsize = AtomicUsize::new(0);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ak1-asio-host: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    let (rate, frames, seconds, channels) = match args {
        [rate, frames, seconds] => (rate, frames, seconds, None),
        [rate, frames, seconds, channels] => (rate, frames, seconds, Some(channels.as_str())),
        _ => return Err(USAGE.into()),
    };
    let rate: f64 = rate.parse()?;
    let seconds: f64 = seconds.parse()?;

    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let unknown: IUnknown = unsafe { CoCreateInstance(&CLSID, None, CLSCTX_INPROC_SERVER)? };
    let driver = unknown.clone();
    let this: Driver = windows_core::Interface::as_raw(&driver).cast();
    let vtbl = unsafe { &**this };
    let call = |error: AsioError, what: &str| -> Result<()> {
        if error == ASE_OK { Ok(()) } else { Err(format!("{what} returned {error}: {}", message(this)).into()) }
    };

    if unsafe { (vtbl.init)(this.cast(), std::ptr::null_mut()) } == 0 {
        return Err(format!("init failed: {}", message(this)).into());
    }
    let mut name = [0u8; 32];
    unsafe { (vtbl.get_driver_name)(this.cast(), name.as_mut_ptr()) };
    let (mut inputs, mut outputs) = (0, 0);
    call(unsafe { (vtbl.get_channels)(this.cast(), &mut inputs, &mut outputs) }, "getChannels")?;
    call(unsafe { (vtbl.can_sample_rate)(this.cast(), rate) }, "canSampleRate")?;
    call(unsafe { (vtbl.set_sample_rate)(this.cast(), rate) }, "setSampleRate")?;
    let (mut min, mut max, mut preferred, mut granularity) = (0, 0, 0, 0);
    call(
        unsafe { (vtbl.get_buffer_size)(this.cast(), &mut min, &mut max, &mut preferred, &mut granularity) },
        "getBufferSize",
    )?;
    let frames: i32 = if frames == "pref" { preferred } else { frames.parse()? };
    println!(
        "{} | {inputs} in, {outputs} out | buffer min {min} max {max} pref {preferred} gran {granularity} | using {frames}",
        c_str(&name)
    );

    let selected: Vec<(i32, usize)> = match channels {
        None => (0..INPUTS).map(|c| (1, c)).chain((0..OUTPUTS).map(|c| (0, c))).collect(),
        Some(list) => list.split(',').map(parse_channel).collect::<Result<_>>()?,
    };
    let mut infos: Vec<AsioBufferInfo> = selected
        .into_iter()
        .map(|(is_input, channel)| AsioBufferInfo {
            is_input,
            channel_num: channel as i32,
            buffers: [std::ptr::null_mut(); 2],
        })
        .collect();
    let callbacks = AsioCallbacks { buffer_switch, sample_rate_did_change, asio_message, buffer_switch_time_info };
    call(
        unsafe { (vtbl.create_buffers)(this.cast(), infos.as_mut_ptr(), infos.len() as i32, frames, &callbacks) },
        "createBuffers",
    )?;
    let (mut input_latency, mut output_latency) = (0, 0);
    call(unsafe { (vtbl.get_latencies)(this.cast(), &mut input_latency, &mut output_latency) }, "getLatencies")?;
    println!("latency in {input_latency} out {output_latency} frames");

    let pointers = |is_input: i32| -> Vec<[*mut i32; 2]> {
        infos.iter().filter(|i| i.is_input == is_input).map(|i| [i.buffers[0].cast(), i.buffers[1].cast()]).collect()
    };
    *SESSION.lock().unwrap() = Some(Session {
        frames: frames as usize,
        rate,
        inputs: pointers(1),
        outputs: pointers(0),
        phase: 0.0,
        peak: 0.0,
        last: None,
        worst_gap: Duration::ZERO,
    });

    call(unsafe { (vtbl.start)(this.cast()) }, "start")?;
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(seconds));
    call(unsafe { (vtbl.stop)(this.cast()) }, "stop")?;
    let elapsed = started.elapsed().as_secs_f64();
    let (mut position, mut timestamp) = (AsioU64::default(), AsioU64::default());
    call(unsafe { (vtbl.get_sample_position)(this.cast(), &mut position, &mut timestamp) }, "getSamplePosition")?;
    call(unsafe { (vtbl.dispose_buffers)(this.cast()) }, "disposeBuffers")?;

    let session = SESSION.lock().unwrap().take().expect("set above");
    let switches = SWITCHES.load(Ordering::Acquire);
    let expected = elapsed * rate / f64::from(frames);
    println!(
        "switches {switches} (expected ~{expected:.0}), worst gap {:.2} ms (period {:.2} ms), position {}, \
         position errors {}, input peak {:.1} dBFS, reset requests {}",
        session.worst_gap.as_secs_f64() * 1e3,
        f64::from(frames) / rate * 1e3,
        u64::from(position),
        POSITION_ERRORS.load(Ordering::Acquire),
        20.0 * session.peak.max(1e-9).log10(),
        RESET_REQUESTS.load(Ordering::Acquire),
    );
    drop(driver);
    drop(unknown);
    Ok(())
}

/// `in1`..`in2` or `out1`..`out4`, as (is_input, zero-based channel).
fn parse_channel(name: &str) -> Result<(i32, usize)> {
    let (is_input, number, count) = if let Some(n) = name.strip_prefix("in") {
        (1, n, INPUTS)
    } else if let Some(n) = name.strip_prefix("out") {
        (0, n, OUTPUTS)
    } else {
        return Err(format!("unknown channel {name}").into());
    };
    match number.parse::<usize>() {
        Ok(n) if (1..=count).contains(&n) => Ok((is_input, n - 1)),
        _ => Err(format!("unknown channel {name}").into()),
    }
}

fn message(this: Driver) -> String {
    let mut text = [0u8; 124];
    unsafe { ((**this).get_error_message)(this.cast(), text.as_mut_ptr()) };
    c_str(&text).to_owned()
}

fn c_str(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("?")
}

fn process(index: usize, position: Option<u64>) {
    let mut guard = SESSION.lock().unwrap();
    let Some(session) = guard.as_mut() else { return };
    let now = Instant::now();
    if let Some(last) = session.last.replace(now) {
        session.worst_gap = session.worst_gap.max(now - last);
    }
    let count = SWITCHES.fetch_add(1, Ordering::AcqRel);
    if let Some(position) = position {
        let expected = count * session.frames as u64;
        if position != expected {
            POSITION_ERRORS.fetch_add(1, Ordering::AcqRel);
        }
        LAST_POSITION.store(position, Ordering::Release);
    }
    for channel in &session.inputs {
        let samples = unsafe { std::slice::from_raw_parts(channel[index], session.frames) };
        for &s in samples {
            session.peak = session.peak.max((f64::from(s) / f64::from(i32::MAX)).abs());
        }
    }
    let step = std::f64::consts::TAU * 1000.0 / session.rate;
    for i in 0..session.frames {
        let value = (session.phase.sin() * 0.25 * f64::from(i32::MAX)) as i32;
        for channel in &session.outputs {
            unsafe { channel[index].add(i).write(value) };
        }
        session.phase = (session.phase + step) % std::f64::consts::TAU;
    }
}

unsafe extern "C" fn buffer_switch(index: i32, _direct: AsioBool) {
    process(index as usize, None);
}

unsafe extern "C" fn buffer_switch_time_info(time: *mut AsioTime, index: i32, _direct: AsioBool) -> *mut AsioTime {
    let position = unsafe { (*time).time_info.sample_position };
    process(index as usize, Some(position.into()));
    time
}

unsafe extern "C" fn sample_rate_did_change(_rate: f64) {}

unsafe extern "C" fn asio_message(selector: i32, value: i32, _message: *mut c_void, _opt: *mut f64) -> i32 {
    match selector {
        K_ASIO_SELECTOR_SUPPORTED => {
            i32::from(matches!(value, K_ASIO_ENGINE_VERSION | K_ASIO_SUPPORTS_TIME_INFO | K_ASIO_RESET_REQUEST))
        }
        K_ASIO_ENGINE_VERSION => 2,
        K_ASIO_RESET_REQUEST => {
            RESET_REQUESTS.fetch_add(1, Ordering::AcqRel);
            1
        }
        K_ASIO_SUPPORTS_TIME_INFO => 1,
        _ => 0,
    }
}
