#[cfg(feature = "cpp")]
#[cfg(not(target_os = "macos"))]
fn main() {
    let mut build = cc::Build::new();
    build.cpp(true).flag("-std=c++11");
    // Vendored copy of esaxx-rs 0.1.10; this is the only behavioral change
    // against crates.io. Prebuilt ONNX Runtime (ort-sys) links the dynamic CRT
    // (/MD), so a /MT esaxx object breaks the final MSVC link with
    // LNK2038/LNK2005. Do not re-enable static CRT for Windows.
    if !cfg!(target_os = "windows") {
        build.static_crt(true);
    }
    build
        .file("src/esaxx.cpp")
        .include("src")
        .compile("esaxx");
}

#[cfg(feature = "cpp")]
#[cfg(target_os = "macos")]
fn main() {
    cc::Build::new()
        .cpp(true)
        .flag("-std=c++11")
        .flag("-stdlib=libc++")
        .static_crt(true)
        .file("src/esaxx.cpp")
        .include("src")
        .compile("esaxx");
}

#[cfg(not(feature = "cpp"))]
fn main() {}
