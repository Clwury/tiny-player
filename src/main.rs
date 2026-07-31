mod logging;

fn main() {
    let _log_guard = logging::init();
    tiny_player::run();
}
