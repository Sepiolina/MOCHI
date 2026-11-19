// No console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(e) = mochi_desktop::run() {
        eprintln!("mochi-desktop: {e}");
        std::process::exit(1);
    }
}
