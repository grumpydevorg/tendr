//! Link-time settings the source cannot express.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Windows gives a program's main thread a 1 MiB stack; Linux and macOS
    // give 8 MiB. The CLI parser clap derives uses nearly 1 MiB of it in a
    // debug build, so a few more flags overflowed it on Windows only. Give
    // Windows binaries the 8 MiB the other platforms have.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" {
        match target_env.as_str() {
            "msvc" => println!("cargo:rustc-link-arg-bins=/STACK:8388608"),
            "gnu" => println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608"),
            _ => {}
        }
    }
}
