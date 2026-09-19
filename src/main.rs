#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod logging;

fn main() {
    let _log_guard = logging::init();
    tiny_player::run();
}
