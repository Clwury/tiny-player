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
    )
}

fn begin_position_reset(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    position_seconds: f64,
) -> f64 {
    let position_seconds = position_seconds.max(0.0);
    session.reset_to(session_id, position_seconds);
    control.set_session_id(session.id());
    control.set_cache_paused(false);
    position_seconds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_seek_preserves_user_pause_while_clearing_cache_pause() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        let mut session = PlaybackSession::new(PlaybackSessionId(1), 0.0);
        let pending = PendingSeek {
            session_id: PlaybackSessionId(2),
            position_seconds: 12.0,
            generation: 1,
        };
        control.set_user_paused(true);
        assert!(control.set_cache_paused(true));

        let _ = begin_seek(&mut session, &control, &pending);

        assert!(control.is_user_paused());
        assert!(!control.is_cache_paused());
        assert!(control.is_paused());
    }
}
