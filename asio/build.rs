fn main() {
    // COM entry points are exported by name; the linker's advice to mark them
    // PRIVATE only matters for import libraries, which nobody links against.
    println!("cargo:rustc-cdylib-link-arg=/IGNORE:4104");
}
