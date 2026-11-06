//! File output preserves playback events at DEBUG level and above.
use std::fmt;

use super::{Arc, PlaybackFileLog, PlaybackLogMakeWriter};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::{
    Layer,
    fmt::{
        FormatFields, MakeWriter,
        format::{DefaultFields, Writer},
        time::FormatTime,
        writer::EitherWriter,
    },
    layer::Context,
};

pub(super) fn is_playback_log_event(metadata: &Metadata<'_>) -> bool {
    if metadata.level() > &Level::DEBUG || !metadata.is_event() {
        return false;
    }
    [
        "tiny_playback::backend",
        "tiny_playback::render_host",
        "tiny_playback::video_presenter",
        "tiny_playback::libplacebo",
        "tiny_playback::dovi",
        "tiny_player::player",
    ]
    .iter()
    .any(|prefix| {
        metadata.target() == *prefix
            || metadata
                .target()
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with("::"))
    })
}

pub(super) struct PlaybackFileLayer(pub(super) Arc<PlaybackFileLog>);

impl<S: Subscriber> Layer<S> for PlaybackFileLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        // Capture ownership before formatting. A concurrent playback reset
        // must also discard an event whose field formatter has not returned.
        let make_writer = PlaybackLogMakeWriter(Arc::clone(&self.0));
        if let EitherWriter::A(mut event_writer) = make_writer.make_writer()
            && format_playback_event(Writer::new(&mut event_writer), event).is_err()
        {
            event_writer.buffer.clear();
            eprintln!("failed to format playback debug event");
        }
    }
}

fn format_playback_event(mut writer: Writer<'_>, event: &Event<'_>) -> fmt::Result {
    tracing_subscriber::fmt::time::SystemTime.format_time(&mut writer)?;
    let metadata = event.metadata();
    write!(
        writer,
        " {} {}:{} ",
        metadata.level(),
        metadata.target(),
        metadata.line().unwrap_or(0)
    )?;
    // Keep playback event fields intact without importing context from
    // unrelated parent spans (for example, application HTTP requests).
    DefaultFields::new().format_fields(writer.by_ref(), event)?;
    writeln!(writer)
}
