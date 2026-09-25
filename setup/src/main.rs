//! Installs the Audio Kontrol 1 driver, ASIO driver and control panel from the
//! files built into it, or removes them again. Without arguments it asks
//! which; `/install` and `/uninstall` skip the question.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::Security::Cryptography::{
    CERT_FIND_EXISTING, CERT_OPEN_STORE_FLAGS, CERT_QUERY_ENCODING_TYPE, CERT_STORE_ADD_REPLACE_EXISTING,
    CERT_STORE_PROV_SYSTEM_W, CERT_SYSTEM_STORE_LOCAL_MACHINE, CertAddEncodedCertificateToStore, CertCloseStore,
    CertCreateCertificateContext, CertDeleteCertificateFromStore, CertFindCertificateInStore,
    CertFreeCertificateContext, CertOpenStore, HCERTSTORE, X509_ASN_ENCODING,
};
use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile};
use windows::Win32::System::Console::GetConsoleProcessList;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{HSTRING, Interface, PCWSTR, w};

mod payload {
    include!(concat!(env!("OUT_DIR"), "/payload.rs"));
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const NAME: &str = "Audio Kontrol 1";
const INF: &str = "ak1acx.inf";
const CATALOG: &str = "ak1acx.cat";
const CERTIFICATE: &str = "ak1-driver-test.cer";
const DRIVER_FILES: [&str; 4] = [INF, "ak1acx.sys", CATALOG, CERTIFICATE];
const ASIO_DLL: &str = "ak1_asio.dll";
const PANEL: &str = "ak1-panel.exe";
const SETUP: &str = "ak1-setup.exe";
const CERT_STORES: [PCWSTR; 2] = [w!("Root"), w!("TrustedPublisher")];
const UNINSTALL_KEY: PCWSTR = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Audio Kontrol 1");

struct Paths {
    /// The driver package, kept where users can find it.
    driver: PathBuf,
    /// The ASIO driver and the control panel. Unlike folders in the root of
    /// the system drive, only administrators can change files here, which
    /// matters for a DLL that every ASIO host loads.
    programs: PathBuf,
    shortcut: PathBuf,
}

impl Paths {
    fn new() -> Result<Paths> {
        Ok(Paths {
            driver: PathBuf::from(format!("{}\\ak1", env("SystemDrive")?.display())),
            programs: env("ProgramFiles")?.join(NAME),
            shortcut: env("ProgramData")?
                .join("Microsoft\\Windows\\Start Menu\\Programs\\Audio Kontrol 1 Control Panel.lnk"),
        })
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = Paths::new().and_then(|paths| match args.as_slice() {
        [] => choose().and_then(|action| action.map_or(Ok(()), |action| action(&paths))),
        [flag] if flag.eq_ignore_ascii_case("/install") => install(&paths),
        [flag] if flag.eq_ignore_ascii_case("/uninstall") => uninstall(&paths),
        _ => Err("usage: ak1-setup [/install | /uninstall]".into()),
    });
    let code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ak1-setup: {e}");
            ExitCode::FAILURE
        }
    };
    pause_if_own_console();
    code
}

type Action = fn(&Paths) -> Result<()>;

/// Asks what to do when started without arguments, as from Explorer.
fn choose() -> Result<Option<Action>> {
    let installed = read_string(UNINSTALL_KEY, w!("DisplayVersion"));
    println!(
        "Audio Kontrol 1 driver setup {} ({})",
        env!("CARGO_PKG_VERSION"),
        installed.map_or("not installed".to_owned(), |version| format!("installed: {version}"))
    );
    println!("  1  Install or update");
    println!("  2  Uninstall");
    println!("  3  Quit");
    loop {
        let Some(answer) = ask("Choose 1, 2 or 3: ")? else { return Ok(None) };
        match answer.trim() {
            "1" => return Ok(Some(install)),
            "2" => return Ok(Some(uninstall)),
            "3" => return Ok(None),
            _ => {}
        }
    }
}

fn install(paths: &Paths) -> Result<()> {
    if payload::FILES.is_empty() {
        return Err("this build carries no files to install; build it with scripts\\make-bundle.ps1".into());
    }
    let mut restart = enable_test_signing()?;

    println!("Extracting the driver package to {}", paths.driver.display());
    std::fs::create_dir_all(&paths.driver)?;
    for name in DRIVER_FILES {
        write(&paths.driver.join(name), embedded(name).unwrap_or_default())?;
    }
    println!("Trusting the driver's certificate");
    trust_certificate(embedded(CERTIFICATE).unwrap_or_default())?;
    // Windows keeps the card on an installed package with a higher version,
    // so this build's driver would not take over from a newer one.
    restart |= remove_driver_packages()?;
    println!("Installing the driver");
    match run(Command::new("pnputil").arg("/add-driver").arg(paths.driver.join(INF)).arg("/install"))? {
        // 259: no Audio Kontrol 1 is plugged in.
        0 | 259 => {}
        3010 => restart = true,
        code => return Err(format!("pnputil /add-driver failed with exit code {code}").into()),
    }

    println!("Installing the ASIO driver and the control panel to {}", paths.programs.display());
    std::fs::create_dir_all(&paths.programs)?;
    for name in [ASIO_DLL, PANEL] {
        write(&paths.programs.join(name), embedded(name).unwrap_or_default())?;
    }
    let setup = paths.programs.join(SETUP);
    let exe = std::env::current_exe()?;
    if exe != setup {
        std::fs::copy(&exe, &setup).map_err(|e| format!("copying {} to {} failed: {e}", exe.display(), setup.display()))?;
    }
    let dll = paths.programs.join(ASIO_DLL);
    match run(Command::new("regsvr32").arg("/s").arg(&dll))? {
        0 => {}
        code => return Err(format!("registering {} failed with exit code {code}", dll.display()).into()),
    }
    create_shortcut(&paths.shortcut, &paths.programs.join(PANEL))?;
    register_uninstall(paths, &setup)?;

    if restart {
        println!("Installed. Restart Windows to finish.");
    } else {
        println!("Installed. Plug in the Audio Kontrol 1, or replug it if it is connected.");
    }
    Ok(())
}

fn uninstall(paths: &Paths) -> Result<()> {
    // A running program cannot delete itself, so the installed copy hands
    // over to one in the temporary directory, which Windows deletes at the
    // next restart.
    let exe = std::env::current_exe()?;
    let temporary = std::env::temp_dir().join(SETUP);
    if exe.starts_with(&paths.programs) {
        std::fs::copy(&exe, &temporary)?;
        Command::new(&temporary).arg("/uninstall").spawn()?;
        return Ok(());
    }
    if exe == temporary {
        let _ = unsafe { MoveFileExW(&HSTRING::from(exe.as_path()), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    }

    remove_file(&paths.shortcut)?;
    let dll = paths.programs.join(ASIO_DLL);
    if dll.exists() {
        println!("Unregistering the ASIO driver");
        run(Command::new("regsvr32").arg("/s").arg("/u").arg(&dll))?;
    }
    remove_dir(&paths.programs)?;

    let restart = remove_driver_packages()?;
    let certificate = match embedded(CERTIFICATE) {
        Some(certificate) => Some(certificate.to_vec()),
        None => std::fs::read(paths.driver.join(CERTIFICATE)).ok(),
    };
    if let Some(certificate) = certificate {
        println!("Removing the trust in the driver's certificate");
        distrust_certificate(&certificate)?;
    }
    remove_dir(&paths.driver)?;
    let status = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, UNINSTALL_KEY) };
    if status != ERROR_FILE_NOT_FOUND {
        status.ok()?;
    }

    if restart {
        println!("Removed. Restart Windows to finish.");
    } else {
        println!("Removed.");
    }
    println!("Test signing is still on: if no other driver needs it, turn it off with");
    println!("\"bcdedit /set testsigning off\" and turn Secure Boot back on.");
    Ok(())
}

/// Windows loads the test-signed driver only when it started with test
/// signing on. Returns whether it has to restart for that.
fn enable_test_signing() -> Result<bool> {
    let options = read_string(w!("SYSTEM\\CurrentControlSet\\Control"), w!("SystemStartOptions")).unwrap_or_default();
    if options.split_whitespace().any(|option| option.eq_ignore_ascii_case("TESTSIGNING")) {
        return Ok(false);
    }
    if read_dword(w!("SYSTEM\\CurrentControlSet\\Control\\SecureBoot\\State"), w!("UEFISecureBootEnabled")) == Some(1) {
        return Err("Secure Boot is on. Windows loads this test-signed driver only with Secure Boot off \
                    and test signing on: turn Secure Boot off in the PC's firmware settings, then run \
                    this setup again."
            .into());
    }
    println!("Windows loads this test-signed driver only with test signing on, which lowers its");
    println!("protection against unsigned drivers. If BitLocker encrypts the system drive, keep");
    println!("its recovery key at hand: Windows may ask for it after the boot settings change.");
    if !confirm("Turn test signing on? [y/N] ")? {
        return Err("test signing is off; nothing was installed".into());
    }
    match run(Command::new("bcdedit").args(["/set", "testsigning", "on"]))? {
        0 => Ok(true),
        code => Err(format!("bcdedit /set testsigning on failed with exit code {code}").into()),
    }
}

/// Removes the installed Audio Kontrol 1 driver packages and returns whether
/// Windows has to restart to finish.
fn remove_driver_packages() -> Result<bool> {
    let mut restart = false;
    for published in published_driver_packages()? {
        println!("Removing driver package {published}");
        match run(Command::new("pnputil").args(["/delete-driver", &published, "/uninstall"]))? {
            0 => {}
            3010 => restart = true,
            code => return Err(format!("pnputil /delete-driver {published} failed with exit code {code}").into()),
        }
    }
    Ok(restart)
}

/// Published names (oem<n>.inf) of the installed Audio Kontrol 1 driver
/// packages. The driver store keeps each package's INF under that name.
fn published_driver_packages() -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(env("SystemRoot")?.join("INF"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !(name.starts_with("oem") && name.ends_with(".inf")) {
            continue;
        }
        let inf = std::fs::read(entry.path()).unwrap_or_default().to_ascii_lowercase();
        if inf.windows(CATALOG.len()).any(|window| window == CATALOG.as_bytes()) {
            names.push(name);
        }
    }
    Ok(names)
}

fn open_store(name: PCWSTR) -> Result<HCERTSTORE> {
    let flags = CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE);
    Ok(unsafe {
        CertOpenStore(CERT_STORE_PROV_SYSTEM_W, CERT_QUERY_ENCODING_TYPE::default(), None, flags, Some(name.as_ptr().cast()))
    }?)
}

fn trust_certificate(certificate: &[u8]) -> Result<()> {
    for name in CERT_STORES {
        let store = open_store(name)?;
        let added = unsafe {
            CertAddEncodedCertificateToStore(Some(store), X509_ASN_ENCODING, certificate, CERT_STORE_ADD_REPLACE_EXISTING, None)
        };
        let _ = unsafe { CertCloseStore(Some(store), 0) };
        added?;
    }
    Ok(())
}

fn distrust_certificate(certificate: &[u8]) -> Result<()> {
    let context = unsafe { CertCreateCertificateContext(X509_ASN_ENCODING, certificate) };
    if context.is_null() {
        return Err(format!("reading the certificate failed: {}", windows::core::Error::from_thread()).into());
    }
    let result = CERT_STORES.into_iter().try_for_each(|name| -> Result<()> {
        let store = open_store(name)?;
        loop {
            let found = unsafe {
                CertFindCertificateInStore(store, X509_ASN_ENCODING, 0, CERT_FIND_EXISTING, Some(context.cast()), None)
            };
            // Deleting frees the found context.
            if found.is_null() || unsafe { CertDeleteCertificateFromStore(found) }.is_err() {
                break;
            }
        }
        let _ = unsafe { CertCloseStore(Some(store), 0) };
        Ok(())
    });
    let _ = unsafe { CertFreeCertificateContext(Some(context)) };
    result
}

fn create_shortcut(link: &Path, target: &Path) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let shell_link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
    unsafe { shell_link.SetPath(&HSTRING::from(target))? };
    unsafe { shell_link.cast::<IPersistFile>()?.Save(&HSTRING::from(link), true)? };
    Ok(())
}

/// Lists the setup in Settings > Apps, whose Uninstall button runs it with /uninstall.
fn register_uninstall(paths: &Paths, setup: &Path) -> Result<()> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(HKEY_LOCAL_MACHINE, UNINSTALL_KEY, None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None)
    }
    .ok()?;
    let string = |value: &OsStr| -> (REG_VALUE_TYPE, Vec<u8>) {
        let bytes = value.to_string_lossy().encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        (REG_SZ, bytes)
    };
    let dword = |value: u32| (REG_DWORD, value.to_le_bytes().to_vec());
    let uninstall = format!("\"{}\" /uninstall", setup.display());
    let values = [
        (w!("DisplayName"), string(OsStr::new("Audio Kontrol 1 driver"))),
        (w!("DisplayVersion"), string(OsStr::new(env!("CARGO_PKG_VERSION")))),
        (w!("DisplayIcon"), string(paths.programs.join(PANEL).as_os_str())),
        (w!("InstallLocation"), string(paths.programs.as_os_str())),
        (w!("UninstallString"), string(OsStr::new(&uninstall))),
        (w!("NoModify"), dword(1)),
        (w!("NoRepair"), dword(1)),
    ];
    let result = values
        .iter()
        .try_for_each(|(name, (kind, data))| unsafe { RegSetValueExW(key, *name, None, *kind, Some(data)) }.ok());
    let _ = unsafe { RegCloseKey(key) };
    Ok(result?)
}

fn read_string(key: PCWSTR, name: PCWSTR) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut size = size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, key, name, RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut size))
    };
    (status == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buffer[..size as usize / 2]).trim_end_matches('\0').to_owned())
}

fn read_dword(key: PCWSTR, name: PCWSTR) -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, key, name, RRF_RT_REG_DWORD, None, Some((&raw mut value).cast()), Some(&mut size))
    };
    (status == ERROR_SUCCESS).then_some(value)
}

fn embedded(name: &str) -> Option<&'static [u8]> {
    payload::FILES.iter().find(|(file, _)| *file == name).map(|(_, data)| *data)
}

fn env(name: &str) -> Result<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).ok_or_else(|| format!("{name} is not set").into())
}

/// Runs a program in this console and returns its exit code.
fn run(command: &mut Command) -> Result<i32> {
    let status = command.status().map_err(|e| format!("starting {} failed: {e}", command.get_program().display()))?;
    Ok(status.code().unwrap_or(-1))
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).map_err(|e| {
        format!("writing {} failed: {e}; close programs that use the ASIO driver or the control panel", path.display())
            .into()
    })
}

fn remove_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("removing {} failed: {e}", path.display()).into()),
        _ => Ok(()),
    }
}

fn remove_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!(
            "removing {} failed: {e}; close programs that use the ASIO driver or the control panel",
            path.display()
        )
        .into()),
        _ => Ok(()),
    }
}

/// Reads a line of input, or `None` at the end of it.
fn ask(question: &str) -> Result<Option<String>> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    Ok((std::io::stdin().read_line(&mut answer)? > 0).then_some(answer))
}

fn confirm(question: &str) -> Result<bool> {
    Ok(ask(question)?.is_some_and(|answer| answer.trim().eq_ignore_ascii_case("y")))
}

/// Keeps the window open when the setup has a console of its own, as it does
/// when started from Explorer or Settings.
fn pause_if_own_console() {
    let mut processes = [0u32; 2];
    if unsafe { GetConsoleProcessList(&mut processes) } == 1 {
        print!("Press Enter to close.");
        let _ = std::io::stdout().flush();
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}
