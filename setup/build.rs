use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Files the setup carries, taken from the directory in AK1_SETUP_PAYLOAD
/// (the bundle make-bundle.ps1 collects). Without it the setup carries none.
const PAYLOAD: [&str; 6] =
    ["ak1acx.inf", "ak1acx.sys", "ak1acx.cat", "ak1-driver-test.cer", "ak1_asio.dll", "ak1-panel.exe"];

fn main() {
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator'");

    println!("cargo:rerun-if-env-changed=AK1_SETUP_PAYLOAD");
    let mut code = String::from("pub const FILES: &[(&str, &[u8])] = &[\n");
    if let Some(bundle) = std::env::var_os("AK1_SETUP_PAYLOAD") {
        for name in PAYLOAD {
            let path = Path::new(&bundle).join(name);
            assert!(path.is_file(), "{} is missing", path.display());
            println!("cargo:rerun-if-changed={}", path.display());
            writeln!(code, "    ({name:?}, include_bytes!({:?})),", path.display().to_string()).unwrap();
        }
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("payload.rs");
    std::fs::write(out, code).unwrap();
}
