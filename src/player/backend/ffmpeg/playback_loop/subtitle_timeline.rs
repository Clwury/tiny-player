use std::{collections::VecDeque, sync::mpsc::Sender};

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::{BackendEvent, BackendEventKind, BackendSubtitleCue},
    render_host::PlaybackSessionId,
};

use super::{AvPacket, StreamInfo, TimestampMapper, timestamp_to_nsecs};

pub(super) fn packet_duration_nsecs(packet: &AvPacket, stream: StreamInfo) -> Option<u64> {
    packet
        .duration()
        .and_then(|duration| timestamp_to_nsecs(duration, stream.time_base))
}

pub(super) fn push_subtitle_cue(cues: &mut VecDeque<BackendSubtitleCue>, cue: BackendSubtitleCue) {
    if !cue.has_content() || cue.end_nsecs <= cue.start_nsecs {
        return;
    }
    let index = cues
        .iter()
        .position(|current| current.start_nsecs > cue.start_nsecs)
        .unwrap_or(cues.len());
    cues.insert(index, cue);
}

pub(in crate::player::backend::ffmpeg) fn trim_overlapping_subtitle_cues_at(
    cues: &mut VecDeque<BackendSubtitleCue>,
    trim_nsecs: u64,
) {
    for cue in cues.iter_mut() {
        if cue.has_content() && cue.start_nsecs < trim_nsecs && trim_nsecs < cue.end_nsecs {
            cue.end_nsecs = trim_nsecs;
        }
    }
    cues.retain(|cue| cue.has_content() && cue.end_nsecs > cue.start_nsecs);
}

pub(super) fn refresh_playback_timeline_origin(
    playback_timeline_origin_nsecs: &mut Option<u64>,
    video_clock: &TimestampMapper,
    subtitle_stream: Option<StreamInfo>,
    subtitle_cues: &mut VecDeque<BackendSubtitleCue>,
) {
    let next_origin_nsecs = video_clock.timeline_origin_nsecs();
    if *playback_timeline_origin_nsecs == next_origin_nsecs {
        return;
    }

    if subtitle_stream
        .is_some_and(|stream| stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE)
    {
        rebase_subtitle_cues_to_timeline_origin(
            subtitle_cues,
            *playback_timeline_origin_nsecs,
            next_origin_nsecs,
        );
    }
    *playback_timeline_origin_nsecs = next_origin_nsecs;
}

pub(in crate::player::backend::ffmpeg) fn rebase_subtitle_cues_to_timeline_origin(
    cues: &mut VecDeque<BackendSubtitleCue>,
    previous_origin_nsecs: Option<u64>,
    next_origin_nsecs: Option<u64>,
) {
    let previous_origin_nsecs = previous_origin_nsecs.unwrap_or(0);
    let next_origin_nsecs = next_origin_nsecs.unwrap_or(0);
    if previous_origin_nsecs == next_origin_nsecs {
        return;
    }

    if next_origin_nsecs > previous_origin_nsecs {
        let delta = next_origin_nsecs - previous_origin_nsecs;
        for cue in cues.iter_mut() {
            cue.start_nsecs = cue.start_nsecs.saturating_sub(delta);
            cue.end_nsecs = cue.end_nsecs.saturating_sub(delta);
        }
    } else {
        let delta = previous_origin_nsecs - next_origin_nsecs;
        for cue in cues.iter_mut() {
            cue.start_nsecs = cue.start_nsecs.saturating_add(delta);
            cue.end_nsecs = cue.end_nsecs.saturating_add(delta);
        }
    }
    cues.retain(|cue| cue.has_content() && cue.end_nsecs > cue.start_nsecs);
}

pub(super) fn subtitle_cue_queue_from_external(
    cues: &[BackendSubtitleCue],
    start_position_nsecs: u64,
) -> VecDeque<BackendSubtitleCue> {
    cues.iter()
        .filter(|cue| cue.end_nsecs >= start_position_nsecs)
        .cloned()
        .collect()
}

pub(in crate::player::backend::ffmpeg) fn subtitle_cue_timeline_nsecs(
    cue_pts_nsecs: Option<u64>,
    packet_timestamp: Option<i64>,
    stream: StreamInfo,
    playback_timeline_origin_nsecs: Option<u64>,
) -> Option<u64> {
    let stream_start_nsecs =
        subtitle_stream_timeline_origin(stream, playback_timeline_origin_nsecs);
    if let Some(packet_nsecs) =
        packet_timestamp.and_then(|timestamp| timestamp_to_nsecs(timestamp, stream.time_base))
    {
        return Some(subtitle_timestamp_to_timeline_nsecs(
            packet_nsecs,
            stream_start_nsecs,
        ));
    }
    cue_pts_nsecs
        .map(|pts_nsecs| subtitle_timestamp_to_timeline_nsecs(pts_nsecs, stream_start_nsecs))
}

fn subtitle_stream_timeline_origin(
    stream: StreamInfo,
    playback_timeline_origin_nsecs: Option<u64>,
) -> Option<u64> {
    if stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE {
        playback_timeline_origin_nsecs.or(stream.start_nsecs)
    } else {
        stream.start_nsecs
    }
}

pub(in crate::player::backend::ffmpeg) fn subtitle_timestamp_to_timeline_nsecs(
    timestamp_nsecs: u64,
    stream_start_nsecs: Option<u64>,
) -> u64 {
    timestamp_nsecs.saturating_sub(stream_start_nsecs.unwrap_or(0))
}

pub(super) fn update_subtitle_overlay(
    position_nsecs: u64,
    cues: &mut VecDeque<BackendSubtitleCue>,
    active: &mut Option<BackendSubtitleCue>,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
) {
    while cues
        .front()
        .is_some_and(|cue| cue.end_nsecs <= position_nsecs)
    {
        cues.pop_front();
    }
    let next = cues
        .iter()
        .find(|cue| cue.start_nsecs <= position_nsecs && position_nsecs < cue.end_nsecs)
        .cloned();
    if *active == next {
        return;
    }
    *active = next.clone();
    let _ = event_tx.send(BackendEvent::new(
        session_id,
        BackendEventKind::SubtitleChanged(next),
    ));
}
