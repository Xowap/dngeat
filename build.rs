fn main() {
    println!("cargo:rerun-if-changed=csrc/libraw_shim.c");
    cc::Build::new()
        .file("csrc/libraw_shim.c")
        .compile("libraw_shim");
    println!("cargo:rustc-link-lib=raw");
}
