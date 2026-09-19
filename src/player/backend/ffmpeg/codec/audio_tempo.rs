use std::{
    ffi::{CStr, CString},
    ptr,
};

use ffmpeg_sys_next as ffi;

use super::{AvFrame, DecodedAudio};
use crate::player::backend::ffmpeg::{ffmpeg_error, nsecs_to_timestamp, timestamp_to_nsecs};

/// Runs off the audio callback thread. Packet timestamps remain in media time;
/// only the PCM sample count changes. Cascading atempo keeps each stage <= 2x,
/// avoiding sample skipping and retaining pitch at all supported rates.
pub(in super::super) struct AudioTempo {
    graph: *mut ffi::AVFilterGraph,
    source: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    stages: [*mut ffi::AVFilterContext; 2],
    sample_rate: i32,
    channels: i32,
    time_base: ffi::AVRational,
    rate: f64,
    input_frames: i64,
    output_frames: u64,
    origin_nsecs: Option<u64>,
    input_end_nsecs: Option<u64>,
}

// The graph has no thread affinity. Its owner serializes all access (the AO
// queue mutex in playback); it is never used from the real-time callback.
unsafe impl Send for AudioTempo {}

impl AudioTempo {
    pub(in super::super) fn new(
        sample_rate: i32,
        channels: i32,
        time_base: ffi::AVRational,
    ) -> Self {
        Self {
            graph: ptr::null_mut(),
            source: ptr::null_mut(),
            sink: ptr::null_mut(),
            stages: [ptr::null_mut(); 2],
            sample_rate,
            channels,
            time_base,
            rate: 1.0,
            input_frames: 0,
            output_frames: 0,
            origin_nsecs: None,
            input_end_nsecs: None,
        }
    }

    pub(in super::super) fn reset(&mut self) {
        unsafe { ffi::avfilter_graph_free(&mut self.graph) };
        self.source = ptr::null_mut();
        self.sink = ptr::null_mut();
        self.stages = [ptr::null_mut(); 2];
        self.input_frames = 0;
        self.output_frames = 0;
        self.origin_nsecs = None;
        self.input_end_nsecs = None;
    }

    pub(in super::super) fn process(
        &mut self,
        audio: DecodedAudio,
        timestamp: i64,
        rate: f64,
        mut emit: impl FnMut(DecodedAudio, i64) -> Result<(), String>,
    ) -> Result<(), String> {
        let rate = crate::player::rate::clamp_playback_rate(rate);
        let timestamp_nsecs = timestamp_to_nsecs(timestamp, self.time_base);
        let discontinuity = timestamp_nsecs
            .zip(self.input_end_nsecs)
            .is_some_and(|(pts, end)| pts.abs_diff(end) > 100_000_000);
        if discontinuity {
            self.drain(&mut emit)?;
        }
        self.set_rate(rate, &mut emit)?;
        if rate == 1.0 && self.graph.is_null() {
            return emit(audio, timestamp);
        }
        if self.sample_rate <= 0
            || self.channels <= 0
            || !audio.samples.len().is_multiple_of(self.channels as usize)
        {
            return Err("音频变速输入格式无效".to_string());
        }
        if self.graph.is_null() {
            self.configure()?;
            self.origin_nsecs = timestamp_nsecs;
        }
        self.input_end_nsecs = timestamp_nsecs.map(|pts| pts.saturating_add(audio.duration_nsecs));
        let frames = i32::try_from(audio.samples.len() / self.channels as usize)
            .map_err(|_| "变速音频帧过大".to_string())?;
        if frames == 0 {
            return Ok(());
        }
        let mut frame = AvFrame::new()?;
        unsafe {
            let raw = frame.as_mut_ptr();
            (*raw).format = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT as i32;
            (*raw).sample_rate = self.sample_rate;
            (*raw).nb_samples = frames;
            (*raw).pts = self.input_frames;
            ffi::av_channel_layout_default(&mut (*raw).ch_layout, self.channels);
            check(ffi::av_frame_get_buffer(raw, 0), "分配变速音频帧")?;
            ptr::copy_nonoverlapping(
                audio.samples.as_ptr(),
                (*raw).data[0].cast::<f32>(),
                audio.samples.len(),
            );
            check(
                ffi::av_buffersrc_add_frame_flags(self.source, raw, 0),
                "输入变速音频",
            )?;
        }
        self.input_frames += i64::from(frames);
        self.receive(&mut emit)
    }

    pub(in super::super) fn drain(
        &mut self,
        mut emit: impl FnMut(DecodedAudio, i64) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.graph.is_null() {
            return Ok(());
        }
        unsafe {
            check(
                ffi::av_buffersrc_add_frame_flags(self.source, ptr::null_mut(), 0),
                "排空变速音频",
            )?;
        }
        self.receive(&mut emit)?;
        self.reset();
        Ok(())
    }

    pub(in super::super) fn pending_range_nsecs(&self) -> Option<(u64, u64)> {
        let start = self
            .origin_nsecs?
            .saturating_add(self.media_offset(self.output_frames));
        let end = self.input_end_nsecs?;
        (end > start).then_some((start, end))
    }

    pub(in super::super) fn set_rate(
        &mut self,
        rate: f64,
        emit: &mut impl FnMut(DecodedAudio, i64) -> Result<(), String>,
    ) -> Result<(), String> {
        let rate = crate::player::rate::clamp_playback_rate(rate);
        if rate == self.rate {
            return Ok(());
        }
        if !self.graph.is_null() {
            // Finish already-produced output at its original rate, then
            // update atempo in place like mpv's SET_SPEED filter command.
            // Preserve overlap/history and reanchor only the media mapping.
            self.receive(emit)?;
            let offset = self.media_offset(self.output_frames);
            self.origin_nsecs = self
                .origin_nsecs
                .map(|origin| origin.saturating_add(offset));
            self.output_frames = 0;
            for (stage, factor) in self.stages.into_iter().zip(tempo_factors(rate)) {
                let value = CString::new(factor.to_string()).map_err(|error| error.to_string())?;
                unsafe {
                    check(
                        ffi::avfilter_process_command(
                            stage,
                            c"tempo".as_ptr(),
                            value.as_ptr(),
                            ptr::null_mut(),
                            0,
                            0,
                        ),
                        "实时更新音频倍速",
                    )?;
                }
            }
        }
        self.rate = rate;
        Ok(())
    }

    fn configure(&mut self) -> Result<(), String> {
        self.graph = unsafe { ffi::avfilter_graph_alloc() };
        if self.graph.is_null() {
            return Err("分配音频变速滤镜失败".to_string());
        }
        // Avoid a filter thread pool per playback session for this small chain.
        unsafe {
            (*self.graph).nb_threads = 1;
        }
        let args = format!(
            "sample_rate={}:sample_fmt=flt:channel_layout={}c:time_base=1/{}",
            self.sample_rate, self.channels, self.sample_rate
        );
        self.source = self.add_filter(c"abuffer", "input", &args)?;
        let mut previous = self.source;
        for (index, factor) in tempo_factors(self.rate).into_iter().enumerate() {
            let next = self.add_filter(
                c"atempo",
                &format!("tempo{index}"),
                &format!("tempo={factor}"),
            )?;
            unsafe {
                check(ffi::avfilter_link(previous, 0, next, 0), "连接音频变速滤镜")?;
            }
            previous = next;
            self.stages[index] = next;
        }
        self.sink = self.add_filter(c"abuffersink", "output", "")?;
        unsafe {
            check(
                ffi::avfilter_link(previous, 0, self.sink, 0),
                "连接音频变速输出",
            )?;
            check(
                ffi::avfilter_graph_config(self.graph, ptr::null_mut()),
                "配置音频变速滤镜",
            )?;
        }
        Ok(())
    }

    fn add_filter(
        &mut self,
        name: &CStr,
        label: &str,
        args: &str,
    ) -> Result<*mut ffi::AVFilterContext, String> {
        let filter = unsafe { ffi::avfilter_get_by_name(name.as_ptr()) };
        if filter.is_null() {
            return Err(format!("FFmpeg 缺少音频滤镜 {}", name.to_string_lossy()));
        }
        let label = CString::new(label).map_err(|error| error.to_string())?;
        let args = CString::new(args).map_err(|error| error.to_string())?;
        let mut context = ptr::null_mut();
        unsafe {
            check(
                ffi::avfilter_graph_create_filter(
                    &mut context,
                    filter,
                    label.as_ptr(),
                    args.as_ptr(),
                    ptr::null_mut(),
                    self.graph,
                ),
                "创建音频变速滤镜",
            )?;
        }
        Ok(context)
    }

    fn receive(
        &mut self,
        emit: &mut impl FnMut(DecodedAudio, i64) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut frame = AvFrame::new()?;
        loop {
            let result = unsafe { ffi::av_buffersink_get_frame(self.sink, frame.as_mut_ptr()) };
            if result == ffi::AVERROR(ffi::EAGAIN) || result == ffi::AVERROR_EOF {
                break;
            }
            check(result, "读取变速音频")?;
            let raw = frame.as_mut_ptr();
            if unsafe { (*raw).nb_samples == 0 } {
                frame.unref();
                continue;
            }
            let (samples, frames) = unsafe {
                if (*raw).format != ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT as i32
                    || (*raw).ch_layout.nb_channels != self.channels
                    || (*raw).nb_samples < 0
                {
                    return Err("音频变速滤镜输出格式无效".to_string());
                }
                let frames = (*raw).nb_samples as usize;
                (
                    std::slice::from_raw_parts(
                        (*raw).data[0].cast::<f32>(),
                        frames * self.channels as usize,
                    )
                    .to_vec(),
                    frames,
                )
            };
            let start = self.media_offset(self.output_frames);
            self.output_frames += frames as u64;
            let end = self.media_offset(self.output_frames);
            let pts = self
                .origin_nsecs
                .map(|origin| nsecs_to_timestamp(origin.saturating_add(start), self.time_base))
                .unwrap_or(ffi::AV_NOPTS_VALUE);
            emit(
                DecodedAudio {
                    samples,
                    duration_nsecs: end.saturating_sub(start),
                },
                pts,
            )?;
            frame.unref();
        }
        Ok(())
    }

    fn media_offset(&self, frames: u64) -> u64 {
        (frames as f64 * self.rate * 1_000_000_000.0 / f64::from(self.sample_rate)).round() as u64
    }
}

fn tempo_factors(rate: f64) -> [f64; 2] {
    let first = rate.clamp(0.5, 2.0);
    [first, rate / first]
}

impl Drop for AudioTempo {
    fn drop(&mut self) {
        self.reset();
    }
}

fn check(result: i32, action: &str) -> Result<(), String> {
    if result < 0 {
        Err(format!("{action}失败：{}", ffmpeg_error(result)))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(start: usize, count: usize) -> DecodedAudio {
        DecodedAudio {
            samples: (start..start + count)
                .flat_map(|frame| {
                    let sample =
                        (frame as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
                    [sample, sample]
                })
                .collect(),
            duration_nsecs: count as u64 * 1_000_000_000 / 48_000,
        }
    }

    #[test]
    fn live_rate_changes_preserve_filter_history_pitch_and_media_timeline() {
        let mut tempo = AudioTempo::new(48_000, 2, tempo_time_base());
        let mut previous_end = 0;
        let mut previous_graph: *mut ffi::AVFilterGraph = ptr::null_mut();
        let mut input = 0;
        for rate in [1.0, 2.0, 0.5, 1.0, 4.0, 0.25, 1.331, 1.0] {
            let mut samples = Vec::new();
            for _ in 0..50 {
                tempo
                    .process(tone(input, 960), input as i64, rate, |audio, pts| {
                        let start = timestamp_to_nsecs(pts, tempo_time_base()).unwrap();
                        assert!(
                            start.abs_diff(previous_end) < 50_000,
                            "{rate}: {start} != {previous_end}"
                        );
                        previous_end = start + audio.duration_nsecs;
                        samples.extend(audio.samples.as_chunks::<2>().0.iter().map(|frame| {
                            assert_eq!(frame[0], frame[1]);
                            frame[0]
                        }));
                        Ok(())
                    })
                    .unwrap();
                input += 960;
            }
            if !previous_graph.is_null() {
                assert_eq!(
                    tempo.graph, previous_graph,
                    "rate updates must keep the filter graph"
                );
            }
            previous_graph = tempo.graph;
            let middle = &samples[samples.len() / 4..samples.len() * 3 / 4];
            let crossings = middle
                .windows(2)
                .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
                .count();
            let hz = crossings as f64 * 48_000.0 / middle.len() as f64;
            assert!((hz - 1000.0).abs() < 12.0, "rate {rate}: {hz} Hz");
        }
        tempo
            .drain(|audio, pts| {
                let start = timestamp_to_nsecs(pts, tempo_time_base()).unwrap();
                assert!(start.abs_diff(previous_end) < 50_000);
                previous_end = start + audio.duration_nsecs;
                Ok(())
            })
            .unwrap();
        assert!(
            previous_end.abs_diff(8_000_000_000) < 100_000_000,
            "{previous_end}"
        );
    }

    #[test]
    fn tempo_changes_duration_without_changing_pitch_or_channel_alignment() {
        for rate in [0.25, 0.5, 1.0 / 1.1, 1.0, 1.1, 2.0, 4.0] {
            let mut tempo = AudioTempo::new(
                48_000,
                2,
                ffi::AVRational {
                    num: 1,
                    den: 48_000,
                },
            );
            let mut result = Vec::new();
            let mut timeline_end = 0;
            let mut emit = |audio: DecodedAudio, pts: i64| {
                assert_eq!(audio.samples.len() % 2, 0);
                assert!(
                    audio
                        .samples
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .all(|frame| frame[0] == frame[1])
                );
                let start = timestamp_to_nsecs(pts, tempo_time_base()).unwrap();
                assert!(start.abs_diff(timeline_end) < 50_000);
                timeline_end = start + audio.duration_nsecs;
                result.extend(
                    audio
                        .samples
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|frame| frame[0]),
                );
                Ok(())
            };
            for start in (0..192_000).step_by(960) {
                tempo
                    .process(tone(start, 960), start as i64, rate, &mut emit)
                    .unwrap();
            }
            tempo.drain(&mut emit).unwrap();
            let actual_duration = result.len() as f64 / 48_000.0;
            assert!(
                (actual_duration - 4.0 / rate).abs() < 0.08,
                "{rate}: {actual_duration}"
            );
            assert!(
                timeline_end.abs_diff(4_000_000_000) < 100_000_000,
                "{rate}: {timeline_end}"
            );
            let middle = &result[2_000..result.len() - 2_000];
            let crossings = middle
                .windows(2)
                .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
                .count();
            let frequency = crossings as f64 * 48_000.0 / middle.len() as f64;
            assert!(
                (frequency - 1_000.0).abs() < 8.0,
                "{rate}: pitch {frequency}"
            );
        }
    }

    fn tempo_time_base() -> ffi::AVRational {
        ffi::AVRational {
            num: 1,
            den: 48_000,
        }
    }

    #[test]
    fn seek_reset_discards_delayed_tempo_samples_and_reanchors_timestamps() {
        let mut tempo = AudioTempo::new(48_000, 2, tempo_time_base());
        tempo.process(tone(0, 960), 0, 0.5, |_, _| Ok(())).unwrap();
        tempo.reset();
        let mut first_timestamp = None;
        let mut emit = |audio: DecodedAudio, pts| {
            first_timestamp.get_or_insert(pts);
            assert!(audio.samples.iter().all(|sample| sample.abs() < 1e-6));
            Ok(())
        };
        for frame in 0..50 {
            tempo
                .process(
                    DecodedAudio {
                        samples: vec![0.0; 1920],
                        duration_nsecs: 20_000_000,
                    },
                    480_000 + frame * 960,
                    2.0,
                    &mut emit,
                )
                .unwrap();
        }
        tempo.drain(&mut emit).unwrap();
        assert_eq!(first_timestamp, Some(480_000));
    }
}
