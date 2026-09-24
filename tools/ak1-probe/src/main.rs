mod usb;
mod wasapi;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::ExitCode;

use ak1_proto::{EP_AUDIO_IN, MAX_PACKET_SIZE, Reply, SampleRate};
use usb::{Ak1, IsochReader, Result};

const USAGE: &str = "usage: ak1-probe info | capture <rate-hz> <seconds> [raw-output-file] | endpoints \
                     | play <endpoint> <seconds> <tone-hz> [exclusive-rate-hz] | identify <endpoint> \
                     | record <endpoint> <seconds> [exclusive-rate-hz]";
const MICROFRAMES_PER_SECOND: u64 = 8000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ak1-probe: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    match args {
        [cmd] if cmd == "info" => {
            println!("{:#?}", Ak1::open()?.spec());
            Ok(())
        }
        [cmd, rate, seconds, rest @ ..] if cmd == "capture" && rest.len() <= 1 => {
            let hz: u32 = rate.parse()?;
            let rate = SampleRate::from_hz(hz).ok_or_else(|| format!("unsupported sample rate {hz}"))?;
            capture(rate, seconds.parse()?, rest.first().map(String::as_str))
        }
        [cmd] if cmd == "endpoints" => wasapi::list(),
        [cmd, endpoint] if cmd == "identify" => wasapi::identify(endpoint),
        [cmd, endpoint, seconds, tone, rate @ ..] if cmd == "play" && rate.len() <= 1 => {
            let rate = rate.first().map(|r| r.parse()).transpose()?;
            wasapi::play(endpoint, seconds.parse()?, tone.parse()?, rate)
        }
        [cmd, endpoint, seconds, rate @ ..] if cmd == "record" && rate.len() <= 1 => {
            let rate = rate.first().map(|r| r.parse()).transpose()?;
            wasapi::record(endpoint, seconds.parse()?, rate)
        }
        _ => Err(USAGE.into()),
    }
}

/// Reads command replies until `want` matches, skipping unsolicited reports.
fn await_reply<T>(dev: &Ak1, mut want: impl FnMut(Reply<'_>) -> Option<T>) -> Result<T> {
    let mut buf = [0u8; ak1_proto::CMD_BUF_SIZE];
    for _ in 0..16 {
        let msg = dev.receive(&mut buf)?;
        match Reply::parse(msg) {
            Some(reply) => {
                if let Some(v) = want(reply) {
                    return Ok(v);
                }
                eprintln!("skipped {reply:02x?}");
            }
            None => eprintln!("skipped unparsable message {msg:02x?}"),
        }
    }
    Err("no matching reply after 16 messages".into())
}

fn capture(rate: SampleRate, seconds: f64, raw_path: Option<&str>) -> Result<()> {
    let dev = Ak1::open()?;
    let spec = dev.spec();
    let max_packet = spec.max_packet_bytes(rate);
    dev.send(&ak1_proto::audio_params_request(rate, max_packet))?;
    let accepted = await_reply(&dev, |r| match r {
        Reply::AudioParams { accepted } => Some(accepted),
        _ => None,
    })
    .map_err(|e| format!("waiting for audio params reply: {e}"))?;
    if !accepted {
        return Err(format!("device rejected {} Hz, max packet {max_packet}", rate.hz()).into());
    }

    let mut raw = raw_path.map(File::create).transpose()?.map(BufWriter::new);
    let target_packets = (seconds * MICROFRAMES_PER_SECOND as f64) as u64;
    let (mut packets, mut failed, mut bytes) = (0u64, 0u64, 0u64);
    let mut lengths = BTreeMap::<u32, u64>::new();
    let mut write_err = None;

    let mut reader = IsochReader::start(&dev, EP_AUDIO_IN, MAX_PACKET_SIZE, 64, 8)
        .map_err(|e| format!("starting isochronous capture: {e}"))?;
    while packets < target_packets {
        reader.next(|t| {
            for p in t.packets {
                // The device sends empty packets until its converters are running.
                if bytes == 0 && p.Length == 0 && p.Status == 0 {
                    continue;
                }
                packets += 1;
                if p.Status != 0 {
                    failed += 1;
                    continue;
                }
                *lengths.entry(p.Length).or_default() += 1;
                bytes += u64::from(p.Length);
                if let Some(w) = raw.as_mut() {
                    let data = &t.data[p.Offset as usize..][..p.Length as usize];
                    if let Err(e) = w.write_all(data) {
                        write_err.get_or_insert(e);
                    }
                }
            }
        })
        .map_err(|e| format!("isochronous capture after {packets} packets: {e}"))?;
    }
    drop(reader);
    if let Some(e) = write_err {
        return Err(e.into());
    }
    if let Some(mut w) = raw {
        w.flush()?;
    }

    let frame_bytes = (spec.wire_bytes_per_sample() * ak1_proto::CHANNELS_PER_STREAM * spec.streams()) as u64;
    let frames = bytes / frame_bytes;
    println!("rate {} Hz, max packet {max_packet} B, frame {frame_bytes} B", rate.hz());
    println!("packets {packets}, failed {failed}, bytes {bytes}, remainder {} B", bytes % frame_bytes);
    println!("measured {:.1} frames/s", frames as f64 * MICROFRAMES_PER_SECOND as f64 / packets as f64);
    println!("packet lengths: {lengths:?}");
    Ok(())
}
