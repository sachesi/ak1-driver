use std::path::{Path, PathBuf};

use wdk_build::{ApiSubset, BuilderExt, Config};

const ACX_VERSION_MAJOR: u32 = 1;
const ACX_VERSION_MINOR: u32 = 1;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let config = Config::from_env_auto()?;
    let acx_dir = Path::new("acx").join("km").join(format!("{ACX_VERSION_MAJOR}.{ACX_VERSION_MINOR}"));

    let km_include = find_dir(config.include_paths()?, &["km"])?;
    let km_lib = find_dir(config.library_paths()?, &["km", "x64"])?;

    let header = config.bindgen_header_contents([ApiSubset::Base, ApiSubset::Wdf])?
        + "#include <windef.h>\n#define NOBITMAP\n#include <mmreg.h>\n#include <ks.h>\n#include <ksmedia.h>\n#include <acx.h>\n";

    let bindings = bindgen::Builder::wdk_default(&config)?
        .header_contents("acx-input.h", &header)
        .clang_arg(format!("--include-directory={}", km_include.join(&acx_dir).display()))
        .clang_arg(format!("--define-macro=ACX_VERSION_MAJOR={ACX_VERSION_MAJOR}"))
        .clang_arg(format!("--define-macro=ACX_VERSION_MINOR={ACX_VERSION_MINOR}"))
        // Types from the NT and WDF headers come from wdk-sys; only ACX and the
        // kernel streaming headers it builds on are generated here.
        .allowlist_file(r"(?i).*[\\/](acx[a-z]*|ks|ksmedia|mmreg|minwindef|windef)\.h")
        .allowlist_recursively(false)
        // The ACX entry points are FORCEINLINE wrappers around the AcxFunctions
        // table and have no exported symbols; `call_acx!` goes through the table.
        .blocklist_function("(?i)acx.*")
        .blocklist_item("AcxMinimumVersionRequired")
        .generate()?;

    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("acx.rs");
    bindings.write_to_file(&out)?;

    println!("cargo:rustc-link-search=native={}", km_lib.join(&acx_dir).display());
    println!("cargo:rustc-link-lib=static=acxstub");
    Ok(())
}

fn find_dir(paths: impl Iterator<Item = PathBuf>, suffix: &[&str]) -> Result<PathBuf> {
    let suffix: PathBuf = suffix.iter().collect();
    let mut paths = paths.collect::<Vec<_>>();
    paths
        .iter()
        .position(|p| p.ends_with(&suffix))
        .map(|i| paths.swap_remove(i))
        .ok_or_else(|| format!("no WDK directory ending in {} among {paths:?}", suffix.display()).into())
}
