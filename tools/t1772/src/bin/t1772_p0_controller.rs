#[cfg(unix)]
#[path = "../bin_support/t1772_p0_controller_unix.rs"]
mod platform;

#[cfg(unix)]
fn main() {
    platform::main();
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "t1772-p0-controller is unsupported on this platform; it requires Unix process, signal, and file-metadata APIs"
    );
    std::process::ExitCode::from(2)
}
