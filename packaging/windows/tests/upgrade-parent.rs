//! Disposable native process used to verify the installer's handoff wait.
#![windows_subsystem = "windows"]

fn main() {
    let ready = std::env::args_os().nth(1).expect("ready-file path");
    std::fs::write(ready, std::process::id().to_string()).expect("signal readiness");
    std::thread::sleep(std::time::Duration::from_secs(4));
}
