use super::*;

pub(super) fn dovi_metadata_from_frame(frame: *mut ffi::AVFrame) -> Option<DoviFrameMetadata> {
    if has_ffmpeg_dovi_metadata(frame) {
        return None;
    }

    let side_data = unsafe {
        ffi::av_frame_get_side_data(
            frame,
            ffi::AVFrameSideDataType::AV_FRAME_DATA_DOVI_RPU_BUFFER,
        )
    };
    if side_data.is_null() {
        return None;
    }

    let (data, size) = unsafe { ((*side_data).data, (*side_data).size) };
    if data.is_null() || size == 0 {
        return None;
    }
    let data = unsafe { slice::from_raw_parts(data, size) };
    match dovi_metadata_from_rpu_buffer(data) {
        Ok(Some(metadata)) => Some(metadata),
        Ok(None) => None,
        Err(error) => {
            tracing::debug!(%error, "failed to parse Dolby Vision RPU from decoded frame side data");
            None
        }
    }
}

pub(super) fn ffmpeg_dovi_metadata_from_frame(
    frame: *mut ffi::AVFrame,
) -> Option<FfmpegDoviMetadata> {
    let side_data = ffmpeg_dovi_side_data(frame);
    if side_data.is_null() {
        return None;
    }

    let (data, size) = unsafe { ((*side_data).data, (*side_data).size) };
    if data.is_null() || size == 0 {
        return None;
    }
    let data = unsafe { slice::from_raw_parts(data, size) };
    FfmpegDoviMetadata::from_bytes(data)
}

fn has_ffmpeg_dovi_metadata(frame: *mut ffi::AVFrame) -> bool {
    !ffmpeg_dovi_side_data(frame).is_null()
}

fn ffmpeg_dovi_side_data(frame: *mut ffi::AVFrame) -> *mut ffi::AVFrameSideData {
    unsafe {
        ffi::av_frame_get_side_data(frame, ffi::AVFrameSideDataType::AV_FRAME_DATA_DOVI_METADATA)
    }
}

fn dovi_metadata_from_rpu_buffer(data: &[u8]) -> anyhow::Result<Option<DoviFrameMetadata>> {
    if let Ok(metadata) = DoviFrameMetadata::from_rpu_payload(data) {
        return Ok(Some(metadata));
    }
    if let Ok(metadata) = DoviFrameMetadata::from_unspec62_nalu(data) {
        return Ok(Some(metadata));
    }

    extract_dovi_metadata(&mut DoviRpuExtractor, data)
}

#[derive(Default)]
pub(super) struct DoviPipeline {
    state: DoviMetadataState,
}

impl DoviPipeline {
    pub(super) fn observe_video_packet(&mut self, packet: &AvPacket, stream: StreamInfo) {
        self.state.observe_packet(packet, stream);
    }

    pub(super) fn metadata_for_decoded_frame(
        &mut self,
        frame: *mut ffi::AVFrame,
        pts: FramePts,
    ) -> Option<DoviFrameMetadata> {
        self.state.metadata_for_frame(frame, pts)
    }

    pub(super) fn discard_frame(&mut self, pts: FramePts) {
        self.state.discard_frame(pts);
    }

    pub(super) fn reset(&mut self) {
        self.state.clear();
    }
}

#[derive(Default)]
struct DoviMetadataState {
    queue: DoviMetadataQueue,
    last_profile5: Option<DoviFrameMetadata>,
    reused_profile5_logged: bool,
}

impl DoviMetadataState {
    pub(super) fn observe_packet(&mut self, packet: &AvPacket, stream: StreamInfo) {
        self.queue.observe_packet(packet, stream);
    }

    pub(super) fn metadata_for_frame(
        &mut self,
        frame: *mut ffi::AVFrame,
        pts: FramePts,
    ) -> Option<DoviFrameMetadata> {
        let metadata = dovi_metadata_from_frame(frame).or_else(|| self.queue.take_for_frame(pts));
        self.resolve(metadata)
    }

    pub(super) fn discard_frame(&mut self, pts: FramePts) {
        let _ = self.queue.take_for_frame(pts);
    }

    pub(super) fn clear(&mut self) {
        self.queue.clear();
        self.last_profile5 = None;
        self.reused_profile5_logged = false;
    }

    fn resolve(&mut self, metadata: Option<DoviFrameMetadata>) -> Option<DoviFrameMetadata> {
        if let Some(metadata) = metadata {
            if metadata.profile == 5 {
                self.last_profile5 = Some(metadata.clone());
            }
            return Some(metadata);
        }

        let metadata = self.last_profile5.clone()?;
        if !self.reused_profile5_logged {
            self.reused_profile5_logged = true;
            tracing::debug!(
                "reusing previous Dolby Vision Profile 5 RPU metadata after missing frame metadata"
            );
        }
        Some(metadata)
    }
}

#[derive(Default)]
struct DoviMetadataQueue {
    extractor: DoviRpuExtractor,
    entries: VecDeque<DoviMetadataEntry>,
    first_packet_nsecs: Option<u64>,
}

struct DoviMetadataEntry {
    pts: Option<FramePts>,
    metadata: DoviFrameMetadata,
}

impl DoviMetadataQueue {
    pub(super) fn observe_packet(&mut self, packet: &AvPacket, stream: StreamInfo) {
        if stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return;
        }
        let Some(data) = packet.data() else {
            return;
        };
        let metadata = match extract_dovi_metadata(&mut self.extractor, data) {
            Ok(Some(metadata)) => metadata,
            Ok(None) => return,
            Err(error) => {
                tracing::trace!(%error, "ignored non-Dolby Vision RPU candidate in FFmpeg packet");
                return;
            }
        };
        let pts = packet.best_timestamp().and_then(|timestamp| {
            dovi_packet_timeline_nsecs(
                &mut self.first_packet_nsecs,
                stream.start_nsecs,
                timestamp,
                stream.time_base,
            )
            .map(|nsecs| FramePts { nsecs })
        });
        self.entries.push_back(DoviMetadataEntry { pts, metadata });
        while self.entries.len() > RPU_QUEUE_CAPACITY {
            self.entries.pop_front();
        }
    }

    pub(super) fn take_for_frame(&mut self, pts: FramePts) -> Option<DoviFrameMetadata> {
        if self.entries.is_empty() {
            return None;
        }
        let tolerance = duration_nsecs(RPU_MATCH_TOLERANCE);
        let nearest = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry
                    .pts
                    .map(|entry_pts| (index, pts_distance(entry_pts, pts)))
            })
            .min_by_key(|(_, distance)| *distance);
        if let Some((index, distance)) = nearest
            && distance <= tolerance
        {
            return self.entries.remove(index).map(|entry| entry.metadata);
        }

        if self
            .entries
            .front()
            .is_some_and(|entry| entry.pts.is_none())
        {
            return self.entries.pop_front().map(|entry| entry.metadata);
        }
        None
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.first_packet_nsecs = None;
    }
}

pub(super) fn dovi_packet_timeline_nsecs(
    first_packet_nsecs: &mut Option<u64>,
    stream_start_nsecs: Option<u64>,
    timestamp: i64,
    time_base: ffi::AVRational,
) -> Option<u64> {
    let nsecs = timestamp_to_nsecs(timestamp, time_base)?;
    if let Some(start_nsecs) = stream_start_nsecs {
        return Some(nsecs.saturating_sub(start_nsecs));
    }

    let first_nsecs = *first_packet_nsecs.get_or_insert(nsecs);
    Some(nsecs.saturating_sub(first_nsecs))
}

pub(super) fn extract_dovi_metadata(
    extractor: &mut DoviRpuExtractor,
    data: &[u8],
) -> anyhow::Result<Option<DoviFrameMetadata>> {
    if has_annex_b_start_code(data) {
        return extractor.extract_from_hevc_access_unit(data, HevcStreamFormat::ByteStream);
    }

    let mut last_error = None;
    for length_size in [4, 2, 1] {
        match extractor
            .extract_from_hevc_access_unit(data, HevcStreamFormat::LengthPrefixed { length_size })
        {
            Ok(metadata) => return Ok(metadata),
            Err(error) => last_error = Some(error),
        }
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    Ok(None)
}

pub(super) fn has_annex_b_start_code(data: &[u8]) -> bool {
    data.windows(3).any(|window| window == [0, 0, 1])
        || data.windows(4).any(|window| window == [0, 0, 0, 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dovi_state_reuses_last_profile5_metadata_when_frame_metadata_is_missing() {
        let mut state = DoviMetadataState::default();
        let metadata = DoviFrameMetadata {
            profile: 5,
            rpu_nalu: vec![1],
            rpu_payload: vec![2],
        };

        assert_eq!(
            state.resolve(Some(metadata.clone())),
            Some(metadata.clone())
        );
        assert_eq!(state.resolve(None), Some(metadata));
    }

    #[test]
    fn dovi_rpu_buffer_parses_start_code_wrapped_raw_payload() {
        let payload = profile5_rpu_payload();
        let mut data = vec![0, 0, 0, 1];
        data.extend_from_slice(&payload);

        let metadata = dovi_metadata_from_rpu_buffer(&data).unwrap().unwrap();

        assert_eq!(metadata.profile, 5);
        assert_eq!(metadata.rpu_payload, payload);
    }

    #[test]
    fn dovi_state_does_not_reuse_non_profile5_metadata() {
        let mut state = DoviMetadataState::default();
        let metadata = DoviFrameMetadata {
            profile: 8,
            rpu_nalu: vec![1],
            rpu_payload: vec![2],
        };

        assert_eq!(
            state.resolve(Some(metadata)),
            Some(DoviFrameMetadata {
                profile: 8,
                rpu_nalu: vec![1],
                rpu_payload: vec![2],
            })
        );
        assert_eq!(state.resolve(None), None);
    }

    fn profile5_rpu_payload() -> Vec<u8> {
        use dolby_vision::rpu::dovi_rpu::DoviRpu;
        use dolby_vision::rpu::generate::{GenerateConfig, GenerateProfile};

        let config = GenerateConfig {
            profile: GenerateProfile::Profile5,
            ..Default::default()
        };
        DoviRpu::profile5_config(&config)
            .unwrap()
            .write_rpu()
            .unwrap()
    }
}
