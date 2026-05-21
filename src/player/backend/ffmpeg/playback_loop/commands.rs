use super::*;
use crate::player::backend::ffmpeg::worker::{PendingSeek, PendingTrackSelection};

pub(super) fn begin_track_switch(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    pending: &PendingTrackSelection,
) -> f64 {
    begin_position_reset(
        session,
        control,
        pending.session_id,
        pending.position_seconds,
        pending.generation,
    )
}

pub(super) fn begin_seek(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    pending: &PendingSeek,
) -> f64 {
    begin_position_reset(
        session,
        control,
        pending.session_id,
        pending.position_seconds,
        pending.generation,
    )
}

fn begin_position_reset(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    position_seconds: f64,
    generation: u64,
) -> f64 {
    let position_seconds = position_seconds.max(0.0);
    session.reset_to(session_id, position_seconds);
    control.set_session_id(session.id());
    control.set_paused(false);
    control.finish_seek(generation);
    position_seconds
}
