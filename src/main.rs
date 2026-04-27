use tracing_subscriber::{EnvFilter, fmt, prelude::*};

fn main() {
    init_tracing();
    tiny::run();
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("tiny=debug,reqwest=debug,hyper=info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .try_init();
}
