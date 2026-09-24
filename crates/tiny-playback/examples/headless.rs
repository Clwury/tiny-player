//! Consume display-ready frames using only the engine's public API.
//! Usage: cargo run -p tiny-playback --example headless -- <media-path-or-url>

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tiny_playback::{
    BackendControl, BackendEventKind, BackendLoadRequest, FfmpegBackend, PlaybackCacheConfig,
    PlaybackTrackSelection, RenderSize, VideoPresenter,
};

fn main() -> Result<()> {
    let url = std::env::args()
        .nth(1)
        .context("provide a media path or URL")?;
    let mut backend = FfmpegBackend::new()?;
    let mut presenter = VideoPresenter::new(backend.video_output())?;
    let result = (|| {
        backend.load(BackendLoadRequest {
            url,
            http_headers: Vec::new(),
            content_length: None,
            start_position_seconds: 0.0,
            selected_tracks: PlaybackTrackSelection::default(),
            cache_config: PlaybackCacheConfig::default(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut size = RenderSize {
            width: 640,
            height: 360,
        };
        let mut frames = 0;
        let mut ended = false;
        while Instant::now() < deadline {
            for event in backend.poll_events() {
                match event.kind {
                    BackendEventKind::VideoSizeChanged(Some(source)) => size = source,
                    BackendEventKind::LoadFailed(error) | BackendEventKind::Fatal(error) => {
                        bail!(error)
                    }
                    BackendEventKind::PlaybackEnded => ended = true,
                    _ => {}
                }
            }
            presenter.prewarm_if_needed();
            if let Some(frame) = presenter.render_if_needed(size)? {
                frames += 1;
                if frames == 1 {
                    println!(
                        "First BGRA frame: {}x{}, {} bytes",
                        frame.size().width,
                        frame.size().height,
                        frame.bytes().len()
                    );
                }
            }
            if ended && !presenter.snapshot().needs_animation_frame() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if frames == 0 {
            bail!("no video frames received within 30 seconds");
        }
        println!("Consumed {frames} frames; playback ended: {ended}");
        Ok(())
    })();
    drop(presenter);
    backend.stop()?;
    result
}
