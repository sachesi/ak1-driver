# Audio Kontrol 1 driver for Windows 11

A driver for the Native Instruments Audio Kontrol 1 (USB 17cc:0815), which has
no vendor driver for current Windows. It is written in Rust and has two parts:

- `ak1acx.sys`, a kernel driver built on KMDF and the Audio Class Extension
  (ACX). It exposes outputs 1/2, outputs 3/4 and inputs 1/2 as Windows audio
  endpoints at 44.1, 48, 88.2, 96 and 192 kHz, 16 or 24 bit.
- `ak1_asio.dll`, an ASIO 2 driver that streams through those endpoints in
  exclusive mode.
- `ak1-panel.exe`, a control panel for the ASIO settings, the device status
  and output and input tests.

The knob, buttons, LEDs and MIDI ports are not supported yet.

## Requirements

- Windows 11 21H2 or later, x64. The driver is built for KMDF 1.33.
- Test signing. The driver is signed with a self-made test certificate, so
  Windows only loads it with Secure Boot disabled and test signing on
  (`bcdedit /set testsigning on`, then reboot). Both lower the machine's
  protection against unsigned boot-time code; undo them when you remove the
  driver.

## Install

Build a bundle (see below), copy `target\bundle` to the machine, then in an
administrator PowerShell 7 in that directory:

```powershell
.\install.ps1              # trusts the certificate, installs the driver, registers ASIO, adds the panel
.\install.ps1 -Uninstall   # removes all of it again
```

Plug the card in afterwards. The endpoints all appear as "Line (Audio Kontrol 1)",
because Windows ignores the names ACX offers for them; rename them in Sound
settings if needed. The ASIO driver shows up as "Audio Kontrol 1".

## Using it

The card has one sample clock, so every stream runs at the rate of the first
one opened. Opening another endpoint at a different rate fails until the first
is closed; the ASIO driver reports this as "another application is using the
device at a rate other than ... Hz".

"Audio Kontrol 1 Control Panel" in the Start menu, or a DAW's ASIO settings
button, opens the control panel. It sets the sample rate and buffer size the
ASIO driver offers to DAWs that do not choose their own; a DAW that has the
driver open is asked to reload it when the buffer size changes. It also shows
whether the card is working, the driver and firmware versions and the error
counts of the last stream, plays a test tone on either output pair and meters
the inputs. While the meter is on it holds the card at the Windows sample
rate.

The front phones output sums left and right and follows the 1/2 - 3/4 selector
next to it. The output 1/2 and 3/4 level knobs only affect the rear outputs.

USB passthrough in a QEMU virtual machine distorts playback, even though
nothing is lost on the guest side. Run it on real hardware.

## Building

Needs the WDK and SDK 10.0.26100, LLVM (set `LIBCLANG_PATH` to its `bin`
directory) and a code signing certificate in `Cert:\CurrentUser\My`.

```powershell
.\scripts\package-driver.ps1 -CertThumbprint <sha1>           # debug driver package
.\scripts\make-bundle.ps1 -CertThumbprint <sha1>              # release bundle in target\bundle
cargo test --workspace
```

## Tools

`ak1-probe` talks to the endpoints (`endpoints`, `play`, `identify`, `record`)
and, with the device bound to WinUSB (`driver\winusb`), to the raw protocol
(`info`, `capture`).
`ak1-asio-host` loads the ASIO driver and reports callback timing, or opens
its control panel:

```powershell
ak1-probe identify output12            # low tone left, then high tone right
ak1-asio-host 48000 pref 10 out1,out2  # 10 s of ASIO on outputs 1/2
ak1-asio-host panel                    # what a DAW's ASIO settings button does
```

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or
the [MIT license](LICENSE-MIT), at your option.
