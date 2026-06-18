#[path = "policy/config.rs"]
mod config;

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use config::*;
