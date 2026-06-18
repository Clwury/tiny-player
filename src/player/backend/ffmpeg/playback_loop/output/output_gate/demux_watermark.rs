use super::{DemuxReaderWatermark, Instant, OutputGateResumeTiming};

pub(in crate::player::backend::ffmpeg::playback_loop::output_gate) fn timed_output_gate_demux_watermark<
    F,
>(
    demux_watermark: &mut F,
    timing: &mut OutputGateResumeTiming,
) -> DemuxReaderWatermark
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let started_at = Instant::now();
    let watermark = demux_watermark();
    timing.demux_watermark += started_at.elapsed();
    watermark
}
