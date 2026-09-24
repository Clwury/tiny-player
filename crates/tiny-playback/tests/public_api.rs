use tiny_playback::{
    BackendCommand, BackendControl, BgraImage, FfmpegBackend, PlaybackTrack, SharedBgraImage,
    VideoOutput, VideoPresenter,
};

#[test]
fn backend_and_presenter_connect_without_a_window_or_native_frame_api() {
    let mut backend = FfmpegBackend::new().unwrap();
    let output: VideoOutput = BackendControl::video_output(&backend);
    let presenter = VideoPresenter::new(output).unwrap();
    assert_eq!(presenter.snapshot().queued, 0);
    backend
        .command(BackendCommand::SetVolume { volume: 0.25 })
        .unwrap();
    drop(presenter);
    backend.command(BackendCommand::Stop).unwrap();
}

#[test]
fn public_media_data_crosses_threads_without_ui_types() {
    let image = SharedBgraImage::new(BgraImage::new(vec![1, 2, 3, 128], 1, 1).unwrap());
    let shared = image.clone();
    let track = PlaybackTrack::new(1, String::from("English"), false);
    let (received, track) = std::thread::spawn(move || (shared, track)).join().unwrap();
    assert_eq!(received, image);
    assert_eq!(received.image().bytes(), [1, 2, 3, 128]);
    assert_eq!(track.label, "English");
}
