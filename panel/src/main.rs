//! Control panel for the Audio Kontrol 1: device status, the ASIO sample rate
//! and buffer size, and output and input tests.

#![windows_subsystem = "windows"]

mod audio;
mod device;
mod template;

use std::cell::RefCell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ak1_asio::settings::{BUFFER_MULTIPLES, RATES, Settings};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, ICC_PROGRESS_CLASS, INITCOMMONCONTROLSEX, InitCommonControlsEx,
    IsDlgButtonChecked, PBM_SETPOS, PBM_SETRANGE32,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BN_CLICKED, BS_AUTOCHECKBOX, BS_GROUPBOX, BS_PUSHBUTTON, CB_ADDSTRING, CB_GETCURSEL, CB_RESETCONTENT,
    CB_SETCURSEL, CBN_SELCHANGE, CBS_DROPDOWNLIST, DS_CENTER, DS_MODALFRAME, DS_SETFONT, DialogBoxIndirectParamW,
    EndDialog, FindWindowW, GetDlgItem, IDCANCEL, KillTimer, SendDlgItemMessageW,
    SetDlgItemTextW, SetForegroundWindow, SetTimer, WM_CLOSE, WM_COMMAND, WM_INITDIALOG, WM_TIMER, WS_CAPTION,
    WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VSCROLL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::core::{HSTRING, w};

use crate::audio::{Meter, Pair};
use crate::template::{Class, Template};

const TITLE: &str = "Audio Kontrol 1 Control Panel";

const IDC_STATUS: u16 = 100;
const IDC_VERSIONS: u16 = 101;
const IDC_LAST_STREAM: u16 = 102;
const IDC_RATE: u16 = 110;
const IDC_BUFFER: u16 = 111;
const IDC_TEST12: u16 = 120;
const IDC_TEST34: u16 = 121;
const IDC_METER: u16 = 122;
const IDC_METER1: u16 = 123;
const IDC_METER2: u16 = 124;
const IDC_LEVEL1: u16 = 125;
const IDC_LEVEL2: u16 = 126;
const IDC_MESSAGE: u16 = 130;
const IDC_NONE: u16 = 0xffff;

const STATUS_TIMER: usize = 1;
const METER_TIMER: usize = 2;
const STATUS_MS: u32 = 1000;
const METER_MS: u32 = 50;
/// The meter shows -60..0 dBFS and falls by this much per meter tick.
const METER_FLOOR_DB: f64 = -60.0;
const METER_FALL_DB: f64 = 1.5;

static TEST_RUNNING: AtomicBool = AtomicBool::new(false);
/// Outcome of the last test, shown by the next meter tick.
static TEST_ERROR: Mutex<Option<String>> = Mutex::new(None);

struct Panel {
    settings: Settings,
    meter: Option<Meter>,
    shown_db: [f64; 2],
}

thread_local! {
    static PANEL: RefCell<Panel> = RefCell::new(Panel {
        settings: Settings::load(),
        meter: None,
        shown_db: [METER_FLOOR_DB; 2],
    });
}

fn main() {
    let existing = unsafe { FindWindowW(w!("#32770"), &HSTRING::from(TITLE)) };
    if let Ok(window) = existing {
        let _ = unsafe { SetForegroundWindow(window) };
        return;
    }
    let controls = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_PROGRESS_CLASS,
    };
    let _ = unsafe { InitCommonControlsEx(&controls) };
    let template = layout().build();
    let instance = unsafe { GetModuleHandleW(None) }.ok().map(Into::into);
    unsafe { DialogBoxIndirectParamW(instance, Template::as_template(&template), None, Some(dialog), LPARAM(0)) };
}

fn layout() -> Template {
    let style = DS_SETFONT as u32 | DS_MODALFRAME as u32 | DS_CENTER as u32;
    let mut t = Template::new(TITLE, style | (WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX).0, "Segoe UI", 9, [260, 262]);
    let group = BS_GROUPBOX as u32;
    let label = 0;
    let combo = CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0 | WS_TABSTOP.0;
    let button = BS_PUSHBUTTON as u32 | WS_TABSTOP.0;

    t.item(Class::Button, "Device", IDC_NONE, group, [7, 5, 246, 54]);
    t.item(Class::Static, "", IDC_STATUS, label, [15, 18, 230, 10]);
    t.item(Class::Static, "", IDC_VERSIONS, label, [15, 30, 230, 10]);
    t.item(Class::Static, "", IDC_LAST_STREAM, label, [15, 42, 230, 10]);

    t.item(Class::Button, "ASIO", IDC_NONE, group, [7, 63, 246, 76]);
    t.item(Class::Static, "Sample rate:", IDC_NONE, label, [15, 78, 55, 10]);
    t.item(Class::ComboBox, "", IDC_RATE, combo, [75, 76, 110, 120]);
    t.item(Class::Static, "Buffer size:", IDC_NONE, label, [15, 96, 55, 10]);
    t.item(Class::ComboBox, "", IDC_BUFFER, combo, [75, 94, 110, 120]);
    t.item(
        Class::Static,
        "For DAWs that do not set these themselves. A DAW that has the driver open reloads it when the buffer size changes.",
        IDC_NONE,
        label,
        [15, 113, 230, 20],
    );

    t.item(Class::Button, "Test", IDC_NONE, group, [7, 143, 246, 90]);
    t.item(Class::Button, "Test outputs 1/2", IDC_TEST12, button, [15, 156, 90, 14]);
    t.item(Class::Button, "Test outputs 3/4", IDC_TEST34, button, [111, 156, 90, 14]);
    t.item(Class::Static, "Plays a low tone on the left output, then a high tone on the right.", IDC_NONE, label, [15, 174, 230, 10]);
    t.item(Class::Button, "Input meter (keeps the card at the Windows sample rate while on)", IDC_METER, BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0, [15, 187, 230, 10]);
    t.item(Class::Static, "In 1", IDC_NONE, label, [15, 203, 20, 10]);
    t.item(Class::Named("msctls_progress32"), "", IDC_METER1, 0, [38, 203, 160, 9]);
    t.item(Class::Static, "", IDC_LEVEL1, label, [204, 203, 42, 10]);
    t.item(Class::Static, "In 2", IDC_NONE, label, [15, 216, 20, 10]);
    t.item(Class::Named("msctls_progress32"), "", IDC_METER2, 0, [38, 216, 160, 9]);
    t.item(Class::Static, "", IDC_LEVEL2, label, [204, 216, 42, 10]);

    t.item(Class::Static, "", IDC_MESSAGE, label, [7, 238, 180, 20]);
    t.item(Class::Button, "Close", IDCANCEL.0 as u16, button, [203, 241, 50, 14]);
    t
}

unsafe extern "system" fn dialog(window: HWND, message: u32, wparam: WPARAM, _lparam: LPARAM) -> isize {
    match message {
        WM_INITDIALOG => {
            init(window);
            1
        }
        WM_COMMAND => {
            let (id, code) = ((wparam.0 & 0xffff) as u16, (wparam.0 >> 16) as u32);
            command(window, id, code);
            1
        }
        WM_TIMER => {
            match wparam.0 {
                STATUS_TIMER => show_status(window),
                METER_TIMER => update_meter(window),
                _ => {}
            }
            1
        }
        WM_CLOSE => {
            close(window);
            1
        }
        _ => 0,
    }
}

fn init(window: HWND) {
    let settings = PANEL.with_borrow(|panel| panel.settings);
    for rate in RATES {
        add_string(window, IDC_RATE, &format!("{rate} Hz"));
    }
    select(window, IDC_RATE, RATES.iter().position(|&r| r == settings.sample_rate).unwrap_or(0));
    fill_buffer_sizes(window, settings);
    for id in [IDC_METER1, IDC_METER2] {
        send(window, id, PBM_SETRANGE32, 0, 101);
    }
    show_levels(window, [METER_FLOOR_DB; 2], false);
    show_status(window);
    unsafe {
        SetTimer(Some(window), STATUS_TIMER, STATUS_MS, None);
        SetTimer(Some(window), METER_TIMER, METER_MS, None);
    }
}

fn command(window: HWND, id: u16, code: u32) {
    match (id, code) {
        (IDC_RATE, CBN_SELCHANGE) | (IDC_BUFFER, CBN_SELCHANGE) => change_settings(window),
        (IDC_TEST12, BN_CLICKED) => start_test(window, Pair::Outputs12),
        (IDC_TEST34, BN_CLICKED) => start_test(window, Pair::Outputs34),
        (IDC_METER, BN_CLICKED) => {
            let on = unsafe { IsDlgButtonChecked(window, i32::from(IDC_METER)) } == BST_CHECKED.0;
            PANEL.with_borrow_mut(|panel| panel.meter = on.then(Meter::start));
            if !on {
                show_levels(window, [METER_FLOOR_DB; 2], false);
            }
        }
        (id, BN_CLICKED) if id == IDCANCEL.0 as u16 => close(window),
        _ => {}
    }
}

fn close(window: HWND) {
    PANEL.with_borrow_mut(|panel| panel.meter = None);
    unsafe {
        let _ = KillTimer(Some(window), STATUS_TIMER);
        let _ = KillTimer(Some(window), METER_TIMER);
        let _ = EndDialog(window, 0);
    }
}

fn change_settings(window: HWND) {
    let rate = RATES[selection(window, IDC_RATE).unwrap_or(0)];
    let multiple = BUFFER_MULTIPLES[selection(window, IDC_BUFFER).unwrap_or(0)];
    let settings = Settings { sample_rate: rate, buffer_multiple: multiple };
    PANEL.with_borrow_mut(|panel| panel.settings = settings);
    fill_buffer_sizes(window, settings);
    if let Err(e) = settings.save() {
        set_text(window, IDC_MESSAGE, &format!("Saving the settings failed: {}", e.message()));
    }
}

/// Lists the buffer sizes as they apply at the selected sample rate.
fn fill_buffer_sizes(window: HWND, settings: Settings) {
    send(window, IDC_BUFFER, CB_RESETCONTENT, 0, 0);
    for multiple in BUFFER_MULTIPLES {
        let frames = Settings { buffer_multiple: multiple, ..settings }.buffer_frames(settings.sample_rate);
        let ms = f64::from(frames) * 1000.0 / f64::from(settings.sample_rate);
        add_string(window, IDC_BUFFER, &format!("{frames} samples ({ms:.1} ms)"));
    }
    select(window, IDC_BUFFER, BUFFER_MULTIPLES.iter().position(|&m| m == settings.buffer_multiple).unwrap_or(0));
}

fn start_test(window: HWND, pair: Pair) {
    if TEST_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    enable_tests(window, false);
    set_text(window, IDC_MESSAGE, "");
    std::thread::spawn(move || {
        if let Err(e) = audio::play_test(pair) {
            *TEST_ERROR.lock().unwrap() = Some(e);
        }
        TEST_RUNNING.store(false, Ordering::Release);
    });
}

fn enable_tests(window: HWND, enabled: bool) {
    for id in [IDC_TEST12, IDC_TEST34] {
        if let Ok(button) = unsafe { GetDlgItem(Some(window), i32::from(id)) } {
            let _ = unsafe { EnableWindow(button, enabled) };
        }
    }
}

fn show_status(window: HWND) {
    let status = device::Status::query();
    set_text(window, IDC_STATUS, &status.summary());
    set_text(window, IDC_VERSIONS, &status.versions());
    set_text(window, IDC_LAST_STREAM, &status.last_stream());
}

fn update_meter(window: HWND) {
    if !TEST_RUNNING.load(Ordering::Acquire) {
        enable_tests(window, true);
    }
    if let Some(error) = TEST_ERROR.lock().unwrap().take() {
        set_text(window, IDC_MESSAGE, &error);
    }
    let reading = PANEL.with_borrow_mut(|panel| {
        let meter = panel.meter.as_ref()?;
        if let Some(error) = meter.take_error() {
            return Some(Err(error));
        }
        let peaks = meter.take_peaks();
        for (shown, peak) in panel.shown_db.iter_mut().zip(peaks) {
            let db = (20.0 * f64::from(peak).log10()).max(METER_FLOOR_DB);
            *shown = db.max(*shown - METER_FALL_DB);
        }
        Some(Ok(panel.shown_db))
    });
    match reading {
        Some(Ok(levels)) => show_levels(window, levels, true),
        Some(Err(error)) => {
            PANEL.with_borrow_mut(|panel| panel.meter = None);
            let _ = unsafe { CheckDlgButton(window, i32::from(IDC_METER), BST_UNCHECKED) };
            set_text(window, IDC_MESSAGE, &error);
        }
        None => {}
    }
}

fn show_levels(window: HWND, levels: [f64; 2], on: bool) {
    for ((bar, text), db) in [(IDC_METER1, IDC_LEVEL1), (IDC_METER2, IDC_LEVEL2)].into_iter().zip(levels) {
        let position = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB * 100.0).round() as usize;
        // Stepping back past the target skips the bar's animation, which would lag behind the signal.
        send(window, bar, PBM_SETPOS, position + 1, 0);
        send(window, bar, PBM_SETPOS, position, 0);
        let label = match on {
            false => String::new(),
            true if db <= METER_FLOOR_DB => "-inf dB".into(),
            true => format!("{db:.0} dB"),
        };
        set_text(window, text, &label);
    }
}

fn send(window: HWND, id: u16, message: u32, wparam: usize, lparam: isize) -> isize {
    unsafe { SendDlgItemMessageW(window, i32::from(id), message, WPARAM(wparam), LPARAM(lparam)) }.0
}

fn add_string(window: HWND, id: u16, text: &str) {
    let text = HSTRING::from(text);
    send(window, id, CB_ADDSTRING, 0, text.as_ptr() as isize);
}

fn select(window: HWND, id: u16, index: usize) {
    send(window, id, CB_SETCURSEL, index, 0);
}

fn selection(window: HWND, id: u16) -> Option<usize> {
    usize::try_from(send(window, id, CB_GETCURSEL, 0, 0)).ok()
}

fn set_text(window: HWND, id: u16, text: &str) {
    let _ = unsafe { SetDlgItemTextW(window, i32::from(id), &HSTRING::from(text)) };
}
