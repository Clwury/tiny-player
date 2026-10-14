use super::*;
use crate::backend::ffmpeg::playback_loop::{
    demux_cache::DemuxPacketCacheInput,
    subtitle_decode_worker::{SubtitleDecodeWorker, SubtitleDecodeWorkerState},
};
use crate::backend::ffmpeg::{Decoder, FormatContext, PgsFrameMergeBitstreamFilter};
use crate::render_host::RenderSize;

#[test]
fn stopped_demux_keeps_streams_alive_for_late_pgs_worker_initialization() {
    let input = FormatContext::with_streams_for_test(&[
        (
            ffi::AVMediaType::AVMEDIA_TYPE_VIDEO,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        ),
        (
            ffi::AVMediaType::AVMEDIA_TYPE_SUBTITLE,
            ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE,
        ),
    ]);
    let streams = input.streams().unwrap();
    let video_stream = streams[0];
    let subtitle_stream = streams[1];
    let decoder = Decoder::open_subtitle(
        subtitle_stream,
        Some(RenderSize {
            width: 16,
            height: 16,
        }),
    )
    .unwrap();
    let session_id = PlaybackSessionId(1);
    let control = Arc::new(FfmpegControl::new(session_id));
    // Esc can stop playback after probing but before the demux/subtitle
    // threads are scheduled. Force demux to finish before subtitle starts.
    control.shutdown();
    let (events, _) = mpsc::channel();
    let cache = DemuxPacketCache::spawn(
        DemuxPacketCacheInput {
            input,
            video_stream,
            audio_stream: None,
            subtitle_stream: Some(subtitle_stream),
            duration_seconds: None,
            start_position_seconds: 0.0,
            session_id,
            cache_config: cache_config_for_test(),
        },
        control,
        events,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !cache.handle.as_ref().unwrap().is_finished() {
        assert!(Instant::now() < deadline, "stopped demux must finish");
        thread::yield_now();
    }

    // Filter initialization errors normally fall back to unfiltered decoding.
    // Assert the copy succeeds as well as exercising worker startup below.
    drop(PgsFrameMergeBitstreamFilter::new(subtitle_stream).unwrap());
    let mut worker = SubtitleDecodeWorker::spawn(decoder, subtitle_stream).unwrap();
    worker.flush_buffers(1).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while matches!(
        worker.snapshot().state,
        SubtitleDecodeWorkerState::Recovering
    ) {
        worker.service().unwrap();
        assert!(
            Instant::now() < deadline,
            "PGS worker must initialize and flush"
        );
        thread::yield_now();
    }
    drop(worker);
    drop(cache);
}
