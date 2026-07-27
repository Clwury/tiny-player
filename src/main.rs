mod logging;

fn main() {
    let _log_guard = logging::init();
    tiny::run();
}
