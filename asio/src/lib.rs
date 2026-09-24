//! ASIO 2 driver for the Native Instruments Audio Kontrol 1, running on top of
//! the kernel driver's audio endpoints.

pub mod abi;
mod driver;
mod duplex;

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, ERROR_SUCCESS, HMODULE, MAX_PATH, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_core::{BOOL, GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, Ref, implement};

pub use driver::{CLSID, DRIVER_NAME};

static MODULE: AtomicIsize = AtomicIsize::new(0);
static OBJECTS: AtomicU32 = AtomicU32::new(0);

pub(crate) fn object_released() {
    OBJECTS.fetch_sub(1, Ordering::AcqRel);
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(module.0 as isize, Ordering::Release);
    }
    true.into()
}

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows_core::Result<()> {
        if object.is_null() || iid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *object = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        OBJECTS.fetch_add(1, Ordering::AcqRel);
        let driver = driver::AsioObject::create();
        let vtbl = unsafe { *driver.cast::<*const abi::IAsioVtbl<driver::AsioObject>>() };
        let hr = unsafe { ((*vtbl).query_interface)(driver, iid, object) };
        unsafe { ((*vtbl).release)(driver) };
        HRESULT(hr).ok()
    }

    fn LockServer(&self, lock: BOOL) -> windows_core::Result<()> {
        if lock.as_bool() {
            OBJECTS.fetch_add(1, Ordering::AcqRel);
        } else {
            object_released();
        }
        Ok(())
    }
}

#[unsafe(no_mangle)]
unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if clsid.is_null() || iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    unsafe { *out = std::ptr::null_mut() };
    if unsafe { *clsid } != CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory.into();
    unsafe { factory.query(iid, out) }
}

#[unsafe(no_mangle)]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(Ordering::Acquire) == 0 { S_OK } else { S_FALSE }
}

#[unsafe(no_mangle)]
extern "system" fn DllRegisterServer() -> HRESULT {
    match register() {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

#[unsafe(no_mangle)]
extern "system" fn DllUnregisterServer() -> HRESULT {
    let clsid = format!("CLSID\\{{{CLSID:?}}}");
    let asio = format!("SOFTWARE\\ASIO\\{DRIVER_NAME}");
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CLASSES_ROOT, &HSTRING::from(clsid));
        let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, &HSTRING::from(asio));
    }
    S_OK
}

fn register() -> windows_core::Result<()> {
    let mut path = [0u16; MAX_PATH as usize];
    let module = HMODULE(MODULE.load(Ordering::Acquire) as *mut c_void);
    let len = unsafe { GetModuleFileNameW(Some(module), &mut path) } as usize;
    let path = String::from_utf16_lossy(&path[..len]);
    let clsid = format!("{{{CLSID:?}}}");

    let class = format!("CLSID\\{clsid}");
    set_value(HKEY_CLASSES_ROOT, &class, None, DRIVER_NAME)?;
    set_value(HKEY_CLASSES_ROOT, &format!("{class}\\InprocServer32"), None, &path)?;
    set_value(HKEY_CLASSES_ROOT, &format!("{class}\\InprocServer32"), Some("ThreadingModel"), "Apartment")?;

    let asio = format!("SOFTWARE\\ASIO\\{DRIVER_NAME}");
    set_value(HKEY_LOCAL_MACHINE, &asio, Some("CLSID"), &clsid)?;
    set_value(HKEY_LOCAL_MACHINE, &asio, Some("Description"), DRIVER_NAME)
}

fn set_value(root: HKEY, subkey: &str, name: Option<&str>, value: &str) -> windows_core::Result<()> {
    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            root,
            &HSTRING::from(subkey),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(status.to_hresult().into());
    }
    let data: Vec<u8> = value.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
    let name = name.map(HSTRING::from);
    let status = unsafe {
        RegSetValueExW(key, name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())), None, REG_SZ, Some(&data))
    };
    let _ = unsafe { RegCloseKey(key) };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(status.to_hresult().into()) }
}
