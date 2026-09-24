//! Bindings to the Audio Class eXtension (ACX) 1.1 kernel-mode framework.

#![no_std]
#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unsafe_op_in_unsafe_fn,
    clippy::all,
    missing_docs
)]

#[doc(hidden)]
pub use paste as __paste;
use wdk_sys::*;

include!(concat!(env!("OUT_DIR"), "/acx.rs"));

/// Minor version of ACX the driver was built against; read by the framework
/// when it binds the function table.
#[unsafe(no_mangle)]
pub static AcxMinimumVersionRequired: ULONG = 1;

/// Calls an ACX function through the framework's function table, the way the
/// `FORCEINLINE` wrappers in the ACX headers do.
///
/// `call_acx!(AcxDeviceInitInitialize, device_init, &mut config)`
#[macro_export]
macro_rules! call_acx {
    ($name:ident $(, $arg:expr)* $(,)?) => {
        $crate::__paste::paste! {{
            let slot = (&raw const $crate::AcxFunctions)
                .cast::<$crate::ACXFUNC>()
                .add($crate::_ACXFUNCENUM::[<$name TableIndex>] as usize);
            let f: $crate::[<PFN_ $name:upper>] = ::core::mem::transmute(slot.read());
            (f.expect(concat!(stringify!($name), " missing from the ACX function table")))(
                $crate::AcxDriverGlobals $(, $arg)*
            )
        }}
    };
}
